use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use toasty_driver_turso::Turso;
use tracing::info;

use crate::memory::memory_db::MemoryRecord;
use crate::persistence::cron::CronJobRecord;
use crate::persistence::wakeup::{WAKEUP_TABLE, WAKEUP_TABLE_DDL, WakeupRecord};
use crate::persistence::{
    DEFAULT_POOL_SIZE, ensure_columns, ensure_table, prepare_turso_path,
    session_event_store::SessionEventStore, with_write_retry,
};

use komo_core::domain::{
    awaiting::{Awaiting, project_awaiting},
    context::SessionOrigin,
    home::HomeRepository,
    inbox::{InboundOrigin, InboxClaim, InboxRepository, UnfinishedInbound},
    message::Message,
    pairing::{
        APPROVE_LOCKOUT_SECS, APPROVE_MAX_FAILURES, ApproveOutcome, PAIRING_CODE_TTL_SECS,
        PairingRepository, PairingRequest, PairingStatus, parse_pairing_status, verify_code,
    },
    repository::{MessageRepository, SessionEventRepository, SessionRepository},
    run::{INTERRUPTED_ERROR, Run, RunRepository, RunStatus, RunStep, parse_run_status},
    run_projection::{ProjectedRun, RunProjectionStore, project_runs},
    session::{ChannelPeer, InboundPeer, Session},
    session_event::{
        SESSION_EVENT_VERSION, SessionEvent, SessionEventKind, SessionHeader, SurfaceProjection,
    },
    todo::{SessionTodoRepository, TodoItem},
};

// ── toasty models (infra-internal) ───────────────────────────────────────────

#[derive(Debug, toasty::Model)]
struct SessionRecord {
    #[key]
    id: String,
    created_at: i64,
    /// Immutable workspace identity chosen when the session is created.
    workspace: String,

    /// Operator-set display name (empty = untitled). Added additively via
    /// `SESSION_COLUMNS`; set through `SessionRepository::set_title`.
    title: String,

    /// Lifecycle status (`active` / `archive` / `deleted`). Additive column;
    /// set through `SessionRepository::set_status`. The list view hides
    /// `deleted`.
    status: String,

    /// Per-session model override (empty = the gateway's configured model) and
    /// reasoning effort (empty = the provider default). Additive columns; set
    /// through `SessionRepository::set_model`. Unlike `workspace` these are not
    /// creation-locked — a conversation may switch models mid-thread.
    model: String,
    effort: String,

    /// The correspondent this conversation answers, split into the chat
    /// platform and that platform's own id for the peer. Both empty for every
    /// local surface and for komo's own sessions. Additive columns; a channel
    /// finds its session by this pair (`find_by_peer`), which is what replaced
    /// deriving the session id from the address.
    channel_platform: String,
    channel_peer_id: String,

    /// What drives this conversation (`user` / `cron` / `delegate`). Additive
    /// column; decides titling, list visibility and learning eligibility. Was
    /// encoded in the id as a prefix.
    origin: String,

    /// The wait this session is stopped in, as JSON (empty = not waiting).
    /// Additive column, and a **cache**: `domain::awaiting::project_awaiting`
    /// folds it out of the log, `commit_awaiting` stores it, and an unreadable
    /// or cleared value costs a badge, never a fact.
    awaiting: String,
}

/// Columns added to `session_records` after a file was created.
const SESSION_COLUMNS: &[(&str, &str)] = &[
    ("title", "\"title\" text NOT NULL DEFAULT ''"),
    ("status", "\"status\" text NOT NULL DEFAULT 'active'"),
    (
        "workspace",
        "\"workspace\" text NOT NULL DEFAULT '__default__'",
    ),
    ("model", "\"model\" text NOT NULL DEFAULT ''"),
    ("effort", "\"effort\" text NOT NULL DEFAULT ''"),
    (
        "channel_platform",
        "\"channel_platform\" text NOT NULL DEFAULT ''",
    ),
    (
        "channel_peer_id",
        "\"channel_peer_id\" text NOT NULL DEFAULT ''",
    ),
    ("origin", "\"origin\" text NOT NULL DEFAULT 'user'"),
    ("awaiting", "\"awaiting\" text NOT NULL DEFAULT ''"),
];

/// Session-scoped working todo list (`domain/todo.rs`). One row per session;
/// `items` is the JSON-serialized `Vec<TodoItem>`. Disposable working state —
/// cleared at a `/new` conversation boundary.
#[derive(Debug, toasty::Model)]
struct SessionTodoRecord {
    #[key]
    session_id: String,
    items: String, // JSON array of TodoItem
    updated_at: i64,
}

#[derive(Debug, toasty::Model)]
struct PairingRecord {
    /// One row per sender: `{platform}:{sender_id}`.
    #[key]
    id: String,
    platform: String,
    sender_id: String,
    chat_id: String,
    code_hash: String, // salted SHA-256 of the code; plaintext never stored
    salt: String,
    status: String, // "pending" | "approved"
    created_at: i64,
}

/// Failure-lockout counter for the `komo pair approve` path. A singleton row
/// (`id = "approve"`); mirrors hermes' per-platform approve lockout.
#[derive(Debug, toasty::Model)]
struct LockoutRecord {
    #[key]
    id: String,
    failed_count: i64,
    locked_until: i64,
}

/// Generic key/value settings. One row per setting (`id` is the key); the home
/// channel set via `/sethome` lives under `id = "home_chat"`.
#[derive(Debug, toasty::Model)]
struct SettingRecord {
    #[key]
    id: String,
    value: String,
}

/// One agent turn in the run ledger (`domain/run.rs`, roadmap §7). `ended_at`
/// uses 0 as the "still running" sentinel (same convention as other optional
/// i64s here).
#[derive(Debug, toasty::Model)]
struct RunRecord {
    #[key]
    id: String,
    session_id: String,
    input: String,
    plan: String,
    status: String, // "running" | "done" | "failed"
    final_output: String,
    error: String,
    recoverable: bool,
    started_at: i64,
    ended_at: i64,

    /// Tokens the turn's model round-trips spent. Additive columns (see
    /// `RUN_COLUMNS`); 0 = unknown, which is what a pre-column row reads as.
    tokens_in: i64,
    tokens_out: i64,
    /// Cache-served part of `tokens_in`. Additive column; 0 = unknown.
    tokens_cached: i64,

    /// The memories that reached this run's prompt, as `RecalledMemories` JSON
    /// (`""` = none, which is also what a pre-column row reads as). Additive.
    memories: String,

    /// Run id this run continued from (journal resume). Additive column;
    /// empty = none, same convention as `structured`.
    resumed_from: String,

    /// The learning pass has consumed this run. Additive column; a pre-column
    /// row reads as `false`, which offers it to the pass once — the extractor's
    /// own dedup makes a re-read harmless.
    learned: bool,

    /// Serialized `OutcomeAssessment`. Additive column; empty = never assessed.
    outcome: String,
}

/// One tool invocation within a run. `run_id` indexes back to [`RunRecord`];
/// `seq` orders steps within a run.
#[derive(Debug, toasty::Model)]
struct RunStepRecord {
    // UUIDv7 string key: MVCC rejects AUTOINCREMENT.
    // Assigned at insert (`RunRepository::append_step`).
    #[key]
    id: String,

    #[index]
    run_id: String,

    seq: i64,
    tool_name: String,
    args: String,
    result: String,
    error: String,
    ok: bool,

