//! Operator control: one module owns how host-operator actions (list/inspect
//! reads, governance and maintenance writes) reach komo's state.
//!
//! Turso's exclusive cross-process lock means a running gateway is the sole
//! owner of the dbs — so every operator action has two transports: routed to
//! the gateway over its loopback api channel, or executed in-process against
//! directly-opened stores. [`OperatorControl`] hides that choice: CLI callers
//! issue one typed [`OperatorQuery`]/[`OperatorCommand`] and never probe the
//! gateway, pick a db, or translate transport payloads themselves.
//!
//! The two adapters may differ only in transport, auth, and connection
//! ownership — the business result comes from the shared projections and
//! transitions in [`actions`], which the gateway's HTTP handlers call too.

pub mod actions;
mod direct;
mod gateway;
pub mod request;

pub use request::*;

use crate::infra::gateway_client::GatewayClient;
use komo_config::RuntimeConfig;

use direct::DirectOperatorAdapter;
use gateway::GatewayOperatorAdapter;

/// Where the store lives, for the direct adapter's lazy connection.
#[derive(Debug, Clone)]
pub struct StoreUrls {
    pub db: String,
    /// Note-vault config, when `[wiki]` declares a vault. Carried here so the
    /// direct adapter can open the index itself when no gateway is running —
    /// the same reason the db urls are here.
    pub wiki: Option<komo_config::WikiConfig>,
}

impl StoreUrls {
    pub fn from_config(runtime: &RuntimeConfig) -> Self {
        Self {
            db: runtime.db_url.clone(),
            wiki: runtime.wiki.clone(),
        }
    }
}

enum OperatorBackend {
    Gateway(GatewayOperatorAdapter),
    Direct(DirectOperatorAdapter),
}

/// The operator surface's single entry point. Resolve once per CLI command
/// (`connect` probes the gateway exactly once), then issue any number of
/// queries/commands against the same backend — a batch never re-probes or
/// reconnects per item.
pub struct OperatorControl {
    backend: OperatorBackend,
}

impl OperatorControl {
    /// Probe for a running gateway once: reachable → route over its loopback
    /// api channel; otherwise operate on the stores directly (opened lazily,
    /// only the ones a request actually needs).
    pub async fn connect(urls: StoreUrls) -> anyhow::Result<Self> {
        let backend = match GatewayClient::try_connect().await {
            Some(client) => OperatorBackend::Gateway(GatewayOperatorAdapter::new(client)),
            None => OperatorBackend::Direct(DirectOperatorAdapter::new(urls)),
        };
        Ok(Self { backend })
    }

    /// Whether actions route to a running gateway (status lines only — never
    /// branch behavior on this).
    pub fn via_gateway(&self) -> bool {
        matches!(self.backend, OperatorBackend::Gateway(_))
    }

    /// Run one read-only operator query.
    pub async fn query(&self, query: OperatorQuery) -> anyhow::Result<OperatorQueryResult> {
        match &self.backend {
            OperatorBackend::Gateway(gw) => gw.query(query).await,
            OperatorBackend::Direct(direct) => direct.query(query).await,
        }
    }

    /// Run one state-changing operator command.
    pub async fn command(&self, command: OperatorCommand) -> anyhow::Result<OperatorCommandResult> {
        match &self.backend {
            OperatorBackend::Gateway(gw) => gw.command(command).await,
            OperatorBackend::Direct(direct) => direct.command(command).await,
        }
    }
}

