use super::*;

fn job() -> CronJob {
    CronJob::new_command("j", Trigger::cron("* * * * *"), "/bin/true", 0)
}

#[test]
fn a_run_is_claimed_running_and_settled_in_place() {
    let mut job = job();
    let id = job.begin_run(100);
    assert_eq!(job.last_run().unwrap().status, RoutineRunStatus::Running);
    job.finish_run(&id, RoutineRunStatus::Ok, "done", Some("s1".into()));
    let run = job.last_run().unwrap();
    assert_eq!(run.status, RoutineRunStatus::Ok);
    assert_eq!(run.output, "done");
    assert_eq!(run.session_id.as_deref(), Some("s1"));
    assert_eq!(job.runs.len(), 1, "settling must not append a second run");
}

#[test]
fn history_keeps_the_newest_runs_only() {
    let mut job = job();
    for n in 0..ROUTINE_RUN_HISTORY + 5 {
        let id = job.begin_run(n as i64);
        job.finish_run(&id, RoutineRunStatus::Ok, "", None);
    }
    assert_eq!(job.runs.len(), ROUTINE_RUN_HISTORY);
    assert_eq!(job.runs[0].started_at, 5);
    assert_eq!(
        job.last_run().unwrap().started_at,
        (ROUTINE_RUN_HISTORY + 4) as i64
    );
}

#[test]
fn a_long_body_is_capped_in_the_record() {
    let mut job = job();
    let id = job.begin_run(0);
    job.finish_run(&id, RoutineRunStatus::Ok, &"x".repeat(5_000), None);
    let output = &job.last_run().unwrap().output;
    assert!(output.chars().count() < 5_000);
    assert!(output.ends_with("(truncated)"), "{output}");
}

/// The approval prompt a waiting routine sends is not a result report — it
/// is the routine asking for something, so silencing results never silences
/// it.
#[test]
fn notify_policies_filter_results_but_never_a_waiting_routine() {
    use RoutineRunStatus::*;
    for status in [Ok, Error] {
        assert!(NotifyPolicy::Always.delivers(status));
        assert!(!NotifyPolicy::Never.delivers(status));
    }
    assert!(NotifyPolicy::OnError.delivers(Error));
    assert!(!NotifyPolicy::OnError.delivers(Ok));
    for policy in [
        NotifyPolicy::Always,
        NotifyPolicy::OnError,
        NotifyPolicy::Never,
    ] {
        assert!(policy.delivers(Waiting), "{policy:?}");
    }
}

/// A mangled row must never silence a routine — the one failure nobody
/// would notice.
#[test]
fn an_unreadable_notify_policy_reads_as_always() {
    assert_eq!(parse_notify_policy("garbage"), NotifyPolicy::Always);
    assert_eq!(parse_notify_policy(""), NotifyPolicy::Always);
    for policy in [
        NotifyPolicy::Always,
        NotifyPolicy::OnError,
        NotifyPolicy::Never,
    ] {
        assert_eq!(parse_notify_policy(policy.as_str()), policy);
    }
}
