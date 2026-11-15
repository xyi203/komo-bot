//! `GET /v1/sessions/{id}/events`（D5 W2b，`docs/storage-durable-d5.md` §5）。
//!
//! 路径、`Accept` 分派、游标写法（`?from=` 与 `Last-Event-ID`，头优先）沿用冻结的协议：
//!
//! - 非 `text/event-stream`：一页 JSON（[`EventPage`]），`seq > from` 的前 `limit` 条；`?lean=true` 时去掉
//!   只画对话的客户端不读的大字段（[`lean`]）；
//! - `text/event-stream`：**先订阅 hub 再补读**，补读之后接直播。补读先看 feed 在内存里留的最近事件，
//!   覆盖不到才读存储（投影与 feed 同一个纯函数，见 [`feed::backfill_recent`]），直播来自 [`feed`] 推进
//!   hub 的帧。两段重叠的部分靠 seq 去重
//!   （只对原始事件，见 [`already_sent`]），所以不重不漏。
//!
//! 直播连接落后（hub 的广播缓冲溢出）时，在连接上**当场补读**落下的那一段原始事件再接着直播
//! （部分回复与状态帧丢了只是晚一点知道）。
//!
//! codemode 的子调用（`docs/codemode-bash.md` §3.5）：补读里跑完的由 `tool.result` 带着（与直播同一份），
//! 补读之后再跟一帧还在跑的那些的 `nested_calls` 快照（[`nested_view::snapshot`]），重连时的画面与
//! 一直连着的一样。

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
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::http::{ErrorCode, EventPage, EventQuery};
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::types::ids::{Seq, SessionId};
use serde_json::Value;

use crate::http::Api;
use crate::http::error::{ApiFailure, ApiResult};
use crate::service::durable::feed::{self, nested_view};
use crate::service::durable::{DurableCore, SessionError};
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

/// 补读之后的子调用快照帧：`id` 取已经交出的最大 seq，不动游标。
async fn nested_frames(storage: &dyn Storage, session: &str, last: Seq) -> Vec<SseFrame> {
    nested_view::snapshot(storage, session)
        .await
        .into_iter()
        .map(|frame| SseFrame {
            id: last,
            session: SessionId::from_raw(session),
            event: SseEvent::NestedCalls {
                run: frame.run,
                call: frame.call,
                calls: frame.calls,
            },
        })
        .collect()
}

fn frame_of(event: Event) -> SseFrame {
    SseFrame {
        id: event.seq,
        session: event.session.clone(),
        event: SseEvent::Event(Box::new(event)),
    }
}

/// `?lean=true` 的一条事件：`tool.result` 去掉 `body`（完整的模型视图，一条最多 30 KiB；1 KiB 的
/// `preview` 留着），`message.assistant` 的 `provider_blocks` 只留推理块（其余与 `text` /
/// `tool_calls` 重复）。
fn lean(event: &mut Event) {
    match &mut event.payload {
        EventPayload::ToolResult(result) => result.body = None,
        EventPayload::MessageAssistant(message) => {
            if let Some(Value::Array(blocks)) = &mut message.provider_blocks {
                blocks.retain(|block| {
                    matches!(
                        block.get("type").and_then(Value::as_str),
                        Some("reasoning" | "thinking")
                    )
                });
            }
        }
        _ => {}
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
    let sessions_dir = api.state.snapshot().paths.sessions_dir.clone();
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
        let mut events = feed::backfill_recent(core, Some(&sessions_dir), &id, from)
            .await
            .map_err(failure)?;
        let more = events.len() > limit;
        events.truncate(limit);
        if query.lean == Some(true) {
            events.iter_mut().for_each(lean);
        }
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
    let mut history: VecDeque<SseFrame> =
        feed::backfill_recent(core, Some(&sessions_dir), &id, from)
            .await
            .map_err(failure)?
            .into_iter()
            .map(frame_of)
            .collect();
    let last = history.back().map_or(from, |frame| frame.id);
    history.extend(nested_frames(&*storage, &id, last).await);

    let stream = frames(Arc::clone(core), sessions_dir, id, history, live, from);
    let body = Body::from_stream(stream.map(move |item| {
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
    core: Arc<DurableCore>,
    sessions_dir: std::path::PathBuf,
    session: String,
    live: tokio::sync::broadcast::Receiver<SseFrame>,
    /// 还没交给客户端的帧（补读的历史，或落后之后补上的那一段）。
    queue: VecDeque<SseFrame>,
    /// 已经交给客户端的最大 seq。
    last: Seq,
}

/// 历史 → 直播 → 心跳。
fn frames(
    core: Arc<DurableCore>,
    sessions_dir: std::path::PathBuf,
    session: String,
    history: VecDeque<SseFrame>,
    live: tokio::sync::broadcast::Receiver<SseFrame>,
    from: Seq,
) -> impl Stream<Item = Result<String, Infallible>> {
    let state = State_ {
        core,
        sessions_dir,
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
                        let dir = Some(state.sessions_dir.as_path());
                        match feed::backfill_recent(&state.core, dir, &state.session, state.last)
                            .await
                        {
                            Ok(events) => {
                                state.queue.extend(events.into_iter().map(frame_of));
                                let last = state.queue.back().map_or(state.last, |frame| frame.id);
                                let snapshot =
                                    nested_frames(&*state.core.storage, &state.session, last).await;
                                state.queue.extend(snapshot);
                            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, data: &str) -> Event {
        Event::from_line(&format!(
            r#"{{"v":1,"seq":1,"event_id":"evt-1","session_id":"sess-1","run_id":"run-1","at":"2026-09-15T08:00:02Z","type":"{kind}","data":{data}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn lean_drops_the_tool_result_body_and_keeps_the_preview() {
        let mut event = event(
            "tool.result",
            r#"{"body":{"status":"completed","result":null,"preview":"全文"},"call_id":"call-7","attempt_id":"attempt-1","status":"completed","output_ref":{"path":"tool-output/run-1/call-7/attempt-1/output.json","size":12,"hash":"deadbeef"},"preview":"预览"}"#,
        );
        lean(&mut event);
        let EventPayload::ToolResult(result) = &event.payload else {
            panic!()
        };
        assert!(result.body.is_none());
        assert_eq!(result.preview.as_deref(), Some("预览"));
    }

    #[test]
    fn lean_keeps_only_the_reasoning_blocks_of_an_assistant_message() {
        let mut event = event(
            "message.assistant",
            r#"{"round":3,"text":"答","provider_blocks":[{"type":"reasoning","summary":[{"text":"想"}]},{"type":"message","content":[{"text":"答"}]},{"type":"thinking","thinking":"想"},{"type":"function_call","arguments":"{}"}]}"#,
        );
        lean(&mut event);
        let EventPayload::MessageAssistant(message) = &event.payload else {
            panic!()
        };
        let kinds: Vec<_> = message
            .provider_blocks
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["reasoning", "thinking"]);
        assert_eq!(message.text.as_deref(), Some("答"));
    }
}
