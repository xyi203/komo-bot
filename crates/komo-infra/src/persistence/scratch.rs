//! A call's durable working state: `scratch_records` in `komo.db`, and the
//! [`TurnScratch`] impl over them (`domain/scratch.rs`).
//!
//! **Disposable by row** — the executor clears a call's rows the moment the
//! call settles, and a deleted session takes its own with it. Nothing here is
//! an authority on anything: a lost row costs a continuation the work it had
//! already done, which is what happened to every suspended call before this
//! table existed.
//!
//! Addressed by (session, root turn, call, key) rather than by the running
//! turn — see `domain::scratch` for why the *root* of the attempt chain is the
//! only id every attempt at one question agrees on.

use async_trait::async_trait;

use super::db::Db;
use crate::persistence::with_write_retry;
use komo_core::domain::scratch::{ScratchKey, TurnScratch};

/// One scratch entry. The address is four plain columns rather than one
/// composite key: `clear` deletes by three of them, and a synthetic key would
/// have to be re-derived identically at every call site to find the rows it
/// covers.
#[derive(Debug, toasty::Model)]
pub(crate) struct ScratchRecord {
    #[key]
    id: String,

    #[index]
    session_id: String,

    /// The attempt chain's first turn — stable across continuations.
    root_turn_id: String,
    call_id: String,
    key: String,
    value: String,
    updated_at: i64,
}

/// The exact DDL `push_schema` emits for [`ScratchRecord`]. Needed because an
/// existing `komo.db` never re-runs `push_schema`, and this table arrived after
/// the file did — byte-parity is locked by `scratch_table_ddl_matches_push_schema`.
pub(crate) const SCRATCH_TABLE: &str = "scratch_records";
pub(crate) const SCRATCH_TABLE_DDL: &[&str] = &[
    "CREATE TABLE \"scratch_records\" (\"id\" TEXT NOT NULL, \"session_id\" TEXT NOT NULL, \
     \"root_turn_id\" TEXT NOT NULL, \"call_id\" TEXT NOT NULL, \"key\" TEXT NOT NULL, \
     \"value\" TEXT NOT NULL, \"updated_at\" BIGINT NOT NULL, PRIMARY KEY (\"id\"))",
    "CREATE INDEX \"index_scratch_records_by_session_id\" ON \"scratch_records\" (\"session_id\")",
];

