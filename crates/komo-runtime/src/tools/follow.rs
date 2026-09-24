//! `follow`：把一句话追加进一个**已有的**任务会话（`docs/home-dispatcher.md` §4）。
//!
//! 与 [`super::dispatch`] 同一类编排操作，差别只在"新建"还是"追加"：`task_id` 是任务
//! 短号（任务会话 id 末几位，[`komo_kernel::types::task::short_id`]），解析成具体会话
//! 要查"这个 home 名下有哪些任务会话"，只有 Gateway 的 `TaskSpawner` 实现够得到，工具
//! 这一层原样把短号交给它。

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode};
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::{normalized, parse_args, plan_time};

/// 模型给的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowArgs {
    /// 任务短号（看板里那四位，或者用户话里带的 `#xxxx`）。
    pub task_id: String,
    /// 追加进那个任务会话的新输入。
    pub text: String,
}

#[derive(Debug)]
pub struct FollowTool;

impl Default for FollowTool {
    fn default() -> Self {
        Self::new()
    }
}

impl FollowTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FollowTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "follow".into(),
            description: "追问一个进行中或最近完成的任务：把 text 作为新的一句话提交进它自己\
                          的会话，不要为同一件事另开一个 dispatch。task_id 是看板里的短号\
                          （例如 3f2a）。提交成功就回一句\"已转给 #短号\"，如果那个任务正好\
                          在跑，会改说\"#短号 正在跑，这句排在它后面\"。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "任务短号，看板里的那几位（例如 3f2a）"
                    },
                    "text": {
                        "type": "string",
                        "description": "追加进那个任务会话的新输入"
                    }
                },
                "required": ["task_id", "text"],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: FollowArgs = parse_args(args, "follow")?;
        if args.task_id.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "task_id 不能是空的：追问总得说清追问哪一个".into(),
            });
        }
        if args.text.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "text 不能是空的：空输入没有什么可以提交".into(),
            });
        }
        // `#3f2a` 这种带井号的写法也收——模型把看板上的短号原样抄过来时常带着它。
        let task_id = args.task_id.trim_start_matches('#').to_string();

        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "follow".into(),
            operation: Operation::Follow {
                task_id,
                text: args.text.clone(),
            },
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![],
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: RecoveryMode::VerifyTarget,
        })
    }

    async fn execute(
        &self,
        _plan: komo_kernel::types::plan::ApprovedPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed {
            message: "follow 不由工具执行：一次追问要么被 executor 的编排受理并立刻收尾，\
                      要么根本不该走到执行。"
                .into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{approved, context, writer};

    #[tokio::test]
    async fn the_plan_carries_the_task_id_and_text() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = FollowTool::new();

        let plan = tool
            .prepare(
                serde_json::json!({ "task_id": "3f2a", "text": "再看看功耗" }),
                &ctx,
            )
            .await
            .unwrap();

        let Operation::Follow { task_id, text } = &plan.operation else {
            panic!("{:?}", plan.operation)
        };
        assert_eq!(task_id, "3f2a");
        assert_eq!(text, "再看看功耗");
        assert_eq!(plan.recovery, RecoveryMode::VerifyTarget);
    }

    /// 带 `#` 的写法也收，井号本身不进计划里的 `task_id`。
    #[tokio::test]
    async fn a_leading_hash_is_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = FollowTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "task_id": "#3f2a", "text": "再看看" }),
                &ctx,
            )
            .await
            .unwrap();
        let Operation::Follow { task_id, .. } = &plan.operation else {
            panic!()
        };
        assert_eq!(task_id, "3f2a");
    }

    #[tokio::test]
    async fn an_empty_task_id_or_text_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = FollowTool::new();

        let error = tool
            .prepare(
                serde_json::json!({ "task_id": " ", "text": "再看看" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments { .. }));

        let error = tool
            .prepare(serde_json::json!({ "task_id": "3f2a", "text": " " }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments { .. }));
    }

    #[tokio::test]
    async fn execute_refuses_because_follow_is_orchestrated_not_executed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = FollowTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "task_id": "3f2a", "text": "再看看" }),
                &ctx,
            )
            .await
            .unwrap();

        let error = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        let ToolError::Failed { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("follow"), "{message}");
    }
}
