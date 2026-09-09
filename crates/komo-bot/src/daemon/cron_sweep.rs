use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use tracing::{error, info, warn};

use crate::notify::Notifier;
use komo_core::domain::{
    context::{SessionContext, SessionOrigin},
    cron::{
        CatchUpVerdict, CronAction, CronJob, CronJobRepository, CronJobStatus, RoutineRunStatus,
    },
    gateway::MessageHandler,
    repository::SessionEventRepository,
    run::RunStatus,
    run_projection::project_runs,
    session_event::SessionEventKind,
    wakeup::{WakeupDispatch, WakeupRegistration, WakeupRepository, is_suspended},
};
use komo_services::tool_execution::{with_job_grants, with_session};

use super::{Maintenance, MaintenanceSummary};

/// Cap on the job output forwarded in a notification, so a chatty script can't
/// blow past a chat platform's message limit. The delivered text is what the
/// operator reads — logs keep nothing extra, so the cap discloses truncation.
const JOB_OUTPUT_CAP: usize = 3000;

/// Sweep the cron store (`~/.komo/cron.db`) every minute and execute due jobs —
/// hermes' `no_agent` cron jobs analog. A job's command is operator-authored
/// (`komo cron add` / the loopback-gated api — the same trust boundary as
/// running the gateway itself), so it executes directly: no shell tool, no
/// approver, no `[policy]` involvement. Reading the store per tick means jobs
/// added/removed/toggled while the gateway runs take effect on the next tick,
/// no restart.
///
/// **Claim-first**: a due job's `next_run_at` is advanced (and `last_run_at`
/// stamped) *before* the command runs, so a crash mid-run can't re-fire the
/// slot on restart, and a job running longer than a sweep tick can't be
/// double-started. A gateway asleep over a slot runs the job late, once —
/// `next_run_at` is computed from now, never replaying missed ticks.
///
/// Every outcome is delivered, success and failure alike: a weekly job whose
/// failures were only log lines would silently stop doing its work for weeks.
/// A failed *command* still leaves the cycle `Ok` — the operator was told, and
/// the breaker's minutes-scale cooldowns are meaningless on a weekly cron. Only
/// delivery failure fails the cycle (nothing reached the operator, which *is*
/// worth the breaker alert).
/// How many recent sessions the startup check reads. A suspended turn is
/// re-registered from its own log, and the sessions worth checking are the ones
/// that were live when the process died — not every conversation komo has ever
/// held.
pub const SUSPEND_RECHECK_SESSIONS: usize = 20;

/// Re-register the waits a crash lost, once at startup. Answers how many.
///
/// The two records are kept honest in both directions: the sweep drops a
/// registration whose turn is no longer waiting, and this adds one back for a
/// turn the log says *is* waiting and nothing is watching. Without it, a crash
/// in the window between `turn/suspended` and the registration write leaves a
/// turn parked forever — the log says it is waiting and nobody is coming.
///
/// The wait itself is read back out of the `turn/suspended` event, which is why
/// that event carries the `wakeup` and its deadline. **Grants are not
/// recoverable this way** — they were the suspending turn's, and only the
/// registration held them — so a re-registered unattended turn wakes able to
/// ask but not to act, which is the safe end of that trade.
///
/// Best-effort throughout: a session whose log cannot be read is warned about
/// and skipped, never fatal. Nothing here may keep the gateway from starting.
pub async fn reregister_suspended_turns(
    events: &Arc<dyn SessionEventRepository>,
    wakeups: &Arc<dyn WakeupRepository>,
    limit: usize,
    now: i64,
) -> usize {
    use komo_core::domain::session_event::SessionEventKind;

    let known = match wakeups.list().await {
        Ok(rows) => rows,
        Err(error) => {
            warn!(%error, "could not read standing wakeups; skipping the suspended-turn check");
            return 0;
        }
    };
    let ids = match events.session_ids().await {
        Ok(ids) => ids,
        Err(error) => {
            warn!(%error, "could not list sessions; skipping the suspended-turn check");
            return 0;
        }
    };
    // Newest first — the ids are UUIDv7, so their order is chronological — and
    // only as far back as the bound.
    let recent: Vec<String> = ids.into_iter().rev().take(limit).collect();

    let mut added = 0;
    for session_id in recent {
        let log = match events.events(&session_id).await {
            Ok(log) => log,
            Err(error) => {
                warn!(%error, session = %session_id, "could not read a session log; skipping it");
                continue;
            }
        };
        for projected in project_runs(&session_id, &log) {
            if projected.run.status != RunStatus::Suspended {
                continue;
            }
            let turn_id = projected.run.id.clone();
            if known
                .iter()
                .any(|r| r.session_id == session_id && r.turn_id.as_deref() == Some(&*turn_id))
            {
                continue;
            }
            // The suspension itself says what it is waiting for. The newest one
            // wins: a turn that suspended, woke and suspended again is waiting
            // for the second thing.
            let Some(suspended) = log
                .iter()
                .rev()
                .filter_map(|event| match &event.kind {
                    SessionEventKind::TurnSuspended(s) if s.turn_id == turn_id => Some(s),
                    _ => None,
                })
                .next()
            else {
                continue;
            };
            let registration = WakeupRegistration::new(&session_id, suspended.wakeup.clone(), now)
                .continuing(&turn_id)
                .expiring_at(suspended.expires_at.or_else(|| {
                    komo_core::domain::wakeup::default_expiry_secs(&suspended.wakeup)
                        .map(|secs| now + secs)
                }));
            match wakeups.save(&registration).await {
                Ok(()) => {
                    warn!(
                        session = %session_id,
                        turn = %turn_id,
                        "re-registered a suspended turn nothing was watching"
                    );
                    added += 1;
                }
                Err(error) => {
                    warn!(%error, session = %session_id, turn = %turn_id, "failed to re-register a suspended turn")
                }
            }
        }
    }
    added
}

