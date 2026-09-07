use super::*;

#[test]
fn new_command_job_is_active_with_default_timeout() {
    let job = CronJob::new_command(
        "weekly",
        Trigger::cron("0 14 * * 5"),
        "/opt/rotate.py",
        1000,
    );
    assert_eq!(job.status, CronJobStatus::Active);
    assert_eq!(job.action.kind(), "command");
    let CronAction::Command { timeout_secs, .. } = &job.action else {
        panic!("command job");
    };
    assert_eq!(*timeout_secs, DEFAULT_CRON_JOB_TIMEOUT_SECS);
    assert_eq!(job.next_run_at, 1000);
    assert!(job.runs.is_empty());
    assert_eq!(job.notify, NotifyPolicy::Always);
    assert!(!job.id.is_empty());
}

#[test]
fn agent_action_roundtrips_through_json() {
    let action = CronAction::Agent {
        prompt: "summarize my day".into(),
        skills: vec!["calendar".into()],
        workspace: Some("/srv/notes".into()),
    };
    let job = CronJob::new("brief", Trigger::cron("0 8 * * *"), action, 0);
    let json = serde_json::to_string(&job).unwrap();
    assert!(json.contains("\"kind\":\"agent\""));
    let back: CronJob = serde_json::from_str(&json).unwrap();
    assert_eq!(back.action.kind(), "agent");
    let CronAction::Agent {
        prompt,
        skills,
        workspace,
    } = &back.action
    else {
        panic!("agent job");
    };
    assert_eq!(prompt, "summarize my day");
    assert_eq!(skills, &vec!["calendar".to_string()]);
    assert_eq!(workspace.as_deref(), Some("/srv/notes"));
}

/// A job stored before agent jobs could name a workspace must still
/// deserialize — as one that names none.
#[test]
fn an_agent_job_written_without_a_workspace_reads_as_having_none() {
    let stored = r#"{"kind":"agent","prompt":"p","skills":[]}"#;
    let action: CronAction = serde_json::from_str(stored).unwrap();
    let CronAction::Agent { workspace, .. } = &action else {
        panic!("agent job");
    };
    assert!(workspace.is_none());
}

#[test]
fn due_requires_active_and_elapsed() {
    let mut job = CronJob::new_command("j", Trigger::cron("* * * * *"), "/bin/true", 100);
    assert!(job.is_due(100));
    assert!(job.is_due(101));
    assert!(!job.is_due(99));
    job.status = CronJobStatus::Paused;
    assert!(!job.is_due(200), "a paused job is never due");
    job.status = CronJobStatus::Done;
    assert!(!job.is_due(200), "a completed one-shot is never due");
}

#[test]
fn once_is_derived_from_the_trigger_shape() {
    let once = CronJob::new_command("o", Trigger::At { at: 1_900_000_000 }, "/bin/true", 100);
    assert!(once.is_once());
    let recurring = CronJob::new_command("r", Trigger::cron("0 8 * * *"), "/bin/true", 100);
    assert!(!recurring.is_once());
}

#[test]
fn job_status_roundtrip() {
    for status in [
        CronJobStatus::Active,
        CronJobStatus::Paused,
        CronJobStatus::Done,
    ] {
        assert_eq!(parse_cron_job_status(status.as_str()), status);
    }
    // A mangled row fires on schedule rather than silently stopping.
    assert_eq!(parse_cron_job_status("garbage"), CronJobStatus::Active);
    assert_eq!(parse_cron_job_status(""), CronJobStatus::Active);
}

#[test]
fn job_names_must_stay_key_shaped() {
    assert!(valid_cron_job_name("morning-brief"));
    assert!(valid_cron_job_name("weekly_alarm.rotation"));
    // A name is for the operator to read, so CJK is fine.
    assert!(valid_cron_job_name("每日简报"));
    assert!(!valid_cron_job_name(""));
    assert!(
        !valid_cron_job_name("morning brief"),
        "whitespace splits it"
    );
    assert!(
        !valid_cron_job_name("cron:brief"),
        "`:` structures the session id"
    );
    assert!(!valid_cron_job_name("a/b"));
    assert!(!valid_cron_job_name("a\\b"));
    assert!(!valid_cron_job_name("a\nb"));
    assert!(!valid_cron_job_name(&"x".repeat(MAX_CRON_JOB_NAME_LEN + 1)));
    assert!(valid_cron_job_name(&"x".repeat(MAX_CRON_JOB_NAME_LEN)));
}

#[test]
fn run_status_roundtrip() {
    for status in [
        RoutineRunStatus::Running,
        RoutineRunStatus::Ok,
        RoutineRunStatus::Error,
        RoutineRunStatus::Waiting,
    ] {
        assert_eq!(parse_routine_run_status(status.as_str()), status);
    }
    assert_eq!(
        parse_routine_run_status("waiting"),
        RoutineRunStatus::Waiting,
        "a routine parked on an approval must not read back as a failure"
    );
    assert_eq!(parse_routine_run_status("garbage"), RoutineRunStatus::Error);
}

#[test]
fn ids_are_unique_across_rapid_creation() {
    let a = CronJob::new_command("a", Trigger::cron("* * * * *"), "/bin/true", 0);
    let b = CronJob::new_command("b", Trigger::cron("* * * * *"), "/bin/true", 0);
    assert_ne!(a.id, b.id);
}
