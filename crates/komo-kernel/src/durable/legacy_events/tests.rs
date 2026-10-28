use serde_json::json;

use super::*;
use crate::durable::compaction::wrap_summary;
use crate::durable::legacy_ids::{frame_of, record_of};
use crate::durable::message::StopReason;
use crate::durable::records::ContextEdit;

const SESSION: &str = "s-1";

fn cid(n: u64) -> ConversationId {
    ConversationId::new(n)
}

fn eid(n: u64) -> EntryId {
    EntryId::new(n)
}

fn sid(n: u64) -> SubmissionId {
    SubmissionId::new(n)
}

fn entry(id: u64, kind: &str) -> EntryRecord {
    EntryRecord::new(eid(id), cid(1), kind)
}

fn user(id: u64, text: &str) -> EntryRecord {
    entry(id, "komo.user").with_model(vec![Message::user(text, id as i64 * 1000)])
}

fn assistant(id: u64, text: &str, calls: &[&str]) -> EntryRecord {
    let mut content = vec![ContentBlock::text(text)];
    content.extend(
        calls
            .iter()
            .map(|call| ContentBlock::tool_call(*call, "read", json!({"path": "a"}))),
    );
    let stop = if calls.is_empty() {
        StopReason::Stop
    } else {
        StopReason::ToolUse
    };
    entry(id, "komo.assistant").with_model(vec![Message::assistant(
        content,
        stop,
        id as i64 * 1000,
    )])
}

fn result(id: u64, call: &str, text: &str) -> EntryRecord {
    entry(id, "komo.tool-result")
        .with_model(vec![Message::tool_result(
            call,
            "read",
            text,
            id as i64 * 1000,
        )])
        .with_data(json!({"tool": "read", "status": "completed", "attempt": 1, "elapsedMs": 12}))
}

fn settled(id: u64, submission: u64, answer: Option<u64>) -> EntryRecord {
    let data = match answer {
        Some(answer) => json!({"submission": submission, "status": "done", "answer": answer}),
        None => json!({"submission": submission, "status": "unanswered", "reason": "aborted"}),
    };
    entry(id, "komo.settled").with_data(data)
}

fn input_sub(id: u64, state: InputState) -> SubmissionRecord {
    SubmissionRecord {
        id: sid(id),
        conversation: cid(1),
        request_id: Some(format!("req-{id}")),
        created_at: None,
        kind: SubmissionKind::Input { state },
    }
}

fn placed(entry: u64) -> InputState {
    InputState::Placed { entry: eid(entry) }
}

fn done(entry: u64, answer: u64) -> InputState {
    InputState::Done {
        entry: eid(entry),
        answer: eid(answer),
    }
}

/// `(run, type)`，看帧的大致形状。
fn shape(events: &[Event]) -> Vec<(String, &str)> {
    events
        .iter()
        .map(|event| {
            (
                event
                    .run
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |run| run.as_str().to_owned()),
                event.type_name(),
            )
        })
        .collect()
}

fn shape_of<'a>(frames: &[(&str, &'a str)]) -> Vec<(String, &'a str)> {
    frames
        .iter()
        .map(|(run, name)| ((*run).to_owned(), *name))
        .collect()
}

/// 一条 run 吸收了 steer、又有 followUp 在最终回答的同一提交里被放置。
///
/// | id | 记录 |
/// |---|---|
/// | 1 | S1 输入（放置在 2） |
/// | 2 | S1 的输入 entry |
/// | 3 | S2 steer（放置在 7） |
/// | 4 | S3 followUp（放置在 9） |
/// | 5, 6 | assistant 调用 c1，工具结果 |
/// | 7 | S2 的输入 entry（postTools） |
/// | 8 | 最终回答 |
/// | 9 | S3 的输入 entry（final boundary，先于结算） |
/// | 10, 11 | S1、S2 的 `komo.settled`（同一个 answer） |
/// | 12, 13 | S3 的回答与结算 |
struct Scenario {
    entries: Vec<EntryRecord>,
    /// 最终状态的 submission。
    submissions: Vec<SubmissionRecord>,
    /// 按提交分批：每批里 submission 是当时的状态。
    commits: Vec<(Vec<SubmissionRecord>, Vec<EntryRecord>)>,
}

