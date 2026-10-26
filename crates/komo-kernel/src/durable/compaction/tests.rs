use serde_json::json;

use super::*;
use crate::durable::context::derive_context;
use crate::durable::ids::ConversationId;
use crate::projection::DecayPolicy;

const CONV: ConversationId = ConversationId::new(1);

fn id(n: u64) -> EntryId {
    EntryId::new(n)
}

fn entry(n: u64, kind: &str, model: Vec<Message>) -> EntryRecord {
    EntryRecord::new(id(n), CONV, kind).with_model(model)
}

fn user(n: u64, text: &str) -> EntryRecord {
    entry(n, "komo.user", vec![Message::user(text, 1)])
}

fn assistant_text(n: u64, text: &str, input: u64) -> EntryRecord {
    let mut message = Message::assistant(vec![ContentBlock::text(text)], StopReason::Stop, 1);
    if let Message::Assistant(assistant) = &mut message {
        assistant
            .extra
            .insert("usage".into(), json!({ "input": input, "output": 5 }));
    }
    entry(n, "komo.assistant", vec![message])
}

fn assistant_call(n: u64, call: &str, input: u64) -> EntryRecord {
    let mut message = Message::assistant(
        vec![ContentBlock::tool_call(call, "read", json!({"path": "a"}))],
        StopReason::ToolUse,
        1,
    );
    if let Message::Assistant(assistant) = &mut message {
        assistant
            .extra
            .insert("usage".into(), json!({ "input": input }));
    }
    entry(n, "komo.assistant", vec![message])
}

/// 一条带 `data.decayed` 的结果；正文 `full`。
fn result(n: u64, call: &str, full: &str, decayed: Option<&str>) -> EntryRecord {
    let mut record = entry(
        n,
        "komo.tool-result",
        vec![Message::tool_result(call, "read", full, 2)],
    );
    if let Some(decayed) = decayed {
        record = record.with_data(json!({ "decayed": decayed }));
    }
    record
}

fn big(n: usize) -> String {
    "x".repeat(n)
}

/// 用户 · 助手(调用 c1) · 结果 c1 · 助手(调用 c2) · 结果 c2 · 助手回答。
fn conversation() -> Vec<EntryRecord> {
    vec![
        user(1, "task"),
        assistant_call(2, "c1", 100),
        result(3, "c1", &big(400), None),
        assistant_call(4, "c2", 200),
        result(5, "c2", &big(400), None),
        assistant_text(6, "done", 300),
    ]
}

#[test]
fn the_cut_lands_on_an_assistant_so_a_call_keeps_its_result() {
    let entries = conversation();
    let view = derive_context(&entries, None);
    // 最新往回数：6 约 1 token、5 约 100 token。留 50 token 在 5 够数，5 是结果、不是候选，
    // 顺延到它之后的第一个候选 6。
    let cut = select_cut(&view, 50).expect("有切点");
    assert_eq!(cut.first_kept, id(6));
    // 留得多一点：够数在 4 之后——4 的调用要和 5 的结果一起留，切在 4。
    let cut = select_cut(&view, 150).expect("有切点");
    assert_eq!(cut.first_kept, id(4));
    assert_eq!(cut.tail, id(6));
    assert!(cut.archive_tokens > 0);
}

#[test]
fn a_tool_result_is_never_the_first_kept_entry() {
    let entries = conversation();
    let view = derive_context(&entries, None);
    for keep in 0..400 {
        if let Some(cut) = select_cut(&view, keep) {
            let kept = entries.iter().find(|e| e.id == cut.first_kept).unwrap();
            assert_ne!(kept.kind, "komo.tool-result", "keep = {keep}");
        }
    }
}

#[test]
fn nothing_is_cut_when_the_whole_context_is_within_the_keep_budget() {
    let entries = conversation();
    let view = derive_context(&entries, None);
    assert_eq!(select_cut(&view, 1_000_000), None, "整段都不到要保留的量");
    // 只有一条 user：切点之前什么也没有。
    let lone = derive_context(&[user(1, "hi")], None);
    assert_eq!(select_cut(&lone, 0), None);
}

#[test]
fn a_user_entry_between_a_call_and_its_result_is_not_a_cut_candidate() {
    // 助手调用 c1；插话的 user 落在结果之前：结果按规则 7 排到调用后面，所以在这条 user 前切
    // 会把调用与结果拆开。
    let entries = vec![
        user(1, "task"),
        assistant_call(2, "c1", 10),
        user(3, "interjection"),
        result(4, "c1", &big(400), None),
        assistant_text(5, "ok", 20),
    ];
    let view = derive_context(&entries, None);
    for keep in 0..200 {
        if let Some(cut) = select_cut(&view, keep) {
            assert_ne!(cut.first_kept, id(3), "keep = {keep}");
        }
    }
}

