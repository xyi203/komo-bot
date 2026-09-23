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
use komo_kernel::policy::PolicyDecision;
use komo_kernel::traits::{ApprovalRepo, Clock, OutputWriter, PythonHost, Tool};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{
    ApprovedPlan, EnvVersion, ExecutionPlan, Operation, PlanSource, PlanVersions, RecoveryMode,
    ResourceRef, Verification,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{
    PyError, PythonJob, ToolContext, ToolDefinition, ToolError, ToolOutput,
};
use serde::{Deserialize, Serialize};

use crate::policy::{DecisionEnv, PolicyEngine, grants_for};
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

/// 跑核对函数要的那三样（§8.6「核对本身仍经过 Policy」）。
///
/// 没有它，[`PythonTool::verify`] 一律答 [`Verification::Unavailable`]——那是"这个工具
/// 没有可用的核对方式"，executor 会停在 `waiting + intervention`。**不装这个门就没有核对**，
/// 而不是"核对不过 Policy 也照跑"。
#[derive(Clone)]
pub struct VerificationGate {
    pub policy: PolicyEngine,
    pub approvals: Arc<dyn ApprovalRepo>,
    pub clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for VerificationGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerificationGate").finish_non_exhaustive()
    }
}

pub struct PythonTool {
    host: Arc<dyn PythonHost>,
    /// 已保存的能力。`None` = 这台 Gateway 没有 toolbox，于是 `call` 模式无从解析
    /// 版本，只能拒绝——**不退化成"不绑版本地调一下试试"**（§5.4）。
    toolbox: Option<Arc<Toolbox>>,
    verification: Option<VerificationGate>,
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
            .field("verifies", &self.verification.is_some())
            .finish_non_exhaustive()
    }
}

impl PythonTool {
    pub fn new(host: Arc<dyn PythonHost>) -> Self {
        Self {
            host,
            toolbox: None,
            verification: None,
        }
    }

    pub fn with_toolbox(mut self, toolbox: Arc<Toolbox>) -> Self {
        self.toolbox = Some(toolbox);
        self
    }

    /// 装上核对这条路（§8.6）。
    pub fn with_verification(mut self, gate: VerificationGate) -> Self {
        self.verification = Some(gate);
        self
    }

