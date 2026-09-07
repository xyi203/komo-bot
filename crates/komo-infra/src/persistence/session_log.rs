//! The session's authoritative event log on disk.
//!
//! A session is a **directory**, not a file, because an append-only log that
//! can never be rewritten still has to have a bounded size:
//!
//! ```text
//! sessions/<session-id>/
//!   manifest.json            header + truncation point + segment table
//!   base.<through-seq>.json  what survives below `truncated_before`
//!   000000.jsonl             sealed
//!   000001.jsonl             active — appended to, never rewritten
//! ```
//!
//! **The manifest is storage metadata, never an event.** It is replaced
//! atomically (temp file → fsync → rename → fsync the directory) and only when
//! the segment table changes — not once per append, or the hot path would pay a
//! directory fsync per turn.
//!
//! **`seq` is assigned here.** A session has one writer; handing callers the
//! right to number their own events is how two ingresses end up committing the
//! same position. [`append_batch`](SessionLog::append_batch) stamps and returns
//! the seqs it assigned.
//!
//! **A durable flush means `fsync`.** The recovery semantics this log exists to
//! support — "no `tool/call-started` means the tool never ran" — are claims
//! about what survived a crash, and flushing a userspace buffer does not make
//! one. [`durable_flush`](SessionLog::durable_flush) reaches the filesystem's
//! durability boundary or it fails.
//!
//! **A torn tail is dropped; a hole is refused.** A process killed mid-append
//! can leave a half-written final line, and that is not a record. A record of a
//! known type that will not parse — or a `seq` that skips — is a gap in history
//! the reader cannot reason about, so it refuses the session instead of
//! guessing. A record of an unknown type is neither: it reads as inert and
//! keeps its seq.

use std::path::{Path, PathBuf};

use komo_core::domain::session_event::{
    FoldError, SESSION_EVENT_VERSION, SessionEvent, SessionEventKind, SessionHeader, decode_event,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::warn;

/// Roll to a new segment once the active one passes this, at the next
/// completed-turn boundary.
///
/// Measured against the sessions on a real install: median transcript 2 KB, p90
/// 14 KB, largest 204 KB — and an event log carries the run ledger and the old
/// turn journal's contents besides. At 1 MiB an ordinary conversation never
/// rolls at all, and only a long-lived channel session rolls regularly, which
/// is the granularity retention wants: segments are the unit it deletes.
const SEGMENT_TARGET_BYTES: u64 = 1024 * 1024;

/// Sealed segments over this much invite a retention sweep. Deliberately far
/// above any single turn: one 400k-token turn's events must never be able to
/// push a session past its own budget mid-turn.
pub(super) const SESSION_RETAINED_BYTES: u64 = 32 * 1024 * 1024;

const MANIFEST_FILE: &str = "manifest.json";

// ── manifest ─────────────────────────────────────────────────────────────────

/// One segment file's place in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentRef {
    pub ordinal: u32,
    pub first_seq: u64,
    /// `None` while the segment is still being appended to. A sealed segment
    /// ends on a completed turn, which is what makes it a safe unit to delete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_last_seq: Option<u64>,
}