fn scenario() -> Scenario {
    let entries = vec![
        user(2, "q1"),
        assistant(5, "looking", &["c1"]),
        result(6, "c1", "file text"),
        user(7, "also check b"),
        assistant(8, "final answer", &[]),
        user(9, "next question"),
        settled(10, 1, Some(8)),
        settled(11, 3, Some(8)),
        assistant(12, "answer to next", &[]),
        settled(13, 4, Some(12)),
    ];
    let pick = |ids: &[u64]| -> Vec<EntryRecord> {
        entries
            .iter()
            .filter(|entry| ids.contains(&entry.id.get()))
            .cloned()
            .collect()
    };
    let commits = vec![
        (vec![input_sub(1, placed(2))], pick(&[2])),
        (vec![input_sub(3, InputState::Queued)], vec![]),
        (vec![input_sub(4, InputState::Queued)], vec![]),
        (vec![], pick(&[5])),
        (vec![], pick(&[6])),
        (vec![input_sub(3, placed(7))], pick(&[7])),
        (
            vec![
                input_sub(1, done(2, 8)),
                input_sub(3, done(7, 8)),
                input_sub(4, placed(9)),
            ],
            pick(&[8, 9, 10, 11]),
        ),
        (vec![input_sub(4, done(9, 12))], pick(&[12, 13])),
    ];
    let submissions = vec![
        input_sub(1, done(2, 8)),
        input_sub(3, done(7, 8)),
        input_sub(4, done(9, 12)),
    ];
    Scenario {
        entries,
        submissions,
        commits,
    }
}

fn live(scenario: &Scenario) -> Vec<Event> {
    let mut projection = Projection::new(SESSION);
    let mut frames = Vec::new();
    for (submissions, entries) in &scenario.commits {
        let records: Vec<Record<'_>> = submissions
            .iter()
            .map(Record::Submission)
            .chain(entries.iter().map(Record::Entry))
            .collect();
        frames.extend(projection.project(&records));
    }
    frames
}

#[test]
fn legacy_events_backfill_equals_live_projection() {
    let scenario = scenario();
    let backfill = legacy_events(SESSION, &scenario.entries, &scenario.submissions);
    let live = live(&scenario);
    assert_eq!(live, backfill);
    assert!(!backfill.is_empty());
}

/// `run.accepted` / `run.queued` 的时间是 submission 的提交时刻（第一条之前没有别的记录可沿用，
/// 以前是 1970）；补读与直播逐帧一致。
#[test]
fn run_accepted_and_queued_carry_the_submission_time() {
    let mut first = input_sub(1, placed(2));
    first.created_at = Some(1_700_000_000_123);
    let entries = vec![user(2, "q1")];
    let backfill = legacy_events(SESSION, &entries, std::slice::from_ref(&first));
    let millis = |event: &Event| (event.ts.unix_timestamp_nanos() / 1_000_000) as i64;
    let stamped: Vec<(&str, i64)> = backfill
        .iter()
        .map(|event| (event.type_name(), millis(event)))
        .collect();
    assert_eq!(
        stamped,
        [
            ("run.accepted", 1_700_000_000_123),
            ("run.queued", 1_700_000_000_123),
            // 输入 entry 上那条用户消息的时间戳照旧。
            ("run.started", 2_000),
            ("message.user", 2_000),
        ]
    );

    // 直播：先喂排队的 submission（还没有输入 entry），再喂放置。
    let mut queued = first.clone();
    queued.kind = SubmissionKind::Input {
        state: InputState::Queued,
    };
    let mut projection = Projection::new(SESSION);
    let mut live = projection.project(&[Record::Submission(&queued)]);
    live.extend(projection.project(&[Record::Submission(&first), Record::Entry(&entries[0])]));
    assert_eq!(live, backfill);
}

