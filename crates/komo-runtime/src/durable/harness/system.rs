//! 位置化的系统段规划（pi-durable spec §7.4 的 `planSystemEntries`；设计 §4.5 的 `prepare`）。
//!
//! 持久的只有 transcript 里的 `komo.system` entry：一条 system 消息是**补丁**（段按名字
//! 加 / 换 / 用 `null` 删，工具先删后加），从不是重置。每次请求之前把钩子渲染出来的期望段
//! 与工具声明和回放出来的**已展示状态**比较，不一样才追加 entry——所以提示文字没变就不写，
//! 前缀字节不动，提示缓存照样命中。
//!
//! - 当前范围有 head 标记、其后没有 `komo.system` entry：写一条**完整基线**，并 omit 掉范围里
//!   保留下来的更早的 `komo.system` entry（head 切掉了上一份基线，补丁无从叠起）。即使它只是
//!   重述已展示的值也写，这样之后每次 `prepare` 都能在标记之后找到一条。
//! - 否则，最小补丁会让段的次序与期望不同：先把已展示的每段删掉（`null`），再按期望次序
//!   全部加回——两条 entry。
//! - 否则只写变了的值与 `null` 删除，或什么都不写。
//!
//! 工具变更跟在最后一条规划的 entry 上，没有段变更时自成一条。声明按 `(name, description,
//! parameters)` 比较；变了的声明先删后加，回放保留原位置、新增追加——这样得不到期望次序时
//! 整张表删掉重加。

use std::collections::HashMap;

use komo_kernel::durable::{
    ContentBlock, ContextEdit, ContextView, EntryId, EntryRecord, Message, Sections, SystemMessage,
};
use komo_kernel::types::tool::ToolDefinition;
use serde_json::{Value, json};

use super::entries::SYSTEM;
use super::hooks::PromptSection;
use crate::durable::tx::EntryDraft;
use crate::llm::replay_system;

/// 回放 `messages` 里的 system 消息之后生效的段：按名字原位替换，`null` 删除，重加追加。
pub fn replay_sections(messages: &[Message]) -> Vec<(String, String)> {
    let mut shown: Vec<(String, String)> = Vec::new();
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        let Some(patch) = &system.sections else {
            continue;
        };
        for (name, value) in &patch.0 {
            match value {
                Some(text) => match shown.iter_mut().find(|(have, _)| have == name) {
                    Some(slot) => slot.1 = text.clone(),
                    None => shown.push((name.clone(), text.clone())),
                },
                None => shown.retain(|(have, _)| have != name),
            }
        }
    }
    shown
}

fn declaration(tool: &ToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    })
}

fn same_declaration(a: &ToolDefinition, b: &ToolDefinition) -> bool {
    a.name == b.name && a.description == b.description && a.parameters == b.parameters
}

struct ToolChanges {
    removed: Vec<Value>,
    added: Vec<Value>,
}

fn plan_tools(offered: &[ToolDefinition], desired: &[ToolDefinition]) -> ToolChanges {
    let kept: Vec<&ToolDefinition> = offered
        .iter()
        .filter(|tool| {
            desired
                .iter()
                .any(|next| next.name == tool.name && same_declaration(tool, next))
        })
        .collect();
    let added: Vec<&ToolDefinition> = desired
        .iter()
        .filter(|tool| !kept.iter().any(|have| have.name == tool.name))
        .collect();
    let replayed: Vec<&str> = kept
        .iter()
        .map(|tool| tool.name.as_str())
        .chain(added.iter().map(|tool| tool.name.as_str()))
        .collect();
    let in_order = replayed
        .iter()
        .zip(desired.iter())
        .all(|(have, want)| *have == want.name);
    if !in_order {
        return ToolChanges {
            removed: offered
                .iter()
                .map(|tool| json!({ "name": tool.name }))
                .collect(),
            added: desired.iter().map(declaration).collect(),
        };
    }
    ToolChanges {
        removed: offered
            .iter()
            .filter(|tool| !kept.iter().any(|have| have.name == tool.name))
            .map(|tool| json!({ "name": tool.name }))
            .collect(),
        added: added.iter().map(|tool| declaration(tool)).collect(),
    }
}

type Patch = Vec<(String, Option<String>)>;

