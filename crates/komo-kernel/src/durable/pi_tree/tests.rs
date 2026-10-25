use std::collections::HashMap;

use serde_json::{Value, json};

use super::*;
use crate::durable::compaction::wrap_summary;
use crate::durable::context::derive_context;
use crate::durable::fork::{fork_chain, visible_entries};
use crate::durable::message::{ContentBlock, Message, StopReason};
use crate::durable::storage::ScanOrder;

fn cid(n: u64) -> ConversationId {
    ConversationId::new(n)
}

fn eid(n: u64) -> EntryId {
    EntryId::new(n)
}

fn entry(id: u64, conversation: u64, kind: &str) -> EntryRecord {
    EntryRecord::new(eid(id), cid(conversation), kind)
}

fn user(id: u64, conversation: u64, text: &str) -> EntryRecord {
    entry(id, conversation, "komo.user").with_model(vec![Message::user(text, id as i64 * 1000)])
}

fn assistant(id: u64, conversation: u64, text: &str, calls: &[&str]) -> EntryRecord {
    let mut content = vec![ContentBlock::text(text)];
    content.extend(
        calls
            .iter()
            .map(|call| ContentBlock::tool_call(*call, "read", json!({}))),
    );
    let stop = if calls.is_empty() {
        StopReason::Stop
    } else {
        StopReason::ToolUse
    };
    entry(id, conversation, "komo.assistant").with_model(vec![Message::assistant(
        content,
        stop,
        id as i64 * 1000,
    )])
}

fn result(id: u64, conversation: u64, call: &str, text: &str) -> EntryRecord {
    entry(id, conversation, "komo.tool-result").with_model(vec![Message::tool_result(
        call,
        "read",
        text,
        id as i64 * 1000,
    )])
}

struct Session {
    conversations: Vec<ConversationRecord>,
    entries: Vec<Vec<EntryRecord>>,
}

impl Session {
    fn new(shape: Vec<(ConversationRecord, Vec<EntryRecord>)>) -> Self {
        let (conversations, entries) = shape.into_iter().unzip();
        Self {
            conversations,
            entries,
        }
    }

    fn inputs(&self) -> Vec<ConversationEntries<'_>> {
        self.conversations
            .iter()
            .zip(&self.entries)
            .map(|(record, entries)| ConversationEntries { record, entries })
            .collect()
    }

    fn tree(&self, branch: u64, leaf: Option<u64>) -> PiTree {
        pi_tree(&self.inputs(), cid(branch), leaf.map(eid))
    }

    /// durable 在 `conversation` 的 `cutoff`（含）处的上下文：每条 entry 的贡献展平，去掉 system 消息。
    fn durable_context(&self, conversation: u64, cutoff: u64) -> Vec<Value> {
        let records: HashMap<ConversationId, &ConversationRecord> = self
            .conversations
            .iter()
            .map(|record| (record.id, record))
            .collect();
        let by_conversation: HashMap<ConversationId, &[EntryRecord]> = self
            .conversations
            .iter()
            .zip(&self.entries)
            .map(|(record, entries)| (record.id, entries.as_slice()))
            .collect();
        let cutoff = eid(cutoff);
        let chain = fork_chain(
            cid(conversation),
            |id| records.get(&id).copied(),
            None,
            Some(cutoff),
        )
        .unwrap();
        let visible: Vec<EntryRecord> = visible_entries(
            &chain,
            |id| by_conversation.get(&id).copied().unwrap_or(&[]),
            None,
            Some(cutoff),
            ScanOrder::Ascending,
        )
        .into_iter()
        .cloned()
        .collect();
        derive_context(&visible, Some(cutoff))
            .contributions
            .into_iter()
            .flatten()
            .filter(|message| !message.is_system())
            .map(|message| serde_json::to_value(message).unwrap())
            .collect()
    }

    /// pi 在 `leaf` 处的上下文，压缩摘要换回 durable 的包好的 user 消息。
    fn pi_context(&self, conversation: u64, leaf: u64) -> Vec<Value> {
        let mut tree = self
            .tree(conversation, Some(leaf))
            .into_session_tree()
            .unwrap();
        // `into_session_tree` 已经把位置放在 leaf_id。
        assert!(tree.leaf_id().is_some());
        tree.branch(tree.leaf_id().map(str::to_owned).as_deref())
            .unwrap();
        tree.project()
            .unwrap()
            .messages
            .into_iter()
            .map(|message| {
                if message["role"] == "compactionSummary" {
                    json!({
                        "role": "user",
                        "content": wrap_summary(message["summary"].as_str().unwrap()),
                        "timestamp": message["timestamp"],
                    })
                } else {
                    message
                }
            })
            .collect()
    }

    /// 每条 entry 作 leaf，pi 与 durable 的上下文都一致。
    fn assert_every_leaf_agrees(&self) {
        for (record, entries) in self.conversations.iter().zip(&self.entries) {
            for entry in entries {
                assert_eq!(
                    self.pi_context(record.id.get(), entry.id.get()),
                    self.durable_context(record.id.get(), entry.id.get()),
                    "conversation {} leaf {} ({})",
                    record.id,
                    entry.id,
                    entry.kind
                );
            }
        }
    }
}

