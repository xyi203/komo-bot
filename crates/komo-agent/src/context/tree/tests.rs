use super::*;

#[test]
fn raw_tools_preserve_identity_arrays_metadata_and_images() {
    let assistant = json!({"role":"assistant", "content":[{"type":"toolCall", "id":"pc", "name":"read", "arguments":{"path":"a"}}], "opaque":{"signature":"keep"}});
    let call = convert(&assistant, Seq(1), &BTreeMap::new()).unwrap();
    let result = convert(&json!({"role":"toolResult", "toolCallId":"pc", "content":[{"type":"text","text":"ok"},{"type":"image","mimeType":"image/png","data":"AQID"}]}), Seq(2), &BTreeMap::new()).unwrap();
    assert_eq!(call.tool_calls[0].call_id, result.tool_results[0].call_id);
    assert_eq!(call.tool_calls[0].provider_call_id, "pc");
    assert_eq!(call.provider_blocks.as_ref().unwrap()["komo_pi"], assistant);
    assert_eq!(result.tool_results[0].images[0].bytes, vec![1, 2, 3]);
}

#[test]
fn edited_or_interrupted_protocol_never_leaves_orphans() {
    let mut messages = vec![convert(&json!({"role":"assistant","content":[{"type":"toolCall","id":"missing","name":"bash","arguments":{}}]}), Seq(1), &BTreeMap::new()).unwrap(), plain(Role::User, Seq(2), "steering".into()), convert(&json!({"role":"toolResult","toolCallId":"removed","content":"orphan"}), Seq(3), &BTreeMap::new()).unwrap()];
    repair(&mut messages);
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1].tool_results[0].provider_call_id, "missing");
    assert!(messages[1].tool_results[0].is_error);
    assert_eq!(messages[2].text.as_deref(), Some("steering"));
}

fn event(seq: u64, run: Option<&str>, parent: Option<&str>, id: &str, kind: Value) -> Event {
    use komo_kernel::traits::Clock;
    let mut value = kind;
    value["id"] = json!(id);
    value["parentId"] = json!(parent);
    value["timestamp"] = json!("2026-10-23T00:00:00Z");
    Event {
        entry: Some(Box::new(serde_json::from_value(value).unwrap())),
        v: 1,
        seq: Seq(seq),
        event_id: komo_kernel::types::ids::EventId::from_raw(format!("event-{seq}")),
        session: komo_kernel::types::ids::SessionId::from_raw("session"),
        run: run.map(RunId::from_raw),
        ts: komo_kernel::test_support::TestClock::fixed().now(),
        payload: EventPayload::Unknown {
            event_type: "session.entry".into(),
            raw: Value::Null,
        },
    }
}
fn message(
    seq: u64,
    run: Option<&str>,
    parent: Option<&str>,
    id: &str,
    role: &str,
    content: Value,
) -> Event {
    event(
        seq,
        run,
        parent,
        id,
        json!({"type":"message", "message":{"role":role,"content":content}}),
    )
}
fn replay(
    events: &[Event],
    run: Option<&str>,
    thread: Option<&[RunId]>,
) -> (AgentContext, Vec<ToolDefinition>) {
    let run = run.map(RunId::from_raw);
    let selected = select(events, run.as_ref(), thread).unwrap();
    let resolved = selected
        .entries()
        .into_iter()
        .map(|entry| ResolvedMessage {
            text: entry.message.text.clone(),
            outputs: entry.message.tool_results.iter().map(|_| None).collect(),
            entry,
        })
        .collect();
    let mut context = AgentContext {
        system_prompt: "template".into(),
        messages: vec![],
        run_from: 17,
        decay_candidates: vec![],
    };
    let mut tools = vec![];
    selected.apply(
        &mut context,
        resolved,
        &ProjectionContext {
            model_result_bytes: 8192,
            decay: None,
        },
        &mut tools,
    );
    (context, tools)
}

#[test]
fn run_head_excludes_siblings_and_ui_leaf_but_keeps_previous_runs_protocol() {
    let mut result = message(
        3,
        Some("old"),
        Some("call"),
        "result",
        "toolResult",
        json!([{"type":"text", "text":"previous output"}]),
    );
    if let EntryKind::Message { message } = &mut result.entry.as_mut().unwrap().kind {
        message["toolCallId"] = json!("pc");
    }
    let events = vec![
        message(1, Some("old"), None, "root", "user", json!("original")),
        message(
            2,
            Some("old"),
            Some("root"),
            "call",
            "assistant",
            json!([{"type":"toolCall","id":"pc","name":"read","arguments":{}}]),
        ),
        result,
        message(
            4,
            Some("new"),
            Some("result"),
            "new",
            "user",
            json!("follow up"),
        ),
        message(
            5,
            Some("sibling"),
            Some("root"),
            "sibling",
            "user",
            json!("wrong branch"),
        ),
    ];
    let (context, _) = replay(&events, Some("new"), None);
    assert_eq!(context.messages.len(), 4);
    assert_eq!(context.messages[1].tool_calls[0].provider_call_id, "pc");
    assert_eq!(
        context.messages[2].tool_results[0].content,
        "previous output"
    );
    assert_eq!(context.messages[3].text.as_deref(), Some("follow up"));
    assert_eq!(context.run_from, 0);
}

