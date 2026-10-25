//! 把一个 komo Session 的 conversation 与 entry 投影成 pi v3 会话树（D5 W1a，
//! `docs/storage-durable-d5.md` §4 / §7）：纯函数，供 `/tree`、导航、`/fork` `/clone`、D6 导出共用。
//!
//! 输入是 Session 的全部 conversation（各自的 entry 按 ID 升序）、当前分支与可选的导航位置
//! `leaf`；输出是 [`SessionEntry`] 列表（追加序，父在子前）与 `leaf_id`，交给
//! [`SessionTree::from_entries`] 再 [`SessionTree::branch`] 就是 pi 的树。
//!
//! # ID 与父子
//!
//! - pi entry 的 `id` = 十进制 entry ID（[`pi_id`]）。一条 entry 可能展开成几条 pi entry（见下），
//!   第一条用基础 ID，其余是 `"{基础ID}:{序号}"`；[`entry_of`] 把任一种还原成 entry ID。
//! - `parentId` = 同一个 conversation 上一条 entry 的**最后一条**展开；fork 出来的 conversation 的
//!   第一条指向 `parent.at` 的最后一条展开；没有父的 conversation 的第一条是新根（`null`）。
//! - 追加序 = 全局 entry ID 序。fork 点的 ID 永远小于子 conversation 的任何 entry，所以父总在子前。
//! - `leaf_id`：给了 `leaf` 就是它的最后一条展开（含这条 entry 自己的 edits，与
//!   `derive_context(.., Some(leaf))` 的 cutoff 含自己一致）；没给就是 `current_branch` 的末端
//!   （它自己最后一条 entry，空的 fork 退到 `parent.at`，空的新根是 `None`）。
//!
//! # entry 映射
//!
//! | durable entry | pi |
//! |---|---|
//! | `model` 里的消息（user / assistant / tool-result，含 `chat.input` 等自定义输入 kind） | 每条消息一条 `Message` |
//! | `komo.compaction`（`head` = 第一条保留的） | `Compaction`：`firstKeptEntryId` = head，`summary` = 去掉包装的摘要，`details` = 原 `data` |
//! | `head: self`（`komo.reset` 等，"自此重新开始"） | `Compaction`（`firstKeptEntryId` 指自己 = 什么都不留）+ 一条 omit 它自己的 `ContextEdit`（pi 的重置没有摘要消息），之后是它自带的交接消息 |
//! | `edits`（`komo.decay`、`context.edit`，或任何 entry 带的） | 每个 edit 一条 `ContextEdit`（Replace 的 `replacement = {content, messages}`） |
//! | `label {targetId, label}` | `Label` |
//! | `session.info {name}` | `SessionInfo` |
//! | `branch.summary {fromId}`（`model` 里是摘要 user 消息） | `BranchSummary`（`summary` = 那段文字原样，不去包装） |
//! | `model.change {provider, modelId}` | `ModelChange` |
//! | `komo.system`、`komo.settled` 与不认识的 kind | `Custom`（`customType` = kind，`data` = `{data?, model?, head?}`） |
//!
//! 一条 entry 展开的顺序：主体（压缩 / 消息 / label / …）、其余消息、edits。没有主体的（只带 edits 的
//! `komo.decay`）第一条 `ContextEdit` 当主体。
//!
//! # 与 `derive_context` 的对应
//!
//! `SessionTree::project().messages` 与 `derive_context(..).contributions` 展平后一致，除这几处
//! **请求时修补**（pi 的树本来也不做，见 `session` 模块文档）和格式差异：
//!
//! - system 消息：`komo.system` 是 `Custom`，不进 pi 的消息序列（komo 的 system 是段补丁，不是 pi 的消息）；
//! - 规则 7、8（工具结果排到调用后面、缺的补错误结果）与规则 10（基线 system 前移）；
//! - 规则 9（`aborted` / `error` / `deferred` 的 assistant 不进后续请求）：pi 的树里它们照样在；
//! - 压缩摘要是 `compactionSummary` 消息，对应 durable 的包好的摘要 user 消息
//!   （[`wrap_summary`](super::compaction::wrap_summary)）；
//! - `Replace` 换成多条消息、或换了 role 时，pi 只能换 `content`（`replacement.messages` 带着完整
//!   的消息，供还原用）；
//! - 一条 entry 的 `model` 有多条消息时，edits 只落在基础 ID 那一条上。
//!
//! 单测里的 `context_of` 按这几条做了归一化后逐条比较。

use std::collections::HashMap;

use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::compaction::{COMPACTION_ENTRY_KIND, SUMMARY_PREFIX, SUMMARY_SUFFIX};
use super::context::SYSTEM_ENTRY_KIND;
use super::ids::{ConversationId, EntryId};
use super::message::{Content, ContentBlock, Message};
use super::records::{ContextEdit, ConversationRecord, EditAction, EntryRecord};
use crate::session::{EntryKind, SessionEntry, SessionTree};

