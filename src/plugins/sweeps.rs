//! Scheduled maintenance as plugins — one per sweep, so `[plugins.dream]
//! enabled = false` and friends mean the same thing they mean for every other
//! plugin. Schedules are parsed by the host (the startup banner and the
//! sweeps must never disagree about what's in effect) and ride in on
//! [`SweepCx`].

use std::sync::Arc;

use async_trait::async_trait;

use komo_bot::daemon::{DreamSweep, ReviewSweep, Schedule};
use komo_bot::gateway::MaintenanceService;

use super::{Plugin, SweepCx, SweepRegistry};

pub struct ReviewPlugin;

#[async_trait]
impl Plugin for ReviewPlugin {
    fn name(&self) -> &'static str {
        "review"
    }

    async fn setup_sweeps(&self, reg: &mut SweepRegistry, cx: &SweepCx) -> anyhow::Result<()> {
        reg.sweep(MaintenanceService {
            name: "review".to_string(),
            schedule: cx.maintenance_schedule.clone(),
            maintenance: Arc::new(ReviewSweep {
                review: cx.review.clone(),
            }),
            alert: Some(cx.notifier.clone()),
        });
        Ok(())
    }
}

/// Routines (`komo cron add`, stored in `cron_job_records`): one every-minute
/// sweep reads the store and executes the ones whose slot has come, so jobs
/// added/removed/toggled while the gateway runs take effect on the next tick —
/// no restart. The same tick fires the standing wakeups, through the shared
/// [`RoutineEventSource`](komo_bot::daemon::RoutineEventSource) the host built
/// and handed here.
pub struct CronJobsPlugin;

#[async_trait]
impl Plugin for CronJobsPlugin {
    fn name(&self) -> &'static str {
        "cron-jobs"
    }

    async fn setup_sweeps(&self, reg: &mut SweepRegistry, cx: &SweepCx) -> anyhow::Result<()> {
        reg.sweep(MaintenanceService {
            name: "cron-jobs".to_string(),
            schedule: Schedule::parse("* * * * *")?,
            maintenance: Arc::new(cx.routines.sweep()),
            alert: Some(cx.notifier.clone()),
        });
        Ok(())
    }
}

/// Dreaming — mounts only when `dream_schedule` is in effect. Reads the whole
/// memory library, promotes well-supported candidates, and archives cold and
/// refuted ones.
pub struct DreamPlugin;

#[async_trait]
impl Plugin for DreamPlugin {
    fn name(&self) -> &'static str {
        "dream"
    }

    async fn setup_sweeps(&self, reg: &mut SweepRegistry, cx: &SweepCx) -> anyhow::Result<()> {
        let Some(schedule) = cx.dream_schedule.clone() else {
            return Ok(());
        };
        reg.sweep(MaintenanceService {
            name: "dreaming".to_string(),
            schedule,
            maintenance: Arc::new(DreamSweep {
                memories: cx.memories.clone(),
            }),
            alert: Some(cx.notifier.clone()),
        });
        Ok(())
    }
}
