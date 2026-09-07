use super::*;
use komo_core::domain::memory::{MemoryConfidence, MemoryContext, MemoryKind, MemoryStatus};

/// A `komo.db` in a home directory of this test's own, wiped first. Its own
/// directory because `Db::connect` scans the one it opens for legacy files
/// to merge — two tests sharing a directory would merge each other's.
fn turso_url(name: &str) -> String {
    let home = std::env::temp_dir().join(format!("komo-mdb-{name}"));
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    format!("turso:{}", home.join("komo.db").display())
}

/// A legacy SQLite `memory.db` — written by the rusqlite backend, two
/// engines and one file merge ago — must still reach `komo.db`. That
/// migration used to run per store; the merge is now the only path there
/// is, so it has to cover the oldest file shape as well as the newest.
#[tokio::test]
async fn merges_a_legacy_sqlite_memory_db_into_komo_db() {
    let home = std::env::temp_dir().join("komo-mdb-legacy-sqlite");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    let path = home.join("memory.db");

    // 1. Seed a legacy SQLite file with two memories via the SQLite driver.
    {
        let sdb = toasty::Db::builder()
            .models(toasty::models!(MemoryRecord))
            .connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        sdb.push_schema().await.unwrap();
        let mut conn = sdb.connection().await.unwrap();
        for r in [
            record_from_memory(&Memory::new(MemoryKind::Project, "written in Rust")),
            record_from_memory(&Memory::new(MemoryKind::Fact, "likes coffee")),
        ] {
            toasty::create!(MemoryRecord {
                id: r.id,
                kind: r.kind,
                content: r.content,
                status: r.status,
                confidence: r.confidence,
                importance: r.importance,
                pinned: r.pinned,
                scope_type: r.scope_type,
                scope_key: r.scope_key,
                source: r.source,
                source_message_id: r.source_message_id,
                created_at: r.created_at,
                updated_at: r.updated_at,
                expires_at: r.expires_at,
                last_used_at: r.last_used_at,
                provenance: r.provenance,
                belief_state: r.belief_state,
                support_count: r.support_count,
                contradiction_count: r.contradiction_count,
                last_confirmed_at: r.last_confirmed_at,
                superseded_by: r.superseded_by,
                evidence: r.evidence,
                recall_count: r.recall_count,
                embedding: r.embedding,
                embedding_model: r.embedding_model,
            })
            .exec(&mut conn)
            .await
            .unwrap();
        }
    }

    // 2. Open `komo.db` beside it: the merge imports the rows and retires
    //    the old file.
    let komo = format!("turso:{}", home.join("komo.db").display());
    let db = Db::connect(&komo).await.unwrap();
    let mut contents: Vec<String> = MemoryRepository::list(&db)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.content)
        .collect();
    contents.sort();
    assert_eq!(contents, vec!["likes coffee", "written in Rust"]);
    assert!(
        !path.exists(),
        "the merged file is renamed, not left in place"
    );
    assert!(
        home.join("memory.db.merged-backup").exists(),
        "and kept: it was the only copy of durable data"
    );

    // 3. Add a row, reconnect: nothing re-imports (still 3, not 5).
    MemoryRepository::save(&db, &Memory::new(MemoryKind::Fact, "third"))
        .await
        .unwrap();
    drop(db);
    let db2 = Db::connect(&komo).await.unwrap();
    assert_eq!(
        MemoryRepository::list(&db2).await.unwrap().len(),
        3,
        "must not re-import"
    );
}

