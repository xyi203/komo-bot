//! `python`：代码或已保存模块调用；使用受管理解释器，**绑定代码版本**（§4、§5.2）。
//!
//! 两种形式属于**同一个工具**：`mode = "code"` 是任意代码，`mode = "call"` 是 toolbox
//! 里明确导出的函数。它们的 [`Operation`] 不同：`call` 只调已启用模块里导出的名字，
//! 执行前核对模块版本还是计划绑的那一版。
//!
//! 计划绑定两个版本：代码内容哈希（或模块名与版本）与**环境版本**。依赖一升级，
//! `env_version` 就变，账本里看得出这次跑在哪个环境上（§5.4）。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, PythonHost, Tool};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{
    EnvVersion, ExecutionPlan, Operation, PlanVersions, RecoveryMode, ResourceRef,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{
    PyError, PythonJob, ToolContext, ToolDefinition, ToolError, ToolOutput,
};
use serde::{Deserialize, Serialize};

use crate::toolbox::{Toolbox, ToolboxError, dotted};

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
    /// 已保存的能力。`None` = 这台 Gateway 没有 toolbox，于是 `call` 模式无从解析
    /// 版本，只能拒绝——**不退化成"不绑版本地调一下试试"**（§5.4）。
    toolbox: Option<Arc<Toolbox>>,
}

impl std::fmt::Debug for PythonTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonTool")
            .field("env_version", &self.host.env_version())
            .field(
                "toolbox",
                &self
                    .toolbox
                    .as_ref()
                    .map(|t| t.layout().root().to_path_buf()),
            )
            .finish_non_exhaustive()
    }
}

impl PythonTool {
    pub fn new(host: Arc<dyn PythonHost>) -> Self {
        Self {
            host,
            toolbox: None,
        }
    }

    pub fn with_toolbox(mut self, toolbox: Arc<Toolbox>) -> Self {
        self.toolbox = Some(toolbox);
        self
    }

    /// 这份计划绑的模块版本还是**现在已启用的那一版**吗（§7.2）。
    ///
    /// `code` 模式没有模块可言，直接过。`call` 模式有三种不通过，**都是版本冲突**：
    /// 没有 toolbox（证不出这是已启用的那一版）、模块解析不出来了（停用 / 删掉 / 不再
    /// 导出这个函数）、以及版本真的变了。一律**在 spawn 之前**返回，所以子进程一个都
    /// 不起。
    fn module_still_matches(&self, plan: &ExecutionPlan) -> Result<(), ToolError> {
        let Operation::PythonCall { module, function } = &plan.operation else {
            return Ok(());
        };
        let conflict = |detail: String| {
            Err(ToolError::VersionConflict {
                path: format!("{module}：{detail}"),
            })
        };
        let Some(toolbox) = &self.toolbox else {
            return conflict("这台 Gateway 没有 toolbox，证不出这是已启用的那一版".into());
        };
        match toolbox.resolve_call(module, function) {
            Ok(resolved) if plan.versions.module.as_deref() == Some(resolved.version.as_str()) => {
                Ok(())
            }
            Ok(resolved) => conflict(format!(
                "计划绑定 {}，现在已启用的是 {}",
                plan.versions.module.as_deref().unwrap_or("（未绑定）"),
                resolved.version
            )),
            // 模块在计划之后被停用 / 删掉 / 改得不再导出这个函数。原因原样转述——
            // 模型下一步要做什么取决于是哪一种。
            Err(error) => conflict(error.to_string()),
        }
    }
}