impl SegmentRef {
    fn file_name(&self) -> String {
        format!("{:06}.jsonl", self.ordinal)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Manifest {
    header: SessionHeader,
    /// Every seq below this was covered by a retention base and its segments
    /// deleted. `0` for a log that has never been truncated.
    truncated_before: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_through_seq: Option<u64>,
    segments: Vec<SegmentRef>,
}

impl Manifest {
    fn active(&self) -> Option<&SegmentRef> {
        self.segments.last().filter(|s| s.sealed_last_seq.is_none())
    }
}

fn base_file_name(through_seq: u64) -> String {
    format!("base.{through_seq}.json")
}

// ── retention base ───────────────────────────────────────────────────────────

/// The authoritative starting point left behind when old segments are deleted.
///
/// Not a cache: once the manifest points at it and the segments it covers are
/// gone, deleting it puts a hole in history — which is why a reader that cannot
/// load it refuses the session rather than falling back to the retained tail.
///
/// It holds surviving events with their seq and content intact, so a reader
/// folds base-then-tail with one rule instead of two. Their seqs are below
/// `truncated_before` and are deliberately not contiguous: only what still
/// matters survives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionBase {
    /// Inclusive. Every event at or below this is accounted for by the base.
    pub through_seq: u64,
    /// In seq order: the conversation surface's message events, plus the latest
    /// `request/header` and `request/context`.
    pub events: Vec<SessionEvent>,
}

impl RetentionBase {
    /// What survives a cut at `through_seq`: the surface as it stands there,
    /// plus the envelope needed to reopen the session.
    ///
    /// `dense_from` is the log's current `truncated_before` — the events are
    /// folded to find the surface, and a log that was already cut once is
    /// already sparse below it.
    ///
    /// Surviving nodes are re-declared as `append`. A replacement is kept only
    /// because it is *on* the surface, and the nodes it shadowed are exactly
    /// what a cut drops — so replaying its `replace` would look for a `start`
    /// that no longer exists and refuse the whole log. Rewriting it to an
    /// append preserves the surface exactly and loses only a citation whose
    /// targets are gone.
    pub fn cut(events: &[SessionEvent], through_seq: u64, dense_from: u64) -> Result<Self> {
        use komo_core::domain::session_event::fold_surface;

        let surface = fold_surface(events, dense_from)?;
        let live: std::collections::HashSet<u64> = surface.nodes().iter().copied().collect();
        let mut kept: Vec<SessionEvent> = events
            .iter()
            .filter(|e| e.seq <= through_seq && live.contains(&e.seq))
            .cloned()
            .map(as_append)
            .collect();

        // The envelope: without it a resumed turn would re-assemble a *different*
        // request. Same "latest at or below the cut" rule the folds read it by.
        // The conversation boundary rides along for the same reason: it is not a
        // surface node, so dropping it would quietly put everything the operator
        // drew a line under back in front of the model.
        let covered = |e: &&SessionEvent| e.seq <= through_seq;
        for latest in [
            events
                .iter()
                .filter(covered)
                .rfind(|e| matches!(e.kind, SessionEventKind::RequestHeader(_))),
            events
                .iter()
                .filter(covered)
                .rfind(|e| matches!(e.kind, SessionEventKind::RequestContext(_))),
            events
                .iter()
                .filter(covered)
                .rfind(|e| matches!(e.kind, SessionEventKind::ConversationBoundary { .. })),
        ]
        .into_iter()
        .flatten()
        {
            if !live.contains(&latest.seq) {
                kept.push(latest.clone());
            }
        }
        kept.sort_by_key(|e| e.seq);
        Ok(Self {
            through_seq,
            events: kept,
        })
    }
}

/// Re-declare a surviving surface node as a plain append. See [`RetentionBase::cut`].
fn as_append(mut event: SessionEvent) -> SessionEvent {
    use komo_core::domain::session_event::SurfacePlacement;
    match &mut event.kind {
        SessionEventKind::UserMessage(message) => message.surface = SurfacePlacement::append(),
        SessionEventKind::AssistantMessage(message) => message.surface = SurfacePlacement::append(),
        _ => {}
    }
    event
}

// ── errors ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum LogError {
    /// The log cannot be read as a session — a hole, an unknown required event,
    /// a missing retention base.
    Corrupt(String),
    Fold(FoldError),
    Io(std::io::Error),
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Corrupt(why) => write!(f, "session log is unreadable: {why}"),
            Self::Fold(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "session log io failed: {error}"),
        }
    }
}

impl std::error::Error for LogError {}

impl From<std::io::Error> for LogError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<FoldError> for LogError {
    fn from(error: FoldError) -> Self {
        Self::Fold(error)
    }
}

type Result<T> = std::result::Result<T, LogError>;

// ── the log ──────────────────────────────────────────────────────────────────

struct LogState {
    manifest: Manifest,
    /// Assigned but not yet written to the active segment. Write-behind: a
    /// barrier turns these into bytes and fsyncs them.
    pending: Vec<SessionEvent>,
    next_seq: u64,
    /// Bytes already durable in the active segment.
    active_bytes: u64,
}

pub struct SessionLog {
    dir: PathBuf,
    state: Mutex<LogState>,
}

