//! 钩子（`docs/storage-durable.md` §12.3 的 `prepareRequest`）：runtime 里定义，D5 由 gateway
//! 实现，D4b 用测试替身。
//!
//! 给定 conversation（和已提交的上下文），钩子返回系统段、工具与能力面、模型配置与预算。
//! **runtime 不依赖 komo-agent**，所以"装配上下文"（Profile、skills、记忆段、Profile → 能力面
//! 选择）都在钩子背后；这里只定义它交回来的值。
//!
//! 每个决定点都按当前配置重新问一次（"names resolved at each use"，spec §7.1）：生成任务的
//! `prepare` 问 [`AgentHooks::setup`]，`request` 的图片解析与工具任务的 `call` / `execute` 问
//! [`AgentHooks::tools`]。已经开始的 `request` 用 checkpoint 里冻结的模型与预算。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::durable::{ConversationId, Message};
use komo_kernel::projection::ProjectionContext;
use komo_kernel::traits::{Tool, ToolOutputStore};
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::{PlanSource, Replay};
use komo_kernel::types::resource::ResourceMounts;
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::{ToolDefinition, ToolExposure, WorkspaceRoot};
use serde::{Deserialize, Serialize};

use crate::agent::RetryBudget;
use crate::llm::ImageResolver;
use crate::recovery::{IdentityProbe, ProcessProbe};

/// 钩子的失败。
#[derive(Debug, Clone, thiserror::Error)]
pub enum HookError {
    /// 没有可用的模型：这一轮的输入以 `no_model` 收场（spec §8.3）。
    #[error("{0}")]
    NoModel(String),
    /// 别的失败：只让调用它的任务 `faulted`。
    #[error("{0}")]
    Failed(String),
}

/// 已经渲染好的一个系统段（带不带标签由钩子决定，spec §7.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSection {
    pub key: String,
    pub text: String,
}

impl PromptSection {
    pub fn new(key: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            text: text.into(),
        }
    }
}

/// 模型请求的有界退避（§8.5，同 [`RetryBudget`]，但可序列化：它随 `request` 的
/// checkpoint 冻结）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryPolicy {
    /// 最多请求几次（含第一次）：到了就是 failed，不是无限循环。
    pub max_attempts: u32,
    pub base_ms: u64,
    pub max_wait_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy::from(&RetryBudget::default())
    }
}

impl From<&RetryBudget> for RetryPolicy {
    fn from(budget: &RetryBudget) -> Self {
        Self {
            max_attempts: budget.max_attempts,
            base_ms: budget.base.as_millis() as u64,
            max_wait_ms: budget.max_wait.as_millis() as u64,
        }
    }
}

impl RetryPolicy {
    /// 第 `attempt` 次（从 1 数）失败之后等多久：`base * 2^(attempt-1)`，封顶 `max_wait`。
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        self.base_ms
            .saturating_mul(1u64 << attempt.saturating_sub(1).min(16))
            .min(self.max_wait_ms)
    }

    /// 第 `attempt` 次失败之后还允许再来一次吗。
    pub fn allows_another(&self, attempt: u32) -> bool {
        attempt < self.max_attempts
    }
}

/// 一次 Run 的预算（§6；总轮数默认不设上限）。**重启不重置**：计数在 `komo.live.run`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunBudget {
    /// 这条 Run 最多跑几轮模型；`None` = 不设。
    pub max_rounds: Option<u32>,
    /// 这条 Run 的 token 上限（输入 + 输出）；`None` = 不设。
    pub max_tokens: Option<u64>,
    pub retry: RetryPolicy,
}

