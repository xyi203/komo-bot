//! Background maintenance daemon.
//!
//! Borrowed from gbrain's `autopilot` supervisor (a long-running loop that runs
//! one work "cycle" on a schedule), trimmed to komo's needs:
//!
//!   - **cron-expression scheduling** — 5-field Unix syntax (`*/5 * * * *`) via
//!     `croner`, rather than gbrain's fixed interval seconds.
//!   - **single fixed maintenance action** — a sweep that runs the reflective
//!     reviewer over stored sessions, instead of gbrain's brain-sync cycle.
//!   - **circuit breaker** — stop after N consecutive failures so a permanent
//!     error (bad config, dead LLM) can't spin forever. This mirrors gbrain's
//!     `consecutiveErrors >= 5` cap / launchd `ThrottleInterval`.
//!
//! The OS-level supervisor install (launchd / systemd / crontab) that gbrain
//! also ships is intentionally left out of v0.1: this is the in-process loop
//! only, which a later `komo daemon --install` can wrap.

use croner::Cron;
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::Utc;
use tracing::{error, info, warn};

use crate::notify::Notifier;
use komo_core::domain::cron::next_occurrence_local;

pub mod config_watch;
mod cron_sweep;
mod dream_sweep;
mod review_sweep;

pub use cron_sweep::{
    CronJobSweep, RoutineEventSource, SUSPEND_RECHECK_SESSIONS, WakeupWiring,
    reregister_suspended_turns,
};
pub use dream_sweep::DreamSweep;
pub use review_sweep::ReviewSweep;

/// Trip the circuit breaker once this many maintenance cycles fail back-to-back.
/// Tripping no longer kills the service — it forces a cooldown before retrying
/// (see [`supervise`]).
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// Escalating cooldowns applied after successive breaker trips: a service that
/// keeps failing backs off further each time (capped at the last entry) instead
/// of hammering a broken dependency every cron tick. Crucially it never stops
/// permanently — an always-on personal agent must recover on its own once the
/// underlying problem (db lock, network) clears, without a gateway restart.
const BREAKER_COOLDOWNS: [Duration; 4] = [
    Duration::from_secs(60),
    Duration::from_secs(300),
    Duration::from_secs(900),
    Duration::from_secs(3600),
];

/// Bounded time to deliver the breaker alert so a hung notifier can't stall the
/// cooldown.
const BREAKER_ALERT_TIMEOUT: Duration = Duration::from_secs(10);

/// A parsed cron schedule. Validated with `croner` at parse time; the "when
/// does it next fire" math goes through `domain::cron::next_occurrence_local`
/// — the **same** function cron jobs use — so a sweep's `30 8 * * *` and a
/// job's mean the identical local-time moment. (Matching against `Utc::now()`
/// here is the bug that made a sweep configured for 8:30 fire at 16:30 on a
/// UTC+8 machine.)
#[derive(Clone)]
pub struct Schedule {
    expr: String,
}

impl Schedule {
    /// Parse a 5-field Unix cron expression (e.g. `0 * * * *` for hourly).
    pub fn parse(expr: &str) -> anyhow::Result<Self> {
        expr.parse::<Cron>()
            .map_err(|e| anyhow::anyhow!("invalid cron expression `{expr}`: {e}"))?;
        Ok(Self {
            expr: expr.to_string(),
        })
    }

    /// Duration from `now` until the next scheduled fire (strictly after `now`),
    /// matched against the **local** calendar.
    fn next_after(&self, now: chrono::DateTime<Utc>) -> anyhow::Result<Duration> {
        let next = next_occurrence_local(&self.expr, now.timestamp())?;
        Ok(Duration::from_secs((next - now.timestamp()).max(0) as u64))
    }
}

