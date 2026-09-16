//! 每个 Session 一个广播；游标就是 seq（§13.1、§8.8）。
//!
//! 「SSE 事件带 Session 内递增序号，断线后按游标补读。JSONL 事件是内容补读来源，
//! state.db 索引负责定位，**内存通知仅提示有新数据**。」——所以这里的广播丢了不会丢
//! 数据：订阅先按游标从账本补读历史，再接直播，补读用的是
//! [`Ledger::read`](komo_kernel::traits::Ledger::read) 的**短事务分页**（§14 待验证项
//! 「Turso 长读事务与并发写提交的快照语义」的既定对策）。
//!
//! 心跳 15 秒一次，只为让代理不掐连接。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::types::ids::{RunId, Seq, SessionId};
use komo_kernel::types::status::RunStatus;

/// 每个订阅者的缓冲。满了就丢最老的——丢掉只是"晚一点知道"，内容补读仍在账本里。
const CHANNEL_CAPACITY: usize = 256;

/// 心跳间隔。
pub const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);

/// 每个 Session 一个 `broadcast`。
#[derive(Debug, Default)]
pub struct EventHub {
    channels: Mutex<BTreeMap<SessionId, tokio::sync::broadcast::Sender<SseFrame>>>,
}

impl EventHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// 订阅一个 Session 的直播。**先订阅再补读**，中间新来的事件才不会漏。
    pub fn subscribe(&self, session: &SessionId) -> tokio::sync::broadcast::Receiver<SseFrame> {
        self.sender(session).subscribe()
    }

    fn sender(&self, session: &SessionId) -> tokio::sync::broadcast::Sender<SseFrame> {
        let mut channels = self.channels.lock().expect("事件广播表");
        channels
            .entry(session.clone())
            .or_insert_with(|| tokio::sync::broadcast::channel(CHANNEL_CAPACITY).0)
            .clone()
    }

    /// 推一帧。没有订阅者就当没发生。
    pub fn publish(&self, frame: SseFrame) {
        let _ = self.sender(&frame.session).send(frame);
    }

    /// 一条日志事件进来时推出去的那几帧：事件本身，加上它派生出的状态帧。
    pub fn publish_event(&self, event: &Event) {
        let session = event.session.clone();
        self.publish(SseFrame {
            id: event.seq,
            session: session.clone(),
            event: SseEvent::Event(Box::new(event.clone())),
        });
        for derived in derived_frames(event) {
            self.publish(SseFrame {
                id: event.seq,
                session: session.clone(),
                event: derived,
            });
        }
    }

    /// 一条审批有结论了（决定不落在会话日志的同一条路径上，所以单独推）。
    pub fn publish_decision(
        &self,
        session: &SessionId,
        approval: &komo_kernel::types::ids::ApprovalId,
        approved: bool,
        at: Seq,
    ) {
        self.publish(SseFrame {
            id: at,
            session: session.clone(),
            event: SseEvent::ApprovalDecided {
                approval: approval.clone(),
                approved,
            },
        });
    }
}

/// 从一条事件派生出的附加帧：Run 状态与待处理审批。
///
/// 「派生自事件，给不想自己 fold 的客户端。」
fn derived_frames(event: &Event) -> Vec<SseEvent> {
    let run = |status: RunStatus| -> Option<SseEvent> {
        event
            .run
            .clone()
            .map(|run: RunId| SseEvent::RunStatus { run, status })
    };
    match &event.payload {
        EventPayload::RunAccepted(_) => run(RunStatus::Ingesting).into_iter().collect(),
        EventPayload::RunQueued(_) => run(RunStatus::Queued).into_iter().collect(),
        EventPayload::RunStarted(_) => run(RunStatus::Running).into_iter().collect(),
        EventPayload::RunWaitingRetry(_) => run(RunStatus::WaitingRetry).into_iter().collect(),
        EventPayload::RunInterrupted(_) => run(RunStatus::Interrupted).into_iter().collect(),
        EventPayload::RunNeedsAttention(_) => run(RunStatus::NeedsAttention).into_iter().collect(),
        EventPayload::RunCompleted(_) => run(RunStatus::Completed).into_iter().collect(),
        EventPayload::RunFailed(_) => run(RunStatus::Failed).into_iter().collect(),
        EventPayload::RunCancelled(_) => run(RunStatus::Cancelled).into_iter().collect(),
        // `run.waiting_approval` 只说"停下了"；待处理审批的那一帧由
        // `approval.requested` 推——短 ID 在它身上，而伪造一个短 ID 会让客户端拿着
        // 一个答不了的编号去回复。
        EventPayload::RunWaitingApproval(_) => {
            run(RunStatus::WaitingApproval).into_iter().collect()
        }
        EventPayload::ApprovalRequested(body) => vec![SseEvent::ApprovalPending {
            approval: body.approval.clone(),
            short_id: body.short_id.clone(),
        }],
        EventPayload::ApprovalDecided(body) => vec![SseEvent::ApprovalDecided {
            approval: body.approval.clone(),
            approved: body.approved,
        }],
        _ => Vec::new(),
    }
}