#[test]
fn an_earlier_summary_is_summarized_again_with_the_history_after_it() {
    let mut entries = conversation();
    // 摘要 entry：head = 4（保留 4 起的原样）。
    entries.push(
        entry(
            7,
            COMPACTION_ENTRY_KIND,
            vec![Message::user(wrap_summary("earlier work"), 3)],
        )
        .with_head(id(4)),
    );
    entries.push(user(8, "next task"));
    entries.push(assistant_text(9, "second answer", 10));
    let view = derive_context(&entries, None);
    assert_eq!(view.head, Some(id(7)));
    let cut = select_cut(&view, 1).expect("有切点");
    assert_eq!(cut.first_kept, id(9));
    let text = serialize_conversation(&summarized_messages(&view, cut.index));
    assert!(text.contains("earlier work"), "{text}");
    assert!(
        text.contains("[Assistant tool calls]: read(path=\"a\")"),
        "{text}"
    );
    assert!(text.contains("[User]: next task"), "{text}");
    assert!(!text.contains("second answer"), "切点之后的不收");
}

#[test]
fn serialization_truncates_long_results_and_leaves_out_system_and_thinking() {
    let messages = vec![
        Message::system(crate::durable::Sections::default().set("a", "prompt"), 1),
        Message::user("hello", 1),
        Message::assistant(
            vec![
                ContentBlock::Thinking {
                    thinking: "secret".into(),
                    extra: Default::default(),
                },
                ContentBlock::text("hi"),
            ],
            StopReason::Stop,
            1,
        ),
        Message::tool_result("c", "read", "y".repeat(2500), 1),
    ];
    let text = serialize_conversation(&messages);
    assert!(!text.contains("prompt") && !text.contains("secret"));
    assert!(text.starts_with("[User]: hello\n\n[Assistant]: hi\n\n[Tool result]: yyyy"));
    assert!(
        text.ends_with("[... 500 more characters truncated]"),
        "{text}"
    );
}

const POLICY: DecayPolicy = DecayPolicy {
    threshold_bytes: 100,
    full_sends: 2,
    head_bytes: 10,
    tail_bytes: 10,
};

fn decaying() -> Vec<EntryRecord> {
    vec![
        user(1, "task"),
        assistant_call(2, "c1", 100),
        result(3, "c1", &big(4000), Some("[short c1]")),
        assistant_call(4, "c2", 200),
        result(5, "c2", &big(4000), Some("[short c2]")),
        assistant_text(6, "done", 300),
    ]
}

#[test]
fn a_result_is_a_decay_candidate_only_after_its_full_sends() {
    let entries = decaying();
    let view = derive_context(&entries, None);
    let found = decay_candidates(&entries, &view, &POLICY);
    // c1 之后有 4、6 两条 assistant（2 次）；c2 之后只有 6（1 次）。
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].entry, id(3));
    assert_eq!(found[0].decayed, "[short c1]");
    assert_eq!(found[0].full_tokens, 1000);
    assert_eq!(found[0].message_index, 2);
}

