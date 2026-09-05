//! Durable inbound dedupe.
//!
//! Chat platforms deliver at-least-once. Telegram redelivers everything above
//! the last committed `getUpdates` offset, so a gateway that dies between
//! running a turn and advancing the offset sees the same message again on
//! restart. Feishu retries a message it thinks was not acked.
//!
//! Deduping in process memory does not survive that: the state that would have
//! recognised the redelivery died with the process. This is a durable record
//! instead — one row per inbound message, keyed by the platform's own id, so
//! "have I already handled this?" outlives a restart.
//!
//! The gate sits in front of *every* inbound message, not just the ones that
//! start a turn: a redelivered `/approve` would approve something twice, which
//! is worse than a duplicated question.
//!
//! The row outlives the claim on purpose. A message is `completed` only once
//! its work is finished — a chat command when it has been answered, a plain
//! message when its turn has settled — so a row still `claimed` at startup is a
//! message the process died owing an answer to, and the gateway re-delivers it
//! from here.

use async_trait::async_trait;

use crate::domain::session::InboundPeer;

/// The platform name [`InboundOrigin::local`] carries.
pub const LOCAL_PLATFORM: &str = "local";

/// What identifies an inbound message on the platform that delivered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundOrigin {
    /// Channel name — `feishu` / `telegram` / `wechat` / `local`.
    pub platform: String,
    /// The platform's own id for this message.
    pub message_id: String,
}

impl InboundOrigin {
    pub fn new(platform: impl Into<String>, message_id: impl Into<String>) -> Self {
        Self {
            platform: platform.into(),
            message_id: message_id.into(),
        }
    }

    /// An origin for input no platform delivered — a test, or a local caller
    /// that owns its own retry story. Nothing can redeliver it, so every call
    /// mints a fresh id and every claim is [`InboxClaim::Fresh`].
    pub fn local() -> Self {
        Self::new(LOCAL_PLATFORM, uuid::Uuid::now_v7().to_string())
    }

    /// Whether this is such an origin. Startup recovery asks: nothing can
    /// redeliver a local message and there is no channel to answer it on, so a
    /// row left behind by one is closed rather than run again.
    pub fn is_local(&self) -> bool {
        self.platform == LOCAL_PLATFORM
    }

    /// The storage key. Deterministic rather than a fresh UUIDv7 (the
    /// convention for every other key in state.db) precisely because dedupe
    /// *wants* the collision: the same platform message has to land on the
    /// same row, and the primary key is what makes that atomic.
    pub fn key(&self) -> String {
        format!("{}:{}", self.platform, self.message_id)
    }
}

/// Whether this delivery is the first one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxClaim {
    /// Not seen before — the caller owns it and should handle it.
    Fresh,
    /// Already recorded by an earlier delivery. Drop it.
    Duplicate,
}

/// A claimed message nobody finished — one row of [`InboxRepository::unfinished`].
///
/// Everything `GatewayDispatcher::handle` was given, so recovery can run the
/// message again through the same path the channel used: the peer decides which
/// conversation it is and how it is authorized, the text is what to run, and
/// `claimed_at` is what tells a transcript search whether the turn ever began.
#[derive(Debug, Clone)]
pub struct UnfinishedInbound {
    pub origin: InboundOrigin,
    pub session_id: String,
    pub text: String,
    pub peer: InboundPeer,
    pub claimed_at: i64,
}

#[async_trait]
pub trait InboxRepository: Send + Sync {
    /// Record an inbound message and report whether it is new.
    ///
    /// `peer` and `text` are stored alongside so a claimed-but-unfinished row
    /// carries everything the message arrived with: a crash between the claim
    /// and the turn's first durable event leaves a row
    /// [`unfinished`](Self::unfinished) hands back, and re-dispatching it needs
    /// the correspondent as much as the payload.
    async fn claim(
        &self,
        origin: &InboundOrigin,
        peer: &InboundPeer,
        session_id: &str,
        text: &str,
    ) -> anyhow::Result<InboxClaim>;

    /// Mark a claimed message as handled.
    ///
    /// "Handled" means the message's work is finished, or a turn owns it in the
    /// ledger: a chat command completes as soon as it has been answered, and a
    /// plain message completes when its turn settles — not when it was
    /// dispatched. A message queued behind a busy session therefore stays
    /// `claimed` until its own turn has run.
    async fn complete(&self, origin: &InboundOrigin) -> anyhow::Result<()>;

    /// Claimed rows nobody completed, oldest first — what the gateway scans at
    /// startup to re-deliver whatever a crash swallowed. Bounded, because a
    /// restart must not stall behind an unbounded backlog.
    async fn unfinished(&self, limit: usize) -> anyhow::Result<Vec<UnfinishedInbound>>;
}