fn ids(tree: &PiTree) -> Vec<(&str, Option<&str>)> {
    tree.entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry.parent_id.as_deref()))
        .collect()
}

#[test]
fn pi_tree_round_trips_through_session_tree() {
    // 根 1：1..=5，2 在 3 处 fork 出 6..=8，3 在 6 处再 fork 出 9。导航位置在 7。
    let session = Session::new(vec![
        (
            ConversationRecord::root(cid(1)),
            vec![
                user(1, 1, "hello"),
                assistant(2, 1, "let me look", &["c1"]),
                result(3, 1, "c1", "file text"),
                assistant(4, 1, "done", &[]),
                entry(5, 1, "komo.settled").with_data(json!({"submission": 0})),
            ],
        ),
        (
            ConversationRecord::fork(cid(2), cid(1), eid(3)),
            vec![
                user(6, 2, "actually, different"),
                assistant(7, 2, "ok", &[]),
                entry(8, 2, "label").with_data(json!({"targetId": 6, "label": "fork"})),
            ],
        ),
        (
            ConversationRecord::fork(cid(3), cid(2), eid(6)),
            vec![user(9, 3, "third way")],
        ),
    ]);
    let tree = session.tree(2, Some(7));
    assert_eq!(
        ids(&tree),
        vec![
            ("1", None),
            ("2", Some("1")),
            ("3", Some("2")),
            ("4", Some("3")),
            ("5", Some("4")),
            ("6", Some("3")),
            ("7", Some("6")),
            ("8", Some("7")),
            ("9", Some("6")),
        ]
    );
    assert_eq!(tree.leaf_id.as_deref(), Some("7"));

    let restored = tree.clone().into_session_tree().unwrap();
    assert_eq!(restored.entries().len(), 9);
    assert_eq!(restored.leaf_id(), Some("7"));
    assert_eq!(restored.label("6"), Some("fork"));
    let path: Vec<&str> = restored
        .path(restored.leaf_id())
        .unwrap()
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    assert_eq!(path, ["1", "2", "3", "6", "7"]);
    // 序列化再读回：pi 的快照格式不丢导航位置。
    let snapshot = serde_json::to_string(&restored).unwrap();
    let again: SessionTree = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(again.leaf_id(), Some("7"));
    assert_eq!(again.entries(), restored.entries());

    session.assert_every_leaf_agrees();
}

#[test]
fn the_default_leaf_is_the_end_of_the_current_branch() {
    let session = Session::new(vec![
        (
            ConversationRecord::root(cid(1)),
            vec![user(1, 1, "a"), assistant(2, 1, "b", &[])],
        ),
        (ConversationRecord::fork(cid(2), cid(1), eid(1)), vec![]),
        (ConversationRecord::root(cid(3)), vec![]),
    ]);
    assert_eq!(session.tree(1, None).leaf_id.as_deref(), Some("2"));
    // 空的 fork 站在它的 fork 点上；空的新根在所有 entry 之前。
    assert_eq!(session.tree(2, None).leaf_id.as_deref(), Some("1"));
    assert_eq!(session.tree(3, None).leaf_id, None);
    let empty = session.tree(3, None).into_session_tree().unwrap();
    assert_eq!(empty.leaf_id(), None);
}

