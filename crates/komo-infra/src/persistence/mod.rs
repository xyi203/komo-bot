//! Persistence infra: the one toasty-backed connection (`komo.db`) over the
//! Turso engine, with a per-operation connection pool.
//!
//! `db` owns the connection and the session/run tables; `cron` and
//! (next door) `memory::memory_db` each hold one durable table's model and its
//! repository impl **for that same `Db`** — one file per domain, one database
//! (docs/adr/0004).
pub mod cron;
pub mod db;
pub mod session_event_store;
pub mod session_log;
pub mod wakeup;

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bring an existing `table` up to the current model shape by adding any
/// columns it lacks, in place — an additive `ALTER TABLE ADD COLUMN` so
/// existing rows take the default and **no data is lost**. Idempotent: a column
/// already present is skipped, so it is safe to run on every connect.
///
/// This is the in-place alternative to the "delete the db after a schema
/// change" reset that toasty's non-idempotent `push_schema` would otherwise
/// force. Toasty's typed API has no raw-DDL path, so the migration runs with a
/// direct `turso` handle, opened and dropped here — before toasty's pool
/// connects — so the two never contend for the file.
///
/// `expected` maps column name → full column DDL. Every column listed MUST be
/// `NOT NULL` with a `DEFAULT` (or be nullable), or `ALTER TABLE ADD COLUMN`
/// fails on a non-empty table.
pub(crate) async fn ensure_columns(
    path: &Path,
    table: &str,
    expected: &[(&str, &str)],
) -> anyhow::Result<()> {
    use anyhow::Context;

    let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
        .build()
        .await
        .with_context(|| format!("opening {} for column migration", path.display()))?;
    let conn = db.connect()?;
    // Match the engine mode the file was written in (the driver enables MVCC
    // under concurrent_writes); harmless if it was not.
    conn.pragma_update("journal_mode", "'mvcc'").await.ok();

    // Existing column names: PRAGMA table_info returns (cid, name, type, …) —
    // the name is column index 1.
    let mut existing = std::collections::HashSet::new();
    let mut rows = conn
        .query(&format!("PRAGMA table_info(\"{table}\")"), ())
        .await
        .with_context(|| format!("reading {table} columns"))?;
    while let Some(row) = rows.next().await? {
        if let turso::Value::Text(name) = row.get_value(1)? {
            existing.insert(name);
        }
    }
    // No columns → the table doesn't exist yet; leave it to toasty's push_schema.
    if existing.is_empty() {
        return Ok(());
    }

    for (name, ddl) in expected {
        if !existing.contains(*name) {
            conn.execute(&format!("ALTER TABLE \"{table}\" ADD COLUMN {ddl}"), ())
                .await
                .with_context(|| format!("adding column `{name}` to {table}"))?;
            tracing::info!(column = name, table, "added missing column in place");
        }
    }
    Ok(())
}

/// In-place migration for a whole table added to a model set after db files
/// already exist — the table-level sibling of [`ensure_columns`], and the same
/// raw-connection window (before the pooled driver opens the file).
///
/// `push_schema` only runs for brand-new files and is not idempotent, so
/// without this a new *table* meant deleting the db file — which is available
/// for nothing now: `komo.db` holds the durable tables too.
/// `ddl` must be the exact statements `push_schema` would emit for the model
/// (lock the parity with a test); they run only when `table` is absent.
pub(crate) async fn ensure_table(path: &Path, table: &str, ddl: &[&str]) -> anyhow::Result<()> {
    use anyhow::Context;

    let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
        .build()
        .await
        .with_context(|| format!("opening {} for table migration", path.display()))?;
    let conn = db.connect()?;
    conn.pragma_update("journal_mode", "'mvcc'").await.ok();

    // Any column at all means the table exists; PRAGMA on a missing table
    // yields no rows.
    let mut rows = conn
        .query(&format!("PRAGMA table_info(\"{table}\")"), ())
        .await
        .with_context(|| format!("probing for table {table}"))?;
    if rows.next().await?.is_some() {
        return Ok(());
    }

    for stmt in ddl {
        conn.execute(stmt, ())
            .await
            .with_context(|| format!("creating table `{table}` in place"))?;
    }
    tracing::info!(table, "created missing table in place");
    Ok(())
}

