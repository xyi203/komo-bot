use std::sync::Arc;

use async_trait::async_trait;
use tracing::{info, warn};

use komo_core::domain::memory::MemoryRepository;

use super::{Maintenance, MaintenanceSummary};

/// The "dreaming" consolidation sweep (OpenClaw's dreaming, adapted to komo's
/// governance ladder). Runs on a low-frequency schedule (e.g. nightly `0 3 * * *`)
/// and decides each candidate memory's fate from its accumulated evidence and
/// usage: a candidate corroborated on independent occasions (or explicitly
/// confirmed) is promoted to active, while one left refuted with nobody ruling
/// on it, or simply old and never recalled, is archived. **Truth is proven by
/// evidence and retention by use** — never the reverse, or a wrong memory
/// promotes itself by being retrieved.
/// Only candidates are ever touched — user-saved/active memories are left
/// to the operator (`komo memory report`) — and nothing is ever auto-*pinned*:
/// dreaming can promote into recall (L3) but never into the always-injected
/// profile (L1), which stays a manual, confirmed-only path.
///
/// On by default (nightly `0 3 * * *` via `dream_schedule`; set it to `"off"` to
/// disable). Wired in `cli/gateway.rs`.
pub struct DreamSweep {
    pub memories: Arc<dyn MemoryRepository>,
}

impl DreamSweep {
    /// Apply one dream cycle over all memories, returning what changed. Shared by
    /// the scheduled sweep and the `komo dream --apply` CLI. A promotion lifts a
    /// candidate to `Active` with `Inferred` confidence — usage-proven, but not
    /// user-confirmed, so it surfaces in recall yet stays ineligible for L1
    /// pinning (which requires confirmed/user-written). Per-memory failures are
    /// logged and skipped, never aborting the cycle.
    pub async fn apply(&self) -> anyhow::Result<MaintenanceSummary> {
        use komo_core::domain::memory::{
            DreamVerdict, MemoryConfidence, MemoryStatus, dream_verdict,
        };
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut summary = MaintenanceSummary::default();
        for mut memory in self.memories.list().await? {
            match dream_verdict(&memory, now) {
                DreamVerdict::Promote => {
                    memory.status = MemoryStatus::Active;
                    memory.confidence = MemoryConfidence::Inferred;
                    memory.updated_at = now;
                    match self.memories.save(&memory).await {
                        Ok(()) => {
                            summary.memories_promoted += 1;
                            info!(
                                id = %memory.id,
                                support = memory.support_count,
                                confirmed = memory.last_confirmed_at.is_some(),
                                "dream: promoted candidate to active"
                            );
                        }
                        Err(error) => {
                            warn!(%error, id = %memory.id, "dream: promote failed (skipped)")
                        }
                    }
                }
                DreamVerdict::Archive => {
                    memory.status = MemoryStatus::Archived;
                    memory.updated_at = now;
                    match self.memories.save(&memory).await {
                        Ok(()) => {
                            summary.memories_archived += 1;
                            info!(id = %memory.id, "dream: archived unused candidate");
                        }
                        Err(error) => {
                            warn!(%error, id = %memory.id, "dream: archive failed (skipped)")
                        }
                    }
                }
                DreamVerdict::Keep => {}
            }
        }
        Ok(summary)
    }
}

#[async_trait]
impl Maintenance for DreamSweep {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary> {
        self.apply().await
    }
}

#[cfg(test)]
mod tests;
