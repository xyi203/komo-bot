//! 在线上下文压缩的纯逻辑（移植自 SoL-Pi 的 Online Context Compact）。
//!
//! 只在上下文贴着窗口时把早先的上下文收成摘要（2026-10-16 起；之前还以 `update_plan`
//! 的计划步骤完成为边界，那个工具已经退役）。这里只有不碰 I/O 的部分：
//!
//! - [`economics`]：压或不压的决策；
//! - [`state`]：从事件日志折出一条 Run 的在线状态；
//! - [`cut`]：token 粗估。
//!
//! 切点与估价（entry 级）在 `durable::compaction`，摘要提示在 `komo-agent`，摘要请求由 runtime
//! 的 harness 发出。大结果换短视图（衰减，§8.3）同样是改写提示前缀，走同一个决策点、同一本缓存账。

pub mod cut;
pub mod economics;
pub mod state;

pub use cut::{IMAGE_TOKENS, estimate_tokens};
pub use economics::{
    CompactionDebt, CompactionDecision, CompactionEconomics, CompactionInput, CompactionReason,
    RequestHorizon, decide, effective_window, estimate_remaining_requests,
};
pub use state::{OnlineFold, OnlineState, online_state};

/// 提示缓存的存活时间。Anthropic `ephemeral` 默认 5 分钟、OpenAI 自动前缀缓存同量级；
/// 按最短的算——估短了只会少一次免费改写，估长了会在缓存还热时改写。
pub const PROMPT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);
