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
    /// 模型正在打字。
    ///
    /// **只在 SSE 上，永不进 JSONL。**§8.3：「工具输出先独立持久化，再一次提交对应结果
    /// 事件，**不按流式 token 写 JSONL**」，而 §6 的账本单位是"一次完整的 assistant
    /// 回复 + 该轮全部调用计划，一个逻辑事件"。把增量写进日志会得到一份按 token 切碎的
    /// 历史，恢复时拼不回一轮完整的调用；更要紧的是「未完成的流式模型输出没有执行
    /// 权限」——一个能被回放的增量事件正好模糊了这条线。
    ///
    /// 所以它是纯粹的界面提示：TUI 拿它做打字机效果，丢了不影响任何东西，一轮结束时
    /// `message.assistant` 会把完整回复正式送到。
    AssistantDelta {
        run: RunId,
        round: u32,
        text: String,
    },
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
    fn an_assistant_delta_is_an_sse_only_shape() {
        let frame = SseFrame {
            id: Seq(0),
            session: SessionId::from_raw("sess-1"),
            event: SseEvent::AssistantDelta {
                run: RunId::from_raw("run-1"),
                round: 1,
                text: "我算".into(),
            },
        };
        let text = serde_json::to_string(&frame).unwrap();
        assert!(text.contains(r#""event":"assistant_delta""#), "{text}");
        assert_eq!(serde_json::from_str::<SseFrame>(&text).unwrap(), frame);

        // 它在事件词汇里没有对应的 `type`：JSONL 那边读到只会是 Unknown。
        let as_log_line = r#"{"v":1,"seq":1,"event_id":"e","session_id":"s","at":"2026-09-15T08:00:00Z","type":"assistant_delta","data":{}}"#;
        let event = crate::events::Event::from_line(as_log_line).unwrap();
        assert!(
            event.payload.is_unknown(),
            "增量不是一个日志事件，账本里没有它的位置"
        );
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