/// 段补丁：无、最小补丁，或次序会不同时的"全删 / 全加"一对。
fn plan_sections(shown: &[(String, String)], desired: &[(String, String)]) -> Vec<Patch> {
    let patched_order: Vec<&str> = shown
        .iter()
        .filter(|(key, _)| desired.iter().any(|(want, _)| want == key))
        .map(|(key, _)| key.as_str())
        .chain(
            desired
                .iter()
                .filter(|(key, _)| !shown.iter().any(|(have, _)| have == key))
                .map(|(key, _)| key.as_str()),
        )
        .collect();
    let in_order = patched_order
        .iter()
        .zip(desired.iter())
        .all(|(have, (want, _))| *have == want);
    if !in_order {
        return vec![
            shown.iter().map(|(key, _)| (key.clone(), None)).collect(),
            desired
                .iter()
                .map(|(key, text)| (key.clone(), Some(text.clone())))
                .collect(),
        ];
    }
    let mut patch: Patch = Vec::new();
    for (key, value) in shown {
        match desired.iter().find(|(want, _)| want == key) {
            Some((_, next)) if next == value => {}
            Some((_, next)) => patch.push((key.clone(), Some(next.clone()))),
            None => patch.push((key.clone(), None)),
        }
    }
    for (key, value) in desired {
        if !shown.iter().any(|(have, _)| have == key) {
            patch.push((key.clone(), Some(value.clone())));
        }
    }
    if patch.is_empty() {
        Vec::new()
    } else {
        vec![patch]
    }
}

fn system_entry(
    sections: Option<Patch>,
    tools: Option<&ToolChanges>,
    timestamp: i64,
) -> EntryDraft {
    let message = SystemMessage {
        content: komo_kernel::durable::Content::Text(String::new()),
        sections: sections.map(Sections),
        tools_removed: tools
            .filter(|changes| !changes.removed.is_empty())
            .map(|changes| changes.removed.clone()),
        tools_added: tools
            .filter(|changes| !changes.added.is_empty())
            .map(|changes| changes.added.clone()),
        timestamp,
        extra: Default::default(),
    };
    EntryDraft::new(SYSTEM).with_model(vec![Message::System(message)])
}

/// 规划让 `view` 回放出的段与工具变成 `desired` 与 `tools` 的 `komo.system` entry。
/// `entries` 是 `view` 推导所用的可见 entry（要看 kind）。
pub fn plan_system_entries(
    entries: &[EntryRecord],
    view: &ContextView,
    desired: &[PromptSection],
    tools: &[ToolDefinition],
    timestamp: i64,
) -> Vec<EntryDraft> {
    let desired: Vec<(String, String)> = desired
        .iter()
        .map(|section| (section.key.clone(), section.text.clone()))
        .collect();
    let kinds: HashMap<EntryId, &str> = entries
        .iter()
        .map(|entry| (entry.id, entry.kind.as_str()))
        .collect();
    let is_system = |id: &EntryId| kinds.get(id) == Some(&SYSTEM);

    if let Some(head) = view.head
        && !view.entries.iter().any(|id| is_system(id) && *id > head)
    {
        let edits: Vec<ContextEdit> = view
            .entries
            .iter()
            .filter(|id| is_system(id))
            .map(|id| ContextEdit::omit(*id))
            .collect();
        let baseline = ToolChanges {
            removed: Vec::new(),
            added: tools.iter().map(declaration).collect(),
        };
        let mut entry = system_entry(
            Some(
                desired
                    .iter()
                    .map(|(key, text)| (key.clone(), Some(text.clone())))
                    .collect(),
            ),
            Some(&baseline),
            timestamp,
        );
        entry.edits = edits;
        return vec![entry];
    }

    let shown = replay_sections(&view.messages);
    let patches = plan_sections(&shown, &desired);
    let offered = replay_system(&view.messages).tools;
    let changes = plan_tools(&offered, tools);
    if changes.removed.is_empty() && changes.added.is_empty() {
        return patches
            .into_iter()
            .map(|patch| system_entry(Some(patch), None, timestamp))
            .collect();
    }
    if patches.is_empty() {
        return vec![system_entry(None, Some(&changes), timestamp)];
    }
    let last = patches.len() - 1;
    patches
        .into_iter()
        .enumerate()
        .map(|(index, patch)| {
            system_entry(Some(patch), (index == last).then_some(&changes), timestamp)
        })
        .collect()
}

