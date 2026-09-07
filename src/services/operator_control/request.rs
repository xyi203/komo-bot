//! Typed operator requests and replies.
//!
//! The whole operator surface is these two request enums and their two reply
//! enums, serialized over `POST /api/operator` — one endpoint, not a route per
//! action. They derive `Serialize`/`Deserialize` because they *are* the wire
//! format: the CLI names a variant, the gateway matches it against the same
//! definition, so a shape can never drift between the two sides.

use serde::{Deserialize, Serialize};

use crate::domain::{
    cron::{CronJob, CronJobSpec},
    memory::Memory,
    run::{Run, RunStep},
};

// The pure view DTOs (no domain dependency) live in `komo-core` so HTTP clients
// — the CLI gateway adapter and the Dioxus GUI — share one definition. Re-export
// them here so `operator_control::{SessionSummary, …}` paths are unchanged.
pub use komo_core::operator_view::{
    DreamItem, DreamReport, PairingView, SessionSummary, WikiHitView, WikiIndexView, WikiStatusView,
};

/// A read-only operator request. One `query` call per CLI render.
#[derive(Debug, Serialize, Deserialize)]
pub enum OperatorQuery {
    /// Recent runs, newest first.
    Runs { limit: usize },
    /// One run with its tool steps (`None` = no such run).
    Run { id: String },
    /// Session summaries (never full transcripts).
    Sessions,
    /// The whole memory library (operator view — no scope enforcement).
    Memories,
    /// Ranked memory search over the same hybrid query recall uses. Routed like
    /// every other operator read so a running gateway lends its embedder;
    /// without one the same scoring runs lexical-only.
    MemorySearch { query: String, limit: usize },
    /// Which turns a memory reached the prompt of.
    /// Hash-free pairing rows.
    Pairings,
    /// The dreaming dry-run classification.
    DreamPreview,
    /// The `/sethome` runtime override (`None` when unset).
    HomeOverride,
    /// Note-vault search. Routed like every other operator read so it works
    /// while the gateway holds the index open.
    WikiSearch { query: String, limit: usize },
    /// What the note-vault index currently holds.
    WikiStatus,
    /// Every scheduled cron job (enabled or not), by name.
    CronJobs,
}

/// The reply to an [`OperatorQuery`], variant-for-variant. Callers match
/// exhaustively — transport JSON shapes never become the caller interface.
#[derive(Debug, Serialize, Deserialize)]
pub enum OperatorQueryResult {
    Runs(Vec<Run>),
    Run(Option<(Run, Vec<RunStep>)>),
    Sessions(Vec<SessionSummary>),
    Memories(Vec<Memory>),
    MemorySearch(Vec<Memory>),
    Pairings(Vec<PairingView>),
    DreamPreview(DreamReport),
    HomeOverride(Option<String>),
    CronJobs(Vec<CronJob>),
    WikiHits(Vec<WikiHitView>),
    WikiStatus(WikiStatusView),
}

/// A state-changing operator action (host-operator writes; the gateway serves
/// these only to loopback callers).
#[derive(Debug, Serialize, Deserialize)]
pub enum OperatorCommand {
    /// Apply one memory governance transition.
    MemoryTransition {
        id: String,
        action: MemoryTransitionAction,
    },
    /// Drop runs (and their steps) started before `cutoff`.
    PruneRuns { cutoff: i64 },
    /// Delete every session with no messages.
    CleanSessions,
    /// Approve the pending pairing bearing `code`.
    PairApprove { code: String },
    /// Remove a pairing by id.
    PairRevoke { id: String },
    /// Run one dreaming consolidation cycle.
    DreamApply,
    /// Widen memories stranded in an ephemeral `api` channel scope to `Global`.
    MemoryRepairScopes,
    /// Embed every memory that still lacks a current vector, and wait for it.
    /// Minutes-long on a library that has never been embedded: the gateway
    /// adapter gives this the same long timeout `ChunkIndex` gets.
    MemoryBackfill,
    /// Index the note vault. Minutes-long: the gateway adapter gives this
    /// command its own, far longer timeout than every other operator call.
    ChunkIndex { rebuild: bool },
    /// Create a scheduled cron job (validated; duplicate names refused).
    CronAdd { spec: CronJobSpec },
    /// Delete a cron job by name.
    CronRemove { name: String },
    /// Enable or disable a cron job. Re-enabling recomputes `next_run_at`
    /// from now, so a long-disabled job doesn't fire immediately off its
    /// stale slot.
    CronSetEnabled { name: String, enabled: bool },
    /// Make a job due now — it fires on the gateway's next sweep tick
    /// (within a minute). With no gateway running, it fires once one starts.
    CronTrigger { name: String },
}