#[test]
fn a_steer_projects_as_absorbed_into_the_current_run() {
    let scenario = scenario();
    let events = legacy_events(SESSION, &scenario.entries, &scenario.submissions);
    assert_eq!(
        shape(&events),
        shape_of(&[
            // S1：受理、排队、开跑、那句用户消息。
            ("1", "run.accepted"),
            ("1", "run.queued"),
            ("1", "run.started"),
            ("1", "message.user"),
            // S2、S3 排队（run 是各自的）。
            ("3", "run.accepted"),
            ("3", "run.queued"),
            ("4", "run.accepted"),
            ("4", "run.queued"),
            // 第 1 轮：调用 c1。
            ("1", "message.assistant"),
            ("1", "tool.planned"),
            ("1", "tool.started"),
            ("1", "tool.result"),
            // S2 并进 run 1：写在被并的那条上，那句话挂在宿主上。
            ("3", "run.absorbed"),
            ("1", "message.user"),
            ("1", "message.assistant"),
            // S3 在 final boundary 放置：开新 run，不并进 1。
            ("4", "run.started"),
            ("4", "message.user"),
            // 结算：只有 S1 出终态，S2（并进来的）不出；S3 的在它自己的 run。
            ("1", "run.completed"),
            ("4", "message.assistant"),
            ("4", "run.completed"),
        ])
    );
    let absorbed = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::RunAbsorbed(body) => Some(body.into.as_str().to_owned()),
            _ => None,
        })
        .unwrap();
    assert_eq!(absorbed, "1");
    // 终态带回答与轮数。
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::RunCompleted(body) => Some((
                event.run.as_ref().unwrap().as_str().to_owned(),
                body.final_message.clone(),
                body.rounds,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        completed,
        vec![
            ("1".to_owned(), Some("final answer".to_owned()), 2),
            ("4".to_owned(), Some("answer to next".to_owned()), 1),
        ]
    );
    // 第 2 轮的 round 号是 2，工具调用帧带着 provider 的 id。
    let rounds: Vec<u32> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::MessageAssistant(body) => Some(body.round),
            _ => None,
        })
        .collect();
    assert_eq!(rounds, [1, 2, 1]);
}

#[test]
fn seq_is_strictly_increasing_per_session() {
    let scenario = scenario();
    let events = legacy_events(SESSION, &scenario.entries, &scenario.submissions);
    assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    // Seq 里是记录 ID 与帧序号：assistant 的计划与开始帧紧跟在它那条消息之后。
    let frames_of_five: Vec<_> = events
        .iter()
        .filter(|event| record_of(event.seq) == 5)
        .map(|event| (frame_of(event.seq), event.type_name()))
        .collect();
    assert_eq!(
        frames_of_five,
        [
            (0, "message.assistant"),
            (1, "tool.planned"),
            (2, "tool.started")
        ]
    );

    // 两个 conversation（fork）交错的 entry 也严格递增，打乱喂入次序结果不变。
    let mut entries = vec![
        user(2, "q"),
        assistant(3, "a", &[]),
        EntryRecord::new(eid(6), cid(2), "komo.user")
            .with_model(vec![Message::user("branch", 6000)]),
        assistant(7, "b", &[]),
        user(8, "back on the root"),
    ];
    let subs = vec![
        input_sub(1, done(2, 3)),
        SubmissionRecord {
            conversation: cid(2),
            ..input_sub(5, placed(6))
        },
    ];
    let forward = legacy_events(SESSION, &entries, &subs);
    entries.reverse();
    let shuffled = legacy_events(SESSION, &entries, &subs);
    assert_eq!(forward, shuffled);
    assert!(forward.windows(2).all(|pair| pair[0].seq < pair[1].seq));
}

#[test]
fn a_withdrawn_queued_input_projects_as_cancelled() {
    // S20 排队后被撤回：从没放置过，只有 submission 和 settled 锚点。
    let submissions = vec![input_sub(
        20,
        InputState::Unanswered {
            entry: None,
            reason: "aborted".into(),
            detail: None,
        },
    )];
    let entries = vec![settled(21, 20, None)];
    let events = legacy_events(SESSION, &entries, &submissions);
    assert_eq!(
        shape(&events),
        shape_of(&[
            ("20", "run.accepted"),
            ("20", "run.queued"),
            ("20", "run.cancelled"),
        ])
    );
    assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
}