impl SessionLog {
    /// Open a session's log, materializing it from `header` when it does not
    /// exist yet. First materialization commits the manifest and an empty first
    /// segment together.
    pub async fn open_or_create(dir: PathBuf, header: SessionHeader) -> Result<Self> {
        let manifest_path = dir.join(MANIFEST_FILE);
        if !manifest_path.exists() {
            tokio::fs::create_dir_all(&dir).await?;
            let manifest = Manifest {
                header,
                truncated_before: 0,
                base_through_seq: None,
                segments: vec![SegmentRef {
                    ordinal: 0,
                    first_seq: 0,
                    sealed_last_seq: None,
                }],
            };
            // The segment exists before the manifest names it, so a crash
            // between the two leaves an unreferenced file, never a dangling
            // reference.
            tokio::fs::File::create(dir.join("000000.jsonl"))
                .await?
                .sync_all()
                .await?;
            write_atomic(
                &manifest_path,
                &serde_json::to_vec_pretty(&manifest).unwrap(),
            )
            .await?;
            return Ok(Self {
                dir,
                state: Mutex::new(LogState {
                    manifest,
                    pending: Vec::new(),
                    next_seq: 0,
                    active_bytes: 0,
                }),
            });
        }

        let manifest: Manifest = serde_json::from_slice(&tokio::fs::read(&manifest_path).await?)
            .map_err(|e| LogError::Corrupt(format!("manifest.json: {e}")))?;
        if manifest.header.format_version > SESSION_EVENT_VERSION {
            return Err(LogError::Corrupt(format!(
                "written by a newer komo (format {}, this build reads {SESSION_EVENT_VERSION}) \
                 — upgrade komo to open this session",
                manifest.header.format_version
            )));
        }
        // The active segment's last whole record decides where writing resumes;
        // the manifest deliberately does not track it, so it need not be
        // rewritten per append.
        let (next_seq, active_bytes) = match manifest.active() {
            Some(segment) => {
                let path = dir.join(segment.file_name());
                let (events, bytes) = read_segment(&path).await?;
                let next = events
                    .last()
                    .map(|e| e.seq + 1)
                    .unwrap_or(segment.first_seq);
                (next, bytes)
            }
            None => (
                manifest
                    .segments
                    .last()
                    .and_then(|s| s.sealed_last_seq)
                    .map(|s| s + 1)
                    .unwrap_or(manifest.truncated_before),
                0,
            ),
        };
        Ok(Self {
            dir,
            state: Mutex::new(LogState {
                manifest,
                pending: Vec::new(),
                next_seq,
                active_bytes,
            }),
        })
    }

    pub async fn header(&self) -> SessionHeader {
        self.state.lock().await.manifest.header.clone()
    }

    /// The seq at which this log is contiguous. Everything below it was
    /// truncated and is represented by the retention base, whose surviving
    /// events are deliberately sparse — pass this to the folds, which is how
    /// they tell a truncation from a hole.
    pub async fn truncated_before(&self) -> u64 {
        self.state.lock().await.manifest.truncated_before
    }