#[test]
fn latest_branch_compaction_replays_system_checkpoint_and_later_tool_patches() {
    let events = vec![
        message(1, None, None, "old", "user", json!("discard")),
        message(
            2,
            None,
            Some("old"),
            "kept",
            "assistant",
            json!([{"type":"text","text":"retain"}]),
        ),
        event(
            3,
            None,
            Some("kept"),
            "compact",
            json!({"type":"compaction","summary":"summary","firstKeptEntryId":"kept","tokensBefore":100,"systemMessage":{"role":"system","content":"checkpoint","tools":[{"name":"read","parameters":{"type":"object"}}]}}),
        ),
        event(
            4,
            None,
            Some("compact"),
            "patch",
            json!({"type":"message","message":{"role":"system","toolsRemoved":["read"],"toolsAdded":[{"name":"write","parameters":{"type":"object"}}]}}),
        ),
        message(5, None, Some("patch"), "next", "user", json!("next")),
    ];
    let (context, tools) = replay(&events, None, None);
    assert_eq!(context.system_prompt, "checkpoint");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["write"]
    );
    assert_eq!(context.messages.len(), 3);
    assert!(
        context.messages[0]
            .text
            .as_ref()
            .unwrap()
            .contains("summary")
    );
    assert_eq!(context.messages[1].text.as_deref(), Some("retain"));
    assert!(
        context
            .messages
            .iter()
            .all(|message| message.text.as_deref() != Some("discard"))
    );
}

