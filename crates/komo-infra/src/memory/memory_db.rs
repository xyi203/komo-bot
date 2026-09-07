//! Long-term memories: `memory_records` in `komo.db`, and the
//! [`MemoryRepository`] over them.
//!
//! The strictest durability rule in the repository lives here: this table may
//! **only ever change additively**. It had its own file (`memory.db`) until
//! docs/adr/0004 moved that guarantee to the table; the file is gone, the rule
//! is not. Markdown (`md_memory.rs`) stays an import/export format, never the
//! canonical backend.
//!
//! Schema is laid out **schema-first**: governance/scope/usage columns land all
//! at once even before every consumer exists, because toasty's `push_schema`
//! is not idempotent. See `docs/personal-agent-roadmap.md`.

use std::path::Path;

use anyhow::Context;
use async_trait::async_trait;

use crate::memory::md_memory::MdMemoryStore;
use crate::persistence::db::Db;
use crate::persistence::with_write_retry;
use komo_core::domain::memory::{
    Evidence, Memory, MemoryRepository, MemoryScope, parse_belief_state, parse_memory_confidence,
    parse_memory_kind, parse_memory_provenance, parse_memory_status,
};

// Optional i64 fields use 0 as the "unset" sentinel (same convention as `Db`).
#[derive(Debug, toasty::Model)]
pub(crate) struct MemoryRecord {
    #[key]
    id: String,
    kind: String,
    content: String,
    status: String,
    confidence: String,
    importance: i64,
    pinned: bool,
    scope_type: String,
    scope_key: String,
    source: String,
    source_message_id: String,
    created_at: i64,
    updated_at: i64,
    expires_at: i64,
    last_used_at: i64,
    // Who the claim came from: `user` or `tool`. Additive column, defaulting to
    // `user` — everything written before it existed was extracted from a
    // conversation.
    provenance: String,
    // Truth signals — see `Memory`'s own docs for why they are kept apart from
    // `recall_count`.
    belief_state: String,
    support_count: i64,
    contradiction_count: i64,
    last_confirmed_at: i64,
    superseded_by: String,
    // JSON array of `Evidence`; empty when none. JSON rather than a child table
    // because the list is capped at a handful of entries and is always read with
    // its memory — a join would buy nothing and cost `list()`, which runs every
    // turn.
    evidence: String,
    recall_count: i64,
    // Base64 of the L2-normalized embedding's little-endian f32 bytes; empty
    // when not embedded. Base64 rather than a JSON array because a 1024-dim
    // vector is ~5.5 KB encoded against ~12 KB as text, and `list()` loads
    // every row on every turn.
    embedding: String,
    // Model that produced `embedding`; empty when not embedded.
    embedding_model: String,
}

