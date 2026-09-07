//! Index a note vault into a [`ChunkIndex`].
//!
//! Lives here rather than in the CLI because two callers need it and must not
//! drift: `komo wiki index` when no gateway is running, and the gateway's own
//! operator action when one is. That is the same rule the operator commands
//! follow — one implementation, two transports.
//!
//! Depends only on the [`ChunkIndex`] and [`EmbeddingClient`] traits, so it stays
//! inside `komo-services`' rule of never reaching into `komo-infra`: the caller
//! supplies the concrete backend and embedder.
//!
//! Indexing is **incremental by mtime**. A note whose file has not changed since
//! it was indexed is never read, chunked, or embedded again — embedding is the
//! entire cost of a run, so skipping unchanged files skips essentially all of it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use komo_core::domain::chunk_index::{ChunkIndex, IndexedChunk};
use komo_core::domain::embedding::EmbeddingClient;

use crate::wiki_chunking::{ChunkSpec, chunk_markdown};

/// How many chunks are embedded per request. A batch is far faster than the
/// same chunks one at a time, but an unbounded one risks the backend's own
/// request limits on a large note.
const EMBED_BATCH: usize = 32;

/// What one indexing run did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexOutcome {
    pub files_seen: usize,
    pub files_changed: usize,
    pub files_removed: usize,
    pub chunks_written: usize,
    /// Chunks in the index when the run finished.
    pub chunks_total: usize,
    /// Notes that could not be read, with the reason. One unreadable file must
    /// not abort a whole run, but it must not vanish silently either.
    pub skipped: Vec<String>,
}

/// Progress callback payload. Emitted per embedded batch, which is the only
/// point where a long run makes observable progress.
#[derive(Debug, Clone, Copy)]
pub struct IndexProgress {
    pub chunks_written: usize,
    pub files_changed: usize,
}

/// Every `.md` under `root`, skipping dot-directories (`.obsidian`, `.trash`).
pub fn walk_vault(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // A vanished or unreadable subdirectory must not abort the whole walk.
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn mtime_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Embed one batch and write it.
async fn flush(
    index: &dyn ChunkIndex,
    embedder: &dyn EmbeddingClient,
    mut batch: Vec<IndexedChunk>,
) -> anyhow::Result<usize> {
    let texts: Vec<String> = batch
        .iter()
        // The heading trail is embedded with the body: a chunk under
        // "checkout policy > 状态机" should match a query naming either, and the
        // body alone often names neither.
        .map(|c| format!("{}\n{}", c.heading_path, c.text))
        .collect();
    let vectors = embedder.embed(&texts).await?;
    if vectors.len() != batch.len() {
        anyhow::bail!(
            "embedding backend returned {} vectors for {} chunks",
            vectors.len(),
            batch.len()
        );
    }
    for (chunk, vector) in batch.iter_mut().zip(vectors) {
        chunk.embedding = vector;
    }
    let n = batch.len();
    index.upsert(&batch).await?;
    Ok(n)
}

/// Index `vault` into `index`, embedding with `embedder`.
///
/// `rebuild` drops the store first. That is not the same as deleting every
/// point: vector width is fixed when the index is created, so changing the
/// embedding model is only possible this way.
///
/// `on_progress` is called after each embedded batch. A run over a large vault
/// takes minutes, and this is the only signal a caller can surface.
pub async fn index_vault(
    index: &dyn ChunkIndex,
    embedder: &dyn EmbeddingClient,
    vault: &Path,
    embedding_model: &str,
    rebuild: bool,
    mut on_progress: impl FnMut(IndexProgress),
) -> anyhow::Result<IndexOutcome> {
    if !vault.is_dir() {
        anyhow::bail!("vault not found: {}", vault.display());
    }
    let files = walk_vault(vault);
    let indexed = if rebuild {
        index.reset().await?;
        HashMap::new()
    } else {
        index.indexed().await?
    };

    // The index stores vault-relative paths, so the whole diff happens in that
    // space — absolute paths would break the moment the vault directory moves.
    let mut on_disk: HashMap<String, (PathBuf, i64)> = HashMap::new();
    for path in &files {
        let rel = path
            .strip_prefix(vault)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        on_disk.insert(rel, (path.clone(), mtime_of(path)));
    }

    let mut changed: Vec<String> = on_disk
        .iter()
        .filter(|(rel, (_, mtime))| indexed.get(*rel).is_none_or(|had| had.mtime != *mtime))
        .map(|(rel, _)| rel.clone())
        .collect();
    changed.sort();
    let live: HashSet<&str> = on_disk.keys().map(String::as_str).collect();
    let removed: Vec<String> = indexed
        .keys()
        .filter(|rel| !live.contains(rel.as_str()))
        .cloned()
        .collect();

    let mut outcome = IndexOutcome {
        files_seen: files.len(),
        files_changed: changed.len(),
        files_removed: removed.len(),
        ..Default::default()
    };

    if !removed.is_empty() {
        index.delete_paths(&removed).await?;
    }
    if changed.is_empty() && removed.is_empty() {
        outcome.chunks_total = index.count().await?;
        return Ok(outcome);
    }
    // A changed file's old chunks must go before its new ones land, or a note
    // that got shorter keeps orphaned tail chunks forever. After `rebuild` the
    // store is already empty.
    if !rebuild {
        index.delete_paths(&changed).await?;
    }

    let spec = ChunkSpec::default();
    let mut pending: Vec<IndexedChunk> = Vec::new();

    for rel in &changed {
        let (path, mtime) = &on_disk[rel];
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) => {
                outcome.skipped.push(format!("{rel}: {e}"));
                continue;
            }
        };
        let title = path.file_stem().unwrap_or_default().to_string_lossy();
        for raw in chunk_markdown(&title, &content, &spec) {
            pending.push(IndexedChunk {
                id: IndexedChunk::make_id(rel, raw.ordinal),
                path: rel.clone(),
                heading_path: raw.heading_path,
                ordinal: raw.ordinal,
                text: raw.text,
                mtime: *mtime,
                embedding: Vec::new(),
                embedding_model: embedding_model.to_string(),
            });
        }
        while pending.len() >= EMBED_BATCH {
            let batch: Vec<IndexedChunk> = pending.drain(..EMBED_BATCH).collect();
            outcome.chunks_written += flush(index, embedder, batch).await?;
            on_progress(IndexProgress {
                chunks_written: outcome.chunks_written,
                files_changed: outcome.files_changed,
            });
        }
    }
    if !pending.is_empty() {
        outcome.chunks_written += flush(index, embedder, pending).await?;
        on_progress(IndexProgress {
            chunks_written: outcome.chunks_written,
            files_changed: outcome.files_changed,
        });
    }

    outcome.chunks_total = index.count().await?;
    Ok(outcome)
}

