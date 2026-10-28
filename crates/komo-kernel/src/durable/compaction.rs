//! 压缩的纯逻辑（`docs/storage-durable.md` §4.4、§4.5；pi-durable spec §8.7 的范围选择）：
//! 作用在 [`ContextView`] 与 entry 记录上，没有 I/O。
//!
//! - **切点**（[`select_cut`]）：pi 的 `selectCut`——候选是以 user / assistant 消息开头的 entry，
//!   工具结果与 system entry 从不是候选，所以留下的 assistant 带着它的工具结果；从最新往回数够
//!   `keep_recent_tokens` 之后，切在那里或其后的第一个候选。
//! - **摘要输入**（[`summarized_messages`]、[`serialize_conversation`]）：切点之前的消息按上下文
//!   次序排好，序列化成纯文本（`[User]` / `[Assistant]` / `[Assistant tool calls]` / `[Tool result]`）。
//! - **衰减候选**（[`decay_candidates`]）：结果 entry 的 `data.decayed` 已经存了短视图，这里只回答
//!   "哪些有资格换"，并把它们写成 `replace` 的 `edits`（[`decay_edits`]）。
//! - **在线状态**（[`online_state`]）：旧实现从事件日志折出来的 [`OnlineState`]，现在从 entry 折
//!   ——请求次数与上下文大小来自 assistant 消息的用量，压缩与衰减的债记在各自 entry 的
//!   `data.debt` 里。**不单独存**：重启、fork 之后重新折一遍就回来了，没有第二份真相会漂。

use serde_json::{Value, json};

use super::context::{ContextView, order_tool_results};
use super::ids::EntryId;
use super::message::{Content, ContentBlock, Message, StopReason, ToolResultMessage};
use super::records::{ContextEdit, EntryRecord};
use crate::compaction::{CompactionDebt, IMAGE_TOKENS, OnlineState, estimate_tokens};
use crate::projection::DecayPolicy;

/// 压缩摘要 entry 的 `kind`：`head` = 第一条保留的 entry，`model` = 包好的摘要 user 消息。
pub const COMPACTION_ENTRY_KIND: &str = "komo.compaction";
/// 衰减 entry 的 `kind`：只带 `edits` 的 write entry，把有资格的结果换成短视图。
pub const DECAY_ENTRY_KIND: &str = "komo.decay";

/// 摘要消息的包装（pi 的 `SUMMARY_PREFIX` / `SUMMARY_SUFFIX`）。
pub const SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const SUMMARY_SUFFIX: &str = "\n</summary>";

/// 工具结果在摘要输入里最多这么多字符（pi 的 `TOOL_RESULT_MAX_CHARS`）。
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// 摘要消息的正文。
pub fn wrap_summary(summary: &str) -> String {
    format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}")
}

// ------------------------------------------------------------------ 估价

fn blocks_tokens(blocks: &[ContentBlock]) -> u64 {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => estimate_tokens(text),
            ContentBlock::Thinking { thinking, .. } => estimate_tokens(thinking),
            ContentBlock::ToolCall {
                name, arguments, ..
            } => estimate_tokens(name) + estimate_tokens(&arguments.to_string()),
            ContentBlock::Image { .. } => IMAGE_TOKENS,
        })
        .sum()
}

/// 一条消息发出去有多大（字节 / 4；图片按 [`IMAGE_TOKENS`]）。助手那一轮存过原生块的，发出去
/// 的就是那些块（含推理块），按它们算——与旧实现同一把尺子。
pub fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::User(user) => match &user.content {
            Content::Text(text) => estimate_tokens(text),
            Content::Blocks(blocks) => blocks_tokens(blocks),
        },
        Message::Assistant(assistant) => match assistant.extra.get("providerBlocks") {
            Some(blocks) => estimate_tokens(&blocks.to_string()),
            None => blocks_tokens(&assistant.content),
        },
        Message::ToolResult(result) => blocks_tokens(&result.content),
        Message::System(system) => {
            let content = match &system.content {
                Content::Text(text) => estimate_tokens(text),
                Content::Blocks(blocks) => blocks_tokens(blocks),
            };
            let sections: u64 = system
                .sections
                .iter()
                .flat_map(|sections| sections.0.iter())
                .map(|(name, text)| {
                    estimate_tokens(name) + text.as_deref().map_or(0, estimate_tokens)
                })
                .sum();
            let tools: u64 = system
                .tools_added
                .iter()
                .flatten()
                .map(|tool| estimate_tokens(&tool.to_string()))
                .sum();
            content + sections + tools
        }
    }
}

pub fn estimate_messages_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

// ------------------------------------------------------------------ 切点

