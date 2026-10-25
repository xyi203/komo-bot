//! `GET /v1/sessions/{id}/events`（D5 W2b，`docs/storage-durable-d5.md` §5）。
//!
//! 路径、`Accept` 分派、游标写法（`?from=` 与 `Last-Event-ID`，头优先）沿用冻结的协议：
//!
//! - 非 `text/event-stream`：一页 JSON（[`EventPage`]），`seq > from` 的前 `limit` 条；
//! - `text/event-stream`：**先订阅 hub 再补读**，补读之后接直播。补读来自存储（投影与 feed 同一个
//!   纯函数，见 [`feed::backfill`]），直播来自 [`feed`] 推进 hub 的帧。两段重叠的部分靠 seq 去重
//!   （只对原始事件，见 [`already_sent`]），所以不重不漏。
//!
//! 直播连接落后（hub 的广播缓冲溢出）时，在连接上**当场补读**落下的那一段原始事件再接着直播
//! （部分回复与状态帧丢了只是晚一点知道）。

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::stream::{self, Stream, StreamExt};
use komo_kernel::durable::Storage;
use komo_kernel::protocol::http::{ErrorCode, EventPage, EventQuery};
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::types::ids::{Seq, SessionId};

use crate::http::Api;
use crate::http::error::{ApiFailure, ApiResult};
use crate::service::durable::SessionError;
use crate::service::durable::feed;
use crate::sse::{HEARTBEAT, already_sent, cursor_from, encode_frame, heartbeat};

/// 一页最多多少条（`limit` 不给时）。
const PAGE: usize = 500;

pub fn routes() -> Router<Api> {
    Router::new().route("/v1/sessions/{id}/events", get(stream))
}

fn failure(error: SessionError) -> ApiFailure {
    match error {
        SessionError::NotFound(what) => ApiFailure::not_found(what),
        SessionError::Invalid(message) => ApiFailure::invalid(message),
        other => ApiFailure::new(ErrorCode::Internal, other.to_string()),
    }
}

fn frame_of(event: komo_kernel::events::Event) -> SseFrame {
    SseFrame {
        id: event.seq,
        session: event.session.clone(),
        event: SseEvent::Event(Box::new(event)),
    }
}

async fn stream(
    State(api): State<Api>,
    Path(id): Path<String>,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let core = api.state.core()?;
    let storage = Arc::clone(&core.storage);
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
        let limit = query
            .limit
            .filter(|limit| *limit > 0)
            .map_or(PAGE, |limit| limit as usize);
        let mut events = feed::backfill(&*storage, &id, from)
            .await
            .map_err(failure)?;
        let more = events.len() > limit;
        events.truncate(limit);
        let next = events.last().map_or(from, |event| event.seq);
        return Ok(Json(EventPage {
            session: SessionId::from_raw(id),
            events,
            next,
            more,
        })
        .into_response());
    }

    // **先订阅再补读**：中间新推进来的帧才不会掉在两者之间。
    let session = SessionId::from_raw(id.clone());
    let live = api.state.hub.subscribe(&session);
    // 记一笔"有人在看"（介入的周期兜底靠它）；守卫跟着响应体走，连接断了自动减一。
    let watching = api.state.hub.watch(&session);
    let history: VecDeque<SseFrame> = feed::backfill(&*storage, &id, from)
        .await
        .map_err(failure)?
        .into_iter()
        .map(frame_of)
        .collect();

    let body = Body::from_stream(frames(storage, id, history, live, from).map(move |item| {
        let _ = &watching;
        item
    }));
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .expect("SSE 响应"))
}

struct State_ {
    storage: Arc<dyn Storage>,
    session: String,
    live: tokio::sync::broadcast::Receiver<SseFrame>,
    /// 还没交给客户端的帧（补读的历史，或落后之后补上的那一段）。
    queue: VecDeque<SseFrame>,
    /// 已经交给客户端的最大 seq。
    last: Seq,
}

/// 历史 → 直播 → 心跳。
fn frames(
    storage: Arc<dyn Storage>,
    session: String,
    history: VecDeque<SseFrame>,
    live: tokio::sync::broadcast::Receiver<SseFrame>,
    from: Seq,
) -> impl Stream<Item = Result<String, Infallible>> {
    let state = State_ {
        storage,
        session,
        live,
        queue: history,
        last: from,
    };
    stream::unfold(state, |mut state| async move {
        loop {
            if let Some(frame) = state.queue.pop_front() {
                state.last = state.last.max(frame.id);
                return Some((Ok(encode_frame(&frame)), state));
            }
            let tick = tokio::time::sleep(HEARTBEAT);
            tokio::pin!(tick);
            tokio::select! {
                frame = state.live.recv() => match frame {
                    Ok(frame) => {
                        if already_sent(&frame, state.last) {
                            continue;
                        }
                        state.last = state.last.max(frame.id);
                        return Some((Ok(encode_frame(&frame)), state));
                    }
                    // 落后了：当场把落下的原始事件从存储补上，再接着直播。
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        match feed::backfill(&*state.storage, &state.session, state.last).await {
                            Ok(events) => state.queue.extend(events.into_iter().map(frame_of)),
                            Err(error) => {
                                tracing::warn!(session = %state.session, %error, "落后之后补读失败");
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                },
                () = &mut tick => return Some((Ok(heartbeat()), state)),
            }
        }
    })
}
