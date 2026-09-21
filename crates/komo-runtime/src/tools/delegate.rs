//! `delegate`：把一件自包含的子任务交给**同 Session 里的一条子 Run**（§4）。
//!
//! 它长得像第七个工具，其实不是：模型看不见任何新能力，子代理用的是同一套六个工具，它
//! 自己的每一次调用照常过 Policy 与审批。所以这里要审的不是"模型能不能做这件事"，而是
//! "允不允许把这件事派出去"——计划因此是 `Operation::Delegate`，规则表按它匹配。
//!
//! 两件**不在这里**发生的事，写下来是为了让读的人不必去别处确认：
//!
//! - **执行不在工具里。** 受理子 Run、父调用停在 `dependency` 上、子 Run 终态回来后
//!   复验，全在 executor（§8.4 的 `dependency`）。理由不是省事：父这一次调用什么时候
//!   算收尾，只有账本说得清——子 Run 可能正在等审批、可能被重启恢复、可能过了很久才
//!   结束，而父侧手里唯一那份能对上的东西是随计划进了审批绑定对象的那份 [`DelegateSpec`]。
//!   所以 [`DelegateTool::execute`] 一旦被走到就是一条不该存在的路径，它返回硬错误。
//! - **子代理看不见父的消息历史。** 它的输入就是 `task` 这一条正文——描述里那句"子代理
//!   只看得见 task"是契约的一部分，不是修辞。

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::delegate::{DelegateSpec, SchemaMode};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode};
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::{normalized, parse_args, plan_time};

/// 模型给的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateArgs {
    /// 交给子代理的自包含任务。**它就是子 Run 的输入正文**，也是审批卡上给人看的那一
    /// 句——两者是同一个值，所以这里不能是"见上文"。
    pub task: String,
    /// 结果契约：子代理要交回来的 JSON 长什么样（kernel 认得的那个 Schema 子集）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// 不合规时怎么办。缺省 permissive：父侧照样拿到结果，但**标记出来**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_mode: Option<SchemaMode>,
    /// 子代理能跑几轮模型。缺省用 kernel 的 [`DEFAULT_DELEGATE_ROUNDS`]。
    ///
    /// [`DEFAULT_DELEGATE_ROUNDS`]: komo_kernel::types::delegate::DEFAULT_DELEGATE_ROUNDS
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rounds: Option<u32>,
}

#[derive(Debug)]
pub struct DelegateTool;

