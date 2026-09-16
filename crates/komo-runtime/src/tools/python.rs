//! `python`：代码或已保存模块调用；使用受管理解释器，**绑定代码版本**（§4、§5.2）。
//!
//! 两种形式属于**同一个工具**：`mode = "code"` 是任意代码，`mode = "call"` 是 toolbox
//! 里明确导出的函数。它们的 [`Operation`] 不同，所以 Policy 分得开——「同意一次
//! Python」不会变成「今后任意脚本均可执行」。任意 code 也不会因为 import 了已审核模块
//! 就自动获得同样授权：授权匹配的是操作与版本，不是它引用了谁。
//!
//! 计划绑定两个版本：代码内容哈希（或模块名与版本）与**环境版本**。依赖一升级，
//! `env_version` 就变，绑定旧环境的授权覆盖不到新计划（§5.4）。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, PythonHost, Tool};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{
    ApprovedPlan, EnvVersion, ExecutionPlan, Operation, PlanVersions, RecoveryMode,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{
    PyError, PythonJob, ToolContext, ToolDefinition, ToolError, ToolOutput,
};
use serde::{Deserialize, Serialize};

use super::{normalized, parse_args, plan_time};

/// 模型给的参数就是一个 [`PythonJob`]；`version` 由 prepare 补。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PythonArgs {
    #[serde(flatten)]
    pub job: PythonJob,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PythonToolResult {
    pub status: ToolResultStatus,
    #[serde(default)]
    pub result: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub env_version: EnvVersion,
}

pub struct PythonTool {
    host: Arc<dyn PythonHost>,
}

impl std::fmt::Debug for PythonTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonTool")
            .field("env_version", &self.host.env_version())
            .finish_non_exhaustive()
    }
}

impl PythonTool {
    pub fn new(host: Arc<dyn PythonHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl Tool for PythonTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "python".into(),
            description: "在受管理的解释器里跑 Python：mode=code 执行任意代码（设 result 返回数据），mode=call 调用 toolbox 中已导出的函数。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["code", "call"] },
                    "code": { "type": "string", "description": "mode=code：要执行的代码" },
                    "module": { "type": "string", "description": "mode=call：模块名，例如 toolbox.ha" },
                    "function": { "type": "string", "description": "mode=call：模块 __all__ 里导出的函数名" },
                    "args": { "type": "object", "description": "mode=call：关键字参数" }
                },
                "required": ["mode"],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: PythonArgs = parse_args(args, "python")?;
        // 上下文固定了环境版本就用它（一次 Run 里前后两个调用必须绑同一个环境）；
        // 没固定就问宿主。
        let env_version = ctx
            .env_version
            .clone()
            .unwrap_or_else(|| self.host.env_version());

