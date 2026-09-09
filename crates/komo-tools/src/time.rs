use async_trait::async_trait;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;

use komo_core::domain::{
    context::ToolContext,
    tool::{Tool, ToolError, ToolOutput},
};

pub struct TimeTool;

#[async_trait]
impl Tool for TimeTool {
    fn name(&self) -> &'static str {
        "time"
    }

    fn description(&self) -> &'static str {
        "Current date and time, UTC, RFC 3339."
    }

    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let s = time::OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|e| ToolError::Failed(e.into()))?;
        Ok(ToolOutput::text(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_core::domain::context::SessionContext;
    use std::sync::Arc;

    struct DenyAll;
    #[async_trait]
    impl komo_core::domain::approval::Approver for DenyAll {
        async fn decide(
            &self,
            _r: &komo_core::domain::approval::ApprovalRequest,
        ) -> komo_core::domain::approval::Decision {
            komo_core::domain::approval::Decision::deny()
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new(
            SessionContext::detached("cli:test"),
            None,
            Arc::new(DenyAll),
        )
    }

    #[tokio::test]
    async fn time_tool_returns_non_empty_string() {
        let out = TimeTool.call(Value::Null, &ctx()).await.unwrap();
        assert!(!out.text.is_empty());
    }

    #[test]
    fn the_model_facing_text_stays_short() {
        crate::test_support::assert_model_text_budget(&TimeTool);
    }
}