    /// `!ok` but the call may still have taken effect (`domain::run::RunStep`).
    /// Additive column.
    uncertain: bool,

    started_at: i64,
    ended_at: i64,

    /// Measured call duration in milliseconds. Additive column (see
    /// `STEP_COLUMNS`); `started_at`/`ended_at` are whole seconds and can't
    /// express a sub-second call.
    elapsed_ms: i64,

    /// `ToolOutput::structured` as JSON text; empty string = none (which is also
    /// what a row written before the column reads as). Additive column.
    structured: String,

    /// Newline-separated paths of stored full outputs; empty = none. Additive
    /// column. A list, not JSON: the entries are paths, and `split('\n')` on the
    /// read side beats a nested parse.
    output_paths: String,

    /// Which rung of the permission ladder let this call happen, projected from
    /// its `approval/resolved` event. Empty = never gated. Additive column.
    approved_by: String,

    /// How long that approval waited to be answered, in milliseconds. 0 = never
    /// gated, or answered instantly. Additive column.
    approval_waited_ms: i64,
}

/// One inbound message the gateway has seen (`domain/inbox.rs`). The key is
/// `<platform>:<message_id>` rather than the UUIDv7 used everywhere else in
/// this file: dedupe wants the collision, and the primary key is what makes
/// "already handled" atomic instead of a check the next delivery can race.
#[derive(Debug, toasty::Model)]
struct InboxRecord {
    #[key]
    id: String,

    session_id: String,
    /// The message body, kept so a claimed-but-uncompleted row can be
    /// re-delivered after a crash — `InboxRepository::unfinished` reads it back
    /// at startup.
    text: String,
    status: String, // "claimed" | "completed"
    claimed_at: i64,
    /// 0 until `complete` runs.
    completed_at: i64,
    /// The `InboundPeer` the channel handed the dispatcher, spread over four
    /// additive columns. Re-dispatching a lost message needs the correspondent
    /// as much as the text: it decides which conversation this is and whether
    /// the sender is the operator. Empty on rows written before the columns
    /// existed, which recovery reads as "no peer to answer".
    peer_platform: String,
    peer_id: String,
    peer_private: bool,
    peer_operator: bool,
}

/// The exact DDL `push_schema` emits for [`InboxRecord`], for the same reason
/// [`JOURNAL_TABLE_DDL`] exists. Byte-parity is locked by
/// `inbox_table_ddl_matches_push_schema`.
const INBOX_TABLE: &str = "inbox_records";
const INBOX_TABLE_DDL: &[&str] = &[
    "CREATE TABLE \"inbox_records\" (\"id\" TEXT NOT NULL, \"session_id\" TEXT NOT NULL, \
     \"text\" TEXT NOT NULL, \"status\" TEXT NOT NULL, \"claimed_at\" BIGINT NOT NULL, \
     \"completed_at\" BIGINT NOT NULL, \"peer_platform\" TEXT NOT NULL, \
     \"peer_id\" TEXT NOT NULL, \"peer_private\" BOOLEAN NOT NULL, \
     \"peer_operator\" BOOLEAN NOT NULL, PRIMARY KEY (\"id\"))",
];
/// The peer columns, for an `inbox_records` that predates them.
const INBOX_COLUMNS: &[(&str, &str)] = &[
    (
        "peer_platform",
        "\"peer_platform\" text NOT NULL DEFAULT ''",
    ),
    ("peer_id", "\"peer_id\" text NOT NULL DEFAULT ''"),
    (
        "peer_private",
        "\"peer_private\" boolean NOT NULL DEFAULT false",
    ),
    (
        "peer_operator",
        "\"peer_operator\" boolean NOT NULL DEFAULT false",
    ),
];

const INBOX_STATUS_CLAIMED: &str = "claimed";
const INBOX_STATUS_COMPLETED: &str = "completed";

/// Setting key for the runtime home channel (`/sethome`).
const HOME_SETTING_KEY: &str = "home_chat";
/// Setting key for the operator's home conversation (D6).
const HOME_SESSION_KEY: &str = "home_session";

/// Settings key holding how far one session's run projection is committed.
/// Per session rather than one global cursor: the fold is per session, and a
/// log that has not changed must be skippable on its own.
fn run_projection_key(session_id: &str) -> String {
    format!("projection:runs:{session_id}")
}

/// The prune tombstone: runs that started before this are deliberately gone.
///
/// Projection control state, not a session event — `run prune` is an operator
/// deleting an index, and broadcasting it into N session logs would make the
/// authoritative record of what happened depend on what someone later chose
/// not to keep. One fence rather than a row per run because prune's own unit is
/// a cutoff: `started_at < cutoff` is exactly the set it deleted.
const RUN_PRUNED_BEFORE_KEY: &str = "projection:runs:pruned_before";

// ── Db ───────────────────────────────────────────────────────────────────────

/// The disposable session/run/pairing store, over the Turso engine with a
/// per-operation connection pool: `inner` is a plain `Arc<toasty::Db>` (no outer
/// `Mutex`), so every method checks out a pooled `Connection` and independent
/// reads/writes run concurrently. Concurrently-written tables (the run ledger)
/// use [`with_write_retry`] for MVCC commit conflicts.
pub struct Db {
    /// The one connection pool. `pub(crate)` because the repository impls for
    /// `Db` are one file per domain (`cron`, `memory::memory_db`) —
    /// the tables share a database, not a module.
    pub(crate) inner: Arc<toasty::Db>,
    /// The session event logs — files rather than rows, one directory per
    /// session. Session *metadata* is still a row here: it is updated (title,
    /// status, model), and a log is the wrong shape for a value that changes.
    events: SessionEventStore,
    /// A second handle on the same file, for the one thing toasty's typed API
    /// cannot express: SQL. The chunk index needs `vector_distance_cos` and
    /// `instr`, so it talks to the engine directly. An in-memory db gets its
    /// own in-memory database here — nothing shares rows across the two, which
    /// only tests ever see.
    raw: Arc<turso::Database>,
}

impl Db {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        // `url` is `turso:<path>` (or `turso::memory:`).
        let (path, is_new) = prepare_turso_path(url);

