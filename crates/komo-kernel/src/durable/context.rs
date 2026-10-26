//! 上下文推导（pi-durable spec §2.1 规则 1–10；设计 §2）：纯函数。
//!
//! 输入是一段可见 entry（已按 fork 祖先拼好、按 ID 升序）和可选的 cutoff（含），输出模型
//! 消息序列与用到的 entry。`ContextRange` 复用已推导的贡献，只重排最后一条 assistant
//! 之后尚未稳定的工具结果；遇到 edits 或 head 变化则重新推导。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::ids::EntryId;
use super::message::{AssistantMessage, ContentBlock, Message, ToolResultMessage};
use super::records::{ContextEdit, EditAction, EntryRecord};

/// 带 `model` 的系统段 entry 的 `kind`（设计 §4.1）。
pub const SYSTEM_ENTRY_KIND: &str = "komo.system";

/// 缺结果时补的错误结果的文本。
pub const MISSING_RESULT_TEXT: &str =
    "Tool result unavailable: history ends before this call completed.";

/// 一次上下文推导的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextView {
    /// 决定范围的 head 标记（规则 1）；没有则为 `None`。
    pub head: Option<EntryId>,
    /// 活动 entry：head 标记在前，其后是范围内不带 head 的 entry（规则 5）。
    pub entries: Vec<EntryId>,
    /// 与 `entries` 一一对应：每条 entry 经 edits 与停止原因过滤后贡献的消息，未排结果。
    pub contributions: Vec<Vec<Message>>,
    /// 发给模型的消息序列。
    pub messages: Vec<Message>,
}

impl ContextView {
    fn empty() -> Self {
        Self {
            head: None,
            entries: Vec::new(),
            contributions: Vec::new(),
            messages: Vec::new(),
        }
    }
}

/// 已推导的范围。调用方保证 `extend` 的 entry 严格接在原范围后、head 标记不变。
#[derive(Clone)]
pub struct ContextRange {
    pub view: Arc<ContextView>,
    edited: HashSet<EntryId>,
    settled: Vec<Tagged>,
    open: Vec<Tagged>,
    baseline: Baseline,
}

impl ContextRange {
    /// `visible` 按 ID 升序，包含最新 head 指向的范围。
    pub fn new(visible: &[EntryRecord]) -> Self {
        derive_range(visible, None)
    }

    /// 只推导新增贡献；edit 可能改变已有贡献，此时用已有记录整段重推，不需要重读存储。
    pub fn extend(&mut self, visible: &[EntryRecord], added: &[EntryRecord]) {
        if added.iter().any(|entry| {
            entry.head.is_some() || !entry.edits.is_empty() || self.edited.contains(&entry.id)
        }) {
            *self = Self::new(visible);
            return;
        }
        let view = Arc::make_mut(&mut self.view);
        let offset = view.entries.len();
        if self.baseline == Baseline::Unknown {
            let active: Vec<_> = added.iter().collect();
            self.baseline = match find_baseline_id(view.head, &active) {
                Baseline::Entry(index) => Baseline::Entry(offset + index),
                other => other,
            };
        }
        for (index, entry) in added.iter().enumerate() {
            let contribution = contribute(entry, None);
            self.open.extend(tag_contribution(
                &contribution,
                self.baseline == Baseline::Entry(offset + index),
            ));
            view.entries.push(entry.id);
            view.contributions.push(contribution);
        }
        settle(&mut self.settled, &mut self.open);
        view.messages = range_messages(&self.settled, &self.open);
    }
}

/// 推导 `visible`（按 ID 升序）在 `cutoff`（含）处的上下文。
pub fn derive_context(visible: &[EntryRecord], cutoff: Option<EntryId>) -> ContextView {
    Arc::unwrap_or_clone(derive_range(visible, cutoff).view)
}