/// One scheduled unit of work. Kept behind a trait so the supervisor loop can be
/// exercised without a real reviewer or database.
#[async_trait]
pub trait Maintenance: Send + Sync {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary>;
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MaintenanceSummary {
    pub sessions_reviewed: usize,
    pub memories_written: usize,
    /// Candidate memories the dream sweep promoted to active this cycle.
    pub memories_promoted: usize,
    /// Candidate memories the dream sweep archived (never earned a recall) this cycle.
    pub memories_archived: usize,
    /// Cron-job commands that ran to a zero exit this sweep.
    pub jobs_run: usize,
    /// Standing waits this sweep woke — a timer that came due, or a wait that
    /// ran out and has to come back and say nobody answered.
    pub wakeups_fired: usize,
}

/// Update the consecutive-failure counter and report whether the circuit breaker
/// has tripped. Pulled out as a pure function so the breaker is unit-testable
/// without driving the real clock.
fn breaker_tripped(consecutive_failures: &mut u32, cycle_ok: bool) -> bool {
    if cycle_ok {
        *consecutive_failures = 0;
        false
    } else {
        *consecutive_failures += 1;
        *consecutive_failures >= MAX_CONSECUTIVE_FAILURES
    }
}

/// Run maintenance on `schedule` until `shutdown` resolves. Returns `Ok` on a
/// clean shutdown. The circuit breaker no longer stops the loop: after
/// [`MAX_CONSECUTIVE_FAILURES`] back-to-back failures it forces an escalating
/// cooldown (and alerts `alert`, if set) before retrying, so a transient outage
/// can't silently kill a sweep for the rest of the process's life — the sweep
/// recovers on its own once the underlying problem clears.
///
/// `name` labels the service in logs and the alert. `alert` is an optional
/// notifier for surfacing a tripped breaker to the operator's home channel
/// (best-effort, bounded) — otherwise the death would be invisible.
pub async fn supervise<S>(
    schedule: &Schedule,
    maintenance: Arc<dyn Maintenance>,
    name: &str,
    alert: Option<Arc<dyn Notifier>>,
    shutdown: S,
) -> anyhow::Result<()>
where
    S: std::future::Future<Output = ()>,
{
    tokio::pin!(shutdown);
    let mut consecutive_failures = 0u32;
    // How many times the breaker has tripped without a recovery in between —
    // indexes the escalating cooldown. Reset by any successful cycle.
    let mut trips = 0usize;

    loop {
        let wait = schedule.next_after(Utc::now())?;
        info!(
            service = name,
            seconds = wait.as_secs(),
            "next maintenance cycle scheduled"
        );

        tokio::select! {
            _ = &mut shutdown => {
                info!(service = name, "shutdown signal received; stopping daemon");
                return Ok(());
            }
            _ = tokio::time::sleep(wait) => {}
        }

        let started = std::time::Instant::now();
        let cycle_ok = match maintenance.run().await {
            Ok(summary) => {
                info!(
                    service = name,
                    sessions = summary.sessions_reviewed,
                    memories = summary.memories_written,
                    promoted = summary.memories_promoted,
                    archived = summary.memories_archived,
                    jobs = summary.jobs_run,
                    elapsed_s = started.elapsed().as_secs(),
                    "maintenance cycle complete"
                );
                true
            }
            Err(error) => {
                error!(service = name, %error, "maintenance cycle failed");
                false
            }
        };

        // Always update the consecutive-failure counter (a good cycle resets it).
        let tripped = breaker_tripped(&mut consecutive_failures, cycle_ok);
        if cycle_ok {
            // A good cycle clears the escalation ladder.
            trips = 0;
        } else if tripped {
            let cooldown = BREAKER_COOLDOWNS[trips.min(BREAKER_COOLDOWNS.len() - 1)];
            trips += 1;
            error!(
                service = name,
                failures = MAX_CONSECUTIVE_FAILURES,
                cooldown_s = cooldown.as_secs(),
                "circuit breaker tripped; cooling down before retrying (service not stopped)"
            );
            // Surface the trip to the operator — an unreachable sweep would
            // otherwise fail silently. Best-effort and bounded so a hung
            // notifier can't stall the cooldown.
            if let Some(alert) = &alert {
                let title = "⚠️ Komo 维护任务异常";
                let body = format!(
                    "维护任务「{name}」连续失败 {MAX_CONSECUTIVE_FAILURES} 次，暂停 {} 分钟后自动重试。",
                    (cooldown.as_secs() + 59) / 60
                );
                match tokio::time::timeout(BREAKER_ALERT_TIMEOUT, alert.notify(title, &body)).await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => warn!(service = name, %error, "failed to send breaker alert"),
                    Err(_) => warn!(service = name, "breaker alert timed out"),
                }
            }
            // Reset the window so the service gets a fresh set of attempts after
            // the cooldown rather than tripping again on the first failure.
            consecutive_failures = 0;
            tokio::select! {
                _ = &mut shutdown => {
                    info!(service = name, "shutdown during breaker cooldown; stopping daemon");
                    return Ok(());
                }
                _ = tokio::time::sleep(cooldown) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests;