        let (operation, versions) = match &args.job {
            PythonJob::Code { code } => {
                if code.trim().is_empty() {
                    return Err(ToolError::InvalidArguments {
                        message: "code 不能是空串".into(),
                    });
                }
                (
                    Operation::PythonCode,
                    PlanVersions {
                        code: Some(ContentHash::of_str(code)),
                        module: None,
                        env: Some(env_version),
                    },
                )
            }
            PythonJob::Call {
                module, function, ..
            } => {
                if module.trim().is_empty() || function.trim().is_empty() {
                    return Err(ToolError::InvalidArguments {
                        message: "call 模式要 module 与 function".into(),
                    });
                }
                if function.starts_with('_') {
                    return Err(ToolError::InvalidArguments {
                        message: format!("{module}.{function} 不是导出函数"),
                    });
                }
                (
                    Operation::PythonCall {
                        module: module.clone(),
                        function: function.clone(),
                    },
                    PlanVersions {
                        code: None,
                        // TODO(decide: toolbox 的版本登记属于 §5.4 的保存 / 审核流程，
                        // 那一波还没做。在它落地前这里留空——留空意味着"没有绑定模块
                        // 版本"，而一条绑定了版本的授权在 `versions_cover` 下**不会**
                        // 覆盖没有版本的计划，所以保守方向是安全的那一侧)。
                        module: None,
                        env: Some(env_version),
                    },
                )
            }
        };

        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "python".into(),
            operation,
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![],
            versions,
            resources: vec![],
            // TODO(decide: §8.6「已保存的 Python 模块可提供与版本绑定的核对函数」——
            // 那要 toolbox 的版本与审核流程（§5.4），属于后面的阶段。在它之前，
            // 两种模式都没有经过验证的恢复方式，停在 needs_attention 是保守的那一侧)。
            recovery: RecoveryMode::NoSafeRecovery,
        })
    }

    async fn execute(
        &self,
        plan: ApprovedPlan,
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let plan = plan.plan();
        let args: PythonArgs = parse_args(plan.args.clone(), "python")?;

        // 计划绑定的环境版本必须还是当前那个：依赖在审批期间换过，就不是这份计划了。
        let current = self.host.env_version();
        if let Some(planned) = &plan.versions.env
            && planned != &current
        {
            return Err(ToolError::VersionConflict {
                path: format!("Python 环境：计划绑定 {}，当前 {}", planned.0, current.0),
            });
        }

        // 拿到的 sink 原样交给宿主：脚本的 print 流进去，结构化结果走另一条路（§5.1）。
        let outcome = self.host.run(args.job, sink, ctx.cancel.clone()).await;

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(PyError::Cancelled) => return Err(ToolError::Cancelled),
            Err(PyError::Timeout { after_secs }) => return Err(ToolError::Timeout { after_secs }),
            Err(PyError::EnvVersionMismatch { planned, current }) => {
                return Err(ToolError::VersionConflict {
                    path: format!("Python 环境：计划绑定 {planned}，当前 {current}"),
                });
            }
            Err(PyError::Protocol(message)) => {
                // 解释器没写下结论：副作用发生没发生不知道，**不能当成失败重试掉**
                // （§6、§8.6）。
                return Err(ToolError::Uncertain { message });
            }
            Err(PyError::Spawn(message)) => {
                return Err(ToolError::Failed {
                    message: format!("解释器起不来：{message}"),
                });
            }
            Err(PyError::Failed(message)) => return Err(ToolError::Failed { message }),
        };

        let result = PythonToolResult {
            status: outcome.status,
            result: outcome.result,
            error: outcome.error,
            env_version: outcome.env_version,
        };
        let preview = match &result.error {
            Some(error) => format!("{:?}：{error}", result.status),
            None => serde_json::to_string(&result.result).unwrap_or_default(),
        };
        Ok(ToolOutput {
            status: result.status,
            result: serde_json::to_value(&result).map_err(|error| ToolError::Failed {
                message: error.to_string(),
            })?,
            exit_code: None,
            artifacts: outcome.artifacts,
            preview: Some(preview),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{approved, context, writer};
    use komo_kernel::test_support::FakePythonHost;
    use komo_kernel::types::tool::PythonResult;

    fn wire() -> (Arc<FakePythonHost>, PythonTool) {
        let host = Arc::new(FakePythonHost::new());
        let tool = PythonTool::new(host.clone());
        (host, tool)
    }

    #[tokio::test]
    async fn code_mode_binds_the_code_hash_and_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, tool) = wire();
        let plan = tool
            .prepare(
                serde_json::json!({ "mode": "code", "code": "result = 1 + 1" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(plan.operation, Operation::PythonCode);
        assert_eq!(
            plan.versions.code,
            Some(ContentHash::of_str("result = 1 + 1"))
        );
        assert_eq!(plan.versions.env, Some(host.env_version()));
    }

    #[tokio::test]
    async fn call_mode_is_a_different_operation_so_policy_can_tell_them_apart() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (_host, tool) = wire();
        let plan = tool
            .prepare(
                serde_json::json!({
                    "mode": "call", "module": "toolbox.ha", "function": "turn_off",
                    "args": { "entity_id": "light.living_room" }
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            plan.operation,
            Operation::PythonCall {
                module: "toolbox.ha".into(),
                function: "turn_off".into()
            }
        );
        assert!(plan.versions.code.is_none(), "call 模式没有代码正文");
    }

    #[tokio::test]
    async fn a_private_function_is_refused_before_it_reaches_the_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, tool) = wire();
        let error = tool
            .prepare(
                serde_json::json!({ "mode": "call", "module": "toolbox.ha", "function": "_secret" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error:?}"
        );
        assert!(host.calls().is_empty(), "prepare 不执行任何东西");
    }

    #[tokio::test]
    async fn the_job_reaches_the_host_and_the_result_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, tool) = wire();
        host.push_result(PythonResult {
            status: ToolResultStatus::Completed,
            result: serde_json::json!(2),
            error: None,
            artifacts: vec![],
            env_version: host.env_version(),
        });
        let plan = tool
            .prepare(
                serde_json::json!({ "mode": "code", "code": "result = 1 + 1" }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap();
        assert_eq!(output.status, ToolResultStatus::Completed);
        assert_eq!(
            host.calls(),
            vec![PythonJob::Code {
                code: "result = 1 + 1".into()
            }]
        );
    }

    #[tokio::test]
    async fn an_environment_that_moved_since_the_plan_is_a_version_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, tool) = wire();
        let plan = tool
            .prepare(
                serde_json::json!({ "mode": "code", "code": "result = 1" }),
                &ctx,
            )
            .await
            .unwrap();

        host.set_env_version("py-test-2");
        let error = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::VersionConflict { .. }),
            "{error:?}"
        );
        assert!(host.calls().is_empty(), "版本对不上就根本不执行");
    }

    #[tokio::test]
    async fn a_host_that_lost_the_result_is_uncertain_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, tool) = wire();
        host.push_error(PyError::Protocol("解释器没有写下结果".into()));
        let plan = tool
            .prepare(
                serde_json::json!({ "mode": "code", "code": "result = 1" }),
                &ctx,
            )
            .await
            .unwrap();
        let error = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        assert!(error.is_uncertain(), "{error:?}");
    }

    #[tokio::test]
    async fn python_has_no_verification_yet_so_it_lands_on_a_human() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (_host, tool) = wire();
        let plan = tool
            .prepare(
                serde_json::json!({ "mode": "code", "code": "result = 1" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(plan.recovery, RecoveryMode::NoSafeRecovery);
        assert_eq!(
            tool.verify(&plan, &ctx).await.unwrap(),
            komo_kernel::types::plan::Verification::Unavailable
        );
    }
}