/// What the sweep needs to fire a standing wait: the registrations, the log to
/// check them against, and whoever knows how to wake a turn.
///
/// Held together because firing one without any of the three is not a partial
/// feature, it is a wrong one: a wake with no log check resumes turns that
/// already came back, and a claim with no dispatch loses the wait.
pub struct WakeupWiring {
    pub registrations: Arc<dyn WakeupRepository>,
    pub events: Arc<dyn SessionEventRepository>,
    pub dispatch: Arc<dyn WakeupDispatch>,
}

/// Everything a routine firing needs. [`CronJobSweep`] is its only ingress —
/// every minute, the routines whose slot has come — and the running lives here
/// rather than in the sweep so a firing is one place.
pub struct RoutineEventSource {
    pub jobs: Arc<dyn CronJobRepository>,
    pub notifier: Arc<dyn Notifier>,
    /// Standing waits, read on the same tick as the jobs (docs/bot-runtime.md
    /// §3.3: one scheduler). `None` = nothing suspends turns yet, so there is
    /// nothing to wake.
    pub wakeups: Option<WakeupWiring>,
    /// The unattended, tool-capable agent that runs `CronAction::Agent` jobs
    /// (wiring's `cron_runtime`: full tool set, policy-gated with a deny-all
    /// inner approver — a `Risk::Normal` action passes only through an
    /// `unattended` policy rule). `None` = command-only; an agent job then
    /// degrades to an error delivery (the gateway always wires it).
    pub runtime: Option<Arc<dyn MessageHandler>>,
}

/// The clock ingress: every minute, the routines whose slot has come.
pub struct CronJobSweep {
    pub routines: Arc<RoutineEventSource>,
}

#[async_trait]
impl Maintenance for CronJobSweep {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary> {
        self.routines.sweep_due().await
    }
}

impl RoutineEventSource {
    /// Wrap this source in the every-minute sweep that drives its triggers and
    /// its standing waits.
    pub fn sweep(self: &Arc<Self>) -> CronJobSweep {
        CronJobSweep {
            routines: self.clone(),
        }
    }

    /// One tick of the clock ingress: the routines whose slot has come, then
    /// the standing waits whose moment has (docs/bot-runtime.md §3.3 — one
    /// scheduler for both).
    pub async fn sweep_due(&self) -> anyhow::Result<MaintenanceSummary> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut summary = MaintenanceSummary::default();
        let due: Vec<CronJob> = self
            .jobs
            .list()
            .await?
            .into_iter()
            .filter(|j| j.is_due(now))
            .collect();

