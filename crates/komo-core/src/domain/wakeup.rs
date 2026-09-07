//! Wakeup registrations: the scheduler's half of a suspended turn
//! (docs/bot-runtime.md §3.2).
//!
//! A turn that stops to wait writes `turn/suspended` into its session log —
//! that is the authority on *what the turn is doing*. This is the authority on
//! *when to come back for it*: one durable row saying
//!
//! > when X happens, on session Z, either continue turn Y or start a new one.
//!
//! Two records rather than one because they answer different questions and are
//! read by different things: the log is read per session, on the turn's own
//! path; registrations are read every sweep tick, across all sessions, by a
//! scheduler that must not open a session artifact per row to find out whether
//! it has anything to do. They are kept honest by checking the log at fire time
//! — a registration whose turn is no longer waiting is dropped, never fired.
//!
//! **Nothing expires silently.** Every variant carries a default lifetime
//! ([`default_expiry_secs`]), and reaching it fires the wake with
//! [`WakeupCause::Expired`] rather than deleting the row: a question nobody
//! answered has to reach the turn that asked it, or the turn waits forever for
//! an answer that is never coming.

use super::policy::RuleSpec;
use super::session_event::{Wakeup, WakeupCause};

/// How long each kind of wait may stand before it fires as expired.
///
/// Chosen by what the waiting is *for*: an approval is a person being asked to
/// look at something now (a day), a question can wait out a weekend (a week),
/// and an outside event may never come at all.
pub fn default_expiry_secs(wakeup: &Wakeup) -> Option<i64> {
    match wakeup {
        Wakeup::Approval { .. } => Some(24 * 3_600),
        Wakeup::UserReply => Some(7 * 86_400),
        Wakeup::Event { .. } => Some(30 * 86_400),
    }
}

/// What a caller holding a connection is told when the turn it asked for
/// stopped to wait. The turn is not over — it continues when the answer
/// arrives, and its reply lands in the transcript.
pub const SUSPENDED_REPLY: &str = "（正在等待，收到后继续）";

/// The error a suspended turn stops with, so every layer tells a wait apart
/// from a failure by downcasting — the same shape as
/// [`Cancelled`](crate::domain::cancel::Cancelled).
///
/// It is not a failure and not a cancel: nothing went wrong, and the turn is
/// coming back. What it is waiting for rides on the turn's
/// `PendingSuspension`, not on the error.
#[derive(Debug, Clone, Copy, Default)]
pub struct Suspended;

impl std::fmt::Display for Suspended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("suspended, waiting")
    }
}

impl std::error::Error for Suspended {}

/// Is this error a turn that stopped to wait?
pub fn is_suspended(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Suspended>().is_some()
}

/// What every registration id starts with. Named because a chat command has to
/// tell an id from free text (`/deny wk-0199… too risky` vs `/deny too risky`),
/// and nothing else in that position looks like this.
pub const WAIT_ID_PREFIX: &str = "wk-";

/// One standing instruction to wake something up.
#[derive(Debug, Clone)]
pub struct WakeupRegistration {
    pub id: String,
    pub session_id: String,
    /// The suspended turn to continue, or `None` to start a fresh turn with
    /// whatever the wake carries.
    ///
    /// The difference between "pick up where you left off" and "here is
    /// something you were waiting to hear about" — a turn that already ended
    /// cannot be continued, but what it was waiting for still has to arrive.
    pub turn_id: Option<String>,
    pub wakeup: Wakeup,
    pub expires_at: Option<i64>,
    /// What the woken turn may do unattended, inherited from the turn that
    /// suspended. A routine's grants have to survive the wait, or a job that
    /// stopped to check back in two hours comes back unable to act.
    pub grants: Vec<RuleSpec>,
    pub created_at: i64,
}

impl WakeupRegistration {
    /// Register a wait on `session_id`, with its variant's default lifetime.
    pub fn new(session_id: impl Into<String>, wakeup: Wakeup, now: i64) -> Self {
        let expires_at = default_expiry_secs(&wakeup).map(|secs| now + secs);
        Self {
            id: format!("{WAIT_ID_PREFIX}{}", uuid::Uuid::now_v7()),
            session_id: session_id.into(),
            turn_id: None,
            wakeup,
            expires_at,
            grants: Vec::new(),
            created_at: now,
        }
    }

