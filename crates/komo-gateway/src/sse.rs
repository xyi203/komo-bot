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
use komo_kernel::types::status::RunState;

/// 每个订阅者的缓冲。满了就丢最老的——丢掉只是"晚一点知道"，内容补读仍在账本里。
const CHANNEL_CAPACITY: usize = 256;

/// 心跳间隔。
pub const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);

/// 每个 Session 一个 `broadcast`。
#[derive(Debug, Default)]
pub struct EventHub {
    channels: Mutex<BTreeMap<SessionId, tokio::sync::broadcast::Sender<SseFrame>>>,
    /// 每个 Session 现在有**几个客户端在看**（HTTP 的 SSE 连接）。
    ///
    /// 与 `channels` 里的订阅者数不是一回事：Run 的看客（`service::run_watch`）自己也
    /// 订阅同一个广播，那是 Gateway 内部的一条，不代表屏幕前有人。要判断"有没有人在看"
    /// 只能用这个计数——它由 [`EventHub::watch`] 的守卫加减。
    viewers: Mutex<BTreeMap<SessionId, usize>>,
}

/// 一条客户端订阅的凭据：**掉了就减一**（连接断开、任务被取消、进程退出都算）。
pub struct Viewer {
    hub: Arc<EventHub>,
    session: SessionId,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let mut viewers = self.hub.viewers.lock().expect("看客表");
        if let Some(count) = viewers.get_mut(&self.session) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                viewers.remove(&self.session);
            }
        }
    }
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

    /// 记下"有一个客户端在看这个 Session"，返回一个**掉了就减一**的守卫。
    ///
    /// 谁该调它：真正把帧送到人眼前的那些连接（HTTP 的 SSE 处理器）。Run 的看客是
    /// Gateway 自己的一条内部订阅，不算。
    pub fn watch(self: &Arc<Self>, session: &SessionId) -> Viewer {
        *self
            .viewers
            .lock()
            .expect("看客表")
            .entry(session.clone())
            .or_insert(0) += 1;
        Viewer {
            hub: Arc::clone(self),
            session: session.clone(),
        }
    }

    /// 这个 Session 现在有几个客户端在看（TUI / HTTP 的 SSE 连接）。
    ///
    /// 审批投递要问它：屏幕前有人看着时，审批只弹在他面前——home chat 那条卡片是一次
    /// 网络往返（实测几秒到几十秒），人在看着的时候它只会**晚到**，还多一份噪音。
    pub fn viewers(&self, session: &SessionId) -> usize {
        self.viewers
            .lock()
            .expect("看客表")
            .get(session)
            .copied()
            .unwrap_or(0)
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
///
/// **`run.waiting` 只推 `state: waiting`**（帧里没有 `WaitReason` 那一格，那是 kernel 的
/// 形状）："在等什么"要么去 `GET /v1/runs/{id}` 的 `wait`、要么去 `GET /v1/interventions`
/// 的清单——两处都是权威，帧只负责说"它停下了"。
///
/// `run.reclaimed` **不推状态帧**：它是一条审计（"上一次执行没有收尾"），§8.9 明说它
/// 不改状态——那条 Run 由对账当场判成 `queued` 或 `waiting`，那两条各自有自己的事件。
fn derived_frames(event: &Event) -> Vec<SseEvent> {
    let run = |state: RunState| -> Option<SseEvent> {
        event
            .run
            .clone()
            .map(|run: RunId| SseEvent::RunStatus { run, state })
    };
    match &event.payload {
        EventPayload::RunAccepted(_) => run(RunState::Accepted).into_iter().collect(),
        EventPayload::RunQueued(_) => run(RunState::Queued).into_iter().collect(),
        EventPayload::RunStarted(_) => run(RunState::Running).into_iter().collect(),
        EventPayload::RunWaiting(_) => run(RunState::Waiting).into_iter().collect(),
        EventPayload::RunAbandoned(_) => run(RunState::Abandoned).into_iter().collect(),
        EventPayload::RunCompleted(_) => run(RunState::Completed).into_iter().collect(),
        EventPayload::RunFailed(_) => run(RunState::Failed).into_iter().collect(),
        EventPayload::RunCancelled(_) => run(RunState::Cancelled).into_iter().collect(),
        EventPayload::RunReclaimed(_) => Vec::new(),
        // 待审批的那一帧由 `approval.requested` 推——短 ID 在它身上，而伪造一个短 ID 会让
        // 客户端拿着一个答不了的编号去回复。
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
///
/// **`data:` 是完整的 [`SseFrame`]**：客户端（`komo-client::sse::FrameParser`）拿这一行
/// 直接 `from_str::<SseFrame>`，两边共用同一个类型才不会各说各话。早先这里只写
/// `event` 的负载，客户端却拿它去解 `SseFrame`——每一帧都必然解不开，于是全部落进
/// 「读不懂，跳过」，TUI 一帧都收不到（消息永远停在"已提交，等待事件同步"）。补读与
/// 直播都走这一个出口，所以改这里就够了。
pub fn encode_frame(frame: &SseFrame) -> String {
    let name = event_name(&frame.event);
    let payload = serde_json::to_string(frame).unwrap_or_else(|_| "null".to_string());
    format!("id: {}\nevent: {}\ndata: {}\n\n", frame.id, name, payload)
}

/// `event:` 那一行用的名字：就是 [`SseEvent`] 的 serde 标签，只给人（`curl`）看，
/// 不参与解析。
fn event_name(event: &SseEvent) -> String {
    match serde_json::to_value(event) {
        Ok(serde_json::Value::Object(mut map)) => map
            .remove("event")
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "message".to_string()),
        _ => "message".to_string(),
    }
}

/// 心跳（不带 `id:`——它不是一个游标位置）。
///
/// 只是一行注释：保活靠"有字节到"，不需要客户端解释它，更不能让它顶着一个假的
/// `event: heartbeat` 冒充一帧。
pub fn heartbeat() -> String {
    ": keep-alive\n\n".to_string()
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
                state: RunState::Completed,
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
                state: RunState::Running,
            },
        });
        assert!(
            text.starts_with("id: 43\nevent: run_status\ndata: {"),
            "{text}"
        );
        assert!(text.ends_with("\n\n"));
    }

    /// 客户端（`komo-client::sse::FrameParser`）只解 `data:` 那一行，且按 [`SseFrame`] 解。
    /// 这里就做它做的那一步：解不出来 = 每一帧都被"跳过"，TUI 一帧都收不到。
    #[test]
    fn the_data_line_round_trips_through_the_client_s_type() {
        let frames = [
            SseFrame {
                id: Seq(7),
                session: SessionId::from_raw("sess-1"),
                event: SseEvent::Event(Box::new(completed(7))),
            },
            SseFrame {
                id: Seq(8),
                session: SessionId::from_raw("sess-1"),
                event: SseEvent::RunStatus {
                    run: RunId::from_raw("run-1"),
                    state: RunState::Completed,
                },
            },
            SseFrame {
                id: Seq(9),
                session: SessionId::from_raw("sess-1"),
                event: SseEvent::Heartbeat,
            },
        ];
        for frame in &frames {
            let text = encode_frame(frame);
            let data = text
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap_or_else(|| panic!("这一帧没有 data 行：{text}"));
            let parsed: SseFrame = serde_json::from_str(data)
                .unwrap_or_else(|error| panic!("客户端解不开这一帧：{error}\n{text}"));
            assert_eq!(&parsed, frame);
        }
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

    /// `run.waiting` 推的是 `waiting` 这一个状态帧——**四类等待共用它**（§8.4 把"等什么"
    /// 收进了 `WaitReason`）：界面靠这一帧知道"它停下了"，再去问清单/详情在等谁。
    #[test]
    fn a_waiting_run_pushes_one_waiting_frame_whatever_it_waits_for() {
        let run = RunId::from_raw("run-1");
        let waiting = |reason: komo_kernel::types::status::WaitReason| Event {
            v: 1,
            seq: Seq(3),
            event_id: EventId::from_raw("evt-3"),
            session: SessionId::from_raw("sess-1"),
            run: Some(run.clone()),
            ts: datetime!(2026-09-16 08:00:00 UTC),
            payload: EventPayload::RunWaiting(komo_kernel::events::RunWaiting { reason }),
        };
        for reason in [
            komo_kernel::types::status::WaitReason::Approval {
                approval: komo_kernel::types::ids::ApprovalId::from_raw("ap-1"),
            },
            komo_kernel::types::status::WaitReason::Intervention {
                intervention: komo_kernel::types::ids::InterventionId::from_raw("run-1"),
            },
            komo_kernel::types::status::WaitReason::Retry {
                attempts: 1,
                not_before: datetime!(2026-09-16 08:05:00 UTC),
                cause: komo_kernel::types::status::RetryCause::RateLimited,
            },
            komo_kernel::types::status::WaitReason::Dependency {
                run: RunId::from_raw("run-0"),
            },
        ] {
            let frames = derived_frames(&waiting(reason.clone()));
            assert_eq!(frames.len(), 1, "{reason:?}");
            assert!(
                matches!(
                    &frames[0],
                    SseEvent::RunStatus {
                        state: RunState::Waiting,
                        ..
                    }
                ),
                "{reason:?} 推出的是 {frames:?}"
            );
        }
    }

    /// `run.reclaimed`（上一次执行没收尾，领取权已交还）**不推状态帧**：§8.9 明说它不改
    /// 状态——推一个 `interrupted` 会让客户端显示一个数据库里根本没有的状态。
    #[test]
    fn a_reclaim_audit_pushes_no_state_frame() {
        let event = Event {
            v: 1,
            seq: Seq(4),
            event_id: EventId::from_raw("evt-4"),
            session: SessionId::from_raw("sess-1"),
            run: Some(RunId::from_raw("run-1")),
            ts: datetime!(2026-09-16 08:00:00 UTC),
            payload: EventPayload::RunReclaimed(komo_kernel::events::RunReclaimed {
                executor: Some(komo_kernel::types::ids::ExecutorId::from_raw("exec-0")),
                reason: "上一次执行没有收尾".into(),
            }),
        };
        assert!(derived_frames(&event).is_empty());
    }
}
