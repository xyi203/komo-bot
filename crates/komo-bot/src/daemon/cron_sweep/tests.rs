use super::*;
use komo_core::domain::cron::Trigger;
use komo_core::domain::session_event::WakeupCause;
use std::sync::Mutex;

// ── standing wakeups ─────────────────────────────────────────────────────

/// A dispatcher that records what it was asked to wake.
#[derive(Default)]
struct RecordingWake(Mutex<Vec<(String, WakeupCause)>>);

#[async_trait]
impl WakeupDispatch for RecordingWake {
    async fn fire(
        &self,
        registration: &WakeupRegistration,
        cause: WakeupCause,
        _payload: &str,
    ) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap()
            .push((registration.id.clone(), cause));
        Ok(())
    }
}

/// A `komo.db` of this test's own, holding both the registrations and the
/// session log they are checked against.
async fn wakeup_store(name: &str) -> Arc<komo_infra::persistence::db::Db> {
    let home = std::env::temp_dir().join(format!("komo-wksweep-{name}"));
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    Arc::new(
        komo_infra::persistence::db::Db::connect(&format!(
            "turso:{}",
            home.join("komo.db").display()
        ))
        .await
        .unwrap(),
    )
}

/// A turn in the log that opened and then stopped to wait.
async fn log_a_suspended_turn(
    db: &Arc<komo_infra::persistence::db::Db>,
    session: &str,
    turn: &str,
) {
    use komo_core::domain::session_event::{SessionEventKind, TurnSuspendedEvent, Wakeup};
    SessionEventRepository::append(
        db.as_ref(),
        session,
        vec![
            SessionEventKind::TurnStarted {
                turn_id: turn.into(),
                resumed_from: None,
            },
            SessionEventKind::TurnSuspended(TurnSuspendedEvent {
                turn_id: turn.into(),
                wakeup: Wakeup::UserReply,
                call_id: "c1".into(),
                summary: "waiting for an answer".into(),
                expires_at: None,
            }),
        ],
    )
    .await
    .unwrap();
    SessionEventRepository::durable_flush(db.as_ref(), session)
        .await
        .unwrap();
}

fn wakeup_sweep(
    db: &Arc<komo_infra::persistence::db::Db>,
    dispatch: Arc<RecordingWake>,
) -> Arc<RoutineEventSource> {
    Arc::new(RoutineEventSource {
        jobs: db.clone(),
        notifier: Arc::new(FakeNotifier::default()),
        runtime: None,
        wakeups: Some(WakeupWiring {
            registrations: db.clone(),
            events: db.clone(),
            dispatch,
        }),
    })
}

/// A due wait wakes its turn once, and the registration is gone with it —
/// so the next tick has nothing to fire. Two wakes for one wait would run
/// the same continuation twice.
#[tokio::test]
async fn a_due_wakeup_fires_once_and_then_is_gone() {
    use komo_core::domain::session_event::Wakeup;

    let db = wakeup_store("fires-once").await;
    log_a_suspended_turn(&db, "s1", "run-1").await;
    let now = 1_700_000_000;
    let registration = WakeupRegistration::new("s1", Wakeup::UserReply, now - 60)
        .continuing("run-1")
        .expiring_at(Some(now));
    WakeupRepository::save(db.as_ref(), &registration)
        .await
        .unwrap();

    let dispatch = Arc::new(RecordingWake::default());
    let sweep = wakeup_sweep(&db, dispatch.clone());

    assert_eq!(
        sweep
            .fire_due_wakeups(sweep.wakeups.as_ref().unwrap(), now)
            .await,
        1
    );
    assert_eq!(
        *dispatch.0.lock().unwrap(),
        vec![(registration.id.clone(), WakeupCause::Expired)]
    );

    // Nothing left to claim, so a second tick wakes nothing.
    assert_eq!(
        sweep
            .fire_due_wakeups(sweep.wakeups.as_ref().unwrap(), now)
            .await,
        0
    );
    assert_eq!(dispatch.0.lock().unwrap().len(), 1, "no second wake");
    assert!(
        WakeupRepository::list(db.as_ref())
            .await
            .unwrap()
            .is_empty(),
        "the claim retired it"
    );
}

/// The log is the authority on what a turn is doing. A registration
/// pointing at a turn that already came back is stale — firing it would
/// re-run work the continuation already did — so it is dropped, not fired.
#[tokio::test]
async fn a_wakeup_for_a_turn_that_already_resumed_is_dropped() {
    use komo_core::domain::session_event::{
        SessionEventKind, Wakeup, WakeupCause as Cause, WakeupFiredEvent,
    };

    let db = wakeup_store("stale").await;
    log_a_suspended_turn(&db, "s1", "run-1").await;
    // …and then it was woken by something else — an arriving `/approve`,
    // say — which is exactly the race the check exists for.
    SessionEventRepository::append(
        db.as_ref(),
        "s1",
        vec![SessionEventKind::WakeupFired(WakeupFiredEvent {
            turn_id: "run-1".into(),
            wakeup_id: String::new(),
            cause: Cause::Approve,
            payload: String::new(),
        })],
    )
    .await
    .unwrap();
    SessionEventRepository::durable_flush(db.as_ref(), "s1")
        .await
        .unwrap();

    let now = 1_700_000_000;
    let registration = WakeupRegistration::new("s1", Wakeup::UserReply, now - 60)
        .continuing("run-1")
        .expiring_at(Some(now));
    WakeupRepository::save(db.as_ref(), &registration)
        .await
        .unwrap();

    let dispatch = Arc::new(RecordingWake::default());
    let sweep = wakeup_sweep(&db, dispatch.clone());

    assert_eq!(
        sweep
            .fire_due_wakeups(sweep.wakeups.as_ref().unwrap(), now)
            .await,
        0
    );
    assert!(
        dispatch.0.lock().unwrap().is_empty(),
        "a turn that is running again must not be woken"
    );
    assert!(
        WakeupRepository::list(db.as_ref())
            .await
            .unwrap()
            .is_empty(),
        "and the stale registration is dropped rather than retried forever"
    );
}

