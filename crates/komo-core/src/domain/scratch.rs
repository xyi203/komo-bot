//! Durable working state one tool call keeps across a suspension —
//! `scratch_records` in `komo.db`, and the [`TurnScratch`] trait over them.
//!
//! A call that stopped to wait **did not happen**: the executor leaves it
//! unsettled and the continuation re-dispatches it, so it starts again from
//! zero (`rebuild_from_events`, docs/bot-runtime.md §4.1). That is exactly
//! right for the call as an *effect* — it never ran — and exactly wrong for
//! the work it had already done before it stopped. A program that made three
//! sub-calls and then hit an approval on the fourth must not make the first
//! three again, and nothing in the loop remembers them: the process may not
//! even be the same one.
//!
//! The event log cannot serve as that memory. It records what happened, but
//! `tool/call-settled`'s `result` is truncated to `STEP_FIELD_CAP` (2,000
//! chars) because it is an audit field the model never replays, while a
//! sub-call's real result can be 16 KiB. Reading progress back out of it would
//! hand the continuation a prefix of its own work and no way to tell that from
//! the whole of it.
//!
//! Keyed by the **root** of the attempt chain, never by the running turn. A
//! continuation is a new turn with a new id linked back by `resumed_from`, so
//! keying on the current one would file every attempt's scratch under a
//! different name and the second attempt would read nothing. The approval gate
//! bounds its answers to [`attempt_chain`](crate::domain::session_event::attempt_chain)
//! for the same reason: what the chain did is one turn's worth of facts, told
//! across several ids.
//!
//! Disposable **by row**: the executor clears a call's keys when the call
//! settles (not when it suspends — that is precisely when the continuation
//! needs them), and a deleted session takes its rows with it.

use async_trait::async_trait;

/// One scratch entry's address: which call, on which attempt chain, wrote it.
///
/// Carries the session too, so two conversations that happen to mint the same
/// call id never read each other's work.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScratchKey {
    pub session_id: String,
    /// The **first** turn of the attempt chain — stable across every
    /// continuation, unlike the turn actually running.
    pub root_turn_id: String,
    pub call_id: String,
    pub key: String,
}

/// The store behind [`ToolContext::scratch_get`](crate::domain::context::ToolContext::scratch_get)
/// / [`scratch_set`](crate::domain::context::ToolContext::scratch_set).
#[async_trait]
pub trait TurnScratch: Send + Sync {
    async fn get(&self, key: &ScratchKey) -> anyhow::Result<Option<String>>;

    /// Write `value` under `key`, replacing whatever was there — a call
    /// re-reaching the same step of its own work is restating it, not adding a
    /// second one.
    async fn set(&self, key: &ScratchKey, value: &str) -> anyhow::Result<()>;

    /// Drop every key a call wrote. Called when the call **settles**, never
    /// when it suspends: a suspended call's scratch is what its continuation
    /// comes back for.
    async fn clear(
        &self,
        session_id: &str,
        root_turn_id: &str,
        call_id: &str,
    ) -> anyhow::Result<()>;
}
