use super::*;
use serde_json::{Value, json};

const DATE: &str = "2024-12-03T14:00:00.000Z";
const MILLIS: i64 = 1_733_234_400_000;

fn entry(id: &str, parent: Option<&str>, fields: Value) -> SessionEntry {
    let mut value = fields;
    value["id"] = json!(id);
    value["parentId"] = json!(parent);
    value["timestamp"] = json!(DATE);
    serde_json::from_value(value).unwrap()
}

fn message(id: &str, parent: Option<&str>, role: &str, content: Value) -> SessionEntry {
    entry(
        id,
        parent,
        json!({"type": "message", "message": {
            "role": role, "content": content, "timestamp": MILLIS,
        }}),
    )
}

fn compact(id: &str, parent: &str, first: &str, summary: &str) -> SessionEntry {
    entry(
        id,
        Some(parent),
        json!({
            "type": "compaction", "summary": summary,
            "firstKeptEntryId": first, "tokensBefore": 100,
        }),
    )
}

fn edit(id: &str, parent: &str, target: &str, replacement: Value) -> SessionEntry {
    entry(
        id,
        Some(parent),
        json!({
            "type": "context_edit", "targetId": target, "replacement": replacement,
        }),
    )
}

fn label(id: &str, parent: Option<&str>, target: &str, name: Value) -> SessionEntry {
    entry(
        id,
        parent,
        json!({"type": "label", "targetId": target, "label": name}),
    )
}

fn ids(entries: &[&SessionEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id.clone()).collect()
}

#[test]
fn all_entry_variants_round_trip_pi_camel_case_without_a_kind_wrapper() {
    let cases = [
        json!({"type": "message", "message": {"role": "user", "content": "hello"}}),
        json!({"type": "model_change", "provider": "p", "modelId": "m"}),
        json!({"type": "thinking_level_change", "thinkingLevel": "high"}),
        json!({"type": "compaction", "summary": "s", "firstKeptEntryId": "root",
            "tokensBefore": u64::MAX, "systemMessage": {"role": "system"},
            "details": {"files": []}, "usage": {"cost": 1}, "fromHook": false}),
        json!({"type": "context_edit", "targetId": "root", "replacement": null}),
        json!({"type": "branch_summary", "fromId": "other", "summary": "s",
            "details": {}, "usage": {}, "fromHook": true}),
        json!({"type": "custom", "customType": "state", "data": {"count": 1}}),
        json!({"type": "custom_message", "customType": "context", "content": "c",
            "display": false, "details": {"private": true}}),
        json!({"type": "label", "targetId": "root", "label": "bookmark"}),
        json!({"type": "session_info", "name": "session"}),
        json!({"type": "usage", "kind": "future_kind", "provider": "p", "model": "m",
            "usage": {"totalTokens": 5}}),
    ];
    for mut expected in cases {
        expected["id"] = json!("id");
        expected["parentId"] = Value::Null;
        expected["timestamp"] = json!(DATE);
        let parsed: SessionEntry = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    }
}

#[test]
fn optional_entry_fields_default_and_header_is_a_v3_session_record() {
    for fields in [
        json!({"type": "compaction", "summary": "s", "firstKeptEntryId": "x", "tokensBefore": 1}),
        json!({"type": "branch_summary", "summary": "s", "fromId": "x"}),
        json!({"type": "custom", "customType": "x"}),
        json!({"type": "custom_message", "customType": "x", "content": [], "display": true}),
        json!({"type": "label", "targetId": "x"}),
        json!({"type": "context_edit", "targetId": "x"}),
    ] {
        let parsed = entry("id", None, fields);
        assert_eq!(
            serde_json::from_value::<SessionEntry>(serde_json::to_value(&parsed).unwrap()).unwrap(),
            parsed
        );
    }
    let mut header = SessionHeader::new("session".into(), DATE.into(), "/project".into());
    let value = serde_json::to_value(&header).unwrap();
    assert_eq!(
        value,
        json!({"type": "session", "version": 3, "id": "session", "timestamp": DATE, "cwd": "/project"})
    );
    assert_eq!(
        serde_json::from_value::<SessionHeader>(value).unwrap(),
        header
    );
    header.parent_session = Some("/original/session.jsonl".into());
    assert_eq!(
        serde_json::to_value(&header).unwrap()["parentSession"],
        "/original/session.jsonl"
    );
}