        // Additive in-place migration for an EXISTING db: `push_schema` only
        // runs for new files, so a column added to a model after the file was
        // created would otherwise be missing and every query on that table
        // would fail — turning "disposable, delete to reset" into "broken on
        // upgrade until the operator remembers to delete". Same mechanism as
        // memory.db's ensure_columns; when adding a column to a model here,
        // extend this list (NOT NULL with a DEFAULT, or nullable) — a new
        // *table* still needs the delete-to-reset.
        if !is_new && let Some(p) = &path {
            ensure_columns(p, "session_records", SESSION_COLUMNS).await?;
            const RUN_COLUMNS: &[(&str, &str)] = &[
                (
                    "recoverable",
                    "\"recoverable\" boolean NOT NULL DEFAULT false",
                ),
                ("tokens_in", "\"tokens_in\" integer NOT NULL DEFAULT 0"),
                ("tokens_out", "\"tokens_out\" integer NOT NULL DEFAULT 0"),
                (
                    "tokens_cached",
                    "\"tokens_cached\" integer NOT NULL DEFAULT 0",
                ),
                ("resumed_from", "\"resumed_from\" text NOT NULL DEFAULT ''"),
                ("memories", "\"memories\" text NOT NULL DEFAULT ''"),
                // `DEFAULT true` backfills history, and only history: every run
                // the learning pass could act on is inserted with an explicit
                // `learned: false`, so this default is reached exactly once per
                // row that predates the column. Defaulting to `false` instead
                // would offer the entire existing ledger to the pass on the
                // upgrade that adds it — thousands of old turns re-extracted at
                // once, each an "independent occasion" to the consolidator.
                ("learned", "\"learned\" boolean NOT NULL DEFAULT true"),
                ("outcome", "\"outcome\" text NOT NULL DEFAULT ''"),
            ];
            ensure_columns(p, "run_records", RUN_COLUMNS).await?;
            const STEP_COLUMNS: &[(&str, &str)] = &[
                ("elapsed_ms", "\"elapsed_ms\" integer NOT NULL DEFAULT 0"),
                ("uncertain", "\"uncertain\" boolean NOT NULL DEFAULT false"),
                ("structured", "\"structured\" text NOT NULL DEFAULT ''"),
                ("output_paths", "\"output_paths\" text NOT NULL DEFAULT ''"),
                ("approved_by", "\"approved_by\" text NOT NULL DEFAULT ''"),
                (
                    "approval_waited_ms",
                    "\"approval_waited_ms\" integer NOT NULL DEFAULT 0",
                ),
            ];
            ensure_columns(p, "run_step_records", STEP_COLUMNS).await?;
            ensure_table(p, INBOX_TABLE, INBOX_TABLE_DDL).await?;
            ensure_columns(p, INBOX_TABLE, INBOX_COLUMNS).await?;
            ensure_table(p, WAKEUP_TABLE, WAKEUP_TABLE_DDL).await?;
            // The durable tables keep their own schema knowledge in their own
            // modules; they are migrated in place and never dropped to be
            // rebuilt.
            super::cron::ensure_schema(p).await?;
            crate::memory::memory_db::ensure_schema(p).await?;
        }

        // MVCC concurrent-writes on (UUID keys throughout, so no AUTOINCREMENT).
        let driver = match &path {
            Some(p) => Turso::file(p).concurrent_writes(),
            None => Turso::in_memory().concurrent_writes(),
        };
        let db = toasty::Db::builder()
            .models(toasty::models!(
                SessionRecord,
                SessionTodoRecord,
                PairingRecord,
                LockoutRecord,
                SettingRecord,
                RunRecord,
                RunStepRecord,
                InboxRecord,
                // Durable, and formerly one file each (docs/adr/0004).
                CronJobRecord,
                MemoryRecord,
                WakeupRecord
            ))
            .max_pool_size(DEFAULT_POOL_SIZE)
            .build(driver)
            .await?;

        if is_new {
            db.push_schema().await?;
        }

        // Transcripts sit beside state.db, so `KOMO_HOME` carries them without
        // this needing to know about it. An in-memory db (tests) gets a
        // directory of its own per connection, which is what keeps two tests
        // from reading each other's transcripts.
        let transcript_home = match &path {
            Some(p) => p
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            None => std::env::temp_dir().join(format!("komo-mem-{}", uuid::Uuid::now_v7())),
        };
        let events = SessionEventStore::new(&transcript_home);

        // Opened after the pool, so the DDL migrations above never contend with
        // it for the file.
        let raw = Arc::new(
            turso::Builder::new_local(
                path.as_deref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| ":memory:".to_string())
                    .as_str(),
            )
            .build()
            .await?,
        );

        Ok(Self {
            inner: Arc::new(db),
            events,
            raw,
        })
    }

    /// A [`ChunkIndex`](komo_core::domain::chunk_index::ChunkIndex) over this
    /// database, in its own collection: one table serves both the note vault
    /// and the transcript corpus.
    pub async fn chunk_index(
        &self,
        collection: &str,
    ) -> anyhow::Result<crate::chunk_index::TursoChunkIndex> {
        crate::chunk_index::TursoChunkIndex::open(self.raw.clone(), collection).await
    }
}

// ── SessionRepository ─────────────────────────────────────────────────────────

#[async_trait]
impl SessionRepository for Db {
    async fn find(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let mut conn = self.inner.connection().await?;
        let Ok(record) = SessionRecord::get_by_id(&mut conn, id).await else {
            return Ok(None);
        };
        let messages = self.events.messages(id).await?;
        Ok(Some(session_from_record(record, messages)))
    }

    async fn find_windowed(&self, id: &str, limit: usize) -> anyhow::Result<Option<Session>> {
        // `limit == 0` means "no window" — fall back to the full load.
        if limit == 0 {
            return SessionRepository::find(self, id).await;
        }
        let mut conn = self.inner.connection().await?;
        let Ok(record) = SessionRecord::get_by_id(&mut conn, id).await else {
            return Ok(None);
        };
        // Derived from the event log's conversation surface. A window cannot be
        // taken from the file's tail the way a message log's could: a compaction
        // near the end can replace a range that began far earlier, so the last N
        // events do not determine the last N messages.
        let messages = self.events.windowed(id, limit).await?;
        Ok(Some(session_from_record(record, messages)))
    }

    async fn find_by_peer(&self, channel: &ChannelPeer) -> anyhow::Result<Option<Session>> {
        // A session with no correspondent stores an empty address, so a query
        // for one would match every local conversation and hand them each
        // other's turns. An empty address is not an address.
        if channel.platform.is_empty() || channel.peer_id.is_empty() {
            return Ok(None);
        }
        let mut conn = self.inner.connection().await?;
        let rows = toasty::query!(SessionRecord).exec(&mut conn).await?;
        // Metadata only — a channel asks this on every inbound message just to
        // learn which conversation it is, and loading a transcript to answer
        // that would pay a turn's read before the turn starts.
        let found = rows
            .into_iter()
            .filter(|r| {
                r.channel_platform == channel.platform && r.channel_peer_id == channel.peer_id
            })
            // Newest wins. There should only ever be one, but a session the
            // operator deleted and a channel then recreated would leave two,
            // and answering with the stale one would strand the conversation.
            .max_by_key(|r| r.created_at);
        Ok(found.map(|record| session_from_record(record, Vec::new())))
    }

    async fn list(&self) -> anyhow::Result<Vec<Session>> {
        let mut conn = self.inner.connection().await?;
        let mut rows = toasty::query!(SessionRecord).exec(&mut conn).await?;
        rows.sort_by_key(|r| r.created_at);

        let mut sessions = Vec::with_capacity(rows.len());
        for record in rows {
            let messages = self.events.messages(&record.id).await?;
            sessions.push(session_from_record(record, messages));
        }
        Ok(sessions)
    }