/// This session's rows. Every operation here narrows them further in memory:
/// the index is on `session_id` alone, because a session holds a handful of
/// live scratch rows at most (one call's, cleared when it settles), so a second
/// index would cost every write to save nothing on the read.
///
/// Over `dyn Executor` rather than a connection, so the same read serves a
/// plain connection and a transaction — the callers below need both.
async fn rows_of(
    exec: &mut dyn toasty::Executor,
    session_id: &str,
) -> anyhow::Result<Vec<ScratchRecord>> {
    let session = session_id.to_string();
    Ok(toasty::query!(ScratchRecord FILTER .session_id == #session)
        .exec(exec)
        .await?)
}

/// Drop every scratch row a session holds — what `delete_session` and
/// `delete_empty_sessions` call. The rows are one call's working state, so a
/// session nobody can resume has no use for them.
///
/// Takes the caller's executor so it joins that caller's transaction: a session
/// row and its scratch go together or not at all.
pub(crate) async fn clear_session_scratch(
    exec: &mut dyn toasty::Executor,
    session_id: &str,
) -> anyhow::Result<()> {
    for record in rows_of(exec, session_id).await? {
        record.delete().exec(exec).await?;
    }
    Ok(())
}

#[async_trait]
impl TurnScratch for Db {
    async fn get(&self, key: &ScratchKey) -> anyhow::Result<Option<String>> {
        let mut conn = self.inner.connection().await?;
        let rows = rows_of(&mut conn, &key.session_id).await?;
        Ok(rows
            .into_iter()
            .find(|record| matches(record, key))
            .map(|record| record.value))
    }

    async fn set(&self, key: &ScratchKey, value: &str) -> anyhow::Result<()> {
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            // Read-then-write in one transaction, so two writes to the same key
            // conflict at commit and the loser re-runs cleanly rather than
            // leaving two rows for one address.
            let mut tx = conn.transaction().await?;
            let existing = rows_of(&mut tx, &key.session_id)
                .await?
                .into_iter()
                .find(|record| matches(record, key));
            let now = now();
            match existing {
                Some(mut record) => {
                    record
                        .update()
                        .value(value.to_string())
                        .updated_at(now)
                        .exec(&mut tx)
                        .await?;
                }
                None => {
                    toasty::create!(ScratchRecord {
                        id: uuid::Uuid::now_v7().to_string(),
                        session_id: key.session_id.clone(),
                        root_turn_id: key.root_turn_id.clone(),
                        call_id: key.call_id.clone(),
                        key: key.key.clone(),
                        value: value.to_string(),
                        updated_at: now,
                    })
                    .exec(&mut tx)
                    .await?;
                }
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn clear(
        &self,
        session_id: &str,
        root_turn_id: &str,
        call_id: &str,
    ) -> anyhow::Result<()> {
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let mut tx = conn.transaction().await?;
            for record in rows_of(&mut tx, session_id).await? {
                if record.root_turn_id == root_turn_id && record.call_id == call_id {
                    record.delete().exec(&mut tx).await?;
                }
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }
}

/// Whether a row is the one this key addresses. The three id columns all have
/// to agree: a different chain's attempt at the same call id is different work.
fn matches(record: &ScratchRecord, key: &ScratchKey) -> bool {
    record.root_turn_id == key.root_turn_id
        && record.call_id == key.call_id
        && record.key == key.key
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `komo.db` in a home of this test's own.
    fn url(name: &str) -> String {
        let home = std::env::temp_dir().join(format!("komo-scratch-{name}"));
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).expect("test home");
        format!("turso:{}", home.join("komo.db").display())
    }

    fn key(root: &str, call: &str, key: &str) -> ScratchKey {
        ScratchKey {
            session_id: "s1".into(),
            root_turn_id: root.into(),
            call_id: call.into(),
            key: key.into(),
        }
    }

    #[tokio::test]
    async fn a_value_comes_back_as_it_was_written() {
        let db = Db::connect(&url("round-trip")).await.unwrap();
        let k = key("turn-1", "call-0", "progress");
        assert_eq!(TurnScratch::get(&db, &k).await.unwrap(), None);

        TurnScratch::set(&db, &k, "step 3 of 5").await.unwrap();
        assert_eq!(
            TurnScratch::get(&db, &k).await.unwrap().as_deref(),
            Some("step 3 of 5")
        );
    }

    /// A call re-reaching the same step of its own work is restating it, not
    /// adding a second answer to the same question.
    #[tokio::test]
    async fn writing_a_key_twice_replaces_it() {
        let db = Db::connect(&url("upsert")).await.unwrap();
        let k = key("turn-1", "call-0", "progress");
        TurnScratch::set(&db, &k, "first").await.unwrap();
        TurnScratch::set(&db, &k, "second").await.unwrap();
        assert_eq!(
            TurnScratch::get(&db, &k).await.unwrap().as_deref(),
            Some("second")
        );
    }

    /// Settling clears the call that settled and nothing else — a round's other
    /// calls, and other keys of a later call, are still in flight.
    #[tokio::test]
    async fn clearing_a_call_leaves_the_rounds_other_calls_alone() {
        let db = Db::connect(&url("clear")).await.unwrap();
        for k in [
            key("turn-1", "call-0", "a"),
            key("turn-1", "call-0", "b"),
            key("turn-1", "call-1", "a"),
        ] {
            TurnScratch::set(&db, &k, "kept").await.unwrap();
        }

        TurnScratch::clear(&db, "s1", "turn-1", "call-0")
            .await
            .unwrap();
        assert_eq!(
            TurnScratch::get(&db, &key("turn-1", "call-0", "a"))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            TurnScratch::get(&db, &key("turn-1", "call-0", "b"))
                .await
                .unwrap(),
            None,
            "every key that call wrote, not just the one named"
        );
        assert_eq!(
            TurnScratch::get(&db, &key("turn-1", "call-1", "a"))
                .await
                .unwrap()
                .as_deref(),
            Some("kept")
        );
    }

    /// The address is the chain's *root*, so an unrelated chain that minted the
    /// same call id reads nothing — which is also what makes keying on the root
    /// safe rather than merely convenient.
    #[tokio::test]
    async fn another_chain_does_not_see_this_ones_work() {
        let db = Db::connect(&url("chains")).await.unwrap();
        TurnScratch::set(&db, &key("turn-1", "call-0", "a"), "mine")
            .await
            .unwrap();
        assert_eq!(
            TurnScratch::get(&db, &key("turn-9", "call-0", "a"))
                .await
                .unwrap(),
            None
        );
    }
}
