//! `dispatch`：把一件需要工具的事派进一条**独立的任务会话**（`docs/home-dispatcher.md`
//! §4）。
//!
//! 它和 [`super::delegate`] 是同一类东西——不是第七个基础工具，模型看不见任何新能力，
//! 任务会话里的每一次调用照常过 Policy 与审批——但**目的不同**：`delegate` 是"这件事
//! 我自己等着"，`dispatch` 是"这件事另起一个会话去跑，我不等它"。分发器（home session
//! 冻结出来的那份身份，§3）一轮就该结束，等它跑完就又把 home 堵住了，正是这次改造要
//! 解决的问题（§1）。
//!
//! 两件**不在这里**发生的事：
//!
//! - **执行不在工具里。** 建任务会话、提交第一条输入，全在 executor（`Operation::Dispatch`
//!   的分流），因为那要一份 [`komo_kernel::traits::TaskSpawner`]——工具只有
//!   [`komo_kernel::types::tool::ToolContext`]，够不到它。
//! - **不等任务跑完。** `dispatch` 提交成功就收尾（不返回 `RoundStop::Dependency`），
//!   结果由任务会话自己的 watcher 投回消息来源的渠道。

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode};
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::{normalized, parse_args, plan_time};

/// 模型给的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchArgs {
    /// 自包含的任务描述：**任务会话看不到 home 的对话**，用户的原话与看板里相关任务
    /// 的结论都要写进去（§6）。
    pub task: String,
    /// 给人看的标题（建议 ≤ 30 字），建会话时写死。
    pub title: String,
}

#[derive(Debug)]
pub struct DispatchTool;

impl Default for DispatchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DispatchTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for DispatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "dispatch".into(),
            description: "派一个新任务：建一个独立的任务会话去做需要读文件、跑命令、查设备、\
                          写东西的事，你自己不用等它跑完。任务会话**看不到这段对话**，所以 \
                          task 要自包含——把用户的原话和看板里相关任务的结论都写进去。\
                          提交成功就立刻回一句\"已派出 #短号：标题\"，结果会由那个任务会话\
                          自己投回消息来源。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "交给任务会话的自包含任务；它看不到这段对话，所以别写\"见上文\""
                    },
                    "title": {
                        "type": "string",
                        "description": "给人看的标题，建议不超过 30 字"
                    }
                },
                "required": ["task", "title"],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: DispatchArgs = parse_args(args, "dispatch")?;
        if args.task.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                message:
                    "task 不能是空的：任务会话只看得见它，空任务等于派出去一个什么都做不了的会话"
                        .into(),
            });
        }
        if args.title.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "title 不能是空的：它是任务在看板与回执里的名字".into(),
            });
        }

        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "dispatch".into(),
            operation: Operation::Dispatch {
                task: args.task.clone(),
                title: args.title.clone(),
            },
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            // 派任务不碰任何文件：它触及的目标是"一个新会话"，不是路径。
            targets: vec![],
            versions: PlanVersions::default(),
            resources: vec![],
            // 核对对象是我们自己的账本（幂等的请求键有没有对应的 Run），与 `delegate`
            // 同一类（§8.6 的"可以核对目标状态"）。
            recovery: RecoveryMode::VerifyTarget,
        })
    }

    async fn execute(
        &self,
        _plan: komo_kernel::types::plan::ApprovedPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        // 走到这里说明有人把 dispatch 当成普通工具执行了——它和 `delegate` 一样只由
        // executor 的编排分流处理，这条路径不该被走到。
        Err(ToolError::Failed {
            message: "dispatch 不由工具执行：一次派任务要么被 executor 的编排受理并立刻收尾，\
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
    async fn the_plan_carries_the_task_and_title() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = DispatchTool::new();

        let plan = tool
            .prepare(
                serde_json::json!({ "task": "查一下空调状态", "title": "查空调" }),
                &ctx,
            )
            .await
            .unwrap();

        let Operation::Dispatch { task, title } = &plan.operation else {
            panic!("{:?}", plan.operation)
        };
        assert_eq!(task, "查一下空调状态");
        assert_eq!(title, "查空调");
        assert!(plan.targets.is_empty());
        assert_eq!(plan.recovery, RecoveryMode::VerifyTarget);
    }

    #[tokio::test]
    async fn an_empty_task_or_title_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = DispatchTool::new();

        let error = tool
            .prepare(serde_json::json!({ "task": "  ", "title": "标题" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments { .. }));

        let error = tool
            .prepare(serde_json::json!({ "task": "干活", "title": " " }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments { .. }));
    }

    /// dispatch 的执行在编排里，这条调用路径不该存在。
    #[tokio::test]
    async fn execute_refuses_because_dispatch_is_orchestrated_not_executed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = DispatchTool::new();
        let plan = tool
            .prepare(serde_json::json!({ "task": "干活", "title": "标题" }), &ctx)
            .await
            .unwrap();

        let error = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        let ToolError::Failed { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("dispatch"), "{message}");
    }

    /// 计划哈希覆盖 task / title：内容变了，审批（若有）不能绑定到旧哈希上。
    #[tokio::test]
    async fn the_plan_hash_changes_when_the_task_text_changes() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = DispatchTool::new();

        let a = tool
            .prepare(
                serde_json::json!({ "task": "查一下空调状态", "title": "查空调" }),
                &ctx,
            )
            .await
            .unwrap();
        let b = tool
            .prepare(
                serde_json::json!({ "task": "查一下热水器状态", "title": "查空调" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_ne!(a.plan_hash(), b.plan_hash());
    }
}