#[test]
fn failures_keep_their_reason_and_detail() {
    let submissions = vec![input_sub(
        1,
        InputState::Unanswered {
            entry: Some(eid(2)),
            reason: "model_error".into(),
            detail: Some(json!("boom")),
        },
    )];
    let failed = entry(4, "komo.settled").with_data(json!({
        "submission": 1, "status": "unanswered", "reason": "model_error", "detail": "boom",
    }));
    let error_reply = entry(3, "komo.assistant").with_model(vec![Message::assistant(
        vec![],
        StopReason::Error,
        3000,
    )]);
    let events = legacy_events(SESSION, &[user(2, "q"), error_reply, failed], &submissions);
    // 失败的空回复不出 assistant 帧，终态说明原因。
    assert_eq!(
        shape(&events),
        shape_of(&[
            ("1", "run.accepted"),
            ("1", "run.queued"),
            ("1", "run.started"),
            ("1", "message.user"),
            ("1", "run.failed"),
        ])
    );
    let EventPayload::RunFailed(body) = &events.last().unwrap().payload else {
        panic!()
    };
    assert_eq!(body.reason, "model_error: boom");
}

#[test]
fn an_abnormal_end_settles_before_the_successor_and_each_run_has_one_terminal() {
    // 取消在工具之间：[E1][A 调用][T 中止][X1 aborted][E2 后继][A][X2]。
    let submissions = vec![
        input_sub(
            1,
            InputState::Unanswered {
                entry: Some(eid(3)),
                reason: "aborted".into(),
                detail: None,
            },
        ),
        input_sub(2, done(7, 8)),
    ];
    let entries = vec![
        user(3, "q"),
        assistant(4, "calling", &["c1"]),
        result(5, "c1", "aborted"),
        settled(6, 1, None),
        user(7, "queued message"),
        assistant(8, "reply", &[]),
        settled(9, 2, Some(8)),
    ];
    let events = legacy_events(SESSION, &entries, &submissions);
    let terminals: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::RunCompleted(_)
                    | EventPayload::RunFailed(_)
                    | EventPayload::RunCancelled(_)
            )
        })
        .map(|event| {
            (
                event.run.as_ref().unwrap().as_str().to_owned(),
                event.type_name(),
            )
        })
        .collect();
    assert_eq!(
        terminals,
        [
            ("1".to_owned(), "run.cancelled"),
            ("2".to_owned(), "run.completed")
        ]
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::RunStarted(_))
                && event.run.as_ref().unwrap().as_str() == "2")
    );
}

#[test]
fn a_reset_in_a_run_closes_it_so_the_following_input_starts_a_new_run() {
    // [E1][A 调用][T][R reset][E2 reset 之后放置][X1 reset][A][X2]
    let entries = vec![
        user(3, "q"),
        assistant(4, "calling", &["c1"]),
        result(5, "c1", "ok"),
        entry(6, "komo.reset").with_head(eid(6)),
        user(7, "after the reset"),
        entry(8, "komo.settled")
            .with_data(json!({"submission": 1, "status": "unanswered", "reason": "reset"})),
        assistant(9, "reply", &[]),
        settled(10, 2, Some(9)),
    ];
    let subs = vec![
        input_sub(
            1,
            InputState::Unanswered {
                entry: Some(eid(3)),
                reason: "reset".into(),
                detail: None,
            },
        ),
        input_sub(2, done(7, 9)),
    ];
    let events = legacy_events(SESSION, &entries, &subs);
    let names = shape(&events);
    assert!(names.contains(&("-".to_owned(), "conversation.boundary")));
    let started: Vec<&str> = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::RunStarted(_)))
        .map(|event| event.run.as_ref().unwrap().as_str())
        .collect();
    assert_eq!(started, ["1", "2"]);
    // reset 收场的 run 是取消，不是失败。
    assert!(names.contains(&("1".to_owned(), "run.cancelled")));
}

