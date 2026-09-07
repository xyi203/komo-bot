use super::*;
use std::sync::Mutex;

// ── DreamSweep ────────────────────────────────────────────────────────────

use komo_core::domain::memory::{
    DREAM_FORGET_AGE_DAYS, DREAM_MIN_SUPPORT, EvidenceRelation, Memory, MemoryConfidence,
    MemoryKind, MemoryRepository, MemoryStatus,
};

/// A memory store whose `save` overwrites by id (the real store is
/// create-or-replace), so a promotion is observable on the next `list`.
#[derive(Default)]
struct OverwriteMemories(Mutex<Vec<Memory>>);

#[async_trait]
impl MemoryRepository for OverwriteMemories {
    async fn list(&self) -> anyhow::Result<Vec<Memory>> {
        Ok(self.0.lock().unwrap().clone())
    }
    async fn save(&self, memory: &Memory) -> anyhow::Result<()> {
        let mut mems = self.0.lock().unwrap();
        if let Some(slot) = mems.iter_mut().find(|m| m.id == memory.id) {
            *slot = memory.clone();
        } else {
            mems.push(memory.clone());
        }
        Ok(())
    }
}

/// A candidate with `support` independent occasions of support behind it —
/// the signal promotion actually reads.
fn dream_candidate(id: &str, support: i64, age_days: i64, now: i64) -> Memory {
    let mut m = Memory::new(MemoryKind::Fact, "a candidate fact");
    m.id = id.to_string();
    m.status = MemoryStatus::Candidate;
    m.confidence = MemoryConfidence::Extracted;
    m.created_at = now - age_days * 86_400;
    for i in 0..support {
        m.record_evidence(
            &format!("s-{id}-{i}"),
            &format!("occ-{id}-{i}"),
            EvidenceRelation::Supports,
            "the user said so",
            now - 86_400,
        );
    }
    // `record_evidence` bumps `updated_at`; the age under test is `created_at`.
    m.created_at = now - age_days * 86_400;
    m
}

#[tokio::test]
async fn dream_sweep_promotes_and_archives() {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let promote = dream_candidate("mem-promote", DREAM_MIN_SUPPORT, 5, now);
    let archive = dream_candidate("mem-archive", 0, DREAM_FORGET_AGE_DAYS + 5, now);
    let keep = dream_candidate("mem-keep", 0, 1, now); // young, never recalled
    let (pid, aid, kid) = (promote.id.clone(), archive.id.clone(), keep.id.clone());

    let repo = Arc::new(OverwriteMemories(Mutex::new(vec![promote, archive, keep])));
    let sweep = DreamSweep {
        memories: repo.clone(),
    };
    let summary = sweep.run().await.unwrap();
    assert_eq!(summary.memories_promoted, 1);
    assert_eq!(summary.memories_archived, 1);

    let mems = repo.0.lock().unwrap();
    let by_id = |id: &str| mems.iter().find(|m| m.id == id).unwrap();
    // Promoted → active + inferred (evidence-proven, not user-confirmed), so
    // it recalls but stays ineligible for L1 pinning.
    assert_eq!(by_id(&pid).status, MemoryStatus::Active);
    assert_eq!(by_id(&pid).confidence, MemoryConfidence::Inferred);
    assert_eq!(by_id(&aid).status, MemoryStatus::Archived);
    assert_eq!(by_id(&kid).status, MemoryStatus::Candidate);
}

#[tokio::test]
async fn dream_sweep_never_promotes_to_pinnable() {
    // Even a heavily-recalled promotion must not become L1-eligible: pinning
    // stays a manual, confirmed-only path.
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let mut m = dream_candidate("mem-hot", 99, 1, now);
    m.kind = MemoryKind::Preference; // an identity kind
    let id = m.id.clone();
    let repo = Arc::new(OverwriteMemories(Mutex::new(vec![m])));
    DreamSweep {
        memories: repo.clone(),
    }
    .run()
    .await
    .unwrap();
    let mems = repo.0.lock().unwrap();
    let promoted = mems.iter().find(|m| m.id == id).unwrap();
    let ctx = komo_core::domain::memory::MemoryContext::local("s1");
    assert!(
        !promoted.is_pinnable(&ctx, now),
        "auto-promoted memory must not be pinnable"
    );
}