/// Encode an embedding for storage: little-endian f32 bytes, base64. Empty
/// vector → empty string, so "not embedded" needs no sentinel.
fn encode_embedding(vector: &[f32]) -> String {
    use base64::Engine;
    if vector.is_empty() {
        return String::new();
    }
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode a stored embedding. Anything malformed — bad base64, a length that is
/// not a whole number of f32s — reads as *not embedded* rather than failing the
/// load: a corrupt vector must cost recall quality, never access to the memory
/// itself.
fn decode_embedding(encoded: &str) -> Vec<f32> {
    use base64::Engine;
    if encoded.is_empty() {
        return Vec::new();
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return Vec::new();
    };
    if bytes.len() % 4 != 0 {
        return Vec::new();
    }
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Encode an evidence list as JSON. Empty list → empty string, so "no evidence"
/// needs no sentinel and costs no bytes on the many rows that have none.
fn encode_evidence(evidence: &[Evidence]) -> String {
    if evidence.is_empty() {
        return String::new();
    }
    serde_json::to_string(evidence).unwrap_or_default()
}

/// Decode a stored evidence list. Malformed JSON reads as *no evidence* rather
/// than failing the load — provenance is an audit aid, and losing it must never
/// cost access to the memory itself.
fn decode_evidence(encoded: &str) -> Vec<Evidence> {
    if encoded.is_empty() {
        return Vec::new();
    }
    serde_json::from_str(encoded).unwrap_or_default()
}

/// Connection to the memory database. Holds only `MemoryRecord`.
///
/// Backed by the Turso engine with a per-operation connection pool: `inner` is a
/// plain `Arc<toasty::Db>` (no outer `Mutex`), and every method checks out a
/// pooled `Connection`, so independent reads/writes run concurrently. Writes use
/// Turso's MVCC concurrent-write mode and retry on commit conflict (see
/// `infra::persistence::with_write_retry`).
/// Every memory in a legacy `memory.db`, for the one-time merge into
/// `komo.db`.
///
/// The old file is brought up to the current column set first — a `memory.db`
/// written before `belief_state` (or with the retired `recall_query_hashes`
/// still on it) cannot be read through today's model — and a pre-Turso SQLite
/// file is opened with the SQLite driver, because that per-store migration ran
/// here before the merge and dropping the path would strand anyone who had not
/// upgraded through it.
pub(crate) async fn import_from(path: &Path) -> anyhow::Result<Vec<Memory>> {
    let native = crate::persistence::turso_marker_path(path).exists();
    if native {
        ensure_columns(path).await?;
    }
    let url = match native {
        true => format!("turso:{}", path.display()),
        false => format!("sqlite:{}", path.display()),
    };
    let db = toasty::Db::builder()
        .models(toasty::models!(MemoryRecord))
        .connect(&url)
        .await
        .with_context(|| format!("opening {} to merge it in", path.display()))?;
    let mut conn = db.connection().await?;
    let rows = toasty::query!(MemoryRecord).exec(&mut conn).await?;
    Ok(rows.into_iter().map(memory_from_record).collect())
}

/// Bring an existing file's `memory_records` up to the current column set,
/// before toasty opens it.
pub(crate) async fn ensure_schema(path: &Path) -> anyhow::Result<()> {
    ensure_columns(path).await
}

impl Db {
    /// One-time migration: import every memory from a legacy markdown directory
    /// into a freshly-created db. No-op when the directory is absent or the db
    /// already holds memories (so it is safe to call on every startup). Returns
    /// the number imported.
    pub async fn import_legacy_markdown(&self, dir: &Path) -> anyhow::Result<usize> {
        // Only seed an empty db — never double-import or fight live writes.
        if !self.list().await?.is_empty() {
            return Ok(0);
        }
        let legacy = MdMemoryStore::new(dir.to_path_buf());
        let memories = legacy.read_all().await?;
        let count = memories.len();
        for memory in &memories {
            self.save(memory).await?;
        }
        Ok(count)
    }
}

fn record_from_memory(memory: &Memory) -> MemoryRecord {
    MemoryRecord {
        id: memory.id.clone(),
        kind: memory.kind.as_str().to_string(),
        content: memory.content.clone(),
        status: memory.status.as_str().to_string(),
        confidence: memory.confidence.as_str().to_string(),
        importance: memory.importance as i64,
        pinned: memory.pinned,
        scope_type: memory.scope.type_str().to_string(),
        scope_key: memory.scope.key(),
        source: memory.source.clone(),
        source_message_id: memory.source_message_id.clone(),
        created_at: memory.created_at,
        updated_at: memory.updated_at,
        expires_at: memory.expires_at.unwrap_or(0),
        last_used_at: memory.last_used_at.unwrap_or(0),
        provenance: memory.provenance.as_str().to_string(),
        belief_state: memory.belief.as_str().to_string(),
        support_count: memory.support_count,
        contradiction_count: memory.contradiction_count,
        last_confirmed_at: memory.last_confirmed_at.unwrap_or(0),
        superseded_by: memory.superseded_by.clone(),
        evidence: encode_evidence(&memory.evidence),
        recall_count: memory.recall_count,
        embedding: encode_embedding(&memory.embedding),
        embedding_model: memory.embedding_model.clone(),
    }
}

fn memory_from_record(record: MemoryRecord) -> Memory {
    let nonzero = |v: i64| (v != 0).then_some(v);
    Memory {
        id: record.id,
        kind: parse_memory_kind(&record.kind),
        content: record.content,
        status: parse_memory_status(&record.status),
        confidence: parse_memory_confidence(&record.confidence),
        importance: record.importance as i32,
        pinned: record.pinned,
        scope: MemoryScope::from_parts(&record.scope_type, &record.scope_key),
        source: record.source,
        source_message_id: record.source_message_id,
        created_at: record.created_at,
        updated_at: record.updated_at,
        expires_at: nonzero(record.expires_at),
        last_used_at: nonzero(record.last_used_at),
        belief: parse_belief_state(&record.belief_state),
        provenance: parse_memory_provenance(&record.provenance),
        support_count: record.support_count,
        contradiction_count: record.contradiction_count,
        last_confirmed_at: nonzero(record.last_confirmed_at),
        superseded_by: record.superseded_by,
        evidence: decode_evidence(&record.evidence),
        recall_count: record.recall_count,
        embedding: decode_embedding(&record.embedding),
        embedding_model: record.embedding_model,
    }
}

#[async_trait]
impl MemoryRepository for Db {
    async fn save(&self, memory: &Memory) -> anyhow::Result<()> {
        // MVCC: retry the whole transaction on a commit conflict. Each attempt
        // re-checks out its own pooled connection.
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let r = record_from_memory(memory);
            // Overwrite on id collision (save is create-or-replace), mirroring
            // the markdown store's filename-keyed overwrite.
            if let Ok(mut existing) = MemoryRecord::get_by_id(&mut conn, &r.id).await {
                existing
                    .update()
                    .kind(r.kind)
                    .content(r.content)
                    .status(r.status)
                    .confidence(r.confidence)
                    .importance(r.importance)
                    .pinned(r.pinned)
                    .scope_type(r.scope_type)
                    .scope_key(r.scope_key)
                    .source(r.source)
                    .source_message_id(r.source_message_id)
                    .updated_at(r.updated_at)
                    .expires_at(r.expires_at)
                    .last_used_at(r.last_used_at)
                    .provenance(r.provenance)
                    .belief_state(r.belief_state)
                    .support_count(r.support_count)
                    .contradiction_count(r.contradiction_count)
                    .last_confirmed_at(r.last_confirmed_at)
                    .superseded_by(r.superseded_by)
                    .evidence(r.evidence)
                    .recall_count(r.recall_count)
                    .embedding(r.embedding)
                    .embedding_model(r.embedding_model)
                    .exec(&mut conn)
                    .await?;
                return Ok(());
            }
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
            .await?;
            Ok(())
        })
        .await
    }

    async fn list(&self) -> anyhow::Result<Vec<Memory>> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut conn = self.inner.connection().await?;
        let rows = toasty::query!(MemoryRecord).exec(&mut conn).await?;
        let mut memories: Vec<Memory> = rows
            .into_iter()
            .map(memory_from_record)
            .filter(|m| !m.is_expired(now))
            .collect();
        memories.sort_by_key(|m| m.created_at);
        Ok(memories)
    }

    /// Fetch by id directly — unlike the default (which scans `list`), this
    /// sees expired and any-status rows, so governance can still operate on
    /// them.
    async fn get(&self, id: &str) -> anyhow::Result<Option<Memory>> {
        let mut conn = self.inner.connection().await?;
        Ok(MemoryRecord::get_by_id(&mut conn, id)
            .await
            .ok()
            .map(memory_from_record))
    }
}