#[test]
fn a_new_root_conversation_starts_a_second_root() {
    let session = Session::new(vec![
        (ConversationRecord::root(cid(1)), vec![user(1, 1, "a")]),
        (ConversationRecord::root(cid(2)), vec![user(2, 2, "b")]),
    ]);
    let tree = session.tree(2, None);
    assert_eq!(ids(&tree), vec![("1", None), ("2", None)]);
    assert_eq!(tree.leaf_id.as_deref(), Some("2"));
}

#[test]
fn compaction_context_edit_and_decay_agree_with_derive_context() {
    let baseline = entry(11, 1, "komo.system")
        .with_model(vec![Message::system(
            crate::durable::message::Sections::default().set("base", "be brief"),
            11_000,
        )])
        .with_edits(vec![ContextEdit::omit(eid(1))]);
    let session = Session::new(vec![(
        ConversationRecord::root(cid(1)),
        vec![
            entry(1, 1, "komo.system").with_model(vec![Message::system(
                crate::durable::message::Sections::default().set("base", "be helpful"),
                1,
            )]),
            user(2, 1, "first question"),
            assistant(3, 1, "calling", &["c1"]),
            result(4, 1, "c1", "a very long result"),
            assistant(5, 1, "first answer", &[]),
            user(6, 1, "second question"),
            assistant(7, 1, "second answer", &[]),
            // 衰减：把 4 换成短视图（只换 content，其余字段不动）。
            entry(8, 1, "komo.decay")
                .with_data(json!({"debt": {"debt_tokens": 1.0, "repayment_tokens": 2}}))
                .with_edits(vec![ContextEdit::replace(
                    eid(4),
                    vec![Message::tool_result("c1", "read", "[short view]", 4000)],
                )]),
            // context.edit：omit 7，改写 2 的内容。
            entry(9, 1, "context.edit").with_edits(vec![
                ContextEdit::omit(eid(7)),
                ContextEdit::replace(eid(2), vec![Message::user("edited question", 2000)]),
            ]),
            // 压缩：留下从 6 起的，摘要换掉 2..=5。
            entry(10, 1, "komo.compaction")
                .with_head(eid(6))
                .with_model(vec![Message::user(
                    wrap_summary("they asked and I answered"),
                    10_000,
                )])
                .with_data(json!({"reason": "manual", "decision": null})),
            baseline,
            user(12, 1, "third question"),
            assistant(13, 1, "third answer", &[]),
        ],
    )]);
    session.assert_every_leaf_agrees();

    // 压缩之后：摘要 + 保留的 6，7 被 omit，12、13 在后面。
    let after = session.pi_context(1, 13);
    let texts: Vec<String> = after
        .iter()
        .map(|message| serde_json::to_string(&message["content"]).unwrap())
        .collect();
    assert!(texts[0].contains("they asked and I answered"), "{texts:?}");
    assert_eq!(after.len(), 4, "{after:?}");

    // 压缩 entry 的 pi 形状。
    let tree = session.tree(1, None);
    let compaction = tree.entries.iter().find(|entry| entry.id == "10").unwrap();
    let EntryKind::Compaction {
        summary,
        first_kept_entry_id,
        ..
    } = &compaction.kind
    else {
        panic!("{compaction:?}");
    };
    assert_eq!(summary, "they asked and I answered");
    assert_eq!(first_kept_entry_id, "6");
    // 一个 entry 的两个 edit 展开成两条 ContextEdit，基础 ID 是第一条。
    let edits: Vec<&str> = tree
        .entries
        .iter()
        .filter(|entry| entry.id.starts_with("9"))
        .map(|entry| entry.id.as_str())
        .collect();
    assert_eq!(edits, ["9", "9:1"]);
    assert_eq!(entry_of("9:1"), Some(eid(9)));
    // 基线 system 是 Custom，它的 omit edit 紧跟其后。
    assert!(matches!(
        tree.entries
            .iter()
            .find(|entry| entry.id == "11")
            .unwrap()
            .kind,
        EntryKind::Custom { .. }
    ));
}