    /// Assign and buffer a batch of events, returning them as stamped.
    ///
    /// Buffered, not written: a caller that needs these bytes to have survived a
    /// crash calls [`durable_flush`](Self::durable_flush) before the effect they
    /// describe.
    pub async fn append_batch(&self, kinds: Vec<SessionEventKind>) -> Vec<SessionEvent> {
        let mut state = self.state.lock().await;
        let mut appended = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let seq = state.next_seq;
            state.next_seq += 1;
            let event = SessionEvent::now(seq, kind);
            appended.push(event.clone());
            state.pending.push(event);
        }
        appended
    }

    /// Write everything buffered to the active segment and `fsync` it.
    pub async fn durable_flush(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.pending.is_empty() {
            return Ok(());
        }
        let Some(segment) = state.manifest.active().cloned() else {
            return Err(LogError::Corrupt(
                "no active segment to append to".to_string(),
            ));
        };
        let mut buf = Vec::new();
        for event in &state.pending {
            serde_json::to_writer(&mut buf, event).map_err(|e| {
                LogError::Corrupt(format!("event {} will not serialize: {e}", event.seq))
            })?;
            buf.push(b'\n');
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(segment.file_name()))
            .await?;
        file.write_all(&buf).await?;
        // Not `flush`: a userspace buffer that reached the kernel is not a fact
        // that survived the machine.
        file.sync_all().await?;
        state.active_bytes += buf.len() as u64;
        state.pending.clear();
        Ok(())
    }

    /// Every event from `seq` forward, base included when it covers `seq`.
    pub async fn read_from(&self, seq: u64) -> Result<Vec<SessionEvent>> {
        let state = self.state.lock().await;
        let manifest = state.manifest.clone();
        drop(state);

        let mut out = Vec::new();
        if let Some(through) = manifest.base_through_seq {
            let path = self.dir.join(base_file_name(through));
            let bytes = tokio::fs::read(&path).await.map_err(|e| {
                // The base is authoritative once the manifest points at it:
                // losing it is a hole, not a reason to serve the tail alone.
                LogError::Corrupt(format!(
                    "retention base {} is missing or unreadable ({e}); the session's history \
                     below seq {through} cannot be reconstructed",
                    path.display()
                ))
            })?;
            let base: RetentionBase = serde_json::from_slice(&bytes)
                .map_err(|e| LogError::Corrupt(format!("retention base: {e}")))?;
            out.extend(base.events.into_iter().filter(|e| e.seq >= seq));
        }
        for segment in &manifest.segments {
            if segment.sealed_last_seq.is_some_and(|last| last < seq) {
                continue;
            }
            let (events, _) = read_segment(&self.dir.join(segment.file_name())).await?;
            out.extend(events.into_iter().filter(|e| e.seq >= seq));
        }
        Ok(out)
    }

    /// The whole readable log: the base, then every retained event.
    pub async fn load(&self) -> Result<Vec<SessionEvent>> {
        self.read_from(0).await
    }

    /// Seal the active segment and open a new one — only if it has grown past
    /// [`SEGMENT_TARGET_BYTES`].
    ///
    /// Call at a completed-turn boundary and nowhere else: a segment is
    /// retention's unit of deletion, so cutting one inside a turn would make
    /// the recoverable unit unsplittable from the deletable one. One very large
    /// turn is allowed to overshoot rather than be cut.
    pub async fn seal_if_full(&self) -> Result<bool> {
        let mut state = self.state.lock().await;
        if !state.pending.is_empty() {
            return Err(LogError::Corrupt(
                "cannot seal a segment with unflushed events".to_string(),
            ));
        }
        if state.active_bytes < SEGMENT_TARGET_BYTES {
            return Ok(false);
        }
        let Some(active) = state.manifest.active().cloned() else {
            return Ok(false);
        };
        let next_seq = state.next_seq;
        let mut manifest = state.manifest.clone();
        if let Some(last) = manifest.segments.last_mut() {
            last.sealed_last_seq = Some(next_seq.saturating_sub(1));
        }
        let ordinal = active.ordinal + 1;
        manifest.segments.push(SegmentRef {
            ordinal,
            first_seq: next_seq,
            sealed_last_seq: None,
        });
        let path = self.dir.join(format!("{ordinal:06}.jsonl"));
        tokio::fs::File::create(&path).await?.sync_all().await?;
        write_atomic(
            &self.dir.join(MANIFEST_FILE),
            &serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .await?;
        state.manifest = manifest;
        state.active_bytes = 0;
        Ok(true)
    }

    /// Where this log may cut, or `None` for nothing to do.
    ///
    /// `budget` is the retained-bytes ceiling; under it there is nothing to cut.
    /// `keep_from` is the first seq that must survive intact — the caller's
    /// question, because the log does not know which turns are still resumable
    /// or still unlearned, and **space never outranks that**: a session sits
    /// over budget rather than drop a turn nobody has finished with.
    ///
    /// Answers the **oldest** sealed boundary below `keep_from`, so a log over
    /// budget sheds one segment each time it gains one and settles at the
    /// ceiling — rather than shedding everything sealed the moment it crosses.
    pub async fn retention_cut(&self, budget: u64, keep_from: u64) -> Result<Option<u64>> {
        Ok(self
            .retention_candidates(budget)
            .await?
            .iter()
            .filter_map(|segment| segment.sealed_last_seq)
            .filter(|last| *last < keep_from)
            .min())
    }

    /// Seal the active segment whatever its size, for tests that care about the
    /// boundary rather than about writing a megabyte to reach it.
    #[cfg(test)]
    pub(super) async fn seal_now(&self) -> Result<bool> {
        self.state.lock().await.active_bytes = SEGMENT_TARGET_BYTES;
        self.seal_if_full().await
    }

    /// Sealed segments, oldest first, when the log is over `budget`. Empty when
    /// it is not.
    async fn retention_candidates(&self, budget: u64) -> Result<Vec<SegmentRef>> {
        let state = self.state.lock().await;
        let sealed: Vec<SegmentRef> = state
            .manifest
            .segments
            .iter()
            .filter(|s| s.sealed_last_seq.is_some())
            .cloned()
            .collect();
        drop(state);

        let mut total = 0u64;
        for segment in &sealed {
            total += tokio::fs::metadata(self.dir.join(segment.file_name()))
                .await
                .map(|m| m.len())
                .unwrap_or(0);
        }
        Ok(if total > budget { sealed } else { Vec::new() })
    }

    /// Replace everything at or below `base.through_seq` with `base`.
    ///
    /// Committed in the one order that has no unreadable failure mode:
    ///
    /// 1. write and fsync the new base — a crash here leaves an unreferenced
    ///    file, and the log still reads exactly as before;
    /// 2. replace the manifest atomically — after this the base is authoritative
    ///    and the covered segments are no longer listed;
    /// 3. delete the covered segments and the old base — a crash here leaves
    ///    files nothing references, costing space and nothing else.
    pub async fn truncate(&self, base: RetentionBase) -> Result<()> {
        let mut state = self.state.lock().await;
        let mut manifest = state.manifest.clone();
        if manifest
            .active()
            .is_some_and(|a| a.first_seq <= base.through_seq)
        {
            return Err(LogError::Corrupt(
                "refusing to truncate through the active segment".to_string(),
            ));
        }
        let (covered, kept): (Vec<SegmentRef>, Vec<SegmentRef>) =
            manifest.segments.iter().cloned().partition(|s| {
                s.sealed_last_seq
                    .is_some_and(|last| last <= base.through_seq)
            });
        if covered.is_empty() {
            return Ok(());
        }
        let old_base = manifest.base_through_seq;

        // 1. base first.
        write_atomic(
            &self.dir.join(base_file_name(base.through_seq)),
            &serde_json::to_vec(&base).map_err(|e| {
                LogError::Corrupt(format!("retention base will not serialize: {e}"))
            })?,
        )
        .await?;

        // 2. then the manifest — the moment the new base becomes authoritative.
        manifest.truncated_before = base.through_seq + 1;
        manifest.base_through_seq = Some(base.through_seq);
        manifest.segments = kept;
        write_atomic(
            &self.dir.join(MANIFEST_FILE),
            &serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .await?;
        state.manifest = manifest;
        drop(state);

        // 3. only then the bytes nothing points at any more.
        for segment in covered {
            if let Err(error) = tokio::fs::remove_file(self.dir.join(segment.file_name())).await {
                warn!(%error, ordinal = segment.ordinal, "could not delete a truncated segment");
            }
        }
        if let Some(through) = old_base.filter(|t| *t != base.through_seq)
            && let Err(error) = tokio::fs::remove_file(self.dir.join(base_file_name(through))).await
        {
            warn!(%error, "could not delete the superseded retention base");
        }
        Ok(())
    }
}

// ── files ────────────────────────────────────────────────────────────────────

/// Replace a file's contents so a reader sees the whole old one or the whole
/// new one: temp file → fsync → rename → fsync the directory. Without the
/// directory fsync the rename itself can be lost.
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = tokio::fs::File::create(&tmp).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await?;
    if let Some(dir) = path.parent() {
        tokio::fs::File::open(dir).await?.sync_all().await?;
    }
    Ok(())
}

/// Read one segment: every whole record, plus the file's size.
///
/// A final line with no terminator is a process killed mid-append — dropped,
/// because a half-written record is not a record. Anything else that will not
/// read is a hole, and a hole is refused.
async fn read_segment(path: &Path) -> Result<(Vec<SessionEvent>, u64)> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(LogError::Corrupt(format!(
                "segment {} is listed in the manifest but missing",
                path.display()
            )));
        }
        Err(error) => return Err(error.into()),
    };
    let bytes = text.len() as u64;
    let complete = text.rfind('\n').map(|i| &text[..=i]).unwrap_or("");
    if complete.len() != text.len() {
        warn!(
            path = %path.display(),
            dropped = text.len() - complete.len(),
            "dropped a torn trailing record from a session segment"
        );
    }
    let mut events = Vec::new();
    for line in complete.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(event) = decode_event(line)? {
            events.push(event);
        }
    }
    Ok((events, bytes))
}

#[cfg(test)]
mod tests;
