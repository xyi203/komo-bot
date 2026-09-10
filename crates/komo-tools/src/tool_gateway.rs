//! The `tool` tool: reach a tool whose schema is not in the request.
//!
//! ## Why an indirection instead of one more schema
//!
//! Every advertised tool's `description` and `parameters_schema` ride in front
//! of every prompt, for the life of the process. That is the right trade for
//! `read` or `edit`, used most turns. It is the wrong one for `cron` —
//! eighteen parameters, most of them the nested `grants` object, against the
//! handful of times a conversation schedules anything. Such a tool is marked
//! [`Tool::advertised`]`() == false`: it stays in the catalog, stays
//! dispatchable, stays gated and ledgered exactly as before, and its schema is
//! handed over on demand instead.
//!
//! Three shapes, which are the three steps of that loop:
//!
//! * `tool()` — what is reachable this way, one line each.
//! * `tool(name)` — that tool's full parameter schema, as a result rather than
//!   as a permanent prompt cost.
//! * `tool(name, args)` — **never handled here.** The executor resolves this
//!   shape into a direct call to `name` before the approval gate and the
//!   ledger see it, so what runs is a plain `cron` call with `cron`'s own
//!   `call_id`, redaction and approval.
//!
//! That last rule is not tidiness. A tool called *inside* another tool's body
//! (the way a `python` program calls one) has no entry in the round's
//! recorded assistant blocks, so a call of that shape that stops for an
//! approval is never re-dispatched when the answer arrives: the operator
//! approves, and nothing happens. `cron` is exactly a tool whose every
//! mutation is approval-gated, so nesting was never available here.

use async_trait::async_trait;
use komo_core::domain::{
    context::ToolContext,
    tool::{Tool, ToolError, ToolOutput, parse_args},
};
use komo_services::tool_execution::WeakToolExecutor;
use serde::Deserialize;
use serde_json::{Value, json};

/// The name the executor looks for when resolving the indirection.
pub const GATEWAY_TOOL: &str = "tool";

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    name: Option<String>,
}

pub struct ToolGateway {
    /// Weak for the same reason `python`'s is: this tool lives in the very
    /// catalog it reads.
    executor: WeakToolExecutor,
}

impl ToolGateway {
    pub fn new(executor: WeakToolExecutor) -> Self {
        Self { executor }
    }
}

#[async_trait]
impl Tool for ToolGateway {
    fn name(&self) -> &'static str {
        GATEWAY_TOOL
    }

    fn description(&self) -> &'static str {
        "Reach a tool whose schema is not shown above. Call with no arguments to \
         list them, with `name` to read one's parameters, then with `name` and \
         `args` to run it — that last call is the tool itself, gated as its own."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Which tool. Omit to list what is reachable this way."
                },
                "args": {
                    "type": "object",
                    "description": "That tool's own arguments, exactly as `tool(name)` described them. Read them first."
                }
            },
            "required": []
        })
    }

    /// Listing and describing only read the catalog, so a repeat is free and
    /// safe — and the model is meant to re-read a schema it half remembers.
    fn idempotent(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: Args = parse_args(&input)?;
        let Some(executor) = self.executor.upgrade() else {
            return Err(ToolError::Failed(anyhow::anyhow!(
                "the tool executor is gone; `tool` cannot read the catalog"
            )));
        };
        let catalog = executor.snapshot();

        let Some(name) = args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        else {
            let mut lines: Vec<String> = catalog
                .unadvertised()
                .map(|t| format!("- {}: {}", t.name(), t.description()))
                .collect();
            if lines.is_empty() {
                return Ok(ToolOutput::text(
                    "No tools are held back — everything available is already described above.",
                ));
            }
            lines.sort();
            return Ok(ToolOutput::text(format!(
                "Call `tool` again with one of these as `name` to read its parameters:\n{}",
                lines.join("\n")
            )));
        };

        // A name that resolves but is already advertised: the model has the
        // schema, so send it there rather than duplicating it. Being explicit
        // beats silently describing a tool it could just call.
        match catalog.get(name) {
            Some(found) if found.advertised() => Ok(ToolOutput::text(format!(
                "`{name}` is already described above — call it directly."
            ))),
            Some(found) => Ok(ToolOutput::text(format!(
                "{}\n\nParameters (pass these as `args` on your next `tool` call):\n{}",
                found.description(),
                serde_json::to_string_pretty(&found.parameters_schema())
                    .unwrap_or_else(|_| "(schema unavailable)".to_string())
            ))
            .with_title(format!("tool {name}"))),
            None => Err(ToolError::InvalidInput(format!(
                "no tool named `{name}`. Reachable this way: {}",
                reachable(&catalog)
            ))),
        }
    }
}