/// 切在哪、收掉多少。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cut {
    /// 第一条保留的 entry 在 `view.entries` 里的下标。
    pub index: usize,
    /// 第一条保留的 entry：摘要 entry 的 `head`。
    pub first_kept: EntryId,
    /// 切点之前、要收成摘要的那一段的估价（含上一次的摘要）。
    pub archive_tokens: u64,
    /// `view.entries` 里最新的一条：摘要请求从这里往前读的、不可变的上下文终点。
    pub tail: EntryId,
}

/// 这条 entry 的贡献是不是以 user / assistant 消息开头，且不会和前一条 assistant 的调用结果
/// 拆开（pi 的 `isCandidate`）。
fn is_candidate(contributions: &[Vec<Message>], index: usize) -> bool {
    match contributions[index].first() {
        Some(Message::Assistant(_)) => return true,
        // 宿主附上的记忆块紧跟着它那条输入：切在它前面会把输入收进摘要、只留下记忆块。
        Some(message) if message.is_host_injected() => return false,
        Some(Message::User(_)) => {}
        _ => return false,
    }
    // 前一条 assistant 的调用的结果若跟在这条 user 之后、下一条 assistant 之前，它们属于一起。
    let calls: Vec<&str> = contributions[..index]
        .iter()
        .rev()
        .find_map(|messages| {
            messages.iter().rev().find_map(|message| match message {
                Message::Assistant(assistant) => {
                    Some(assistant.tool_calls().map(|(id, _)| id).collect())
                }
                _ => None,
            })
        })
        .unwrap_or_default();
    if calls.is_empty() {
        return true;
    }
    for (after, messages) in contributions.iter().enumerate().skip(index) {
        for (position, message) in messages.iter().enumerate() {
            match message {
                Message::Assistant(_) if after > index || position > 0 => return true,
                Message::ToolResult(result) if calls.contains(&result.tool_call_id.as_str()) => {
                    return false;
                }
                _ => {}
            }
        }
    }
    true
}

/// 切点（pi-durable spec §8.7 的 range selection）：从最新往回数够 `keep_recent_tokens`，切在
/// 够数的那条或其后的第一个候选，没有就取最新的候选。够不到（整段还不到 `keep_recent_tokens`）、
/// 或切点之前没有任何消息可收时返回 `None`。
pub fn select_cut(view: &ContextView, keep_recent_tokens: u64) -> Option<Cut> {
    let contributions = &view.contributions;
    let start = usize::from(view.head.is_some());
    let candidates: Vec<usize> = (start..contributions.len())
        .filter(|&index| is_candidate(contributions, index))
        .collect();
    let mut kept = 0u64;
    let mut cut = None;
    for index in (start..contributions.len()).rev() {
        kept = kept.saturating_add(estimate_messages_tokens(&contributions[index]));
        if kept < keep_recent_tokens {
            continue;
        }
        cut = candidates
            .iter()
            .copied()
            .find(|&candidate| candidate >= index)
            .or_else(|| candidates.last().copied());
        break;
    }
    let cut = cut?;
    if !contributions[start..cut]
        .iter()
        .any(|messages| !messages.is_empty())
    {
        return None;
    }
    Some(Cut {
        index: cut,
        first_kept: view.entries[cut],
        archive_tokens: contributions[..cut]
            .iter()
            .map(|messages| estimate_messages_tokens(messages))
            .sum(),
        tail: view
            .entries
            .iter()
            .copied()
            .max()
            .unwrap_or(view.entries[cut]),
    })
}

/// 切点之前那些 entry 的消息，按上下文次序排好（head 标记在最前，工具结果跟在各自的调用后）。
pub fn summarized_messages(view: &ContextView, cut_index: usize) -> Vec<Message> {
    let flat: Vec<Message> = view.contributions[..cut_index]
        .iter()
        .flatten()
        .cloned()
        .collect();
    order_tool_results(&flat)
}

fn content_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Blocks(blocks) => blocks_text(blocks),
    }
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_owned();
    }
    let shown: String = text.chars().take(max_chars).collect();
    format!(
        "{shown}\n\n[... {} more characters truncated]",
        total - max_chars
    )
}