        let mut delivery_failures = 0usize;
        for mut job in due {
            // A slot the gateway slept through is claimed either way — the point
            // is to stop re-firing it — but only *run* when running late is
            // still the right thing to do. `is_due` has no upper bound on
            // lateness; a laptop closed over a weekend leaves Friday's 07:00 job
            // due, and firing it Monday afternoon is not catching up.
            let abandoned = match job.catch_up_verdict(now) {
                CatchUpVerdict::TooLate { late_by } => {
                    warn!(
                        job = %job.name,
                        late_by_s = late_by,
                        catch_up = job.catch_up.as_str(),
                        "cron slot missed by too much; skipping to the next one"
                    );
                    true
                }
                CatchUpVerdict::Late { late_by } => {
                    info!(job = %job.name, late_by_s = late_by, "running a missed cron slot late");
                    false
                }
                CatchUpVerdict::OnTime => false,
            };
            // Claim the slot before executing (see the type docs). A broken
            // expression (bypassed add-time validation) pauses the job with
            // the reason recorded, rather than erroring every tick.
            let mut broken_trigger = false;
            match job.trigger.next_slot(now) {
                Ok(Some(next)) => job.next_run_at = next,
                // Nothing left to fire: a one-shot completes at claim time —
                // the same crash-safety as advancing `next_run_at`, and the row
                // stays behind as the queryable record of what ran.
                Ok(None) => job.status = CronJobStatus::Done,
                Err(e) => {
                    warn!(job = %job.name, error = %e, "broken cron trigger; pausing job");
                    job.status = CronJobStatus::Paused;
                    job.last_error = format!("invalid schedule: {e}");
                    broken_trigger = true;
                }
            }
            if broken_trigger || abandoned {
                if let Err(error) = self.jobs.update(&job).await {
                    warn!(%error, job = %job.name, "failed to claim cron job");
                }
                continue;
            }
            match self.fire(&mut job, now).await {
                Some(fired) => {
                    if fired.status == RoutineRunStatus::Ok {
                        summary.jobs_run += 1;
                    }
                    if fired.delivery_failed {
                        delivery_failures += 1;
                    }
                }
                None => continue,
            }
        }
        if let Some(wiring) = &self.wakeups {
            summary.wakeups_fired = self.fire_due_wakeups(wiring, now).await;
        }
        if delivery_failures > 0 {
            anyhow::bail!("{delivery_failures} cron job notification(s) failed to deliver");
        }
        Ok(summary)
    }

    /// Run one firing: claim it as a `running` run, act, deliver under the
    /// job's notification policy, settle the run.
    ///
    /// `None` = the claim did not land, so nothing ran — missing one firing
    /// beats double-running it. The caller has already advanced anything about
    /// the job that the firing changes (the slot's `next_run_at`); the claim and
    /// the `running` run go out as **one** write, so a crash between them
    /// cannot leave a claimed slot with no record of what it was running.
    async fn fire(&self, job: &mut CronJob, now: i64) -> Option<FiredRun> {
        let run_id = job.begin_run(now);
        if let Err(error) = self.jobs.update(job).await {
            warn!(%error, job = %job.name, "failed to claim cron job; skipping this run");
            return None;
        }

        let started = std::time::Instant::now();
        let outcome = self.execute(job).await;
        let elapsed_s = started.elapsed().as_secs();
        match outcome.status {
            RoutineRunStatus::Ok => {
                info!(job = %job.name, kind = job.action.kind(), elapsed_s, "cron job succeeded")
            }
            RoutineRunStatus::Error => {
                error!(job = %job.name, kind = job.action.kind(), elapsed_s, outcome = %outcome.body, "cron job failed")
            }
            // Neither ran nor failed: it stopped for an approval and comes
            // back when the operator answers, so it is not completed work.
            RoutineRunStatus::Waiting => {
                info!(job = %job.name, kind = job.action.kind(), elapsed_s, "cron job is waiting for an approval")
            }
            RoutineRunStatus::Running => {}
        }
        // Per-routine notification policy (docs/bot-runtime.md §5.15). A
        // silenced routine still records every run — "tell me only when it
        // breaks" is about the notification, not about the history.
        let mut delivery_failed = false;
        if job.notify.delivers(outcome.status) {
            if let Err(error) = self.notifier.notify(&outcome.title, &outcome.body).await {
                warn!(%error, job = %job.name, "failed to deliver cron job outcome");
                delivery_failed = true;
            }
        } else {
            info!(
                job = %job.name,
                notify = job.notify.as_str(),
                status = outcome.status.as_str(),
                "cron job outcome not delivered by its notification policy"
            );
        }
        // Settle the run best-effort (it already happened). The delivered
        // body lands on the run — success, failure and "waiting for an
        // approval" alike — so what ran stays queryable after the
        // notification is gone; `last_error` is reserved for trigger/config
        // problems.
        job.last_error = String::new();
        job.finish_run(&run_id, outcome.status, &outcome.body, outcome.session);
        if let Err(error) = self.jobs.update(job).await {
            warn!(%error, job = %job.name, "failed to record cron job outcome");
        }
        Some(FiredRun {
            status: outcome.status,
            delivery_failed,
        })
    }
    /// Wake every standing registration whose moment has come, and answer how
    /// many. Never fails the sweep: a wait that could not be woken this tick is
    /// still registered, and the next tick tries again.
    ///
    /// Two rules, in this order:
    ///
    /// 1. **Claim before firing.** `take` answers `false` when the row is
    ///    already gone, so two sweeps racing one registration — or a sweep
    ///    racing an arriving `/approve` — wake it exactly once.
    /// 2. **The log decides whether it is still waiting.** A registration is
    ///    the authority on *when* to come back, never on what the turn is
    ///    doing: one pointing at a turn that already resumed (or ended) is
    ///    stale, and firing it would run the same work twice. It is dropped —
    ///    the claim above already removed it — and named in the log.
    pub(crate) async fn fire_due_wakeups(&self, wiring: &WakeupWiring, now: i64) -> usize {
        let registrations = match wiring.registrations.list().await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(%error, "failed to read standing wakeups; nothing woken this tick");
                return 0;
            }
        };
        let mut fired = 0;
        for registration in registrations {
            let Some(cause) = registration.due_cause(now) else {
                continue;
            };
            match wiring.registrations.take(&registration.id).await {
                Ok(true) => {}
                // Somebody else got there first — an arriving answer, or
                // another sweep. Not an error, and not ours to fire.
                Ok(false) => continue,
                Err(error) => {
                    warn!(%error, id = %registration.id, "failed to claim a wakeup; leaving it for the next tick");
                    continue;
                }
            }
            if !self.still_waiting(wiring, &registration).await {
                warn!(
                    id = %registration.id,
                    session = %registration.session_id,
                    turn = ?registration.turn_id,
                    "dropping a wakeup whose turn is no longer waiting"
                );
                continue;
            }
            // No payload: a clock going off brings nothing with it, and the
            // turn is told which wait ended by the wait itself.
            match wiring.dispatch.fire(&registration, cause, "").await {
                Ok(()) => {
                    info!(
                        id = %registration.id,
                        session = %registration.session_id,
                        cause = cause.as_str(),
                        "woke a suspended turn"
                    );
                    fired += 1;
                }
                Err(error) => {
                    warn!(%error, id = %registration.id, cause = cause.as_str(), "failed to wake a turn")
                }
            }
        }
        fired
    }

    /// Whether the log still says this registration's turn is suspended.
    ///
    /// A registration with no turn starts a fresh one, so there is nothing to
    /// check — it is always live. A log that cannot be read answers **no**:
    /// waking a turn on a guess is the failure this check exists to prevent.
    async fn still_waiting(
        &self,
        wiring: &WakeupWiring,
        registration: &WakeupRegistration,
    ) -> bool {
        let Some(turn_id) = &registration.turn_id else {
            return true;
        };
        let events = match wiring.events.events(&registration.session_id).await {
            Ok(events) => events,
            Err(error) => {
                warn!(%error, session = %registration.session_id, "could not read the log to check a wakeup");
                return false;
            }
        };
        project_runs(&registration.session_id, &events)
            .iter()
            .find(|projected| projected.run.id == *turn_id)
            .is_some_and(|projected| projected.run.status == RunStatus::Suspended)
    }

    /// Dispatch one firing to the job's action.
    async fn execute(&self, job: &CronJob) -> JobOutcome {
        match &job.action {
            CronAction::Command {
                command,
                args,
                workdir,
                timeout_secs,
            } => {
                let (title, body, ok) = execute_cron_command(
                    &job.name,
                    command,
                    args,
                    workdir.as_deref(),
                    Duration::from_secs(*timeout_secs),
                )
                .await;
                JobOutcome {
                    title,
                    body,
                    status: match ok {
                        true => RoutineRunStatus::Ok,
                        false => RoutineRunStatus::Error,
                    },
                    session: None,
                }
            }
            CronAction::Agent {
                prompt,
                skills,
                workspace,
            } => {
                self.execute_cron_agent(job, prompt, skills, workspace.as_deref())
                    .await
            }
            // Nothing runs: the text *is* the outcome. It still goes out under
            // the job's `notify` policy and settles as a run like any other,
            // so a nudge is queryable after the notification is gone.
            CronAction::Message { text } => JobOutcome {
                title: format!("Komo「{}」", job.name),
                body: text.clone(),
                status: RoutineRunStatus::Ok,
                session: None,
            },
        }
    }

    /// Run an agent-mode job: one unattended turn on the cron runtime, its reply
    /// delivered. A per-run session keeps each scheduled run an isolated,
    /// cleanly-ledgered turn — no cross-run contamination — and its id is
    /// returned so the job can record where its transcript lives.
    async fn execute_cron_agent(
        &self,
        job: &CronJob,
        prompt: &str,
        skills: &[String],
        workspace: Option<&str>,
    ) -> JobOutcome {
        let name = &job.name;
        let fail_title = format!("Komo job「{name}」failed");
        let Some(handler) = &self.runtime else {
            return JobOutcome {
                title: fail_title,
                body: "agent-mode cron jobs need the gateway's cron runtime, which is not wired"
                    .to_string(),
                status: RoutineRunStatus::Error,
                session: None,
            };
        };
        // A fresh session per firing. What used to be encoded in the id
        // (`cron:{name}:{ts}`) is now the record's own `origin`, set from this
        // context when the turn opens it.
        let session_id = uuid::Uuid::now_v7().to_string();
        // Establish the turn's session *here*, marked unattended, rather than
        // letting `handle_input` build a plain detached one: that default is
        // `SessionOrigin::User`, which would hand the policy engine a `cron`
        // channel and quietly skip its unattended branch.
        let mut session = SessionContext::detached(&session_id).with_origin(SessionOrigin::Cron);
        // The job's own directory, when it named one: the same root the file
        // tools confine to and `shell` runs in. Already canonicalized and proven
        // to exist when the job was created — the sweep resolves nothing, so a
        // path cannot change meaning between approval and 03:00.
        if let Some(root) = workspace {
            session = session.with_workspace_roots(vec![std::path::PathBuf::from(root)]);
        }
        // …and this job's own approved actions, scoped to exactly this turn.
        // Installed around the whole turn (not per tool call) so the grants are
        // in scope wherever the approver is consulted, and out of scope the
        // moment the turn ends.
        match with_job_grants(
            job.granted_rules(),
            with_session(
                session,
                handler.handle(&session_id, cron_agent_prompt(prompt, skills)),
            ),
        )
        .await
        {
            Ok(reply) => {
                let reply = reply.trim();
                let body = if reply.is_empty() {
                    "(agent produced no output)".to_string()
                } else {
                    truncate_head(reply, JOB_OUTPUT_CAP)
                };
                JobOutcome {
                    title: format!("Komo job「{name}」"),
                    body,
                    status: RoutineRunStatus::Ok,
                    session: Some(session_id),
                }
            }
            // The turn stopped for an approval its grants don't cover. Not a
            // failure: it is parked on a standing wait and continues when the
            // operator answers — so what goes out is the question, not an
            // error report.
            Err(error) if is_suspended(&error) => JobOutcome {
                title: format!("Komo job「{name}」等待批准"),
                body: self.approval_notice(name, &session_id).await,
                status: RoutineRunStatus::Waiting,
                session: Some(session_id),
            },
            Err(e) => JobOutcome {
                title: fail_title,
                body: format!("agent turn failed: {e}"),
                status: RoutineRunStatus::Error,
                session: Some(session_id),
            },
        }
    }

    /// What the operator is told when a routine stopped for an approval: what
    /// it wants to do, and which wait to answer.
    ///
    /// Read back out of the two records the suspension left rather than passed
    /// down from it — the log says what the turn is waiting for, the
    /// registration says how to name it — because the wait's id does not exist
    /// until the registration is written, which is after the approver has
    /// already answered. Sending the prompt from the approver would hand the
    /// operator an id nothing will answer to.
    ///
    /// Only `/approve <id>` and `/deny <id>` are offered: `session` / `always`
    /// widen a grant, and an unattended turn's actions are approved one at a
    /// time or not at all.
    async fn approval_notice(&self, job: &str, session_id: &str) -> String {
        let Some(wait) = self.pending_wait(session_id).await else {
            warn!(
                job,
                session = session_id,
                "a routine is waiting for an approval that has no registration to answer; \
                 it will be re-registered at the next gateway start"
            );
            return format!(
                "routine「{job}」停下等待批准，但没能找到对应的等待登记。\
                 重启 gateway 后会补上；`komo run list` 可以看到这个 turn。"
            );
        };
        format!(
            "⚠️ routine「{job}」需要审批：{}\n回复 /approve {} 批准本次 · \
             /deny {} 拒绝（可写理由：/deny {} 别动生产库）",
            wait.summary, wait.id, wait.id, wait.id
        )
    }

    /// The wait a just-suspended routine turn is parked on: its id (what the
    /// operator answers with) and its summary (what it wants to do).
    ///
    /// The session is this firing's own — a fresh uuid per run — so the
    /// registration and the `turn/suspended` event that belong to it are
    /// unambiguous.
    async fn pending_wait(&self, session_id: &str) -> Option<PendingWait> {
        let wiring = self.wakeups.as_ref()?;
        let events = wiring.events.events(session_id).await.ok()?;
        let (turn_id, summary) = events.iter().rev().find_map(|event| match &event.kind {
            SessionEventKind::TurnSuspended(suspended) => {
                Some((suspended.turn_id.clone(), suspended.summary.clone()))
            }
            _ => None,
        })?;
        let id = wiring
            .registrations
            .list()
            .await
            .ok()?
            .into_iter()
            .find(|r| r.turn_id.as_deref() == Some(turn_id.as_str()))
            .map(|r| r.id)?;
        Some(PendingWait { id, summary })
    }
}

