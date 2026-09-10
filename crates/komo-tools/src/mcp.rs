//! Adapter turning one tool on an external MCP server into a komo [`Tool`].
//!
//! Everything protocol-shaped lives in `komo-mcp`; this file is only the seam.
//! One instance per mounted remote tool, all sharing the server's
//! [`McpClient`] — the client owns a session, and one per tool would multiply
//! handshakes for nothing.
//!
//! **The catalog is fixed at wiring time.** `ToolExecutor::register` takes
//! `Arc::get_mut`, and its name-sorted order exists so the serialized tool block
//! is byte-stable for the provider's prompt cache. A server that is down when
//! komo starts has no tools for the process's lifetime — deliberately, since
//! adding one mid-session would invalidate every cached prefix.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use komo_core::domain::approval::{ActionRef, ApprovalRequest};
use komo_core::domain::context::ToolContext;
use komo_core::domain::tool::{
    APPROVAL_BOUND, RetryHint, Tool, ToolError, ToolOutput, TransientError,
};
use komo_mcp::{McpClient, McpError, McpToolDef};
use serde_json::Value;

/// Prefix that namespaces every mounted remote tool.
///
/// Without it a server's `read` would collide with komo's own in the catalog's
/// `BTreeMap` — last registration wins, and the shadowing would be silent.
const NAMESPACE: &str = "mcp";

/// One tool on one MCP server.
pub struct McpTool {
    client: Arc<McpClient>,
    /// The namespaced catalog name. Leaked because [`Tool::name`] is
    /// `&'static str` while MCP names are only known after `tools/list` — one
    /// leak per mounted tool per process, allocated at wiring and alive until
    /// exit anyway.
    name: &'static str,
    /// Server-authored, likewise leaked. This text goes into the system prompt:
    /// mounting a server means trusting its tool descriptions the same way you
    /// trust its results, which is why the allowlist is per-tool.
    description: &'static str,
    /// The name to send in `tools/call` — the server's own, un-namespaced.
    remote_name: String,
    schema: Value,
}

impl McpTool {
    /// Build the adapter for `def` on `client`.
    pub fn new(client: Arc<McpClient>, def: McpToolDef) -> Self {
        let name: &'static str = String::leak(qualified_name(client.server(), &def.name));
        let description: &'static str = String::leak(match def.description {
            Some(text) if !text.trim().is_empty() => text,
            // A nameless, description-less tool is unusable by the model; say
            // where it came from rather than advertising an empty string.
            _ => format!("`{}` on MCP server `{}`", def.name, client.server()),
        });
        Self {
            client,
            name,
            description,
            remote_name: def.name,
            schema: normalize_schema(def.input_schema),
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    /// Never in the schema block, whatever the server sent.
    ///
    /// A remote schema is the one komo cannot size: it is authored elsewhere,
    /// arrives at wiring, and is re-sent every round for the life of the
    /// process. That cost is why `[mcp.servers.*]`'s `tools` allowlist is
    /// required and closed by default — and with the schema out of the
    /// request, that allowlist goes back to meaning what it should, *which of
    /// this server's tools may be called at all*, rather than doubling as a
    /// budget.
    ///
    /// Every MCP call is approval-gated, so this relies on the `tool`
    /// indirection being resolved before the gate (see
    /// `tool_execution::resolve_gateway_calls`) — a wrapper would break resume
    /// after an approval.
    fn advertised(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    /// Approval-gated, always.
    ///
    /// MCP's `annotations.readOnlyHint` would let a server mark a tool safe,
    /// but the server is the party being gated — a remote must not be able to
    /// declare itself harmless. An operator who wants a specific tool to run
    /// unprompted says so in `[policy]`:
    ///
    /// ```toml
    /// [[policy.rule]]
    /// category = "mcp"
    /// match    = "exact"
    /// value    = "memos.list_memos"
    /// effect   = "allow"
    /// ```
    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let server = self.client.server();
        let target = format!("{server}.{}", self.remote_name);
        let request = ApprovalRequest::normal(format!("call MCP tool `{target}`"))
            .with_scope_key(format!("mcp:{target}"))
            .with_action(ActionRef::Mcp {
                server: server.to_string(),
                tool: self.remote_name.clone(),
            });
        let decision = ctx.decide(&request).await;
        if !decision.is_allowed() {
            return Err(ToolError::Denied(
                decision
                    .feedback()
                    .unwrap_or("MCP tool call was not approved.")
                    .to_string(),
            ));
        }

        let result = self
            .client
            .call(&self.remote_name, input)
            .await
            .map_err(|e| map_error(e, &target))?;

        let mut text = result.text;
        if !result.dropped.is_empty() {
            // The model can only read text; saying what was elided beats a
            // response that silently looks complete.
            text.push_str(&format!(
                "\n\n[{} returned {} that this tool cannot render as text; \
                 the full payload is in the run ledger]",
                target,
                describe_dropped(&result.dropped)
            ));
        }
        if result.is_error {
            // MCP's in-band `isError`: the call succeeded, the *operation*
            // failed. Deliberately not a `ToolError` — the message is
            // remote-controlled text, and the executor's retry classifier falls
            // back to substring matching on error strings, so a server echoing
            // "connection refused" could otherwise trigger a retry of a
            // non-idempotent call.
            text = format!("The MCP server reported an error:\n{}", text.trim_start());
        }
        if text.trim().is_empty() {
            text = format!("`{target}` returned no content.");
        }

        Ok(ToolOutput::text(text)
            .with_title(target)
            .with_structured(result.structured))
    }

    /// Long enough to outlast a human reading the approval prompt — otherwise
    /// the call is aborted while they are still deciding.
    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(APPROVAL_BOUND)
    }

    /// A remote tool's effects are unknown by construction, so an ambiguous
    /// failure must never be re-sent.
    fn idempotent(&self) -> bool {
        false
    }
}

