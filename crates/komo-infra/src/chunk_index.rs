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
mod tests {
    use super::*;

    async fn index(name: &str) -> TursoChunkIndex {
        let path = std::env::temp_dir().join(format!("komo_chunk_{name}.db"));
        crate::persistence::reset_test_db(&path);
        let db = Arc::new(
            turso::Builder::new_local(path.to_string_lossy().as_ref())
                .build()
                .await
                .unwrap(),
        );
        TursoChunkIndex::open(db, "komo_wiki").await.unwrap()
    }

    /// Unit vectors, as the `EmbeddingClient` contract guarantees.
    fn chunk(path: &str, ordinal: usize, embedding: Vec<f32>) -> IndexedChunk {
        IndexedChunk {
            id: IndexedChunk::make_id(path, ordinal),
            path: path.to_string(),
            heading_path: format!("{path} > 节"),
            ordinal,
            text: format!("{path} 第{ordinal}段的正文"),
            mtime: 1780000000 + ordinal as i64,
            embedding,
            embedding_model: "test-model".into(),
        }
    }

    fn chunk_with_text(path: &str, text: &str, embedding: Vec<f32>) -> IndexedChunk {
        let mut c = chunk(path, 0, embedding);
        c.text = text.to_string();
        c.heading_path = path.to_string();
        c
    }

    /// A corpus that was never indexed must answer "empty", not error.
    #[tokio::test]
    async fn empty_index_reads_as_empty() {
        let index = index("empty").await;
        assert_eq!(index.count().await.unwrap(), 0);
        assert!(
            index
                .search(&[1.0, 0.0], "q", 5, 0.0)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(index.indexed().await.unwrap().is_empty());
        assert!(index.vector_spec().await.unwrap().is_none());
        index.delete_paths(&["a.md".into()]).await.unwrap();
    }

    #[tokio::test]
    async fn upsert_then_search_returns_the_nearest_chunk() {
        let index = index("nearest").await;
        index
            .upsert(&[
                chunk("a.md", 0, vec![1.0, 0.0]),
                chunk("b.md", 0, vec![0.0, 1.0]),
            ])
            .await
            .unwrap();

        assert_eq!(index.count().await.unwrap(), 2);
        assert_eq!(index.vector_spec().await.unwrap().unwrap().0, 2);
        assert_eq!(index.vector_spec().await.unwrap().unwrap().1, "test-model");

        let hits = index.search(&[1.0, 0.0], "", 1, 0.0).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk.path, "a.md");
        assert!(hits[0].score > 0.9, "score was {}", hits[0].score);
        // Payload survived the round trip; vectors deliberately did not.
        assert_eq!(hits[0].chunk.heading_path, "a.md > 节");
        assert!(hits[0].chunk.embedding.is_empty());
    }

    /// `min_score` must drop weak neighbours — an unrelated query always has a
    /// nearest chunk, and returning it is how a search tool invents relevance.
    #[tokio::test]
    async fn min_score_filters_weak_hits() {
        let index = index("floor").await;
        index
            .upsert(&[chunk("a.md", 0, vec![1.0, 0.0])])
            .await
            .unwrap();
        // Orthogonal query: cosine 0.
        assert!(
            index
                .search(&[0.0, 1.0], "", 5, 0.5)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            index.search(&[0.0, 1.0], "", 5, -1.0).await.unwrap().len(),
            1
        );
    }

