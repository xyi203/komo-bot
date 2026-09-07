use super::*;

#[test]
fn rejects_invalid_cron() {
    assert!(Schedule::parse("not a cron").is_err());
}

#[test]
fn next_fire_of_every_minute_is_within_a_minute() {
    let schedule = Schedule::parse("* * * * *").unwrap();
    let wait = schedule.next_after(Utc::now()).unwrap();
    assert!(wait <= Duration::from_secs(60));
}

#[test]
fn breaker_trips_only_after_max_consecutive_failures() {
    let mut failures = 0u32;
    // The first MAX-1 straight failures do not trip the breaker.
    for _ in 0..MAX_CONSECUTIVE_FAILURES - 1 {
        assert!(!breaker_tripped(&mut failures, false));
    }
    // The MAX-th straight failure trips it.
    assert!(breaker_tripped(&mut failures, false));
}

#[test]
fn breaker_resets_on_success() {
    let mut failures = 0u32;
    breaker_tripped(&mut failures, false);
    breaker_tripped(&mut failures, false);
    // A success clears the count so the next failure starts from one.
    breaker_tripped(&mut failures, true);
    assert_eq!(failures, 0);
    assert!(!breaker_tripped(&mut failures, false));
    assert_eq!(failures, 1);
}

/// A maintenance that always fails, counting its runs — for asserting the
/// supervisor keeps retrying after a breaker trip instead of dying.
struct AlwaysFail {
    runs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Maintenance for AlwaysFail {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        anyhow::bail!("always fails")
    }
}

#[tokio::test(start_paused = true)]
async fn supervise_recovers_after_breaker_trip_instead_of_dying() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let runs = std::sync::Arc::new(AtomicUsize::new(0));
    let maint: Arc<dyn Maintenance> = Arc::new(AlwaysFail { runs: runs.clone() });
    let schedule = Schedule::parse("* * * * *").unwrap();
    // A never-recovering sweep, run for ~30 virtual minutes (the paused
    // clock auto-advances through the cron waits and cooldowns). Before the
    // recovery change this would `bail!` after 5 failures; now it must keep
    // retrying across cooldowns and exit cleanly only on shutdown.
    let shutdown = tokio::time::sleep(Duration::from_secs(30 * 60));
    let result = supervise(&schedule, maint, "test", None, shutdown).await;
    assert!(
        result.is_ok(),
        "a tripped breaker must not error out the supervisor"
    );
    assert!(
        runs.load(Ordering::Relaxed) > MAX_CONSECUTIVE_FAILURES as usize,
        "supervisor should keep retrying after each cooldown, ran {}",
        runs.load(Ordering::Relaxed)
    );
}

// ── Schedule ──────────────────────────────────────────────────────────────

/// The sweep scheduler and the cron-job store must mean the same local
/// moment by the same expression — this is the alignment that keeps a
/// sweep's `"30 8 * * *"` from firing at 16:30 on a UTC+8 host.
#[test]
fn schedule_next_after_matches_cron_job_local_semantics() {
    let now = Utc::now();
    let schedule = Schedule::parse("30 8 * * *").unwrap();
    let wait = schedule.next_after(now).unwrap();
    let expected = next_occurrence_local("30 8 * * *", now.timestamp()).unwrap();
    assert_eq!(now.timestamp() + wait.as_secs() as i64, expected);
}
