//! [`ChunkIndex`] over Turso — the same `komo.db` everything else here writes.
//!
//! Both corpora (the note vault and komo's own transcripts) are a few thousand
//! chunks, and the engine komo already links can answer both retrieval arms
//! directly:
//!
//! - **dense** — `vector_distance_cos` over a `BLOB` of little-endian `f32`s.
//!   Brute force, no ANN index: there is none in Turso, and at this scale a
//!   full scan is tens of milliseconds — the same trade memory recall already
//!   makes in Rust.
//! - **lexical** — a `terms` column holding the chunk's tokens, matched with
//!   `instr`. Turso *has* an FTS index method, but it refuses to be created in
//!   MVCC mode ("Custom index modules are not supported in MVCC mode"), and
//!   MVCC is not optional here — it is what lets the gateway's sessions write
//!   concurrently. So the tokenizing moves to index time, where it is paid
//!   once per chunk instead of once per query, and the query does set
//!   membership over the result.
//!
//! Tokens come from the splitter memory recall uses ([`lexical_terms`]): CJK
//! bigrams plus ASCII words. That sharing is the point — a Chinese query
//! against an English note is the same problem in both corpora, and two
//! answers to it would be a bug in one of them.
//!
//! The table is **disposable**: every row is reproducible from its source file
//! or transcript, so `reset` deletes the collection's rows and the next
//! `komo wiki index` (or the first `session` search) rebuilds them.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use komo_core::domain::chunk_index::{
    ChunkHit, ChunkIndex, DIVERSIFY_OVERFETCH, IndexedChunk, IndexedFile, MAX_CHUNKS_PER_FILE,
    diversify, lexical_terms, reciprocal_rank_fusion,
};

use crate::persistence::with_write_retry;

/// One table for every corpus, split by `collection`. Created here rather than
/// through toasty's `push_schema` because there is no model to derive it from:
/// this is the one table komo talks to in SQL, so it owns its own DDL and needs
/// no parity test against a generated schema.
const CHUNK_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS chunk_records (
    collection TEXT NOT NULL,
    id TEXT NOT NULL,
    path TEXT NOT NULL,
    heading_path TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    text TEXT NOT NULL,
    terms TEXT NOT NULL,
    mtime INTEGER NOT NULL,
    embedding BLOB NOT NULL,
    embedding_model TEXT NOT NULL,
    PRIMARY KEY (collection, id)
)";

/// The note vault's collection.
pub const WIKI: &str = "komo_wiki";
/// komo's own transcripts. A separate collection, not a separate store: the two
/// corpora are indexed the same way and neither may see the other's chunks.
pub const SESSIONS: &str = "komo_sessions";

/// Upper bound on how many query terms reach the SQL expression, which grows
/// one `instr` call per term. A search phrase never comes near it; a whole
/// pasted message would.
const MAX_QUERY_TERMS: usize = 48;

/// Rows written per statement batch. Bounded so one indexing batch cannot hold a
/// write open long enough to conflict with every session in the gateway.
const WRITE_BATCH: usize = 200;

pub struct TursoChunkIndex {
    db: Arc<turso::Database>,
    collection: String,
}

impl TursoChunkIndex {
    /// Open the index for one collection, creating the shared table if this is
    /// the first caller to need it.
    pub async fn open(db: Arc<turso::Database>, collection: &str) -> anyhow::Result<Self> {
        let this = Self {
            db,
            collection: collection.to_string(),
        };
        this.conn()
            .await?
            .execute(CHUNK_TABLE_DDL, ())
            .await
            .context("creating chunk_records")?;
        Ok(this)
    }

    /// A connection in the engine mode the file was written in — toasty's
    /// driver enables MVCC per connection, and a raw one has to say so too or
    /// it reads the file in the wrong mode.
    async fn conn(&self) -> anyhow::Result<turso::Connection> {
        let conn = self.db.connect()?;
        conn.pragma_update("journal_mode", "'mvcc'").await.ok();
        Ok(conn)
    }

    /// Vector width currently stored, if anything is.
    async fn stored_dim(&self) -> anyhow::Result<Option<usize>> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query(
                "SELECT length(embedding) FROM chunk_records WHERE collection = ? LIMIT 1",
                vec![turso::Value::Text(self.collection.clone())],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        Ok(int(&row.get_value(0)?).map(|bytes| bytes as usize / 4))
    }