#[test]
fn a_reset_drops_everything_before_it_and_keeps_the_handoff() {
    let session = Session::new(vec![(
        ConversationRecord::root(cid(1)),
        vec![
            user(1, 1, "old"),
            assistant(2, 1, "old answer", &[]),
            entry(3, 1, "komo.reset")
                .with_head(eid(3))
                .with_model(vec![Message::user("handoff: carry on", 3000)]),
            user(4, 1, "new"),
        ],
    )]);
    session.assert_every_leaf_agrees();
    let context = session.pi_context(1, 4);
    assert_eq!(context.len(), 2);
    assert_eq!(context[0]["content"], "handoff: carry on");
}

#[test]
fn labels_names_branch_summaries_and_model_changes_get_their_own_entries() {
    let session = Session::new(vec![(
        ConversationRecord::root(cid(1)),
        vec![
            user(1, 1, "hi"),
            entry(2, 1, "label").with_data(json!({"targetId": 1, "label": "start"})),
            entry(3, 1, "session.info").with_data(json!({"name": "my session"})),
            entry(4, 1, "model.change")
                .with_data(json!({"provider": "anthropic", "modelId": "m-1"})),
            entry(5, 1, "branch.summary")
                .with_data(json!({"fromId": 1}))
                .with_model(vec![Message::user("the other branch did X", 5000)]),
            entry(6, 1, "komo.settled").with_data(json!({"submission": 0, "status": "done"})),
            entry(7, 1, "weird.kind").with_data(json!({"x": 1})),
        ],
    )]);
    let tree = session.tree(1, None).into_session_tree().unwrap();
    assert_eq!(tree.label("1"), Some("start"));
    assert_eq!(tree.name(), Some("my session"));
    let kinds: Vec<&EntryKind> = tree.entries().iter().map(|entry| &entry.kind).collect();
    assert!(
        matches!(kinds[3], EntryKind::ModelChange { provider, model_id }
        if provider == "anthropic" && model_id == "m-1")
    );
    assert!(
        matches!(kinds[4], EntryKind::BranchSummary { from_id, summary, .. }
        if from_id == "1" && summary == "the other branch did X")
    );
    let EntryKind::Custom { custom_type, data } = kinds[5] else {
        panic!("{:?}", kinds[5]);
    };
    assert_eq!(custom_type, "komo.settled");
    assert_eq!(data.as_ref().unwrap()["data"]["submission"], 0);
    assert!(
        matches!(kinds[6], EntryKind::Custom { custom_type, .. } if custom_type == "weird.kind")
    );
    // 展示用的 entry 不改上下文；branch summary 是有内容的消息。
    let messages = tree.project().unwrap().messages;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["role"], "branchSummary");
}

#[test]
fn input_entries_of_custom_kinds_are_messages() {
    let session = Session::new(vec![(
        ConversationRecord::root(cid(1)),
        vec![
            entry(1, 1, "chat.input")
                .with_data(json!({"peer": "p", "principal": "u"}))
                .with_model(vec![Message::user("from chat", 1000)]),
            assistant(2, 1, "hi", &[]),
        ],
    )]);
    session.assert_every_leaf_agrees();
    let tree = session.tree(1, None);
    assert!(matches!(
        &tree.entries[0].kind,
        EntryKind::Message { message } if message["role"] == "user"
    ));
}

#[test]
fn pi_ids_decode_only_in_canonical_form() {
    assert_eq!(pi_id(eid(42)), "42");
    assert_eq!(entry_of("42"), Some(eid(42)));
    assert_eq!(entry_of("42:3"), Some(eid(42)));
    for bad in ["", "042", "+1", "x", "1-fork", ":1"] {
        assert_eq!(entry_of(bad), None, "{bad:?}");
    }
}