/// `mcp__<server>__<tool>`, with anything outside `[A-Za-z0-9_-]` folded to `_`.
///
/// Providers validate function names against roughly that character set, so a
/// server using dots or slashes would otherwise produce a request the model API
/// rejects — a failure that surfaces as a broken turn, far from its cause.
fn qualified_name(server: &str, tool: &str) -> String {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("{NAMESPACE}__{}__{}", sanitize(server), sanitize(tool))
}

/// Ensure the server's schema is an object schema the provider will accept.
///
/// Servers do send `{}` or a bare `null` for a no-argument tool; several
/// providers reject a function whose parameters aren't a `type: "object"`
/// schema, so fill in the canonical empty one.
fn normalize_schema(schema: Value) -> Value {
    match &schema {
        Value::Object(map) if map.contains_key("type") => schema,
        _ => serde_json::json!({ "type": "object", "properties": {}, "required": [] }),
    }
}

/// `"1 image and 2 resources"`.
fn describe_dropped(dropped: &BTreeMap<&'static str, usize>) -> String {
    let parts: Vec<String> = dropped
        .iter()
        .map(|(kind, n)| {
            if *n == 1 {
                format!("1 {kind}")
            } else {
                format!("{n} {kind}s")
            }
        })
        .collect();
    match parts.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// Map a client failure onto the executor's error taxonomy.
///
/// - Transport: tagged [`RetryHint::Ambiguous`], not `Connection` — rmcp does
///   not tell us whether the bytes left the machine, and this tool is not
///   idempotent, so the executor correctly declines to re-send.
/// - Protocol: the server accepted the request and rejected it (`invalid
///   params`, `method not found`, a tool-specific refusal). Never retried, and
///   [`ToolError::InvalidInput`] is what turns it into a "fix the arguments"
///   nudge the model can act on.
fn map_error(error: McpError, target: &str) -> ToolError {
    let message = error.to_string();
    if error.retryable() {
        ToolError::Failed(anyhow::Error::new(TransientError::new(
            RetryHint::Ambiguous,
            message,
        )))
    } else {
        ToolError::InvalidInput(format!("`{target}` rejected the call: {message}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_name_namespaces_and_sanitizes() {
        assert_eq!(
            qualified_name("memos", "create_memo"),
            "mcp__memos__create_memo"
        );
        // Dots and slashes would be rejected by the providers' function-name
        // validation, so they must not survive into the catalog.
        assert_eq!(
            qualified_name("my.server", "memo/create"),
            "mcp__my_server__memo_create"
        );
    }

    #[test]
    fn qualified_name_keeps_servers_from_colliding() {
        assert_ne!(
            qualified_name("memos", "read"),
            qualified_name("notes", "read"),
        );
        // …and neither can shadow komo's own `read`.
        assert!(qualified_name("memos", "read").starts_with("mcp__"));
    }

    #[test]
    fn schema_without_a_type_becomes_the_empty_object_schema() {
        let filled = normalize_schema(serde_json::json!({}));
        assert_eq!(filled["type"], "object");
        assert_eq!(normalize_schema(Value::Null)["type"], "object");
    }

    #[test]
    fn schema_from_the_server_is_passed_through_untouched() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "content": { "type": "string" } },
            "required": ["content"],
        });
        assert_eq!(normalize_schema(schema.clone()), schema);
    }

    #[test]
    fn dropped_content_is_described_in_english() {
        let mut dropped = BTreeMap::new();
        dropped.insert("image", 1);
        assert_eq!(describe_dropped(&dropped), "1 image");
        dropped.insert("resource", 2);
        assert_eq!(describe_dropped(&dropped), "1 image and 2 resources");
    }

    #[test]
    fn transport_errors_are_ambiguous_and_protocol_errors_are_terminal() {
        let transport = McpError::Transport {
            server: "memos".into(),
            source: Box::new(std::io::Error::other("socket closed")),
        };
        match map_error(transport, "memos.create_memo") {
            ToolError::Failed(e) => {
                let hint = e
                    .downcast_ref::<TransientError>()
                    .expect("transport failures must carry a retry hint")
                    .hint;
                assert_eq!(hint, RetryHint::Ambiguous);
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        // A server-authored message must not reach the error path, where the
        // executor's fallback classifier matches on substrings like this one.
        let protocol = McpError::Protocol {
            server: "memos".into(),
            message: "connection refused by upstream".into(),
        };
        assert!(matches!(
            map_error(protocol, "memos.create_memo"),
            ToolError::InvalidInput(_)
        ));
    }
}
