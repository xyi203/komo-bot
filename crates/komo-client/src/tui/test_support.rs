//! 测试用的固定样本。只在 `cfg(test)` 下存在，不进任何正式构建。

use komo_kernel::events::{
    EVENT_FORMAT_VERSION, Event, EventPayload, MessageAssistant, RunAbsorbed, RunAccepted,
    RunCompleted, RunQueued, RunStarted, ToolPlanned, ToolResult, ToolStarted,
};
use komo_kernel::protocol::http::{EventPage, InterventionKind, InterventionSummary};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{
    AttemptId, EventId, ExecutorId, OperationId, RequestKey, RunId, Seq, SessionId, ToolCallId,
};
use komo_kernel::types::model::{Effort, EffortSetting};
use komo_kernel::types::plan::{
    ExecutionPlan, Operation, PlanSource, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
};
use komo_kernel::types::refs::{ContentRef, OutputRef, ToolResultStatus};
use komo_kernel::types::turn::ToolCallRequest;
use time::OffsetDateTime;
use time::macros::datetime;

pub const T0: OffsetDateTime = datetime!(2026-09-16 08:00:00 UTC);

pub fn session() -> SessionId {
    SessionId::from_raw("sess-1")
}

pub fn run() -> RunId {
    RunId::from_raw("run-1")
}

pub fn call() -> ToolCallId {
    ToolCallId::from_raw("call-7")
}

pub fn event(seq: u64, run: Option<RunId>, payload: EventPayload) -> Event {
    Event {
        v: EVENT_FORMAT_VERSION,
        seq: Seq(seq),
        event_id: EventId::from_raw(format!("evt-{seq}")),
        session: session(),
        run,
        ts: T0 + time::Duration::seconds(seq as i64),
        payload,
        entry: None,
    }
}

/// 一段最小但完整的会话：提问 → 排队 → 开跑 → 一次工具调用 → 回答。
pub fn conversation() -> Vec<Event> {
    vec![
        event(
            1,
            Some(run()),
            EventPayload::RunAccepted(RunAccepted {
                request_key: RequestKey::new("tui:1"),
                input_hash: ContentHash::of_str("把 build 目录清掉"),
                text: Some("把 build 目录清掉".into()),
                text_ref: None,
                source: PlanSource::Interactive { session: session() },
                peer: None,
                model: Some("chat-a".into()),
                effort: Some(EffortSetting::Explicit(Effort::new("high"))),
                delegate: None,
                snapshot: None,
                interject: false,
            }),
        ),
        event(
            2,
            Some(run()),
            EventPayload::RunQueued(RunQueued {
                input_ref: EventId::from_raw("evt-1"),
            }),
        ),
        event(
            3,
            Some(run()),
            EventPayload::RunStarted(RunStarted {
                executor: ExecutorId::from_raw("exec-1"),
                generation: 1,
            }),
        ),
        event(
            4,
            Some(run()),
            EventPayload::MessageAssistant(MessageAssistant {
                round: 1,
                text: Some("我来清一下。".into()),
                text_ref: None,
                tool_calls: vec![ToolCallRequest {
                    call_id: call(),
                    provider_call_id: "pc-1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({ "command": "rm -rf build" }),
                    arguments_ref: None,
                }],
                provider_blocks: None,
                input_tokens: Some(120),
                output_tokens: Some(30),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            }),
        ),
        event(
            5,
            Some(run()),
            EventPayload::ToolPlanned(ToolPlanned {
                call_id: call(),
                plan_hash: plan().plan_hash(),
                plan: Some(Box::new(plan())),
                plan_ref: None,
            }),
        ),
        event(
            6,
            Some(run()),
            EventPayload::ToolStarted(ToolStarted {
                call_id: call(),
                attempt_id: AttemptId::from_raw("attempt-1"),
                plan_ref: EventId::from_raw("evt-5"),
                plan_hash: plan().plan_hash(),
            }),
        ),
        event(
            7,
            Some(run()),
            EventPayload::ToolResult(ToolResult {
                body: None,
                call_id: call(),
                attempt_id: AttemptId::from_raw("attempt-1"),
                status: ToolResultStatus::Completed,
                output_ref: OutputRef(ContentRef {
                    path: "tool-output/run-1/call-7/attempt-1/output.json".into(),
                    size: 12,
                    hash: ContentHash::of_str("x"),
                    pointer: None,
                }),
                elapsed_ms: 340,
                preview: Some("removed 12 files".into()),
                stdout: None,
                stderr: None,
                attempt_state: None,
            }),
        ),
        event(
            8,
            Some(run()),
            EventPayload::MessageAssistant(MessageAssistant {
                round: 2,
                text: Some("清完了，删掉 12 个文件。".into()),
                text_ref: None,
                tool_calls: vec![],
                provider_blocks: None,
                input_tokens: Some(200),
                output_tokens: Some(20),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            }),
        ),
        event(
            9,
            Some(run()),
            EventPayload::RunCompleted(RunCompleted {
                final_message: Some("清完了，删掉 12 个文件。".into()),
                final_message_ref: None,
                rounds: 2,
            }),
        ),
    ]
}

