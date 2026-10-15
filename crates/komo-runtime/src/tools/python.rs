//! `python`：在受管理解释器里跑代码；**绑定代码版本**（§4、§5.2）。
//!
//! 计划绑定两个版本：代码内容哈希与**环境版本**。依赖一升级，`env_version` 就变，账本里看得出
//! 这次跑在哪个环境上（§7.2）。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, PythonHost, Tool};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{EnvVersion, ExecutionPlan, Operation, PlanVersions, RecoveryMode};
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
            description: "在受管理的解释器里跑 Python：执行任意代码（设 result 返回数据）。要交给操作者的产物写进 $KOMO_ARTIFACT_DIR（本次调用的产出目录）：那底下每个新文件都会被登记，之后可以用 artifact://files/<run>/<文件名> 读回来。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["code"] },
                    "code": { "type": "string", "description": "要执行的代码" }
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
        let args: PythonArgs = parse_args(infer_mode(args), "python")?;
        // 上下文固定了环境版本就用它（一次 Run 里前后两个调用必须绑同一个环境）；
        // 没固定就问宿主。
        let env_version = ctx
            .env_version
            .clone()
            .unwrap_or_else(|| self.host.env_version());

        let PythonJob::Code { code } = &args.job;
        if code.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "code 不能是空串".into(),
            });
        }
        let versions = PlanVersions {
            code: Some(ContentHash::of_str(code)),
            env: Some(env_version),
        };

        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "python".into(),
            operation: Operation::PythonCode,
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![],
            versions,
            // 重启后不从头再跑整个脚本，中断结果交给模型（§8.6）。
            recovery: RecoveryMode::NoSafeRecovery,
            then_run: None,
        })
    }

    async fn execute(
        &self,
        plan: ExecutionPlan,
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let plan = &plan;
        let args: PythonArgs = parse_args(plan.args.clone(), "python")?;

        // 计划绑定的环境版本必须还是当前那个：依赖在计划之后换过，就不是这份计划了。
        let current = self.host.env_version();
        if let Some(planned) = &plan.versions.env
            && planned != &current
        {
            return Err(ToolError::VersionConflict {
                path: format!("Python 环境：计划绑定 {}，当前 {}", planned.0, current.0),
            });
        }

        // 这次调用的产出目录（§4.7）：`<会话内容目录>/artifacts/<run>`。目录由运行时建，
        // 环境变量的名字（`KOMO_ARTIFACT_DIR`）也只在那里出现——这里只把路径算出来。
        // 精简装配（没有会话内容目录）时是 `None`：不建目录、不登记产物，按旧样子跑。
        let artifacts = ctx
            .mounts
            .session_root
            .as_ref()
            .map(|root| root.join("artifacts").join(ctx.run.as_str()));

        // 拿到的 sink 原样交给宿主：脚本的 print 流进去，结构化结果走另一条路（§5.1）。
        let outcome = self
            .host
            .run_with_artifacts(
                args.job,
                artifacts.as_deref(),
                // 时限就是这次调用的活动执行时限，python 自己不另设上限。
                ctx.call_timeout,
                sink,
                ctx.cancel.clone(),
            )
            .await;

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

        // 交给模型的那段预览（§8.3 的投影正文——抬头只有 stdout / stderr 的字节数）。
        //
        // 顺序有讲究：**只 print、不返回结构化结果**是最常见的一种跑法，而原先这里
        // `to_string(&Value::Null)` 得到的是字面量 `null`——模型看到它以为工具坏了，改用
        // `bash` + `python3` 把同一件事重跑一遍（真实会话里就这么白花了两轮）。所以没有
        // 结构化结果时给 stdout 的首尾摘录，再不济也要说清"没有输出"，绝不把 `null` 当正文。
        let preview = match (&outcome.error, outcome.result.is_null()) {
            (Some(error), _) => format!("{:?}：{error}", outcome.status),
            (None, false) => serde_json::to_string(&outcome.result).unwrap_or_default(),
            (None, true) => {
                let excerpt = outcome.stdout_excerpt.trim();
                if excerpt.is_empty() {
                    "没有返回值，也没有输出".to_string()
                } else {
                    format!("没有返回值；这是 stdout 的首尾摘录：\n{excerpt}")
                }
            }
        };
        let result = PythonToolResult {
            status: outcome.status,
            result: outcome.result,
            error: outcome.error,
            env_version: outcome.env_version,
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

/// 模型少写了 `mode` 时按参数形状补上（§5.1）。
///
/// 有 `code` 就是 `mode = "code"`。**这不放松绑定**——计划绑的仍是 `Operation` 与代码版本
/// （§5.2、§7.2），少写一个判别字段不该换来一次失败往返（真实会话里模型就这么白跑了一轮，
/// 报错是 `missing field mode`）。
fn infer_mode(args: serde_json::Value) -> serde_json::Value {
    let Some(object) = args.as_object() else {
        return args;
    };
    if object.contains_key("mode") || !object.contains_key("code") {
        return args;
    }
    let mut object = object.clone();
    object.insert("mode".into(), serde_json::Value::String("code".into()));
    serde_json::Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{context, writer};
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
        assert_eq!(plan.recovery, RecoveryMode::NoSafeRecovery);
    }

    #[tokio::test]
    async fn a_python_call_runs_under_the_call_timeout_not_a_fixed_limit_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.call_timeout = std::time::Duration::from_secs(3600);
        let (host, tool) = wire();
        let plan = tool
            .prepare(
                serde_json::json!({ "mode": "code", "code": "result = 1" }),
                &ctx,
            )
            .await
            .unwrap();
        tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
        assert_eq!(
            host.timeouts(),
            vec![std::time::Duration::from_secs(3600)],
            "python 的时限就是这次调用的活动执行时限，没有自己的 120s"
        );
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
            stdout_excerpt: String::new(),
            env_version: host.env_version(),
        });
        let plan = tool
            .prepare(
                serde_json::json!({ "mode": "code", "code": "result = 1 + 1" }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
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
            .execute(plan, &ctx, &mut writer(&ctx))
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
            .execute(plan, &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        assert!(error.is_uncertain(), "{error:?}");
    }

    /// 少写 `mode` 就按参数形状补上：判别字段写漏一次，不该换来一次失败往返。
    #[tokio::test]
    async fn a_code_call_without_a_mode_is_still_planned_as_code() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, tool) = wire();
        let plan = tool
            .prepare(serde_json::json!({ "code": "result = 1 + 1" }), &ctx)
            .await
            .unwrap();
        assert!(
            matches!(plan.operation, Operation::PythonCode),
            "{:?}",
            plan.operation
        );
        assert_eq!(plan.args["mode"], "code", "计划里补上，哈希之后才稳定");
        assert!(host.calls().is_empty(), "prepare 一行都不执行");
    }

    /// 只 print 的脚本：预览给 stdout 的首尾摘录，**不是字面量 `null`**。
    ///
    /// 真实会话里模型看到 `null` 以为工具坏了，改用 `bash` + `python3` 把同一件事重跑了一遍。
    #[tokio::test]
    async fn a_script_that_only_prints_shows_its_stdout_excerpt() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, tool) = wire();
        host.push_stdout("{\"rows\": [[241653]]}\n");
        host.push_result(PythonResult {
            status: ToolResultStatus::Completed,
            result: serde_json::Value::Null,
            error: None,
            artifacts: vec![],
            stdout_excerpt: String::new(),
            env_version: host.env_version(),
        });
        let plan = tool
            .prepare(
                serde_json::json!({ "mode": "code", "code": "print(1)" }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();

        let preview = output.preview.expect("有预览");
        assert!(preview.contains("[[241653]]"), "{preview}");
        assert!(!preview.contains("null"), "不要拿 `null` 当正文：{preview}");
    }

    /// 既没返回值也没输出：明说，别让模型猜。
    #[tokio::test]
    async fn a_script_with_neither_a_result_nor_output_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, tool) = wire();
        host.push_result(PythonResult {
            status: ToolResultStatus::Completed,
            result: serde_json::Value::Null,
            error: None,
            artifacts: vec![],
            stdout_excerpt: String::new(),
            env_version: host.env_version(),
        });
        let plan = tool
            .prepare(serde_json::json!({ "mode": "code", "code": "x = 1" }), &ctx)
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
        assert_eq!(
            output.preview.as_deref(),
            Some("没有返回值，也没有输出"),
            "空就是空，说清楚"
        );
    }
}
