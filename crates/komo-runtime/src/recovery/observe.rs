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
            | EventPayload::RunCancelled(_)
            // 操作者放弃也是终态（§7.5、§8.4：四个终态各自算数）。漏掉它，一条已经
            // `abandoned` 的 Run 会被当成"还在跑"，于是被放回队列再跑一次。
            | EventPayload::RunAbandoned(_) => return LogTail::Final,
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
///
/// 事件只有一个 `run.waiting`，"在等什么"在 `WaitReason` 里（§8.4）。这里只认**最后
/// 那一条** `run.waiting`，而且它必须是等审批：
///
/// - 往前翻找"最近一条等审批的"会把一条**早就答复过**的审批当成现在的落点，于是恢复
///   扫描拿着一个陈旧的审批号去问数据库，答"有效"就把 Run 接着跑起来——那是拿旧授权
///   放行新动作。
/// - 最后停的不是审批（等退避、等前一条 Run）→ 答案就是"没有"，让上层照 §8.4 判成
///   需要人重答，而不是替它挑一个。
pub fn waiting_on_approval(events: &[Event], run: &RunId) -> Option<ApprovalId> {
    let last = events_of(events, run)
        .into_iter()
        .rev()
        .find(|event| matches!(event.payload, EventPayload::RunWaiting(_)))?;
    match &last.payload {
        EventPayload::RunWaiting(waiting) => match &waiting.reason {
            komo_kernel::types::status::WaitReason::Approval { approval } => Some(approval.clone()),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::events::{EVENT_FORMAT_VERSION, MessageAssistant, RunAccepted, RunStarted};
    use komo_kernel::types::digest::ContentHash;
    use komo_kernel::types::ids::{EventId, RequestKey, Seq, SessionId};
    use komo_kernel::types::plan::PlanSource;
    use komo_kernel::types::status::{RetryCause, WaitReason};
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
            // 这个夹具是普通 Run 的受理事件：没有父、没有契约。
            delegate: None,
            snapshot: None,
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

    /// 操作者放弃也是终态（§7.5、§8.4）。漏掉它，一条已经 `abandoned` 的 Run 会被当成
    /// "还在跑"，于是被放回队列再跑一次。
    #[test]
    fn an_abandoned_run_is_final_in_the_log_too() {
        let events = vec![
            event(1, accepted()),
            event(
                2,
                EventPayload::RunAbandoned(komo_kernel::events::RunAbandoned {
                    by: None,
                    reason: Some("查过了，不追究".into()),
                }),
            ),
        ];
        assert_eq!(log_tail(&events, &run()), LogTail::Final);
    }

    /// 只有**最后停的那一条**是等审批时，才答得出审批号（§8.4）。
    ///
    /// 往前翻找"最近一条等审批的"会把一条早就答复过的审批当成现在的落点——恢复扫描
    /// 于是拿着它去问数据库，答"有效"就把 Run 接着跑起来，那是拿旧授权放行新动作。
    #[test]
    fn only_the_last_wait_counts_and_only_if_it_is_an_approval() {
        let approval = ApprovalId::from_raw("ap-7");
        let waiting = |reason: WaitReason| {
            EventPayload::RunWaiting(komo_kernel::events::RunWaiting { reason })
        };
        let approval_wait = event(
            2,
            waiting(WaitReason::Approval {
                approval: approval.clone(),
            }),
        );
        let retry_wait = event(
            3,
            waiting(WaitReason::Retry {
                attempts: 1,
                not_before: datetime!(2026-09-15 08:05:00 UTC),
                cause: RetryCause::Transport,
            }),
        );

        // 停审批：答得出来。
        let events = vec![event(1, accepted()), approval_wait.clone()];
        assert_eq!(waiting_on_approval(&events, &run()), Some(approval.clone()));

        // 它之后又停在退避上：最后那条不是审批，答案就是"没有"。
        let events = vec![
            event(1, accepted()),
            approval_wait.clone(),
            retry_wait.clone(),
        ];
        assert_eq!(waiting_on_approval(&events, &run()), None);

        // 之后停在前一条 Run 上也一样。
        let events = vec![
            event(1, accepted()),
            approval_wait,
            event(
                4,
                waiting(WaitReason::Dependency {
                    run: RunId::from_raw("run-0"),
                }),
            ),
        ];
        assert_eq!(waiting_on_approval(&events, &run()), None);
    }
}