    /// 这份计划绑的模块版本还是**现在已启用的那一版**吗（§7.2）。
    ///
    /// `code` 模式没有模块可言，直接过。`call` 模式有三种不通过，**都是版本冲突**：
    /// 没有 toolbox（证不出这是已审核的那一版）、模块解析不出来了（停用 / 删掉 / 不再
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
            return conflict("这台 Gateway 没有 toolbox，证不出这是已审核的那一版".into());
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
            // 模块在审批期间被停用 / 删掉 / 改得不再导出这个函数。原因原样转述——
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
                    // 任意代码没有可靠恢复方式：停在 waiting + intervention，不自动从头再跑
                    // 整个脚本（§8.6 那张表的最后一行）。
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
                    reason: "这台 Gateway 没有 toolbox：call 模式无从确定已审核版本".into(),
                })?;
                let resolved = toolbox.resolve_call(module, function).map_err(refusal_of)?;
                (
                    Operation::PythonCall {
                        // 规范化成 `toolbox.<module>`：Policy 的 `modules` 匹配器与授权
                        // 里的模块名必须是同一种写法，否则 `toolbox.memos` 与 `memos`
                        // 会是两条互不覆盖的授权。
                        module: dotted(&resolved.module),
                        function: function.clone(),
                    },
                    PlanVersions {
                        code: None,
                        // 授权绑定的就是它（§7.2）：模块一升级，版本就变，
                        // `versions_cover` 让绑旧版本的授权覆盖不到新计划（§5.4）。
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
                    // §8.6：有与版本绑定的核对函数才谈得上"可以核对目标状态"。
                    // 模块自称幂等不算——只认它**真的提供了**一个核对函数。
                    match resolved.verifier {
                        Some(_) => RecoveryMode::VerifyTarget,
                        None => RecoveryMode::NoSafeRecovery,
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
            resources,
            recovery,
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

        // 模块版本也一样（§7.2「**批准后再次校验目标和版本**，变化则重新评估」）。
        //
        // 审批可以停在那里一天，而 `komo toolbox enable` 在那期间可以把模块换掉。换过
        // 之后，这份计划里的一切——导出的函数、参数的含义、`resources` 里那串凭证引用
        // ——说的都是**上一版**的事；照着跑一遍等于拿一份对旧代码的批准去执行新代码。
        // `verify` 那一路已经这么判了（"模块升级过 → Unknown"），执行这一路没有理由更松。
        //
        // 交回模型的是一个**版本冲突**，和 `write` / `edit` 覆盖到别人改过的文件是同一类：
        // 它会重新 `prepare` → 新计划 → 新哈希 → 旧授权覆盖不到 → 重新审（§5.4）。
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
            .run_with_artifacts(args.job, artifacts.as_deref(), sink, ctx.cancel.clone())
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
        // 结构化结果时给 stdout 的尾巴，再不济也要说清"没有输出"，绝不把 `null` 当正文。
        let preview = match (&outcome.error, outcome.result.is_null()) {
            (Some(error), _) => format!("{:?}：{error}", outcome.status),
            (None, false) => serde_json::to_string(&outcome.result).unwrap_or_default(),
            (None, true) => {
                let tail = outcome.stdout_tail.trim();
                if tail.is_empty() {
                    "没有返回值，也没有输出".to_string()
                } else {
                    format!("没有返回值；这是 stdout 的尾部：\n{tail}")
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

    /// §8.6：用**模块自带的、与版本绑定的**核对函数判断上一次到底发生了没有。
    ///
    /// 「已保存的 Python 模块可提供与版本绑定的核对函数，由执行器通过同一 python 执行
    /// 机制调用；核对本身仍经过 Policy。它不增加第六个模型工具。」——所以这里做的是：
    ///
    /// 1. 另做一份计划，来源是 [`PlanSource::Verification`]，**操作是调那个核对函数**。
    /// 2. 让它过一遍 [`PolicyEngine`]。过不去就是"核对不出结论"
    ///    （[`Verification::Unknown`]），executor 于是交给人——**不是**"过不去也照跑"。
    /// 3. 过得去就用同一个 [`PythonHost`] 调它，把它的答复映射成四种结论之一。
    ///
    /// 三种情况直接答 [`Verification::Unavailable`]（= 没有可用的核对方式 →
    /// `waiting + intervention`）：`code` 模式、模块没声明核对函数、这台 Gateway 没装核对门。
    /// **模块自称幂等不构成证明**：这里只认核对函数真的跑出来的那个答案。
    async fn verify(
        &self,
        plan: &ExecutionPlan,
        ctx: &ToolContext,
    ) -> Result<Verification, ToolError> {
        let Operation::PythonCall { module, function } = &plan.operation else {
            return Ok(Verification::Unavailable);
        };
        let (Some(toolbox), Some(gate)) = (&self.toolbox, &self.verification) else {
            return Ok(Verification::Unavailable);
        };
        let resolved = match toolbox.resolve_call(module, function) {
            Ok(resolved) => resolved,
            // 模块被停用 / 改没了：核对逻辑对不上那份已审核的计划，这是"不知道"。
            Err(error) => {
                return Ok(Verification::Unknown {
                    reason: format!("{module} 的核对逻辑取不到：{error}"),
                });
            }
        };
        let Some(verifier) = resolved.verifier else {
            return Ok(Verification::Unavailable);
        };
        // 「代码、依赖、参数以及核对逻辑都必须对应已审核的执行计划」（§8.6）：模块在这
        // 之后升级过，现在这份核对逻辑就不是那份计划的核对逻辑。
        let planned = plan.versions.module.as_deref();
        if planned != Some(resolved.version.as_str()) {
            return Ok(Verification::Unknown {
                reason: format!(
                    "{module} 现在是 {}，而这份计划绑的是 {}：核对逻辑与已审核计划对不上",
                    resolved.version,
                    planned.unwrap_or("（未绑定）")
                ),
            });
        }

        let job = PythonJob::Call {
            module: dotted(&resolved.module),
            function: verifier.clone(),
            // 核对函数拿到的是**原调用**的身份与参数，不是一堆自由参数。
            args: serde_json::json!({ "function": function, "args": call_args(plan) }),
        };
        let check = verification_plan(plan, ctx, &resolved.module, &verifier, &job)?;

        // 核对本身仍经过 Policy（§8.6）。
        let now = gate.clock.now();
        let grants = grants_for(gate.approvals.as_ref(), &check, now)
            .await
            .map_err(|error| ToolError::Failed {
                message: error.to_string(),
            })?;
        let env = DecisionEnv {
            grants: &grants,
            principal: None,
            roots: &ctx.roots,
            now,
        };
        match gate.policy.decide(&check, &env) {
            PolicyDecision::Allow { .. } => {}
            // Ask 在这里**不能**变成"问一次人"：核对是恢复流程里的一步，不是一次新的
            // 模型动作。答不出结论就说答不出结论，executor 会停在 waiting + intervention，
            // 由操作者决定——那正是"交给人"的正确形状。
            PolicyDecision::Ask { reason, .. } => {
                return Ok(Verification::Unknown {
                    reason: format!("核对调用需要审批，未自动执行：{reason}"),
                });
            }
            PolicyDecision::Deny { reason } => {
                return Ok(Verification::Unknown {
                    reason: format!("核对调用被拒绝：{reason}"),
                });
            }
        }

        let mut sink = crate::toolbox::NullWriter::new(ctx);
        let outcome = match self.host.run(job, &mut sink, ctx.cancel.clone()).await {
            Ok(outcome) => outcome,
            Err(error) => {
                return Ok(Verification::Unknown {
                    reason: format!("核对函数跑不起来：{error}"),
                });
            }
        };
        if outcome.status != ToolResultStatus::Completed {
            return Ok(Verification::Unknown {
                reason: outcome
                    .error
                    .unwrap_or_else(|| "核对函数没有给出结论".into()),
            });
        }
        Ok(verdict_of(outcome.result))
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
/// 调用。**这不放松授权**——计划绑的仍是 `Operation` 与代码 / 模块版本（§5.2、§7.2），少写
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

/// 原调用的关键字参数。
fn call_args(plan: &ExecutionPlan) -> serde_json::Value {
    plan.args
        .get("args")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}))
}

/// 核对用的那份计划（§8.6：`PlanSource::Verification { of }`）。
///
/// 它与被核对的那份计划共享 **run / tool_call / 模块版本 / 环境版本**——"核对逻辑必须
/// 对应已审核的执行计划"就是这几样；来源换成 `Verification`，操作换成调核对函数本身，
/// 恢复方式是 `SafeReread`（核对是只读的，重做安全）。
fn verification_plan(
    plan: &ExecutionPlan,
    ctx: &ToolContext,
    module: &str,
    verifier: &str,
    job: &PythonJob,
) -> Result<ExecutionPlan, ToolError> {
    let of = plan.tool_call.clone().unwrap_or_else(|| ctx.call.clone());
    Ok(ExecutionPlan {
        operation_id: OperationId::new_at(plan_time()),
        source: PlanSource::Verification { of },
        tool: "python".into(),
        operation: Operation::PythonCall {
            module: dotted(module),
            function: verifier.to_string(),
        },
        run: plan.run.clone(),
        tool_call: plan.tool_call.clone(),
        args: normalized(&PythonArgs { job: job.clone() })?,
        cwd: plan.cwd.clone(),
        targets: vec![],
        versions: plan.versions.clone(),
        resources: plan.resources.clone(),
        recovery: RecoveryMode::SafeReread,
    })
}

/// 核对函数的答复 → 四种结论。
///
/// 认不出来的形状是 [`Verification::Unknown`]，**不是** `AlreadySatisfied`：一个写坏了
/// 的核对函数不能把"不知道"变成"已经做过了"。
fn verdict_of(value: serde_json::Value) -> Verification {
    let kind = value.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let text = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_default()
    };
    match kind {
        "already_satisfied" => Verification::AlreadySatisfied {
            evidence: text("evidence"),
        },
        "not_performed" => Verification::NotPerformed {
            evidence: text("evidence"),
        },
        "conflict" => Verification::Conflict {
            evidence: text("evidence"),
        },
        "unknown" => Verification::Unknown {
            reason: text("reason"),
        },
        _ => Verification::Unknown {
            reason: format!("核对函数的答复读不懂：{value}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolbox::{ModuleVersion, Toolbox};
    use crate::tools::test_support::{approved, context, writer};
    use komo_kernel::policy::{
        Effect, Grant, GrantScope, Matcher, OperationMatch, PolicyRule, RuleTable,
    };
    use komo_kernel::test_support::{FakePythonHost, MemApprovalRepo, TestClock};
    use komo_kernel::types::ids::{ApprovalId, GrantId, RunId};
    use komo_kernel::types::plan::SourceKind;
    use komo_kernel::types::tool::PythonResult;

    /// 一个导出 `turn_off`、并且**带核对函数**的模块。
    const HA: &str = r#""""假的 HA 模块。"""

__all__ = ["turn_off", "check"]
__komo_verify__ = "check"


def turn_off(entity_id):
    return {"off": entity_id}


def check(function=None, args=None):
    return {"kind": "already_satisfied", "evidence": "灯是关着的"}
"#;

    /// 同样导出 `turn_off`，但**没有**核对函数。
    const HA_NO_CHECK: &str = r#""""没有核对函数的版本。"""

__all__ = ["turn_off"]


def turn_off(entity_id):
    return {"off": entity_id}
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
    async fn call_mode_is_a_different_operation_so_policy_can_tell_them_apart() {
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

    /// §14 阶段 6 第一条：**调用使用已批准且已测试版本**——计划绑的就是已启用的那一版。
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

    /// §14 阶段 6 第二条：**模块更新使旧授权失效**。
    ///
    /// 判断在 kernel 的 `Grant::covers` 里（那里有它自己的测试）；这里测的是这条链子
    /// 真的接上了——`prepare` 填的版本换了，同一条授权就覆盖不到新计划。
    #[tokio::test]
    async fn updating_the_module_invalidates_a_grant_bound_to_the_old_version() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let now = TestClock::fixed().now();
        let (_host, toolbox, tool) = wired(dir.path(), HA);

        let before = tool.prepare(call_args(), &ctx).await.unwrap();
        let grant = Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: GrantScope::Run {
                run: RunId::from_raw("run-1"),
                matcher: Matcher::modules(["toolbox.ha"]),
                versions: before.versions.clone(),
            },
            granted_at: now,
            valid_until: None,
            consumed: false,
            reason: "操作者批准了这一版".into(),
        };
        assert!(grant.covers(&before, now), "刚批的当然覆盖得到");

        // 模块升级：换一份正文启用。
        std::fs::write(toolbox.layout().enabled_code("ha"), HA_NO_CHECK).unwrap();
        let after = tool.prepare(call_args(), &ctx).await.unwrap();
        assert_ne!(after.versions.module, before.versions.module);
        assert!(!grant.covers(&after, now), "模块更新使旧授权失效（§5.4）");
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
            stdout_tail: String::new(),
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

    /// §7.2「**批准后再次校验目标和版本，变化则重新评估**」。
    ///
    /// 审批可以停一天，而这期间模块可以被换掉——换掉之后，这份计划里的导出函数、参数
    /// 含义与凭证引用说的都是上一版的事。这里换的是**声明了不同 `__komo_env__`** 的
    /// 一版：它正好是那条"计划里写着要 X、子进程拿到的是 Y"的缝。
    #[tokio::test]
    async fn a_module_swapped_during_the_approval_window_is_a_version_conflict_not_a_run() {
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

        // 审批还等着的时候，模块被换成了声明 `HA_TOKEN` 的一版——快照与指针都跟着
        // 走，这一版在 toolbox 眼里是**正正当当的当前版本**，只是不是被批准的那一版。
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
            .execute(approved(plan), &ctx, &mut writer(&ctx))
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

    /// 模块在审批期间被停用 / 删掉，同样是版本冲突——而且说得出是哪一种。
    #[tokio::test]
    async fn a_module_disabled_during_the_approval_window_is_a_version_conflict_too() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, toolbox, tool) = wired(dir.path(), HA);
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();

        toolbox.disable("ha").unwrap();

        let error = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
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
            stdout_tail: String::new(),
            env_version: host.env_version(),
        });
        let output = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap();
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
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        assert!(error.is_uncertain(), "{error:?}");
    }

    // ------------------------------------------------------------ 核对（§8.6）

    /// 一张只放行**核对来源**的 `python_call` 的规则表。核对是恢复流程里的一步，
    /// 它与模型自己发起的调用是两种来源，规则分得开。
    fn verification_allowed() -> PolicyEngine {
        PolicyEngine::from_rules(RuleTable {
            rules: vec![PolicyRule {
                id: "verification-call".into(),
                effect: Effect::Allow,
                reason: "恢复流程的核对调用".into(),
                matcher: Matcher {
                    sources: Some(vec![SourceKind::Verification]),
                    operations: Some(vec![OperationMatch::PythonCall]),
                    ..Default::default()
                },
                scopes: vec![],
                requires_isolation: false,
                grant_proof: false,
            }],
            default: Effect::Ask,
        })
    }

    fn gate(policy: PolicyEngine) -> VerificationGate {
        VerificationGate {
            policy,
            approvals: Arc::new(MemApprovalRepo::new()),
            clock: Arc::new(TestClock::fixed()),
        }
    }

    /// 任意 code 没有核对方式：停在 `waiting + intervention`，不自动从头跑整个脚本（§8.6）。
    #[tokio::test]
    async fn code_mode_has_no_verification_so_it_lands_on_a_human() {
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
            Verification::Unavailable
        );
    }

    /// 模块**没有**核对函数 → 同样是"没有可用的核对方式"。模块自称幂等不算数（§8.6）。
    #[tokio::test]
    async fn a_module_without_a_verifier_offers_no_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (_host, _toolbox, tool) = wired(dir.path(), HA_NO_CHECK);
        let tool = tool.with_verification(gate(verification_allowed()));
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();
        assert_eq!(plan.recovery, RecoveryMode::NoSafeRecovery);
        assert_eq!(
            tool.verify(&plan, &ctx).await.unwrap(),
            Verification::Unavailable
        );
    }

    /// 有核对函数 → 计划的恢复方式是"可以核对目标状态"，核对真的跑起来，四种结论
    /// 逐个映射。
    #[tokio::test]
    async fn a_verifier_runs_through_policy_and_its_four_answers_all_map() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, _toolbox, tool) = wired(dir.path(), HA);
        let tool = tool.with_verification(gate(verification_allowed()));
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();
        assert_eq!(plan.recovery, RecoveryMode::VerifyTarget);

        for (answer, want) in [
            (
                serde_json::json!({ "kind": "already_satisfied", "evidence": "灯是关着的" }),
                Verification::AlreadySatisfied {
                    evidence: "灯是关着的".into(),
                },
            ),
            (
                serde_json::json!({ "kind": "not_performed", "evidence": "灯还亮着" }),
                Verification::NotPerformed {
                    evidence: "灯还亮着".into(),
                },
            ),
            (
                serde_json::json!({ "kind": "conflict", "evidence": "变成了第三种状态" }),
                Verification::Conflict {
                    evidence: "变成了第三种状态".into(),
                },
            ),
            (
                serde_json::json!({ "kind": "unknown", "reason": "读不到" }),
                Verification::Unknown {
                    reason: "读不到".into(),
                },
            ),
        ] {
            host.push_result(PythonResult {
                status: ToolResultStatus::Completed,
                result: answer,
                error: None,
                artifacts: vec![],
                stdout_tail: String::new(),
                env_version: host.env_version(),
            });
            assert_eq!(tool.verify(&plan, &ctx).await.unwrap(), want);
        }

        // 核对调的是核对函数，拿到的是**原调用**的身份与参数。
        let last = host.calls().last().cloned().unwrap();
        assert_eq!(
            last,
            PythonJob::Call {
                module: "toolbox.ha".into(),
                function: "check".into(),
                args: serde_json::json!({
                    "function": "turn_off",
                    "args": { "entity_id": "light.living_room" }
                }),
            }
        );
    }

    /// 一个写坏了的核对函数不能把"不知道"变成"已经做过了"。
    #[tokio::test]
    async fn an_answer_the_verifier_did_not_understand_is_unknown_not_satisfied() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, _toolbox, tool) = wired(dir.path(), HA);
        let tool = tool.with_verification(gate(verification_allowed()));
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();

        host.push_result(PythonResult {
            status: ToolResultStatus::Completed,
            result: serde_json::json!({ "idempotent": true }),
            error: None,
            artifacts: vec![],
            stdout_tail: String::new(),
            env_version: host.env_version(),
        });
        assert!(
            matches!(
                tool.verify(&plan, &ctx).await.unwrap(),
                Verification::Unknown { .. }
            ),
            "模块自称幂等不构成证明（§8.6）"
        );
    }

    /// **核对本身仍经过 Policy**：一张什么都问的表下，核对不自动执行，答"不出结论"。
    #[tokio::test]
    async fn a_verification_policy_refuses_and_the_verdict_is_unknown_not_a_silent_run() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, _toolbox, tool) = wired(dir.path(), HA);
        let tool = tool.with_verification(gate(PolicyEngine::conservative()));
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();

        let verdict = tool.verify(&plan, &ctx).await.unwrap();
        assert!(
            matches!(verdict, Verification::Unknown { .. }),
            "{verdict:?}"
        );
        assert!(host.calls().is_empty(), "没过 Policy 就一次都没跑");
    }

    /// 「核对逻辑必须对应已审核的执行计划」（§8.6）：模块在审批之后升级过，
    /// 现在这份核对逻辑不是那份计划的核对逻辑。
    #[tokio::test]
    async fn a_verifier_from_a_newer_module_version_is_not_this_plans_verifier() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, toolbox, tool) = wired(dir.path(), HA);
        let tool = tool.with_verification(gate(verification_allowed()));
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();

        std::fs::write(
            toolbox.layout().enabled_code("ha"),
            format!("{HA}\n# 升级过了\n"),
        )
        .unwrap();
        let verdict = tool.verify(&plan, &ctx).await.unwrap();
        assert!(
            matches!(verdict, Verification::Unknown { .. }),
            "{verdict:?}"
        );
        assert!(host.calls().is_empty());
    }

    /// 没装核对门的 Gateway 一律答"没有核对方式"，**不是**"照跑"。
    #[tokio::test]
    async fn without_a_verification_gate_nothing_is_verified() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, _toolbox, tool) = wired(dir.path(), HA);
        let plan = tool.prepare(call_args(), &ctx).await.unwrap();
        assert_eq!(
            tool.verify(&plan, &ctx).await.unwrap(),
            Verification::Unavailable
        );
        assert!(host.calls().is_empty());
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

    /// 只 print 的脚本：预览给 stdout 的尾巴，**不是字面量 `null`**。
    ///
    /// 真实会话里模型看到 `null` 以为工具坏了，改用 `shell` + `python3` 把同一件事重跑了一遍。
    #[tokio::test]
    async fn a_script_that_only_prints_shows_its_stdout_tail() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (host, tool) = wire();
        host.push_stdout("{\"rows\": [[241653]]}\n");
        host.push_result(PythonResult {
            status: ToolResultStatus::Completed,
            result: serde_json::Value::Null,
            error: None,
            artifacts: vec![],
            stdout_tail: String::new(),
            env_version: host.env_version(),
        });
        let plan = tool
            .prepare(
                serde_json::json!({ "mode": "code", "code": "print(1)" }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap();

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
            stdout_tail: String::new(),
            env_version: host.env_version(),
        });
        let plan = tool
            .prepare(serde_json::json!({ "mode": "code", "code": "x = 1" }), &ctx)
            .await
            .unwrap();
        let output = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap();
        assert_eq!(
            output.preview.as_deref(),
            Some("没有返回值，也没有输出"),
            "空就是空，说清楚"
        );
    }
}