/// 记忆召回块（§9.4）投影成挂在所属 run 上的 `memory.injected`：只带 uses，正文不出
/// `message.user`——提取窗口里看不到它，也就引不成用户原话。改写它的 `komo.memory.revoke` 不出帧。
#[test]
fn a_memory_recall_block_is_an_injection_on_its_run_not_a_user_message() {
    use crate::durable::message::HOST_ORIGIN_KEY;
    use crate::durable::recall::{MEMORY_RECALL_ENTRY_KIND, MEMORY_REVOKE_ENTRY_KIND};

    let mut block = Message::user("[已知记忆]\n- 发布工单默认 saas 〔· m-1@2〕", 3000);
    if let Message::User(user) = &mut block {
        user.extra.insert(HOST_ORIGIN_KEY.into(), json!("memory"));
    }
    let entries = vec![
        user(2, "提个发布工单"),
        entry(3, MEMORY_RECALL_ENTRY_KIND)
            .with_model(vec![block])
            .with_data(
                json!({"submission": 1, "uses": [{"memory": "m-1", "revision": 2}], "at": 3}),
            ),
        entry(4, MEMORY_REVOKE_ENTRY_KIND).with_edits(vec![ContextEdit::omit(eid(3))]),
        assistant(5, "好", &[]),
        settled(6, 1, Some(5)),
    ];
    let subs = vec![input_sub(1, done(2, 5))];
    let events = legacy_events(SESSION, &entries, &subs);
    let users: Vec<&str> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::MessageUser(body) => body.text.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(users, ["提个发布工单"], "召回块的正文不是用户消息");
    let injected: Vec<&Event> = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::MemoryInjected(_)))
        .collect();
    assert_eq!(injected.len(), 1);
    assert_eq!(injected[0].run.as_ref().map(RunId::as_str), Some("1"));
    let EventPayload::MemoryInjected(body) = &injected[0].payload else {
        panic!()
    };
    assert_eq!(body.uses.len(), 1);
    assert_eq!(body.uses[0].revision, 2);
    assert!(body.text.is_none());
}

#[test]
fn context_changes_and_plain_user_messages_have_their_own_frames() {
    let entries = vec![
        entry(2, "komo.compaction")
            .with_head(eid(1))
            .with_model(vec![Message::user(wrap_summary("sum"), 2000)])
            .with_data(json!({"reason": "manual"})),
        entry(3, "komo.decay").with_edits(vec![ContextEdit::replace(
            eid(1),
            vec![Message::tool_result("c9", "read", "short", 1)],
        )]),
        entry(4, "model.change").with_data(json!({"provider": "p", "modelId": "m-2"})),
        // `!` 命令：write submission 放下的 entry，不开 run。
        entry(5, "shell.command")
            .with_model(vec![Message::user("!ls", 5000)])
            .with_data(json!({"command": "ls"})),
        // 展示 / 辅助 entry 与不认识的 kind 不出帧。
        entry(6, "label").with_data(json!({"targetId": 1, "label": "x"})),
        entry(7, "komo.system"),
        entry(8, "session.info").with_data(json!({"name": "n"})),
        entry(9, "branch.summary").with_model(vec![Message::user("sum", 9000)]),
        entry(10, "weird.kind").with_data(json!({"a": 1})),
    ];
    let events = legacy_events(SESSION, &entries, &[]);
    assert_eq!(
        shape(&events),
        shape_of(&[
            ("-", "context.compacted"),
            ("-", "context.compacted"),
            ("-", "config.changed"),
            ("-", "message.user"),
        ])
    );
    let EventPayload::ContextCompacted(ContextCompacted::Compacted {
        first_kept,
        summary,
        ..
    }) = &events[0].payload
    else {
        panic!()
    };
    assert_eq!(record_of(*first_kept), 1);
    assert_eq!(summary.as_deref(), Some("sum"));
    let EventPayload::ContextCompacted(ContextCompacted::Decayed { calls, .. }) =
        &events[1].payload
    else {
        panic!()
    };
    assert_eq!(calls[0].as_str(), "c9");
    let EventPayload::ConfigChanged(body) = &events[2].payload else {
        panic!()
    };
    assert_eq!(body.model.as_deref(), Some("m-2"));
}

#[test]
fn replayed_records_produce_no_second_frames() {
    let scenario = scenario();
    let mut projection = Projection::new(SESSION);
    let records: Vec<Record<'_>> = scenario
        .submissions
        .iter()
        .map(Record::Submission)
        .chain(scenario.entries.iter().map(Record::Entry))
        .collect();
    let first = projection.project(&records);
    assert!(!first.is_empty());
    assert!(projection.project(&records).is_empty());
}

