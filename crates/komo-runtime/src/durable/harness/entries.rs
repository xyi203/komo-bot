//! 内置 entry（`docs/storage-durable.md` §4.1；pi-durable spec §8.1）。
//!
//! | kind | `model` | 谁写 |
//! |---|---|---|
//! | `komo.user` | `[UserMessage]` | submission 放置 |
//! | `komo.assistant` | `[AssistantMessage]`，任何停止原因 | 生成 |
//! | `komo.system` | `[SystemMessage]`（`content` 恒为空，§2） | 生成的 `prepare` |
//! | `komo.tool-result` | `[ToolResultMessage]` | 工具任务；生成对没跑过的调用 |
//! | `komo.reset` | 无，或 `[UserMessage]`；恒带 `head: "self"` | `/new`、交接 |
//!
//! 每条生成回复都是一条 `komo.assistant`：回答、失败的尝试（带错误文字与用量）、转成
//! `aborted` 的部分回复。上下文推导（规则 9）把失败与中断的消息挡在之后的请求之外，所以
//! 没有单独的用量或通知 entry。

use komo_kernel::durable::{
    AssistantMessage, Content, ContentBlock, ConversationId, EntryRecord, Message,
    ToolResultMessage,
};
use serde_json::json;

use super::inbox::InputEntry;
use crate::durable::error::DurableError;
use crate::durable::tx::{EntryDraft, Tx};

pub const USER: &str = "komo.user";
pub const ASSISTANT: &str = "komo.assistant";
pub const TOOL_RESULT: &str = "komo.tool-result";
pub const RESET: &str = "komo.reset";
pub use komo_kernel::durable::context::SYSTEM_ENTRY_KIND as SYSTEM;

/// 输入放置成的 entry：`entry` 给了的 kind 与 data，或 `komo.user`；`model` 是 `content`
/// 的 user 消息。
pub fn input_draft(content: &Content, entry: Option<&InputEntry>, now: i64) -> EntryDraft {
    let message = Message::User(komo_kernel::durable::UserMessage {
        content: content.clone(),
        timestamp: now,
        extra: Default::default(),
    });
    let mut draft = EntryDraft::new(entry.map_or(USER, |entry| entry.kind.as_str()));
    draft.model = Some(vec![message]);
    draft.data = entry.and_then(|entry| entry.data.clone());
    draft
}

pub async fn append_assistant(
    tx: &mut Tx,
    conversation: ConversationId,
    message: AssistantMessage,
) -> Result<EntryRecord, DurableError> {
    tx.append_entry(
        conversation,
        EntryDraft::new(ASSISTANT).with_model(vec![Message::Assistant(message)]),
    )
    .await
}

/// 生成自己写的错误结果（没有 task、没有落盘输出）：对话里的"这个调用没跑，因为……"。
/// 只在没有工具任务可交给的地方用（生成被取消时没开始的调用）。
pub async fn append_harness_error(
    tx: &mut Tx,
    conversation: ConversationId,
    call_id: &str,
    tool: &str,
    code: &str,
    message: &str,
) -> Result<EntryRecord, DurableError> {
    let result = ToolResultMessage {
        tool_call_id: call_id.to_owned(),
        tool_name: tool.to_owned(),
        content: vec![ContentBlock::text(message)],
        is_error: true,
        details: Some(json!({ "reason": code })),
        timestamp: tx.now(),
        extra: Default::default(),
    };
    tx.append_entry(
        conversation,
        EntryDraft::new(TOOL_RESULT)
            .with_model(vec![Message::ToolResult(result)])
            .with_data(json!({ "harness": code })),
    )
    .await
}
