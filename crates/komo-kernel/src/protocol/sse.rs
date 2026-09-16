//! SSE 的线格式（§13.1）。
//!
//! 「SSE 事件带 Session 内递增序号，断线后按游标补读。JSONL 事件是内容补读来源，
//! state.db 索引负责定位，内存通知仅提示有新数据。」——所以每一帧都带 Session 内的
//! seq 当 SSE 的 `id:`，客户端断线后把它作为 `Last-Event-ID` / `?from=` 交回来，服务
//! 端从 JSONL 补读。**内存通知丢了不会丢数据**，它只是让客户端早一点知道。

use serde::{Deserialize, Serialize};

use crate::events::Event;
use crate::types::ids::{RunId, Seq, SessionId};
use crate::types::status::RunStatus;

/// 一帧 SSE。`id` 就是 Session 内的 seq——游标和序号是同一个数，不再发明第二套。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SseFrame {
    /// SSE 的 `id:`。
    pub id: Seq,
    pub session: SessionId,
    #[serde(flatten)]
    pub event: SseEvent,
}

/// SSE 的 `event:` 与它的负载。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum SseEvent {
    /// 一条已经同步且索引完成的 JSONL 事件（§8.8）。
    Event(Box<Event>),
    /// Run 的状态变了。派生自事件，给不想自己 fold 的客户端。
    RunStatus { run: RunId, status: RunStatus },
    /// 有新的待处理审批；详情去 `GET /v1/approvals/{id}` 取，**不靠这条通知传授权**。
    ApprovalPending {
        approval: crate::types::ids::ApprovalId,
        short_id: crate::types::ids::ShortId,
    },
    /// 一条审批有结论了。
    ApprovalDecided {
        approval: crate::types::ids::ApprovalId,
        approved: bool,
    },
    /// 心跳。只为让代理不掐连接，不带任何状态。
    Heartbeat,
}

/// 客户端的订阅游标。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// 已经收到的最后一个 seq。服务端从它**之后**补读。
    pub after: Seq,
}

impl Cursor {
    pub fn after(seq: Seq) -> Self {
        Cursor { after: seq }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_carries_its_cursor_as_its_id() {
        let frame = SseFrame {
            id: Seq(43),
            session: SessionId::from_raw("sess-1"),
            event: SseEvent::RunStatus {
                run: RunId::from_raw("run-1"),
                status: RunStatus::Running,
            },
        };
        let text = serde_json::to_string(&frame).unwrap();
        assert!(text.contains(r#""event":"run_status""#), "{text}");
        assert_eq!(serde_json::from_str::<SseFrame>(&text).unwrap(), frame);
    }

    #[test]
    fn a_heartbeat_carries_nothing() {
        let frame = SseFrame {
            id: Seq(0),
            session: SessionId::from_raw("sess-1"),
            event: SseEvent::Heartbeat,
        };
        let text = serde_json::to_string(&frame).unwrap();
        assert!(!text.contains("\"data\""), "{text}");
    }
}