/// A wait that ran out comes back as `expired` rather than being deleted: a
/// question nobody answered has to reach the turn that asked it.
#[tokio::test]
async fn a_wait_that_ran_out_wakes_as_expired() {
    use komo_core::domain::session_event::Wakeup;

    let db = wakeup_store("expired").await;
    log_a_suspended_turn(&db, "s1", "run-1").await;
    let created = 1_700_000_000;
    let registration =
        WakeupRegistration::new("s1", Wakeup::UserReply, created).continuing("run-1");
    let deadline = registration.expires_at.unwrap();
    WakeupRepository::save(db.as_ref(), &registration)
        .await
        .unwrap();

    let dispatch = Arc::new(RecordingWake::default());
    let sweep = wakeup_sweep(&db, dispatch.clone());
    let wiring = sweep.wakeups.as_ref().unwrap();

    assert_eq!(sweep.fire_due_wakeups(wiring, deadline - 1).await, 0);
    assert_eq!(sweep.fire_due_wakeups(wiring, deadline).await, 1);
    assert_eq!(
        dispatch.0.lock().unwrap()[0].1,
        WakeupCause::Expired,
        "and the turn is told nobody answered"
    );
}

/// The other direction of the same invariant: a turn the log says is
/// waiting, with nothing registered to wake it, is a turn parked forever.
/// The startup check adds the wait back, reading it out of the suspension
/// itself.
#[tokio::test]
async fn a_suspended_turn_nothing_is_watching_is_re_registered() {
    use komo_core::domain::session_event::Wakeup;

    let db = wakeup_store("recheck").await;
    log_a_suspended_turn(&db, "s1", "run-1").await;
    let events: Arc<dyn SessionEventRepository> = db.clone();
    let wakeups: Arc<dyn WakeupRepository> = db.clone();
    let now = 1_700_000_000;

    assert_eq!(
        reregister_suspended_turns(&events, &wakeups, 20, now).await,
        1
    );
    let registered = WakeupRepository::list(db.as_ref()).await.unwrap();
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0].turn_id.as_deref(), Some("run-1"));
    assert_eq!(
        registered[0].wakeup,
        Wakeup::UserReply,
        "read back out of the suspension, not guessed"
    );
    assert_eq!(
        registered[0].expires_at,
        Some(now + 7 * 86_400),
        "and it gets its variant's deadline, so it cannot hang forever"
    );

    // Idempotent: a second startup finds the wait already watched.
    assert_eq!(
        reregister_suspended_turns(&events, &wakeups, 20, now).await,
        0
    );
    assert_eq!(WakeupRepository::list(db.as_ref()).await.unwrap().len(), 1);
}

/// A turn that is not waiting must not have a wait invented for it.
#[tokio::test]
async fn a_running_or_finished_turn_is_not_re_registered() {
    use komo_core::domain::session_event::SessionEventKind;

    let db = wakeup_store("recheck-none").await;
    SessionEventRepository::append(
        db.as_ref(),
        "s1",
        vec![
            SessionEventKind::TurnStarted {
                turn_id: "run-1".into(),
                resumed_from: None,
            },
            SessionEventKind::TurnCompleted {
                turn_id: "run-1".into(),
            },
            SessionEventKind::TurnStarted {
                turn_id: "run-2".into(),
                resumed_from: None,
            },
        ],
    )
    .await
    .unwrap();
    SessionEventRepository::durable_flush(db.as_ref(), "s1")
        .await
        .unwrap();

    let events: Arc<dyn SessionEventRepository> = db.clone();
    let wakeups: Arc<dyn WakeupRepository> = db.clone();
    assert_eq!(
        reregister_suspended_turns(&events, &wakeups, 20, 1_700_000_000).await,
        0,
        "one finished turn and one still running: neither is waiting"
    );
}

/// A wake with no turn to continue starts one, so there is nothing in the
/// log to check it against — it must not be dropped for that.
#[tokio::test]
async fn a_wakeup_that_starts_a_fresh_turn_needs_no_suspended_turn() {
    use komo_core::domain::session_event::Wakeup;

    let db = wakeup_store("fresh").await;
    let now = 1_700_000_000;
    WakeupRepository::save(
        db.as_ref(),
        &WakeupRegistration::new("s1", Wakeup::UserReply, now - 60).expiring_at(Some(now)),
    )
    .await
    .unwrap();

    let dispatch = Arc::new(RecordingWake::default());
    let sweep = wakeup_sweep(&db, dispatch.clone());
    assert_eq!(
        sweep
            .fire_due_wakeups(sweep.wakeups.as_ref().unwrap(), now)
            .await,
        1
    );
}

// ── FakeNotifier ──────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeNotifier {
    calls: Mutex<Vec<(String, String)>>,
    fail: bool,
}

#[async_trait]
impl Notifier for FakeNotifier {
    async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()> {
        if self.fail {
            return Err(anyhow::anyhow!("notification failed"));
        }
        self.calls
            .lock()
            .unwrap()
            .push((title.to_string(), body.to_string()));
        Ok(())
    }
}

// ── CronJobSweep ──────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeCronRepo {
    jobs: Mutex<Vec<CronJob>>,
}

