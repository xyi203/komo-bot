//! `ToolExecutor`：工具**目录**（`docs/komo_bot.md` §4 末）与几个工具任务共用的小函数。
//!
//! 新存储下工具由 harness 的工具任务执行（`durable::harness::tool`），这里只剩：
//!
//! - 目录本身：全部内置工具 + codemode + MCP 连接，运行中可追加（[`ToolExecutor::install`]）
//!   与撤掉（[`ToolExecutor::withdraw`]）；钩子按能力面从它渲染 schema 与服务器段；
//! - 工具结果的落地帮手：[`store_images`]、[`result_body`]、[`timeout_error`]、
//!   [`interrupted_message`]；
//! - [`check_contract`]：`task` 交回之前对契约的复验。
//!
//! 没有 Policy 也没有审批（§7）：模型要的、能力面里有的，就执行。

pub mod cancel;
pub(crate) mod codemode;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::delegate::{DelegateContract, Validation, validate};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::plan::{ExecutionPlan, Replay};
use komo_kernel::types::refs::{InterruptedOutput, ToolResultBody, ToolResultStatus};
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::{
    IMAGE_LIMIT_BYTES, ToolContext, ToolError, ToolExposure, ToolImage, ToolNamespace, ToolOutput,
};

pub struct ToolExecutor {
    /// 全局工具目录。启动后会追加（[`Self::install`]：登录后才连上、热重载新加的 MCP 服务器，
    /// `docs/mcp.md` §2.2），也会撤（[`Self::withdraw`]：从配置删掉的服务器）；挂着的名字不换。
    /// 改动都是整份替换，读的人拿一份快照。
    tools: std::sync::RwLock<Arc<BTreeMap<String, Arc<dyn Tool>>>>,
    /// 撤掉的名字 → 为什么撤。在跑的 Run 能力面里还有它，再调时把这句交给模型，而不是一句
    /// "没有这个工具"。同名的又挂回来就删掉。
    withdrawn: std::sync::RwLock<BTreeMap<String, String>>,
    /// codemode 的 worker 启动方式（`docs/codemode.md`）。`None` = 这台执行器没有接 codemode
    /// （测试装配），`codemode` 也不会被注册。
    codemode: Option<crate::codemode::Worker>,
    /// MCP 连接（`docs/mcp.md` §2.2）：被接续的 Run 调到还没挂上的 MCP 工具时先等那台服务器
    /// （[`Self::await_mcp`]）。`None` = 没接 MCP（测试装配）。
    mcp: Option<Arc<crate::mcp::McpHub>>,
}

