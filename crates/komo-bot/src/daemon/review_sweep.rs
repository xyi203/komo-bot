use std::sync::Arc;

use async_trait::async_trait;

use super::{Maintenance, MaintenanceSummary};

/// The fixed maintenance action: learn from every finished turn the interval
/// left behind, letting the extractor distill durable memories.
pub struct ReviewSweep {
    /// The shared coordinator (same instance as the runtime's post-run trigger,
    /// so the per-session in-flight guard spans both paths). The interval, the
    /// backlog scan, and the watermark all live there.
    pub review: Arc<crate::learning_coordinator::LearningCoordinator>,
}

#[async_trait]
impl Maintenance for ReviewSweep {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary> {
        let report = self
            .review
            .run(crate::learning_coordinator::LearningTrigger::Scheduled)
            .await?;
        Ok(MaintenanceSummary {
            sessions_reviewed: report.sessions_learned,
            memories_written: report.memories_written,
            ..Default::default()
        })
    }
}