    /// The dense arm: cosine over every chunk in the collection, floored.
    async fn dense_arm(
        &self,
        query: &[f32],
        depth: usize,
        min_score: f32,
    ) -> anyhow::Result<Vec<ChunkHit>> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query(
                "SELECT id, path, heading_path, ordinal, text, mtime, embedding_model, \
                 1.0 - vector_distance_cos(embedding, ?) AS score \
                 FROM chunk_records WHERE collection = ? AND score >= ? \
                 ORDER BY score DESC LIMIT ?",
                vec![
                    turso::Value::Blob(vector_blob(query)),
                    turso::Value::Text(self.collection.clone()),
                    turso::Value::Real(min_score as f64),
                    turso::Value::Integer(depth as i64),
                ],
            )
            .await
            .context("cosine search over chunk_records")?;
        let mut hits = Vec::new();
        while let Some(row) = rows.next().await? {
            let Some(chunk) = read_chunk(&row)? else {
                continue;
            };
            hits.push(ChunkHit {
                chunk,
                score: real(&row.get_value(7)?).unwrap_or(0.0) as f32,
            });
        }
        Ok(hits)
    }

    /// The lexical arm: IDF-weighted term coverage.
    ///
    /// Two statements, because the weights have to be known before the rows are
    /// ranked: the first counts how many chunks carry each term (its document
    /// frequency), the second scores with those weights baked in. Neither ships
    /// a chunk's text unless it is in the answer.
    ///
    /// Presence, not term frequency — `terms` is a set, so a word repeated in a
    /// note counts once. What survives is the half that decides ranking here:
    /// a rare term outweighs a common one, so an exact order id beats a note
    /// that merely shares four everyday bigrams with the query.
    async fn lexical_arm(&self, query_text: &str, depth: usize) -> anyhow::Result<Vec<ChunkHit>> {
        let mut terms = lexical_terms(query_text);
        terms.truncate(MAX_QUERY_TERMS);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let needles: Vec<turso::Value> = terms
            .iter()
            .map(|term| turso::Value::Text(format!(" {term} ")))
            .collect();

        let conn = self.conn().await?;
        let df_sql = format!(
            "SELECT {}, count(*) FROM chunk_records WHERE collection = ?",
            terms
                .iter()
                .map(|_| "sum(instr(terms, ?) > 0)")
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut params = needles.clone();
        params.push(turso::Value::Text(self.collection.clone()));
        let mut rows = conn
            .query(&df_sql, params)
            .await
            .context("counting term frequencies in chunk_records")?;
        let Some(row) = rows.next().await? else {
            return Ok(Vec::new());
        };
        let total = int(&row.get_value(terms.len())?).unwrap_or(0);
        if total == 0 {
            return Ok(Vec::new());
        }
        let weights: Vec<f64> = (0..terms.len())
            .map(|i| {
                let df = int(&row.get_value(i).unwrap_or(turso::Value::Null)).unwrap_or(0);
                idf(df, total)
            })
            .collect();

        let score_sql = format!(
            "SELECT id, path, heading_path, ordinal, text, mtime, embedding_model, {} AS lex \
             FROM chunk_records WHERE collection = ? AND lex > 0 ORDER BY lex DESC LIMIT ?",
            weights
                .iter()
                .map(|w| format!("(instr(terms, ?) > 0) * {w}"))
                .collect::<Vec<_>>()
                .join(" + ")
        );
        let mut params = needles;
        params.push(turso::Value::Text(self.collection.clone()));
        params.push(turso::Value::Integer(depth as i64));
        let mut rows = conn
            .query(&score_sql, params)
            .await
            .context("term search over chunk_records")?;
        let mut hits = Vec::new();
        while let Some(row) = rows.next().await? {
            let Some(chunk) = read_chunk(&row)? else {
                continue;
            };
            hits.push(ChunkHit {
                chunk,
                score: real(&row.get_value(7)?).unwrap_or(0.0) as f32,
            });
        }
        Ok(hits)
    }
}

/// BM25's IDF: a term in every chunk carries nothing, a term in one carries
/// most. `+ 1.0` inside the log keeps it non-negative, so a term more common
/// than half the corpus cannot subtract from a chunk's score.
fn idf(df: i64, total: i64) -> f64 {
    let df = df.max(0) as f64;
    let total = total as f64;
    (1.0 + (total - df + 0.5) / (df + 0.5)).ln()
}

/// `f32` slice → the blob layout Turso's vector functions read: little-endian
/// `f32`s, no header. An even-length blob is a float32 dense vector to them.
fn vector_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn text(value: &turso::Value) -> Option<String> {
    match value {
        turso::Value::Text(s) => Some(s.clone()),
        _ => None,
    }
}