    async fn save(&self, session: &Session) -> anyhow::Result<()> {
        // Idempotent create (save runs on every load-or-create). The old form
        // `let _ = create!(...)` swallowed *every* error — including an MVCC
        // write conflict, which left the session uncreated and the very next
        // MessageRepository::save failing with a phantom "session not found".
        // Pre-check existence, then insert only when absent; a conflict retries
        // and any real error surfaces.
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            if SessionRecord::get_by_id(&mut conn, &session.id)
                .await
                .is_ok()
            {
                return Ok(());
            }
            let created = toasty::create!(SessionRecord {
                id: session.id.clone(),
                created_at: session.created_at,
                workspace: session.workspace.clone(),
                title: session.title.clone(),
                status: session.status.clone(),
                model: session.model.clone(),
                effort: session.effort.clone(),
                channel_platform: session
                    .channel
                    .as_ref()
                    .map(|c| c.platform.clone())
                    .unwrap_or_default(),
                channel_peer_id: session
                    .channel
                    .as_ref()
                    .map(|c| c.peer_id.clone())
                    .unwrap_or_default(),
                origin: session.origin.as_str().to_string(),
                awaiting: String::new(),
            })
            .exec(&mut conn)
            .await;
            if let Err(error) = created {
                // Concurrent create of the same brand-new id: the dispatcher
                // serializes chat turns per session, but the api channel calls
                // the handler directly, so two first-requests can race here.
                // If the winner committed, Turso reports a UNIQUE-constraint
                // violation (not a retryable busy/conflict) — the row exists,
                // which is all save() promises, so treat it as success. A
                // genuinely absent row means a real failure: propagate.
                if SessionRecord::get_by_id(&mut conn, &session.id)
                    .await
                    .is_ok()
                {
                    return Ok(());
                }
                return Err(error.into());
            }
            Ok(())
        })
        .await
    }

    async fn delete_empty_sessions(&self) -> anyhow::Result<usize> {
        let mut conn = self.inner.connection().await?;
        let rows = toasty::query!(SessionRecord).exec(&mut conn).await?;

        let mut removed = 0usize;
        for record in rows {
            if self.events.messages(&record.id).await?.is_empty() {
                // No transcript file to remove — that is what empty means here.
                record.delete().exec(&mut conn).await?;
                removed += 1;
            }
        }

        if removed > 0 {
            info!(removed, "pruned empty sessions");
        }
        Ok(removed)
    }

    async fn commit_awaiting(
        &self,
        session_id: &str,
        events: &[SessionEvent],
    ) -> anyhow::Result<()> {
        let mut conn = self.inner.connection().await?;
        let Ok(record) = SessionRecord::get_by_id(&mut conn, session_id).await else {
            return Ok(()); // no such session
        };
        let prior = serde_json::from_str(&record.awaiting).ok();
        self.write_awaiting(session_id, project_awaiting(prior, events).as_ref())
            .await
    }

    async fn set_title(&self, session_id: &str, title: &str) -> anyhow::Result<()> {
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let Ok(mut record) = SessionRecord::get_by_id(&mut conn, session_id).await else {
                return Ok(()); // no such session — nothing to rename
            };
            record
                .update()
                .title(title.to_string())
                .exec(&mut conn)
                .await?;
            Ok(())
        })
        .await
    }

    async fn set_model(&self, session_id: &str, model: &str, effort: &str) -> anyhow::Result<()> {
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let Ok(mut record) = SessionRecord::get_by_id(&mut conn, session_id).await else {
                return Ok(()); // no such session
            };
            // Skip the write when nothing moved: the chat endpoint sends the
            // client's current selection on *every* turn, so an unchanged
            // selection would otherwise be a pointless write per turn.
            if record.model == model && record.effort == effort {
                return Ok(());
            }
            record
                .update()
                .model(model.to_string())
                .effort(effort.to_string())
                .exec(&mut conn)
                .await?;
            Ok(())
        })
        .await
    }

    async fn set_status(&self, session_id: &str, status: &str) -> anyhow::Result<()> {
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let Ok(mut record) = SessionRecord::get_by_id(&mut conn, session_id).await else {
                return Ok(()); // no such session
            };
            record
                .update()
                .status(status.to_string())
                .exec(&mut conn)
                .await?;
            Ok(())
        })
        .await
    }

    async fn delete_session(&self, session_id: &str) -> anyhow::Result<bool> {
        // Transactional cascade: remove the session's messages then the session
        // row itself, so a mid-sequence failure rolls back cleanly (mirrors
        // `RunRepository::prune`). Runs/todos keyed by this session
        // are left as harmless orphans — they never surface in the session list.
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let mut tx = conn.transaction().await?;
            let Ok(record) = SessionRecord::get_by_id(&mut tx, session_id).await else {
                return Ok(false);
            };
            record.delete().exec(&mut tx).await?;
            tx.commit().await?;
            Ok(true)
        })
        .await
    }
}

// ── MessageRepository ─────────────────────────────────────────────────────────

#[async_trait]
impl MessageRepository for Db {
    async fn list_by_session(&self, session_id: &str) -> anyhow::Result<Vec<Message>> {
        Ok(self.events.messages(session_id).await?)
    }
}

// ── SessionEventRepository ────────────────────────────────────────────────────

#[async_trait]
impl SessionEventRepository for Db {
    async fn append(
        &self,
        session_id: &str,
        kinds: Vec<SessionEventKind>,
    ) -> anyhow::Result<Vec<SessionEvent>> {
        // The header is only consulted when the log does not exist yet, so this
        // describes a session at its first event and never overwrites identity.
        let header = self.session_header(session_id).await;
        Ok(self.events.append(session_id, header, kinds).await?)
    }

    async fn durable_flush(&self, session_id: &str) -> anyhow::Result<()> {
        if let Some(log) = self.events.existing(session_id).await? {
            log.durable_flush().await?;
        }
        Ok(())
    }

    async fn events(&self, session_id: &str) -> anyhow::Result<Vec<SessionEvent>> {
        Ok(self.events.events(session_id).await?)
    }

    async fn events_from(&self, session_id: &str, seq: u64) -> anyhow::Result<Vec<SessionEvent>> {
        Ok(self.events.events_from(session_id, seq).await?)
    }

    async fn surface(&self, session_id: &str) -> anyhow::Result<Option<SurfaceProjection>> {
        Ok(self.events.surface(session_id).await?)
    }

    async fn session_ids(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.events.session_ids().await?)
    }

    async fn turn_boundary(&self, session_id: &str) -> anyhow::Result<bool> {
        // Refresh the surface checkpoint here and nowhere else: a turn boundary
        // is where the log is quiet, and the next turn's history read is what
        // the checkpoint is for. Best-effort — the log already holds everything
        // it describes, so a failed write costs the next read a full fold.
        if let Err(error) = self.events.checkpoint_surface(session_id).await {
            info!(%error, session_id, "could not refresh the surface checkpoint (non-fatal)");
        }
        Ok(self.events.seal(session_id).await?)
    }

    async fn retain(&self, session_id: &str, keep_from: u64) -> anyhow::Result<Option<u64>> {
        Ok(self.events.retain(session_id, keep_from).await?)
    }
}

impl Db {
    /// Identity for a log being materialized: taken from the session row when
    /// there is one, so a session's origin and workspace reach its log without
    /// every caller having to carry them.
    async fn session_header(&self, session_id: &str) -> SessionHeader {
        let row = SessionRepository::find_windowed(self, session_id, 1)
            .await
            .ok()
            .flatten();
        SessionHeader {
            session_id: session_id.to_string(),
            origin: row
                .as_ref()
                .map(|s| s.origin.as_str().to_string())
                .unwrap_or_else(|| SessionOrigin::User.as_str().to_string()),
            workspace: row.as_ref().and_then(|s| {
                Some(s.workspace.clone())
                    .filter(|w| w != komo_core::domain::session::DEFAULT_WORKSPACE)
            }),
            created_at: time::OffsetDateTime::now_utc(),
            format_version: SESSION_EVENT_VERSION,
        }
    }
}