impl std::fmt::Debug for ToolExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolExecutor")
            .field("tools", &self.tools().keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ToolExecutor {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            tools: std::sync::RwLock::new(Arc::new(
                tools
                    .into_iter()
                    .map(|tool| (tool.definition().name, tool))
                    .collect(),
            )),
            withdrawn: std::sync::RwLock::new(BTreeMap::new()),
            codemode: None,
            mcp: None,
        }
    }

    pub fn with_codemode(mut self, worker: Option<crate::codemode::Worker>) -> Self {
        self.codemode = worker;
        self
    }

    pub fn with_mcp(mut self, hub: Arc<crate::mcp::McpHub>) -> Self {
        self.mcp = Some(hub);
        self
    }

    /// `start` 记下的指纹：脚本、引擎与 worker 协议版本、能力表、cwd 任一变了，恢复就是分歧。
    /// 升级前（CPython driver 时代）在途的调用指纹里是 `driver` / `env`，对不上，按分歧返回
    /// uncertain。
    pub(crate) fn codemode_fingerprint(
        code: &str,
        names: &[String],
        ctx: &ToolContext,
    ) -> serde_json::Value {
        serde_json::json!({
            "version": 3,
            "code": ContentHash::of_str(code),
            "engine": format!("monty-{}", crate::codemode::MONTY_VERSION),
            "worker": crate::codemode::WORKER_PROTOCOL,
            "prelude": crate::codemode::prelude_hash(),
            "tools": names,
            "cwd": ctx.cwd,
        })
    }

    /// 能力面里有、目录里还没有的 MCP 工具（`docs/mcp.md` §2.2）：Gateway 启动不等 MCP 握手，
    /// 被接续的 Run 又不经过受理，它冻结的能力面里的 MCP 工具这时可能还没挂上。先等那几台
    /// 服务器第一次连上（至多 [`crate::mcp::STARTUP_WAIT`]），连上的工具挂进目录；交回仍然
    /// 没有着落的名字 → 原因（"还没连上"、"连接失败"、"要登录"），调用方把它交给模型，而不是
    /// 一句"没有这个工具"。目录里已经有的、撤掉过的不等。
    pub async fn await_mcp(&self, names: &[String]) -> BTreeMap<String, String> {
        let Some(hub) = &self.mcp else {
            return BTreeMap::new();
        };
        let tools = self.tools();
        let missing: Vec<String> = names
            .iter()
            .filter(|name| {
                !tools.contains_key(name.as_str()) && self.withdrawn_reason(name).is_none()
            })
            .cloned()
            .collect();
        if missing.is_empty() {
            return BTreeMap::new();
        }
        let (fresh, unresolved) = hub.await_tools(&missing).await;
        let added = self.install(fresh);
        if !added.is_empty() {
            tracing::info!(tools = %added.join("、"), "MCP 服务器连上了，工具挂进目录");
        }
        unresolved
    }

    /// 全局工具目录：**发现与构造**用它（`komo skills` 的 `requires_tools` 门控也问它）。
    ///
    /// 它**不是**执行时查找的表——执行看的是 [`CallEnv::surface`]。这两件事分开正是为了
    /// 让"目录里有"不等于"这次能用"（§4 末）。
    pub fn catalog(&self) -> Vec<komo_kernel::types::tool::ToolDefinition> {
        self.tools()
            .values()
            .map(|tool| tool.definition())
            .collect()
    }

    /// 目录里此刻的名字（受理 Run 时按它算能力面）。
    pub fn tool_names(&self) -> Vec<String> {
        self.tools().keys().cloned().collect()
    }

    /// 此刻的工具表。拿的是快照：之后 [`Self::install`] 进来的不在里面。新存储的
    /// `AgentHooks`（gateway，D5）按它组 `ToolSet`。
    pub fn tools(&self) -> Arc<BTreeMap<String, Arc<dyn Tool>>> {
        Arc::clone(&self.tools.read().expect("工具目录"))
    }

    /// 运行中往目录里追加工具：登录后才连上、热重载新加的 MCP 服务器（`docs/mcp.md` §2.2）。
    /// 已经有的名字不换——在跑的 Run 的能力面里那个名字指的就是它。交回真的加进去的名字。
    ///
    /// 已经受理的 Run 不受影响：能力面在受理时冻结，新名字从下一条 Run 起才进来。
    pub fn install(&self, tools: Vec<Arc<dyn Tool>>) -> Vec<String> {
        if tools.is_empty() {
            return Vec::new();
        }
        let mut guard = self.tools.write().expect("工具目录");
        let mut next = BTreeMap::clone(&guard);
        let mut added = Vec::new();
        for tool in tools {
            let name = tool.definition().name;
            if next.contains_key(&name) {
                continue;
            }
            next.insert(name.clone(), tool);
            added.push(name);
        }
        *guard = Arc::new(next);
        drop(guard);
        let mut withdrawn = self.withdrawn.write().expect("撤掉的工具");
        for name in &added {
            withdrawn.remove(name);
        }
        added
    }

    /// 从目录里撤掉工具（照 pi 的 `hideTools`）：从配置删掉、或改了配置要重连的 MCP 服务器。
    /// 每个名字带一句原因，在跑的 Run 再调它时交给模型。交回真的撤掉的名字。
    ///
    /// 已经在执行的那一次调用不受影响：它手里有自己那份工具。之后受理的 Run 看不到这些名字。
    pub fn withdraw(&self, tools: Vec<(String, String)>) -> Vec<String> {
        if tools.is_empty() {
            return Vec::new();
        }
        let mut guard = self.tools.write().expect("工具目录");
        let mut next = BTreeMap::clone(&guard);
        let mut withdrawn = self.withdrawn.write().expect("撤掉的工具");
        let mut removed = Vec::new();
        for (name, reason) in tools {
            if next.remove(&name).is_some() {
                removed.push(name.clone());
            }
            withdrawn.insert(name, reason);
        }
        *guard = Arc::new(next);
        removed
    }

    /// 这个名字被撤过：交回撤的原因。新存储的钩子（gateway，D5）用它给撤掉的工具放桩。
    pub fn withdrawn_reason(&self, name: &str) -> Option<String> {
        self.withdrawn
            .read()
            .expect("撤掉的工具")
            .get(name)
            .cloned()
    }

    /// 此刻撤着的名字 → 原因（新存储的钩子按它把撤掉的名字留在能力面里，再调时报原因）。
    pub fn withdrawn(&self) -> BTreeMap<String, String> {
        self.withdrawn.read().expect("撤掉的工具").clone()
    }

    /// 交给模型的工具 Schema：**按能力面的顺序**、只列能力面里真的装着的那些。
    ///
    /// 名字在能力面里却没有实现，是装配错误（目录与能力面不同源），这里跳过并在日志里
    /// 说一声——把没有 schema 的工具交给模型没有意义，而静默地补一条假 schema 更坏。
    ///
    /// 能力面里没有 `bash` 时，`edit` / `write` 的 `then_run` 参数一并摘掉：模型看不见
    /// 一个注定被拒的参数（执行侧 `begin` 照样拒，不靠这里）。
    ///
    /// 只列直接声明的那些：`exposure = codemode` 的 MCP 工具仍在能力面里（脚本调得到），
    /// 但不进模型的 schema（`docs/mcp.md` §3）。隐藏 schema 不等于撤权。
    pub fn definitions_for(
        &self,
        surface: &AgentSurface,
    ) -> Vec<komo_kernel::types::tool::ToolDefinition> {
        self.surface_definitions(surface, false)
    }

    /// 能力面里全部工具的说明，包括只在 codemode 里调的那些：`tool://` 的挂载与脚本里的
    /// 发现函数用它。
    pub fn callable_definitions_for(
        &self,
        surface: &AgentSurface,
    ) -> Vec<komo_kernel::types::tool::ToolDefinition> {
        self.surface_definitions(surface, true)
    }

    /// 系统提示服务器段要列的那些工具组：能力面里有 `exposure = codemode` 的工具、而且这次
    /// 真的调得到 codemode。codemode 不在时一组都不列——说了"在脚本里调"也调不到。
    pub fn namespaces_for(&self, surface: &AgentSurface) -> Vec<ToolNamespace> {
        if !self.can_run_codemode(surface) {
            return Vec::new();
        }
        let tools = self.tools();
        let mut namespaces: Vec<ToolNamespace> = Vec::new();
        for name in surface.names() {
            let Some(tool) = tools.get(name) else {
                continue;
            };
            if tool.exposure() != ToolExposure::Codemode {
                continue;
            }
            if let Some(namespace) = tool.namespace()
                && !namespaces.iter().any(|known| known.name == namespace.name)
            {
                namespaces.push(namespace);
            }
        }
        namespaces
    }

    /// 这次运行调得到 codemode：能力面里有它，这台 Gateway 也接了 worker。
    fn can_run_codemode(&self, surface: &AgentSurface) -> bool {
        self.codemode.is_some()
            && surface.allows(crate::tools::codemode::CODEMODE_TOOL)
            && self
                .tools()
                .contains_key(crate::tools::codemode::CODEMODE_TOOL)
    }

    fn surface_definitions(
        &self,
        surface: &AgentSurface,
        include_codemode_only: bool,
    ) -> Vec<komo_kernel::types::tool::ToolDefinition> {
        let commands = self.can_run_commands(surface);
        let tools = self.tools();
        surface
            .names()
            .iter()
            .filter(|name| {
                include_codemode_only
                    || tools
                        .get(name.as_str())
                        .is_none_or(|tool| tool.exposure() == ToolExposure::Direct)
            })
            .filter_map(|name| match tools.get(name) {
                Some(tool) => {
                    let mut definition = tool.definition();
                    if !commands
                        && let Some(properties) = definition
                            .parameters
                            .get_mut("properties")
                            .and_then(serde_json::Value::as_object_mut)
                    {
                        properties.remove("then_run");
                    }
                    Some(definition)
                }
                // 热重载撤掉的 MCP 工具：在跑的 Run 的能力面里还有名字，不再给模型看（照 pi 的
                // `hideTools`）。不是装配错误，不告警。
                None if self.withdrawn_reason(name).is_some() => None,
                None => {
                    tracing::warn!(tool = %name, "能力面里的工具没有实现，跳过它的 schema");
                    None
                }
            })
            .collect()
    }

    /// 这次运行能不能跑命令：能力面里有 `bash`，执行器也装着它。`then_run` 的放行与
    /// schema 都问它。
    fn can_run_commands(&self, surface: &AgentSurface) -> bool {
        surface.allows("bash") && self.tools().contains_key("bash")
    }
}