#[test]
fn edits_replace_content_omit_messages_and_include_branch_and_custom_summaries() {
    let events = vec![
        message(1, None, None, "root", "user", json!("old")),
        message(
            2,
            None,
            Some("root"),
            "call",
            "assistant",
            json!([{"type":"toolCall","id":"pc","name":"read","arguments":{}}]),
        ),
        event(
            3,
            None,
            Some("call"),
            "replace",
            json!({"type":"context_edit","targetId":"root","replacement":{"content":[{"type":"text","text":"new"}]}}),
        ),
        event(
            4,
            None,
            Some("replace"),
            "omit",
            json!({"type":"context_edit","targetId":"call","replacement":null}),
        ),
        event(
            5,
            None,
            Some("omit"),
            "branch",
            json!({"type":"branch_summary","fromId":"other","summary":"branch work"}),
        ),
        event(
            6,
            None,
            Some("branch"),
            "custom",
            json!({"type":"custom_message","customType":"memory","display":false,"content":"saved memory","details":{"opaque":7}}),
        ),
    ];
    let (context, _) = replay(&events, None, None);
    assert_eq!(
        context
            .messages
            .iter()
            .map(|message| message.text.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["new", "branch work", "saved memory"]
    );
    assert_eq!(
        context.messages[2].provider_blocks.as_ref().unwrap()["komo_pi"]["details"]["opaque"],
        7
    );
    assert!(
        context
            .messages
            .iter()
            .all(|message| message.tool_calls.is_empty())
    );
}

#[test]
fn delegate_thread_retains_its_own_historical_protocol_without_parent_messages() {
    let events = vec![
        message(
            1,
            Some("parent"),
            None,
            "parent",
            "user",
            json!("parent secret"),
        ),
        message(
            2,
            Some("child1"),
            Some("parent"),
            "task",
            "user",
            json!("task"),
        ),
        message(
            3,
            Some("child1"),
            Some("task"),
            "work",
            "assistant",
            json!([{"type":"text","text":"previous work"}]),
        ),
        message(
            4,
            Some("child2"),
            Some("work"),
            "resume",
            "user",
            json!("continue"),
        ),
    ];
    let (context, _) = replay(
        &events,
        Some("child2"),
        Some(&[RunId::from_raw("child1"), RunId::from_raw("child2")]),
    );
    assert_eq!(
        context
            .messages
            .iter()
            .filter_map(|message| message.text.as_deref())
            .collect::<Vec<_>>(),
        ["task", "previous work", "continue"]
    );
}

#[test]
fn production_tool_projection_keeps_provider_native_blocks_and_images() {
    use komo_kernel::events::MessageAssistant;
    use komo_kernel::types::refs::ToolResultStatus;
    let mut call = message(
        1,
        Some("old"),
        None,
        "call",
        "assistant",
        json!([{"type":"toolCall","id":"provider-id","name":"read","arguments":{}}]),
    );
    call.payload = EventPayload::MessageAssistant(MessageAssistant {
        round: 1,
        text: None,
        text_ref: None,
        tool_calls: vec![ToolCallRequest {
            call_id: ToolCallId::from_raw("internal"),
            provider_call_id: "provider-id".into(),
            name: "read".into(),
            arguments: json!({}),
            arguments_ref: None,
        }],
        provider_blocks: Some(json!([{"type":"thinking","signature":"native"}])),
        input_tokens: None,
        output_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
    });
    // Reuse a real tool result fixture shape through serde to avoid duplicating
    // output reference constructors; the selected source is still an execution Event.
    let mut result = message(
        2,
        Some("old"),
        Some("call"),
        "result",
        "toolResult",
        json!([{"type":"text","text":"raw preview"}]),
    );
    result.payload = EventPayload::ToolResult(komo_kernel::events::ToolResult {
        body: None,
        call_id: ToolCallId::from_raw("internal"),
        attempt_id: komo_kernel::types::ids::AttemptId::from_raw("attempt"),
        status: ToolResultStatus::Completed,
        output_ref: komo_kernel::types::refs::OutputRef(komo_kernel::types::refs::ContentRef {
            path: "tool-output/old/internal/attempt/output.json".into(),
            size: 0,
            hash: komo_kernel::types::digest::ContentHash::of_str(""),
            pointer: None,
        }),
        stdout: None,
        stderr: None,
        elapsed_ms: 3,
        preview: Some("raw preview".into()),
        attempt_state: None,
    });
    let next = message(
        3,
        Some("new"),
        Some("result"),
        "next",
        "user",
        json!("continue"),
    );
    let events = [call, result, next];
    let selected = select(&events, Some(&RunId::from_raw("new")), None).unwrap();
    let resolved = selected
        .entries()
        .into_iter()
        .map(|entry| ResolvedMessage {
            text: entry.message.text.clone(),
            outputs: entry
                .message
                .tool_results
                .iter()
                .map(|_| {
                    Some(history::StoredOutput {
                        preview: Some("full projected output".into()),
                        artifacts: vec![],
                        images: vec![],
                        image_bytes: vec![ToolImage {
                            mime: "image/png".into(),
                            bytes: vec![1, 2, 3],
                        }],
                    })
                })
                .collect(),
            entry,
        })
        .collect();
    let mut context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        run_from: 0,
        decay_candidates: vec![],
    };
    selected.apply(
        &mut context,
        resolved,
        &ProjectionContext {
            model_result_bytes: 8192,
            decay: None,
        },
        &mut vec![],
    );
    assert_eq!(
        context.messages[0].provider_blocks.as_ref().unwrap()[0]["signature"],
        "native"
    );
    let result = &context.messages[1].tool_results[0];
    assert_eq!(result.provider_call_id, "provider-id");
    assert_eq!(result.call_id.as_str(), "internal");
    assert!(result.content.contains("full projected output"));
    let EventPayload::ToolResult(event) = &events[1].payload else {
        panic!("tool result")
    };
    let expected = komo_kernel::projection::project(
        &komo_kernel::projection::ToolResultFacts {
            tool: "read",
            status: event.status,
            elapsed_ms: 3,
            text: Some("full projected output"),
            output: &event.output_ref,
            stdout: None,
            stderr: None,
            artifacts: &[],
            images: &[],
        },
        &ProjectionContext {
            model_result_bytes: 8192,
            decay: None,
        },
    );
    assert_eq!(
        result.content, expected,
        "fresh executor and tree replay use the identical projection"
    );
    assert_eq!(result.images[0].bytes, vec![1, 2, 3]);
}

#[test]
fn latest_adjudicated_tool_result_supersedes_uncertain_result() {
    let calls = BTreeMap::new();
    let mut messages = vec![
        convert(&json!({"role":"assistant","content":[{"type":"toolCall","id":"pc","name":"bash","arguments":{}}]}), Seq(1), &calls).unwrap(),
        convert(&json!({"role":"toolResult","toolCallId":"pc","isError":true,"content":"uncertain"}), Seq(2), &calls).unwrap(),
        convert(&json!({"role":"toolResult","toolCallId":"pc","content":"verified"}), Seq(3), &calls).unwrap(),
    ];
    repair(&mut messages);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].tool_results[0].content, "verified");
    assert!(!messages[1].tool_results[0].is_error);
}

#[test]
fn live_recovery_calls_remain_open_until_executor_supplies_results() {
    let calls = BTreeMap::new();
    let mut messages = vec![convert(&json!({"role":"assistant","content":[{"type":"toolCall","id":"pc","name":"bash","arguments":{}}]}), Seq(1), &calls).unwrap()];
    let pending = BTreeSet::from([messages[0].tool_calls[0].call_id.clone()]);
    repair_pending(&mut messages, &pending);
    assert_eq!(
        messages.len(),
        1,
        "executor will supply the real result before provider next()"
    );
    repair(&mut messages);
    assert_eq!(
        messages.len(),
        2,
        "a historical cancelled call must have a model-only result"
    );
}