#[cfg(test)]
mod tests;

/// Serializes indexing runs over one vault and remembers what the last one did.
///
/// Every caller that indexes goes through this: the `wiki_index` tool (from a
/// conversation), `komo wiki index` (through the operator surface), and any
/// scheduled job. Without a single gate the agent's background rebuild and an
/// operator's `komo wiki index --rebuild` would run concurrently over the same
/// store — and a `rebuild` resets it first, so the interleaving is not merely
/// wasteful.
///
/// The last outcome is kept because a background run has nowhere else to report:
/// the tool call that started it has long returned by the time it finishes.
pub struct WikiIndexRunner {
    index: Arc<dyn ChunkIndex>,
    embedder: Arc<dyn EmbeddingClient>,
    vault: PathBuf,
    embedding_model: String,
    state: Arc<Mutex<RunState>>,
}

#[derive(Default)]
struct RunState {
    /// Unix seconds a run started, while one is in flight.
    running_since: Option<i64>,
    /// Whether the in-flight run is a full rebuild. "A run is going" is not
    /// enough for a caller to reason about — only a rebuild empties the store.
    running_rebuild: bool,
    last: Option<LastRun>,
}

/// The previous run's result, as the tool and `status` report it.
#[derive(Debug, Clone)]
pub struct LastRun {
    pub finished_at: i64,
    pub rebuild: bool,
    /// `Ok` carries the run's counts; `Err` its message. A failed rebuild is the
    /// state an operator most needs to see — the index is empty or partial and
    /// nothing else will say so.
    pub result: Result<IndexOutcome, String>,
}

/// What [`WikiIndexRunner::snapshot`] reports about run state.
#[derive(Debug, Clone, Default)]
pub struct RunStatus {
    pub running_since: Option<i64>,
    pub running_rebuild: bool,
    pub last: Option<LastRun>,
}

/// Refused because a run is already going — carries when it started, so a caller
/// can say how long rather than just "busy".
#[derive(Debug, Clone, Copy)]
pub struct AlreadyRunning {
    pub since: i64,
    pub rebuild: bool,
}

/// The right to run, held for the duration of one run.
///
/// A guard rather than a bool because the run may be **abandoned**: a detached
/// background rebuild whose task is aborted, or one that panics, would otherwise
/// leave the in-flight flag set and lock indexing out for the whole life of the
/// process. Dropping the claim always clears it.
///
/// Claiming is separate from running so a caller that intends to run in the
/// background finds out *synchronously* whether it got the slot — reporting
/// "started" for a run that was actually refused is the one lie this design must
/// not tell.
pub struct RunClaim {
    state: Arc<Mutex<RunState>>,
    rebuild: bool,
    /// Set once an outcome has been recorded, so `Drop` knows this was a real
    /// finish rather than an abandonment.
    settled: bool,
}

