//! 测试用的固定样本。只在 `cfg(test)` 下存在，不进任何正式构建。

use komo_kernel::events::{
    EVENT_FORMAT_VERSION, Event, EventPayload, MessageAssistant, RunAccepted, RunCompleted,
    RunQueued, RunStarted, ToolPlanned, ToolResult, ToolStarted,
};
use komo_kernel::protocol::http::{
    ApprovalRecord, EventPage, InterventionKind, InterventionSummary,
};
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{
    ApprovalId, AttemptId, EventId, ExecutorId, OperationId, RequestKey, RunId, Seq, SessionId,
    ShortId, ToolCallId,
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
                    name: "shell".into(),
                    arguments: serde_json::json!({ "command": "rm -rf build" }),
                    arguments_ref: None,
                }],
                provider_blocks: None,
                input_tokens: Some(120),
                output_tokens: Some(30),
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
                grant: None,
            }),
        ),
        event(
            7,
            Some(run()),
            EventPayload::ToolResult(ToolResult {
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
        tool: "shell".into(),
        operation: Operation::ShellCommand {
            command: "rm -rf build".into(),
        },
        run: Some(run()),
        tool_call: Some(call()),
        args: serde_json::json!({ "command": "rm -rf build" }),
        cwd: Some("/home/u/project".into()),
        targets: vec![PlanTarget {
            path: "/home/u/project/build".into(),
            access: TargetAccess::Write,
            expected_version: None,
        }],
        versions: PlanVersions {
            code: Some(ContentHash::of_str("rm -rf build")),
            module: None,
            env: None,
        },
        resources: vec![],
        recovery: RecoveryMode::NoSafeRecovery,
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

pub fn approval_record() -> ApprovalRecord {
    ApprovalRecord {
        approval: ApprovalId::from_raw("appr-1"),
        short_id: ShortId::parse("7K2M").unwrap(),
        session: session(),
        run: Some(run()),
        call: Some(call()),
        plan_hash: plan().plan_hash(),
        plan: plan(),
        reason: "命中 shell 规则：任意代码需要人看一眼".into(),
        changes: Some("--- a/build.rs\n+++ b/build.rs\n@@ -1 +1 @@\n-老的一行\n+新的一行".into()),
        evidence: Some("候选模块测试：3 passed".into()),
        scopes: vec![ApprovalScope::Once, ApprovalScope::Run],
        requested_at: T0,
        valid_until: None,
        decision: None,
    }
}