#[test]
fn decay_edits_replace_the_view_and_derivation_picks_the_short_one() {
    let mut entries = decaying();
    let view = derive_context(&entries, None);
    let found = decay_candidates(&entries, &view, &POLICY);
    let before = view.messages.clone();
    let mut edit_entry =
        EntryRecord::new(id(7), CONV, DECAY_ENTRY_KIND).with_edits(decay_edits(&found));
    edit_entry.data = Some(json!({ "decayed": [3] }));
    entries.push(edit_entry);
    let after = derive_context(&entries, None).messages;
    assert_eq!(after.len(), before.len());
    let texts = |messages: &[Message]| -> Vec<String> {
        messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult(r) => match &r.content[0] {
                    ContentBlock::Text { text, .. } => Some(text.chars().take(10).collect()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        texts(&after),
        vec!["[short c1]".to_owned(), "xxxxxxxxxx".to_owned()]
    );
    // 衰减过的不再是候选。
    let view = derive_context(&entries, None);
    assert!(decay_candidates(&entries, &view, &POLICY).is_empty());
}

#[test]
fn online_state_counts_requests_debts_and_resets_like_the_old_fold() {
    let mut entries = conversation();
    let state = online_state(&entries, None);
    assert_eq!(state.request_count, 3);
    assert_eq!(state.last_context_tokens, Some(300));
    assert_eq!(state.positive_context_delta_total, 200);
    assert_eq!(state.compaction_count, 0);

    entries.push(
        entry(7, COMPACTION_ENTRY_KIND, vec![Message::user("s", 1)])
            .with_head(id(4))
            .with_data(json!({ "debt": { "debt_tokens": 1500.0, "repayment_tokens": 1000 } })),
    );
    let state = online_state(&entries, None);
    assert_eq!(state.compaction_count, 1);
    assert_eq!(state.cache_debt_tokens, 1500.0);
    assert_eq!(state.last_context_tokens, None, "摘要换掉了前面的内容");
    assert_eq!(state.requests_since_last_summary(), Some(0));

    entries.push(assistant_text(8, "again", 900));
    let state = online_state(&entries, None);
    assert_eq!(state.requests_since_last_summary(), Some(1));
    assert_eq!(state.cache_debt_tokens, 500.0, "每次请求还一次债");

    entries.push(
        EntryRecord::new(id(9), CONV, DECAY_ENTRY_KIND)
            .with_data(json!({ "debt": { "debt_tokens": 10.0, "repayment_tokens": 5 } })),
    );
    let state = online_state(&entries, None);
    assert_eq!(state.requests_since_last_compaction(), Some(0));
    assert_eq!(
        state.requests_since_last_summary(),
        Some(1),
        "衰减不算压成摘要"
    );

    entries.push(EntryRecord::new(id(10), CONV, "komo.reset").with_head(id(10)));
    assert_eq!(online_state(&entries, None), OnlineState::default());
}

#[test]
fn a_refused_summary_holds_until_real_tool_work() {
    let mut entries = conversation();
    let refused_at = id(6);
    assert!(online_state(&entries, Some(refused_at)).compaction_refused);
    // 失败的调用不算工作。
    let mut failed = result(7, "c3", "boom", None);
    if let Some(Message::ToolResult(r)) = failed.model.as_mut().and_then(|m| m.first_mut()) {
        r.is_error = true;
    }
    entries.push(failed);
    assert!(online_state(&entries, Some(refused_at)).compaction_refused);
    entries.push(result(8, "c4", "fine", None));
    assert!(!online_state(&entries, Some(refused_at)).compaction_refused);
}

#[test]
fn incremental_online_fold_matches_full_history_with_changing_refusal_markers() {
    let mut entries = conversation();
    entries.push(
        entry(7, COMPACTION_ENTRY_KIND, vec![Message::user("summary", 1)])
            .with_head(id(4))
            .with_data(json!({"debt": {"debt_tokens": 1500.0, "repayment_tokens": 1000}})),
    );
    entries.push(assistant_text(8, "repay", 900));
    entries.push(
        EntryRecord::new(id(9), CONV, DECAY_ENTRY_KIND)
            .with_data(json!({"debt": {"debt_tokens": 10.0, "repayment_tokens": 5}})),
    );
    entries.push(result(10, "c3", "work", None));
    entries.push(EntryRecord::new(id(11), CONV, "komo.reset").with_head(id(11)));
    entries.push(assistant_text(12, "new", 42));
    for split in 0..=entries.len() {
        let mut fold = OnlineFold::default();
        fold.extend(&entries[..split]);
        for end in split..=entries.len() {
            if end > split {
                fold.extend(&entries[end - 1..end]);
            }
            for refused in std::iter::once(None).chain((0..=13).map(|n| Some(id(n)))) {
                assert_eq!(
                    fold.state(refused),
                    online_state(&entries[..end], refused),
                    "split={split}, end={end}, refused={refused:?}"
                );
            }
        }
    }
}

#[test]
fn the_cache_is_cold_after_the_ttl_and_only_counting_finished_replies() {
    let entries = vec![user(1, "hi"), assistant_text(2, "yo", 10)];
    // assistant_text 的时间戳是 1。
    assert!(
        !cache_cold(&[user(1, "hi")], 10_000_000, 300_000),
        "没请求过不算冷"
    );
    assert!(!cache_cold(&entries, 200_000, 300_000));
    assert!(cache_cold(&entries, 301_002, 300_000));
    let mut with_error = entries.clone();
    with_error.push(entry(
        3,
        "komo.assistant",
        vec![Message::assistant(vec![], StopReason::Error, 400_000)],
    ));
    assert!(
        cache_cold(&with_error, 401_000, 300_000),
        "失败的回复不算缓存写入"
    );
}
