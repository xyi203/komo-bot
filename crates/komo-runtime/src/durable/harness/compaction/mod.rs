//! 压缩（D4c；`docs/storage-durable.md` §4.4、§4.5、§12.3；pi-durable spec §8.7）。
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`setup`] | 钩子交回来的值：设置、摘要提示、摘要模型（[`AgentHooks::compaction`](super::AgentHooks::compaction)） |
//! | [`plan`] | 压不压、换不换：kernel 的缓存账接到 entry 推导出的上下文上（旧实现的 `komo-agent::plan`） |
//! | [`task`] | `komo.compaction` 任务：阻塞与后台两种，摘要请求走无状态 `ModelStream` |
//!
//! 纯逻辑（切点、摘要输入序列化、衰减候选与 `edits`、在线状态折账）在
//! `komo_kernel::durable::compaction`。
//!
//! **状态放在哪**：旧实现的在线状态（请求次数、上下文增量、缓存债、冷却）是从事件日志折出来的，
//! 现在从 entry 折（`online_state`）——请求次数与上下文大小读 assistant 消息的用量，压缩与衰减
//! 的债记在各自 entry 的 `data.debt`。没有第二份真相，重启、fork 之后重新折一遍就回来。唯一不在
//! entry 里的是"刚压失败过"（旧的 `compaction_refused`）：它在 `komo.live.compactionFailure`
//! （任务 ID 当位置标记，ID 与 entry 同一计数）。

pub mod plan;
pub mod setup;
pub mod task;

pub use plan::{Plan, PlanInput, plan, plan_overflow};
pub use setup::{CompactionSettings, CompactionSetup, SummaryPrompt};
pub use task::{CompactionTask, Input, Reason, Settled};

use komo_kernel::durable::{ConversationId, EntryId};

use super::live::{self, LiveState};
use crate::durable::error::TaskError;
use crate::durable::runtime::TaskRuntime;

/// 已提交的 `komo.live`（阶段里读，不经过提交）。
pub async fn read_live(
    rt: &TaskRuntime,
    conversation: ConversationId,
) -> Result<LiveState, TaskError> {
    match rt.find_document(&live::live_address(conversation)).await? {
        None => Ok(LiveState::default()),
        Some(record) => serde_json::from_value(record.value)
            .map_err(|e| TaskError::new(format!("komo.live is malformed: {e}"))),
    }
}

/// 旧的 `compaction_refused` 的位置标记：最近一次压缩失败的那个任务（任务 ID 与 entry ID 同一
/// 计数，所以能和 entry 比先后）。
pub fn refused_after(live: &LiveState) -> Option<EntryId> {
    live.compaction_failure
        .as_ref()
        .map(|failure| EntryId::new(failure.task.get()))
}
