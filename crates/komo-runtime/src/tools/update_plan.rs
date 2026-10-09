//! `update_plan`：模型报一次工作计划（移植自 SoL-Pi 的 Online Context Compact）。
//!
//! 与 [`super::delegate`] / [`super::dispatch`] 同一类编排操作，不是第七个基础工具：它
//! 不碰文件、进程或外部服务，没有副作用。计划不另存一份——整份计划就在 `tool.planned`
//! 的 `args` 里，这条 Run 现在的计划是从日志折出来的
//! （[`komo_kernel::compaction::online_state`]）。
//!
//! **执行不在工具里**：拿上一版计划要读这条 Run 的日志，工具只有
//! [`ToolContext`]，够不到账本——比较前后两版、写结果都在 executor 的编排分流里。

use async_trait::async_trait;
use komo_kernel::compaction::PlanUpdate;
use komo_kernel::compaction::plan::{
    MAX_PLAN_STEPS, MAX_PROGRESS_ITEM_BYTES, MAX_PROGRESS_ITEMS, MAX_STEP_GOAL_BYTES,
    MAX_STEP_ID_BYTES,
};
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode};
use komo_kernel::types::refs::INLINE_ARGUMENT_LIMIT_BYTES;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};

use super::{normalized, parse_args, plan_time};

pub const NAME: &str = "update_plan";

#[derive(Debug, Default)]
pub struct UpdatePlanTool;

impl UpdatePlanTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for UpdatePlanTool {
    fn definition(&self) -> ToolDefinition {
        let items = serde_json::json!({
            "type": "array",
            "maxItems": MAX_PROGRESS_ITEMS,
            "items": { "type": "string", "maxLength": MAX_PROGRESS_ITEM_BYTES }
        });
        ToolDefinition {
            name: NAME.into(),
            description: "更新这次任务的工作计划。多步的事先登记计划，之后每推进一步再调一次；\
                          没有副作用，不用审批。规则：每次都发**整份**计划，不是增量；步骤 id \
                          保持不变，先登记、做完再标 completed（一上来就标 completed 的新步骤\
                          只算补记历史）；同一时间最多一个 in_progress；标 completed 时在 \
                          progress 里写上改了哪些文件、怎么验证的、做了什么决定。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_PLAN_STEPS,
                        "description": "整份计划，按顺序",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "maxLength": MAX_STEP_ID_BYTES,
                                    "description": "稳定的步骤 id，前后几次更新里同一步用同一个"
                                },
                                "goal": {
                                    "type": "string",
                                    "maxLength": MAX_STEP_GOAL_BYTES,
                                    "description": "这一步要做成什么，一句话"
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["id", "goal", "status"],
                            "additionalProperties": false
                        }
                    },
                    "progress": {
                        "type": "object",
                        "description": "完成一步时附带的证据",
                        "properties": {
                            "files_changed": items,
                            "verification": items,
                            "decisions": items
                        },
                        "required": ["files_changed", "verification", "decisions"],
                        "additionalProperties": false
                    }
                },
                "required": ["steps"],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let update: PlanUpdate = parse_args(args, NAME)?;
        update
            .validate()
            .map_err(|error| ToolError::InvalidArguments {
                message: format!("{NAME} 的计划不合法：{error}"),
            })?;

        let plan = ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: NAME.into(),
            operation: Operation::UpdatePlan,
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&update)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![],
            versions: PlanVersions::default(),
            resources: vec![],
            // 没有副作用：started 而没有结果的那次，再收一次尾就是了，不必核对。
            recovery: RecoveryMode::SafeReread,
        };
        // 计划要整份内联进 `tool.planned`：外置了，从日志折在线状态的那一步读不到它，
        // 等于这次更新没发生。参数上限之外再按账本真正比的那个长度兜一次底。
        let encoded = serde_json::to_vec(&plan).map_or(usize::MAX, |bytes| bytes.len());
        if encoded > INLINE_ARGUMENT_LIMIT_BYTES {
            return Err(ToolError::InvalidArguments {
                message: format!(
                    "{NAME} 的计划太大（{encoded} 字节，最多 {INLINE_ARGUMENT_LIMIT_BYTES} 字节）：\
                     把步骤目标写短一点"
                ),
            });
        }
        Ok(plan)
    }

    async fn execute(
        &self,
        _plan: komo_kernel::types::plan::ApprovedPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed {
            message: "update_plan 不由工具执行：它由 executor 的编排分流读日志、比较前后两版计划。"
                .into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{approved, context, writer};

    #[tokio::test]
    async fn the_plan_is_inline_args_with_a_safe_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let plan = UpdatePlanTool::new()
            .prepare(
                serde_json::json!({
                    "steps": [{ "id": "a", "goal": "读代码", "status": "in_progress" }],
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(plan.operation, Operation::UpdatePlan);
        assert_eq!(plan.recovery, RecoveryMode::SafeReread);
        assert!(plan.targets.is_empty());
        let update: PlanUpdate = serde_json::from_value(plan.args).unwrap();
        assert_eq!(update.steps[0].id, "a");
    }

    #[tokio::test]
    async fn malformed_or_invalid_plans_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        for args in [
            serde_json::json!({ "steps": [] }),
            serde_json::json!({ "steps": [{ "id": "a", "goal": "g", "status": "done" }] }),
            serde_json::json!({
                "steps": [
                    { "id": "a", "goal": "一", "status": "pending" },
                    { "id": "a", "goal": "二", "status": "pending" },
                ],
            }),
        ] {
            let error = UpdatePlanTool::new()
                .prepare(args.clone(), &ctx)
                .await
                .unwrap_err();
            assert!(
                matches!(error, ToolError::InvalidArguments { .. }),
                "{args}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn execute_refuses_because_update_plan_is_orchestrated() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = UpdatePlanTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "steps": [{ "id": "a", "goal": "g", "status": "pending" }] }),
                &ctx,
            )
            .await
            .unwrap();
        let error = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Failed { .. }), "{error:?}");
    }
}
