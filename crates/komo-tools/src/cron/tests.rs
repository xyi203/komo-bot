use super::*;
use komo_core::domain::approval::Decision;
use komo_core::domain::cron::{RoutineRunStatus, Trigger};
use std::sync::Mutex;

#[derive(Default)]
struct FakeJobs {
    jobs: Mutex<Vec<CronJob>>,
}

#[async_trait]
impl CronJobRepository for FakeJobs {
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
        if let Some(slot) = jobs.iter_mut().find(|j| j.id == job.id) {
            *slot = job.clone();
        }
        Ok(())
    }
    async fn delete(&self, name: &str) -> anyhow::Result<bool> {
        let mut jobs = self.jobs.lock().unwrap();
        let before = jobs.len();
        jobs.retain(|j| j.name != name);
        Ok(jobs.len() != before)
    }
}

/// Records what it was asked and answers with a fixed verdict. Keeps the
/// detail and scope key too — for an `add` carrying grants they are the
/// substance of the prompt, not decoration.
struct Recorder {
    allow: bool,
    seen: Mutex<Vec<(String, komo_core::domain::approval::Risk)>>,
    details: Mutex<Vec<Option<String>>>,
    scope_keys: Mutex<Vec<Option<String>>>,
}

impl Recorder {
    fn new(allow: bool) -> Arc<Self> {
        Arc::new(Self {
            allow,
            seen: Mutex::new(Vec::new()),
            details: Mutex::new(Vec::new()),
            scope_keys: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl komo_core::domain::approval::Approver for Recorder {
    async fn decide(&self, request: &ApprovalRequest) -> Decision {
        self.seen
            .lock()
            .unwrap()
            .push((request.summary.clone(), request.risk));
        self.details.lock().unwrap().push(request.detail.clone());
        self.scope_keys
            .lock()
            .unwrap()
            .push(request.scope_key.clone());
        self.allow.into()
    }
}

fn tool(allow: bool) -> (CronTool, Arc<FakeJobs>, Arc<Recorder>) {
    let jobs = Arc::new(FakeJobs::default());
    let approver = Recorder::new(allow);
    let t = CronTool::new(jobs.clone() as Arc<dyn CronJobRepository>);
    (t, jobs, approver)
}

/// Run one call with `rec` as the turn's approver (it now rides on the
/// context, not the tool).
async fn run(t: &CronTool, args: Value, rec: &Arc<Recorder>) -> Result<ToolOutput, ToolError> {
    let ctx = ToolContext::new(
        komo_core::domain::context::SessionContext::detached("cli:test"),
        None,
        rec.clone(),
    );
    t.call(args, &ctx).await
}

#[tokio::test]
async fn add_agent_job_persists_after_approval() {
    let (t, jobs, rec) = tool(true);
    let out = run(&t, json!({"action": "add", "name": "morning-brief", "schedule": "0 8 * * *", "prompt": "总结我今天的日程", "skills": ["calendar"]}), &rec)
            .await
            .unwrap()
            .text;
    assert!(out.contains("morning-brief"), "{out}");
    assert!(out.contains("first run"), "{out}");

    let stored = jobs.jobs.lock().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].trigger, Trigger::cron("0 8 * * *"));
    let CronAction::Agent { prompt, skills, .. } = &stored[0].action else {
        panic!("agent job");
    };
    assert_eq!(prompt, "总结我今天的日程");
    assert_eq!(skills, &vec!["calendar".to_string()]);
    assert!(stored[0].next_run_at > 0, "schedule was resolved");

    let seen = rec.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, komo_core::domain::approval::Risk::Normal);
}

/// A nudge: `message` plus a relative delay, approved as ordinary work
/// because nothing runs when it fires.
#[tokio::test]
async fn add_message_job_from_a_relative_delay() {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let (t, jobs, rec) = tool(true);
    run(
        &t,
        json!({"action": "add", "name": "meeting", "after": "45m", "message": "下午3点开会"}),
        &rec,
    )
    .await
    .unwrap();

    let stored = jobs.jobs.lock().unwrap();
    let CronAction::Message { text } = &stored[0].action else {
        panic!("message job");
    };
    assert_eq!(text, "下午3点开会");
    assert!(matches!(stored[0].trigger, Trigger::At { .. }), "one-shot");
    assert!(stored[0].next_run_at > now + 45 * 60 - 60);
    assert_eq!(
        rec.seen.lock().unwrap()[0].1,
        komo_core::domain::approval::Risk::Normal
    );
}

/// The whole point of the feature: creating the job and approving what it
/// may do are **one** interaction, and the prompt spells the permissions out.
#[tokio::test]
async fn creating_a_job_approves_its_grants_in_the_same_prompt() {
    let (t, jobs, rec) = tool(true);
    let out = run(
        &t,
        json!({
            "action": "add", "name": "ac-temp", "schedule": "0 22 * * *",
            "prompt": "把卧室空调设到 26 度",
            "grants": [{"category": "homeassistant", "match": "exact",
                        "value": "climate.set_temperature"}]
        }),
        &rec,
    )
    .await
    .unwrap()
    .text;
    assert!(out.contains("ac-temp"), "{out}");

    // Exactly one prompt, and it names the action being granted.
    let seen = rec.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "creating the job must ask exactly once");
    let detail = rec.details.lock().unwrap()[0]
        .clone()
        .expect("a grant list must be shown before it is approved");
    assert!(detail.contains("climate.set_temperature"), "{detail}");
    assert!(detail.contains("homeassistant"), "{detail}");