/// 把工具交出来的图片写进这次尝试的输出目录，结果里只留引用；返回写下来的那几张，这一次
/// 直接交给模型。超过上限或写不下来的那张不让整次调用失败：正文里补一句，模型知道少了什么。
pub(crate) async fn store_images(
    writer: &mut dyn OutputWriter,
    body: &mut ToolResultBody,
    images: &[ToolImage],
) -> Vec<ToolImage> {
    let mut stored = Vec::new();
    let mut notes = Vec::new();
    for image in images {
        if image.bytes.len() > IMAGE_LIMIT_BYTES {
            notes.push(format!(
                "有一张 {} 图片 {} 字节，超过上限 {} 字节，没有附上",
                image.mime,
                image.bytes.len(),
                IMAGE_LIMIT_BYTES
            ));
            continue;
        }
        match writer.write_image(&image.mime, &image.bytes).await {
            Ok(reference) => {
                body.images.push(reference);
                stored.push(image.clone());
            }
            Err(error) => {
                tracing::warn!(%error, mime = %image.mime, "结果图片没存下来");
                notes.push(format!("有一张 {} 图片没存下来：{error}", image.mime));
            }
        }
    }
    if !notes.is_empty() {
        let preview = body.preview.get_or_insert_with(String::new);
        for note in notes {
            if !preview.is_empty() {
                preview.push('\n');
            }
            preview.push_str(&note);
        }
    }
    stored
}