/// 消息序列化成纯文本，摘要模型读的是一份记录而不是接着聊（pi 的 `serializeConversation`）。
/// system 消息与思考块都不进：回放的思考常常不在真实上下文里，带上它会让摘要请求比它要概括的
/// 上下文还大。
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for message in messages {
        match message {
            // 宿主附上的记忆块不是用户说的，也不该被摘要成对话的一部分（§9.4）。
            Message::User(user) if user.host_origin().is_some() => {}
            Message::User(user) => {
                let text = content_text(&user.content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let text = blocks_text(&assistant.content);
                let calls: Vec<String> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall {
                            name, arguments, ..
                        } => {
                            let args = arguments
                                .as_object()
                                .map(|map| {
                                    map.iter()
                                        .map(|(key, value)| format!("{key}={value}"))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            Some(format!("{name}({args})"))
                        }
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let text = blocks_text(&result.content);
                if !text.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate(&text, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            Message::System(_) => {}
        }
    }
    parts.join("\n\n")
}

// ------------------------------------------------------------------ 衰减

/// 一条有资格换成短视图、但还没换的工具结果：改写决策的原料。
#[derive(Debug, Clone, PartialEq)]
pub struct DecayCandidate {
    pub entry: EntryId,
    /// 它的消息在 `view.messages` 里的下标：改写要从这里起重写缓存前缀。
    pub message_index: usize,
    /// 现在发出去的（完整）视图有多大。
    pub full_tokens: u64,
    /// 换上去的短视图正文（`data.decayed`）。
    pub decayed: String,
    /// 换上去的整条消息：原结果，正文换成短视图，非文字块（图片引用）留在原处。
    pub replacement: ToolResultMessage,
}

/// 范围里已经被某条 edit 动过的 entry（衰减过的，或别的编辑）：不再是候选。
fn edited_targets(entries: &[EntryRecord], view: &ContextView) -> Vec<EntryId> {
    let from = view
        .head
        .and_then(|head| entries.iter().find(|entry| entry.id == head))
        .and_then(|marker| marker.head);
    entries
        .iter()
        .filter(|entry| from.is_none_or(|from| entry.id >= from))
        .flat_map(|entry| entry.edits.iter().map(|edit| edit.target))
        .collect()
}

/// 当前上下文里有资格换成短视图的结果：`data.decayed` 在、没被编辑过、完整给过至少
/// `policy.full_sends` 次（= 它之后的 assistant 消息数，每一条都是带着它发出去的一次请求）。
/// `entries` 是 `view` 推导所用的可见 entry。按 entry 次序。
pub fn decay_candidates(
    entries: &[EntryRecord],
    view: &ContextView,
    policy: &DecayPolicy,
) -> Vec<DecayCandidate> {
    let edited = edited_targets(entries, view);
    let by_id: std::collections::HashMap<EntryId, &EntryRecord> =
        entries.iter().map(|entry| (entry.id, entry)).collect();
    // 每条 entry 的贡献在 view.messages 里的下标：靠消息本身对位（同一条消息只出现一次，
    // 工具结果 `toolCallId` 唯一）。
    let mut candidates = Vec::new();
    for (position, id) in view.entries.iter().enumerate() {
        let Some(entry) = by_id.get(id) else { continue };
        let Some(decayed) = entry
            .data
            .as_ref()
            .and_then(|data| data.get("decayed"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if edited.contains(id) {
            continue;
        }
        let Some(result) = view.contributions[position]
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) => Some(result),
                _ => None,
            })
        else {
            continue;
        };
        let sends = view.contributions[position + 1..]
            .iter()
            .filter(|messages| messages.iter().any(|m| matches!(m, Message::Assistant(_))))
            .count() as u32;
        if sends < policy.full_sends {
            continue;
        }
        let mut replacement = result.clone();
        let mut content: Vec<ContentBlock> = vec![ContentBlock::text(decayed)];
        content.extend(
            result
                .content
                .iter()
                .filter(|block| !matches!(block, ContentBlock::Text { .. }))
                .cloned(),
        );
        replacement.content = content;
        let message_index = view
            .messages
            .iter()
            .position(|message| {
                matches!(message, Message::ToolResult(other) if other.tool_call_id == result.tool_call_id)
            })
            .unwrap_or(0);
        candidates.push(DecayCandidate {
            entry: *id,
            message_index,
            full_tokens: blocks_tokens(&result.content),
            decayed: decayed.to_owned(),
            replacement,
        });
    }
    candidates
}

/// 这批候选的 `replace` edits。
pub fn decay_edits(candidates: &[DecayCandidate]) -> Vec<ContextEdit> {
    candidates
        .iter()
        .map(|candidate| {
            ContextEdit::replace(
                candidate.entry,
                vec![Message::ToolResult(candidate.replacement.clone())],
            )
        })
        .collect()
}

// ------------------------------------------------------------------ 在线状态

fn debt_of(entry: &EntryRecord) -> CompactionDebt {
    entry
        .data
        .as_ref()
        .and_then(|data| data.get("debt"))
        .and_then(|debt| serde_json::from_value(debt.clone()).ok())
        .unwrap_or_default()
}

/// 一条 assistant 消息报告的提示 token 数（整个提示，含缓存）；没有用量是 `None`。
fn prompt_tokens(message: &super::message::AssistantMessage) -> Option<u64> {
    message.extra.get("usage")?.get("input")?.as_u64()
}

/// 把可见 entry（按 ID 升序）折成在线压缩状态。可切分：先折前缀再续折等于一次折完。
///
/// - 每条正常结束的 assistant 消息是一次 provider 请求（失败与中断的不算，它们没有用量）；
/// - `komo.compaction` / `komo.decay` entry 的 `data.debt` 记压缩与衰减欠下的缓存债；
/// - 完成了的工具结果（`isError` 为假）是真正的工具工作，解除"刚被拒过"；
/// - `head` 指自己的 entry（`/new`）是一次重新开始：状态归零；
/// - `refused_after`：最近一次压缩失败时的最新 entry——此后窗口压力不再触发重试，直到出现
///   真正的工具工作或一次成功的压缩（同一段上下文再请求一次摘要，多半还是同样的结果）。
pub fn online_state(entries: &[EntryRecord], refused_after: Option<EntryId>) -> OnlineState {
    let mut state = OnlineState::default();
    let mut refused_applied = refused_after.is_none();
    for entry in entries {
        if !refused_applied && refused_after.is_some_and(|after| entry.id > after) {
            state.record_skipped(true);
            refused_applied = true;
        }
        if entry.head == Some(entry.id) {
            state = OnlineState::default();
            continue;
        }
        match entry.kind.as_str() {
            COMPACTION_ENTRY_KIND => {
                state.record_compaction(debt_of(entry));
                continue;
            }
            DECAY_ENTRY_KIND => {
                state.record_decay(debt_of(entry));
                continue;
            }
            _ => {}
        }
        for message in entry.model.iter().flatten() {
            match message {
                Message::Assistant(assistant)
                    if assistant.stop_reason != StopReason::Error
                        && assistant.stop_reason != StopReason::Aborted
                        && assistant.stop_reason != StopReason::Deferred =>
                {
                    state.record_provider_request(prompt_tokens(assistant));
                }
                Message::ToolResult(result) if !result.is_error => state.record_tool_work(),
                _ => {}
            }
        }
    }
    if !refused_applied {
        state.record_skipped(true);
    }
    state
}

/// 可续折的全历史账本。拒绝标记来自可变的 `komo.live`，读取时才叠加；记录最后一次能
/// 解除拒绝的 entry，就不必为了新的 `refused_after` 重读 head 之前的历史。
#[derive(Debug, Clone, Default)]
pub struct OnlineFold {
    state: OnlineState,
    last_clear: Option<EntryId>,
}

impl OnlineFold {
    pub fn state(&self, refused_after: Option<EntryId>) -> OnlineState {
        let mut state = self.state.clone();
        if refused_after.is_some_and(|after| self.last_clear.is_none_or(|clear| clear <= after)) {
            state.record_skipped(true);
        }
        state
    }

    pub fn extend(&mut self, entries: &[EntryRecord]) {
        for entry in entries {
            if entry.head == Some(entry.id) {
                self.state = OnlineState::default();
                self.last_clear = Some(entry.id);
                continue;
            }
            match entry.kind.as_str() {
                COMPACTION_ENTRY_KIND => {
                    self.state.record_compaction(debt_of(entry));
                    self.last_clear = Some(entry.id);
                    continue;
                }
                DECAY_ENTRY_KIND => {
                    self.state.record_decay(debt_of(entry));
                    continue;
                }
                _ => {}
            }
            for message in entry.model.iter().flatten() {
                match message {
                    Message::Assistant(assistant)
                        if !assistant.stop_reason.excluded_from_context() =>
                    {
                        self.state.record_provider_request(prompt_tokens(assistant));
                    }
                    Message::ToolResult(result) if !result.is_error => {
                        self.state.record_tool_work();
                        self.last_clear = Some(entry.id);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 上一次请求写下的提示缓存是不是已经冷了：最后一条正常结束的 assistant 消息离 `now_ms`
/// 超过 `ttl_ms`。没请求过不算冷——还没有缓存可以失效。
pub fn cache_cold(entries: &[EntryRecord], now_ms: i64, ttl_ms: i64) -> bool {
    entries
        .iter()
        .rev()
        .flat_map(|entry| entry.model.iter().flatten().rev())
        .find_map(|message| match message {
            Message::Assistant(assistant) if !assistant.stop_reason.excluded_from_context() => {
                Some(assistant.timestamp)
            }
            _ => None,
        })
        .is_some_and(|at| now_ms - at > ttl_ms)
}

/// 摘要 entry 的 `data`：理由、决策与欠下的债（[`online_state`] 靠 `debt` 折账）。
pub fn compaction_data(reason: &str, decision: Option<&Value>, debt: &CompactionDebt) -> Value {
    let mut data = json!({ "reason": reason, "debt": debt });
    if let Some(decision) = decision {
        data["decision"] = decision.clone();
    }
    data
}

#[cfg(test)]
mod tests;
