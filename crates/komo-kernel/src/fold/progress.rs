//! `progress_of(events) -> Option<RunProgress>`：任务会话里最近一条 Run 现在跑到哪了。
//!
//! 建在 [`fold`] 上：状态、轮数、调用与终态回复都是 `Surface` 已有的；这里只多扫一遍事件，
//! 补 fold 不留的东西——时间戳、调用的工具名与参数摘要、最近一句带正文的助手回复。纯函数，
//! 没有 I/O：正在跑的调用的输出尾部（`.partial`）由 Gateway 填。

use std::collections::BTreeMap;

use time::OffsetDateTime;

use crate::events::{Event, EventPayload};
use crate::types::ids::ToolCallId;
use crate::types::status::{RunState, ToolCallState, WaitReason};
use crate::types::task::{RunProgress, RunningCall};

use super::fold;

const SUMMARY_CHARS: usize = 160;
const NOTE_CHARS: usize = 300;
const FINAL_CHARS: usize = 500;

/// 参数里挑哪个字段当一行摘要，按顺序取第一个有的。
const SUMMARY_KEYS: [&str; 6] = ["command", "code", "path", "pattern", "prompt", "query"];

/// 最近一条没被并走的 Run 的进度；会话里还没有 Run 时是 `None`。
pub fn progress_of<'a, I>(events: I) -> Option<RunProgress>
where
    I: IntoIterator<Item = &'a Event>,
    I::IntoIter: Clone,
{
    let events = events.into_iter();
    let surface = fold(events.clone());
    // 被并走的 Run 已经是另一条 Run 的一部分，不算"最近一条"。
    let view = surface
        .runs
        .values()
        .filter(|run| run.status != RunState::Absorbed)
        .max_by_key(|run| run.first_seq)?;
    let run = &view.run;

    let mut accepted_at = None;
    let mut last_at = None;
    let mut note = None;
    let mut started_at: BTreeMap<ToolCallId, OffsetDateTime> = BTreeMap::new();
    // 调用 id → (工具名, 参数摘要)：计划内联时取计划，否则取发起它的那条助手回复。
    let mut described: BTreeMap<ToolCallId, (String, String)> = BTreeMap::new();

    for event in events {
        if event.run.as_ref() != Some(run) {
            continue;
        }
        last_at = Some(event.ts);
        match &event.payload {
            EventPayload::RunAccepted(_) => accepted_at = Some(event.ts),
            EventPayload::MessageAssistant(body) => {
                if let Some(text) = body
                    .text
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    note = Some(truncate(text, NOTE_CHARS));
                }
                for call in &body.tool_calls {
                    described
                        .entry(call.call_id.clone())
                        .or_insert_with(|| (call.name.clone(), summarize(&call.arguments)));
                }
            }
            EventPayload::ToolPlanned(body) => {
                if let Some(plan) = &body.plan {
                    described.insert(
                        body.call_id.clone(),
                        (plan.tool.clone(), summarize(&plan.args)),
                    );
                }
            }
            EventPayload::ToolStarted(body) => {
                started_at.insert(body.call_id.clone(), event.ts);
            }
            _ => {}
        }
    }
    let accepted_at = accepted_at?;

    let running = view
        .calls
        .iter()
        .filter_map(|id| surface.calls.get(id))
        .filter(|call| matches!(call.state, ToolCallState::Planned | ToolCallState::Started))
        .map(|call| {
            let (tool, summary) = described.get(&call.call).cloned().unwrap_or_default();
            RunningCall {
                call: call.call.clone(),
                attempt: call.attempt.clone(),
                tool,
                summary,
                started_at: started_at.get(&call.call).copied(),
                output_tail: None,
            }
        })
        .collect();

    Some(RunProgress {
        run: run.clone(),
        state: view.status,
        accepted_at,
        last_at: last_at.unwrap_or(accepted_at),
        rounds: view.rounds,
        calls: view.calls.len() as u32,
        running,
        note,
        final_message: view
            .final_message
            .as_deref()
            .map(|text| truncate(text.trim(), FINAL_CHARS)),
        reason: view.reason.clone(),
        waiting_on: view.wait.as_ref().map(|wait| wait_phrase(wait)),
    })
}

fn wait_phrase(wait: &WaitReason) -> String {
    match wait {
        WaitReason::Approval { .. } => "旧的审批记录".to_string(),
        WaitReason::Retry { .. } => "模型或网络出错后的退避重试".to_string(),
        WaitReason::Intervention { intervention } => {
            format!("操作者处理 {intervention}")
        }
        WaitReason::Dependency { run } => format!("更早的一条 Run（{run}）跑完"),
    }
}

/// 参数的一行摘要（对外给 gateway / runtime 的查进度用，口径与进度快照里的 `summary` 一致）。
pub fn summarize_args(args: &serde_json::Value) -> String {
    summarize(args)
}