#[test]
fn tool_results_carry_status_output_and_preview() {
    let output = json!({"path": "tool-output/2/9/1/output.json", "size": 4, "hash": "ab"});
    let entries = vec![
        assistant(2, "go", &["c1"]),
        entry(3, "komo.tool-result")
            .with_model(vec![Message::tool_result("c1", "bash", "boom", 3000)])
            .with_data(json!({
                "tool": "bash", "status": "failed", "attempt": 2, "elapsedMs": 40, "output": output,
            })),
    ];
    let events = legacy_events(SESSION, &entries, &[]);
    let EventPayload::ToolResult(body) = &events.last().unwrap().payload else {
        panic!()
    };
    assert_eq!(body.status, ToolResultStatus::Failed);
    assert_eq!(body.attempt_id.as_str(), "attempt-2");
    assert_eq!(body.elapsed_ms, 40);
    assert_eq!(body.output_ref.path(), "tool-output/2/9/1/output.json");
    assert_eq!(body.preview.as_deref(), Some("boom"));
    assert_eq!(body.call_id.as_str(), "c1");
}

#[test]
fn codemode_results_carry_the_nested_calls_live_and_backfilled_alike() {
    let output = json!({"path": "tool-output/2/m1/1/output.json", "size": 4, "hash": "ab"});
    let codemode = entry(3, "komo.tool-result")
        .with_model(vec![Message::tool_result("m1", "codemode", "done", 3000)])
        .with_data(json!({
            "tool": "codemode", "status": "completed", "attempt": 1, "elapsedMs": 9, "output": output,
        }));
    let entries = vec![assistant(2, "go", &["m1"]), codemode];
    assert_eq!(
        wants_result(&entries[1]).map(|output| output.path().to_owned()),
        Some("tool-output/2/m1/1/output.json".to_owned())
    );
    assert!(
        wants_result(&result(4, "c1", "x")).is_none(),
        "别的工具不读"
    );
    // output.json 里的原样：大参数、运行时字段，还有模型看不到的脚本变量。
    let raw = json!({
        "calls": [{ "id": "c1", "tool": "write", "args": "{}", "ok": true, "status": "completed",
                    "arguments": { "path": "a", "content": "x".repeat(5000) }, "elapsed_ms": 4 }],
        "error": null,
        "value": { "secret": 1 },
    });
    let results = HashMap::from([(eid(3), raw.clone())]);
    let backfill = legacy_events_with(SESSION, &entries, &[], results);

    let mut projection = Projection::new(SESSION);
    let mut live = projection.project(&[Record::Entry(&entries[0])]);
    projection.attach_result(eid(3), raw);
    live.extend(projection.project(&[Record::Entry(&entries[1])]));
    assert_eq!(live, backfill);

    let EventPayload::ToolResult(body) = &backfill.last().unwrap().payload else {
        panic!()
    };
    let shown = &body.body.as_ref().unwrap().result;
    assert!(shown.get("value").is_none(), "{shown}");
    let calls = crate::protocol::nested::calls_of(shown).unwrap();
    assert_eq!(calls[0].tool, "write");
    assert_eq!(calls[0].args["path"], "a");
    assert!(calls[0].args["content"].as_str().unwrap().len() < 1000);
    assert_eq!(calls[0].elapsed_ms, Some(4));
    // 没读到：照旧是 Null。
    let plain = legacy_events(SESSION, &entries, &[]);
    let EventPayload::ToolResult(body) = &plain.last().unwrap().payload else {
        panic!()
    };
    assert!(body.body.as_ref().unwrap().result.is_null());
}

/// 受理时补在正文后面的项目规范只给模型：消息面上只剩用户原话，另带一个标记。
#[test]
fn a_project_update_appended_to_an_input_is_cut_back_to_the_spoken_text() {
    let spoken = "再说一句";
    let mut input = user(
        2,
        &format!("{spoken}\n\n（项目规范在本会话中改过）\n<project_instructions>…"),
    );
    input = input.with_data(json!({"inputHash": "h", PROJECT_UPDATE_AT: spoken.len()}));
    let events = legacy_events(SESSION, &[input], &[input_sub(1, placed(2))]);
    let message = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::MessageUser(body) => Some(body.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(message.text.as_deref(), Some(spoken));
    assert!(message.project_update);

    // 没有这一格的输入照旧全文、不带标记。
    let events = legacy_events(SESSION, &[user(2, "q1")], &[input_sub(1, placed(2))]);
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::MessageUser(body) if body.text.as_deref() == Some("q1") && !body.project_update
    )));
}