/// 插话（§6）：`conversation()` 跑到工具结果那一刻，用户又打了一句（`run-2`，要求插话），
/// 宿主在下一次请求之前把它并了进来，接着回答。
pub fn interjected() -> Vec<Event> {
    let late = RunId::from_raw("run-2");
    let mut events: Vec<Event> = conversation().into_iter().take(7).collect();
    events.extend([
        event(
            8,
            Some(late.clone()),
            EventPayload::RunAccepted(RunAccepted {
                request_key: RequestKey::new("tui:2"),
                input_hash: ContentHash::of_str("顺便看下 dist"),
                text: Some("顺便看下 dist".into()),
                text_ref: None,
                source: PlanSource::Interactive { session: session() },
                peer: None,
                model: Some("chat-a".into()),
                effort: Some(EffortSetting::Explicit(Effort::new("high"))),
                delegate: None,
                snapshot: None,
                interject: true,
            }),
        ),
        event(
            9,
            Some(late.clone()),
            EventPayload::RunQueued(RunQueued {
                input_ref: EventId::from_raw("evt-8"),
            }),
        ),
        event(
            10,
            Some(late),
            EventPayload::RunAbsorbed(RunAbsorbed { into: run() }),
        ),
        event(
            11,
            Some(run()),
            EventPayload::MessageAssistant(MessageAssistant {
                round: 2,
                text: Some("build 清完了，dist 是空的。".into()),
                text_ref: None,
                tool_calls: vec![],
                provider_blocks: None,
                input_tokens: Some(220),
                output_tokens: Some(20),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            }),
        ),
        event(
            12,
            Some(run()),
            EventPayload::RunCompleted(RunCompleted {
                final_message: Some("build 清完了，dist 是空的。".into()),
                final_message_ref: None,
                rounds: 2,
            }),
        ),
    ]);
    events
}