    /// Re-indexing an unchanged file must not duplicate its rows — this is what
    /// the derived, stable chunk id buys.
    #[tokio::test]
    async fn upsert_is_idempotent() {
        let index = index("idempotent").await;
        let chunks = [
            chunk("a.md", 0, vec![1.0, 0.0]),
            chunk("a.md", 1, vec![0.0, 1.0]),
        ];
        index.upsert(&chunks).await.unwrap();
        index.upsert(&chunks).await.unwrap();
        assert_eq!(index.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn indexed_groups_by_path_and_delete_removes_a_whole_file() {
        let index = index("grouping").await;
        index
            .upsert(&[
                chunk("a.md", 0, vec![1.0, 0.0]),
                chunk("a.md", 1, vec![0.0, 1.0]),
                chunk("b.md", 0, vec![0.0, 1.0]),
            ])
            .await
            .unwrap();

        let indexed = index.indexed().await.unwrap();
        assert_eq!(indexed.len(), 2);
        assert_eq!(indexed["a.md"].chunks, 2);
        assert_eq!(indexed["b.md"].chunks, 1);
        // Oldest mtime wins, so a half-updated file re-indexes.
        assert_eq!(indexed["a.md"].mtime, 1780000000);

        index.delete_paths(&["a.md".into()]).await.unwrap();
        assert_eq!(index.count().await.unwrap(), 1);
        assert!(index.indexed().await.unwrap().contains_key("b.md"));
    }

    /// The entire reason hybrid exists: an exact token the dense arm cannot
    /// reach. The query vector points *away* from the note holding the id, so
    /// only the lexical arm can surface it.
    #[tokio::test]
    async fn lexical_arm_finds_an_exact_id_the_dense_arm_points_away_from() {
        let index = index("lexical").await;
        index
            .upsert(&[
                chunk_with_text(
                    "orders.md",
                    "订单 ORD-A1B2C3 在 complete 步骤连续提交失败",
                    vec![1.0, 0.0],
                ),
                chunk_with_text(
                    "unrelated.md",
                    "今天的天气很好，适合出门散步",
                    vec![0.0, 1.0],
                ),
            ])
            .await
            .unwrap();

        // Vector points at `unrelated.md`; the text names the id in `orders.md`.
        let hits = index
            .search(&[0.0, 1.0], "ORD-A1B2C3", 5, -1.0)
            .await
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.chunk.path.as_str()).collect();
        assert!(
            paths.contains(&"orders.md"),
            "lexical arm did not surface the exact id: {paths:?}"
        );
    }

    /// CJK must tokenize, or the lexical arm is dead weight on this vault.
    #[tokio::test]
    async fn chinese_text_matches_a_chinese_query() {
        let index = index("cjk").await;
        index
            .upsert(&[
                chunk_with_text(
                    "order.md",
                    "订单创建失败的排查记录与链路还原",
                    vec![1.0, 0.0],
                ),
                chunk_with_text("weather.md", "今天的天气很好", vec![0.0, 1.0]),
            ])
            .await
            .unwrap();
        let hits = index
            .search(&[0.0, 1.0], "订单创建失败", 5, -1.0)
            .await
            .unwrap();
        assert_eq!(hits[0].chunk.path, "order.md", "{hits:?}");
    }

    /// A rare term must outweigh a common one, or a query mixing an id with
    /// everyday words ranks the everyday words.
    #[tokio::test]
    async fn a_rare_term_outranks_shared_common_ones() {
        let index = index("idf").await;
        let mut chunks: Vec<IndexedChunk> = (0..8)
            .map(|i| {
                let mut c = chunk_with_text(
                    &format!("common{i}.md"),
                    "结账 服务 编排 记录",
                    vec![0.0, 1.0],
                );
                c.heading_path = format!("common{i}.md");
                c
            })
            .collect();
        chunks.push(chunk_with_text("rare.md", "ORD-ZZZ9 结账", vec![0.0, 1.0]));
        index.upsert(&chunks).await.unwrap();

        // Every common note shares three terms with the query; `rare.md` shares
        // one of those plus the id nothing else carries. Read off the arm
        // itself — fusion with a dense arm that scores all nine alike would say
        // nothing about the lexical ranking.
        let hits = index
            .lexical_arm("结账 服务 编排 ORD-ZZZ9", 5)
            .await
            .unwrap();
        assert_eq!(hits[0].chunk.path, "rare.md", "{hits:?}");
    }

    /// Measured on the real vault: one note held 9 of the dense arm's top 15,
    /// so fusion never saw the notes ranked behind it.
    #[tokio::test]
    async fn one_note_cannot_monopolize_an_arm_before_fusion() {
        let index = index("hog").await;
        let mut chunks: Vec<IndexedChunk> = (0..6)
            .map(|i| {
                let mut c = chunk("hog.md", i, vec![1.0, 0.0]);
                c.text = "结账 服务 编排".into();
                c
            })
            .collect();
        chunks.push(chunk_with_text(
            "rival.md",
            "结账 服务 地图",
            vec![0.99, 0.01],
        ));
        index.upsert(&chunks).await.unwrap();

        // Dense-only, so this isolates the arm cap from anything lexical.
        let hits = index.search(&[1.0, 0.0], "", 5, -1.0).await.unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.chunk.path.as_str()).collect();
        assert_eq!(
            paths.iter().filter(|p| **p == "hog.md").count(),
            MAX_CHUNKS_PER_FILE,
            "{paths:?}"
        );
        assert!(paths.contains(&"rival.md"), "{paths:?}");
    }

    /// A query with no usable terms leaves the lexical arm empty, and the hits
    /// must then still carry a cosine — the scale `min_score` is on.
    #[tokio::test]
    async fn dense_only_search_keeps_cosine_scores() {
        let index = index("dense_only").await;
        index
            .upsert(&[chunk("a.md", 0, vec![1.0, 0.0])])
            .await
            .unwrap();
        let hits = index.search(&[1.0, 0.0], "", 5, 0.0).await.unwrap();
        assert_eq!(hits.len(), 1);
        // Cosine, not an RRF value (which would be ~0.5 here).
        assert!(hits[0].score > 0.9, "score was {}", hits[0].score);
    }

