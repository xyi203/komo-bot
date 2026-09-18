//! `GET /v1/sessions/{id}/events`：按游标获取**或**订阅（§13.1）。
//!
//! 同一条路径两种读法，由 `Accept` 决定：
//!
//! - `Accept: text/event-stream` → SSE：**先按游标补读历史，再接直播**。补读来源是
//!   JSONL（`Ledger::read` 的短事务分页），内存广播只提示有新数据——丢了不丢数据
//!   （§8.8、§13.1）。
//! - 其余 → 一页 JSON（[`EventPage`]）。
//!
//! 游标两种写法都认：`?from=N` 与断线重连时浏览器自动带的 `Last-Event-ID`，后者优先
//! （重连时它更新）。

use std::convert::Infallible;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream, StreamExt};
use komo_kernel::protocol::http::{EventPage, EventQuery};
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::traits::Ledger;
use komo_kernel::types::ids::{Seq, SessionId};

use super::Api;
use super::error::ApiResult;
use crate::sse::{HEARTBEAT, cursor_from, encode_frame, heartbeat};

/// 一页最多多少条（`limit` 不给时）。
const PAGE: u32 = 500;

pub async fn stream(
    State(api): State<Api>,
    Path(id): Path<String>,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let session = SessionId::from_raw(id);
    let wants_stream = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/event-stream"));

    let from = cursor_from(
        headers
            .get("last-event-id")
            .and_then(|value| value.to_str().ok()),
        query.from,
    );

    if !wants_stream {
        let batch = api
            .state
            .routed
            .read(&session, from, query.limit.unwrap_or(PAGE))
            .await?;
        let next = batch.events.last().map(|event| event.seq).unwrap_or(from);
        return Ok(Json(EventPage {
            session: batch.session,
            events: batch.events,
            next,
            more: batch.next.is_some(),
        })
        .into_response());
    }

    // **先订阅再补读**：中间新写进来的事件才不会掉在两者之间。
    let live = api.state.hub.subscribe(&session);
    let history = backfill(&api, &session, from).await?;
    let last = history.last().map(|frame| frame.id).unwrap_or(from);

    let body = Body::from_stream(frames(history, live, last));
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .expect("SSE 响应"))
}

/// 从账本按游标补读（短事务分页）。
async fn backfill(api: &Api, session: &SessionId, from: Seq) -> ApiResult<Vec<SseFrame>> {
    let mut out = Vec::new();
    let mut cursor = from;
    loop {
        let batch = api.state.routed.read(session, cursor, PAGE).await?;
        if batch.events.is_empty() {
            return Ok(out);
        }
        for event in batch.events {
            cursor = cursor.max(event.seq);
            out.push(SseFrame {
                id: event.seq,
                session: session.clone(),
                event: SseEvent::Event(Box::new(event)),
            });
        }
        if batch.next.is_none() {
            return Ok(out);
        }
    }
}

/// 历史 → 直播 → 心跳。
fn frames(
    history: Vec<SseFrame>,
    live: tokio::sync::broadcast::Receiver<SseFrame>,
    last: Seq,
) -> impl Stream<Item = Result<String, Infallible>> {
    let past = stream::iter(history.into_iter().map(|frame| Ok(encode_frame(&frame))));

    let live = stream::unfold((live, last), |(mut live, mut last)| async move {
        loop {
            let tick = tokio::time::sleep(HEARTBEAT);
            tokio::pin!(tick);
            tokio::select! {
                frame = live.recv() => match frame {
                    Ok(frame) => {
                        // 补读已经给过的那些不再给一遍——**这条只对原始事件成立**。补读
                        // 只发 `SseEvent::Event`（`backfill` 造的那些），而派生帧
                        // （`run_status` / `approval_pending` / `approval_decided` /
                        // `assistant_delta`）与它们的源事件**同号**（`EventHub::publish_event`
                        // 一对多推），一帧都没被补读过。按号一律丢的话这四类**永远到不了
                        // 客户端**：原始帧先把号推上去，紧接着的派生帧就被当成重复吃掉。
                        // 后果之一是 `approval_pending` 到不了 TUI——它停在"等待审批"
                        // 不弹窗，审批在别处答完也不收窗。
                        if frame.id <= last && matches!(frame.event, SseEvent::Event(_)) {
                            continue;
                        }
                        last = last.max(frame.id);
                        return Some((Ok(encode_frame(&frame)), (live, last)));
                    }
                    // 落后了：不装作没事——客户端按 `Last-Event-ID` 重连就会补读。
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                },
                () = &mut tick => return Some((Ok(heartbeat()), (live, last))),
            }
        }
    });

    past.chain(live)
}