/// 参数的一行摘要：先挑常见的"主字段"，没有就用整份参数的紧凑 JSON。
fn summarize(args: &serde_json::Value) -> String {
    let picked = SUMMARY_KEYS
        .iter()
        .find_map(|key| args.get(key))
        .map(|value| match value.as_str() {
            Some(text) => text.to_string(),
            None => value.to_string(),
        });
    let text = match picked {
        Some(text) => text,
        None if args.is_null() || args.as_object().is_some_and(|map| map.is_empty()) => {
            String::new()
        }
        None => args.to_string(),
    };
    truncate(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        SUMMARY_CHARS,
    )
}

/// 按字符数截断，超出补 `…`。
fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        EVENT_FORMAT_VERSION, MessageAssistant, RunAbsorbed, RunAccepted, RunCompleted, RunQueued,
        RunStarted, ToolPlanned, ToolResult, ToolStarted,
    };
    use crate::test_support::sample_plan;
    use crate::types::digest::ContentHash;
    use crate::types::ids::{AttemptId, EventId, ExecutorId, RequestKey, RunId, Seq, SessionId};
    use crate::types::plan::{PlanHash, PlanSource};
    use crate::types::refs::{ContentRef, OutputRef, ToolResultStatus};
    use crate::types::turn::ToolCallRequest;
    use time::macros::datetime;

    fn event(seq: u64, run: &str, payload: EventPayload) -> Event {
        Event {
            entry: None,
            v: EVENT_FORMAT_VERSION,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("evt-{seq}")),
            session: SessionId::from_raw("task-1"),
            run: Some(RunId::from_raw(run)),
            ts: datetime!(2026-10-19 08:00:00 UTC) + time::Duration::seconds(seq as i64 * 10),
            payload,
        }
    }

    fn accepted(seq: u64, run: &str) -> Event {
        event(
            seq,
            run,
            EventPayload::RunAccepted(RunAccepted {
                request_key: RequestKey::new(format!("task:{seq}")),
                input_hash: ContentHash::of_str("干活"),
                text: Some("干活".into()),
                text_ref: None,
                source: PlanSource::Interactive {
                    session: SessionId::from_raw("task-1"),
                },
                peer: None,
                model: None,
                effort: None,
                delegate: None,
                snapshot: None,
                interject: false,
            }),
        )
    }

    fn started(seq: u64, run: &str) -> Event {
        event(
            seq,
            run,
            EventPayload::RunStarted(RunStarted {
                executor: ExecutorId::from_raw("ex-1"),
                generation: 1,
            }),
        )
    }

    fn assistant(seq: u64, run: &str, round: u32, text: &str, calls: &[(&str, &str)]) -> Event {
        event(
            seq,
            run,
            EventPayload::MessageAssistant(MessageAssistant {
                round,
                text: Some(text.into()),
                text_ref: None,
                tool_calls: calls
                    .iter()
                    .map(|(id, command)| ToolCallRequest {
                        call_id: ToolCallId::from_raw(*id),
                        provider_call_id: format!("p-{id}"),
                        name: "bash".into(),
                        arguments: serde_json::json!({ "command": command }),
                        arguments_ref: None,
                    })
                    .collect(),
                provider_blocks: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            }),
        )
    }

    fn planned(seq: u64, run: &str, call: &str, command: &str, inline: bool) -> Event {
        let mut plan = sample_plan("bash", &SessionId::from_raw("task-1"));
        plan.args = serde_json::json!({ "command": command });
        event(
            seq,
            run,
            EventPayload::ToolPlanned(ToolPlanned {
                call_id: ToolCallId::from_raw(call),
                plan_hash: PlanHash::from_raw("h"),
                plan: inline.then(|| Box::new(plan)),
                plan_ref: None,
            }),
        )
    }

    fn call_started(seq: u64, run: &str, call: &str) -> Event {
        event(
            seq,
            run,
            EventPayload::ToolStarted(ToolStarted {
                call_id: ToolCallId::from_raw(call),
                attempt_id: AttemptId::from_raw(format!("att-{call}")),
                plan_ref: EventId::from_raw("evt-0"),
                plan_hash: PlanHash::from_raw("h"),
            }),
        )
    }

    fn call_result(seq: u64, run: &str, call: &str) -> Event {
        event(
            seq,
            run,
            EventPayload::ToolResult(ToolResult {
                body: None,
                call_id: ToolCallId::from_raw(call),
                attempt_id: AttemptId::from_raw(format!("att-{call}")),
                status: ToolResultStatus::Completed,
                output_ref: OutputRef(ContentRef {
                    path: "out.json".into(),
                    size: 2,
                    hash: ContentHash::of_str("ok"),
                    pointer: None,
                }),
                elapsed_ms: 5,
                preview: None,
                stdout: None,
                stderr: None,
                attempt_state: None,
            }),
        )
    }

    fn running_task() -> Vec<Event> {
        vec![
            accepted(1, "run-1"),
            started(2, "run-1"),
            assistant(3, "run-1", 1, "先看看目录", &[("c1", "ls /tmp/x")]),
            planned(4, "run-1", "c1", "ls /tmp/x", true),
            call_started(5, "run-1", "c1"),
            call_result(6, "run-1", "c1"),
            assistant(
                7,
                "run-1",
                2,
                "开始跑长任务",
                &[("c2", "cd /tmp/x &&\n  for i in 1 2 3; do sleep 1; done")],
            ),
            planned(8, "run-1", "c2", "x", true),
            call_started(9, "run-1", "c2"),
        ]
    }

    #[test]
    fn a_running_task_reports_rounds_calls_and_the_unfinished_call() {
        let progress = progress_of(&running_task()).expect("有 Run");
        assert_eq!(progress.state, RunState::Running);
        assert_eq!(progress.rounds, 2);
        assert_eq!(progress.calls, 2);
        assert_eq!(progress.note.as_deref(), Some("开始跑长任务"));
        assert_eq!(progress.accepted_at, datetime!(2026-10-19 08:00:10 UTC));
        assert_eq!(progress.last_at, datetime!(2026-10-19 08:01:30 UTC));
        assert_eq!(progress.running.len(), 1);
        let call = &progress.running[0];
        assert_eq!(call.tool, "bash");
        assert_eq!(call.summary, "x", "内联计划里的参数优先");
        assert_eq!(call.started_at, Some(datetime!(2026-10-19 08:01:30 UTC)));
        assert_eq!(call.output_tail, None);
    }

    #[test]
    fn an_external_plan_falls_back_to_the_assistant_tool_call() {
        let mut events = running_task();
        events[7] = planned(8, "run-1", "c2", "x", false);
        let progress = progress_of(&events).expect("有 Run");
        let call = &progress.running[0];
        assert_eq!(call.tool, "bash");
        assert_eq!(
            call.summary,
            "cd /tmp/x && for i in 1 2 3; do sleep 1; done"
        );
    }

    #[test]
    fn a_planned_but_not_started_call_has_no_start_time() {
        let mut events = running_task();
        events.pop();
        let progress = progress_of(&events).expect("有 Run");
        assert_eq!(progress.running.len(), 1);
        assert_eq!(progress.running[0].started_at, None);
    }

    #[test]
    fn a_completed_task_has_no_running_calls_and_keeps_its_final_message() {
        let mut events = running_task();
        events.push(call_result(10, "run-1", "c2"));
        events.push(event(
            11,
            "run-1",
            EventPayload::RunCompleted(RunCompleted {
                final_message: Some("都跑完了".into()),
                final_message_ref: None,
                rounds: 2,
            }),
        ));
        let progress = progress_of(&events).expect("有 Run");
        assert_eq!(progress.state, RunState::Completed);
        assert!(progress.running.is_empty());
        assert_eq!(progress.final_message.as_deref(), Some("都跑完了"));
        assert_eq!(progress.last_at, datetime!(2026-10-19 08:01:50 UTC));
    }

    #[test]
    fn a_queued_only_task_reports_the_queued_run() {
        let events = vec![
            accepted(1, "run-1"),
            event(
                2,
                "run-1",
                EventPayload::RunQueued(RunQueued {
                    input_ref: EventId::from_raw("evt-1"),
                }),
            ),
        ];
        let progress = progress_of(&events).expect("有 Run");
        assert_eq!(progress.state, RunState::Queued);
        assert_eq!(progress.rounds, 0);
        assert_eq!(progress.calls, 0);
        assert!(progress.running.is_empty());
    }

    #[test]
    fn an_absorbed_run_is_skipped_for_the_host_run() {
        let events = vec![
            accepted(1, "run-1"),
            started(2, "run-1"),
            accepted(3, "run-2"),
            event(
                4,
                "run-2",
                EventPayload::RunAbsorbed(RunAbsorbed {
                    into: RunId::from_raw("run-1"),
                }),
            ),
        ];
        let progress = progress_of(&events).expect("有 Run");
        assert_eq!(progress.run, RunId::from_raw("run-1"));
    }

    #[test]
    fn no_events_means_no_progress() {
        assert_eq!(progress_of(&Vec::<Event>::new()), None);
    }

    #[test]
    fn summaries_pick_the_main_field_collapse_whitespace_and_cut_on_char_boundaries() {
        assert_eq!(
            summarize(&serde_json::json!({"path": "/a/b", "x": 1})),
            "/a/b"
        );
        assert_eq!(summarize(&serde_json::json!({"x": 1})), r#"{"x":1}"#);
        assert_eq!(summarize(&serde_json::json!({})), "");
        let long = "啊".repeat(200);
        let cut = summarize(&serde_json::json!({ "command": long }));
        assert_eq!(cut.chars().count(), SUMMARY_CHARS + 1);
        assert!(cut.ends_with('…'));
    }
}