#[async_trait]
impl Tool for PythonTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "python".into(),
            description: "在受管理的解释器里跑 Python：mode=code 执行任意代码（设 result 返回数据），mode=call 调用 toolbox 中已导出的函数。要交给操作者的产物写进 $KOMO_ARTIFACT_DIR（本次调用的产出目录）：那底下每个新文件都会被登记，之后可以用 artifact://files/<run>/<文件名> 读回来。"
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
        let args: PythonArgs = parse_args(infer_mode(args), "python")?;
        // 上下文固定了环境版本就用它（一次 Run 里前后两个调用必须绑同一个环境）；
        // 没固定就问宿主。
        let env_version = ctx
            .env_version
            .clone()
            .unwrap_or_else(|| self.host.env_version());

        let (operation, versions, resources, recovery) = match &args.job {
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
                    vec![],
                    // 重启后不从头再跑整个脚本，中断结果交给模型（§8.6）。
                    RecoveryMode::NoSafeRecovery,
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
                // **解析已启用版本**。它是静态读的（`toolbox::source`）：`prepare` 不许
                // 通过 import 提前跑未审核的代码（§7.3）。候选、停用、没声明 `__all__`、
                // 没导出这个名字——四种都在这里被挡下来，一行 Python 都没跑过。
                let toolbox = self.toolbox.as_ref().ok_or_else(|| ToolError::Denied {
                    reason: "这台 Gateway 没有 toolbox：call 模式无从确定已启用的版本".into(),
                })?;
                let resolved = toolbox.resolve_call(module, function).map_err(refusal_of)?;
                (
                    Operation::PythonCall {
                        // 规范化成 `toolbox.<module>`：账本里同一个模块只有一种写法，
                        // `toolbox.memos` 与 `memos` 不会被当成两个模块。
                        module: dotted(&resolved.module),
                        function: function.clone(),
                    },
                    PlanVersions {
                        code: None,
                        // 计划绑定的就是它（§7.2）：模块一升级，版本就变，绑旧版本的
                        // 计划执行前按版本冲突拒（§5.4）。
                        module: Some(resolved.version.to_string()),
                        env: Some(env_version),
                    },
                    // 凭证**引用**：变量名进计划，值不进（§5.3、§7.2）。
                    resolved
                        .env
                        .iter()
                        .map(|name| ResourceRef {
                            name: dotted(&resolved.module),
                            endpoint: None,
                            credential_env: Some(name.clone()),
                        })
                        .collect(),
                    // 模块自称幂等不算：重启后同样不重跑，中断结果交给模型（§8.6）。
                    RecoveryMode::NoSafeRecovery,
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
            resources,
            recovery,
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

        // 模块版本也一样：执行前再校验一次目标和版本（§7.2）。
        //
        // 计划与执行之间（恢复续跑时可能隔得很久）`komo toolbox enable` 可以把模块换掉。
        // 换过之后，这份计划里的一切——导出的函数、参数的含义、`resources` 里那串凭证
        // 引用——说的都是**上一版**的事。
        //
        // 交回模型的是一个**版本冲突**，和 `write` / `edit` 覆盖到别人改过的文件是同一类：
        // 它会重新 `prepare` → 新计划绑新版本（§5.4）。
        self.module_still_matches(plan)?;

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
        // `shell` + `python3` 把同一件事重跑一遍（真实会话里就这么白花了两轮）。所以没有
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

/// toolbox 的拒绝**作为结果交回模型**（§7.4 最后一段）：名字写错了是参数问题，
/// "还没启用"是权限 / 流程问题——分开说，模型的下一步不一样。
fn refusal_of(error: ToolboxError) -> ToolError {
    match error {
        ToolboxError::BadName(_)
        | ToolboxError::NoSuchModule { .. }
        | ToolboxError::NotExported { .. }
        | ToolboxError::NoExports { .. } => ToolError::InvalidArguments {
            message: error.to_string(),
        },
        ToolboxError::NotEnabled { .. } => ToolError::Denied {
            reason: format!("{error}；先跑候选测试并让操作者启用（komo toolbox enable）"),
        },
        other => ToolError::Failed {
            message: other.to_string(),
        },
    }
}

/// 模型少写了 `mode` 时按参数形状补上（§5.1）。
///
/// 两种形式的参数天然分得开：有 `code` 是任意代码，有 `module` / `function` 是已保存模块的
/// 调用。**这不放松绑定**——计划绑的仍是 `Operation` 与代码 / 模块版本（§5.2、§7.2），少写
/// 一个判别字段不该换来一次失败往返（真实会话里模型就这么白跑了一轮，报错是
/// `missing field mode`）。两个都写了或者都没写就让它照常报错——那种形状是模型自己没说清，
/// 别替它猜。
fn infer_mode(args: serde_json::Value) -> serde_json::Value {
    let Some(object) = args.as_object() else {
        return args;
    };
    if object.contains_key("mode") {
        return args;
    }
    let mode = match (
        object.contains_key("code"),
        object.contains_key("module") || object.contains_key("function"),
    ) {
        (true, false) => "code",
        (false, true) => "call",
        _ => return args,
    };
    let mut object = object.clone();
    object.insert("mode".into(), serde_json::Value::String(mode.into()));
    serde_json::Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolbox::{ModuleVersion, Toolbox};
    use crate::tools::test_support::{context, writer};
    use komo_kernel::test_support::{FakePythonHost, TestClock};
    use komo_kernel::traits::Clock;
    use komo_kernel::types::tool::PythonResult;

    /// 一个导出 `turn_off` 的模块。
    const HA: &str = r#""""假的 HA 模块。"""

__all__ = ["turn_off"]


def turn_off(entity_id):
    return {"off": entity_id}
"#;

    /// 同样导出 `turn_off` 的下一版。
    const HA_NEXT: &str = r#""""下一版。"""

__all__ = ["turn_off"]


def turn_off(entity_id):
    return {"off": entity_id, "v": 2}
"#;

    fn wire() -> (Arc<FakePythonHost>, PythonTool) {
        let host = Arc::new(FakePythonHost::new());
        let tool = PythonTool::new(host.clone());
        (host, tool)
    }

    /// 一个装好 `toolbox.ha` 的工具。
    fn wired(dir: &std::path::Path, code: &str) -> (Arc<FakePythonHost>, Arc<Toolbox>, PythonTool) {
        let toolbox = Arc::new(Toolbox::new(dir.join("toolbox")));
        toolbox
            .install_builtin("ha", code, None, TestClock::fixed().now())
            .unwrap()
            .expect("装得上");
        let host = Arc::new(FakePythonHost::new());
        let tool = PythonTool::new(host.clone()).with_toolbox(Arc::clone(&toolbox));
        (host, toolbox, tool)
    }

    fn call_args() -> serde_json::Value {
        serde_json::json!({
            "mode": "call", "module": "toolbox.ha", "function": "turn_off",
            "args": { "entity_id": "light.living_room" }
        })
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
    async fn call_mode_is_a_different_operation_from_code_mode() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (_host, _toolbox, tool) = wired(dir.path(), HA);
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();
        assert_eq!(
            plan.operation,
            Operation::PythonCall {
                module: "toolbox.ha".into(),
                function: "turn_off".into()
            }
        );
        assert!(plan.versions.code.is_none(), "call 模式没有代码正文");
    }

    /// §14 阶段 6 第一条：**调用使用已启用且已测试版本**——计划绑的就是已启用的那一版。
    #[tokio::test]
    async fn a_call_binds_the_enabled_module_version() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (_host, _toolbox, tool) = wired(dir.path(), HA);
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();
        assert_eq!(
            plan.versions.module.as_deref(),
            Some(ModuleVersion::of(HA, None).as_str())
        );
    }

    /// 模块更新之后，新计划绑的是新版本（§5.4）：旧计划因此对不上，执行前按版本冲突拒。
    #[tokio::test]
    async fn updating_the_module_changes_the_bound_version() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (_host, toolbox, tool) = wired(dir.path(), HA);

        let before = tool.prepare(call_args(), &ctx).await.unwrap();
        // 模块升级：换一份正文启用。
        std::fs::write(toolbox.layout().enabled_code("ha"), HA_NEXT).unwrap();
        let after = tool.prepare(call_args(), &ctx).await.unwrap();
        assert_ne!(after.versions.module, before.versions.module);
    }

    /// 候选调不到：`.staging` 里那一份不是"已启用版本"（§5.2、§5.4）。
    #[tokio::test]
    async fn a_candidate_module_cannot_be_called() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let toolbox = Arc::new(Toolbox::new(dir.path().join("toolbox")));
        toolbox
            .save_candidate("ha", HA, None, TestClock::fixed().now())
            .unwrap();
        let host = Arc::new(FakePythonHost::new());
        let tool = PythonTool::new(host.clone()).with_toolbox(toolbox);

        let error = tool.prepare(call_args(), &ctx).await.unwrap_err();
        assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
        assert!(host.calls().is_empty(), "prepare 一行都不执行");
    }

    #[tokio::test]
    async fn a_private_function_is_refused_before_it_reaches_the_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, _toolbox, tool) = wired(dir.path(), HA);
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

    /// 没有声明导出的名字够不到——**在 prepare 里就够不到**，一行 Python 都没跑。
    #[tokio::test]
    async fn an_undeclared_function_never_reaches_the_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, _toolbox, tool) = wired(dir.path(), HA);
        let error = tool
            .prepare(
                serde_json::json!({ "mode": "call", "module": "toolbox.ha", "function": "undeclared" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error:?}"
        );
        assert!(host.calls().is_empty());
    }

    /// 凭证**引用**（变量名）进计划，凭证的值不进（§5.3、§7.2）。
    #[tokio::test]
    async fn the_plan_names_the_credential_variables_but_never_their_values() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let code = format!("{HA}\n__komo_env__ = [\"HA_TOKEN\"]\n");
        let (_host, _toolbox, tool) = wired(dir.path(), &code);
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();
        assert_eq!(plan.resources.len(), 1);
        assert_eq!(
            plan.resources[0].credential_env.as_deref(),
            Some("HA_TOKEN")
        );
        assert_eq!(plan.resources[0].name, "toolbox.ha");
        let serialized = serde_json::to_string(&plan).unwrap();
        assert!(!serialized.contains("secret"), "{serialized}");
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

    /// 执行前再校验一次模块版本（§7.2）。
    ///
    /// 计划与执行之间模块可以被换掉（恢复续跑时隔得更久）——换掉之后，这份计划里的导出函数、参数
    /// 含义与凭证引用说的都是上一版的事。这里换的是**声明了不同 `__komo_env__`** 的
    /// 一版：它正好是那条"计划里写着要 X、子进程拿到的是 Y"的缝。
    #[tokio::test]
    async fn a_module_swapped_after_planning_is_a_version_conflict_not_a_run() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, toolbox, tool) = wired(dir.path(), HA);
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();
        assert_eq!(
            plan.resources
                .iter()
                .filter_map(|r| r.credential_env.as_deref())
                .collect::<Vec<_>>(),
            Vec::<&str>::new(),
            "这一版没有声明任何凭证引用"
        );

        // 计划之后、执行之前，模块被换成了声明 `HA_TOKEN` 的一版——快照与指针都跟着
        // 走，这一版在 toolbox 眼里是**正正当当的当前版本**，只是不是计划绑的那一版。
        let next = format!("{HA}\n__komo_env__ = [\"HA_TOKEN\"]\n");
        toolbox.disable("ha").unwrap();
        toolbox
            .install_builtin("ha", &next, None, TestClock::fixed().now())
            .unwrap()
            .expect("换上去了");
        assert_eq!(
            toolbox.resolve_call("toolbox.ha", "turn_off").unwrap().env,
            vec!["HA_TOKEN"],
            "新那一版声明的凭证引用与计划里那串对不上"
        );

        let error = tool
            .execute(plan, &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::VersionConflict { .. }),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("toolbox.ha"),
            "说得出是哪个模块：{error}"
        );
        assert!(host.calls().is_empty(), "**一个子进程都不该起**");
    }

    /// 模块在计划之后被停用 / 删掉，同样是版本冲突——而且说得出是哪一种。
    #[tokio::test]
    async fn a_module_disabled_after_planning_is_a_version_conflict_too() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, toolbox, tool) = wired(dir.path(), HA);
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();

        toolbox.disable("ha").unwrap();

        let error = tool
            .execute(plan, &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::VersionConflict { .. }),
            "{error:?}"
        );
        assert!(host.calls().is_empty(), "一个子进程都不该起");
    }

    /// 没换过就照常跑——这条守的是"别把版本校验写成永远冲突"。
    #[tokio::test]
    async fn an_untouched_module_runs_as_planned() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, _toolbox, tool) = wired(dir.path(), HA);
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();
        host.push_result(PythonResult {
            status: ToolResultStatus::Completed,
            result: serde_json::json!({ "off": "light.living_room" }),
            error: None,
            artifacts: vec![],
            stdout_excerpt: String::new(),
            env_version: host.env_version(),
        });
        let output = tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
        assert_eq!(output.status, ToolResultStatus::Completed);
        assert_eq!(host.calls().len(), 1);
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

    #[tokio::test]
    async fn a_module_call_without_a_mode_is_still_planned_as_a_call() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (_host, _toolbox, tool) = wired(dir.path(), HA);
        let plan = tool
            .prepare(
                serde_json::json!({ "module": "toolbox.ha", "function": "turn_off", "args": {} }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            matches!(plan.operation, Operation::PythonCall { .. }),
            "{:?}",
            plan.operation
        );
        assert_eq!(plan.args["mode"], "call");
    }

    /// 两个都写或都没写：那是模型自己没说清，照常报错（**不替它猜**）。
    #[tokio::test]
    async fn an_ambiguous_argument_is_still_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (_host, tool) = wire();
        let error = tool
            .prepare(
                serde_json::json!({ "code": "x = 1", "module": "toolbox.ha" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error:?}"
        );
    }

    /// 只 print 的脚本：预览给 stdout 的首尾摘录，**不是字面量 `null`**。
    ///
    /// 真实会话里模型看到 `null` 以为工具坏了，改用 `shell` + `python3` 把同一件事重跑了一遍。
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
