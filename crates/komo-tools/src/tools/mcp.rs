//! 一个 MCP 工具（`docs/mcp.md`）：走与内置工具同一条路——`prepare` 出计划、
//! `start_call`、执行、落账。
//!
//! 它不是第七个基础工具，而是操作者接进来的外部能力：副作用由服务器决定，komo 看不见。
//! 所以计划是 [`Operation::McpCall`]，只读与否由操作者在配置里声明（`read_only`），不信
//! 服务器自报的 `readOnlyHint`。只读的中断后可以重读（`SafeReread`），其余的不重跑、中断
//! 结果交给模型（`NoSafeRecovery`，§8.6）。
//!
//! 默认不声明给模型（`exposure = "codemode"`，`docs/mcp.md` §3）：只在 codemode 脚本里调。

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode, Replay};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{
    ToolContext, ToolDefinition, ToolError, ToolExposure, ToolImage, ToolNamespace, ToolOutput,
    image_mime,
};
use rmcp::model::{CallToolResult, ContentBlock};

use super::plan_time;
use crate::mcp::{CallError, McpConnection};

#[derive(Debug)]
pub struct McpTool {
    /// 连接一直在：没连上、断了、要登录，调用时由它当场处理（`docs/mcp.md` §2.2）。
    server: Arc<McpConnection>,
    /// 服务器上的原名（调用时用它）。
    remote: String,
    definition: ToolDefinition,
    read_only: bool,
    exposure: ToolExposure,
    /// 操作者在 `outward` 里声明的（`docs/confirm.md` §4）。
    outward: bool,
}

impl McpTool {
    /// `name` 是 [`crate::mcp::tool_name`] 给的模型侧名字。
    pub fn new(
        server: Arc<McpConnection>,
        remote: &rmcp::model::Tool,
        name: String,
        read_only: bool,
        exposure: ToolExposure,
        outward: bool,
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
            doc: None,
        };
        Self {
            server,
            remote: remote.name.to_string(),
            definition,
            read_only,
            exposure,
            outward,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    /// 操作者声明为只读的工具重启后可以重跑；其余的说不清发生没有，不重跑（§8.6）。
    fn replay(&self) -> Replay {
        if self.read_only {
            Replay::Safe
        } else {
            Replay::Unsafe
        }
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn namespace(&self) -> Option<ToolNamespace> {
        Some(self.server.namespace())
    }

    fn outward(&self) -> bool {
        self.outward
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
            recovery: if self.read_only {
                RecoveryMode::SafeReread
            } else {
                RecoveryMode::NoSafeRecovery
            },
            then_run: None,
        })
    }

    async fn execute(
        &self,
        plan: komo_kernel::types::plan::ExecutionPlan,
        _ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let arguments = match &plan.args {
            serde_json::Value::Object(map) => map.clone(),
            _ => Default::default(),
        };
        let result =
            self.server
                .call(&self.remote, arguments)
                .await
                .map_err(|error| match error {
                    // 没连上、凭证被拒：请求没到服务器，确定没执行。
                    CallError::NotSent(message) => ToolError::Failed { message },
                    // 请求可能已经到了服务器：只读的当失败重读，其余的说不清发生没有。
                    CallError::Sent(message) => {
                        let message = format!("MCP {} 调用失败：{message}", self.server.name());
                        if self.read_only {
                            ToolError::Failed { message }
                        } else {
                            ToolError::Uncertain { message }
                        }
                    }
                })?;
        let (text, images) = render(&result);
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
            images,
            preview: Some(text),
        })
    }
}

/// 给模型看的正文与图片：文本原样；认得出的图片（PNG / JPEG / GIF / WebP）解出来随结果附给
/// 模型，正文里留一句说明；认不出的图片和音频只留一句说明（base64 不是给模型读的）；资源给
/// 出它的文本或 URI。一段文本都没有时退回结构化结果的 JSON。
fn render(result: &CallToolResult) -> (String, Vec<ToolImage>) {
    let mut parts: Vec<String> = Vec::new();
    let mut images = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::Image(image) => match decode_image(&image.data) {
                Some(decoded) => {
                    parts.push(format!("[图片 {}，已附在结果里]", decoded.mime));
                    images.push(decoded);
                }
                None => parts.push(format!(
                    "[图片 {}，{} 字节 base64，未展开]",
                    image.mime_type,
                    image.data.len()
                )),
            },
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
    (parts.join("\n"), images)
}

/// 解 base64、按文件头认格式。格式以字节为准，不信服务器报的 `mimeType`。
fn decode_image(data: &str) -> Option<ToolImage> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()?;
    let mime = image_mime(&bytes)?;
    Some(ToolImage {
        mime: mime.to_string(),
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recognised_image_is_attached_and_anything_else_stays_a_note() {
        let png = b"\x89PNG\r\n\x1a\nfake".to_vec();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&png);
        let result = CallToolResult::success(vec![
            ContentBlock::text("截图如下"),
            ContentBlock::image(encoded, "image/png"),
            ContentBlock::image("bm90IGFuIGltYWdl", "image/png"),
        ]);
        let (text, images) = render(&result);
        assert_eq!(
            images,
            vec![ToolImage {
                mime: "image/png".into(),
                bytes: png
            }]
        );
        assert!(text.contains("[图片 image/png，已附在结果里]"), "{text}");
        assert!(text.contains("未展开"), "认不出的那张只留说明：{text}");
    }
}