#[async_trait]
impl CronJobRepository for FakeCronRepo {
    async fn save(&self, job: &CronJob) -> anyhow::Result<()> {
        self.jobs.lock().unwrap().push(job.clone());
        Ok(())
    }
    async fn list(&self) -> anyhow::Result<Vec<CronJob>> {
        Ok(self.jobs.lock().unwrap().clone())
    }
    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<CronJob>> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.name == name)
            .cloned())
    }
    async fn update(&self, job: &CronJob) -> anyhow::Result<()> {
        let mut jobs = self.jobs.lock().unwrap();
        let slot = jobs
            .iter_mut()
            .find(|j| j.id == job.id)
            .ok_or_else(|| anyhow::anyhow!("not found"))?;
        *slot = job.clone();
        Ok(())
    }
    async fn delete(&self, name: &str) -> anyhow::Result<bool> {
        let mut jobs = self.jobs.lock().unwrap();
        let before = jobs.len();
        jobs.retain(|j| j.name != name);
        Ok(jobs.len() < before)
    }
}

/// A command job due now, running `/bin/sh -c <script>` with a 10s budget.
fn due_job(name: &str, script: &str) -> CronJob {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    CronJob::new(
        name,
        Trigger::cron("* * * * *"),
        CronAction::Command {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            workdir: None,
            timeout_secs: 10,
        },
        now,
    )
}

fn cron_sweep_with(
    jobs: Vec<CronJob>,
    notifier_fail: bool,
) -> (
    Arc<RoutineEventSource>,
    Arc<FakeCronRepo>,
    Arc<FakeNotifier>,
) {
    cron_sweep_full(jobs, notifier_fail, None)
}

fn cron_sweep_full(
    jobs: Vec<CronJob>,
    notifier_fail: bool,
    runtime: Option<Arc<dyn MessageHandler>>,
) -> (
    Arc<RoutineEventSource>,
    Arc<FakeCronRepo>,
    Arc<FakeNotifier>,
) {
    let repo = Arc::new(FakeCronRepo {
        jobs: Mutex::new(jobs),
    });
    let notifier = Arc::new(FakeNotifier {
        fail: notifier_fail,
        ..Default::default()
    });
    let sweep = Arc::new(RoutineEventSource {
        jobs: repo.clone(),
        notifier: notifier.clone(),
        runtime,
        wakeups: None,
    });
    (sweep, repo, notifier)
}

#[tokio::test]
async fn cron_job_success_delivers_stdout_and_reschedules() {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let (sweep, repo, notifier) =
        cron_sweep_with(vec![due_job("test-job", "echo hello-from-job")], false);
    let summary = sweep.sweep_due().await.unwrap();
    assert_eq!(summary.jobs_run, 1);
    let calls = notifier.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].0.contains("test-job"));
    assert_eq!(calls[0].1, "hello-from-job");
    let job = repo.jobs.lock().unwrap()[0].clone();
    assert!(job.next_run_at > now, "the fired slot is rescheduled");
    assert!(job.last_error.is_empty());
    let run = job.last_run().expect("the firing is recorded");
    assert_eq!(run.status, RoutineRunStatus::Ok);
    assert!(run.started_at > 0);
}

/// A message job runs nothing: the text is delivered verbatim and the
/// firing settles `ok` like any other run.
#[tokio::test]
async fn cron_message_job_delivers_its_text_verbatim() {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let job = CronJob::new(
        "meeting",
        Trigger::cron("* * * * *"),
        CronAction::Message {
            text: "下午3点开会".to_string(),
        },
        now,
    );
    let (sweep, repo, notifier) = cron_sweep_with(vec![job], false);
    let summary = sweep.sweep_due().await.unwrap();
    assert_eq!(summary.jobs_run, 1);
    let calls = notifier.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "下午3点开会");
    let stored = repo.jobs.lock().unwrap()[0].clone();
    let run = stored.last_run().expect("the firing is recorded");
    assert_eq!(run.status, RoutineRunStatus::Ok);
    assert_eq!(run.output, "下午3点开会");
    assert!(run.session_id.is_none(), "no turn ran");
}

#[tokio::test]
async fn cron_job_failure_records_and_delivers_exit_and_stderr() {
    let (sweep, repo, notifier) = cron_sweep_with(
        vec![due_job("test-job", "echo partial; echo boom >&2; exit 3")],
        false,
    );
    let summary = sweep.sweep_due().await.unwrap();
    assert_eq!(
        summary.jobs_run, 0,
        "a failed command is not a completed job"
    );
    let calls = notifier.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "failure is delivered, not just logged");
    assert!(calls[0].0.contains("failed"));
    assert!(
        calls[0].1.contains("3"),
        "exit code surfaces: {}",
        calls[0].1
    );
    assert!(calls[0].1.contains("partial"));
    assert!(calls[0].1.contains("boom"));
    let job = repo.jobs.lock().unwrap()[0].clone();
    let run = job.last_run().expect("the firing is recorded");
    assert_eq!(run.status, RoutineRunStatus::Error);
    assert!(
        run.output.contains("boom"),
        "the failure body is queryable after the notification: {}",
        run.output
    );
    assert!(
        job.last_error.is_empty(),
        "last_error is reserved for trigger problems"
    );
}

#[tokio::test]
async fn cron_job_skips_future_and_paused_jobs() {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let mut future = due_job("future", "echo nope");
    future.next_run_at = now + 3600;
    let mut disabled = due_job("disabled", "echo nope");
    disabled.status = CronJobStatus::Paused;
    let (sweep, repo, notifier) = cron_sweep_with(vec![future, disabled], false);
    let summary = sweep.sweep_due().await.unwrap();
    assert_eq!(summary.jobs_run, 0);
    assert!(notifier.calls.lock().unwrap().is_empty());
    // Neither was claimed or touched.
    assert!(repo.jobs.lock().unwrap().iter().all(|j| j.runs.is_empty()));
}