fn run_from_record(record: RunRecord) -> anyhow::Result<Run> {
    Ok(Run {
        id: record.id,
        session_id: record.session_id,
        input: record.input,
        plan: record.plan,
        status: parse_run_status(&record.status)?,
        final_output: record.final_output,
        error: record.error,
        recoverable: record.recoverable,
        started_at: record.started_at,
        ended_at: (record.ended_at != 0).then_some(record.ended_at),
        tokens_in: record.tokens_in,
        tokens_out: record.tokens_out,
        tokens_cached: record.tokens_cached,
        resumed_from: (!record.resumed_from.is_empty()).then_some(record.resumed_from),
        // A malformed cell reads as "none recorded": the ledger is an audit
        // record, and one bad row must not fail the read of a whole run.
        memories: serde_json::from_str(&record.memories).unwrap_or_default(),
        learned: record.learned,
        outcome: record.outcome,
    })
}

// ── SessionTodoRepository ─────────────────────────────────────────────────────

#[async_trait]
impl SessionTodoRepository for Db {
    async fn get(&self, session_id: &str) -> anyhow::Result<Vec<TodoItem>> {
        let mut conn = self.inner.connection().await?;
        match SessionTodoRecord::get_by_session_id(&mut conn, session_id).await {
            Ok(record) => Ok(serde_json::from_str(&record.items).unwrap_or_default()),
            Err(_) => Ok(Vec::new()),
        }
    }

    async fn set(&self, session_id: &str, items: &[TodoItem]) -> anyhow::Result<()> {
        let json = serde_json::to_string(items)?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            match SessionTodoRecord::get_by_session_id(&mut conn, session_id).await {
                Ok(mut record) => {
                    record
                        .update()
                        .items(json.clone())
                        .updated_at(now)
                        .exec(&mut conn)
                        .await?;
                }
                Err(_) => {
                    toasty::create!(SessionTodoRecord {
                        session_id: session_id.to_string(),
                        items: json.clone(),
                        updated_at: now,
                    })
                    .exec(&mut conn)
                    .await?;
                }
            }
            Ok(())
        })
        .await
    }

    async fn clear(&self, session_id: &str) -> anyhow::Result<()> {
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            if let Ok(record) = SessionTodoRecord::get_by_session_id(&mut conn, session_id).await {
                record.delete().exec(&mut conn).await?;
            }
            Ok(())
        })
        .await
    }
}

// ── PairingRepository ─────────────────────────────────────────────────────────

#[async_trait]
impl PairingRepository for Db {
    async fn upsert(&self, request: &PairingRequest) -> anyhow::Result<()> {
        // delete-if-exists + create: the delete is conditional on the row being
        // present, so a conflict-retry of the whole closure re-reads cleanly
        // (an already-deleted row is simply skipped on the next attempt).
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            if let Ok(record) = PairingRecord::get_by_id(&mut conn, &request.id).await {
                record.delete().exec(&mut conn).await?;
            }
            toasty::create!(PairingRecord {
                id: request.id.clone(),
                platform: request.platform.clone(),
                sender_id: request.sender_id.clone(),
                chat_id: request.chat_id.clone(),
                code_hash: request.code_hash.clone(),
                salt: request.salt.clone(),
                status: request.status.as_str().to_string(),
                created_at: request.created_at,
            })
            .exec(&mut conn)
            .await?;
            Ok(())
        })
        .await
    }

    async fn find(
        &self,
        platform: &str,
        sender_id: &str,
    ) -> anyhow::Result<Option<PairingRequest>> {
        let mut conn = self.inner.connection().await?;
        let id = format!("{platform}:{sender_id}");
        match PairingRecord::get_by_id(&mut conn, &id).await {
            Ok(record) => Ok(Some(pairing_from_record(record))),
            Err(_) => Ok(None),
        }
    }

    async fn count_active_pending(&self, platform: &str) -> anyhow::Result<usize> {
        let mut conn = self.inner.connection().await?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let rows = toasty::query!(PairingRecord).exec(&mut conn).await?;
        Ok(rows
            .iter()
            .filter(|r| {
                r.platform == platform
                    && r.status == "pending"
                    && now - r.created_at <= PAIRING_CODE_TTL_SECS
            })
            .count())
    }

    async fn approve_code(&self, code: &str) -> anyhow::Result<ApproveOutcome> {
        const LOCK_ID: &str = "approve";
        // Transactional: the code-match status flip and the failure-counter
        // update are two writes that must land together — a mid-sequence failure
        // used to leave "approved but counter not cleared" (or vice versa).
        // with_write_retry re-runs the whole closure on an MVCC conflict; the
        // rolled-back transaction makes that safe.
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let mut tx = conn.transaction().await?;
            let now = time::OffsetDateTime::now_utc().unix_timestamp();

            // Honor an active lockout before testing the code (read-only path:
            // returning here rolls the empty transaction back).
            let lock = LockoutRecord::get_by_id(&mut tx, LOCK_ID).await.ok();
            if let Some(l) = &lock
                && l.locked_until > now
            {
                return Ok(ApproveOutcome::Locked {
                    retry_after_secs: l.locked_until - now,
                });
            }

            let rows = toasty::query!(PairingRecord).exec(&mut tx).await?;
            let matched = rows.into_iter().find(|r| {
                r.status == "pending"
                    && now - r.created_at <= PAIRING_CODE_TTL_SECS
                    && verify_code(&r.salt, &r.code_hash, code)
            });

            let outcome = match matched {
                Some(mut record) => {
                    record
                        .update()
                        .status(PairingStatus::Approved.as_str().to_string())
                        .exec(&mut tx)
                        .await?;
                    // Success clears the failure counter.
                    if let Some(mut l) = lock {
                        l.update()
                            .failed_count(0)
                            .locked_until(0)
                            .exec(&mut tx)
                            .await?;
                    }
                    ApproveOutcome::Approved(pairing_from_record(record))
                }
                None => {
                    let mut count = lock.as_ref().map(|l| l.failed_count).unwrap_or(0) + 1;
                    let mut locked_until = 0;
                    if count >= APPROVE_MAX_FAILURES {
                        locked_until = now + APPROVE_LOCKOUT_SECS;
                        count = 0; // reset the counter once locked
                    }
                    match lock {
                        Some(mut l) => {
                            l.update()
                                .failed_count(count)
                                .locked_until(locked_until)
                                .exec(&mut tx)
                                .await?;
                        }
                        None => {
                            toasty::create!(LockoutRecord {
                                id: LOCK_ID.to_string(),
                                failed_count: count,
                                locked_until,
                            })
                            .exec(&mut tx)
                            .await?;
                        }
                    }
                    if locked_until > now {
                        ApproveOutcome::Locked {
                            retry_after_secs: locked_until - now,
                        }
                    } else {
                        ApproveOutcome::NotFound
                    }
                }
            };
            tx.commit().await?;
            Ok(outcome)
        })
        .await
    }

    async fn list(&self) -> anyhow::Result<Vec<PairingRequest>> {
        let mut conn = self.inner.connection().await?;
        let mut rows = toasty::query!(PairingRecord).exec(&mut conn).await?;
        rows.sort_by_key(|r| r.created_at);
        Ok(rows.into_iter().map(pairing_from_record).collect())
    }

    async fn revoke(&self, id: &str) -> anyhow::Result<bool> {
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            match PairingRecord::get_by_id(&mut conn, id).await {
                Ok(record) => {
                    record.delete().exec(&mut conn).await?;
                    Ok(true)
                }
                Err(_) => Ok(false),
            }
        })
        .await
    }
}

