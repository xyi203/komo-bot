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
