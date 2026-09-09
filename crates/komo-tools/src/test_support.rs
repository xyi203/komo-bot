//! Shared test fixtures for tool tests.
//!
//! Every tool now takes an explicit [`ToolContext`] (tool trait v2), so each
//! test module used to hand-roll the same deny-all / allow-all approver plus a
//! detached session. These are those, once.
//!
//! Tools with interesting approval behavior (`shell`, `file`, `homeassistant`)
//! keep their own recording doubles — asserting *what* was asked is part of what
//! those tests are for.

use std::sync::Arc;

use komo_core::domain::{
    approval::{ApprovalRequest, Approver, Decision, Risk},
    context::{SessionContext, ToolContext},
    tool::Tool,
};

/// Mirrors what every real approver does when nobody can answer: `Risk::Safe`
/// passes (reads never prompt), anything side-effecting is refused.
///
/// Deliberately not a blanket deny — that would refuse the `read`/`grep` family
/// too, which no production path does, so tests would be asserting against a
/// policy that doesn't exist.
pub struct SafeOnly;

#[async_trait::async_trait]
impl Approver for SafeOnly {
    async fn decide(&self, request: &ApprovalRequest) -> Decision {
        if request.risk == Risk::Safe {
            Decision::Allow
        } else {
            Decision::deny()
        }
    }
}

/// Approves everything.
pub struct AllowAll;

#[async_trait::async_trait]
impl Approver for AllowAll {
    async fn decide(&self, _request: &ApprovalRequest) -> Decision {
        Decision::Allow
    }
}

/// A detached context for `session` with the [`SafeOnly`] approver: a test using
/// it fails loudly if the tool asks approval for something it shouldn't, while
/// read-only work behaves as it does in production.
pub fn detached_ctx(session: &str) -> ToolContext {
    ToolContext::new(SessionContext::detached(session), None, Arc::new(SafeOnly))
}

/// A detached context whose approver allows everything.
pub fn approving_ctx(session: &str) -> ToolContext {
    ToolContext::new(SessionContext::detached(session), None, Arc::new(AllowAll))
}

/// The budget every tool's model-facing text is held to: the description and
/// each parameter description are re-sent on every round of every turn, so they
/// pay for themselves in tokens or they do not belong there. Behavioral rules
/// belong in `komo-bot`'s gated `*_GUIDANCE` consts, which are sent once.
pub const MAX_DESCRIPTION_CHARS: usize = 240;
pub const MAX_PARAM_DESCRIPTION_CHARS: usize = 120;

/// Asserts a tool's description and every parameter description — nested
/// `items`/`properties` included — stay inside that budget.
pub fn assert_model_text_budget(tool: &dyn Tool) {
    assert_description_budget(tool.name(), tool.description());
    assert_schema_budget(tool.name(), &tool.parameters_schema());
}

/// The description half, for a tool too expensive to construct in a test.
pub fn assert_description_budget(name: &str, description: &str) {
    let len = description.chars().count();
    assert!(
        len <= MAX_DESCRIPTION_CHARS,
        "{name}: description is {len} chars, over {MAX_DESCRIPTION_CHARS}"
    );
}

/// The parameter half, likewise.
pub fn assert_schema_budget(name: &str, schema: &serde_json::Value) {
    walk_schema(name, &[], schema);
}

fn walk_schema(tool: &str, path: &[&str], schema: &serde_json::Value) {
    let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) else {
        return;
    };
    for (param, spec) in properties {
        let mut here: Vec<&str> = path.to_vec();
        here.push(param);
        if let Some(text) = spec.get("description").and_then(|v| v.as_str()) {
            let len = text.chars().count();
            assert!(
                len <= MAX_PARAM_DESCRIPTION_CHARS,
                "{tool}.{}: description is {len} chars, over {MAX_PARAM_DESCRIPTION_CHARS}",
                here.join(".")
            );
        }
        walk_schema(tool, &here, spec);
        if let Some(items) = spec.get("items") {
            walk_schema(tool, &here, items);
        }
    }
}