/// 让类型检查替我们证明这个流是能塞进 `Body::from_stream` 的。
#[allow(dead_code)]
fn assert_stream_shape() {
    fn is_stream<S: Stream<Item = Result<String, Infallible>> + Send>(_: S) {}
    let (_tx, rx) = tokio::sync::broadcast::channel(1);
    is_stream(frames(Vec::new(), rx, Seq::ZERO));
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use komo_kernel::events::{Event, EventPayload, RunCompleted};
    use komo_kernel::types::ids::{ApprovalId, EventId, RunId, ShortId};
    use time::macros::datetime;

    const SESSION: &str = "sess-1";

    fn completed(seq: u64) -> Event {
        Event {
            v: 1,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("evt-{seq}")),
            session: SessionId::from_raw(SESSION),
            run: Some(RunId::from_raw("run-1")),
            ts: datetime!(2026-09-16 08:00:00 UTC),
            payload: EventPayload::RunCompleted(RunCompleted {
                final_message: Some("好了".into()),
                final_message_ref: None,
                rounds: 1,
            }),
        }
    }

    fn raw(seq: u64) -> SseFrame {
        SseFrame {
            id: Seq(seq),
            session: SessionId::from_raw(SESSION),
            event: SseEvent::Event(Box::new(completed(seq))),
        }
    }

    /// 补读与直播的重叠只吃掉**重复的那一条原始事件**，不连它的派生帧一起吃掉。
    ///
    /// 派生帧与源事件同号（`EventHub::publish_event` 一对多推）。按号一律去重的话，
    /// 原始帧先把号推上去，紧随其后的派生帧就被当成重复——四类派生帧**一帧都到不了
    /// 客户端**：`approval_pending` 到不了 TUI，它就停在"等待审批"不弹窗。
    #[tokio::test]
    async fn a_derived_frame_is_not_deduped_away_by_its_own_event() {
        let (live, rx) = tokio::sync::broadcast::channel(8);
        // 补读已经给过 seq 7；直播里 seq 7 又来了一遍，后面跟着 seq 8。
        let history = vec![raw(7)];
        live.send(raw(7)).expect("推得进去");
        live.send(SseFrame {
            id: Seq(7),
            session: SessionId::from_raw(SESSION),
            event: SseEvent::ApprovalPending {
                approval: ApprovalId::from_raw("ap-1"),
                short_id: ShortId::from_index(3),
            },
        })
        .expect("推得进去");
        live.send(raw(8)).expect("推得进去");

        let out: Vec<String> = frames(history, rx, Seq(7))
            .take(3)
            .map(|item| item.expect("编码不会失败"))
            .collect()
            .await;
        assert!(out[0].starts_with("id: 7\nevent: event\n"), "{}", out[0]);
        assert!(
            out[1].starts_with("id: 7\nevent: approval_pending\n"),
            "派生帧被自己的事件吃掉了：{}",
            out[1]
        );
        assert!(out[1].contains(r#""short_id":"0003""#), "{}", out[1]);
        assert!(out[2].starts_with("id: 8\nevent: event\n"), "{}", out[2]);
    }
}
