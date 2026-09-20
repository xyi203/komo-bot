//! SSE 订阅与断线后的游标补读（§13.1）。
//!
//! 「SSE 事件带 Session 内递增序号，断线后按游标补读。」游标和序号是同一个数
//! （[`SseFrame::id`]），所以这里只需要记住**最后一个到手的 id**，重连时把它当
//! `?from=` 交回去。
//!
//! 三件事是这个模块存在的理由，各自是一段代码而不是一个注释：
//!
//! - **重连自己发生**，且从最后一个 id 续读——断了一秒钟就丢掉中间几条事件，会让消息
//!   面出现一个谁也看不见的洞。
//! - **重连状态可观察**（[`ConnectionState`] 的 watch 流）：状态行要能说"重连中"，而不是
//!   安静地停止更新。
//! - **解析容错**：未知 `event:` 类型跳过，**但游标照常前进**。跳过一帧却不推进游标，
//!   下一次重连会把它再取一遍，然后再跳过一次，永远卡在那里。

use std::time::Duration;

use futures_util::StreamExt;
use komo_kernel::protocol::sse::{Cursor, SseFrame};
use komo_kernel::types::ids::{Seq, SessionId};
use tokio::sync::{mpsc, watch};

use crate::api::KomoClient;

/// 订阅的连接状态。状态行读它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// 还没连上过。
    Connecting,
    Connected,
    /// 断了，正在退避重连。`attempt` 从 1 数起。
    Reconnecting {
        attempt: u32,
        reason: String,
    },
    /// 订阅结束了（调用方丢掉了句柄，或服务端明确拒绝）。
    Closed {
        reason: String,
    },
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionState::Connected)
    }

    /// 状态行上的一个短标签。
    pub fn label(&self) -> String {
        match self {
            ConnectionState::Connecting => "连接中".into(),
            ConnectionState::Connected => "已连接".into(),
            ConnectionState::Reconnecting { attempt, .. } => format!("重连中 #{attempt}"),
            ConnectionState::Closed { reason } => format!("已断开：{reason}"),
        }
    }
}

/// 订阅送出来的一条东西。
#[derive(Debug, Clone, PartialEq)]
pub enum SseMessage {
    Frame(Box<SseFrame>),
    /// 这个版本读不懂的一帧。**游标已经前进**，只是内容没法解释。
    Skipped {
        id: Seq,
        reason: String,
    },
}

impl SseMessage {
    /// 这一帧把游标推到哪里。
    pub fn cursor(&self) -> Seq {
        match self {
            SseMessage::Frame(frame) => frame.id,
            SseMessage::Skipped { id, .. } => *id,
        }
    }
}

/// 订阅的可调参数。测试把退避调小，正式构建用默认值。
#[derive(Debug, Clone)]
pub struct SseConfig {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// 心跳之间最长可以多久没有任何字节。到了就当连接死了主动重连。
    pub idle_timeout: Duration,
    /// 通道容量。
    pub buffer: usize,
}

impl Default for SseConfig {
    fn default() -> Self {
        SseConfig {
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(90),
            buffer: 256,
        }
    }
}

/// 一个活着的订阅。丢掉它就停止重连。
pub struct SseHandle {
    frames: mpsc::Receiver<SseMessage>,
    state: watch::Receiver<ConnectionState>,
    task: tokio::task::JoinHandle<()>,
}

impl SseHandle {
    /// 下一帧。`None` = 订阅结束。
    pub async fn recv(&mut self) -> Option<SseMessage> {
        self.frames.recv().await
    }

    /// 连接状态流。`changed()` 等下一次变化，`borrow()` 读当前值。
    pub fn state(&self) -> watch::Receiver<ConnectionState> {
        self.state.clone()
    }

    pub fn current_state(&self) -> ConnectionState {
        self.state.borrow().clone()
    }

    pub fn abort(self) {
        self.task.abort();
    }
}

/// 订阅 `GET /v1/sessions/{id}/events`，从 `cursor` **之后**读起。
pub fn subscribe(client: &KomoClient, session: SessionId, cursor: Cursor) -> SseHandle {
    subscribe_with(client, session, cursor, SseConfig::default())
}