    /// Changing embedding model changes vector width, and two widths cannot be
    /// compared. This must say so, and say what fixes it.
    #[tokio::test]
    async fn a_different_vector_width_is_rejected_with_a_fix() {
        let index = index("width").await;
        index
            .upsert(&[chunk("a.md", 0, vec![1.0, 0.0])])
            .await
            .unwrap();

        let err = index
            .upsert(&[chunk("b.md", 0, vec![1.0, 0.0, 0.0])])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("2-dim") && err.contains("3-dim"), "{err}");
        assert!(err.contains("--rebuild"), "must name the fix: {err}");
    }

    /// `reset` is what makes a model change possible: after it, an index built
    /// for one width accepts another.
    #[tokio::test]
    async fn reset_allows_a_new_vector_width() {
        let index = index("reset").await;
        index
            .upsert(&[chunk("a.md", 0, vec![1.0, 0.0])])
            .await
            .unwrap();
        assert_eq!(index.count().await.unwrap(), 1);

        index.reset().await.unwrap();
        assert_eq!(index.count().await.unwrap(), 0);

        index
            .upsert(&[chunk("a.md", 0, vec![0.0, 1.0, 0.0])])
            .await
            .unwrap();
        assert_eq!(index.vector_spec().await.unwrap().unwrap().0, 3);
        assert_eq!(index.count().await.unwrap(), 1);
    }

    /// One index stores one vector width; a mixed batch is a bug upstream and
    /// must be rejected loudly rather than half-written.
    #[tokio::test]
    async fn mixed_vector_widths_are_rejected() {
        let index = index("mixed").await;
        let err = index
            .upsert(&[
                chunk("a.md", 0, vec![1.0, 0.0]),
                chunk("b.md", 0, vec![1.0, 0.0, 0.0]),
            ])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("mixed vector widths"), "{err}");
    }

    /// Two corpora, one table: neither may see the other's chunks, even when a
    /// chunk id collides.
    #[tokio::test]
    async fn collections_are_isolated() {
        let path = std::env::temp_dir().join("komo_chunk_collections.db");
        crate::persistence::reset_test_db(&path);
        let db = Arc::new(
            turso::Builder::new_local(path.to_string_lossy().as_ref())
                .build()
                .await
                .unwrap(),
        );
        let wiki = TursoChunkIndex::open(db.clone(), "komo_wiki")
            .await
            .unwrap();
        let sessions = TursoChunkIndex::open(db, "komo_sessions").await.unwrap();

        wiki.upsert(&[chunk_with_text("a.md", "笔记正文", vec![1.0, 0.0])])
            .await
            .unwrap();
        sessions
            .upsert(&[chunk_with_text("a.md", "会话正文", vec![1.0, 0.0])])
            .await
            .unwrap();

        assert_eq!(wiki.count().await.unwrap(), 1);
        assert_eq!(sessions.count().await.unwrap(), 1);
        let hits = wiki.search(&[1.0, 0.0], "", 5, -1.0).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk.text, "笔记正文");
        sessions.reset().await.unwrap();
        assert_eq!(wiki.count().await.unwrap(), 1, "reset crossed collections");
    }

    /// The two corpora may run different embedding models — `[wiki]` is allowed
    /// its own — so one table can hold two vector widths at once. Comparing
    /// them is an error in the engine, and a wiki search must never reach a
    /// session row to find that out.
    #[tokio::test]
    async fn a_search_ignores_another_collection_of_a_different_width() {
        let path = std::env::temp_dir().join("komo_chunk_widths.db");
        crate::persistence::reset_test_db(&path);
        let db = Arc::new(
            turso::Builder::new_local(path.to_string_lossy().as_ref())
                .build()
                .await
                .unwrap(),
        );
        let wiki = TursoChunkIndex::open(db.clone(), WIKI).await.unwrap();
        let sessions = TursoChunkIndex::open(db, SESSIONS).await.unwrap();

        wiki.upsert(&[chunk("a.md", 0, vec![1.0, 0.0])])
            .await
            .unwrap();
        sessions
            .upsert(&[chunk("t.md", 0, vec![0.0, 1.0, 0.0])])
            .await
            .unwrap();

        let hits = wiki.search(&[1.0, 0.0], "", 5, -1.0).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk.path, "a.md");
        let hits = sessions
            .search(&[0.0, 1.0, 0.0], "", 5, -1.0)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk.path, "t.md");
    }

    #[test]
    fn a_term_column_is_space_delimited_and_padded() {
        let column = term_column("订单 ORD-1");
        assert!(column.starts_with(' ') && column.ends_with(' '), "{column}");
        assert!(column.contains(" 订单 "), "{column}");
        assert!(column.contains(" ord "), "{column}");
    }

    #[test]
    fn text_with_no_terms_yields_an_empty_column() {
        assert!(term_column("!!! ,,,").is_empty());
    }

    #[test]
    fn idf_falls_as_a_term_gets_common() {
        assert!(idf(1, 1000) > idf(500, 1000));
        assert!(idf(1000, 1000) >= 0.0, "never negative");
    }
}
