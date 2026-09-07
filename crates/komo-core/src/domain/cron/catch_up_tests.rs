use super::*;

fn job(trigger: Trigger, next_run_at: i64, catch_up: CatchUp) -> CronJob {
    let mut j = CronJob::new(
        "j",
        trigger,
        CronAction::Command {
            command: "/bin/true".into(),
            args: Vec::new(),
            workdir: None,
            timeout_secs: 1,
        },
        next_run_at,
    );
    j.catch_up = catch_up;
    j
}

/// The bound is the job's own period, not a fixed grace: half an hour late
/// is nothing to a daily job and absurd for one that runs every five
/// minutes. A fixed window would have to be wrong for one of them.
#[test]
fn lateness_is_bounded_by_the_jobs_own_interval() {
    // 2026-01-01 08:00 local-ish; the exact epoch does not matter, only the
    // distances from it.
    let due = 1_767_225_600;
    let hour = 3_600;

    let daily = job(Trigger::cron("0 8 * * *"), due, CatchUp::Late);
    assert_eq!(daily.catch_up_verdict(due), CatchUpVerdict::OnTime);
    assert!(matches!(
        daily.catch_up_verdict(due + 3 * hour),
        CatchUpVerdict::Late { .. }
    ));
    // Slept through more than a whole day: the next slot is closer than the
    // one that was missed, so run that instead.
    assert!(matches!(
        daily.catch_up_verdict(due + 30 * hour),
        CatchUpVerdict::TooLate { .. }
    ));

    // Same 30 minutes, opposite answer, because the period differs.
    let every_five = job(Trigger::cron("*/5 * * * *"), due, CatchUp::Late);
    assert!(matches!(
        every_five.catch_up_verdict(due + 1800),
        CatchUpVerdict::TooLate { .. }
    ));
}

/// Some work is only correct at its hour — turning the lights off at 23:00
/// is not something to do at 09:00 the next morning, however "recent" the
/// miss looks against a daily period.
#[test]
fn skip_never_runs_late_however_small_the_miss() {
    let due = 1_767_225_600;
    let lights = job(Trigger::cron("0 23 * * *"), due, CatchUp::Skip);
    assert_eq!(lights.catch_up_verdict(due), CatchUpVerdict::OnTime);
    assert!(matches!(
        lights.catch_up_verdict(due + 60),
        CatchUpVerdict::TooLate { .. }
    ));
}

/// A one-shot has no later slot to wait for: running it late is the only
/// way it runs at all.
#[test]
fn a_one_shot_runs_however_late_it_is() {
    let due = 1_767_225_600;
    let once = job(Trigger::At { at: due }, due, CatchUp::Late);
    assert!(matches!(
        once.catch_up_verdict(due + 30 * 86_400),
        CatchUpVerdict::Late { .. }
    ));
}
