use super::*;
use komo_core::domain::memory::{MemoryConfidence, MemoryContext, MemoryKind, MemoryStatus};

/// A `komo.db` in a home directory of this test's own, wiped first — a home
/// holds transcripts beside the db, and two tests sharing a directory would
/// read each other's conversations.
fn turso_url(name: &str) -> String {
    let home = std::env::temp_dir().join(format!("komo-mdb-{name}"));
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    format!("turso:{}", home.join("komo.db").display())
}

/// An existing `komo.db` created before `recall_count` and the truth-signal
/// columns existed must gain them **in place** on connect — additive ALTER, no
/// data loss — rather than force a destructive reset. Memories are durable
/// personal data; "delete the file" is not an available migration.
#[tokio::test]
async fn adds_missing_columns_in_place() {
    let home = std::env::temp_dir().join("komo-mdb-addcol");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    let path = home.join("komo.db");

    // 1. Seed a turso file with the OLD 15-column schema (no recall_count)
    //    and one row, then drop the handle.
    {
        let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.pragma_update("journal_mode", "'mvcc'").await.ok();
        conn.execute(
                "CREATE TABLE \"memory_records\" (\
                 \"id\" TEXT NOT NULL, \"kind\" TEXT NOT NULL, \"content\" TEXT NOT NULL, \
                 \"status\" TEXT NOT NULL, \"confidence\" TEXT NOT NULL, \"importance\" BIGINT NOT NULL, \
                 \"pinned\" BOOLEAN NOT NULL, \"scope_type\" TEXT NOT NULL, \"scope_key\" TEXT NOT NULL, \
                 \"source\" TEXT NOT NULL, \"source_message_id\" TEXT NOT NULL, \"created_at\" BIGINT NOT NULL, \
                 \"updated_at\" BIGINT NOT NULL, \"expires_at\" BIGINT NOT NULL, \"last_used_at\" BIGINT NOT NULL, \
                 PRIMARY KEY (\"id\"))",
                (),
            )
            .await
            .unwrap();
        conn.execute(
            "INSERT INTO \"memory_records\" VALUES \
                 ('mem-old', 'fact', 'a pre-migration memory', 'active', 'confirmed', 50, 0, \
                 'global', '', '', '', 100, 100, 0, 0)",
            (),
        )
        .await
        .unwrap();
    }
    // 2. Connect: the missing columns are added in place first, which is what
    //    makes the file readable at all.
    let db = Db::connect(&format!("turso:{}", path.display()))
        .await
        .unwrap();
    let rows = MemoryRepository::list(&db).await.unwrap();
    assert_eq!(rows.len(), 1, "the pre-migration row survives");
    assert_eq!(rows[0].content, "a pre-migration memory");
    assert_eq!(rows[0].recall_count, 0, "new column defaults to 0");
    // The truth-signal columns are additive too, and their defaults have to
    // read as "believed, nothing recorded" — a pre-migration memory must not
    // arrive contested or superseded.
    assert_eq!(
        rows[0].belief,
        komo_core::domain::memory::BeliefState::Current
    );
    assert_eq!(rows[0].support_count, 0);
    assert_eq!(rows[0].contradiction_count, 0);
    assert_eq!(rows[0].last_confirmed_at, None);
    assert!(rows[0].superseded_by.is_empty());
    assert!(rows[0].evidence.is_empty());

    // 3. The added columns are fully usable: a recall bump persists.
    db.mark_used(&[rows[0].id.clone()], 9_000).await.unwrap();
    let after = db.get("mem-old").await.unwrap().unwrap();
    assert_eq!(after.recall_count, 1);

    // …and so are the new ones: evidence and a contest survive a write.
    let mut memory = after;
    memory.record_evidence(
        "s-1",
        "s-1",
        komo_core::domain::memory::EvidenceRelation::Contradicts,
        "actually no",
        9_100,
    );
    memory.contest(9_100);
    db.save(&memory).await.unwrap();
    let reloaded = db.get("mem-old").await.unwrap().unwrap();
    assert_eq!(
        reloaded.belief,
        komo_core::domain::memory::BeliefState::Contested
    );
    assert_eq!(reloaded.contradiction_count, 1);
    assert_eq!(reloaded.evidence.len(), 1);
    assert_eq!(reloaded.evidence[0].session, "s-1");
    assert_eq!(reloaded.evidence[0].excerpt, "actually no");
}

