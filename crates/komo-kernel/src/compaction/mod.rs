//! 在线上下文压缩的纯逻辑（移植自 SoL-Pi 的 Online Context Compact）。
//!
//! 模型用 `update_plan` 报进度；**完成一个先前登记过的步骤**是一个安全点。在这个点上
//! 按缓存账决定要不要把早先的上下文收成摘要。这里只有不碰 I/O 的部分：
//!
//! - [`plan`]：计划的参数、校验与前后两版之间的转移；
//! - [`economics`]：压或不压的决策；
//! - [`state`]：从事件日志折出一条 Run 的在线状态；
//! - [`cut`]：token 粗估与切点。
//!
//! 价格怎么算（模型实际看到的那份视图）、摘要请求长什么样在 `komo-agent`；摘要请求
//! 由 runtime 的 loop 发出，结论经 `Ledger::record_compaction` 落成 `context.compacted`。
//!
//! 大结果换短视图（衰减，§8.3）同样是改写提示前缀，走同一个决策点、同一本缓存账：结论是
//! `context.compacted` 的 `decayed`，一次换一批（[`DecayJob`]）。

pub mod cut;
pub mod economics;
pub mod plan;
pub mod state;

pub use cut::{estimate_tokens, find_cut};
pub use economics::{
    CompactionDebt, CompactionDecision, CompactionEconomics, CompactionInput, CompactionReason,
    RequestHorizon, decide, estimate_remaining_requests,
};
pub use plan::{
    PlanError, PlanProgress, PlanStatus, PlanStep, PlanTransition, PlanUpdate, analyze_transition,
    format_snapshot,
};
pub use state::{OnlineFold, OnlineState, ProgressSummary, online_state};

/// 提示缓存的存活时间。Anthropic `ephemeral` 默认 5 分钟、OpenAI 自动前缀缓存同量级；
/// 按最短的算——估短了只会少一次免费改写，估长了会在缓存还热时改写。
pub const PROMPT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// 决定了要压：一次摘要请求，加上压成之后从哪一轮起原样保留。由 Gateway 装配、loop
/// 执行——摘要请求成了才记 `compacted`，失败或被截断记 `skipped`（带着这份决策）。
#[derive(Debug, Clone)]
pub struct CompactionJob {
    /// 没有工具；早先那段上下文渲染成一条纯文本用户消息。
    pub request: crate::types::turn::TurnRequest,
    /// 这条 Run 某一轮 `message.assistant` 的 seq。
    pub first_kept: crate::types::ids::Seq,
    pub decision: CompactionDecision,
}

/// 决定了把一批结果换成短视图。由 Gateway 装配、loop 执行：先记 `decayed`，再把
/// `revised` 交给驱动就地换正文——换上的字节就是回放按已衰减集合给出的那一份。
#[derive(Debug, Clone)]
pub struct DecayJob {
    pub calls: Vec<crate::types::ids::ToolCallId>,
    /// 每条结果的短视图，按 `provider_call_id` 就地换。
    pub revised: Vec<crate::types::turn::ToolResultForModel>,
    pub decision: CompactionDecision,
}

impl DecayJob {
    /// 落账的那条事件。
    pub fn outcome(&self) -> crate::events::ContextCompacted {
        crate::events::ContextCompacted::Decayed {
            calls: self.calls.clone(),
            debt: self.decision.debt(),
            decision: self.decision.clone(),
        }
    }
}

/// 一次对提示前缀的改写：压成摘要，或把一批结果换成短视图。
// 每次判断只造一个、立刻消费，装箱省不了什么。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Reshape {
    Compact(CompactionJob),
    Decay(DecayJob),
}