    // A grant list must not ride on a session scope key: the next job's
    // permissions would then ride in on this job's approval.
    assert_eq!(rec.scope_keys.lock().unwrap()[0], None);

    // Stored, with the rule shape fixed rather than taken from the caller.
    let stored = jobs.jobs.lock().unwrap();
    assert_eq!(stored[0].grants.len(), 1);
    let rule = &stored[0].granted_rules()[0];
    assert!(
        rule.unattended,
        "a job grant must work with nobody watching"
    );
    assert!(!rule.include_dangerous);
    assert_eq!(rule.channels, None);
}

/// Denying the prompt leaves nothing behind — no job, and so no grants.
#[tokio::test]
async fn denying_the_prompt_creates_no_job_and_no_grants() {
    let (t, jobs, rec) = tool(false);
    let out = run(
        &t,
        json!({
            "action": "add", "name": "ac-temp", "schedule": "0 22 * * *",
            "prompt": "设到 26 度",
            "grants": [{"category": "homeassistant", "match": "exact",
                        "value": "climate.set_temperature"}]
        }),
        &rec,
    )
    .await
    .unwrap()
    .text;
    assert!(out.contains("rejected"), "{out}");
    assert!(jobs.jobs.lock().unwrap().is_empty());
}

/// A malformed grant fails **before** the prompt: approving a list and then
/// silently storing a different one is the failure this guards against.
#[tokio::test]
async fn an_invalid_grant_fails_before_asking() {
    let (t, jobs, rec) = tool(true);
    let err = run(
        &t,
        json!({
            "action": "add", "name": "x", "schedule": "0 22 * * *", "prompt": "do it",
            "grants": [{"category": "teleport", "match": "exact", "value": "somewhere"}]
        }),
        &rec,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
    assert!(
        rec.seen.lock().unwrap().is_empty(),
        "must not prompt for a list it cannot store"
    );
    assert!(jobs.jobs.lock().unwrap().is_empty());
}

/// An `add` without grants keeps its session scope key — the old behavior,
/// where approving "schedule jobs" once per session is the right trade.
#[tokio::test]
async fn an_add_without_grants_keeps_its_scope_key() {
    let (t, _jobs, rec) = tool(true);
    run(
        &t,
        json!({"action": "add", "name": "brief", "schedule": "0 8 * * *", "prompt": "summarize"}),
        &rec,
    )
    .await
    .unwrap();
    assert_eq!(
        rec.scope_keys.lock().unwrap()[0].as_deref(),
        Some("cron:add")
    );
}

#[tokio::test]
async fn command_job_is_gated_as_dangerous() {
    let (t, jobs, rec) = tool(true);
    run(
        &t,
        json!({"action": "add", "name": "rotate", "schedule": "0 14 * * 5",
                   "command": "/opt/rotate.py", "args": ["--push"]}),
        &rec,
    )
    .await
    .unwrap();
    let seen = rec.seen.lock().unwrap();
    assert_eq!(seen[0].1, komo_core::domain::approval::Risk::Dangerous);
    assert!(seen[0].0.contains("/opt/rotate.py --push"), "{}", seen[0].0);
    assert_eq!(jobs.jobs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn denied_add_stores_nothing() {
    let (t, jobs, rec) = tool(false);
    let out = run(
        &t,
        json!({"action": "add", "name": "x", "schedule": "0 8 * * *", "prompt": "hi"}),
        &rec,
    )
    .await
    .unwrap()
    .text;
    assert!(out.contains("rejected"), "{out}");
    assert!(jobs.jobs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn add_rejects_bad_schedule_and_missing_action_fields() {
    let (t, jobs, rec) = tool(true);
    // A schedule croner can't parse never reaches the store.
    assert!(
        run(
            &t,
            json!({"action": "add", "name": "x", "schedule": "nope", "prompt": "hi"}),
            &rec
        )
        .await
        .is_err()
    );
    // Neither prompt nor command.
    assert!(
        run(
            &t,
            json!({"action": "add", "name": "x", "schedule": "0 8 * * *"}),
            &rec
        )
        .await
        .is_err()
    );
    // Both.
    assert!(
        run(
            &t,
            json!({"action": "add", "name": "x", "schedule": "0 8 * * *",
                       "prompt": "hi", "command": "/bin/true"}),
            &rec
        )
        .await
        .is_err()
    );
    // No schedule at all.
    assert!(
        run(
            &t,
            json!({"action": "add", "name": "x", "prompt": "hi"}),
            &rec
        )
        .await
        .is_err()
    );
    assert!(jobs.jobs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn add_rejects_a_name_that_is_not_key_shaped() {
    let (t, jobs, rec) = tool(true);
    let err = run(
        &t,
        json!({"action": "add", "name": "morning brief", "schedule": "0 8 * * *", "prompt": "hi"}),
        &rec,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("invalid job name"), "{err}");
    assert!(jobs.jobs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn add_rejects_duplicate_name() {
    let (t, _jobs, rec) = tool(true);
    let add = json!({"action": "add", "name": "dup", "schedule": "0 8 * * *", "prompt": "hi"});
    run(&t, add.clone(), &rec).await.unwrap();
    let err = run(&t, add, &rec).await.unwrap_err().to_string();
    assert!(err.contains("already exists"), "{err}");
}

#[tokio::test]
async fn disable_then_enable_recomputes_next_run() {
    let (t, jobs, rec) = tool(true);
    run(
        &t,
        json!({"action": "add", "name": "j", "schedule": "0 8 * * *", "prompt": "hi"}),
        &rec,
    )
    .await
    .unwrap();

    let out = run(&t, json!({"action": "disable", "name": "j"}), &rec)
        .await
        .unwrap()
        .text;
    assert!(out.contains("Disabled"), "{out}");
    assert_eq!(jobs.jobs.lock().unwrap()[0].status, CronJobStatus::Paused);

    let out = run(&t, json!({"action": "enable", "name": "j"}), &rec)
        .await
        .unwrap()
        .text;
    assert!(out.contains("next"), "{out}");
    let stored = jobs.jobs.lock().unwrap();
    assert_eq!(stored[0].status, CronJobStatus::Active);
    assert!(stored[0].next_run_at > time::OffsetDateTime::now_utc().unix_timestamp());
}

#[tokio::test]
async fn run_makes_the_job_due_now() {
    let (t, jobs, rec) = tool(true);
    run(
        &t,
        json!({"action": "add", "name": "j", "schedule": "0 8 * * *", "prompt": "hi"}),
        &rec,
    )
    .await
    .unwrap();
    let out = run(&t, json!({"action": "run", "name": "j"}), &rec)
        .await
        .unwrap()
        .text;
    assert!(out.contains("due now"), "{out}");
    assert!(
        jobs.jobs.lock().unwrap()[0].next_run_at
            <= time::OffsetDateTime::now_utc().unix_timestamp()
    );
}

#[tokio::test]
async fn denied_management_leaves_the_job_alone() {
    let (t, jobs, rec) = tool(true);
    run(
        &t,
        json!({"action": "add", "name": "j", "schedule": "0 8 * * *", "prompt": "hi"}),
        &rec,
    )
    .await
    .unwrap();
    // Same tool and store, but this turn's approver denies.
    let denier = Recorder::new(false);
    let out = run(&t, json!({"action": "remove", "name": "j"}), &denier)
        .await
        .unwrap()
        .text;
    assert!(out.contains("kept"), "{out}");
    assert_eq!(jobs.jobs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unknown_job_errors_without_prompting() {
    let (t, _jobs, rec) = tool(true);
    for action in ["remove", "enable", "disable", "run"] {
        let err = run(&t, json!({"action": action, "name": "ghost"}), &rec)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no cron job named"), "{action}: {err}");
    }
    assert!(
        rec.seen.lock().unwrap().is_empty(),
        "a missing job must not raise an approval prompt"
    );
}

#[tokio::test]
async fn list_reports_schedule_state_and_last_outcome() {
    let (t, jobs, rec) = tool(true);
    assert_eq!(
        run(&t, json!({"action": "list"}), &rec).await.unwrap().text,
        "No scheduled jobs."
    );
    run(
        &t,
        json!({"action": "add", "name": "j", "schedule": "0 8 * * *",
                   "prompt": "a very long prompt that goes on and on"}),
        &rec,
    )
    .await
    .unwrap();
    {
        let mut stored = jobs.jobs.lock().unwrap();
        let id = stored[0].begin_run(1_700_000_000);
        stored[0].finish_run(&id, RoutineRunStatus::Error, "boom\nsecond line", None);
    }
    let out = run(&t, json!({"action": "list"}), &rec).await.unwrap().text;
    assert!(out.contains("j (agent) [cron `0 8 * * *`]"), "{out}");
    assert!(out.contains("last run"), "{out}");
    assert!(out.contains("error"), "{out}");
    assert!(out.contains("— boom second line"), "{out}");
}

/// "只有出问题才告诉我" reaches the store as the job's own policy, and the
/// listing says so — a silenced job that looked ordinary would be the kind
/// of surprise nobody debugs.
#[tokio::test]
async fn a_notify_policy_is_stored_and_listed() {
    let (t, jobs, rec) = tool(true);
    run(
        &t,
        json!({"action": "add", "name": "backup", "schedule": "0 3 * * *",
                   "prompt": "back things up", "notify": "on_error"}),
        &rec,
    )
    .await
    .unwrap();
    assert_eq!(
        jobs.jobs.lock().unwrap()[0].notify,
        komo_core::domain::cron::NotifyPolicy::OnError
    );
    let out = run(&t, json!({"action": "list"}), &rec).await.unwrap().text;
    assert!(out.contains("notify on_error"), "{out}");

    // Unstated stays today's behaviour, and is not called out in the list.
    run(
        &t,
        json!({"action": "add", "name": "brief", "schedule": "0 8 * * *", "prompt": "brief"}),
        &rec,
    )
    .await
    .unwrap();
    assert_eq!(
        jobs.jobs.lock().unwrap()[1].notify,
        komo_core::domain::cron::NotifyPolicy::Always
    );
}

/// The schedule is parsed where the model can be told it was wrong, not at
/// 03:00 against a store that already accepted it.
#[tokio::test]
async fn a_broken_schedule_is_refused_at_add() {
    let (t, jobs, rec) = tool(true);
    let err = run(
        &t,
        json!({"action": "add", "name": "j", "schedule": "not a cron", "prompt": "p"}),
        &rec,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not a cron"), "{err}");
    assert!(jobs.jobs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_action_errors() {
    let (t, _jobs, rec) = tool(true);
    assert!(
        run(&t, json!({"action": "frobnicate"}), &rec)
            .await
            .is_err()
    );
}

#[test]
fn oneline_flattens_and_caps_on_char_boundaries() {
    assert_eq!(oneline("a\n b  c", 40), "a b c");
    assert_eq!(oneline("日程日程日程", 3), "日程日…");
}

#[test]
fn the_model_facing_text_stays_short() {
    crate::test_support::assert_model_text_budget(&tool(true).0);
}
