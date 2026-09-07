//! Operator control: one module owns how host-operator actions (list/inspect
//! reads, governance and maintenance writes) reach komo's state.
//!
//! Turso's exclusive cross-process lock means the gateway is the only process
//! that opens the db, so there is exactly one transport: the gateway's loopback
//! api channel. [`OperatorControl`] is the CLI's whole view of it — issue one
//! typed [`OperatorQuery`]/[`OperatorCommand`] and never touch HTTP, a db, or a
//! route path. The business result comes from the shared projections and
//! transitions in [`actions`], which is also what the gateway's own handlers
//! call, so the two can't fork.

pub mod actions;
pub mod request;
pub mod view;

pub use request::*;
pub use view::*;

use crate::infra::gateway_client::GatewayClient;

/// The operator surface's single entry point. Resolve once per CLI command,
/// then issue any number of queries/commands over the same connection.
pub struct OperatorControl {
    client: GatewayClient,
}

impl OperatorControl {
    /// Reach the gateway, starting one if none is running.
    pub async fn connect() -> anyhow::Result<Self> {
        Ok(Self {
            client: GatewayClient::connect_or_start().await?,
        })
    }

    /// Probe for a running gateway without starting one — for `komo doctor`,
    /// whose job is to describe the machine, not to change it.
    pub async fn probe() -> Option<Self> {
        GatewayClient::try_connect()
            .await
            .map(|client| Self { client })
    }

    /// Run one read-only operator query.
    pub async fn query(&self, query: OperatorQuery) -> anyhow::Result<OperatorQueryResult> {
        self.client.operator_query(query).await
    }

    /// Run one state-changing operator command.
    pub async fn command(&self, command: OperatorCommand) -> anyhow::Result<OperatorCommandResult> {
        self.client.operator_command(command).await
    }
}

/// Unix seconds — the operator surface's one clock read per request.
pub(crate) fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