#[tokio::test]
async fn cron_job_broken_schedule_is_paused_not_run() {
    let mut job = due_job("broken", "echo nope");
    job.trigger = Trigger::cron("not a cron");
    let (sweep, repo, notifier) = cron_sweep_with(vec![job], false);
    let summary = sweep.sweep_due().await.unwrap();
    assert_eq!(summary.jobs_run, 0);
    assert!(
        notifier.calls.lock().unwrap().is_empty(),
        "the command never ran"
    );
    let job = repo.jobs.lock().unwrap()[0].clone();
    assert_eq!(
        job.status,
        CronJobStatus::Paused,
        "a broken schedule pauses the job"
    );
    assert!(job.last_error.contains("invalid schedule"));
    assert!(job.runs.is_empty(), "nothing ran, so nothing is recorded");
}

#[tokio::test]
async fn one_shot_job_runs_once_and_completes() {
    let mut job = due_job("once", "echo done-and-dusted");
    job.trigger = Trigger::At {
        at: job.next_run_at,
    };
    let (sweep, repo, notifier) = cron_sweep_with(vec![job], false);
    let summary = sweep.sweep_due().await.unwrap();
    assert_eq!(summary.jobs_run, 1);
    assert_eq!(notifier.calls.lock().unwrap().len(), 1, "outcome delivered");

    let job = repo.jobs.lock().unwrap()[0].clone();
    assert_eq!(job.status, CronJobStatus::Done, "one-shot completes");
    let run = job.last_run().expect("the firing is recorded");
    assert_eq!(run.status, RoutineRunStatus::Ok);
    assert!(
        run.output.contains("done-and-dusted"),
        "the output stays queryable on the row: {}",
        run.output
    );
    assert!(!job.is_due(i64::MAX), "a completed one-shot never re-fires");

    // A later sweep leaves it alone.
    let summary = sweep.sweep_due().await.unwrap();
    assert_eq!(summary.jobs_run, 0);
    assert_eq!(notifier.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cron_job_timeout_kills_and_reports() {
    let mut job = due_job("slow", "sleep 30");
    if let CronAction::Command { timeout_secs, .. } = &mut job.action {
        *timeout_secs = 1;
    }
    let (sweep, _repo, notifier) = cron_sweep_with(vec![job], false);
    let started = std::time::Instant::now();
    let summary = sweep.sweep_due().await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the wait must not outlive the budget"
    );
    assert_eq!(summary.jobs_run, 0);
    let calls = notifier.calls.lock().unwrap();
    assert!(calls[0].1.contains("timed out"), "got: {}", calls[0].1);
}

#[tokio::test]
async fn cron_job_spawn_error_is_delivered() {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let job = CronJob::new(
        "ghost",
        Trigger::cron("* * * * *"),
        CronAction::Command {
            command: "/nonexistent/komo-test-binary".into(),
            args: vec![],
            workdir: None,
            timeout_secs: 5,
        },
        now,
    );
    let (sweep, _repo, notifier) = cron_sweep_with(vec![job], false);
    let summary = sweep.sweep_due().await.unwrap();
    assert_eq!(summary.jobs_run, 0);
    let calls = notifier.calls.lock().unwrap();
    assert!(calls[0].1.contains("could not start"));
}

/// A fake agent handler that records (session_id, message), to exercise
/// agent-mode cron jobs.
struct FakeCronHandler {
    reply: String,
    seen: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl MessageHandler for FakeCronHandler {
    async fn handle(&self, session_id: &str, message: String) -> anyhow::Result<String> {
        self.seen
            .lock()
            .unwrap()
            .push((session_id.to_string(), message));
        Ok(self.reply.clone())
    }
}

/// Records the ambient session the sweep invoked it under. The approver
/// reads exactly this, so it is what pins the sweep's half of the
/// unattended contract — the `PolicyApprover` tests cover the other half.
#[derive(Default)]
struct OriginProbe {
    seen: Mutex<Option<Option<SessionOrigin>>>,
}

#[async_trait]
impl MessageHandler for OriginProbe {
    async fn handle(&self, _session_id: &str, _message: String) -> anyhow::Result<String> {
        *self.seen.lock().unwrap() =
            Some(komo_services::tool_execution::current_session().map(|c| c.origin));
        Ok("done".to_string())
    }
}

/// Reads the workspace root the sweep installed on the ambient session —
/// the same field `fs_common` and `shell` confine against.
#[derive(Default)]
struct WorkspaceProbe {
    seen: Mutex<Option<Option<std::path::PathBuf>>>,
}

#[async_trait]
impl MessageHandler for WorkspaceProbe {
    async fn handle(&self, _session_id: &str, _message: String) -> anyhow::Result<String> {
        *self.seen.lock().unwrap() = Some(
            komo_services::tool_execution::current_session().and_then(|c| c.workspace_root.clone()),
        );
        Ok("done".to_string())
    }
}

/// A job that named a directory must run confined to it. The turn reads the
/// root off its ambient session, so installing it anywhere else — or not at
/// all — leaves the job working in the gateway's own directory while
/// `cron list` says otherwise.
#[tokio::test]
async fn an_agent_job_with_a_workspace_runs_confined_to_it() {
    let probe = Arc::new(WorkspaceProbe::default());
    let (sweep, _repo, _notifier) = cron_sweep_full(
        vec![agent_job_in("tidy", "do it", vec![], Some("/srv/notes"))],
        false,
        Some(probe.clone()),
    );
    sweep.sweep_due().await.unwrap();
    assert_eq!(
        *probe.seen.lock().unwrap(),
        Some(Some(std::path::PathBuf::from("/srv/notes")))
    );
}

/// And a job that named none keeps the wired default, rather than being
/// pinned to some incidental directory.
#[tokio::test]
async fn an_agent_job_without_a_workspace_leaves_the_root_unset() {
    let probe = Arc::new(WorkspaceProbe::default());
    let (sweep, _repo, _notifier) = cron_sweep_full(
        vec![agent_job("tidy", "do it", vec![])],
        false,
        Some(probe.clone()),
    );
    sweep.sweep_due().await.unwrap();
    assert_eq!(*probe.seen.lock().unwrap(), Some(None));
}

/// A cron turn must reach the runtime already marked unattended. Left to
/// `handle_input`'s fallback it would get a plain detached context, whose
/// origin is `User` — and the policy engine would read `cron` as a channel.
#[tokio::test]
async fn cron_agent_job_runs_under_an_unattended_session() {
    let probe = Arc::new(OriginProbe::default());
    let (sweep, _repo, _notifier) = cron_sweep_full(
        vec![agent_job("brief", "do it", vec![])],
        false,
        Some(probe.clone()),
    );
    sweep.sweep_due().await.unwrap();
    assert_eq!(*probe.seen.lock().unwrap(), Some(Some(SessionOrigin::Cron)));
}

fn agent_job(name: &str, prompt: &str, skills: Vec<String>) -> CronJob {
    agent_job_in(name, prompt, skills, None)
}

fn agent_job_in(name: &str, prompt: &str, skills: Vec<String>, workspace: Option<&str>) -> CronJob {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    CronJob::new(
        name,
        Trigger::cron("* * * * *"),
        CronAction::Agent {
            prompt: prompt.to_string(),
            skills,
            workspace: workspace.map(str::to_string),
        },
        now,
    )
}

#[tokio::test]
async fn cron_agent_job_runs_turn_and_delivers_reply() {
    let handler = Arc::new(FakeCronHandler {
        reply: "本周值班：Alice".to_string(),
        seen: Mutex::new(Vec::new()),
    });
    let (sweep, repo, notifier) = cron_sweep_full(
        vec![agent_job(
            "brief",
            "总结告警轮换",
            vec!["alarmhandler".into()],
        )],
        false,
        Some(handler.clone()),
    );
    let summary = sweep.sweep_due().await.unwrap();
    assert_eq!(summary.jobs_run, 1);
    // The turn ran on a per-run session of its own, with the skill-load
    // preamble. The session is a plain uuid — what marks it a cron turn is
    // the context's `origin` (asserted in its own test below), not a shape
    // spelled into the id.
    let seen = handler.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(uuid::Uuid::parse_str(&seen[0].0).is_ok(), "{}", seen[0].0);
    assert!(
        seen[0].1.contains("alarmhandler"),
        "skill preamble: {}",
        seen[0].1
    );
    assert!(seen[0].1.contains("总结告警轮换"));
    // The reply was delivered and recorded — output and ledger session on
    // the row, so the run stays traceable after the notification is gone.
    assert_eq!(notifier.calls.lock().unwrap()[0].1, "本周值班：Alice");
    let job = repo.jobs.lock().unwrap()[0].clone();
    let run = job.last_run().expect("the firing is recorded");
    assert_eq!(run.status, RoutineRunStatus::Ok);
    assert_eq!(run.output, "本周值班：Alice");
    assert_eq!(run.session_id.as_deref(), Some(seen[0].0.as_str()));
}

// ── unattended approval (docs/bot-runtime.md §5.4) ───────────────────────

/// An approver that must never be consulted: the continuation's answer is
/// already in the log, and asking again would be asking the operator to
/// approve the same action twice.
#[derive(Default)]
struct MustNotAsk(Mutex<usize>);

#[async_trait]
impl komo_core::domain::approval::Approver for MustNotAsk {
    async fn decide(
        &self,
        _request: &komo_core::domain::approval::ApprovalRequest,
    ) -> komo_core::domain::approval::Decision {
        *self.0.lock().unwrap() += 1;
        komo_core::domain::approval::Decision::deny()
    }
}

/// What a turn is running as, read from inside its gated call — the one
/// place the ambient context exists. The continuation has to come back as
/// what it was: the permission engine reads `origin`, and the job's grants
/// are what let a routine act at all.
///
/// Registered under the gated tool's own name, so it replaces it in the
/// continuation's catalog and the scripted call reaches this instead.
#[derive(Default)]
struct ContextProbe {
    seen: Mutex<Option<(SessionOrigin, usize)>>,
}

#[async_trait]
impl komo_core::domain::tool::Tool for ContextProbe {
    fn name(&self) -> &'static str {
        "gated"
    }
    fn description(&self) -> &'static str {
        "asks for approval, then claims to have acted"
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        ctx: &komo_core::domain::context::ToolContext,
    ) -> Result<komo_core::domain::tool::ToolOutput, komo_core::domain::tool::ToolError> {
        let origin = komo_services::tool_execution::current_session()
            .map(|c| c.origin)
            .unwrap_or_default();
        let grants = komo_services::tool_execution::current_job_grants().len();
        *self.seen.lock().unwrap() = Some((origin, grants));

        let request = komo_core::domain::approval::ApprovalRequest::normal("delete the tree");
        let decision = ctx.decide(&request).await;
        match decision.is_allowed() {
            true => Ok(komo_core::domain::tool::ToolOutput::text("acted")),
            false => Err(komo_core::domain::tool::ToolError::Denied(
                decision.feedback().unwrap_or("refused").to_string(),
            )),
        }
    }
}

