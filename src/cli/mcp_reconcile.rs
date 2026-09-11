//! Mount the MCP servers that were not reachable at boot, without a restart.
//!
//! ## Why this exists
//!
//! `[mcp.servers.*]` is connected once during wiring, and a server that was
//! down then had no tools for the rest of the process's life. That was never a
//! limit of the catalog — `ToolCatalog::mount_all` takes `&self` and hands back
//! a guard, which is how the plugin host adds tools mid-process — it was a
//! missing caller. And the premise was wrong on its own terms: an MCP server is
//! a separate process on a network, the one kind of dependency that *is*
//! expected to come and go, so binding komo's view of it to one instant at boot
//! matched nothing about how it behaves.
//!
//! ## What it does and deliberately does not
//!
//! It only **adds**. A server whose tools are already mounted is left alone:
//! those `Arc<dyn Tool>`s are shared across three catalogs and a running turn
//! may be in the middle of a call on one, so re-mounting a working server would
//! buy nothing and risk swapping a tool out from under a call. Tools are never
//! removed either — a server that stops answering fails its calls with a
//! message the model can act on, which is more useful than a tool that silently
//! vanishes mid-conversation.
//!
//! Three things make this safe rather than clever, and none of them are new:
//! a turn pins one `CatalogSnapshot` for its whole length, so a mount lands
//! between turns and never inside one; MCP tools are not advertised, so a mount
//! does not touch the request's schema block at all; and the held-back roster
//! in the system prompt is rendered per turn, so a newly mounted tool is named
//! to the model on its next turn rather than after a restart.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_bot::daemon::{Maintenance, MaintenanceSummary};
use komo_core::domain::catalog::{Registration, ToolCatalog};
use komo_core::domain::tool::Tool;
use tracing::info;

/// The prefix every tool from `server` carries in the catalog.
fn prefix_of(server: &str) -> String {
    format!("mcp__{server}__")
}

pub struct McpReconcileSweep {
    servers: Vec<komo_config::McpServerConfig>,
    /// One per runtime — the same tool object is mounted into all three, as
    /// wiring does, so a sub-agent and a routine see what the conversation
    /// sees.
    catalogs: Vec<Arc<ToolCatalog>>,
    /// Kept for the life of the process: dropping a [`Registration`] unmounts
    /// what it mounted, so letting one fall would undo the work on the way out
    /// of this function.
    held: Mutex<Vec<Registration>>,
}

impl McpReconcileSweep {
    pub fn new(
        servers: Vec<komo_config::McpServerConfig>,
        catalogs: Vec<Arc<ToolCatalog>>,
    ) -> Self {
        Self {
            servers,
            catalogs,
            held: Mutex::new(Vec::new()),
        }
    }

    /// Servers with nothing mounted right now — read from the live catalog
    /// rather than from a record of what this sweep did, so a tool wiring
    /// mounted at boot counts exactly as one this sweep mounted later.
    fn missing(&self) -> Vec<&komo_config::McpServerConfig> {
        let Some(catalog) = self.catalogs.first() else {
            return Vec::new();
        };
        let snapshot = catalog.snapshot();
        self.servers
            .iter()
            .filter(|server| {
                let prefix = prefix_of(&server.name);
                !snapshot.names().any(|name| name.starts_with(&prefix))
            })
            .collect()
    }
}

#[async_trait]
impl Maintenance for McpReconcileSweep {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary> {
        let missing = self.missing();
        if missing.is_empty() {
            return Ok(MaintenanceSummary::default());
        }
        let configs: Vec<komo_config::McpServerConfig> = missing.into_iter().cloned().collect();
        let names: Vec<&str> = configs.iter().map(|s| s.name.as_str()).collect();
        info!(servers = %names.join(", "), "retrying MCP servers with no tools mounted");

        let tools: Vec<Arc<dyn Tool>> = super::wiring::mcp_tools(&configs).await;
        if tools.is_empty() {
            // Still down. Not a failure of the sweep — a server nobody can
            // reach is the normal case this exists to recover from, and
            // returning `Err` here would walk the supervisor's breaker toward
            // tripping on somebody else's outage.
            return Ok(MaintenanceSummary::default());
        }

        let mounted: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
        let mut held = self.held.lock().expect("mcp registration mutex");
        for catalog in &self.catalogs {
            // One mount per catalog, not one per tool: a server arriving with
            // a dozen tools costs the prompt cache one generation bump.
            held.push(catalog.mount_all(tools.clone()));
        }
        info!(
            tools = %mounted.join(", "),
            count = mounted.len(),
            "mounted MCP tools from a server that was unreachable at boot"
        );
        Ok(MaintenanceSummary::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_core::domain::{
        context::ToolContext,
        tool::{ToolError, ToolOutput},
    };

    struct Stub(&'static str);
    #[async_trait]
    impl Tool for Stub {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "stub"
        }
        async fn call(
            &self,
            _i: serde_json::Value,
            _c: &ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    fn server(name: &str) -> komo_config::McpServerConfig {
        komo_config::McpServerConfig {
            name: name.to_string(),
            url: "http://127.0.0.1:1/mcp".to_string(),
            token: None,
            tools: vec!["t".to_string()],
        }
    }

    /// The live catalog is the authority on "already mounted" — not a record
    /// of what this sweep did, so a server wiring connected at boot is never
    /// retried.
    #[test]
    fn a_server_with_tools_mounted_is_not_retried() {
        let catalog = Arc::new(ToolCatalog::new());
        catalog.register(Arc::new(Stub("mcp__up__t")));
        let sweep = McpReconcileSweep::new(vec![server("up"), server("down")], vec![catalog]);
        let missing: Vec<&str> = sweep.missing().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(missing, vec!["down"]);
    }

    /// A prefix match, so one server's tools never satisfy another's — the
    /// names are `mcp__<server>__<tool>` and `up` must not match `upstream`.
    #[test]
    fn one_servers_tools_do_not_satisfy_another() {
        let catalog = Arc::new(ToolCatalog::new());
        catalog.register(Arc::new(Stub("mcp__upstream__t")));
        let sweep = McpReconcileSweep::new(vec![server("up")], vec![catalog]);
        assert_eq!(sweep.missing().len(), 1, "`up` is still unmounted");
    }

    #[tokio::test]
    async fn nothing_configured_is_a_quiet_no_op() {
        let sweep = McpReconcileSweep::new(vec![], vec![Arc::new(ToolCatalog::new())]);
        assert_eq!(sweep.run().await.unwrap(), MaintenanceSummary::default());
    }
}