/// Shared prologue for the Turso-backed `connect(url)`: parse the
/// `turso:<path>` url to its bare filesystem path (`None` for in-memory),
/// ensure the parent dir exists, and report whether the live file is new (so
/// the caller knows to `push_schema`).
pub(crate) fn prepare_turso_path(url: &str) -> (Option<PathBuf>, bool) {
    let path = url
        .strip_prefix("turso:")
        .filter(|p| *p != ":memory:")
        .map(PathBuf::from);
    if let Some(p) = &path
        && let Some(dir) = p.parent()
    {
        std::fs::create_dir_all(dir).ok();
    }
    let is_new = path.as_deref().map(|p| !p.exists()).unwrap_or(true);
    (path, is_new)
}

/// Remove a test db and every sidecar Turso may leave next to it
/// (`-log`/`-wal`/`-shm`/`-journal`), so a reused temp path starts clean. A
/// stale MVCC `-log` against a fresh header is read as corruption, so this must
/// be thorough.
/// Exposed to dependent crates' tests through the `test-support` feature (the
/// agent's own tests reuse it), so it still never exists in a release build.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_test_db(path: &Path) {
    for suffix in ["", "-log", "-wal", "-shm", "-journal"] {
        let mut p = path.as_os_str().to_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
}

/// Connection-pool size for a Turso-backed db. Turso's file driver sets no
/// `max_connections` cap, so we pick one: enough for the gateway's handful of
/// concurrent sessions + maintenance sweeps, small enough that MVCC write-write
/// conflicts (which we retry) stay rare.
pub(crate) const DEFAULT_POOL_SIZE: usize = 8;

/// Total attempts for a write that hits an MVCC commit conflict (1 initial +
/// retries). Turso's concurrent-write mode (`BEGIN CONCURRENT`) lets writers run
/// in parallel, but conflicting transactions fail at commit and must be retried
/// by the caller — see [`with_write_retry`].
const WRITE_RETRY_MAX_ATTEMPTS: u32 = 5;

/// Whether a failed write should be retried: an MVCC commit conflict, or generic
/// busy/locked text. Conservative — anything unrecognized is not retried.
pub(crate) fn is_write_conflict(err: &anyhow::Error) -> bool {
    let s = format!("{err:#}").to_lowercase();
    s.contains("conflict")
        || s.contains("write-write")
        || s.contains("busy")
        || s.contains("locked")
}

/// Run a write closure, retrying on an MVCC commit conflict with short backoff.
/// Each attempt re-runs the whole closure (so it re-checks out its own pooled
/// connection and re-issues the transaction) — a conflicting write is retried
/// cleanly, never resumed mid-flight. Non-conflict errors surface immediately.
pub(crate) async fn with_write_retry<T, F, Fut>(mut op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Err(e) if attempt + 1 < WRITE_RETRY_MAX_ATTEMPTS && is_write_conflict(&e) => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(10 * attempt as u64)).await;
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod turso_schema_smoke {
    //! De-risk the schema shape db.rs relies on, on the Turso **MVCC** engine:
    //! UUID string keys (replacing the old `#[auto]` autoincrement, which MVCC
    //! rejects), `#[index]`, and `has_many`/`belongs_to` relations. Throwaway
    //! coverage — if Turso can't do these under MVCC, db.rs can't move.
    use toasty_driver_turso::Turso;

    #[derive(Debug, toasty::Model)]
    struct Parent {
        #[key]
        id: String,
        #[has_many]
        kids: toasty::Deferred<Vec<Kid>>,
    }

    #[derive(Debug, toasty::Model)]
    struct Kid {
        #[key]
        id: String,
        #[index]
        parent_id: String,
        #[belongs_to(key = parent_id, references = id)]
        parent: toasty::Deferred<Parent>,
        label: String,
    }

    #[tokio::test]
    async fn turso_mvcc_supports_uuid_keys_index_and_relations() {
        let db = toasty::Db::builder()
            .models(toasty::models!(Parent, Kid))
            .build(Turso::in_memory().concurrent_writes()) // MVCC on
            .await
            .unwrap();
        db.push_schema().await.unwrap();

        let mut conn = db.connection().await.unwrap();
        toasty::create!(Parent {
            id: "p1".to_string()
        })
        .exec(&mut conn)
        .await
        .unwrap();
        let p = Parent::get_by_id(&mut conn, "p1").await.unwrap();
        toasty::create!(in p.kids() { id: uuid::Uuid::now_v7().to_string(), label: "a".to_string() })
            .exec(&mut conn).await.unwrap();
        toasty::create!(in p.kids() { id: uuid::Uuid::now_v7().to_string(), label: "b".to_string() })
            .exec(&mut conn).await.unwrap();

        // index-backed relation query
        let kids = p.kids().exec(&mut conn).await.unwrap();
        assert_eq!(kids.len(), 2);
    }
}