impl Default for DelegateTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DelegateTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "delegate".into(),
            description: "把一件自包含的子任务交给一条子代理去跑，你自己停在等待上，它结束后\
                          结果会回到你这里。子代理**只看得见 task 这一段正文**，看不见这次会话\
                          的历史，所以任务必须写全：要做什么、在哪个目录、做到什么算完成。\
                          它做完之后，结果放在**最后一条回复**里——给了 output_schema 就交一个\
                          符合它的 JSON 对象（可以用 ```json 围栏包起来），没给就给自由文本。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "交给子代理的自包含任务；它只看得见这一段，所以别写\"见上文\""
                    },
                    "output_schema": {
                        "type": "object",
                        "description": "结果契约（JSON Schema 的子集：type / required / properties / \
                                        items / enum / additionalProperties）。别用 oneOf、$ref 一类\
                                        这里不检查的关键字，它们不会被校验"
                    },
                    "schema_mode": {
                        "type": "string",
                        "enum": ["permissive", "strict"],
                        "description": "结果不符合契约时怎么办：permissive（默认）放行并标记，\
                                        strict 判这次委派失败"
                    },
                    "rounds": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "子代理最多跑几轮模型，默认 8"
                    }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: DelegateArgs = parse_args(args, "delegate")?;
        if args.task.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                message:
                    "task 不能是空的：子代理只看得见它，空任务等于派出去一个什么都做不了的 Run"
                        .into(),
            });
        }
        if args.rounds == Some(0) {
            return Err(ToolError::InvalidArguments {
                message: "rounds 至少是 1".into(),
            });
        }
        // 模式是"结果不合契约时怎么办"，没有契约就没有可违反的东西。静默丢掉这个参数
        // 会让模型以为它设定了什么——说出来。
        if args.schema_mode.is_some() && args.output_schema.is_none() {
            return Err(ToolError::InvalidArguments {
                message: "schema_mode 只在同时给了 output_schema 时才有意义".into(),
            });
        }

        let mut spec = DelegateSpec::new(ctx.run.clone(), ctx.call.clone(), args.task.clone());
        if let Some(schema) = args.output_schema.clone() {
            spec = spec.with_contract(schema, args.schema_mode.unwrap_or_default());
        }
        if let Some(rounds) = args.rounds {
            spec = spec.with_rounds(rounds);
        }

        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "delegate".into(),
            operation: Operation::Delegate { spec },
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            // 委派不碰任何文件：它触及的目标是"一条子 Run"，而 Run 不是路径。审批绑定
            // 的是这份计划（含整份 spec），不是某个文件版本。
            targets: vec![],
            versions: PlanVersions::default(),
            resources: vec![],
            // 子 Run 的终态就在我们自己的账本里，所以"重做安不安全"这件事有确定答案：
            // 核对目标状态（§8.6 的第二行），而不是重派一次。
            recovery: RecoveryMode::VerifyTarget,
        })
    }

    async fn execute(
        &self,
        _plan: komo_kernel::types::plan::ApprovedPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        // 走到这里说明有人把委派当成普通工具执行了。**不能返回一个"没做事但成功"的空
        // 结果**：父侧会拿到一个假的成功，而子 Run 从来没被受理过——账本上什么都查不到，
        // 事后只能靠猜。硬错误至少指向了漏掉 `Operation::Delegate` 分流的那一处。
        Err(ToolError::Failed {
            message: "delegate 不由工具执行：一次委派要么被判成子 Run 并让父调用进入等待\
                      （executor 的编排），要么根本不该走到执行。"
                .into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{approved, context, writer};

    #[tokio::test]
    async fn the_plan_carries_who_delegated_and_what_the_result_looks_like() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = DelegateTool::new();

        let plan = tool
            .prepare(
                serde_json::json!({
                    "task": "把 a.txt 里的小数点都改成逗号",
                    "output_schema": {
                        "type": "object",
                        "required": ["changed"],
                        "properties": { "changed": { "type": "integer" } }
                    },
                    "rounds": 3
                }),
                &ctx,
            )
            .await
            .unwrap();

        let Operation::Delegate { spec } = &plan.operation else {
            panic!("{:?}", plan.operation)
        };
        assert_eq!(spec.parent, ctx.run);
        assert_eq!(spec.call, ctx.call);
        assert_eq!(spec.task, "把 a.txt 里的小数点都改成逗号");
        assert_eq!(spec.rounds, 3);
        assert_eq!(spec.contract.as_ref().unwrap().mode, SchemaMode::Permissive);
        // 派出去这件事没有路径目标；恢复方式说得出来："核对子 Run 的终态"。
        assert!(plan.targets.is_empty());
        assert_eq!(plan.recovery, RecoveryMode::VerifyTarget);
    }

    #[tokio::test]
    async fn without_a_schema_the_contract_is_absent_and_the_budget_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let tool = DelegateTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "task": "看一下这个 PR" }),
                &context(dir.path()),
            )
            .await
            .unwrap();
        let Operation::Delegate { spec } = &plan.operation else {
            panic!()
        };
        assert_eq!(
            spec.rounds,
            komo_kernel::types::delegate::DEFAULT_DELEGATE_ROUNDS
        );
        assert!(spec.contract.is_none());
    }

    #[tokio::test]
    async fn an_empty_task_is_refused_with_something_the_model_can_act_on() {
        let dir = tempfile::tempdir().unwrap();
        let error = DelegateTool::new()
            .prepare(serde_json::json!({ "task": "   " }), &context(dir.path()))
            .await
            .unwrap_err();
        let ToolError::InvalidArguments { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("task"), "{message}");
    }

    /// 静默丢掉一个参数会让模型以为自己设定了什么。
    #[tokio::test]
    async fn a_schema_mode_without_a_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let error = DelegateTool::new()
            .prepare(
                serde_json::json!({ "task": "干活", "schema_mode": "strict" }),
                &context(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error:?}"
        );

        let zero = DelegateTool::new()
            .prepare(
                serde_json::json!({ "task": "干活", "rounds": 0 }),
                &context(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(zero, ToolError::InvalidArguments { .. }),
            "{zero:?}"
        );
    }

    /// 委派的执行在编排里。这条调用路径不该存在，而且必须**明显失败**——一个"成功但
    /// 什么都没做"的结果会让父侧拿到假的成功，账本上却查不到任何子 Run。
    #[tokio::test]
    async fn execute_refuses_because_delegation_is_orchestrated_not_executed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = DelegateTool::new();
        let plan = tool
            .prepare(serde_json::json!({ "task": "干活" }), &ctx)
            .await
            .unwrap();

        let error = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        let ToolError::Failed { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("子 Run"), "{message}");
    }
}