/// 一帧渲染成 SSE 的三行（`id:` / `event:` / `data:`）。
///
/// `id` 就是 Session 内的 seq，客户端断线后把它作为 `Last-Event-ID` 交回来。
pub fn encode_frame(frame: &SseFrame) -> String {
    let (name, data) = match serde_json::to_value(&frame.event) {
        Ok(serde_json::Value::Object(mut map)) => {
            let name = map
                .remove("event")
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "message".to_string());
            let data = map
                .remove("data")
                .unwrap_or(serde_json::Value::Object(Default::default()));
            (name, data)
        }
        _ => ("message".to_string(), serde_json::Value::Null),
    };
    let payload = serde_json::to_string(&data).unwrap_or_else(|_| "null".to_string());
    format!("id: {}\nevent: {}\ndata: {}\n\n", frame.id, name, payload)
}

/// 心跳帧（不带 `id:`——它不是一个游标位置）。
pub fn heartbeat() -> String {
    ": keep-alive\nevent: heartbeat\ndata: {}\n\n".to_string()
}

/// `Last-Event-ID` 头与 `?from=` 两种游标，都认。头优先——重连时它更新。
pub fn cursor_from(last_event_id: Option<&str>, from: Seq) -> Seq {
    last_event_id
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Seq)
        .unwrap_or(from)
}

/// 共享的 [`EventHub`]。
pub type SharedHub = Arc<EventHub>;

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::events::{Event, EventPayload, RunCompleted};
    use komo_kernel::types::ids::EventId;
    use time::macros::datetime;

    fn completed(seq: u64) -> Event {
        Event {
            v: 1,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("evt-{seq}")),
            session: SessionId::from_raw("sess-1"),
            run: Some(RunId::from_raw("run-1")),
            ts: datetime!(2026-09-16 08:00:00 UTC),
            payload: EventPayload::RunCompleted(RunCompleted {
                final_message: Some("好了".into()),
                final_message_ref: None,
                rounds: 1,
            }),
        }
    }

    #[tokio::test]
    async fn a_published_event_reaches_a_subscriber_with_its_seq_as_the_id() {
        let hub = EventHub::new();
        let session = SessionId::from_raw("sess-1");
        let mut sub = hub.subscribe(&session);
        hub.publish_event(&completed(7));

        let frame = sub.recv().await.expect("收到事件帧");
        assert_eq!(frame.id, Seq(7));
        assert!(matches!(frame.event, SseEvent::Event(_)));

        let derived = sub.recv().await.expect("收到派生的状态帧");
        assert!(matches!(
            derived.event,
            SseEvent::RunStatus {
                status: RunStatus::Completed,
                ..
            }
        ));
    }

    #[test]
    fn a_frame_encodes_its_cursor_as_the_sse_id() {
        let text = encode_frame(&SseFrame {
            id: Seq(43),
            session: SessionId::from_raw("sess-1"),
            event: SseEvent::RunStatus {
                run: RunId::from_raw("run-1"),
                status: RunStatus::Running,
            },
        });
        assert!(
            text.starts_with("id: 43\nevent: run_status\ndata: {"),
            "{text}"
        );
        assert!(text.ends_with("\n\n"));
    }

    #[test]
    fn last_event_id_beats_the_query_cursor() {
        assert_eq!(cursor_from(Some("41"), Seq(3)), Seq(41));
        assert_eq!(cursor_from(None, Seq(3)), Seq(3));
        assert_eq!(
            cursor_from(Some("垃圾"), Seq(3)),
            Seq(3),
            "读不懂就用 ?from="
        );
    }
}
