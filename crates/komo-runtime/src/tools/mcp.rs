//! 一个 MCP 工具（`docs/mcp.md`）：走与内置工具同一条路——`prepare` 出计划、Policy /
//! 审批、`start_call`、执行、落账。
//!
//! 它不是第七个基础工具，而是操作者接进来的外部能力：副作用由服务器决定，komo 看不见。
//! 所以计划是 [`Operation::McpCall`]，只读与否由操作者在配置里声明（`read_only`），不信
//! 服务器自报的 `readOnlyHint`。只读的中断后可以重读（`SafeReread`），其余的停下来问人
//! （`NoSafeRecovery`）。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use rmcp::model::{CallToolResult, ContentBlock};

use super::plan_time;
use crate::mcp::McpServer;

#[derive(Debug)]
pub struct McpTool {
    server: Arc<McpServer>,
    /// 服务器上的原名（调用时用它）。
    remote: String,
    definition: ToolDefinition,
    read_only: bool,
}

impl McpTool {
    /// `name` 是 [`crate::mcp::tool_name`] 给的模型侧名字。
    pub fn new(
        server: Arc<McpServer>,
        remote: &rmcp::model::Tool,
        name: String,
        read_only: bool,
    ) -> Self {
        let description = remote
            .description
            .as_deref()
            .unwrap_or_default()
            .to_string();
        let definition = ToolDefinition {
            name,
            description: format!("[MCP {}] {description}", server.name()),
            parameters: serde_json::Value::Object((*remote.input_schema).clone()),
        };
        Self {
            server,
            remote: remote.name.to_string(),
            definition,
            read_only,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args = match args {
            serde_json::Value::Object(map) => serde_json::Value::Object(map),
            serde_json::Value::Null => serde_json::Value::Object(Default::default()),
            other => {
                return Err(ToolError::InvalidArguments {
                    message: format!(
                        "{} 的参数要是一个 JSON 对象，收到 {other}",
                        self.definition.name
                    ),
                });
            }
        };
        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: self.definition.name.clone(),
            operation: Operation::McpCall {
                server: self.server.name().to_string(),
                tool: self.remote.clone(),
                read_only: self.read_only,
            },
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args,
            cwd: Some(ctx.cwd.clone()),
            // 目标在服务器那边，komo 只看得见"哪个服务器的哪个工具"，已经在操作里了。
            targets: vec![],
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: if self.read_only {
                RecoveryMode::SafeReread
            } else {
                RecoveryMode::NoSafeRecovery
            },
        })
    }

    async fn execute(
        &self,
        plan: komo_kernel::types::plan::ApprovedPlan,
        _ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let arguments = match &plan.plan().args {
            serde_json::Value::Object(map) => map.clone(),
            _ => Default::default(),
        };
        let result = self
            .server
            .call(&self.remote, arguments)
            .await
            .map_err(|message| {
                // 请求可能已经到了服务器：只读的当失败重读，其余的说不清发生没有。
                let message = format!("MCP {} 调用失败：{message}", self.server.name());
                if self.read_only {
                    ToolError::Failed { message }
                } else {
                    ToolError::Uncertain { message }
                }
            })?;
        let text = render(&result);
        sink.write_stdout(text.as_bytes())
            .await
            .map_err(|error| ToolError::Failed {
                message: error.to_string(),
            })?;
        Ok(ToolOutput {
            status: if result.is_error == Some(true) {
                ToolResultStatus::Failed
            } else {
                ToolResultStatus::Completed
            },
            // 完整的 `CallToolResult` 留在 `output.json`：结构化结果给以后的脚本用。
            result: serde_json::to_value(&result).unwrap_or_default(),
            exit_code: None,
            artifacts: vec![],
            preview: Some(text),
        })
    }
}

/// 给模型看的正文：文本原样，图片 / 音频只留一句说明（base64 不是给模型读的），资源给
/// 出它的文本或 URI。一段文本都没有时退回结构化结果的 JSON。
fn render(result: &CallToolResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::Image(image) => parts.push(format!(
                "[图片 {}，{} 字节 base64，未展开]",
                image.mime_type,
                image.data.len()
            )),
            ContentBlock::Audio(audio) => parts.push(format!(
                "[音频 {}，{} 字节 base64，未展开]",
                audio.mime_type,
                audio.data.len()
            )),
            ContentBlock::Resource(resource) => {
                parts.push(serde_json::to_string(&resource.resource).unwrap_or_default())
            }
            ContentBlock::ResourceLink(link) => parts.push(format!("[资源 {}]", link.uri)),
            other => parts.push(serde_json::to_string(other).unwrap_or_default()),
        }
    }
    if parts.is_empty()
        && let Some(structured) = &result.structured_content
    {
        parts.push(serde_json::to_string_pretty(structured).unwrap_or_default());
    }
    parts.join("\n")
}