/// 工具执行的环境：由 Gateway 装配，**不能由模型参数提升**（§4）。
#[derive(Clone)]
pub struct ToolEnv {
    pub session: SessionId,
    pub source: PlanSource,
    pub cwd: PathBuf,
    pub roots: Vec<WorkspaceRoot>,
    pub mounts: ResourceMounts,
    /// 工具结果怎么投影给模型（§8.3）；收尾时算一次，存进结果 entry。
    pub projection: ProjectionContext,
    pub call_timeout: Duration,
    pub detach_after: Option<Duration>,
    /// 对话会话里直接受理的 Run（`bash` 拒绝 10 秒以上的 `sleep`）。
    pub conversation: bool,
    /// 这个 Session 的工具输出目录（`tool-output/{conversation}/{tool_task}/{attempt}/`）。
    pub outputs: Arc<dyn ToolOutputStore>,
    /// 图片引用 → 字节（组请求时读回，§3.5）。
    pub images: Option<Arc<dyn ImageResolver>>,
}

impl std::fmt::Debug for ToolEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolEnv")
            .field("session", &self.session)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

/// 这次运行的工具表：实现、能力面（允许哪些名字）与执行环境。
#[derive(Clone)]
pub struct ToolSet {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    surface: AgentSurface,
    pub env: ToolEnv,
}

impl ToolSet {
    pub fn new(tools: Vec<Arc<dyn Tool>>, surface: AgentSurface, env: ToolEnv) -> Self {
        Self {
            tools: tools
                .into_iter()
                .map(|tool| (tool.definition().name, tool))
                .collect(),
            surface,
            env,
        }
    }

    pub fn surface(&self) -> &AgentSurface {
        &self.surface
    }

    /// 能力边界**先于**"有没有这个工具"（§4 末）：不在能力面里就等于不认识这个名字。
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        if self.surface.allows(name) {
            self.tools.get(name)
        } else {
            None
        }
    }

    /// 能力面里有 `bash`、这个工具表也装着它：`then_run` 的放行问它。
    pub fn can_run_commands(&self) -> bool {
        self.get("bash").is_some()
    }

    /// 交给模型的工具声明：按能力面的顺序、只列直接声明（`exposure = direct`）且真有实现的。
    /// 没有 `bash` 时 `edit` / `write` 的 `then_run` 参数一并摘掉（同 `ToolExecutor`）。
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let commands = self.can_run_commands();
        self.surface
            .names()
            .iter()
            .filter_map(|name| self.tools.get(name))
            .filter(|tool| tool.exposure() == ToolExposure::Direct)
            .map(|tool| {
                let mut definition = tool.definition();
                if !commands
                    && let Some(properties) = definition
                        .parameters
                        .get_mut("properties")
                        .and_then(serde_json::Value::as_object_mut)
                {
                    properties.remove("then_run");
                }
                definition
            })
            .collect()
    }

    /// 同一轮里能不能和后面的调用同时在飞（§6："只读的并行，有副作用的与 codemode 顺序"）。
    /// 工具要等 `prepare` 才知道自己的计划是不是只读，但一轮要在**建任务之前**就分好组，
    /// 所以按工具现在声明的重放策略近似：`replay = safe`（读、搜索）且不是 codemode 的算只读。
    /// 工具表里没有的名字不挡别人（它会立刻被拒）。
    pub fn parallel_safe(&self, name: &str) -> bool {
        match self.get(name) {
            None => true,
            Some(tool) => {
                name != crate::tools::codemode::CODEMODE_TOOL && tool.replay() == Replay::Safe
            }
        }
    }
}

/// `prepare` 要的全部：钩子在每一轮生成之前交回来。
#[derive(Clone)]
pub struct AgentSetup {
    /// 系统段，按渲染次序（spec §7.4：只有这些是提示文字的来源）。
    pub sections: Vec<PromptSection>,
    pub tools: ToolSet,
    /// 这一轮用的模型（含 effort）。
    pub model: ModelConfig,
    pub budget: RunBudget,
    /// 提示缓存的路由键（通常是会话 id，fork 任务给派它的会话）。
    pub cache_key: Option<String>,
    pub max_output_tokens: Option<u32>,
}

#[async_trait]
pub trait AgentHooks: Send + Sync {
    /// 生成的 `prepare` 问：conversation 此刻的 agent 是什么样。`context` 是已提交上下文
    /// 推导出的消息（带 system 消息），渲染"只在变了才重写"的段时要看已经展示过什么。
    async fn setup(
        &self,
        conversation: ConversationId,
        context: &[Message],
    ) -> Result<AgentSetup, HookError>;