#[tokio::test]
async fn save_list_roundtrip_and_overwrite() {
    let db = Db::connect(&turso_url("komo_memory_db_roundtrip.db"))
        .await
        .unwrap();
    let mut m = Memory::new(MemoryKind::Preference, "prefers concise answers");
    m.pinned = true;
    m.confidence = MemoryConfidence::UserWritten;
    m.scope = MemoryScope::Channel {
        platform: "telegram".into(),
        chat_id: "42".into(),
    };
    db.save(&m).await.unwrap();

    let rows = db.list().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "prefers concise answers");
    assert!(rows[0].pinned);
    assert_eq!(rows[0].confidence, MemoryConfidence::UserWritten);

    // Overwrite same id.
    let mut updated = m.clone();
    updated.content = "prefers terse answers".into();
    db.save(&updated).await.unwrap();
    let rows = db.list().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "prefers terse answers");
}

#[tokio::test]
async fn expired_hidden_from_list() {
    let db = Db::connect(&turso_url("komo_memory_db_expired.db"))
        .await
        .unwrap();
    db.save(&Memory::new(MemoryKind::Fact, "live"))
        .await
        .unwrap();
    let mut stale = Memory::new(MemoryKind::Fact, "stale");
    stale.expires_at = Some(1);
    db.save(&stale).await.unwrap();

    let rows = db.list().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "live");
}

#[tokio::test]
async fn recall_returns_in_scope_active_and_candidate_matches() {
    let db = Db::connect(&turso_url("komo_memory_db_recall.db"))
        .await
        .unwrap();

    // Relevant, active, global → recalled.
    db.save(&Memory::new(
        MemoryKind::Project,
        "the komo project is written in Rust",
    ))
    .await
    .unwrap();
    // Irrelevant → excluded by term overlap.
    db.save(&Memory::new(MemoryKind::Fact, "the user likes coffee"))
        .await
        .unwrap();
    // Relevant candidate → INCLUDED (so it can earn its recall signal for
    // the dreaming loop), though it ranks below the active hit.
    let mut cand = Memory::new(MemoryKind::Fact, "the rust toolchain is pinned to nightly");
    cand.status = MemoryStatus::Candidate;
    db.save(&cand).await.unwrap();
    // Relevant but rejected → excluded by status.
    let mut rejected = Memory::new(MemoryKind::Fact, "rust borrow checker notes");
    rejected.status = MemoryStatus::Rejected;
    db.save(&rejected).await.unwrap();
    // Relevant but scoped to another channel → excluded by scope.
    let mut other = Memory::new(MemoryKind::Fact, "rust edition is 2021");
    other.scope = MemoryScope::Channel {
        platform: "feishu".into(),
        chat_id: "oc_other".into(),
    };
    db.save(&other).await.unwrap();

    let ctx = MemoryContext::local("s1");
    let hits = db
        .recall(&ctx, "what language is the rust project in", 5)
        .await
        .unwrap();
    // Active + candidate both recalled; rejected and out-of-scope excluded.
    assert_eq!(hits.len(), 2);
    assert!(
        hits.iter()
            .any(|h| h.memory.content.contains("written in Rust"))
    );
    assert!(
        hits.iter()
            .any(|h| h.memory.status == MemoryStatus::Candidate)
    );
    assert!(
        !hits
            .iter()
            .any(|h| h.memory.status == MemoryStatus::Rejected)
    );
}

#[tokio::test]
async fn mark_used_sets_last_used_without_touching_updated_at() {
    let db = Db::connect(&turso_url("komo_memory_db_mark_used.db"))
        .await
        .unwrap();
    let mut m = Memory::new(MemoryKind::Fact, "recalled at least once");
    m.updated_at = 500;
    db.save(&m).await.unwrap();

    for at in [9_000, 9_100, 9_200, 9_300] {
        db.mark_used(&[m.id.clone()], at).await.unwrap();
    }

    let after = db.get(&m.id).await.unwrap().unwrap();
    assert_eq!(after.last_used_at, Some(9_300));
    assert_eq!(after.recall_count, 4, "each recall bumps the count");
    assert_eq!(after.updated_at, 500, "recall must not bump updated_at");
}