#[test]
fn navigation_preserves_siblings_and_supports_multiple_roots_and_empty_paths() {
    let mut tree = SessionTree::new();
    assert!(tree.path(None).unwrap().is_empty());
    assert!(tree.project().unwrap().messages.is_empty());
    tree.append(message("root", None, "user", json!("start")))
        .unwrap();
    tree.append(message("left", Some("root"), "assistant", json!([])))
        .unwrap();
    tree.branch(Some("root")).unwrap();
    tree.append(message("right", tree.leaf_id(), "user", json!("right")))
        .unwrap();
    assert_eq!(ids(&tree.path(tree.leaf_id()).unwrap()), ["root", "right"]);
    assert_eq!(ids(&tree.children(Some("root"))), ["left", "right"]);
    assert_eq!(tree.entries().len(), 3);
    tree.branch(None).unwrap();
    assert!(tree.context_entries().unwrap().is_empty());
    tree.append(message("another-root", None, "user", json!("second")))
        .unwrap();
    assert_eq!(ids(&tree.children(None)), ["root", "another-root"]);
    assert_eq!(ids(&tree.path(Some("left")).unwrap()), ["root", "left"]);
    assert!(tree.path(Some("missing")).is_err());
}

#[test]
fn invalid_appends_and_navigation_are_atomic() {
    let mut tree =
        SessionTree::from_entries(vec![message("root", None, "user", json!("a"))]).unwrap();
    let before = serde_json::to_value(&tree).unwrap();
    assert!(
        tree.append(message("root", None, "user", json!("duplicate")))
            .is_err()
    );
    assert!(
        tree.append(message("orphan", Some("missing"), "user", json!("bad")))
            .is_err()
    );
    assert!(
        tree.append(message("self", Some("self"), "user", json!("bad")))
            .is_err()
    );
    assert!(tree.branch(Some("missing")).is_err());
    assert_eq!(serde_json::to_value(tree).unwrap(), before);
}

#[test]
fn snapshots_rebuild_indexes_and_preserve_navigation_but_file_restore_uses_last_entry() {
    let mut tree = SessionTree::from_entries(vec![
        message("a", None, "user", json!("a")),
        message("b", Some("a"), "user", json!("b")),
    ])
    .unwrap();
    tree.branch(Some("a")).unwrap();
    let snapshot = serde_json::to_value(&tree).unwrap();
    assert_eq!(snapshot.as_object().unwrap().len(), 2);
    assert_eq!(snapshot["leafId"], "a");
    let mut restored: SessionTree = serde_json::from_value(snapshot).unwrap();
    assert_eq!(restored.get("b").unwrap().id, "b");
    assert_eq!(restored.leaf_id(), Some("a"));
    assert_eq!(
        SessionTree::from_entries(restored.entries().to_vec())
            .unwrap()
            .leaf_id(),
        Some("b")
    );
    restored.branch(None).unwrap();
    let empty_path: SessionTree =
        serde_json::from_value(serde_json::to_value(restored).unwrap()).unwrap();
    assert_eq!(empty_path.leaf_id(), None);
    assert!(empty_path.context_entries().unwrap().is_empty());
}

#[test]
fn malformed_cycles_missing_parents_duplicate_ids_and_unknown_leaves_are_rejected() {
    let cyclic = vec![
        message("a", Some("b"), "user", json!("a")),
        message("b", Some("a"), "user", json!("b")),
    ];
    assert!(SessionTree::from_entries(cyclic.clone()).is_err());
    assert!(
        serde_json::from_value::<SessionTree>(json!({"entries": cyclic, "leafId": "b"})).is_err()
    );
    let root = message("a", None, "user", json!("a"));
    assert!(SessionTree::from_entries(vec![root.clone(), root.clone()]).is_err());
    assert!(
        serde_json::from_value::<SessionTree>(
            json!({"entries": [root.clone()], "leafId": "missing"})
        )
        .is_err()
    );
    // Even an internally corrupted tree cannot hang a caller walking its path.
    let mut tree = SessionTree::from_entries(vec![root]).unwrap();
    tree.entries[0].parent_id = Some("a".into());
    assert!(tree.path(Some("a")).unwrap_err().contains("Cycle"));
    tree.entries[0].parent_id = Some("missing".into());
    assert!(tree.project().is_err());
}

