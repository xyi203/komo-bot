//! 对外动作确认（`docs/confirm.md`）：无人值守的 Run 调对外工具，`prepare` 之后、提交意图之前停在
//! `confirm` 阶段，等操作者的答复。
//!
//! 答复只写一份 conversation 级 document（`komo.confirm`，key = 工具任务 ID），不碰任务；工具任务
//! 每拍读它。任务结束后 document 留着，事后能查谁、什么时候、答了什么。

use komo_kernel::durable::{
    ConversationId, DocumentAddress, DocumentScope, InputState, SubmissionKind, TaskId,
};
use serde::{Deserialize, Serialize};

use super::hooks::ToolEnv;
use super::live;
use super::orchestration::TASK_RESULT_ENTRY;
use super::orchestration::monitor::{MONITOR_END_ENTRY, MONITOR_EVENT_ENTRY};
use crate::durable::error::TaskError;
use crate::durable::runtime::TaskRuntime;

pub const CONFIRM_KIND: &str = "komo.confirm";
pub const CONFIRM_VERSION: u32 = 1;

/// 不是人打的字起的 Run（`docs/confirm.md` §3）。`cron.input` 是 gateway 的 kind，这里只认名字。
const UNATTENDED_INPUTS: [&str; 4] = [
    "cron.input",
    MONITOR_EVENT_ENTRY,
    MONITOR_END_ENTRY,
    TASK_RESULT_ENTRY,
];

pub fn confirm_address(conversation: ConversationId, task: TaskId) -> DocumentAddress {
    DocumentAddress {
        kind: CONFIRM_KIND.to_owned(),
        scope: DocumentScope::Conversation { conversation },
        key: Some(task.to_string()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmVerdict {
    Approve,
    Reject,
}

/// `komo.confirm` 的值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmAnswer {
    pub verdict: ConfirmVerdict,
    /// 谁答的：渠道 peer（`feishu:ou_…`）或 `cli`。
    pub by: String,
    /// 答复时刻（Unix 毫秒）。
    pub at: i64,
}

/// 这次 Run 有没有人在看：对话会话里、第一条输入是人打的字。出身看第一条，后来的插话不改它。
/// 只读已提交的事实，重启后算出来一样。
pub(super) async fn attended(env: &ToolEnv, rt: &TaskRuntime) -> Result<bool, TaskError> {
    if !env.conversation {
        return Ok(false);
    }
    let conversation = rt.conversation();
    let Some(document) = rt.find_document(&live::live_address(conversation)).await? else {
        return Ok(false);
    };
    let live: live::LiveState = serde_json::from_value(document.value)
        .map_err(|error| TaskError::new(format!("invalid live Run: {error}")))?;
    let Some(first) = live.run.and_then(|run| run.inputs.first().copied()) else {
        return Ok(false);
    };
    let entry = match rt.submission(first).await?.map(|record| record.kind) {
        Some(SubmissionKind::Input {
            state: InputState::Placed { entry } | InputState::Done { entry, .. },
        }) => entry,
        _ => return Ok(false),
    };
    let Some(entry) = rt.entry(entry).await? else {
        return Ok(false);
    };
    Ok(!UNATTENDED_INPUTS.contains(&entry.kind.as_str()))
}

/// 已有的答复（没答是 `None`）。
pub(super) async fn answer(rt: &TaskRuntime) -> Result<Option<ConfirmAnswer>, TaskError> {
    let address = confirm_address(rt.conversation(), rt.task_id());
    let Some(document) = rt.find_document(&address).await? else {
        return Ok(None);
    };
    serde_json::from_value(document.value)
        .map(Some)
        .map_err(|error| TaskError::new(format!("invalid {CONFIRM_KIND}: {error}")))
}

pub(super) fn rejected(tool: &str) -> String {
    format!("操作者拒绝了这次 {tool} 调用，没有执行。")
}

pub(super) fn timed_out(tool: &str, minutes: i64) -> String {
    format!("{tool} 是对外动作，等了 {minutes} 分钟没有确认，没有执行。")
}

pub(super) fn not_in_scripts(tool: &str) -> String {
    format!(
        "{tool} 是对外动作，这次 Run 没有人在看，需要确认；脚本里等不了确认。请在顶层直接调用 {tool}。"
    )
}
