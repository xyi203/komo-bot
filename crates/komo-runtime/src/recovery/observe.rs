//! 从 JSONL 尾部读出 §8.4 的那三样观察——**纯函数**，不碰 I/O。
//!
//! 「输入是三样观察的并置：JSONL 尾部最后看到什么、state.db 最后看到什么、以及被引用
//! 的输出文件校验成不成立」（kernel `recovery` 的模块文档）。决定在 kernel，采集在这
//! 里，而采集里唯一难的一步——**这一轮停在哪个调用上**——是可以在没有数据库、没有文件
//! 的情况下逐条断言的。

use komo_kernel::events::{Event, EventPayload};
use komo_kernel::recovery::{LogTail, PendingCall};
use komo_kernel::types::ids::{ApprovalId, AttemptId, RunId, ToolCallId};
use komo_kernel::types::plan::PlanHash;
use komo_kernel::types::refs::OutputRef;

/// 这个 Run 在日志里的事件，按 seq 顺序。
pub fn events_of<'a>(events: &'a [Event], run: &RunId) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|event| event.run.as_ref() == Some(run))
        .collect()
}

/// JSONL 尾部最后看到什么（§8.4 的左列）。
pub fn log_tail(events: &[Event], run: &RunId) -> LogTail {
    let events = events_of(events, run);
    if events.is_empty() {
        // 只用请求键预留了 Run ID，正文尚未完整写入——**不能凭输入哈希补造用户指令**。
        return LogTail::InputIncomplete;
    }

    let mut accepted = false;
    let mut queued = false;
    let mut started = false;
    let mut last_round: Option<&komo_kernel::events::MessageAssistant> = None;
    let mut last_round_at = 0usize;

    for (index, event) in events.iter().enumerate() {
        match &event.payload {
            EventPayload::RunAccepted(_) => accepted = true,
            EventPayload::RunQueued(_) => queued = true,
            EventPayload::RunStarted(_) => started = true,
            EventPayload::MessageAssistant(round) => {
                last_round = Some(round);
                last_round_at = index;
            }
            EventPayload::RunCompleted(_)
            | EventPayload::RunFailed(_)
            | EventPayload::RunCancelled(_) => return LogTail::Final,
            _ => {}
        }
    }

    if let Some(round) = last_round {
        return LogTail::RoundPersisted {
            pending: pending_call(&events[last_round_at..], round),
        };
    }
    if started {
        // 已经在跑，但这一轮的完整 assistant 回复尚未保存。
        return LogTail::AwaitingModelReply;
    }
    if accepted || queued {
        // 输入已持久保存，Run 尚未开始。
        return LogTail::InputPersisted;
    }
    LogTail::InputIncomplete
}

/// 这一轮里**最靠前**的那个未完成调用停在哪。
fn pending_call(tail: &[&Event], round: &komo_kernel::events::MessageAssistant) -> PendingCall {
    let mut last_result: Option<ToolCallId> = None;
    let mut last_event_was_result = false;

    for event in tail {
        match &event.payload {
            EventPayload::ToolResult(result) => {
                last_result = Some(result.call_id.clone());
                last_event_was_result = true;
            }
            EventPayload::ToolPlanned(_) | EventPayload::ToolStarted(_) => {
                last_event_was_result = false;
            }
            _ => {}
        }
    }

    for call in &round.tool_calls {
        if has(tail, &call.call_id, Kind::Result) {
            continue;
        }
        // `tool.started` 已写、没有结果：**started 本身不证明副作用已发生**，去核对。
        if has(tail, &call.call_id, Kind::Started) {
            return PendingCall::Started {
                call: call.call_id.clone(),
            };
        }
        // 计划已写而没有 started，或者连计划都还没写——两种都是**确定尚未执行**。
        return PendingCall::Planned {
            call: call.call_id.clone(),
        };
    }

    // 这一轮的调用都有结果了。最后一条事件就是结果时，state.db 很可能还没跟上
    // （§8.4 第 5 行）；否则接着请求下一轮模型（第 4 行）。
    match (last_event_was_result, last_result) {
        (true, Some(call)) => PendingCall::ResultPersisted { call },
        _ => PendingCall::None,
    }
}

enum Kind {
    Started,
    Result,
}

fn has(tail: &[&Event], call: &ToolCallId, kind: Kind) -> bool {
    tail.iter().any(|event| match (&event.payload, &kind) {
        (EventPayload::ToolStarted(started), Kind::Started) => &started.call_id == call,
        (EventPayload::ToolResult(result), Kind::Result) => &result.call_id == call,
        _ => false,
    })
}

/// 这一轮尾部那条 `tool.result` 引用的输出——校验它要用。
pub fn output_ref_of(events: &[Event], run: &RunId, call: &ToolCallId) -> Option<OutputRef> {
    events_of(events, run)
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            EventPayload::ToolResult(result) if &result.call_id == call => {
                Some(result.output_ref.clone())
            }
            _ => None,
        })
}