#[test]
fn compaction_places_checkpoint_first_drops_retained_systems_and_keeps_later_patches() {
    let checkpoint = json!({"role": "system", "content": "checkpoint", "toolsAdded": [{"name": "read"}], "timestamp": MILLIS});
    let mut compaction = compact("compact", "system", "user", "summary");
    if let EntryKind::Compaction { system_message, .. } = &mut compaction.kind {
        *system_message = Some(checkpoint.clone());
    }
    let tree = SessionTree::from_entries(vec![
        message("old", None, "user", json!("discard")),
        message("user", Some("old"), "user", json!("keep")),
        message("system", Some("user"), "system", json!("old prompt")),
        compaction,
        message("patch", Some("compact"), "system", json!("new prompt")),
    ])
    .unwrap();
    assert_eq!(
        ids(&tree.context_entries().unwrap()),
        ["compact", "user", "patch"]
    );
    let projection = tree.project().unwrap();
    assert_eq!(projection.messages[0], checkpoint);
    assert_eq!(
        projection.messages[1],
        json!({"role": "compactionSummary", "summary": "summary", "tokensBefore": 100, "timestamp": MILLIS})
    );
    assert_eq!(projection.messages[2]["content"], "keep");
    assert_eq!(projection.messages[3]["content"], "new prompt");
}

#[test]
fn repeated_compaction_retains_old_raw_compactions_without_projecting_their_summaries() {
    let tree = SessionTree::from_entries(vec![
        message("user", None, "user", json!("keep")),
        compact("first", "user", "user", "old summary"),
        message("reply", Some("first"), "assistant", json!([])),
        compact("second", "reply", "user", "latest summary"),
    ])
    .unwrap();
    assert_eq!(
        ids(&tree.context_entries().unwrap()),
        ["second", "user", "first", "reply"]
    );
    let messages = tree.project().unwrap().messages;
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["summary"], "latest summary");
    assert_eq!(messages[1]["content"], "keep");
    assert_eq!(messages[2]["role"], "assistant");
}

#[test]
fn retain_none_and_unknown_first_kept_ids_exclude_all_earlier_entries() {
    for first in ["second", "missing"] {
        let tree = SessionTree::from_entries(vec![
            message("user", None, "user", json!("discard")),
            compact("first", "user", "user", "old summary"),
            compact("second", "first", first, "latest summary"),
            message("later", Some("second"), "user", json!("after")),
        ])
        .unwrap();
        assert_eq!(ids(&tree.context_entries().unwrap()), ["second", "later"]);
        let messages = tree.project().unwrap().messages;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["summary"], "latest summary");
        assert_eq!(messages[1]["content"], "after");
    }
}

#[test]
fn model_and_thinking_restore_from_whole_active_path_including_compacted_settings() {
    let mut tree = SessionTree::from_entries(vec![
        entry("model", None, json!({"type": "model_change", "provider": "p", "modelId": "virtual"})),
        entry("thinking", Some("model"), json!({"type": "thinking_level_change", "thinkingLevel": "high"})),
        entry("assistant", Some("thinking"), json!({"type": "message", "message": {
            "role": "assistant", "provider": "physical", "model": "actual", "thinkingLevel": "low", "content": [],
        }})),
        compact("compact", "assistant", "compact", "s"),
    ]).unwrap();
    let projection = tree.project().unwrap();
    assert_eq!(projection.model, Some(("physical".into(), "actual".into())));
    assert_eq!(projection.thinking_level, "high");
    tree.append(entry(
        "new-model",
        Some("compact"),
        json!({"type": "model_change", "provider": "p2", "modelId": "virtual2"}),
    ))
    .unwrap();
    assert_eq!(
        tree.project().unwrap().model,
        Some(("p2".into(), "virtual2".into()))
    );
    tree.branch(Some("model")).unwrap();
    assert_eq!(tree.project().unwrap().thinking_level, "off");
    tree.branch(None).unwrap();
    assert_eq!(tree.project().unwrap().model, None);
}

#[test]
fn context_edits_are_branch_local_latest_wins_and_original_content_is_immutable() {
    let mut tree = SessionTree::from_entries(vec![
        message("user", None, "user", json!("original")),
        edit("replace", "user", "user", json!({"content": "first edit"})),
        edit("omit", "replace", "user", Value::Null),
        edit("restore", "omit", "user", json!({"content": "last edit"})),
        message("sibling", Some("user"), "user", json!("sibling")),
    ])
    .unwrap();
    assert_eq!(tree.project().unwrap().messages[0]["content"], "original");
    tree.branch(Some("replace")).unwrap();
    assert_eq!(tree.project().unwrap().messages[0]["content"], "first edit");
    tree.branch(Some("omit")).unwrap();
    assert!(tree.project().unwrap().messages.is_empty());
    tree.branch(Some("restore")).unwrap();
    assert_eq!(tree.project().unwrap().messages[0]["content"], "last edit");
    assert!(
        matches!(&tree.get("user").unwrap().kind, EntryKind::Message { message } if message["content"] == "original")
    );
    tree.branch(Some("user")).unwrap();
    assert_eq!(tree.project().unwrap().messages[0]["content"], "original");
}

