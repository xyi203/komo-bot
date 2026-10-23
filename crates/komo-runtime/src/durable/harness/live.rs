//! `komo.live`（`docs/storage-durable.md` §3.3；pi-durable spec §8.2）：conversation 级的
//! 可变状态，整值替换。
//!
//! ```json
//! {
//!   "run":        { "task": 7, "inputs": [3], "rounds": 2, "tokens": 1840 },
//!   "generation": { "attempt": 1, "message": { …节流提交的部分回复… },
//!                   "retry": { "at": 1761206400000, "error": "…" } },
//!   "tools": [ { "callId": "c1", "name": "read", "taskId": 9, "status": "running",
//!                "replay": "safe", "attempt": 1, "pid": null,
//!                "output": "…输出尾巴…", "droppedBytes": 0 },
//!              { "callId": "c2", "name": "bash", "status": "done", "entry": 31 } ]
//! }
//! ```
//!
//! - **`run` 在不在就是 conversation 忙不忙**（spec §6）。`task` 是现在对这条 run 负责的
//!   生成任务，`inputs` 是它放置过、收尾时要 settle 的输入 submission（boundary 往里加 steer）。
//!   `rounds` / `tokens` 是这条 run 已经花的轮数与 token——**重启不重置预算**（§8.5），所以
//!   计数落在这里，生成任务交接时带着走。
//! - `generation` 是当前这次请求的展示状态：节流提交的部分回复、重试到点。
//! - `tools` 是当前这一轮的槽位，按调用顺序。槽位**就是意图**（§12.5）：`running` 时带着
//!   重放策略、尝试序号、子进程身份（先留字段，D5 的 probe 阶段用）与输出尾巴；`done` 时
//!   只剩结果 entry，输出尾巴随结果清掉。
//!
//! 进度写入（部分回复、输出尾巴）只替换这一个 document，经 `Line` 走 `commit_progress`。

use komo_kernel::durable::{
    AssistantMessage, ConversationId, DocumentAddress, DocumentScope, EntryId, StopReason,
    SubmissionId, SubmissionSettlement, TaskId,
};
use komo_kernel::types::plan::Replay;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{entries, inbox};
use crate::durable::error::DurableError;
use crate::durable::tx::Tx;

pub const LIVE_KIND: &str = "komo.live";
pub const LIVE_VERSION: u32 = 1;

pub fn live_address(conversation: ConversationId) -> DocumentAddress {
    DocumentAddress {
        kind: LIVE_KIND.to_owned(),
        scope: DocumentScope::Conversation { conversation },
        key: None,
    }
}

/// 对这条 run 负责的任务与它的输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunControl {
    pub task: TaskId,
    pub inputs: Vec<SubmissionId>,
    /// 这条 run 已经跑完的模型轮数（成功的回复）。
    #[serde(default)]
    pub rounds: u32,
    /// 已花的 token（输入 + 输出）。
    #[serde(default)]
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryLive {
    /// 到点时刻（毫秒）。
    pub at: i64,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationLive {
    pub attempt: u32,
    /// 节流提交的部分回复（assistant 消息 JSON）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryLive>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SlotStatus {
    Pending,
    Running,
    Done,
}

/// 一轮里一个调用的槽位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSlot {
    pub call_id: String,
    pub name: String,
    /// 还没轮到的调用（顺序组）没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    pub status: SlotStatus,
    /// 意图里记下的重放策略（计划的 `recovery.replay()`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<Replay>,
    /// 当前这次尝试（从 1 数）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// 子进程身份（D5 的 probe 阶段用；D4b 不写）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 运行中的输出尾巴（stdout / stderr 按到达次序）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// 尾巴之前丢掉的字节数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_bytes: Option<u64>,
    /// 结果 entry；工具任务 faulted / orphaned 时没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
}

impl ToolSlot {
    pub fn pending(call_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            task_id: None,
            status: SlotStatus::Pending,
            replay: None,
            attempt: None,
            pid: None,
            output: None,
            dropped_bytes: None,
            entry: None,
        }
    }

    /// 运行中发布的东西：结果 entry 或重跑会替掉它。
    pub fn clear_progress(&mut self) {
        self.output = None;
        self.dropped_bytes = None;
    }

    /// 标记完成并清掉运行中的输出；调用方设 `entry`。
    pub fn finish(&mut self) {
        self.status = SlotStatus::Done;
        self.clear_progress();
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunControl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationLive>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSlot>>,
}

impl LiveState {
    pub fn is_busy(&self) -> bool {
        self.run.is_some()
    }

    pub fn slot_mut(&mut self, task: TaskId) -> Option<&mut ToolSlot> {
        self.tools
            .as_mut()?
            .iter_mut()
            .find(|slot| slot.task_id == Some(task))
    }

    pub fn slot(&self, task: TaskId) -> Option<&ToolSlot> {
        self.tools
            .as_ref()?
            .iter()
            .find(|slot| slot.task_id == Some(task))
    }
}

/// 读 conversation 的 `komo.live`；没有就是空状态。
pub async fn load(tx: &mut Tx, conversation: ConversationId) -> Result<LiveState, DurableError> {
    match tx.document(&live_address(conversation)).await? {
        None => Ok(LiveState::default()),
        Some((_, value)) => serde_json::from_value(value)
            .map_err(|e| DurableError::rejected(format!("komo.live is malformed: {e}"))),
    }
}

/// 整值写回。文档还不存在、状态又是空的就不建。
pub async fn store(
    tx: &mut Tx,
    conversation: ConversationId,
    state: &LiveState,
) -> Result<(), DurableError> {
    let address = live_address(conversation);
    if *state == LiveState::default() && tx.document(&address).await?.is_none() {
        return Ok(());
    }
    let value = serde_json::to_value(state).expect("LiveState 可序列化");
    tx.put_document(address, LIVE_VERSION, value).await
}

/// 结束 `task` 负责的 run：把它的每个输入 settle，移除 `run`。**总是**一并清掉
/// `generation` 与 `tools`——它们的展示属于正在结束的 run。
pub async fn end_run(
    tx: &mut Tx,
    live: &mut LiveState,
    task: TaskId,
    settlement: &SubmissionSettlement,
) -> Result<(), DurableError> {
    if live.run.as_ref().is_some_and(|run| run.task == task) {
        let run = live.run.take().expect("刚判过它在");
        for id in run.inputs {
            inbox::settle(tx, id, settlement).await?;
        }
    }
    live.generation = None;
    live.tools = None;
    Ok(())
}

/// 把 `live.generation.message` 里已提交的部分回复转成一条 `aborted` 的 assistant entry
/// （spec §8.3：中断、取消、fault、orphan 都这样处理）。调用方随后替换或移除 `generation`。
pub async fn convert_partial(
    tx: &mut Tx,
    conversation: ConversationId,
    live: &mut LiveState,
) -> Result<(), DurableError> {
    let Some(partial) = live
        .generation
        .as_mut()
        .and_then(|generation| generation.message.take())
    else {
        return Ok(());
    };
    // 读不懂的部分回复丢掉：它只是展示。
    let Ok(mut message) = serde_json::from_value::<AssistantMessage>(partial) else {
        return Ok(());
    };
    message.stop_reason = StopReason::Aborted;
    entries::append_assistant(tx, conversation, message).await?;
    Ok(())
}