/// Bring an existing `memory_records` table up to the current `MemoryRecord`
/// shape by adding any columns it lacks, in place (no data loss, idempotent) —
/// the shared additive migration in `infra/persistence/mod.rs`. When adding a
/// `MemoryRecord` field, extend this list (NOT NULL with a DEFAULT, or nullable).
async fn ensure_columns(path: &Path) -> anyhow::Result<()> {
    const EXPECTED: &[(&str, &str)] = &[
        (
            "recall_count",
            "\"recall_count\" integer NOT NULL DEFAULT 0",
        ),
        // `user` is what every row written before this column meant: they were
        // all extracted from conversations, never from fetched content.
        ("provenance", "\"provenance\" text NOT NULL DEFAULT 'user'"),
        // Truth signals. `belief_state` defaults to `current`, which is exactly
        // what every row written before the column existed means.
        (
            "belief_state",
            "\"belief_state\" text NOT NULL DEFAULT 'current'",
        ),
        (
            "support_count",
            "\"support_count\" integer NOT NULL DEFAULT 0",
        ),
        (
            "contradiction_count",
            "\"contradiction_count\" integer NOT NULL DEFAULT 0",
        ),
        (
            "last_confirmed_at",
            "\"last_confirmed_at\" integer NOT NULL DEFAULT 0",
        ),
        (
            "superseded_by",
            "\"superseded_by\" text NOT NULL DEFAULT ''",
        ),
        ("evidence", "\"evidence\" text NOT NULL DEFAULT ''"),
        ("embedding", "\"embedding\" text NOT NULL DEFAULT ''"),
        (
            "embedding_model",
            "\"embedding_model\" text NOT NULL DEFAULT ''",
        ),
    ];
    crate::persistence::ensure_columns(path, "memory_records", EXPECTED).await?;

    // Columns this komo no longer models. `recall_query_hashes` backed the
    // dream-promotion query-diversity signal, added 2026-07-03 and dropped when
    // promotion moved to truth signals (2026-08-12) — but dropping it from the
    // model left it in every store built in between, `NOT NULL` and with no
    // default, so every insert after the upgrade failed the constraint and the
    // store silently stopped accepting memories. Durable data may only change
    // additively (see AGENTS.md); this is the repair for the one time it did
    // not.
    const RETIRED: &[&str] = &["recall_query_hashes"];
    crate::persistence::drop_retired_columns(path, "memory_records", RETIRED).await
}

#[cfg(test)]
mod tests;