/// Unix seconds — the operator surface's one clock read per request.
pub(crate) fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
    //! Contract tests over the direct backend. The gateway backend is a thin
    //! mapping onto `GatewayClient` (its transport behaviors — stale
    //! rendezvous fallback, 404 version skew — are tested there); business
    //! results on both paths come from the same `actions` helpers, which these
    //! tests exercise end-to-end through the `OperatorControl` interface.

    use super::*;
    use crate::domain::memory::{Memory, MemoryKind, MemoryRepository, MemoryStatus};

    fn temp_urls(tag: &str) -> StoreUrls {
        let dir = std::env::temp_dir().join(format!("komo_opctl_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        StoreUrls {
            db: format!("turso:{}", dir.join("komo.db").display()),
            // These tests exercise the db-backed operations only.
            wiki: None,
        }
    }

    fn direct(urls: StoreUrls) -> OperatorControl {
        OperatorControl {
            backend: OperatorBackend::Direct(DirectOperatorAdapter::new(urls)),
        }
    }

    #[tokio::test]
    async fn queries_on_empty_stores_return_empty() {
        let control = direct(temp_urls("empty"));
        let OperatorQueryResult::Runs(runs) = control
            .query(OperatorQuery::Runs { limit: 10 })
            .await
            .unwrap()
        else {
            panic!("Runs answers Runs");
        };
        assert!(runs.is_empty());
        let OperatorQueryResult::Sessions(sessions) =
            control.query(OperatorQuery::Sessions).await.unwrap()
        else {
            panic!("Sessions answers Sessions");
        };
        assert!(sessions.is_empty());
        let OperatorQueryResult::DreamPreview(report) =
            control.query(OperatorQuery::DreamPreview).await.unwrap()
        else {
            panic!("DreamPreview answers DreamPreview");
        };
        assert!(report.is_empty());
    }

    /// The laziness that is left after the merge: one file, opened on the
    /// first request that needs it and not before. A `komo doctor` that only
    /// prints config must not create a database.
    #[tokio::test]
    async fn the_store_opens_on_the_first_request_that_needs_it() {
        let urls = temp_urls("lazy");
        let path = urls.db.strip_prefix("turso:").unwrap().to_string();
        let control = direct(urls);
        assert!(
            !std::path::Path::new(&path).exists(),
            "resolving the backend must not open the store"
        );

        control
            .query(OperatorQuery::Runs { limit: 5 })
            .await
            .unwrap();
        assert!(
            std::path::Path::new(&path).exists(),
            "and the first query that reads it does"
        );
    }

    #[tokio::test]
    async fn memory_transition_promotes_and_batch_reuses_one_backend() {
        let control = direct(temp_urls("memtrans"));
        // Seed two candidates through the same lazily-opened store.
        let OperatorQueryResult::Memories(initial) =
            control.query(OperatorQuery::Memories).await.unwrap()
        else {
            panic!();
        };
        assert!(initial.is_empty());
        let backend = match &control.backend {
            OperatorBackend::Direct(d) => d,
            _ => unreachable!(),
        };
        let store = backend.db().await.unwrap().clone();
        for content in ["likes tea", "works late"] {
            let mut m = Memory::new(MemoryKind::Preference, content);
            m.status = MemoryStatus::Candidate;
            MemoryRepository::save(store.as_ref(), &m).await.unwrap();
        }
        let OperatorQueryResult::Memories(seeded) =
            control.query(OperatorQuery::Memories).await.unwrap()
        else {
            panic!();
        };
        assert_eq!(seeded.len(), 2);
        // Batch: two transitions on the one resolved backend.
        for m in &seeded {
            let result = control
                .command(OperatorCommand::MemoryTransition {
                    id: m.id.clone(),
                    action: MemoryTransitionAction::Promote,
                })
                .await
                .unwrap();
            assert!(matches!(result, OperatorCommandResult::MemoryTransitioned));
        }
        let OperatorQueryResult::Memories(after) =
            control.query(OperatorQuery::Memories).await.unwrap()
        else {
            panic!();
        };
        assert!(after.iter().all(|m| m.status == MemoryStatus::Active));

        // An unknown id surfaces the same message the gateway path produces.
        let err = control
            .command(OperatorCommand::MemoryTransition {
                id: "nope".into(),
                action: MemoryTransitionAction::Reject,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no memory with id `nope`"));
    }

    #[tokio::test]
    async fn pair_approve_unknown_code_is_not_found() {
        let control = direct(temp_urls("pair"));
        let OperatorCommandResult::PairApproved(outcome) = control
            .command(OperatorCommand::PairApprove {
                code: "ZZZZZZ".into(),
            })
            .await
            .unwrap()
        else {
            panic!();
        };
        assert!(matches!(outcome, PairApproveOutcome::NotFound));
    }
}