fn derive_range(visible: &[EntryRecord], cutoff: Option<EntryId>) -> ContextRange {
    let upto: Vec<&EntryRecord> = visible
        .iter()
        .filter(|entry| cutoff.is_none_or(|cutoff| entry.id <= cutoff))
        .collect();

    // 规则 1、2：最新的 head 标记 H，范围从 H.head 起、到 cutoff 止。
    let head = upto
        .iter()
        .rev()
        .find(|entry| entry.head.is_some())
        .copied();
    let range: Vec<&EntryRecord> = match head.and_then(|head| head.head) {
        Some(from) => upto
            .iter()
            .filter(|entry| entry.id >= from)
            .copied()
            .collect(),
        None => upto,
    };
    if range.is_empty() && head.is_none() {
        return ContextRange {
            view: Arc::new(ContextView::empty()),
            edited: HashSet::new(),
            settled: Vec::new(),
            open: Vec::new(),
            baseline: Baseline::Unknown,
        };
    }

    // 规则 4：范围内每个目标取最新的一条 edit。老的 head 标记的 edits 也算。
    let mut edits: HashMap<EntryId, &ContextEdit> = HashMap::new();
    for entry in &range {
        for edit in &entry.edits {
            edits.insert(edit.target, edit);
        }
    }

    // 规则 5：有 H 时是 H 加上范围内不带 head 的 entry；否则就是范围。
    let active: Vec<&EntryRecord> = match head {
        Some(head) => std::iter::once(head)
            .chain(range.iter().copied().filter(|entry| entry.head.is_none()))
            .collect(),
        None => range,
    };
    let contributions: Vec<Vec<Message>> = active
        .iter()
        .map(|entry| contribute(entry, edits.get(&entry.id).copied()))
        .collect();

    // 规则 6：system 消息按位置保留，不丢也不合并，直接随各 entry 的贡献进入序列。
    let baseline = find_baseline(head, &active);
    let mut open = Vec::new();
    for (index, entry) in active.iter().enumerate() {
        open.extend(tag_contribution(
            &contributions[index],
            !edits.contains_key(&entry.id) && baseline == Baseline::Entry(index),
        ));
    }
    let mut settled = Vec::new();
    settle(&mut settled, &mut open);
    let messages = range_messages(&settled, &open);

    ContextRange {
        view: Arc::new(ContextView {
            head: head.map(|head| head.id),
            entries: active.iter().map(|entry| entry.id).collect(),
            contributions,
            messages,
        }),
        edited: edits.keys().copied().collect(),
        settled,
        open,
        baseline,
    }
}

fn tag_contribution(messages: &[Message], baseline: bool) -> Vec<Tagged> {
    let mut taken = false;
    messages
        .iter()
        .map(|message| {
            let is_baseline = baseline && message.is_system() && !taken;
            taken |= message.is_system();
            Tagged {
                message: message.clone(),
                baseline: is_baseline,
            }
        })
        .collect()
}

fn settle(settled: &mut Vec<Tagged>, open: &mut Vec<Tagged>) {
    if let Some(last) = open
        .iter()
        .rposition(|tagged| matches!(tagged.message, Message::Assistant(_)))
        && last > 0
    {
        let tail = open.split_off(last);
        settled.extend(order_tagged(std::mem::replace(open, tail)));
    }
}

fn range_messages(settled: &[Tagged], open: &[Tagged]) -> Vec<Message> {
    let mut messages = settled.to_vec();
    messages.extend(order_tagged(open.to_vec()));
    lead_with_system(messages)
}