/// 一条 Run 东翻西找：第一轮并发读两个文件、搜一次，第二轮跑一条命令，第三轮再读一个，
/// 第四轮回答。除了最后那轮，助手消息都只有调用、没有正文。
pub fn explored() -> Vec<Event> {
    type Call = (&'static str, &'static str, serde_json::Value);
    let rounds: [(Option<&str>, &[Call]); 4] = [
        (
            None,
            &[
                ("c-1", "read", serde_json::json!({ "path": "src/a.rs" })),
                ("c-2", "read", serde_json::json!({ "path": "src/b.rs" })),
                ("c-3", "rg", serde_json::json!({ "pattern": "fn main" })),
            ],
        ),
        (
            None,
            &[("c-4", "bash", serde_json::json!({ "command": "ls" }))],
        ),
        (
            None,
            &[("c-5", "read", serde_json::json!({ "path": "src/c.rs" }))],
        ),
        (Some("看完了。"), &[]),
    ];
    let mut events = conversation()[..3].to_vec();
    let mut seq = 3;
    let mut next = || {
        seq += 1;
        seq
    };
    for (round, (text, calls)) in rounds.into_iter().enumerate() {
        events.push(event(
            next(),
            Some(run()),
            EventPayload::MessageAssistant(MessageAssistant {
                round: round as u32 + 1,
                text: text.map(Into::into),
                text_ref: None,
                tool_calls: calls
                    .iter()
                    .map(|(id, name, args)| ToolCallRequest {
                        call_id: ToolCallId::from_raw(*id),
                        provider_call_id: format!("p{id}"),
                        name: (*name).into(),
                        arguments: args.clone(),
                        arguments_ref: None,
                    })
                    .collect(),
                provider_blocks: None,
                input_tokens: Some(100),
                output_tokens: Some(10),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            }),
        ));
        for (id, _, _) in calls {
            let attempt = AttemptId::from_raw(format!("a{id}"));
            events.push(event(
                next(),
                Some(run()),
                EventPayload::ToolStarted(ToolStarted {
                    call_id: ToolCallId::from_raw(*id),
                    attempt_id: attempt.clone(),
                    plan_ref: EventId::from_raw("evt-0"),
                    plan_hash: plan().plan_hash(),
                }),
            ));
            events.push(event(
                next(),
                Some(run()),
                EventPayload::ToolResult(ToolResult {
                    body: None,
                    call_id: ToolCallId::from_raw(*id),
                    attempt_id: attempt,
                    status: ToolResultStatus::Completed,
                    output_ref: OutputRef(ContentRef {
                        path: format!("tool-output/run-1/{id}/output.json"),
                        size: 2,
                        hash: ContentHash::of_str("x"),
                        pointer: None,
                    }),
                    elapsed_ms: 20,
                    preview: Some(format!("{id} 的输出")),
                    stdout: None,
                    stderr: None,
                    attempt_state: None,
                }),
            ));
        }
    }
    events.push(event(
        next(),
        Some(run()),
        EventPayload::RunCompleted(RunCompleted {
            final_message: Some("看完了。".into()),
            final_message_ref: None,
            rounds: 4,
        }),
    ));
    events
}

pub fn page(events: Vec<Event>, more: bool) -> EventPage {
    let next = events.last().map(|e| e.seq).unwrap_or(Seq::ZERO);
    EventPage {
        session: session(),
        events,
        next,
        more,
    }
}

pub fn plan() -> ExecutionPlan {
    ExecutionPlan {
        operation_id: OperationId::from_raw("op-1"),
        source: PlanSource::Interactive { session: session() },
        tool: "bash".into(),
        operation: Operation::ShellCommand {
            command: "rm -rf build".into(),
        },
        run: Some(run()),
        tool_call: Some(call()),
        args: serde_json::json!({ "command": "rm -rf build" }),
        cwd: Some("/home/u/project".into()),
        targets: vec![PlanTarget::local(
            "/home/u/project/build",
            TargetAccess::Write,
        )],
        versions: PlanVersions {
            code: Some(ContentHash::of_str("rm -rf build")),
            env: None,
        },
        recovery: RecoveryMode::NoSafeRecovery,
        then_run: None,
    }
}

/// 一条待处理 Intervention 的摘要（`GET /v1/interventions` 的一项，§7.5）。
///
/// `verdicts` 用 kernel 给这一类定的那几档：清单里"此刻能答什么"就是它，界面不该自己推。
pub fn intervention_summary(
    handle: &str,
    kind: InterventionKind,
    question: &str,
) -> InterventionSummary {
    InterventionSummary {
        handle: handle.to_string(),
        kind,
        session: session(),
        run: Some(run()),
        call: Some(call()),
        question: question.to_string(),
        verdicts: kind.verdicts(),
        created_at: T0,
    }
}