impl RunClaim {
    /// Whether this claim is for a full rebuild.
    pub fn is_rebuild(&self) -> bool {
        self.rebuild
    }

    fn settle(&mut self, result: Result<&IndexOutcome, String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.running_since = None;
        state.running_rebuild = false;
        state.last = Some(LastRun {
            // Read here rather than passed in: only this point knows when the
            // run actually ended, and a background run has no caller left to ask.
            finished_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            rebuild: self.rebuild,
            result: result.cloned(),
        });
        self.settled = true;
    }
}

impl Drop for RunClaim {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.running_since = None;
        state.running_rebuild = false;
        // Deliberately recorded as a failure, not left as the previous success:
        // an abandoned rebuild leaves the store emptied, and `status` claiming
        // the last run was fine would be the most misleading thing it could say.
        state.last = Some(LastRun {
            finished_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            rebuild: self.rebuild,
            result: Err(
                "the run was abandoned before it finished (process shutting \
                         down, or the task was cancelled)"
                    .to_string(),
            ),
        });
    }
}

impl WikiIndexRunner {
    pub fn new(
        index: Arc<dyn ChunkIndex>,
        embedder: Arc<dyn EmbeddingClient>,
        vault: PathBuf,
        embedding_model: String,
    ) -> Self {
        Self {
            index,
            embedder,
            vault,
            embedding_model,
            state: Arc::new(Mutex::new(RunState::default())),
        }
    }

    pub fn vault(&self) -> &Path {
        &self.vault
    }

    pub fn embedding_model(&self) -> &str {
        &self.embedding_model
    }

    pub fn index(&self) -> &Arc<dyn ChunkIndex> {
        &self.index
    }

    pub fn embedder(&self) -> &Arc<dyn EmbeddingClient> {
        &self.embedder
    }

    /// Current run state — the in-flight run, if any, plus the last one's result.
    pub fn snapshot(&self) -> RunStatus {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        RunStatus {
            running_since: state.running_since,
            running_rebuild: state.running_rebuild,
            last: state.last.clone(),
        }
    }

    /// Claim the right to run, or report the run already in flight.
    ///
    /// `now` is passed in rather than read here so callers keep their own clock,
    /// matching how `cron_actions` takes its timestamps.
    pub fn claim(&self, rebuild: bool, now: i64) -> Result<RunClaim, AlreadyRunning> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(since) = state.running_since {
            return Err(AlreadyRunning {
                since,
                rebuild: state.running_rebuild,
            });
        }
        state.running_since = Some(now);
        state.running_rebuild = rebuild;
        Ok(RunClaim {
            state: self.state.clone(),
            rebuild,
            settled: false,
        })
    }

    /// Run against an already-taken [`RunClaim`]. The claim is consumed, so a
    /// slot cannot be used twice, and its outcome is recorded for `snapshot`.
    ///
    /// Progress goes to the tracing log from here rather than through a callback
    /// each caller supplies: a minutes-long run's only observable signal should
    /// read the same whoever started it, and a background run has no caller left
    /// to hand it to. `komo logs -f` is the progress view.
    pub async fn run_claimed(&self, mut claim: RunClaim) -> anyhow::Result<IndexOutcome> {
        let rebuild = claim.rebuild;
        tracing::info!(vault = %self.vault.display(), rebuild, "wiki index: starting");
        let outcome = index_vault(
            self.index.as_ref(),
            self.embedder.as_ref(),
            &self.vault,
            &self.embedding_model,
            rebuild,
            |progress| {
                tracing::info!(
                    chunks = progress.chunks_written,
                    files = progress.files_changed,
                    "wiki index: embedding"
                )
            },
        )
        .await;
        match &outcome {
            Ok(o) => tracing::info!(
                chunks = o.chunks_total,
                written = o.chunks_written,
                rebuild,
                "wiki index: done"
            ),
            // Logged at error level even though the caller also gets it: a
            // background run's caller is gone, and a failed rebuild leaves the
            // store empty.
            Err(error) => {
                tracing::error!(error = format!("{error:#}"), rebuild, "wiki index: failed")
            }
        }
        claim.settle(outcome.as_ref().map_err(|e| format!("{e:#}")));
        outcome
    }

    /// Claim and run in one step — the synchronous callers' path.
    pub async fn run(
        &self,
        rebuild: bool,
        now: i64,
    ) -> Result<anyhow::Result<IndexOutcome>, AlreadyRunning> {
        let claim = self.claim(rebuild, now)?;
        Ok(self.run_claimed(claim).await)
    }
}

#[cfg(test)]
mod runner_tests;