/// The conversation's runtime. A routine's continuation must never reach
/// it: its tool set is wider, it is fed the user's memory library, and its
/// approver answers on behalf of a human who is not in this turn.
#[derive(Default)]
struct ConversationHandler(Mutex<usize>);

#[async_trait]
impl komo_core::domain::gateway::MessageHandler for ConversationHandler {
    async fn handle(&self, _session_id: &str, _input: String) -> anyhow::Result<String> {
        *self.0.lock().unwrap() += 1;
        Ok("the conversation answered".into())
    }
    async fn resume_interrupted(
        &self,
        _run: &komo_core::domain::run::Run,
    ) -> anyhow::Result<Option<String>> {
        *self.0.lock().unwrap() += 1;
        Ok(Some("the conversation answered".into()))
    }
}

/// The gateway's two halves over one store: the routine runtime the sweep
/// drives (it suspends where the policy would have to ask) and the
/// dispatcher that brings the turn back when the operator answers. The
/// dispatcher holds both runtimes production does — the conversation's and
/// the routine's — so which one a wake picks is what these tests are about.
struct RoutineHarness {
    db: Arc<komo_infra::persistence::db::Db>,
    dispatcher: Arc<crate::interaction::GatewayDispatcher>,
    sweep: Arc<RoutineEventSource>,
    jobs: Arc<FakeCronRepo>,
    notifier: Arc<FakeNotifier>,
    asked: Arc<MustNotAsk>,
    continued_as: Arc<ContextProbe>,
    conversation: Arc<ConversationHandler>,
}