/// The reply to an [`OperatorCommand`], variant-for-variant.
#[derive(Debug, Serialize, Deserialize)]
pub enum OperatorCommandResult {
    /// The transition applied (an unknown id is an `Err`, identical on both
    /// transports).
    MemoryTransitioned,
    WikiIndexed(WikiIndexView),
    RunsPruned {
        removed: usize,
    },
    SessionsCleaned {
        removed: usize,
    },
    PairApproved(PairApproveOutcome),
    PairRevoked {
        revoked: bool,
    },
    DreamApplied {
        promoted: usize,
        archived: usize,
    },
    /// How many memories were widened to `Global`.
    MemoryScopesRepaired {
        repaired: usize,
    },
    /// How many memories gained an embedding.
    MemoryBackfilled {
        embedded: usize,
    },
    /// The created job (with its computed `next_run_at`).
    CronAdded(Box<CronJob>),
    CronRemoved,
    /// The job after an enable/disable/trigger update.
    CronUpdated(Box<CronJob>),
}

/// A memory governance transition. The domain owns the semantics
/// (`Memory::promote/reject/pin`); this only names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryTransitionAction {
    Promote,
    Reject,
    Pin,
}

impl MemoryTransitionAction {
    /// The domain method this action names.
    pub fn apply(self) -> fn(&mut Memory, i64) {
        match self {
            MemoryTransitionAction::Promote => Memory::promote,
            MemoryTransitionAction::Reject => Memory::reject,
            MemoryTransitionAction::Pin => Memory::pin,
        }
    }
}

/// The outcome of a pairing approval.
#[derive(Debug, Serialize, Deserialize)]
pub enum PairApproveOutcome {
    Approved { id: String },
    NotFound,
    Locked { retry_after_secs: i64 },
}

/// One operator call as it travels over `POST /api/operator`, and its reply.
///
/// The pair exists so *one* route serves the whole surface: the handler matches
/// this, dispatches to [`super::actions`], and answers with the matching reply
/// arm. A caller that asked a query and got a command reply has hit a version
/// skew, which is an error rather than a shape to tolerate.
#[derive(Debug, Serialize, Deserialize)]
pub enum OperatorRequest {
    Query(OperatorQuery),
    Command(OperatorCommand),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum OperatorReply {
    Query(OperatorQueryResult),
    Command(OperatorCommandResult),
}

impl OperatorRequest {
    /// Whether this call is minutes-long by nature (vault indexing, embedding
    /// a never-embedded memory library) and so must ride the client's
    /// unbounded HTTP client — the server, not the client, bounds it.
    pub fn is_long_running(&self) -> bool {
        matches!(
            self,
            OperatorRequest::Command(
                OperatorCommand::ChunkIndex { .. } | OperatorCommand::MemoryBackfill
            )
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These enums *are* the wire format, so a variant that cannot round-trip
    /// is a command that silently stops working — a `#[serde(skip)]` on a
    /// payload field, a tuple shape serde won't read back. One assertion per
    /// shape the surface uses: a unit variant, a struct variant, a payload
    /// carrying a domain type, and a boxed one.
    #[track_caller]
    fn round_trips_request(request: OperatorRequest) {
        let json = serde_json::to_string(&request).expect("serializes");
        serde_json::from_str::<OperatorRequest>(&json).expect("deserializes");
    }

    #[track_caller]
    fn round_trips_reply(reply: OperatorReply) {
        let json = serde_json::to_string(&reply).expect("serializes");
        serde_json::from_str::<OperatorReply>(&json).expect("deserializes");
    }

    #[test]
    fn every_request_shape_round_trips() {
        round_trips_request(OperatorRequest::Query(OperatorQuery::Sessions));
        round_trips_request(OperatorRequest::Query(OperatorQuery::Runs { limit: 20 }));
        round_trips_request(OperatorRequest::Query(OperatorQuery::MemorySearch {
            query: "智能插座".into(),
            limit: 5,
        }));
        round_trips_request(OperatorRequest::Command(OperatorCommand::CleanSessions));
        round_trips_request(OperatorRequest::Command(
            OperatorCommand::MemoryTransition {
                id: "m1".into(),
                action: MemoryTransitionAction::Promote,
            },
        ));
    }

    #[test]
    fn every_reply_shape_round_trips() {
        round_trips_reply(OperatorReply::Query(OperatorQueryResult::Runs(Vec::new())));
        round_trips_reply(OperatorReply::Query(OperatorQueryResult::Run(None)));
        round_trips_reply(OperatorReply::Query(OperatorQueryResult::HomeOverride(
            Some("feishu:oc_x".into()),
        )));
        round_trips_reply(OperatorReply::Command(
            OperatorCommandResult::MemoryTransitioned,
        ));
        round_trips_reply(OperatorReply::Command(OperatorCommandResult::PairApproved(
            PairApproveOutcome::Locked {
                retry_after_secs: 30,
            },
        )));
    }

    /// Only the two calls whose duration the *server* bounds may ride the
    /// client's unbounded HTTP client; everything else must keep its timeout,
    /// or a wedged gateway hangs the CLI forever.
    #[test]
    fn only_indexing_and_backfill_are_long_running() {
        assert!(
            OperatorRequest::Command(OperatorCommand::ChunkIndex { rebuild: true })
                .is_long_running()
        );
        assert!(OperatorRequest::Command(OperatorCommand::MemoryBackfill).is_long_running());
        assert!(!OperatorRequest::Command(OperatorCommand::CleanSessions).is_long_running());
        assert!(!OperatorRequest::Query(OperatorQuery::Sessions).is_long_running());
    }
}