fn reachable(catalog: &komo_core::domain::catalog::CatalogSnapshot) -> String {
    let names: Vec<&str> = catalog.unadvertised().map(|t| t.name()).collect();
    if names.is_empty() {
        "(none)".to_string()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::detached_ctx;
    use komo_services::tool_execution::{ToolExecutionConfig, ToolExecutor};
    use std::sync::Arc;

    struct Stub(&'static str, bool);
    #[async_trait]
    impl Tool for Stub {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "what it is for"
        }
        fn advertised(&self) -> bool {
            self.1
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{"action":{"type":"string"}}})
        }
        async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ran"))
        }
    }

    /// Holds the executor alive: the gateway's handle is weak on purpose, so a
    /// test that drops it would exercise the "executor is gone" branch instead.
    fn gateway() -> (ToolExecutor, ToolGateway) {
        let mut tools = ToolExecutor::new(ToolExecutionConfig::default());
        tools.register(Arc::new(Stub("cron", false)));
        tools.register(Arc::new(Stub("wiki_search", false)));
        tools.register(Arc::new(Stub("read", true)));
        let gateway = ToolGateway::new(tools.downgrade());
        (tools, gateway)
    }

    #[tokio::test]
    async fn no_arguments_lists_only_what_is_held_back() {
        let (_keep, tool) = gateway();
        let out = tool.call(json!({}), &detached_ctx("s")).await.unwrap();
        assert!(out.text.contains("cron"), "{}", out.text);
        assert!(out.text.contains("wiki_search"), "{}", out.text);
        assert!(
            !out.text.contains("- read:"),
            "an advertised tool is already described above: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn a_name_returns_the_parameters_the_prompt_does_not_carry() {
        let (_keep, tool) = gateway();
        let out = tool
            .call(json!({ "name": "cron" }), &detached_ctx("s"))
            .await
            .unwrap();
        assert!(out.text.contains("what it is for"));
        assert!(
            out.text.contains("\"action\""),
            "the schema itself: {}",
            out.text
        );
    }

    /// Describing a tool the model can already see would duplicate a schema it
    /// is holding — say so instead, so it calls rather than asks again.
    #[tokio::test]
    async fn an_advertised_name_is_sent_back_to_the_direct_call() {
        let (_keep, tool) = gateway();
        let out = tool
            .call(json!({ "name": "read" }), &detached_ctx("s"))
            .await
            .unwrap();
        assert!(out.text.contains("already described"), "{}", out.text);
    }

    /// The error has to name what *is* reachable: the model guessed, and the
    /// fix is one call away only if it is told the alternatives.
    #[tokio::test]
    async fn an_unknown_name_names_the_alternatives() {
        let (_keep, tool) = gateway();
        let err = tool
            .call(json!({ "name": "nope" }), &detached_ctx("s"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        let text = err.to_string();
        assert!(
            text.contains("cron") && text.contains("wiki_search"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn the_model_facing_text_stays_short() {
        let (_keep, tool) = gateway();
        crate::test_support::assert_model_text_budget(&tool);
    }
}