/// gateway 写的展示 / 辅助 entry 的 kind（`docs/storage-durable-d5.md` §2）。
pub const LABEL_KIND: &str = "label";
pub const SESSION_INFO_KIND: &str = "session.info";
pub const BRANCH_SUMMARY_KIND: &str = "branch.summary";
pub const MODEL_CHANGE_KIND: &str = "model.change";
pub const CONTEXT_EDIT_KIND: &str = "context.edit";

/// 一个 conversation 与它自己的全部 entry（按 ID 升序）。
#[derive(Debug, Clone, Copy)]
pub struct ConversationEntries<'a> {
    pub record: &'a ConversationRecord,
    pub entries: &'a [EntryRecord],
}

/// [`pi_tree`] 的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct PiTree {
    /// 追加序，父在子前。
    pub entries: Vec<SessionEntry>,
    pub leaf_id: Option<String>,
}

impl PiTree {
    /// 交给 pi 的树：按追加序还原，再把当前位置移到 `leaf_id`。
    pub fn into_session_tree(self) -> Result<SessionTree, String> {
        let mut tree = SessionTree::from_entries(self.entries)?;
        tree.branch(self.leaf_id.as_deref())?;
        Ok(tree)
    }
}

/// entry 的 pi ID（十进制）。
pub fn pi_id(entry: EntryId) -> String {
    entry.get().to_string()
}

/// pi ID 对应的 entry ID：基础 ID 与 `"{基础ID}:{序号}"` 两种写法都认；别的返回 `None`。
pub fn entry_of(pi_id: &str) -> Option<EntryId> {
    let base = pi_id.split_once(':').map_or(pi_id, |(base, _)| base);
    let value: u64 = base.parse().ok()?;
    (value.to_string() == base).then(|| EntryId::new(value))
}