    /// 只要工具表（工具任务、图片解析）。默认从 [`Self::setup`] 里取；钩子可以覆盖成更便宜的。
    async fn tools(&self, conversation: ConversationId) -> Result<ToolSet, HookError> {
        Ok(self.setup(conversation, &[]).await?.tools)
    }
}

/// 队列模式（spec §6 boundary 选择）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QueueMode {
    /// 每个 boundary 取该模式的第一项。
    #[default]
    OneAtATime,
    /// 取该模式的全部。
    All,
}

/// Harness 的设置；每个决定点现读（`Harness::set_settings` 热换）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessSettings {
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    /// 部分回复的节流提交间隔（§3.6 默认 250 ms）。
    pub partial_interval: Duration,
    /// 工具输出尾巴的节流提交间隔。
    pub output_interval: Duration,
    /// 槽位里保留的输出尾巴最多多少字节。
    pub tail_bytes: usize,
    /// `probe` 阶段两次探测的间隔（§5：每轮探一次，`sleep(until)` 后再探）。
    pub probe_interval: Duration,
}

impl Default for HarnessSettings {
    fn default() -> Self {
        Self {
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            partial_interval: Duration::from_millis(250),
            output_interval: Duration::from_millis(250),
            tail_bytes: 4096,
            probe_interval: Duration::from_secs(5),
        }
    }
}

/// 生成与工具任务共用的依赖：钩子、无状态的模型调用、子进程探测、设置。
pub struct HarnessDeps {
    pub hooks: Arc<dyn AgentHooks>,
    /// 按模型配置路由（生产是 [`RoutingStream`](crate::llm::RoutingStream)）。
    pub stream: Arc<dyn crate::llm::ModelStream>,
    /// 工具任务 `probe` 阶段探子进程用的（生产是带身份核对的 [`IdentityProbe`]，测试里换替身）。
    pub probe: Arc<dyn ProcessProbe>,
    /// codemode 的 worker 怎么起（`docs/codemode.md` §4）；`None` = 这台 Gateway 没有接 codemode，
    /// `codemode` 工具的任务以 Failed 收场。热换：Gateway 重装配时 [`Self::set_codemode`]。
    codemode: arc_swap::ArcSwapOption<crate::codemode::Worker>,
    settings: arc_swap::ArcSwap<HarnessSettings>,
}

impl HarnessDeps {
    pub fn new(hooks: Arc<dyn AgentHooks>, stream: Arc<dyn crate::llm::ModelStream>) -> Self {
        Self {
            hooks,
            stream,
            probe: Arc::new(IdentityProbe),
            codemode: arc_swap::ArcSwapOption::empty(),
            settings: arc_swap::ArcSwap::from_pointee(HarnessSettings::default()),
        }
    }

    pub fn with_probe(mut self, probe: Arc<dyn ProcessProbe>) -> Self {
        self.probe = probe;
        self
    }

    pub fn with_codemode(self, worker: Option<crate::codemode::Worker>) -> Self {
        self.set_codemode(worker);
        self
    }

    pub fn with_settings(self, settings: HarnessSettings) -> Self {
        self.settings.store(Arc::new(settings));
        self
    }

    /// 当前设置：每个决定点现读。
    pub fn settings(&self) -> Arc<HarnessSettings> {
        self.settings.load_full()
    }

    /// codemode 的 worker：每次脚本现读。
    pub fn codemode(&self) -> Option<crate::codemode::Worker> {
        self.codemode.load_full().map(|worker| (*worker).clone())
    }

    pub fn set_codemode(&self, worker: Option<crate::codemode::Worker>) {
        self.codemode.store(worker.map(Arc::new));
    }

    pub fn set_settings(&self, settings: HarnessSettings) {
        self.settings.store(Arc::new(settings));
    }
}