// ── Settings (HomeRepository) ────────────────────────────────────────────────

impl Db {
    /// Read one settings row; empty value reads as unset.
    async fn setting_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let mut conn = self.inner.connection().await?;
        match SettingRecord::get_by_id(&mut conn, key).await {
            Ok(record) => Ok(Some(record.value).filter(|v| !v.is_empty())),
            Err(_) => Ok(None),
        }
    }

    /// Upsert one settings row.
    async fn setting_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            match SettingRecord::get_by_id(&mut conn, key).await {
                Ok(mut record) => {
                    record
                        .update()
                        .value(value.to_string())
                        .exec(&mut conn)
                        .await?;
                }
                Err(_) => {
                    toasty::create!(SettingRecord {
                        id: key.to_string(),
                        value: value.to_string(),
                    })
                    .exec(&mut conn)
                    .await?;
                }
            }
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl HomeRepository for Db {
    async fn get(&self) -> anyhow::Result<Option<String>> {
        self.setting_get(HOME_SETTING_KEY).await
    }

    async fn set(&self, session_id: &str) -> anyhow::Result<()> {
        self.setting_set(HOME_SETTING_KEY, session_id).await
    }

    async fn home_session(&self) -> anyhow::Result<String> {
        if let Some(id) = self.setting_get(HOME_SESSION_KEY).await? {
            return Ok(id);
        }
        // Only the settings row is minted here. The session record itself is
        // written by the first turn that lands on the id, the same way every
        // other conversation's is — a row nobody ever spoke into would be a
        // conversation that never happened.
        self.setting_set(HOME_SESSION_KEY, &uuid::Uuid::now_v7().to_string())
            .await?;
        // Read back rather than returning what was written: two processes
        // racing the first ask must agree on one id, and the row is what they
        // agree through.
        self.setting_get(HOME_SESSION_KEY)
            .await?
            .ok_or_else(|| anyhow::anyhow!("home session id did not persist"))
    }
}

// ── RunRepository ─────────────────────────────────────────────────────────────

#[async_trait]
impl RunRepository for Db {
    async fn list(&self, limit: usize) -> anyhow::Result<Vec<Run>> {
        let mut conn = self.inner.connection().await?;
        // Most-recent-first ordering and the cap are pushed down to SQL, so a
        // large ledger doesn't get fully materialized just to take the head.
        let rows = toasty::query!(RunRecord ORDER BY .started_at DESC LIMIT #limit)
            .exec(&mut conn)
            .await?;
        rows.into_iter().map(run_from_record).collect()
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Run>> {
        let mut conn = self.inner.connection().await?;
        match RunRecord::get_by_id(&mut conn, id).await {
            Ok(record) => Ok(Some(run_from_record(record)?)),
            Err(_) => Ok(None),
        }
    }

    async fn steps(&self, run_id: &str) -> anyhow::Result<Vec<RunStep>> {
        let mut conn = self.inner.connection().await?;
        // Use the `run_id` index instead of scanning the whole step table.
        let rows = toasty::query!(RunStepRecord FILTER .run_id == #run_id)
            .exec(&mut conn)
            .await?;
        let mut steps: Vec<RunStep> = rows.into_iter().map(step_from_record).collect();
        steps.sort_by_key(|s| s.seq);
        Ok(steps)
    }

    async fn prune(&self, cutoff: i64) -> anyhow::Result<usize> {
        // Transactional: each run and all its steps drop together — a partial
        // prune used to orphan steps whose run was already deleted (or vice
        // versa). with_write_retry re-runs cleanly after a rolled-back conflict.
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let mut tx = conn.transaction().await?;
            // Select the stale runs with the cutoff pushed down to SQL, then drop
            // each run's steps via the `run_id` index — no full step-table scan.
            let stale = toasty::query!(RunRecord FILTER .started_at < #cutoff)
                .exec(&mut tx)
                .await?;
            let count = stale.len();
            let mut newest_pruned: Option<i64> = None;
            for run in stale {
                newest_pruned =
                    Some(newest_pruned.map_or(run.started_at, |at: i64| at.max(run.started_at)));
                let run_id = run.id.clone();
                let steps = toasty::query!(RunStepRecord FILTER .run_id == #run_id)
                    .exec(&mut tx)
                    .await?;
                for step in steps {
                    step.delete().exec(&mut tx).await?;
                }
                run.delete().exec(&mut tx).await?;
            }
            // The tombstone, in the same transaction as the deletes: a rebuild
            // reads the log, and without this every pruned run comes back.
            //
            // Bounded by what was *actually* deleted rather than by `cutoff`,
            // which `--before` will happily accept in the future: the newest run
            // that went is the exact edge of the deleted set, since everything
            // that survived started at or after the cutoff above it.
            if let Some(newest) = newest_pruned {
                let fence = newest + 1;
                match SettingRecord::get_by_id(&mut tx, RUN_PRUNED_BEFORE_KEY).await {
                    // Monotonic: a later prune with an older cutoff must not
                    // unfence what an earlier one deleted.
                    Ok(mut record) => {
                        let held = record.value.parse::<i64>().unwrap_or(i64::MIN);
                        if fence > held {
                            record
                                .update()
                                .value(fence.to_string())
                                .exec(&mut tx)
                                .await?;
                        }
                    }
                    Err(_) => {
                        toasty::create!(SettingRecord {
                            id: RUN_PRUNED_BEFORE_KEY.to_string(),
                            value: fence.to_string(),
                        })
                        .exec(&mut tx)
                        .await?;
                    }
                }
            }
            tx.commit().await?;
            Ok(count)
        })
        .await
    }

    async fn reconcile_interrupted(&self, now: i64) -> anyhow::Result<usize> {
        // Transactional: flip every crash-residue "running" run to failed as one
        // unit, so a failure partway doesn't leave some rows stuck "running"
        // (they'd never be reconciled on a later startup). Retry-safe.
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let mut tx = conn.transaction().await?;
            let running = RunStatus::Running.as_str();
            // Only the still-"running" rows are touched — filter pushed to SQL.
            let rows = toasty::query!(RunRecord FILTER .status == #running)
                .exec(&mut tx)
                .await?;
            let mut reconciled = 0;
            for mut record in rows {
                record
                    .update()
                    .status(RunStatus::Failed.as_str().to_string())
                    .error(INTERRUPTED_ERROR.to_string())
                    .recoverable(true)
                    .ended_at(now)
                    .exec(&mut tx)
                    .await?;
                reconciled += 1;
            }
            tx.commit().await?;
            Ok(reconciled)
        })
        .await
    }

    async fn unlearned(&self, session_id: Option<&str>, limit: usize) -> anyhow::Result<Vec<Run>> {
        let mut conn = self.inner.connection().await?;
        // The `learned` filter and the cap are pushed to SQL: once the ledger
        // is mostly learned, a scan that filtered in Rust would spend its whole
        // limit on already-consumed rows and report an empty backlog that isn't.
        // Oldest first — learning replays a conversation forwards, so a
        // correction is extracted after the claim it corrects.
        let rows = match session_id {
            Some(session) => {
                toasty::query!(
                    RunRecord FILTER .learned == false AND .session_id == #session
                    ORDER BY .started_at LIMIT #limit
                )
                .exec(&mut conn)
                .await?
            }
            None => {
                toasty::query!(
                    RunRecord FILTER .learned == false ORDER BY .started_at LIMIT #limit
                )
                .exec(&mut conn)
                .await?
            }
        };
        rows.into_iter()
            .map(run_from_record)
            // A turn still in flight — running, or parked waiting for
            // something — is not an episode. Filtered here rather than in the
            // query because the crash residue it guards against is rare and
            // short-lived (`reconcile_interrupted` clears it at every startup),
            // so it never eats a meaningful share of the limit.
            .filter(|run| !matches!(run, Ok(r) if !r.status.is_terminal()))
            .collect()
    }

    async fn set_outcome(&self, run_id: &str, outcome: &str) -> anyhow::Result<()> {
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let mut record = RunRecord::get_by_id(&mut conn, run_id).await?;
            record
                .update()
                .outcome(outcome.to_string())
                .exec(&mut conn)
                .await?;
            Ok(())
        })
        .await
    }