/// What one claimed firing came to, as the ingress that started it needs it.
struct FiredRun {
    status: RoutineRunStatus,
    /// The outcome was supposed to go somewhere and did not — the sweep's own
    /// failure, reported up so a broken home channel trips the breaker.
    delivery_failed: bool,
}

/// One firing's result, as the sweep delivers and records it.
struct JobOutcome {
    title: String,
    body: String,
    status: RoutineRunStatus,
    /// Ledger session of an agent run; `None` for command jobs.
    session: Option<String>,
}

/// The standing wait a suspended routine turn left behind.
struct PendingWait {
    id: String,
    summary: String,
}

/// Wrap an agent-job prompt with the skill-loading preamble (progressive
/// disclosure — the turn loads each named skill before acting). Pure, so the
/// wording is testable.
fn cron_agent_prompt(prompt: &str, skills: &[String]) -> String {
    if skills.is_empty() {
        prompt.to_string()
    } else {
        let list = skills.join(", ");
        format!(
            "First load {} skill(s) with the `skill` tool (action=view: {list}) and follow \
             the loaded instructions. Then carry out this task:\n\n{prompt}",
            skills.len()
        )
    }
}

/// Run one command-mode job and render the notification (title, body, success).
/// Free function so the outcome wording is testable without a store or notifier.
async fn execute_cron_command(
    name: &str,
    command: &str,
    args: &[String],
    workdir: Option<&str>,
    timeout: Duration,
) -> (String, String, bool) {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Dropping the wait future (timeout) must kill the process — a
        // runaway job can't outlive its budget as an orphan.
        .kill_on_drop(true);
    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }
    let fail_title = format!("Komo job「{name}」failed");
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return (
                fail_title,
                format!("could not start `{command}`: {e}"),
                false,
            );
        }
    };
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => {
            return (
                fail_title,
                format!("timed out after {}s (process killed)", timeout.as_secs()),
                false,
            );
        }
        Ok(Err(e)) => return (fail_title, format!("could not collect output: {e}"), false),
        Ok(Ok(output)) => output,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        // The script's stdout is the message (hermes' no_agent contract: the
        // wrapper formats its own push text). Head-capped — these messages
        // lead with the summary.
        let body = match stdout.trim() {
            "" => "(command produced no output)".to_string(),
            s => truncate_head(s, JOB_OUTPUT_CAP),
        };
        (format!("Komo job「{name}」"), body, true)
    } else {
        // Tail-capped: failure detail (a traceback, git's last words)
        // accumulates at the end.
        let mut combined = stdout.trim().to_string();
        if !stderr.trim().is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(stderr.trim());
        }
        let body = format!(
            "exit status: {}\n{}",
            output.status,
            truncate_tail(&combined, JOB_OUTPUT_CAP)
        );
        (fail_title, body, false)
    }
}

/// Keep the first `cap` bytes (on a char boundary), disclosing the cut.
fn truncate_head(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…(output truncated)", &s[..end])
}

/// Keep the last `cap` bytes (on a char boundary), disclosing the cut.
fn truncate_tail(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut start = s.len() - cap;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…(earlier output truncated)\n{}", &s[start..])
}

#[cfg(test)]
mod tests;