async fn routine_harness(name: &str, job: CronJob) -> RoutineHarness {
    routine_harness_with(name, job, false).await
}

/// `stops_again` gives the routine runtime a second ungranted action to meet
/// once the first is allowed — the continuation's own approval, which no
/// sweep is standing behind.
async fn routine_harness_with(name: &str, job: CronJob, stops_again: bool) -> RoutineHarness {
    use crate::interaction::{ApprovalState, GatewayDispatcher, TurnWaker, WaitParts};
    use crate::policy_approver::PolicyApprover;
    use crate::runtime::tests::{gated_runtime, sqlite_url, twice_gated_runtime};
    use crate::unattended::UnattendedSuspend;
    use komo_core::domain::policy::Policy;

    let db = Arc::new(
        komo_infra::persistence::db::Db::connect(&sqlite_url(name))
            .await
            .unwrap(),
    );
    let routine = Arc::new(gated_runtime(
        db.clone(),
        PolicyApprover::wrap(Policy::default(), Arc::new(UnattendedSuspend)),
    ));
    let asked = Arc::new(MustNotAsk::default());
    let continued_as = Arc::new(ContextProbe::default());
    let mut continuing = match stops_again {
        true => twice_gated_runtime(
            db.clone(),
            PolicyApprover::wrap(Policy::default(), Arc::new(UnattendedSuspend)),
        ),
        false => gated_runtime(db.clone(), asked.clone()),
    };
    continuing.tool_executor.register(continued_as.clone());
    let conversation = Arc::new(ConversationHandler::default());
    let notifier = Arc::new(FakeNotifier::default());
    let dispatcher = Arc::new(
        GatewayDispatcher::new(
            conversation.clone(),
            Arc::new(ApprovalState::new()),
            db.clone(),
            db.clone(),
            db.clone(),
            None,
            db.clone(),
            db.clone(),
        )
        .with_runtime(SessionOrigin::Cron, Arc::new(continuing))
        .with_notifier(notifier.clone())
        .with_waits(WaitParts {
            runs: db.clone(),
            events: db.clone(),
            wakeups: db.clone(),
        }),
    );
    let jobs = Arc::new(FakeCronRepo {
        jobs: Mutex::new(vec![job]),
    });
    let waker = Arc::new(TurnWaker::new(dispatcher.clone()));
    let sweep = Arc::new(RoutineEventSource {
        jobs: jobs.clone(),
        notifier: notifier.clone(),
        runtime: Some(routine),
        wakeups: Some(WakeupWiring {
            registrations: db.clone(),
            events: db.clone(),
            dispatch: waker,
        }),
    });
    RoutineHarness {
        db,
        dispatcher,
        sweep,
        jobs,
        notifier,
        asked,
        continued_as,
        conversation,
    }
}

/// An agent job carrying one grant of its own — inert for the gated call
/// under test (that request names no resource, so no rule can match it),
/// and present to prove the grants survive the wait.
fn granted_agent_job(name: &str) -> CronJob {
    use komo_core::domain::policy::{Category, Effect, Matcher, Rule, RuleSpec};
    let mut job = agent_job(name, "tidy up", vec![]);
    job.grants = vec![RuleSpec::from_rule(&Rule {
        channels: None,
        category: Category::Shell,
        matcher: Matcher::Prefix,
        value: "git".into(),
        access: None,
        effect: Effect::Allow,
        include_dangerous: false,
        unattended: true,
    })];
    job
}