/// 这份计划的动作重做一遍是安全的吗：可安全重做的读取，和外部接口确实保证去重的幂等键
/// （[`Replay::Safe`]，§8.6）。
fn safe_to_redo(plan: &ExecutionPlan) -> bool {
    plan.recovery.replay() == Replay::Safe
}

/// 中断结果里每条流最多带多少字节的末尾。
pub(crate) const INTERRUPTED_TAIL_BYTES: usize = 2048;

/// 被重启打断、又不能重跑的那次调用交给模型的正文。
pub(crate) fn interrupted_message(tool: &str, left: &InterruptedOutput) -> String {
    let mut out = format!(
        "上一次执行被 Gateway 重启打断，{tool} 重启后不会自动重跑，副作用是否已经发生未知。\
         {UNCERTAIN_ADVICE}。"
    );
    if left.is_empty() {
        out.push_str("\n中断前没有落下任何输出。");
        return out;
    }
    out.push_str("\n中断前已落盘的部分输出（不是完成结果）：");
    for (label, bytes, tail) in [
        ("stdout", left.stdout_bytes, &left.stdout_tail),
        ("stderr", left.stderr_bytes, &left.stderr_tail),
    ] {
        if bytes == 0 {
            continue;
        }
        let shown = tail.len() as u64;
        if shown < bytes {
            out.push_str(&format!("\n[{label} 共 {bytes} 字节，末尾 {shown} 字节]\n"));
        } else {
            out.push_str(&format!("\n[{label} 共 {bytes} 字节]\n"));
        }
        out.push_str(tail);
    }
    out
}