/// 一条 entry 在 edit 与停止原因过滤之后的消息（规则 4、9）。
fn contribute(entry: &EntryRecord, edit: Option<&ContextEdit>) -> Vec<Message> {
    let messages: &[Message] = match edit.map(|edit| &edit.action) {
        Some(EditAction::Omit) => return Vec::new(),
        Some(EditAction::Replace { messages }) => messages,
        None => entry.model.as_deref().unwrap_or(&[]),
    };
    messages
        .iter()
        .filter(|message| match message {
            Message::Assistant(assistant) => !assistant.stop_reason.excluded_from_context(),
            _ => true,
        })
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Baseline {
    /// 范围里还没出现能定下基线的 entry。
    Unknown,
    /// 这个范围不可能有基线，没有 system 消息能靠"是基线"前移。
    None,
    /// 活动 entry 里第 N 条的 system 消息。
    Entry(usize),
}

/// 基线：H 之后第一条 `komo.system` entry 的 system 消息；没有 H 时，除非更早的 entry 已经
/// 带过 system 消息（哪怕之后被 edit omit 了），否则也是第一条 `komo.system` entry。
///
/// 压缩或重置之后，生成阶段会把当时生效的所有段与工具写进这条基线，并省掉保留下来的
/// `komo.system` entry，所以它描述的是整个提示，值得放到最前面。
fn find_baseline(head: Option<&EntryRecord>, active: &[&EntryRecord]) -> Baseline {
    find_baseline_id(head.map(|head| head.id), active)
}

fn find_baseline_id(head: Option<EntryId>, active: &[&EntryRecord]) -> Baseline {
    for (index, entry) in active.iter().enumerate() {
        if head.is_some_and(|head| entry.id <= head) {
            continue;
        }
        if entry.kind == SYSTEM_ENTRY_KIND {
            let has_system = entry
                .model
                .as_ref()
                .is_some_and(|model| model.iter().any(Message::is_system));
            return if has_system {
                Baseline::Entry(index)
            } else {
                Baseline::None
            };
        }
        if head.is_none() && carries_system_message(entry) {
            return Baseline::None;
        }
    }
    Baseline::Unknown
}

/// entry 自己的消息或它的 edit 的替换消息里有没有 system 消息。
fn carries_system_message(entry: &EntryRecord) -> bool {
    entry
        .model
        .as_ref()
        .is_some_and(|model| model.iter().any(Message::is_system))
        || entry.edits.iter().any(|edit| match &edit.action {
            EditAction::Replace { messages } => messages.iter().any(Message::is_system),
            EditAction::Omit => false,
        })
}

/// 规则 10：只有 user 消息在它前面、或它就是基线时，第一条 system 消息前移到最前。
///
/// 提供方只把开头的 system 消息当初始提示与工具集；没有它，之后一次工具变更会重写整个
/// 请求的工具列表，让整段提示缓存失效。只动第一条：别的 system 消息前面若已有 assistant
/// 消息就留在原位，挪了会改变它对模型的意思。
fn lead_with_system(mut messages: Vec<Tagged>) -> Vec<Message> {
    if let Some(index) = messages
        .iter()
        .position(|tagged| tagged.message.is_system())
        && index > 0
        && (messages[index].baseline
            || messages[..index]
                .iter()
                .all(|tagged| tagged.message.is_user()))
    {
        let system = messages.remove(index);
        messages.insert(0, system);
    }
    messages.into_iter().map(|tagged| tagged.message).collect()
}

/// 带"是不是基线"标记走完工具结果排序的消息。
#[derive(Clone)]
struct Tagged {
    message: Message,
    baseline: bool,
}

/// 规则 7、8：把每条 assistant 的工具结果挪到它后面，按调用顺序；缺的补错误结果，没有
/// 前置调用的丢掉。
///
/// 结果只在"下一条 assistant 之前"找，跨过中间的 user / system 消息。同一 `toolCallId`
/// 出现多次取第一条。
pub fn order_tool_results(messages: &[Message]) -> Vec<Message> {
    let tagged = messages
        .iter()
        .map(|message| Tagged {
            message: message.clone(),
            baseline: false,
        })
        .collect();
    order_tagged(tagged)
        .into_iter()
        .map(|tagged| tagged.message)
        .collect()
}

fn order_tagged(messages: Vec<Tagged>) -> Vec<Tagged> {
    let mut ordered = Vec::with_capacity(messages.len());
    for (index, tagged) in messages.iter().enumerate() {
        match &tagged.message {
            Message::ToolResult(_) => continue,
            Message::Assistant(assistant) => {
                ordered.push(Tagged {
                    message: tagged.message.clone(),
                    baseline: tagged.baseline,
                });
                push_results(&mut ordered, assistant, &messages[index + 1..]);
            }
            _ => ordered.push(Tagged {
                message: tagged.message.clone(),
                baseline: tagged.baseline,
            }),
        }
    }
    ordered
}

fn push_results(ordered: &mut Vec<Tagged>, assistant: &AssistantMessage, following: &[Tagged]) {
    let mut results: HashMap<&str, &ToolResultMessage> = HashMap::new();
    for tagged in following {
        match &tagged.message {
            Message::Assistant(_) => break,
            Message::ToolResult(result) => {
                results
                    .entry(result.tool_call_id.as_str())
                    .or_insert(result);
            }
            _ => {}
        }
    }
    for (id, name) in assistant.tool_calls() {
        let message = match results.get(id) {
            Some(result) => Message::ToolResult((*result).clone()),
            None => missing_result(id, name, assistant.timestamp),
        };
        ordered.push(Tagged {
            message,
            baseline: false,
        });
    }
}

fn missing_result(id: &str, name: &str, timestamp: i64) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.to_owned(),
        tool_name: name.to_owned(),
        content: vec![ContentBlock::text(MISSING_RESULT_TEXT)],
        is_error: true,
        details: Some(serde_json::json!({"reason": "missing_result"})),
        timestamp,
        extra: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::ids::ConversationId;
    use super::super::message::{Sections, StopReason};
    use super::*;

    const CONV: ConversationId = ConversationId::new(1);

    fn id(n: u64) -> EntryId {
        EntryId::new(n)
    }

    fn entry(n: u64, kind: &str, model: Vec<Message>) -> EntryRecord {
        EntryRecord::new(id(n), CONV, kind).with_model(model)
    }

    fn user(text: &str) -> Message {
        Message::user(text, 1)
    }

    fn call(call_id: &str) -> ContentBlock {
        ContentBlock::tool_call(call_id, "read", json!({}))
    }

    fn assistant_calling(ids: &[&str]) -> Message {
        Message::assistant(
            ids.iter().map(|id| call(id)).collect(),
            StopReason::ToolUse,
            7,
        )
    }

    fn assistant_text(text: &str, stop: StopReason) -> Message {
        Message::assistant(vec![ContentBlock::text(text)], stop, 1)
    }

    fn result(call_id: &str) -> Message {
        Message::tool_result(call_id, "read", format!("out-{call_id}"), 2)
    }

    fn system(section: &str) -> Message {
        Message::system(Sections::default().set(section, "x"), 3)
    }

    /// 消息的一行简写，断言用：`u:` `a:` `r:` `s:`，错误结果加 `!`。
    fn shape(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .map(|message| match message {
                Message::User(user) => match &user.content {
                    super::super::message::Content::Text(text) => format!("u:{text}"),
                    _ => "u:".into(),
                },
                Message::Assistant(assistant) => {
                    let calls: Vec<_> = assistant.tool_calls().map(|(id, _)| id).collect();
                    match assistant.content.first() {
                        Some(ContentBlock::Text { text, .. }) => format!("a:{text}"),
                        _ => format!("a:{}", calls.join(",")),
                    }
                }
                Message::ToolResult(result) => {
                    format!(
                        "r:{}{}",
                        result.tool_call_id,
                        if result.is_error { "!" } else { "" }
                    )
                }
                Message::System(system) => {
                    let names: Vec<_> = system
                        .sections
                        .iter()
                        .flat_map(|sections| sections.0.iter().map(|(name, _)| name.as_str()))
                        .collect();
                    format!("s:{}", names.join(","))
                }
            })
            .collect()
    }

    fn derive(entries: &[EntryRecord]) -> Vec<String> {
        shape(&derive_context(entries, None).messages)
    }

    #[test]
    fn rule1_uses_the_newest_head_marker_and_starts_from_its_head() {
        let entries = vec![
            entry(1, "m", vec![user("old")]),
            entry(2, "m", vec![user("keep")]),
            entry(3, "reset", vec![user("marker")]).with_head(id(2)),
            entry(4, "m", vec![user("new")]),
        ];
        let view = derive_context(&entries, None);
        assert_eq!(view.head, Some(id(3)));
        assert_eq!(shape(&view.messages), ["u:marker", "u:keep", "u:new"]);
        // 更新的 head 标记盖过旧的；旧标记自己不再进上下文，但它的 edits 仍然生效（规则 4）。
        let entries = [
            entries,
            vec![entry(5, "reset", vec![user("marker2")]).with_head(id(5))],
        ]
        .concat();
        assert_eq!(derive(&entries), ["u:marker2"]);
    }

    #[test]
    fn rule2_and_3_cutoff_picks_the_marker_and_scan_range() {
        let entries = vec![
            entry(1, "m", vec![user("a")]),
            entry(2, "m", vec![user("b")]).with_head(id(2)),
            entry(3, "m", vec![user("c")]),
            entry(4, "m", vec![user("d")]).with_head(id(4)),
        ];
        assert_eq!(
            shape(&derive_context(&entries, Some(id(3))).messages),
            ["u:b", "u:c"]
        );
        assert_eq!(
            shape(&derive_context(&entries, Some(id(1))).messages),
            ["u:a"]
        );
        assert_eq!(shape(&derive_context(&entries, None).messages), ["u:d"]);
        assert!(derive_context(&[], None).messages.is_empty());
    }

    #[test]
    fn rule4_newest_edit_per_target_wins_and_omit_or_replace_apply() {
        let entries = vec![
            entry(1, "m", vec![user("one")]),
            entry(2, "m", vec![user("two")]),
            entry(3, "m", vec![user("three")]),
            EntryRecord::new(id(4), CONV, "edit").with_edits(vec![
                ContextEdit::omit(id(1)),
                ContextEdit::replace(id(2), vec![user("two-v1")]),
            ]),
            EntryRecord::new(id(5), CONV, "edit").with_edits(vec![
                ContextEdit::replace(id(1), vec![user("one-back")]),
                ContextEdit::replace(id(2), vec![user("two-v2")]),
                ContextEdit::omit(id(3)),
            ]),
        ];
        assert_eq!(derive(&entries), ["u:one-back", "u:two-v2"]);
        // 同一条 entry 里同一目标写两次，后一条赢。
        let entries = vec![
            entry(1, "m", vec![user("one")]),
            EntryRecord::new(id(2), CONV, "edit").with_edits(vec![
                ContextEdit::omit(id(1)),
                ContextEdit::replace(id(1), vec![user("r")]),
            ]),
        ];
        assert_eq!(derive(&entries), ["u:r"]);
    }

    #[test]
    fn rule5_head_marker_leads_and_other_head_entries_in_range_are_dropped() {
        let entries = vec![
            entry(1, "m", vec![user("a")]),
            entry(2, "mid", vec![user("older-marker")]).with_head(id(1)),
            entry(3, "m", vec![user("b")]),
            entry(4, "marker", vec![user("newest-marker")]).with_head(id(1)),
            entry(5, "m", vec![user("c")]),
        ];
        let view = derive_context(&entries, None);
        assert_eq!(view.entries, [id(4), id(1), id(3), id(5)]);
        assert_eq!(
            shape(&view.messages),
            ["u:newest-marker", "u:a", "u:b", "u:c"]
        );
    }

    #[test]
    fn rule6_positional_system_messages_stay_where_they_were_committed() {
        let entries = vec![
            entry(1, "m", vec![user("hi")]),
            entry(2, "m", vec![assistant_text("hello", StopReason::Stop)]),
            entry(3, "other", vec![system("late")]),
            entry(4, "m", vec![user("again")]),
        ];
        // 前面有 assistant，不前移；顺序与提交顺序一致。
        assert_eq!(derive(&entries), ["u:hi", "a:hello", "s:late", "u:again"]);
    }

    #[test]
    fn rule7_results_move_after_their_call_in_call_order_past_interleaved_messages() {
        let entries = vec![
            entry(1, "m", vec![assistant_calling(&["c1", "c2"])]),
            entry(2, "m", vec![user("steer")]),
            entry(3, "m", vec![result("c2")]),
            entry(4, "other", vec![system("mid")]),
            entry(5, "m", vec![result("c1")]),
        ];
        assert_eq!(
            derive(&entries),
            ["a:c1,c2", "r:c1", "r:c2", "u:steer", "s:mid"]
        );
    }

    #[test]
    fn rule7_results_after_the_next_assistant_belong_to_no_earlier_call() {
        let entries = vec![
            entry(1, "m", vec![assistant_calling(&["c1"])]),
            entry(2, "m", vec![assistant_text("next", StopReason::Stop)]),
            entry(3, "m", vec![result("c1")]),
        ];
        // c1 的结果出现在下一条 assistant 之后：补错误结果，真结果因无前置调用被丢。
        assert_eq!(derive(&entries), ["a:c1", "r:c1!", "a:next"]);
    }

    #[test]
    fn rule8_missing_results_are_synthesized_and_orphans_dropped() {
        let entries = vec![
            entry(1, "m", vec![result("ghost")]),
            entry(2, "m", vec![assistant_calling(&["c1", "c2"])]),
            entry(3, "m", vec![result("c1")]),
        ];
        let messages = derive_context(&entries, None).messages;
        assert_eq!(shape(&messages), ["a:c1,c2", "r:c1", "r:c2!"]);
        let Message::ToolResult(missing) = &messages[2] else {
            panic!("tool result expected");
        };
        assert_eq!(missing.tool_name, "read");
        assert_eq!(missing.timestamp, 7, "补的结果沿用 assistant 的时间戳");
        assert_eq!(missing.details, Some(json!({"reason": "missing_result"})));
        assert_eq!(
            missing.content,
            vec![ContentBlock::text(MISSING_RESULT_TEXT)]
        );
        // fork 截断在调用之后、结果之前时同样补。
        assert_eq!(
            shape(&derive_context(&entries, Some(id(2))).messages),
            ["a:c1,c2", "r:c1!", "r:c2!"]
        );
    }

    #[test]
    fn rule9_excluded_stop_reasons_and_model_less_entries_contribute_nothing() {
        let entries = vec![
            entry(1, "m", vec![user("q")]),
            entry(2, "m", vec![assistant_text("partial", StopReason::Aborted)]),
            entry(3, "m", vec![assistant_text("boom", StopReason::Error)]),
            entry(4, "m", vec![assistant_text("later", StopReason::Deferred)]),
            EntryRecord::new(id(5), CONV, "note").with_data(json!({"label": "x"})),
            entry(6, "m", vec![assistant_text("fine", StopReason::Stop)]),
        ];
        let view = derive_context(&entries, None);
        assert_eq!(shape(&view.messages), ["u:q", "a:fine"]);
        // 被排除的 entry 仍是活动 entry，只是贡献为空。
        assert_eq!(view.entries.len(), 6);
        assert_eq!(view.contributions[1], Vec::<Message>::new());
        // replace 进来的消息同样按停止原因过滤。
        let entries = vec![
            entry(1, "m", vec![user("q")]),
            EntryRecord::new(id(2), CONV, "edit").with_edits(vec![ContextEdit::replace(
                id(1),
                vec![user("q2"), assistant_text("x", StopReason::Error)],
            )]),
        ];
        assert_eq!(derive(&entries), ["u:q2"]);
    }

    #[test]
    fn rule10_first_system_moves_to_front_when_only_user_messages_precede_it() {
        let entries = vec![
            entry(1, "m", vec![user("input")]),
            entry(2, SYSTEM_ENTRY_KIND, vec![system("base")]),
            entry(3, "m", vec![assistant_text("answer", StopReason::Stop)]),
            entry(4, SYSTEM_ENTRY_KIND, vec![system("change")]),
        ];
        // 只前移第一条；后一条前面有 assistant，留在原位。
        assert_eq!(
            derive(&entries),
            ["s:base", "u:input", "a:answer", "s:change"]
        );
    }

    #[test]
    fn rule10_the_baseline_leads_even_behind_an_assistant_message() {
        // 没有 H：第一条 komo.system entry 就是基线，前面有 assistant 也前移。
        let entries = vec![
            entry(1, "m", vec![assistant_text("hi", StopReason::Stop)]),
            entry(2, SYSTEM_ENTRY_KIND, vec![system("base")]),
        ];
        assert_eq!(derive(&entries), ["s:base", "a:hi"]);
        // 有 H：H 之后第一条 komo.system 是基线，保留下来的 assistant 在它后面。
        let entries = vec![
            entry(1, "m", vec![user("kept-q")]),
            entry(2, "m", vec![assistant_text("kept-a", StopReason::Stop)]),
            entry(3, "compaction", vec![user("summary")]).with_head(id(1)),
            entry(4, SYSTEM_ENTRY_KIND, vec![system("baseline")]),
        ];
        assert_eq!(
            derive(&entries),
            ["s:baseline", "u:summary", "u:kept-q", "a:kept-a"]
        );
    }

    #[test]
    fn rule10_retained_system_entries_are_not_the_baseline() {
        // H 本身与 H 之前保留下来的 system entry 不是基线：前面有 assistant 就留在原位。
        let entries = vec![
            entry(1, "m", vec![user("kept-q")]),
            entry(2, "m", vec![assistant_text("kept-a", StopReason::Stop)]),
            entry(3, SYSTEM_ENTRY_KIND, vec![system("retained")]),
            entry(4, "compaction", vec![user("summary")]).with_head(id(1)),
        ];
        assert_eq!(
            derive(&entries),
            ["u:summary", "u:kept-q", "a:kept-a", "s:retained"]
        );
    }

    #[test]
    fn rule10_an_edit_on_the_baseline_keeps_it_from_leading() {
        let entries = vec![
            entry(1, "m", vec![user("kept-q")]),
            entry(2, "m", vec![assistant_text("kept-a", StopReason::Stop)]),
            entry(3, "compaction", vec![user("summary")]).with_head(id(1)),
            entry(4, SYSTEM_ENTRY_KIND, vec![system("baseline")]),
            EntryRecord::new(id(5), CONV, "edit")
                .with_edits(vec![ContextEdit::replace(id(4), vec![system("replaced")])]),
        ];
        // 替换后的 system 消息不再是基线；前面有 assistant，留在原位。
        assert_eq!(
            derive(&entries),
            ["u:summary", "u:kept-q", "a:kept-a", "s:replaced"]
        );
    }

    #[test]
    fn rule10_without_a_marker_an_earlier_omitted_system_message_still_blocks_the_baseline() {
        let entries = vec![
            entry(1, "other", vec![system("early")]),
            EntryRecord::new(id(2), CONV, "edit").with_edits(vec![ContextEdit::omit(id(1))]),
            entry(3, "m", vec![assistant_text("a", StopReason::Stop)]),
            entry(4, SYSTEM_ENTRY_KIND, vec![system("later")]),
        ];
        assert_eq!(derive(&entries), ["a:a", "s:later"]);
    }

    #[test]
    fn incremental_contributions_handle_future_edits_and_multiple_messages_per_entry() {
        let entries = vec![
            EntryRecord::new(id(1), CONV, "edit").with_edits(vec![ContextEdit::replace(
                id(4),
                vec![system("replacement"), user("future")],
            )]),
            entry(
                2,
                "m",
                vec![assistant_calling(&["c1", "c2"]), user("steer")],
            ),
            entry(3, "m", vec![result("c2"), result("c2"), result("ghost")]),
            entry(4, "m", vec![user("original")]),
            entry(5, SYSTEM_ENTRY_KIND, vec![system("base"), system("second")]),
            entry(
                6,
                "m",
                vec![result("c1"), assistant_calling(&["c3"]), result("c3")],
            ),
            EntryRecord::new(id(7), CONV, "edit").with_edits(vec![ContextEdit::omit(id(2))]),
            entry(8, "m", vec![assistant_text("done", StopReason::Stop)]),
        ];
        for split in 0..=entries.len() {
            let mut range = ContextRange::new(&entries[..split]);
            for end in split + 1..=entries.len() {
                range.extend(&entries[..end], &entries[end - 1..end]);
                assert_eq!(
                    *range.view,
                    derive_context(&entries[..end], None),
                    "split={split}, end={end}"
                );
            }
        }
    }

    #[test]
    fn order_tool_results_is_usable_on_plain_message_lists() {
        let messages = vec![assistant_calling(&["c1"]), user("x"), result("c1")];
        assert_eq!(
            shape(&order_tool_results(&messages)),
            ["a:c1", "r:c1", "u:x"]
        );
    }
}