    async fn previous_in_session(&self, run_id: &str) -> anyhow::Result<Option<Run>> {
        let mut conn = self.inner.connection().await?;
        let Ok(current) = RunRecord::get_by_id(&mut conn, run_id).await else {
            return Ok(None);
        };
        let session = current.session_id.clone();
        let started = current.started_at;
        // Strictly earlier, newest first — the turn whose work a follow-up
        // message is most plausibly about.
        let rows = toasty::query!(
            RunRecord FILTER .session_id == #session AND .started_at < #started
            ORDER BY .started_at DESC LIMIT 1usize
        )
        .exec(&mut conn)
        .await?;
        rows.into_iter().next().map(run_from_record).transpose()
    }

    async fn mark_learned(&self, run_ids: &[String]) -> anyhow::Result<()> {
        if run_ids.is_empty() {
            return Ok(());
        }
        // One transaction inside the retry: a conflicting commit rolls the whole
        // batch back and re-runs it, so a partial mark can never make half a
        // learning pass look complete.
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let mut tx = conn.transaction().await?;
            for id in run_ids {
                let mut record = RunRecord::get_by_id(&mut tx, id).await?;
                record.update().learned(true).exec(&mut tx).await?;
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }
}

// ── RunProjectionStore ────────────────────────────────────────────────────────

#[async_trait]
impl RunProjectionStore for Db {
    async fn commit(
        &self,
        session_id: &str,
        runs: &[ProjectedRun],
        through: u64,
    ) -> anyhow::Result<()> {
        let key = run_projection_key(session_id);
        let committed = self
            .setting_get(&key)
            .await?
            .and_then(|value| value.parse::<u64>().ok());
        if committed.is_some_and(|at| at >= through) {
            return Ok(());
        }
        self.write_projection(runs).await?;
        // The watermark lands *after* the rows, so a crash between the two
        // re-commits a fold the tables already hold — which is the one thing a
        // commit is allowed to do twice.
        self.setting_set(&key, &through.to_string()).await
    }
}

impl Db {
    /// Re-fold every session's log into the rows projected from it — the run
    /// ledger and each session's open wait — watermarks ignored.
    ///
    /// The repair path: these tables are disposable, and this is what makes
    /// that true of everything folded into them. The wait is re-folded from
    /// `None`, not merged onto the stored value: a rebuild is the log having the
    /// last word, which is the whole point of the column being a cache. Answers
    /// how many runs it wrote.
    pub async fn rebuild_projections(&self) -> anyhow::Result<usize> {
        let mut total = 0;
        for session_id in self.events.session_ids().await? {
            let events = self.events.events(&session_id).await?;
            self.write_awaiting(&session_id, project_awaiting(None, &events).as_ref())
                .await?;
            let runs = project_runs(&session_id, &events);
            if runs.is_empty() {
                continue;
            }
            let through = events.last().map(|event| event.seq).unwrap_or(0);
            self.write_projection(&runs).await?;
            self.setting_set(&run_projection_key(&session_id), &through.to_string())
                .await?;
            total += runs.len();
        }
        Ok(total)
    }

