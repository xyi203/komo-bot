//! agent 的生成与工具执行（D4b；`docs/storage-durable.md` §4.3–§4.5、§12.3–§12.5；
//! pi-durable spec §6、§8 的 `harness/{generation,tool,inbox,submissions,live}.ts`）。
//!
//! 跑在 D3 的通用 Task 引擎上：一条输入 = 一个 submission + `komo.live.run` + 一串
//! `komo.generation` 任务；每个模型调用是一个 `komo.tool` 任务。**loop 不是一个长跑的 async
//! 函数**，而是拆在提交点上——阶段之间的状态只在 checkpoint 里，重启后从 checkpoint 所在阶段继续。
//!
//! ```text
//! submit ─▶ komo.inbox / placed ─▶ komo.generation
//!                                    prepare ─▶ request ─▶ (retry ─▶ prepare)*
//!                                                   │ toolUse
//!                                                   ▼
//!                                    tools (waiting on komo.tool × N, 一组一组建)
//!                                      call ─▶ [意图] execute ─▶ 结果 entry
//!                                                   │ postTools boundary
//!                                                   ▼
//!                                    下一个 komo.generation（接手 run）  /  final boundary 收尾
//! ```
//!
//! # 边界
//!
//! - **runtime 不依赖 komo-agent**：系统段、工具与能力面、模型配置、预算都由钩子
//!   [`AgentHooks`] 给（D5 在 gateway 里实现，装配上下文的逻辑在钩子背后）。
//! - 模型调用是 D4a 的无状态 [`ModelStream`](crate::llm::ModelStream)；工具是今天的
//!   [`Tool`](komo_kernel::traits::Tool)（`prepare` / `execute` / `replay`）；完整输出落盘走
//!   [`ToolOutputStore`](komo_kernel::traits::ToolOutputStore)，模型视图由
//!   `komo_kernel::projection` 在收尾时算一次、存进结果 entry。
//! - 压缩、衰减、codemode 嵌套调用、子进程 probe 是 D4c / D5。

pub mod entries;
pub mod generation;
pub mod hooks;
pub mod inbox;
pub mod live;
pub mod outcome;
pub mod submit;
pub mod system;
pub mod tool;

#[cfg(test)]
mod tests;

pub use generation::GenerationTask;
pub use hooks::{
    AgentHooks, AgentSetup, HarnessDeps, HarnessSettings, HookError, PromptSection, QueueMode,
    RetryPolicy, RunBudget, ToolEnv, ToolSet,
};
pub use inbox::{InboxItem, InboxState, InputEntry, SubmissionDraft, WhenBusy, WriteDraft};
pub use live::{
    GenerationLive, LiveState, RetryLive, RunControl, SlotStatus, ToolSlot, live_address,
};
pub use submit::{Harness, HarnessHooks, SubmitError, WithdrawResult};
pub use tool::ToolTask;

/// 任务种类名（`TaskRecord::kind`）。
pub const GENERATION_KIND: &str = "komo.generation";
pub const TOOL_KIND: &str = "komo.tool";