/// 把 Session 投影成 pi 树。`current_branch` 只在没给 `leaf` 时用来定 `leaf_id`。
pub fn pi_tree(
    conversations: &[ConversationEntries<'_>],
    current_branch: ConversationId,
    leaf: Option<EntryId>,
) -> PiTree {
    let records: HashMap<ConversationId, &ConversationEntries<'_>> = conversations
        .iter()
        .map(|item| (item.record.id, item))
        .collect();
    let mut all: Vec<&EntryRecord> = conversations
        .iter()
        .flat_map(|item| item.entries.iter())
        .collect();
    all.sort_by_key(|entry| entry.id);

    let mut entries = Vec::with_capacity(all.len());
    let mut last_in_conversation: HashMap<ConversationId, String> = HashMap::new();
    let mut last_of_entry: HashMap<EntryId, String> = HashMap::new();
    let mut at_ms: i64 = 0;
    for entry in all {
        let parent = last_in_conversation
            .get(&entry.conversation)
            .cloned()
            .or_else(|| {
                let fork = records.get(&entry.conversation)?.record.parent?;
                last_of_entry.get(&fork.at).cloned()
            });
        // 没有自带时间的 entry（label、settled……）沿用前一条的。
        at_ms = entry_millis(entry).unwrap_or(at_ms);
        let timestamp = rfc3339(at_ms);
        let base = pi_id(entry.id);
        let mut parent = parent;
        for (index, kind) in pieces(entry, &base).into_iter().enumerate() {
            let id = if index == 0 {
                base.clone()
            } else {
                format!("{base}:{index}")
            };
            entries.push(SessionEntry {
                id: id.clone(),
                parent_id: parent.take(),
                timestamp: timestamp.clone(),
                kind,
            });
            parent = Some(id);
        }
        let last = parent.expect("pieces 至少有一条");
        last_in_conversation.insert(entry.conversation, last.clone());
        last_of_entry.insert(entry.id, last);
    }

    let leaf_id = match leaf {
        Some(leaf) => last_of_entry.get(&leaf).cloned(),
        None => records.get(&current_branch).and_then(|item| {
            match item.entries.iter().map(|entry| entry.id).max() {
                Some(last) => last_of_entry.get(&last).cloned(),
                None => item
                    .record
                    .parent
                    .and_then(|fork| last_of_entry.get(&fork.at).cloned()),
            }
        }),
    };
    PiTree { entries, leaf_id }
}

/// 一条 entry 展开成的 pi entry（第一条是主体，拿基础 ID）。
fn pieces(entry: &EntryRecord, base: &str) -> Vec<EntryKind> {
    let mut out = Vec::new();
    let model = entry.model.as_deref().unwrap_or(&[]);
    if entry.kind == COMPACTION_ENTRY_KIND || entry.head.is_some() {
        let is_compaction = entry.kind == COMPACTION_ENTRY_KIND;
        let first_kept = entry.head.unwrap_or(entry.id);
        out.push(EntryKind::Compaction {
            summary: if is_compaction {
                model
                    .iter()
                    .find_map(user_text)
                    .map(|text| unwrap_summary(&text))
                    .unwrap_or_default()
            } else {
                String::new()
            },
            first_kept_entry_id: pi_id(first_kept),
            tokens_before: entry
                .data
                .as_ref()
                .and_then(|data| data.pointer("/decision/context_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            system_message: None,
            details: entry.data.clone(),
            usage: None,
            from_hook: None,
        });
        if !is_compaction {
            // pi 的重置没有摘要消息：把压缩 entry 自己的那条贡献 omit 掉。
            out.push(EntryKind::ContextEdit {
                target_id: base.to_owned(),
                replacement: None,
            });
            out.extend(model.iter().map(message_entry));
        }
    } else {
        match entry.kind.as_str() {
            LABEL_KIND => out.push(label(entry)),
            SESSION_INFO_KIND => out.push(EntryKind::SessionInfo {
                name: data_str(entry, "name").unwrap_or_default().to_owned(),
            }),
            BRANCH_SUMMARY_KIND => out.push(EntryKind::BranchSummary {
                from_id: data_id(entry, "fromId").unwrap_or_default(),
                summary: model
                    .iter()
                    .find_map(user_text)
                    .or_else(|| data_str(entry, "summary").map(str::to_owned))
                    .unwrap_or_default(),
                details: None,
                usage: None,
                from_hook: None,
            }),
            MODEL_CHANGE_KIND => out.push(EntryKind::ModelChange {
                provider: data_str(entry, "provider").unwrap_or_default().to_owned(),
                model_id: data_str(entry, "modelId")
                    .or_else(|| data_str(entry, "model"))
                    .unwrap_or_default()
                    .to_owned(),
            }),
            SYSTEM_ENTRY_KIND => out.push(custom(entry)),
            _ if !model.is_empty() => out.extend(model.iter().map(message_entry)),
            _ if !entry.edits.is_empty() => {}
            _ => out.push(custom(entry)),
        }
    }
    out.extend(entry.edits.iter().map(edit_entry));
    if out.is_empty() {
        out.push(custom(entry));
    }
    out
}

fn message_entry(message: &Message) -> EntryKind {
    EntryKind::Message {
        message: serde_json::to_value(message).expect("Message 可序列化"),
    }
}

fn edit_entry(edit: &ContextEdit) -> EntryKind {
    let replacement = match &edit.action {
        EditAction::Omit => None,
        EditAction::Replace { messages } if messages.is_empty() => None,
        EditAction::Replace { messages } => {
            let messages = serde_json::to_value(messages).expect("Message 可序列化");
            Some(json!({ "content": messages[0]["content"].clone(), "messages": messages }))
        }
    };
    EntryKind::ContextEdit {
        target_id: pi_id(edit.target),
        replacement,
    }
}

fn label(entry: &EntryRecord) -> EntryKind {
    EntryKind::Label {
        target_id: data_id(entry, "targetId").unwrap_or_default(),
        label: data_str(entry, "label").map(str::to_owned),
    }
}

/// `Custom`：`data` 是 `{data?, model?, head?}`，什么都没有就是 `None`。
fn custom(entry: &EntryRecord) -> EntryKind {
    let mut envelope = Map::new();
    if let Some(data) = &entry.data {
        envelope.insert("data".into(), data.clone());
    }
    if let Some(model) = &entry.model {
        envelope.insert(
            "model".into(),
            serde_json::to_value(model).expect("Message 可序列化"),
        );
    }
    if let Some(head) = entry.head {
        envelope.insert("head".into(), json!(head));
    }
    EntryKind::Custom {
        custom_type: entry.kind.clone(),
        data: (!envelope.is_empty()).then_some(Value::Object(envelope)),
    }
}

fn data_str<'a>(entry: &'a EntryRecord, key: &str) -> Option<&'a str> {
    entry.data.as_ref()?.get(key)?.as_str()
}

/// `data[key]` 当 entry 引用读：数字是 entry ID，字符串原样（已经是 pi ID）。
fn data_id(entry: &EntryRecord, key: &str) -> Option<String> {
    match entry.data.as_ref()?.get(key)? {
        Value::Number(number) => number.as_u64().map(|id| pi_id(EntryId::new(id))),
        Value::String(text) => Some(text.clone()),
        _ => None,
    }
}

/// user 消息的文字（块里的 text 拼起来）。
pub(crate) fn user_text(message: &Message) -> Option<String> {
    let Message::User(user) = message else {
        return None;
    };
    Some(match &user.content {
        Content::Text(text) => text.clone(),
        Content::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

/// 去掉 [`wrap_summary`](super::compaction::wrap_summary) 的包装；不是这个形状就原样。
fn unwrap_summary(text: &str) -> String {
    text.strip_prefix(SUMMARY_PREFIX)
        .and_then(|rest| rest.strip_suffix(SUMMARY_SUFFIX))
        .unwrap_or(text)
        .to_owned()
}

/// entry 自带的时间：第一条消息的毫秒时间戳。
pub(crate) fn entry_millis(entry: &EntryRecord) -> Option<i64> {
    entry.model.as_ref()?.first().map(|message| match message {
        Message::User(message) => message.timestamp,
        Message::Assistant(message) => message.timestamp,
        Message::ToolResult(message) => message.timestamp,
        Message::System(message) => message.timestamp,
    })
}

fn rfc3339(millis: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .ok()
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests;