/// 工具守着 `call_timeout` 自己超时之后，还要终止进程组、回收（TERM 与 KILL 各等 5s）、
/// 写完输出。兜底计时器多给这么一段，否则两边同时到点，赢的总是执行器——一个本来
/// 说得清"超时被杀"的结果就成了"结果不明"。
pub(crate) const REAP_GRACE: Duration = Duration::from_secs(15);

/// 活动执行时限到了。
///
/// 读取可以说"失败了，再读一次就是"；其余动作在超时这一刻**不知道**副作用发没发生，
/// 所以是 `Uncertain` 而不是 `Failed`——后者会让模型自己再发一次，把效果做两遍。
pub(crate) fn timeout_error(plan: &ExecutionPlan, limit: Duration) -> ToolError {
    if safe_to_redo(plan) {
        ToolError::Timeout {
            after_secs: limit.as_secs(),
        }
    } else {
        ToolError::Uncertain {
            message: format!(
                "超过活动执行时限 {}s，副作用是否已经发生未知",
                limit.as_secs()
            ),
        }
    }
}

pub(crate) fn result_body(outcome: &Result<ToolOutput, ToolError>) -> ToolResultBody {
    match outcome {
        Ok(output) => ToolResultBody {
            status: output.status,
            result: output.result.clone(),
            error: None,
            exit_code: output.exit_code,
            artifacts: output.artifacts.clone(),
            // 图片要先写成文件才有引用：发布前由 [`store_images`] 填。
            images: vec![],
            // 工具写给模型看的那段正文跟着结果一起落进 `output.json`：模型上下文的投影读
            // 这一份，而 JSONL 里的事件只留它的前 1 KiB。
            preview: output.preview.clone(),
        },
        Err(error) => ToolResultBody {
            status: if error.is_uncertain() {
                ToolResultStatus::Uncertain
            } else {
                ToolResultStatus::Failed
            },
            result: serde_json::Value::Null,
            error: Some(error.to_string()),
            exit_code: None,
            artifacts: vec![],
            images: vec![],
            // 工具失败时模型必须看见那句话（"版本冲突：a.txt"）——投影读的就是这一份。
            // 结果不明的再附上判断方法：它不停下来等人，由模型去核对。
            preview: Some(if error.is_uncertain() {
                format!("{error}。{UNCERTAIN_ADVICE}")
            } else {
                error.to_string()
            }),
        },
    }
}

/// 结果不明（执行当中超时 / 断开，或续跑时核对不出）交给模型的那段判断方法：先核对，
/// 再决定重做还是接着往下。
const UNCERTAIN_ADVICE: &str = "执行器不会自动重做。\
先核对目标的实际状态（读文件、查进程或服务状态、看 git、查远端），再决定：已经生效就接着往下；\
没生效或只做了一半就继续或重新做；只读的命令直接重跑即可。\
发消息、扣款、切换一次这类重复做会出问题、又核对不了的，先问用户";

/// 从最后一条回复里取 JSON 并按契约验一遍：`task` 交回之前的复验（`orchestration::report`）用它。
///
/// 校验器来自 kernel，**两头共用同一份代码**（§8.6）：两份实现会漂移，漂移的症状是
/// "子代理说成功、父侧说结果不合规"，而判断只能有一个出处。取 JSON 那一步与记忆提取
/// 共用（两边都容忍围栏与前后闲话），理由相同。
pub fn check_contract(
    contract: &DelegateContract,
    text: &str,
) -> Result<(serde_json::Value, Validation), String> {
    let Some(json) = crate::memory::parse_json_object(text) else {
        return Err("最后一条回复里找不到 JSON 对象".into());
    };
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| format!("最后一条回复里的 JSON 解不开：{error}"))?;
    let validation = validate(contract, &value);
    if validation.is_valid() {
        Ok((value, validation))
    } else {
        Err(validation.describe())
    }
}

#[cfg(test)]
mod tests;