/// 一条 `tool.started` 说了什么：哪次尝试、哪份计划。
///
/// 恢复要它是因为「started 本身不证明副作用已发生」（§8.5）——要去核对那次**尝试**的
/// 输出，就得先知道它的 ID；而计划哈希是核对身份的另一半。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedCall {
    pub attempt: AttemptId,
    pub plan_hash: PlanHash,
}

/// 这个调用**最后一次** `tool.started`。一次重试沿用 ToolCall ID、新增一条 attempt
/// （§8.6），所以要的是最后那条。
pub fn started_call(events: &[Event], run: &RunId, call: &ToolCallId) -> Option<StartedCall> {
    events_of(events, run)
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            EventPayload::ToolStarted(started) if &started.call_id == call => Some(StartedCall {
                attempt: started.attempt_id.clone(),
                plan_hash: started.plan_hash.clone(),
            }),
            _ => None,
        })
}

/// 这个 Run 最后停在哪条审批上（日志侧；**权威仍是 state.db**，§7.4）。
pub fn waiting_approval(events: &[Event], run: &RunId) -> Option<ApprovalId> {
    events_of(events, run)
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            EventPayload::RunWaitingApproval(waiting) => Some(waiting.approval.clone()),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::events::{EVENT_FORMAT_VERSION, MessageAssistant, RunAccepted, RunStarted};
    use komo_kernel::types::digest::ContentHash;
    use komo_kernel::types::ids::{EventId, RequestKey, Seq, SessionId};
    use komo_kernel::types::plan::PlanSource;
    use komo_kernel::types::turn::ToolCallRequest;
    use time::macros::datetime;

    fn run() -> RunId {
        RunId::from_raw("run-1")
    }

    fn event(seq: u64, payload: EventPayload) -> Event {
        Event {
            v: EVENT_FORMAT_VERSION,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("e-{seq}")),
            session: SessionId::from_raw("s-1"),
            run: Some(run()),
            ts: datetime!(2026-09-15 08:00:00 UTC),
            payload,
        }
    }

    fn accepted() -> EventPayload {
        EventPayload::RunAccepted(RunAccepted {
            request_key: RequestKey::new("k"),
            input_hash: ContentHash::of_str("hi"),
            text: Some("hi".into()),
            text_ref: None,
            source: PlanSource::Interactive {
                session: SessionId::from_raw("s-1"),
            },
            peer: None,
            model: Some("m".into()),
            effort: None,
        })
    }

    fn round(calls: &[&str]) -> EventPayload {
        EventPayload::MessageAssistant(MessageAssistant {
            round: 1,
            text: None,
            text_ref: None,
            tool_calls: calls
                .iter()
                .map(|id| ToolCallRequest {
                    call_id: ToolCallId::from_raw(*id),
                    provider_call_id: format!("p-{id}"),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                    arguments_ref: None,
                })
                .collect(),
            provider_blocks: None,
            input_tokens: None,
            output_tokens: None,
        })
    }

    #[test]
    fn a_run_with_no_events_only_ever_reserved_its_id() {
        assert_eq!(log_tail(&[], &run()), LogTail::InputIncomplete);
    }

    #[test]
    fn an_accepted_run_that_never_started_is_input_persisted() {
        let events = vec![event(1, accepted())];
        assert_eq!(log_tail(&events, &run()), LogTail::InputPersisted);
    }

    #[test]
    fn a_started_run_without_an_assistant_round_is_awaiting_the_model() {
        let events = vec![
            event(1, accepted()),
            event(
                2,
                EventPayload::RunStarted(RunStarted {
                    executor: komo_kernel::types::ids::ExecutorId::from_raw("x"),
                    generation: 1,
                }),
            ),
        ];
        assert_eq!(log_tail(&events, &run()), LogTail::AwaitingModelReply);
    }

    #[test]
    fn a_round_whose_call_never_got_a_plan_is_still_certainly_not_run() {
        let events = vec![event(1, accepted()), event(2, round(&["call-1"]))];
        assert_eq!(
            log_tail(&events, &run()),
            LogTail::RoundPersisted {
                pending: PendingCall::Planned {
                    call: ToolCallId::from_raw("call-1")
                }
            },
            "连计划都没落盘，和 planned 一样是「确定尚未执行」"
        );
    }

    #[test]
    fn a_round_without_any_calls_has_nothing_left_unfinished() {
        let events = vec![event(1, accepted()), event(2, round(&[]))];
        assert_eq!(
            log_tail(&events, &run()),
            LogTail::RoundPersisted {
                pending: PendingCall::None
            }
        );
    }

    #[test]
    fn events_belonging_to_another_run_are_not_read_as_this_ones_tail() {
        let mut other = event(1, accepted());
        other.run = Some(RunId::from_raw("run-2"));
        assert_eq!(log_tail(&[other], &run()), LogTail::InputIncomplete);
    }
}