/// The continuation is spawned, so the answer returns before the turn does.
async fn continuation_of(
    db: &Arc<komo_infra::persistence::db::Db>,
    suspended: &str,
) -> komo_core::domain::run::Run {
    use komo_core::domain::run::RunRepository;
    for _ in 0..200 {
        let found = RunRepository::list(db.as_ref(), 20)
            .await
            .unwrap()
            .into_iter()
            .find(|r| {
                r.resumed_from.as_deref() == Some(suspended) && r.status != RunStatus::Running
            });
        if let Some(run) = found {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the woken turn never finished");
}

/// Five hours pass before anyone looks at the prompt. Only the
/// registration's clock is involved: `waited_ms` measures from the moment
/// the question was put in front of the operator.
async fn age_the_wait(
    db: &Arc<komo_infra::persistence::db::Db>,
    wait: &WakeupRegistration,
    secs: i64,
) {
    assert!(WakeupRepository::take(db.as_ref(), &wait.id).await.unwrap());
    let mut aged = wait.clone();
    aged.created_at -= secs;
    WakeupRepository::save(db.as_ref(), &aged).await.unwrap();
}

/// The routine path end to end (docs/bot-runtime.md §5.4, and §8's second
/// criterion): a job with no grants meets an action the policy does not
/// cover, stops rather than failing, tells the operator which wait to
/// answer — and when they answer hours later, comes back and does it.
#[tokio::test]
async fn a_routine_stops_for_an_ungranted_action_and_acts_once_it_is_approved() {
    use crate::interaction::Answer;
    use komo_core::domain::run::RunRepository;

    let h = routine_harness("cron-wait-approve", granted_agent_job("nightly")).await;
    let summary = h.sweep.sweep_due().await.unwrap();
    assert_eq!(
        summary.jobs_run, 0,
        "a turn that stopped to ask has not run yet"
    );

    // The turn is parked, not finished and not broken.
    let suspended = RunRepository::list(h.db.as_ref(), 10)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(suspended.status, RunStatus::Suspended);
    let kinds: Vec<String> = SessionEventRepository::events(h.db.as_ref(), &suspended.session_id)
        .await
        .unwrap()
        .iter()
        .map(|event| {
            serde_json::to_value(&event.kind).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(kinds.iter().any(|k| k == "turn/suspended"), "{kinds:?}");

    // …and something is registered to bring it back.
    let waits = WakeupRepository::list(h.db.as_ref()).await.unwrap();
    assert_eq!(waits.len(), 1);
    let wait = waits[0].clone();
    assert_eq!(wait.turn_id.as_deref(), Some(suspended.id.as_str()));

    // The operator was handed that wait's id, and what it is for.
    let job = h.jobs.jobs.lock().unwrap()[0].clone();
    let run = job.last_run().expect("the firing is recorded").clone();
    assert_eq!(run.status, RoutineRunStatus::Waiting);
    assert_eq!(run.session_id.as_deref(), Some(wait.session_id.as_str()));
    assert!(run.output.contains(&wait.id), "{}", run.output);
    assert!(
        run.output.contains("delete the tree"),
        "the operator is told what it wants to do: {}",
        run.output
    );
    let delivered = h.notifier.calls.lock().unwrap()[0].clone();
    assert!(delivered.0.contains("nightly"), "{}", delivered.0);
    assert_eq!(delivered.1, run.output);

    // Five hours later, in a chat of their own, they allow it.
    age_the_wait(&h.db, &wait, 5 * 3_600).await;
    assert!(
        h.dispatcher
            .answer_approval("home-chat", Some(&wait.id), Answer::Once)
            .await,
        "a routine's wait is answerable from another session by id"
    );

    let continuation = continuation_of(&h.db, &suspended.id).await;
    assert_eq!(continuation.status, RunStatus::Done);
    assert_eq!(*h.asked.0.lock().unwrap(), 0, "nobody was asked twice");
    // What the turn was is what it comes back as: still unattended (so the
    // policy engine keeps evaluating it channel-lessly) and still holding
    // the job's own grants (so it can do the work it was granted).
    assert_eq!(
        *h.continued_as.seen.lock().unwrap(),
        Some((SessionOrigin::Cron, 1))
    );
    assert_eq!(
        *h.conversation.0.lock().unwrap(),
        0,
        "and it came back on the routine runtime, not the conversation's"
    );
    let steps = RunRepository::steps(h.db.as_ref(), &continuation.id)
        .await
        .unwrap();
    assert_eq!(steps.len(), 1, "the gated call ran exactly once");
    assert!(steps[0].ok, "{}", steps[0].error);
    assert_eq!(steps[0].result, "acted");
    // The audit half: who let it happen, and how long they took.
    assert_eq!(steps[0].approved_by, "human");
    // Measured against the real clock, so a second may pass between the
    // wait being back-dated and the answer landing.
    let waited = steps[0].approval_waited_ms;
    let five_hours = 5 * 3_600 * 1_000;
    assert!(
        (waited - five_hours).abs() < 5_000,
        "waited {waited}ms, expected ≈ {five_hours}ms"
    );
    assert!(
        WakeupRepository::list(h.db.as_ref())
            .await
            .unwrap()
            .is_empty(),
        "an answered wait is retired"
    );
}

/// §5.15's exception, end to end: a routine set to deliver *nothing* still
/// delivers the question it stopped on. Silencing results must never
/// silence a routine that is waiting for a person — nobody else is coming.
#[tokio::test]
async fn a_silenced_routine_still_asks_for_its_approval() {
    let mut job = granted_agent_job("nightly");
    job.notify = komo_core::domain::cron::NotifyPolicy::Never;
    let h = routine_harness("cron-wait-silenced", job).await;
    h.sweep.sweep_due().await.unwrap();

    let delivered = h.notifier.calls.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1, "the approval prompt went out anyway");
    assert!(delivered[0].0.contains("等待批准"), "{}", delivered[0].0);
    assert_eq!(
        h.jobs.jobs.lock().unwrap()[0].last_run().map(|r| r.status),
        Some(RoutineRunStatus::Waiting)
    );
}

/// The other answer. A refusal is not an error either: the turn comes back,
/// the tool is told no, and the routine finishes and reports as usual.
#[tokio::test]
async fn a_refused_routine_comes_back_and_does_not_act() {
    use crate::interaction::Answer;
    use komo_core::domain::run::RunRepository;

    let h = routine_harness("cron-wait-deny", agent_job("nightly", "tidy up", vec![])).await;
    h.sweep.sweep_due().await.unwrap();
    let suspended = RunRepository::list(h.db.as_ref(), 10)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let wait = WakeupRepository::list(h.db.as_ref())
        .await
        .unwrap()
        .pop()
        .unwrap();

    assert!(
        h.dispatcher
            .answer_approval(
                "home-chat",
                Some(&wait.id),
                Answer::Deny(Some("别动生产库".into())),
            )
            .await
    );

    let continuation = continuation_of(&h.db, &suspended.id).await;
    assert_eq!(continuation.status, RunStatus::Done);
    assert_eq!(*h.asked.0.lock().unwrap(), 0, "the answer was on record");
    let steps = RunRepository::steps(h.db.as_ref(), &continuation.id)
        .await
        .unwrap();
    assert_eq!(steps.len(), 1);
    // A refusal is a terminal, recoverable outcome: it rides back as the
    // model-facing result rather than as a tool failure — but it has to
    // say the action did not happen, and why.
    assert_ne!(steps[0].result, "acted", "the action was not taken");
    assert!(
        steps[0].result.contains("别动生产库"),
        "the model is told why: {}",
        steps[0].result
    );
    assert_eq!(steps[0].approved_by, "human");
}

/// The rest of §4.2: one answer rarely covers a whole job, so what matters
/// is what the *continuation* does when it meets a second action nobody
/// granted. It stops and asks again — which is only true because it runs on
/// the routine runtime. On the conversation's, the same action would come
/// back refused: its approver prompts a chat nobody is standing in.
///
/// And because no sweep is behind this turn, the dispatcher is what tells
/// the operator which wait to answer this time.
#[tokio::test]
async fn a_woken_routine_that_meets_another_ungranted_action_stops_again() {
    use crate::interaction::Answer;
    use komo_core::domain::run::RunRepository;

    let h = routine_harness_with("cron-wait-twice", granted_agent_job("nightly"), true).await;
    h.sweep.sweep_due().await.unwrap();
    let suspended = RunRepository::list(h.db.as_ref(), 10)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let first = WakeupRepository::list(h.db.as_ref())
        .await
        .unwrap()
        .pop()
        .unwrap();

    assert!(
        h.dispatcher
            .answer_approval("home-chat", Some(&first.id), Answer::Once)
            .await
    );

    let continuation = continuation_of(&h.db, &suspended.id).await;
    assert_eq!(
        continuation.status,
        RunStatus::Suspended,
        "it stopped to ask again rather than being refused"
    );
    assert_eq!(
        *h.conversation.0.lock().unwrap(),
        0,
        "which is what running on the routine runtime buys"
    );

    // A wait of its own, standing for the continuation…
    let second = WakeupRepository::list(h.db.as_ref())
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_ne!(second.id, first.id, "the answered wait was retired");
    assert_eq!(second.turn_id.as_deref(), Some(continuation.id.as_str()));

    // …and the operator hears about it, with the id to answer.
    let told = until_notified(&h.notifier, 2).await.pop().unwrap();
    assert!(told.1.contains(&second.id), "{}", told.1);
    assert!(told.1.contains("delete the tree"), "{}", told.1);
    assert!(
        !told.1.contains("/approve session") && !told.1.contains("/approve always"),
        "an unattended action is approved one at a time: {}",
        told.1
    );
}

/// The notifier's calls, once there are `want` of them. The continuation is
/// spawned, so its prompt lands after the run row does.
async fn until_notified(notifier: &Arc<FakeNotifier>, want: usize) -> Vec<(String, String)> {
    for _ in 0..200 {
        let calls = notifier.calls.lock().unwrap().clone();
        if calls.len() >= want {
            return calls;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the operator was never told");
}

#[tokio::test]
async fn cron_agent_job_without_runtime_reports_error() {
    let (sweep, repo, notifier) =
        cron_sweep_full(vec![agent_job("brief", "do it", vec![])], false, None);
    let summary = sweep.sweep_due().await.unwrap();
    assert_eq!(summary.jobs_run, 0);
    assert!(notifier.calls.lock().unwrap()[0].0.contains("failed"));
    assert_eq!(
        repo.jobs.lock().unwrap()[0].last_run().map(|r| r.status),
        Some(RoutineRunStatus::Error)
    );
}

#[test]
fn cron_agent_prompt_prepends_skill_load() {
    assert_eq!(cron_agent_prompt("do X", &[]), "do X");
    let p = cron_agent_prompt("do X", &["a".into(), "b".into()]);
    assert!(p.contains("action=view: a, b"));
    assert!(p.contains("do X"));
}

#[tokio::test]
async fn cron_job_notifier_failure_fails_the_cycle() {
    // Nothing reached the operator — that is the one outcome worth the
    // breaker (a failed *command* still returns Ok, it was delivered).
    let (sweep, repo, _notifier) = cron_sweep_with(vec![due_job("test-job", "echo hi")], true);
    assert!(sweep.sweep_due().await.is_err());
    // The slot was still claimed and the outcome still recorded.
    let job = repo.jobs.lock().unwrap()[0].clone();
    assert_eq!(job.last_run().map(|r| r.status), Some(RoutineRunStatus::Ok));
}

/// §5.15. "Only tell me when it breaks" silences the *notification*, never
/// the record — and never a routine that stopped to ask for something.
#[tokio::test]
async fn a_notify_policy_filters_delivery_but_not_the_run_history() {
    use komo_core::domain::cron::NotifyPolicy;

    for (policy, script, delivered) in [
        (NotifyPolicy::OnError, "echo fine", false),
        (NotifyPolicy::OnError, "exit 3", true),
        (NotifyPolicy::Never, "echo fine", false),
        (NotifyPolicy::Never, "exit 3", false),
        (NotifyPolicy::Always, "echo fine", true),
    ] {
        let mut job = due_job("quiet", script);
        job.notify = policy;
        let (sweep, repo, notifier) = cron_sweep_with(vec![job], false);
        sweep.sweep_due().await.unwrap();
        assert_eq!(
            notifier.calls.lock().unwrap().len(),
            usize::from(delivered),
            "{policy:?} + `{script}`"
        );
        let job = repo.jobs.lock().unwrap()[0].clone();
        let run = job.last_run().expect("every firing is recorded");
        assert_ne!(run.status, RoutineRunStatus::Running, "the run is settled");
        assert!(
            !run.output.is_empty(),
            "a silenced run still keeps its output: {policy:?}"
        );
    }
}

#[test]
fn job_output_truncation_keeps_boundaries_and_discloses() {
    assert_eq!(truncate_head("short", 100), "short");
    assert_eq!(truncate_tail("short", 100), "short");
    let long = "然".repeat(100); // 3 bytes per char — caps land mid-char
    let head = truncate_head(&long, 10);
    assert!(head.starts_with("然然然"));
    assert!(head.ends_with("…(output truncated)"));
    let tail = truncate_tail(&long, 10);
    assert!(tail.starts_with("…(earlier output truncated)"));
    assert!(tail.ends_with("然然然"));
}