    /// Point it at the suspended turn it continues.
    pub fn continuing(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    /// Carry the suspending turn's unattended grants across the wait.
    pub fn with_grants(mut self, grants: Vec<RuleSpec>) -> Self {
        self.grants = grants;
        self
    }

    /// Override the default lifetime — a commitment waiting on a reply expires
    /// with its own `due_at`, not with the generic 30 days.
    pub fn expiring_at(mut self, at: Option<i64>) -> Self {
        self.expires_at = at;
        self
    }

    /// Whether the sweep should fire this now, and why.
    ///
    /// Only one clock-driven answer lives here: a wait that ran out. Every
    /// variant is fired by the thing that happened — an answer, a message, an
    /// event — and the sweep's job is the case where that never came.
    pub fn due_cause(&self, now: i64) -> Option<WakeupCause> {
        match self.expires_at {
            Some(at) if at <= now => Some(WakeupCause::Expired),
            _ => None,
        }
    }
}

/// Who knows how to actually wake something up.
///
/// The sweep owns *when* (the clock, the claim, and checking the log agrees the
/// turn is still waiting); this owns *what happens next* — continue the
/// suspended turn, or start a fresh one carrying what the wake brought. Split
/// because the scheduler lives in the daemon and the turn lives in the runtime,
/// and neither should have to know the other's shape.
#[async_trait::async_trait]
pub trait WakeupDispatch: Send + Sync {
    /// Wake it, carrying whatever the thing that happened brought with it — a
    /// finished task's result, a webhook's body. Empty for a wake that is only
    /// a clock going off, and the caller then has nothing to say beyond "it is
    /// time".
    async fn fire(
        &self,
        registration: &WakeupRegistration,
        cause: WakeupCause,
        payload: &str,
    ) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
pub trait WakeupRepository: Send + Sync {
    async fn save(&self, registration: &WakeupRegistration) -> anyhow::Result<()>;
    /// Every standing registration, oldest first.
    async fn list(&self) -> anyhow::Result<Vec<WakeupRegistration>>;
    /// Retire one. Answers `false` when it was already gone — which is what
    /// makes it a **claim**: two sweeps racing the same registration, or a
    /// sweep racing an arriving `/approve`, and only one of them fires it.
    async fn take(&self, id: &str) -> anyhow::Result<bool>;
    /// Retire every registration for a session's turn, whatever fired first.
    /// Answers how many were dropped.
    async fn take_for_turn(&self, session_id: &str, turn_id: &str) -> anyhow::Result<usize>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The waits nobody may forget. An approval that nobody answers is not a
    /// registration to quietly delete — it is a turn parked forever unless the
    /// clock brings it back and tells it so.
    #[test]
    fn every_wait_that_can_go_unanswered_expires_rather_than_hanging() {
        let now = 1_000;
        for wakeup in [
            Wakeup::Approval {
                call_id: "c1".into(),
            },
            Wakeup::UserReply,
            Wakeup::Event {
                filter: super::super::session_event::EventFilter::Webhook { name: "ci".into() },
            },
        ] {
            let r = WakeupRegistration::new("s1", wakeup.clone(), now);
            let expires = r.expires_at.expect("every answerable wait has a deadline");
            assert!(expires > now);
            assert_eq!(r.due_cause(expires - 1), None);
            assert_eq!(
                r.due_cause(expires),
                Some(WakeupCause::Expired),
                "{wakeup:?} must come back and say nobody answered"
            );
        }
    }

    /// A caller that names its own deadline — a commitment's `due_at` — keeps
    /// it, and a wait with none never comes due on the clock at all.
    #[test]
    fn an_overridden_deadline_is_the_only_clock_that_applies() {
        let now = 1_000;
        let r = WakeupRegistration::new("s1", Wakeup::UserReply, now).expiring_at(Some(now + 60));
        assert_eq!(r.due_cause(now + 59), None);
        assert_eq!(r.due_cause(now + 60), Some(WakeupCause::Expired));

        let standing = WakeupRegistration::new("s1", Wakeup::UserReply, now).expiring_at(None);
        assert_eq!(standing.due_cause(now + 10 * 86_400), None);
    }
}