/// 一条 assistant 消息里的工具调用，按调用顺序：`(id, name, arguments)`。
pub fn tool_calls_of(message: &komo_kernel::durable::AssistantMessage) -> Vec<(String, String)> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use komo_kernel::durable::{ConversationId, derive_context};

    use super::*;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: format!("{name} tool"),
            parameters: json!({"type": "object"}),
            doc: None,
        }
    }

    fn section(key: &str, text: &str) -> PromptSection {
        PromptSection::new(key, text)
    }

    fn entry(id: u64, kind: &str, messages: Vec<Message>) -> EntryRecord {
        EntryRecord::new(EntryId::new(id), ConversationId::new(1), kind).with_model(messages)
    }

    fn apply(entries: &mut Vec<EntryRecord>, drafts: Vec<EntryDraft>) {
        for draft in drafts {
            let id = entries.last().map_or(1, |entry| entry.id.get() + 1);
            let mut record = EntryRecord::new(EntryId::new(id), ConversationId::new(1), draft.kind);
            record.model = draft.model;
            record.edits = draft.edits;
            entries.push(record);
        }
    }

    // 对照 pi 的 `planSystemEntries`：第一次写一条完整补丁，之后值与次序都没变就什么都不写。
    #[test]
    fn a_stable_prompt_is_written_once_and_never_again() {
        let mut entries = vec![entry(1, "komo.user", vec![Message::user("hi", 1)])];
        let desired = [section("preamble", "be kind"), section("cwd", "/work")];
        let tools = [tool("read")];
        let first = plan_system_entries(
            &entries,
            &derive_context(&entries, None),
            &desired,
            &tools,
            5,
        );
        assert_eq!(first.len(), 1);
        apply(&mut entries, first);
        let again = plan_system_entries(
            &entries,
            &derive_context(&entries, None),
            &desired,
            &tools,
            6,
        );
        assert!(again.is_empty(), "nothing changed, nothing is appended");
    }

    // 对照 pi 的 `planSystemEntries`：值变了只写最小补丁，删掉的段写 null，工具变更跟在最后一条上。
    #[test]
    fn a_changed_value_is_a_minimal_patch_and_tools_ride_along() {
        let mut entries = vec![entry(1, "komo.user", vec![Message::user("hi", 1)])];
        let tools = [tool("read")];
        let first = plan_system_entries(
            &entries,
            &derive_context(&entries, None),
            &[section("a", "1"), section("b", "2")],
            &tools,
            5,
        );
        apply(&mut entries, first);
        let next = plan_system_entries(
            &entries,
            &derive_context(&entries, None),
            &[section("a", "1b")],
            &[tool("read"), tool("rg")],
            6,
        );
        assert_eq!(next.len(), 1);
        let Message::System(system) = &next[0].model.as_ref().unwrap()[0] else {
            panic!("system expected");
        };
        assert_eq!(
            system.sections,
            Some(Sections(vec![
                ("a".into(), Some("1b".into())),
                ("b".into(), None)
            ]))
        );
        assert_eq!(system.tools_added.as_ref().unwrap().len(), 1);
        assert!(system.tools_removed.is_none());
    }

    // 对照 pi 的 `planSystemEntries`：次序变了要两条——全删再全加，回放才得到期望次序。
    #[test]
    fn a_reordered_section_set_is_removed_and_re_added() {
        let mut entries = vec![entry(1, "komo.user", vec![Message::user("hi", 1)])];
        let first = plan_system_entries(
            &entries,
            &derive_context(&entries, None),
            &[section("a", "1"), section("b", "2")],
            &[],
            5,
        );
        apply(&mut entries, first);
        let swapped = [section("b", "2"), section("a", "1")];
        let plan = plan_system_entries(&entries, &derive_context(&entries, None), &swapped, &[], 6);
        assert_eq!(plan.len(), 2);
        apply(&mut entries, plan);
        let shown = replay_sections(&derive_context(&entries, None).messages);
        assert_eq!(
            shown,
            vec![
                ("b".to_owned(), "2".to_owned()),
                ("a".to_owned(), "1".to_owned())
            ]
        );
    }

    // 对照 pi 的 `planSystemEntries`：head 之后没有 system entry 时写完整基线，并 omit 保留下来的旧的。
    #[test]
    fn after_a_reset_a_complete_baseline_replaces_the_retained_deltas() {
        let mut entries = vec![entry(1, "komo.user", vec![Message::user("hi", 1)])];
        let first = plan_system_entries(
            &entries,
            &derive_context(&entries, None),
            &[section("a", "1")],
            &[tool("read")],
            5,
        );
        apply(&mut entries, first);
        let reset = EntryRecord::new(EntryId::new(10), ConversationId::new(1), "komo.reset")
            .with_head(EntryId::new(10));
        entries.push(reset);
        let view = derive_context(&entries, None);
        let plan = plan_system_entries(&entries, &view, &[section("a", "1")], &[tool("read")], 9);
        assert_eq!(plan.len(), 1, "written even though it restates the values");
        let Message::System(system) = &plan[0].model.as_ref().unwrap()[0] else {
            panic!("system expected");
        };
        assert_eq!(system.tools_added.as_ref().unwrap().len(), 1);
    }
}