/// An existing memory.db created before `recall_count` and the truth-signal
/// columns existed must gain them **in place** on connect — additive ALTER, no
/// data loss — rather than force a destructive reset. Memories are durable
/// personal data; "delete the file" is not an available migration.
/// Every schema komo has ever shipped must still accept a write.
///
/// This is the guard the `recall_query_hashes` outage did not have. Removing
/// a field from the model removes the column from *new* files only; every
/// store already on disk keeps it, and a column that is `NOT NULL` without a
/// default then fails every insert that no longer mentions it. The store
/// reports no schema problem — it just stops accepting memories.
///
/// **When you remove a field from `MemoryRecord`, add its column name to
/// `RETIRED` in `ensure_columns`.** A snapshot below still creates that
/// column, so forgetting makes this test fail instead of making someone's
/// memory store fail silently, days later.
///
/// Adding a snapshot: paste the `CREATE TABLE` a released komo would have
/// written, and never edit an existing one — each is a record of a file
/// somebody may still be running.
#[tokio::test]
async fn every_shipped_schema_still_accepts_writes() {
    // The 15 columns before any of the recall/truth work.
    const ORIGINAL: &str = "\"id\" TEXT NOT NULL, \"kind\" TEXT NOT NULL, \"content\" TEXT NOT NULL, \
             \"status\" TEXT NOT NULL, \"confidence\" TEXT NOT NULL, \"importance\" BIGINT NOT NULL, \
             \"pinned\" BOOLEAN NOT NULL, \"scope_type\" TEXT NOT NULL, \"scope_key\" TEXT NOT NULL, \
             \"source\" TEXT NOT NULL, \"source_message_id\" TEXT NOT NULL, \"created_at\" BIGINT NOT NULL, \
             \"updated_at\" BIGINT NOT NULL, \"expires_at\" BIGINT NOT NULL, \"last_used_at\" BIGINT NOT NULL";

    let snapshots: &[(&str, String)] = &[
        ("2026-06 original", ORIGINAL.to_string()),
        (
            // 2026-07-03: dream promotion weighed query diversity. Retired
            // 2026-08-12 — this is the shape that stopped accepting writes.
            "2026-07-03 query diversity",
            format!(
                "{ORIGINAL}, \"recall_count\" BIGINT NOT NULL, \"recall_query_hashes\" TEXT NOT NULL"
            ),
        ),
    ];

    for (name, columns) in snapshots {
        let home =
            std::env::temp_dir().join(format!("komo-test-mem-schema-{}", name.replace(' ', "-")));
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).expect("test home");
        let path = home.join("memory.db");

        {
            let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            conn.pragma_update("journal_mode", "'mvcc'").await.ok();
            conn.execute(
                &format!("CREATE TABLE \"memory_records\" ({columns}, PRIMARY KEY (\"id\"))"),
                (),
            )
            .await
            .unwrap();
        }
        std::fs::write(
            crate::persistence::turso_marker_path(&path),
            b"turso-native\n",
        )
        .unwrap();

        // Merged into a fresh `komo.db` beside it: the old file's columns
        // are brought up to date before it is read, which is what makes an
        // ancient store readable at all.
        let db = Db::connect(&format!("turso:{}", home.join("komo.db").display()))
            .await
            .unwrap_or_else(|e| panic!("`{name}` must still open: {e}"));
        let mut memory = Memory::new(MemoryKind::Fact, "written after the upgrade");
        memory.id = "mem-after".to_string();
        db.save(&memory).await.unwrap_or_else(|e| {
            panic!("`{name}` must still accept a write — add the retired column to RETIRED: {e}")
        });

        let rows = db.list().await.unwrap();
        assert_eq!(rows.len(), 1, "`{name}`");
        assert_eq!(rows[0].content, "written after the upgrade", "`{name}`");
    }
}

#[tokio::test]
async fn adds_missing_columns_in_place() {
    let home = std::env::temp_dir().join("komo-mdb-addcol");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    let path = home.join("memory.db");

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
    // Mark it turso-native so it is read as one, not staged as sqlite.
    std::fs::write(
        crate::persistence::turso_marker_path(&path),
        b"turso-native\n",
    )
    .unwrap();

    // 2. Merge it into a fresh `komo.db`: the old file's missing columns are
    //    added in place first, which is what makes it readable at all.
    let db = Db::connect(&format!("turso:{}", home.join("komo.db").display()))
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
async fn pinned_filters_by_eligibility_and_scope() {
    let db = Db::connect(&turso_url("komo_memory_db_pinned.db"))
        .await
        .unwrap();

    // Eligible: pinned, active, user_written, preference, global.
    let mut good = Memory::new(MemoryKind::Preference, "concise answers");
    good.pinned = true;
    good.confidence = MemoryConfidence::UserWritten;
    db.save(&good).await.unwrap();

    // Not pinned.
    db.save(&Memory::new(MemoryKind::Preference, "not pinned"))
        .await
        .unwrap();

    // Pinned but candidate → excluded.
    let mut cand = Memory::new(MemoryKind::Profile, "candidate");
    cand.pinned = true;
    cand.confidence = MemoryConfidence::UserWritten;
    cand.status = MemoryStatus::Candidate;
    db.save(&cand).await.unwrap();

    let ctx = MemoryContext::local("s1");
    let pinned = db.pinned(&ctx).await.unwrap();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].content, "concise answers");
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

#[tokio::test]
async fn import_legacy_seeds_empty_db_only_once() {
    let dir = std::env::temp_dir().join("komo_memory_db_import_src");
    let _ = std::fs::remove_dir_all(&dir);
    let legacy = MdMemoryStore::new(dir.clone());
    legacy
        .save(&Memory::new(MemoryKind::Project, "uses Rust"))
        .await
        .unwrap();

    let db = Db::connect(&turso_url("komo_memory_db_import.db"))
        .await
        .unwrap();
    assert_eq!(db.import_legacy_markdown(&dir).await.unwrap(), 1);
    assert_eq!(db.list().await.unwrap().len(), 1);
    // Second call is a no-op (db non-empty).
    assert_eq!(db.import_legacy_markdown(&dir).await.unwrap(), 0);
    assert_eq!(db.list().await.unwrap().len(), 1);
}