fn int(value: &turso::Value) -> Option<i64> {
    match value {
        turso::Value::Integer(i) => Some(*i),
        _ => None,
    }
}

fn real(value: &turso::Value) -> Option<f64> {
    match value {
        turso::Value::Real(f) => Some(*f),
        turso::Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

/// Read the seven payload columns every search selects. `None` for a row whose
/// shape does not decode — one malformed row costs its own result, never the
/// query.
///
/// `embedding` is deliberately absent: search never selects the blob, because a
/// 4 KB vector per hit is read by nobody.
fn read_chunk(row: &turso::Row) -> anyhow::Result<Option<IndexedChunk>> {
    let (Some(id), Some(path), Some(ordinal)) = (
        text(&row.get_value(0)?),
        text(&row.get_value(1)?),
        int(&row.get_value(3)?),
    ) else {
        return Ok(None);
    };
    Ok(Some(IndexedChunk {
        id,
        path,
        heading_path: text(&row.get_value(2)?).unwrap_or_default(),
        ordinal: ordinal.max(0) as usize,
        text: text(&row.get_value(4)?).unwrap_or_default(),
        mtime: int(&row.get_value(5)?).unwrap_or(0),
        embedding: Vec::new(),
        embedding_model: text(&row.get_value(6)?).unwrap_or_default(),
    }))
}

#[async_trait]
impl ChunkIndex for TursoChunkIndex {
    async fn upsert(&self, chunks: &[IndexedChunk]) -> anyhow::Result<()> {
        let Some(dim) = chunks.iter().map(|c| c.embedding.len()).find(|n| *n > 0) else {
            return Ok(());
        };
        if let Some(bad) = chunks
            .iter()
            .find(|c| !c.embedding.is_empty() && c.embedding.len() != dim)
        {
            anyhow::bail!(
                "mixed vector widths in one batch ({} vs {} for {}) — the index stores one width",
                dim,
                bad.embedding.len(),
                bad.path
            );
        }
        // One width per collection: `vector_distance_cos` refuses to compare
        // two, so a model change is a rebuild and has to say so.
        if let Some(existing) = self.stored_dim().await?
            && existing != dim
        {
            anyhow::bail!(
                "index was built for {existing}-dim vectors but the embedding \
                 model produces {dim}-dim. Vectors of two widths are not comparable — \
                 run `komo wiki index --rebuild` to rebuild it."
            );
        }

        for batch in chunks.chunks(WRITE_BATCH) {
            with_write_retry(|| async {
                let conn = self.conn().await?;
                conn.execute("BEGIN CONCURRENT", ()).await?;
                let write = async {
                    for chunk in batch.iter().filter(|c| !c.embedding.is_empty()) {
                        conn.execute(
                            "INSERT OR REPLACE INTO chunk_records (collection, id, path, \
                             heading_path, ordinal, text, terms, mtime, embedding, \
                             embedding_model) VALUES (?,?,?,?,?,?,?,?,?,?)",
                            vec![
                                turso::Value::Text(self.collection.clone()),
                                turso::Value::Text(chunk.id.clone()),
                                turso::Value::Text(chunk.path.clone()),
                                turso::Value::Text(chunk.heading_path.clone()),
                                turso::Value::Integer(chunk.ordinal as i64),
                                turso::Value::Text(chunk.text.clone()),
                                // Indexed over the same text that was embedded
                                // (heading trail + body), so both arms see one
                                // document.
                                turso::Value::Text(term_column(&format!(
                                    "{}\n{}",
                                    chunk.heading_path, chunk.text
                                ))),
                                turso::Value::Integer(chunk.mtime),
                                turso::Value::Blob(vector_blob(&chunk.embedding)),
                                turso::Value::Text(chunk.embedding_model.clone()),
                            ],
                        )
                        .await?;
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                match write {
                    Ok(()) => {
                        conn.execute("COMMIT", ()).await?;
                        Ok(())
                    }
                    Err(e) => {
                        conn.execute("ROLLBACK", ()).await.ok();
                        Err(e)
                    }
                }
            })
            .await
            .context("writing chunk_records")?;
        }
        Ok(())
    }

    async fn search(
        &self,
        query: &[f32],
        query_text: &str,
        limit: usize,
        min_score: f32,
    ) -> anyhow::Result<Vec<ChunkHit>> {
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // Each arm is fetched deeper than it will contribute, because the cap
        // below throws hits away: without the headroom, a note that fills an
        // arm's top-k just yields a shorter run instead of a wider one.
        let depth = limit.saturating_mul(DIVERSIFY_OVERFETCH);
        let dense = self.dense_arm(query, depth, min_score).await?;
        let lexical = self.lexical_arm(query_text, depth).await?;

        // Cap each arm before fusing: a note that owns most of one arm's top-k
        // is spending slots that fusion never gets to choose among. RRF scores
        // by rank, so discarding a note's 3rd chunk here promotes everything
        // below it rather than leaving a hole.
        let dense = diversify(dense, limit, MAX_CHUNKS_PER_FILE);
        let lexical = diversify(lexical, limit, MAX_CHUNKS_PER_FILE);
        // Dense-only: return cosine scores unchanged, so a query with no usable
        // terms reads on the same scale a caller's `min_score` is on.
        if lexical.is_empty() {
            return Ok(dense.into_iter().take(limit).collect());
        }
        Ok(reciprocal_rank_fusion(vec![dense, lexical], limit))
    }

    async fn indexed(&self) -> anyhow::Result<HashMap<String, IndexedFile>> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query(
                "SELECT path, min(mtime), count(*) FROM chunk_records \
                 WHERE collection = ? GROUP BY path",
                vec![turso::Value::Text(self.collection.clone())],
            )
            .await
            .context("reading indexed files")?;
        let mut out = HashMap::new();
        while let Some(row) = rows.next().await? {
            let Some(path) = text(&row.get_value(0)?) else {
                continue;
            };
            out.insert(
                path,
                IndexedFile {
                    // A file's chunks all carry the same mtime; if a partial
                    // re-index left a mix, the oldest is the honest answer — it
                    // forces a re-index rather than skipping a half-updated file.
                    mtime: int(&row.get_value(1)?).unwrap_or(0),
                    chunks: int(&row.get_value(2)?).unwrap_or(0).max(0) as usize,
                },
            );
        }
        Ok(out)
    }

    async fn delete_paths(&self, paths: &[String]) -> anyhow::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        for batch in paths.chunks(WRITE_BATCH) {
            with_write_retry(|| async {
                let conn = self.conn().await?;
                let placeholders = vec!["?"; batch.len()].join(",");
                let mut params = vec![turso::Value::Text(self.collection.clone())];
                params.extend(batch.iter().cloned().map(turso::Value::Text));
                conn.execute(
                    &format!(
                        "DELETE FROM chunk_records WHERE collection = ? AND path IN ({placeholders})"
                    ),
                    params,
                )
                .await?;
                Ok(())
            })
            .await
            .context("deleting chunk_records")?;
        }
        Ok(())
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query(
                "SELECT count(*) FROM chunk_records WHERE collection = ?",
                vec![turso::Value::Text(self.collection.clone())],
            )
            .await
            .context("counting chunk_records")?;
        let Some(row) = rows.next().await? else {
            return Ok(0);
        };
        Ok(int(&row.get_value(0)?).unwrap_or(0).max(0) as usize)
    }

    async fn reset(&self) -> anyhow::Result<()> {
        with_write_retry(|| async {
            let conn = self.conn().await?;
            conn.execute(
                "DELETE FROM chunk_records WHERE collection = ?",
                vec![turso::Value::Text(self.collection.clone())],
            )
            .await?;
            Ok(())
        })
        .await
        .context("clearing chunk_records")
    }

    async fn vector_spec(&self) -> anyhow::Result<Option<(usize, String)>> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query(
                "SELECT length(embedding), embedding_model FROM chunk_records \
                 WHERE collection = ? LIMIT 1",
                vec![turso::Value::Text(self.collection.clone())],
            )
            .await
            .context("reading the chunk index's vector spec")?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let Some(bytes) = int(&row.get_value(0)?) else {
            return Ok(None);
        };
        Ok(Some((
            bytes.max(0) as usize / 4,
            text(&row.get_value(1)?).unwrap_or_default(),
        )))
    }
}

/// A chunk's tokens as one space-delimited, space-padded string, so a term
/// match is `instr(terms, " <term> ")` — exact, with no word-boundary guessing
/// a substring search would need.
fn term_column(text: &str) -> String {
    let terms = lexical_terms(text);
    if terms.is_empty() {
        return String::new();
    }
    format!(" {} ", terms.join(" "))
}

#[cfg(test)]
mod tests;