pub fn subscribe_with(
    client: &KomoClient,
    session: SessionId,
    cursor: Cursor,
    config: SseConfig,
) -> SseHandle {
    let (frame_tx, frames) = mpsc::channel(config.buffer);
    let (state_tx, state) = watch::channel(ConnectionState::Connecting);
    // 客户端进任务里：**每次重连先按发现文件核对当前实例**（见 `run_subscription`）。
    let client = client.clone();

    let task = tokio::spawn(async move {
        run_subscription(client, session, cursor, config, frame_tx, state_tx).await;
    });

    SseHandle {
        frames,
        state,
        task,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_subscription(
    client: KomoClient,
    session: SessionId,
    mut cursor: Cursor,
    config: SseConfig,
    frames: mpsc::Sender<SseMessage>,
    state: watch::Sender<ConnectionState>,
) {
    let mut attempt: u32 = 0;
    let mut backoff = config.initial_backoff;

    loop {
        // **网关重启会换令牌**（发现文件里那个每次启动重新生成），端口也可能变。每次
        // 重连先按发现文件核对一次：实例换了就把地址与令牌一起换掉（§3 第 1–2 步）。
        // 少了这一步，重启之后客户端会攥着上一个实例的令牌无限重连——界面永远"重连中"。
        client.refresh().await;
        let http = client.http().clone();
        let url = client.url(&format!("/v1/sessions/{session}/events"));
        let token = client.token();

        match connect_once(
            &http,
            &url,
            token.as_deref(),
            cursor,
            &config,
            &frames,
            &state,
        )
        .await
        {
            // 服务端正常收尾（读完一段就关流）：重连，从新游标继续。
            Ok(next) => {
                cursor = next;
                attempt = 0;
                backoff = config.initial_backoff;
                let _ = state.send(ConnectionState::Reconnecting {
                    attempt: 1,
                    reason: "服务端关闭了事件流".into(),
                });
            }
            Err(StreamStop::Closed) => {
                // 订阅方走了。
                let _ = state.send(ConnectionState::Closed {
                    reason: "订阅已丢弃".into(),
                });
                return;
            }
            Err(StreamStop::Failed { at, reason }) => {
                cursor = at;
                attempt += 1;
                let _ = state.send(ConnectionState::Reconnecting {
                    attempt,
                    reason: reason.clone(),
                });
                tracing::debug!(attempt, %reason, from = cursor.after.0, "SSE 重连");
            }
        }
        // `Ok(next)` 的那一支走到这里 attempt 是 0，等一个最短退避即可。
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(config.max_backoff);
        if frames.is_closed() {
            let _ = state.send(ConnectionState::Closed {
                reason: "订阅已丢弃".into(),
            });
            return;
        }
    }
}

enum StreamStop {
    /// 接收端没了。
    Closed,
    Failed {
        at: Cursor,
        reason: String,
    },
}

/// 连一次，一直读到流结束或出错。返回值 / 错误里都带**走到哪个游标**。
async fn connect_once(
    http: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    mut cursor: Cursor,
    config: &SseConfig,
    frames: &mpsc::Sender<SseMessage>,
    state: &watch::Sender<ConnectionState>,
) -> Result<Cursor, StreamStop> {
    let mut request = http
        .get(format!("{url}{}", cursor_query(cursor)))
        .header("Accept", "text/event-stream")
        // 标准的 SSE 续读头，和 `?from=` 说的是同一件事；服务端认哪个都行。
        .header("Last-Event-ID", cursor.after.0.to_string());
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }

    let response = request.send().await.map_err(|e| StreamStop::Failed {
        at: cursor,
        reason: e.to_string(),
    })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = response.text().await.unwrap_or_default();
        return Err(StreamStop::Failed {
            at: cursor,
            reason: format!("HTTP {status}：{}", body.trim()),
        });
    }
    let _ = state.send(ConnectionState::Connected);

    let mut stream = response.bytes_stream();
    let mut parser = FrameParser::default();
    loop {
        let chunk = tokio::time::timeout(config.idle_timeout, stream.next()).await;
        let chunk = match chunk {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(e))) => {
                return Err(StreamStop::Failed {
                    at: cursor,
                    reason: e.to_string(),
                });
            }
            // 流正常结束。
            Ok(None) => return Ok(cursor),
            Err(_) => {
                return Err(StreamStop::Failed {
                    at: cursor,
                    reason: format!("{} 内没有任何字节（含心跳）", pretty(config.idle_timeout)),
                });
            }
        };
        for message in parser.push(&chunk) {
            cursor = Cursor::after(message.cursor());
            if frames.send(message).await.is_err() {
                return Err(StreamStop::Closed);
            }
        }
    }
}