#[test]
fn edits_normalize_assistant_and_tool_text_and_preserve_metadata_and_custom_display() {
    let mut tree = SessionTree::from_entries(vec![
        entry("a", None, json!({"type": "message", "message": {
            "role": "assistant", "provider": "p", "model": "m", "content": [{"type": "toolCall", "id": "call"}], "usage": {"totalTokens": 42},
        }})),
        entry("tool", Some("a"), json!({"type": "message", "message": {
            "role": "toolResult", "toolCallId": "call", "toolName": "read", "isError": true, "content": [],
        }})),
        entry("custom", Some("tool"), json!({"type": "custom_message", "customType": "injection", "content": "raw", "display": false})),
        edit("ea", "custom", "a", json!({"content": "assistant text"})),
        edit("et", "ea", "tool", json!({"content": "tool text"})),
        edit("ec", "et", "custom", json!({"content": "custom text"})),
    ]).unwrap();
    let messages = tree.project().unwrap().messages;
    assert_eq!(
        messages[0]["content"],
        json!([{"type": "text", "text": "assistant text"}])
    );
    assert_eq!(messages[0]["usage"], json!({"totalTokens": 42}));
    assert_eq!(
        messages[1]["content"],
        json!([{"type": "text", "text": "tool text"}])
    );
    assert_eq!(messages[1]["toolCallId"], "call");
    assert_eq!(messages[1]["isError"], true);
    assert_eq!(messages[2]["content"], "custom text");
    assert_eq!(messages[2]["role"], "custom");
    assert_eq!(messages[2]["display"], false);
    tree.append(edit(
        "array",
        "ec",
        "a",
        json!({"content": [{"type": "text", "text": "array"}]}),
    ))
    .unwrap();
    assert_eq!(
        tree.project().unwrap().messages[0]["content"],
        json!([{"type": "text", "text": "array"}])
    );
}

#[test]
fn retained_edits_survive_compaction_without_projecting_edits_as_messages() {
    let tree = SessionTree::from_entries(vec![
        message("user", None, "user", json!("raw")),
        edit("edit", "user", "user", json!({"content": "edited"})),
        compact("compact", "edit", "user", "summary"),
    ])
    .unwrap();
    assert_eq!(
        ids(&tree.context_entries().unwrap()),
        ["compact", "user", "edit"]
    );
    let messages = tree.project().unwrap().messages;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["content"], "edited");
}

#[test]
fn conversion_uses_pi_summary_and_custom_roles_and_excludes_state_only_entries() {
    let tree = SessionTree::from_entries(vec![
        entry("branch", None, json!({"type": "branch_summary", "summary": "left branch", "fromId": "left", "usage": {"totalTokens": 1}})),
        entry("empty", Some("branch"), json!({"type": "branch_summary", "summary": "", "fromId": "left"})),
        entry("custom", Some("empty"), json!({"type": "custom_message", "customType": "injection", "content": [{"type": "image", "data": "x"}], "display": true, "details": {"private": true}})),
        entry("state", Some("custom"), json!({"type": "custom", "customType": "state", "data": 123})),
        entry("usage", Some("state"), json!({"type": "usage", "kind": "new_kind", "provider": "p", "model": "m", "usage": {"totalTokens": 1}})),
        entry("info", Some("usage"), json!({"type": "session_info", "name": "title"})),
        label("label", Some("info"), "branch", json!("bookmark")),
    ]).unwrap();
    assert_eq!(
        tree.project().unwrap().messages,
        vec![
            json!({"role": "branchSummary", "summary": "left branch", "fromId": "left", "timestamp": MILLIS}),
            json!({"role": "custom", "customType": "injection", "content": [{"type": "image", "data": "x"}], "display": true, "details": {"private": true}, "timestamp": MILLIS}),
        ]
    );
}

