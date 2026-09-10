//! Cooperative turn cancellation.
//!
//! A turn started over the api channel runs on a spawned task, so a client
//! hanging up doesn't stop it — the agent keeps going and its reply lands in the
//! transcript with nobody watching. This is the signal that lets the caller stop
//! it for real.
//!
//! **Cooperative**, in three senses worth being precise about:
//!
//!   - The agent loop stops between rounds and mid-await (the model round-trip
//!     and the tool round are raced against the signal), so cancelling lands
//!     within one await, not after the whole turn.
//!   - A tool call already executing stops only if it **claims** the signal via
//!     [`ToolContext::cancelled`](crate::domain::context::ToolContext::cancelled).
//!     `shell` does (it kills its process group, so interrupting a ten-minute
//!     build actually ends the build) and so do `web_fetch` / `web_search` (they
//!     drop the request). Everything else runs to completion and observes the
//!     cancellation only after returning.
//!   - The executor deliberately does **not** race every call against the signal
//!     on the tools' behalf. That would also interrupt the filesystem tools,
//!     and there is nothing to gain by interrupting a millisecond-long local
//!     write: one `tokio::fs::write` is one `spawn_blocking`, so the syscall
//!     completes regardless of when the signal arrives.
//!
//! Like [`ToolEventSink`](crate::domain::events::ToolEventSink), this is a trait
//! so the domain stays runtime-agnostic; the `watch`-channel implementation
//! lives with the api channel's interaction state.

use async_trait::async_trait;

/// What a cancelled turn answers with. Persisted as the assistant message so the
/// transcript stays user/assistant-alternating and re-reading the session shows
/// why it stopped, rather than a question with no reply.
pub const CANCELLED_REPLY: &str = "（已中断）";

/// Ledger `error` for a run the user cancelled. Distinct from
/// [`INTERRUPTED_ERROR`](crate::domain::run::INTERRUPTED_ERROR), which marks
/// crash residue and is resumable — a deliberate cancel is not.
pub const CANCELLED_ERROR: &str = "cancelled by user";

/// The error a cancelled turn fails with, so every layer can tell a cancel from
/// a genuine failure by downcasting.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(CANCELLED_ERROR)
    }
}

impl std::error::Error for Cancelled {}

/// Is this error a turn cancellation?
pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Cancelled>().is_some()
}

/// One turn's cancellation signal.
#[async_trait]
pub trait CancelSignal: Send + Sync {
    /// Cheap check, for the point between rounds.
    fn is_cancelled(&self) -> bool;
    /// Resolves once cancelled. Must never resolve otherwise — it is raced
    /// against real work in a `select!`, so a spurious wake would abort a
    /// perfectly good turn.
    async fn cancelled(&self);
}