fn cursor_query(cursor: Cursor) -> String {
    crate::api::query_string(&[("from", cursor.after.0.to_string())])
}

fn pretty(duration: Duration) -> String {
    format!("{}s", duration.as_secs())
}

/// SSE 的行解析：按空行切事件，收 `id:` / `event:` / `data:`。
///
/// 它是纯的——喂字节片，出解析结果——所以断线续读与容错都可以在没有网络的情况下测。
#[derive(Debug, Default)]
pub struct FrameParser {
    buffer: String,
    id: Option<u64>,
    event: Option<String>,
    data: String,
    /// 上一片字节可能切在一个 UTF-8 字符中间。
    pending: Vec<u8>,
}

impl FrameParser {
    /// 喂一片字节，取出这一片里读完的帧。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseMessage> {
        self.pending.extend_from_slice(chunk);
        // 尽量多地解出合法 UTF-8，剩下的留到下一片。
        let text = match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_string();
                self.pending.clear();
                text
            }
            Err(error) => {
                let valid = error.valid_up_to();
                let text = String::from_utf8_lossy(&self.pending[..valid]).into_owned();
                self.pending.drain(..valid);
                text
            }
        };
        self.buffer.push_str(&text);

        let mut out = Vec::new();
        while let Some(index) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=index).collect();
            let line = line.trim_end_matches(['\n', '\r']).to_string();
            if let Some(message) = self.line(&line) {
                out.push(message);
            }
        }
        out
    }

    fn line(&mut self, line: &str) -> Option<SseMessage> {
        if line.is_empty() {
            return self.dispatch();
        }
        // 注释行（`:` 开头）——有的代理靠它保活。
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "id" => self.id = value.trim().parse().ok(),
            "event" => self.event = Some(value.trim().to_string()),
            "data" => {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseMessage> {
        let id = self.id.take();
        let event = self.event.take();
        let data = std::mem::take(&mut self.data);
        if data.is_empty() && id.is_none() {
            return None;
        }
        match serde_json::from_str::<SseFrame>(&data) {
            Ok(frame) => Some(SseMessage::Frame(Box::new(frame))),
            Err(error) => {
                // 读不懂就跳过，**但游标要走**。没有 `id:` 行就真的什么也做不了：
                // 那一帧既不能解释也不能定位，只能丢掉。
                let id = id?;
                Some(SseMessage::Skipped {
                    id: Seq(id),
                    reason: match event {
                        Some(name) => format!("event={name}：{error}"),
                        None => error.to_string(),
                    },
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::protocol::sse::SseEvent;
    use komo_kernel::types::ids::RunId;
    use komo_kernel::types::status::RunState;

    fn frame(id: u64) -> SseFrame {
        SseFrame {
            id: Seq(id),
            session: SessionId::from_raw("sess-1"),
            event: SseEvent::RunStatus {
                run: RunId::from_raw("run-1"),
                state: RunState::Running,
            },
        }
    }

    fn wire(frame: &SseFrame, event: &str) -> String {
        format!(
            "id: {}\nevent: {event}\ndata: {}\n\n",
            frame.id.0,
            serde_json::to_string(frame).unwrap()
        )
    }

    #[test]
    fn a_whole_frame_parses() {
        let mut parser = FrameParser::default();
        let out = parser.push(wire(&frame(7), "run_state").as_bytes());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cursor(), Seq(7));
        assert_eq!(out[0], SseMessage::Frame(Box::new(frame(7))));
    }

    #[test]
    fn a_frame_split_across_chunks_parses_once_it_is_whole() {
        let text = wire(&frame(9), "run_state");
        let (head, tail) = text.split_at(text.len() / 2);
        let mut parser = FrameParser::default();
        assert!(parser.push(head.as_bytes()).is_empty());
        let out = parser.push(tail.as_bytes());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cursor(), Seq(9));
    }

    #[test]
    fn a_chunk_boundary_inside_a_multibyte_character_does_not_corrupt_it() {
        let mut frame = frame(11);
        frame.session = SessionId::from_raw("会话");
        let text = wire(&frame, "run_state");
        let bytes = text.as_bytes();
        // 切在 "会" 的中间。
        let cut = text.find("会").unwrap() + 1;
        let mut parser = FrameParser::default();
        parser.push(&bytes[..cut]);
        let out = parser.push(&bytes[cut..]);
        assert_eq!(out.len(), 1);
        let SseMessage::Frame(parsed) = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!(parsed.session.as_str(), "会话");
    }

    #[test]
    fn an_unknown_event_type_is_skipped_and_the_cursor_still_moves() {
        let mut parser = FrameParser::default();
        let out = parser.push(
            b"id: 12\nevent: something_new\ndata: {\"id\":12,\"session\":\"s\",\"event\":\"something_new\",\"data\":{}}\n\n",
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], SseMessage::Skipped { id: Seq(12), .. }));
        // 游标照常前进——不然重连会把这一帧再取一遍，永远卡在这里。
        assert_eq!(out[0].cursor(), Seq(12));
    }

    #[test]
    fn a_heartbeat_does_not_look_like_an_unknown_frame() {
        let heartbeat = SseFrame {
            id: Seq(13),
            session: SessionId::from_raw("sess-1"),
            event: SseEvent::Heartbeat,
        };
        let mut parser = FrameParser::default();
        let out = parser.push(wire(&heartbeat, "heartbeat").as_bytes());
        assert_eq!(out, vec![SseMessage::Frame(Box::new(heartbeat))]);
    }

    #[test]
    fn a_comment_line_keeps_the_connection_alive_without_producing_a_frame() {
        let mut parser = FrameParser::default();
        assert!(parser.push(b": keep-alive\n\n").is_empty());
    }

    #[test]
    fn several_frames_in_one_chunk_all_come_out_in_order() {
        let text = format!(
            "{}{}{}",
            wire(&frame(1), "run_state"),
            wire(&frame(2), "run_state"),
            wire(&frame(3), "run_state")
        );
        let mut parser = FrameParser::default();
        let out = parser.push(text.as_bytes());
        let cursors: Vec<_> = out.iter().map(|m| m.cursor()).collect();
        assert_eq!(cursors, vec![Seq(1), Seq(2), Seq(3)]);
    }

    #[test]
    fn the_cursor_query_names_the_seq_we_already_have() {
        assert_eq!(cursor_query(Cursor::after(Seq(41))), "?from=41");
        assert_eq!(cursor_query(Cursor::default(),), "?from=0");
    }

    #[test]
    fn a_reconnecting_state_says_which_attempt() {
        let state = ConnectionState::Reconnecting {
            attempt: 3,
            reason: "connection reset".into(),
        };
        assert_eq!(state.label(), "重连中 #3");
        assert!(!state.is_connected());
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use crate::test_server::{FakeGateway, Reply};
    use komo_kernel::protocol::sse::SseEvent;
    use komo_kernel::types::ids::RunId;
    use komo_kernel::types::status::RunState;

    fn frame_chunk(id: u64) -> String {
        let frame = SseFrame {
            id: Seq(id),
            session: SessionId::from_raw("sess-1"),
            event: SseEvent::RunStatus {
                run: RunId::from_raw("run-1"),
                state: RunState::Running,
            },
        };
        format!(
            "id: {id}\nevent: run_state\ndata: {}\n\n",
            serde_json::to_string(&frame).unwrap()
        )
    }

    fn fast() -> SseConfig {
        SseConfig {
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
            idle_timeout: Duration::from_secs(5),
            buffer: 64,
        }
    }

    /// **网关重启之后，事件订阅要自己跟过去。**
    ///
    /// 令牌每次启动重新生成、地址也可能变：重连时先按发现文件核对一次当前实例，否则
    /// 界面会停在"重连中"直到人手动重开（线上就是这么卡住的）。
    #[tokio::test]
    async fn a_subscription_follows_a_restarted_gateway() {
        let home = tempfile::tempdir().expect("临时数据目录");
        let data_dir = home.path().to_string_lossy().to_string();

        // 老实例：吐一帧，然后就"下线"（下面的 `drop`）。
        //
        // **别看序号判"第一次连接"**：`ordinal` 数的是这个假服务端收到的**所有**请求，
        // 而 `discover` 会先打一次 `/healthz`——按 `ordinal == 0` 判的话，真正的 SSE
        // 请求会被当成第二次，测试就卡在第一帧上。
        let old_token = "old-token";
        let old_dir = data_dir.clone();
        let old = FakeGateway::spawn(move |request, _| {
            if request.path.contains("/healthz") {
                return Reply::ok(serde_json::json!({
                    "instance_id": "inst-1",
                    "version": "0.8.0",
                    "protocol_version": komo_kernel::protocol::PROTOCOL_VERSION,
                    "started_at": "2026-09-19T00:00:00Z",
                    "data_dir": old_dir
                }));
            }
            Reply::Sse(vec![frame_chunk(41)])
        })
        .await;

        // 新实例：认新令牌，接着吐下一帧。`/healthz` 要报**同一个数据目录**，否则
        // `discover` 会把它判成"不是同一个实例"，刷新永远不成功。
        let new_dir = data_dir.clone();
        let new = FakeGateway::spawn(move |request, _| {
            if request.path.contains("/healthz") {
                return Reply::ok(serde_json::json!({
                    "instance_id": "inst-2",
                    "version": "0.8.0",
                    "protocol_version": komo_kernel::protocol::PROTOCOL_VERSION,
                    "started_at": "2026-09-19T00:00:00Z",
                    "data_dir": new_dir
                }));
            }
            Reply::Sse(vec![frame_chunk(42)])
        })
        .await;

        write_discovery(&home, &old.base_url(), "inst-1", old_token);
        let client = crate::discovery::discover(home.path())
            .await
            .expect("发现得了")
            .client()
            .expect("建得出");
        let mut subscription = subscribe_with(
            &client,
            SessionId::from_raw("sess-1"),
            Cursor::after(Seq(40)),
            fast(),
        );
        // 第一帧也带时限：接不上就**失败并说清楚**，不是把测试挂死。
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), subscription.recv())
            .await
            .expect("第一帧就该到（超时说明客户端的第一次连接没接上）")
            .unwrap();
        assert_eq!(first.cursor(), Seq(41));

        // "重启"：老实例下线，发现文件指向新实例。
        drop(old);
        write_discovery(&home, &new.base_url(), "inst-2", "new-token");

        // 重连时按发现文件跟过去，续读到新实例的帧。**带时限**：跟不过去时这条会
        // 失败并说清楚卡在哪，而不是把测试挂死。
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), subscription.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "5 秒内没有接上新实例：客户端还指着 {}，发现文件说 {}，新实例收到 {} 条请求",
                    client.base_url(),
                    new.base_url(),
                    new.requests().len()
                )
            });
        assert_eq!(next.unwrap().cursor(), Seq(42));
        assert_eq!(client.base_url(), new.base_url(), "地址跟着换");
    }

    /// 发现文件那一行的样子（`runtime/gateway.json`）——与 `api.rs` 的测试同一份形状。
    fn write_discovery(home: &tempfile::TempDir, base_url: &str, instance: &str, token: &str) {
        let dir = home.path().join("runtime");
        std::fs::create_dir_all(&dir).expect("建 runtime/");
        let body = serde_json::json!({
            "instance_id": instance,
            "base_url": base_url,
            "protocol_version": komo_kernel::protocol::PROTOCOL_VERSION,
            "version": "0.8.0",
            "pid": 4242,
            "data_dir": home.path().to_string_lossy(),
            "token": token,
            "started_at": "2026-09-19T00:00:00Z"
        });
        std::fs::write(dir.join("gateway.json"), body.to_string()).expect("写发现文件");
    }

    #[tokio::test]
    async fn a_dropped_connection_reconnects_from_the_last_frame_id() {
        // 第一次连接吐两帧就断；之后每次只吐一帧。
        let server = FakeGateway::spawn(|_, ordinal| {
            if ordinal == 0 {
                Reply::Sse(vec![frame_chunk(41), frame_chunk(42)])
            } else {
                Reply::Sse(vec![frame_chunk(43)])
            }
        })
        .await;

        let client = server.client();
        let mut subscription = subscribe_with(
            &client,
            SessionId::from_raw("sess-1"),
            Cursor::after(Seq(40)),
            fast(),
        );

        assert_eq!(subscription.recv().await.unwrap().cursor(), Seq(41));
        assert_eq!(subscription.recv().await.unwrap().cursor(), Seq(42));
        // 断线之后自己重连，并且从 42 之后续读。
        assert_eq!(subscription.recv().await.unwrap().cursor(), Seq(43));

        let requests = server.requests();
        assert!(requests.len() >= 2, "应当重连过：{requests:?}");
        assert_eq!(
            requests[0].param("from").as_deref(),
            Some("40"),
            "第一次从调用方给的游标读起"
        );
        assert_eq!(
            requests[1].param("from").as_deref(),
            Some("42"),
            "第二次从最后一个 SseFrame.id 续读"
        );
        // 标准的续读头说的是同一件事。
        assert_eq!(requests[1].header("last-event-id"), Some("42"));
        assert_eq!(requests[1].header("accept"), Some("text/event-stream"));
    }

    #[tokio::test]
    async fn an_open_stream_is_reported_as_connected() {
        let server = FakeGateway::spawn(|_, _| Reply::SseOpen(vec![frame_chunk(1)])).await;
        let client = server.client();
        let subscription = subscribe_with(
            &client,
            SessionId::from_raw("sess-1"),
            Cursor::default(),
            fast(),
        );
        let mut state = subscription.state();
        let mut connected = false;
        for _ in 0..20 {
            if state.borrow_and_update().is_connected() {
                connected = true;
                break;
            }
            let _ = tokio::time::timeout(Duration::from_millis(200), state.changed()).await;
        }
        assert!(connected, "服务端已接受订阅，状态行要显示已连接");
    }

    #[tokio::test]
    async fn the_reconnect_is_observable_on_the_state_stream() {
        let server = FakeGateway::spawn(|_, ordinal| {
            if ordinal == 0 {
                Reply::Sse(vec![frame_chunk(1)])
            } else {
                // 之后一直挂起：让状态停在「重连中」好断言。
                Reply::Sse(vec![])
            }
        })
        .await;
        let client = server.client();
        let subscription = subscribe_with(
            &client,
            SessionId::from_raw("sess-1"),
            Cursor::default(),
            fast(),
        );
        let mut state = subscription.state();
        let mut seen_reconnect = false;
        for _ in 0..20 {
            if matches!(
                &*state.borrow_and_update(),
                ConnectionState::Reconnecting { .. }
            ) {
                seen_reconnect = true;
                break;
            }
            let _ = tokio::time::timeout(Duration::from_millis(200), state.changed()).await;
        }
        assert!(seen_reconnect, "状态行要看得见重连");
    }

    #[tokio::test]
    async fn a_refused_subscription_retries_rather_than_giving_up() {
        let server = FakeGateway::spawn(|_, ordinal| {
            if ordinal == 0 {
                Reply::error(503, "internal", "还没准备好")
            } else {
                Reply::Sse(vec![frame_chunk(5)])
            }
        })
        .await;
        let client = server.client();
        let mut subscription = subscribe_with(
            &client,
            SessionId::from_raw("sess-1"),
            Cursor::default(),
            fast(),
        );
        assert_eq!(subscription.recv().await.unwrap().cursor(), Seq(5));
        assert!(server.requests().len() >= 2);
    }

    #[tokio::test]
    async fn an_unreadable_frame_does_not_stall_the_cursor_across_a_reconnect() {
        let server = FakeGateway::spawn(|_, ordinal| {
            if ordinal == 0 {
                Reply::Sse(vec![
                    "id: 9\nevent: from_the_future\ndata: {\"nope\":true}\n\n".into(),
                ])
            } else {
                Reply::Sse(vec![frame_chunk(10)])
            }
        })
        .await;
        let client = server.client();
        let mut subscription = subscribe_with(
            &client,
            SessionId::from_raw("sess-1"),
            Cursor::default(),
            fast(),
        );
        let first = subscription.recv().await.unwrap();
        assert!(matches!(first, SseMessage::Skipped { id: Seq(9), .. }));
        assert_eq!(subscription.recv().await.unwrap().cursor(), Seq(10));
        // 重连没有把那一帧再取一遍。
        assert_eq!(server.requests()[1].param("from").as_deref(), Some("9"));
    }
}