    /// Store one session's open wait (`None` = not waiting).
    async fn write_awaiting(
        &self,
        session_id: &str,
        awaiting: Option<&Awaiting>,
    ) -> anyhow::Result<()> {
        let stored = match awaiting {
            Some(awaiting) => serde_json::to_string(awaiting)?,
            None => String::new(),
        };
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let Ok(mut record) = SessionRecord::get_by_id(&mut conn, session_id).await else {
                return Ok(()); // no such session
            };
            if record.awaiting == stored {
                return Ok(());
            }
            record
                .update()
                .awaiting(stored.clone())
                .exec(&mut conn)
                .await?;
            Ok(())
        })
        .await
    }

    /// Write one session's fold as rows, in a single transaction.
    ///
    /// Every write is an upsert and nothing is deleted: a settled call and a
    /// finished turn are immutable in the log, so an existing row already
    /// agrees with the fold, and a row the fold no longer produces would mean
    /// the log lost events — which is a gap the loader rejects, not something
    /// to clean up here.
    async fn write_projection(&self, runs: &[ProjectedRun]) -> anyhow::Result<()> {
        if runs.is_empty() {
            return Ok(());
        }
        // Runs an operator pruned are not resurrected, however long their log
        // outlives them.
        let pruned_before = self
            .setting_get(RUN_PRUNED_BEFORE_KEY)
            .await?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(i64::MIN);
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let mut tx = conn.transaction().await?;
            for projected in runs {
                let run = &projected.run;
                if run.started_at < pruned_before {
                    continue;
                }
                let memories = if run.memories.is_empty() {
                    String::new()
                } else {
                    serde_json::to_string(&run.memories).unwrap_or_default()
                };
                match RunRecord::get_by_id(&mut tx, &run.id).await {
                    Ok(mut record) => {
                        // `outcome` is not here at all, and `learned` only ever
                        // advances: both are row-held, and the fold overwriting
                        // them would drop a verdict the user gave after the turn.
                        let learned = record.learned || run.learned;
                        // A turn with no terminal event folds as *running*, and
                        // whether it is running or dead is the one thing the log
                        // cannot say — the startup reconciler rules on that and
                        // writes it here. So the fold's silence never un-decides
                        // it: without this, the next turn in the session would
                        // put every interrupted run back to "running".
                        let undecided = matches!(run.status, RunStatus::Running)
                            && record.status != RunStatus::Running.as_str();
                        let status = if undecided {
                            record.status.clone()
                        } else {
                            run.status.as_str().to_string()
                        };
                        let error = if undecided {
                            record.error.clone()
                        } else {
                            run.error.clone()
                        };
                        let ended_at = if undecided {
                            record.ended_at
                        } else {
                            run.ended_at.unwrap_or(0)
                        };
                        record
                            .update()
                            .input(run.input.clone())
                            .plan(run.plan.clone())
                            .status(status)
                            .final_output(run.final_output.clone())
                            .error(error)
                            .recoverable(run.recoverable)
                            .started_at(run.started_at)
                            .ended_at(ended_at)
                            .tokens_in(run.tokens_in)
                            .tokens_out(run.tokens_out)
                            .tokens_cached(run.tokens_cached)
                            .memories(memories.clone())
                            .resumed_from(run.resumed_from.clone().unwrap_or_default())
                            .learned(learned)
                            .exec(&mut tx)
                            .await?;
                    }
                    Err(_) => {
                        toasty::create!(RunRecord {
                            id: run.id.clone(),
                            session_id: run.session_id.clone(),
                            input: run.input.clone(),
                            plan: run.plan.clone(),
                            status: run.status.as_str().to_string(),
                            final_output: run.final_output.clone(),
                            error: run.error.clone(),
                            recoverable: run.recoverable,
                            started_at: run.started_at,
                            ended_at: run.ended_at.unwrap_or(0),
                            tokens_in: run.tokens_in,
                            tokens_out: run.tokens_out,
                            tokens_cached: run.tokens_cached,
                            memories: memories.clone(),
                            resumed_from: run.resumed_from.clone().unwrap_or_default(),
                            learned: run.learned,
                            outcome: String::new(),
                        })
                        .exec(&mut tx)
                        .await?;
                    }
                }

                // Only the calls that settled become rows — that is what the
                // ledger has always held, and an unsettled call is the fold's
                // own answer to recovery, not a step anyone ran.
                let run_id = run.id.clone();
                let existing = toasty::query!(RunStepRecord FILTER .run_id == #run_id)
                    .exec(&mut tx)
                    .await?;
                for step in projected
                    .steps
                    .iter()
                    .filter(|s| s.settled)
                    .map(|s| &s.step)
                {
                    if existing.iter().any(|row| row.seq == step.seq) {
                        continue;
                    }
                    toasty::create!(RunStepRecord {
                        id: uuid::Uuid::now_v7().to_string(),
                        run_id: step.run_id.clone(),
                        seq: step.seq,
                        tool_name: step.tool_name.clone(),
                        args: step.args.clone(),
                        result: step.result.clone(),
                        error: step.error.clone(),
                        ok: step.ok,
                        uncertain: step.uncertain,
                        started_at: step.started_at,
                        ended_at: step.ended_at,
                        elapsed_ms: step.elapsed_ms,
                        structured: match &step.structured {
                            serde_json::Value::Null => String::new(),
                            value => value.to_string(),
                        },
                        output_paths: step.output_paths.join("\n"),
                        approved_by: step.approved_by.clone(),
                        approval_waited_ms: step.approval_waited_ms,
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
}

// ── InboxRepository ──────────────────────────────────────────────────────────

#[async_trait]
impl InboxRepository for Db {
    async fn claim(
        &self,
        origin: &InboundOrigin,
        peer: &InboundPeer,
        session_id: &str,
        text: &str,
    ) -> anyhow::Result<InboxClaim> {
        let id = origin.key();
        let lookup = id.as_str();
        let mut conn = self.inner.connection().await?;
        let seen = toasty::query!(InboxRecord FILTER .id == #lookup)
            .exec(&mut conn)
            .await?;
        if !seen.is_empty() {
            return Ok(InboxClaim::Duplicate);
        }
        drop(conn);
        // Each channel consumes its own messages one at a time, so two claims
        // for the same id never race here. If that ever changes, the primary
        // key still refuses the second insert — loudly, rather than by letting
        // both through.
        let session_id = session_id.to_string();
        let text = text.to_string();
        with_write_retry(|| {
            let id = id.clone();
            let session_id = session_id.clone();
            let text = text.clone();
            let peer = peer.clone();
            async move {
                let mut conn = self.inner.connection().await?;
                toasty::create!(InboxRecord {
                    id,
                    session_id,
                    text,
                    status: INBOX_STATUS_CLAIMED.to_string(),
                    claimed_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                    completed_at: 0,
                    peer_platform: peer.peer.platform,
                    peer_id: peer.peer.peer_id,
                    peer_private: peer.private,
                    peer_operator: peer.operator,
                })
                .exec(&mut conn)
                .await?;
                Ok(())
            }
        })
        .await?;
        Ok(InboxClaim::Fresh)
    }

    async fn complete(&self, origin: &InboundOrigin) -> anyhow::Result<()> {
        let id = origin.key();
        with_write_retry(|| {
            let id = id.clone();
            async move {
                let mut conn = self.inner.connection().await?;
                toasty::query!(InboxRecord FILTER .id == #id)
                    .update()
                    .status(INBOX_STATUS_COMPLETED)
                    .completed_at(time::OffsetDateTime::now_utc().unix_timestamp())
                    .exec(&mut conn)
                    .await?;
                Ok(())
            }
        })
        .await
    }

    async fn unfinished(&self, limit: usize) -> anyhow::Result<Vec<UnfinishedInbound>> {
        let claimed = INBOX_STATUS_CLAIMED;
        let mut conn = self.inner.connection().await?;
        let rows = toasty::query!(
            InboxRecord FILTER .status == #claimed ORDER BY .claimed_at LIMIT #limit
        )
        .exec(&mut conn)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                // The key is `<platform>:<message_id>`, and a message id may
                // itself contain a colon — split once, from the left.
                let (platform, message_id) = row.id.split_once(':')?;
                Some(UnfinishedInbound {
                    origin: InboundOrigin::new(platform, message_id),
                    session_id: row.session_id,
                    text: row.text,
                    peer: InboundPeer::new(
                        ChannelPeer::new(row.peer_platform, row.peer_id),
                        row.peer_private,
                        row.peer_operator,
                    ),
                    claimed_at: row.claimed_at,
                })
            })
            .collect())
    }
}

fn step_from_record(record: RunStepRecord) -> RunStep {
    RunStep {
        approved_by: record.approved_by,
        approval_waited_ms: record.approval_waited_ms,
        run_id: record.run_id,
        seq: record.seq,
        tool_name: record.tool_name,
        args: record.args,
        result: record.result,
        error: record.error,
        ok: record.ok,
        uncertain: record.uncertain,
        started_at: record.started_at,
        ended_at: record.ended_at,
        elapsed_ms: record.elapsed_ms,
        // Empty (a tool with no structured view, or a pre-column row) reads back
        // as `Null` — absence, not an empty object. Unparseable text does too:
        // the ledger is an audit record, and a malformed cell must not fail a read.
        structured: serde_json::from_str(&record.structured).unwrap_or(serde_json::Value::Null),
        output_paths: record
            .output_paths
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    }
}

fn session_from_record(record: SessionRecord, messages: Vec<Message>) -> Session {
    let id = record.id.clone();
    let workspace = record.workspace.clone();
    let created_at = record.created_at;
    let title = record.title.clone();
    let status = record.status.clone();
    let model = record.model.clone();
    let effort = record.effort.clone();
    // Both halves or neither: a half-written address names no correspondent.
    let channel = (!record.channel_platform.is_empty() && !record.channel_peer_id.is_empty())
        .then(|| ChannelPeer::new(&record.channel_platform, &record.channel_peer_id));
    let origin = SessionOrigin::parse(&record.origin);
    // A cache that will not parse is a cache miss: the fold puts it back at the
    // next turn boundary, and a rebuild puts it back now.
    let awaiting = serde_json::from_str(&record.awaiting).ok();
    Session {
        id,
        workspace,
        messages,
        created_at,
        title,
        status,
        model,
        effort,
        channel,
        origin,
        awaiting,
    }
}

fn pairing_from_record(record: PairingRecord) -> PairingRequest {
    PairingRequest {
        id: record.id,
        platform: record.platform,
        sender_id: record.sender_id,
        chat_id: record.chat_id,
        code_hash: record.code_hash,
        salt: record.salt,
        status: parse_pairing_status(&record.status),
        created_at: record.created_at,
    }
}

#[cfg(test)]
mod tests;