#[test]
fn missing_message_content_is_normalized_without_losing_native_provider_fields() {
    let tree = SessionTree::from_entries(vec![
        entry("system", None, json!({"type": "message", "message": {"role": "system", "sections": {"prompt": "x"}}})),
        entry("user", Some("system"), json!({"type": "message", "message": {"role": "user", "content": null}})),
        entry("assistant", Some("user"), json!({"type": "message", "message": {"role": "assistant", "native": {"signature": "opaque"}}})),
        entry("tool", Some("assistant"), json!({"type": "message", "message": {"role": "toolResult"}})),
        entry("custom", Some("tool"), json!({"type": "custom_message", "customType": "null", "content": null, "display": false})),
    ]).unwrap();
    let messages = tree.project().unwrap().messages;
    assert_eq!(messages[0]["content"], "");
    for message in &messages[1..] {
        assert_eq!(message["content"], json!([]));
    }
    assert_eq!(messages[2]["native"]["signature"], "opaque");
}

#[test]
fn labels_and_names_resolve_globally_including_clear_operations() {
    let mut tree = SessionTree::from_entries(vec![
        message("root", None, "user", json!("a")),
        label("left", Some("root"), "root", json!("old")),
        label("right", Some("root"), "root", json!("new")),
        entry(
            "info",
            Some("right"),
            json!({"type": "session_info", "name": "  title  "}),
        ),
    ])
    .unwrap();
    tree.branch(Some("left")).unwrap();
    assert_eq!(tree.label("root"), Some("new"));
    assert_eq!(tree.name(), Some("title"));
    tree.append(label("clear", Some("left"), "root", json!("")))
        .unwrap();
    assert_eq!(tree.label("root"), None);
    tree.append(entry(
        "clear-name",
        Some("clear"),
        json!({"type": "session_info", "name": " \n "}),
    ))
    .unwrap();
    assert_eq!(tree.name(), None);
}

#[test]
fn extraction_rechains_labels_and_compaction_boundaries_and_copies_effective_sibling_labels() {
    let tree = SessionTree::from_entries(vec![
        message("root", None, "user", json!("root")),
        label("l1", Some("root"), "root", json!("old")),
        label("l2", Some("l1"), "root", json!("old again")),
        message("keep", Some("l2"), "user", json!("kept")),
        edit("edit", "keep", "keep", json!({"content": "edited"})),
        compact("compact", "edit", "l1", "summary"),
        label("sibling-label", Some("root"), "root", json!("latest")),
        label(
            "keep-label",
            Some("sibling-label"),
            "keep",
            json!("kept label"),
        ),
    ])
    .unwrap();
    let before = serde_json::to_value(&tree).unwrap();
    let fork = SessionTree::from_entries(tree.extract("compact").unwrap()).unwrap();
    assert_eq!(fork.get("keep").unwrap().parent_id.as_deref(), Some("root"));
    assert!(
        matches!(&fork.get("compact").unwrap().kind, EntryKind::Compaction { first_kept_entry_id, .. } if first_kept_entry_id == "keep")
    );
    assert_eq!(fork.label("root"), Some("latest"));
    assert_eq!(fork.label("keep"), Some("kept label"));
    assert_eq!(
        fork.project().unwrap().messages,
        tree.project_at_for_test("compact").messages
    );
    assert_eq!(fork.project().unwrap().messages[1]["content"], "edited");
    assert_eq!(serde_json::to_value(&tree).unwrap(), before);
    assert!(tree.extract("missing").is_err());
}

#[test]
fn extraction_keeps_retain_none_and_handles_label_only_paths_and_generated_id_collisions() {
    let tree = SessionTree::from_entries(vec![
        label("only-label", None, "missing", json!("orphan label")),
        message("root", None, "user", json!("root")),
        compact("compact", "root", "compact", "summary"),
        label("bookmark", Some("compact"), "root", json!("name")),
        message("bookmark-fork", None, "user", json!("collision")),
    ])
    .unwrap();
    assert!(tree.extract("only-label").unwrap().is_empty());
    let fork = SessionTree::from_entries(tree.extract("bookmark").unwrap()).unwrap();
    assert!(
        matches!(&fork.get("compact").unwrap().kind, EntryKind::Compaction { first_kept_entry_id, .. } if first_kept_entry_id == "compact")
    );
    assert_eq!(fork.entries().last().unwrap().id, "bookmark-fork-1");
    assert_eq!(fork.project().unwrap().messages.len(), 1);
}

impl SessionTree {
    fn project_at_for_test(&self, target: &str) -> SessionProjection {
        let mut copy = self.clone();
        copy.branch(Some(target)).unwrap();
        copy.project().unwrap()
    }
}
