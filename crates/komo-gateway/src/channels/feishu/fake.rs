//! 测试用的假开放平台：一个真的 HTTP 服务端，跑在 loopback 上。
//!
//! 为什么是真服务端而不是给 [`FeishuApi`] 挖一个接缝：token 缓存这件事的行为全在"什么
//! 时候真的去取一张"上，而那只有在请求真的发出去之后才看得见。同理，"决定之后 PATCH 的
//! 是哪一条消息"要的是那条 `message_id` 的**来回**，不是一个由替身编出来的值。
//!
//! ws 那一侧没有替身：openlark 的协议实现不在我们手里，本机也没有凭证。`serve` 的事件
//! 入口做成了一个可注入的 channel（`FeishuChannel::injected`），测试直接塞负载。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::traits::{GatewayError, Inbound};
use komo_kernel::types::ids::{RunId, SessionId};

use super::api::{FeishuApi, ThreadMessage};

/// 假平台上机器人自己的 open_id。
pub const BOT_OPEN_ID: &str = "ou_komo_bot";

/// 假平台上机器人所在的租户；`text_event` 里的发送者默认也在这个租户。
pub const APP_TENANT: &str = "tenant_app";

/// 一串按发生顺序记下来的事件名。断言"谁在谁之后"用它。
#[derive(Clone, Default)]
pub struct EventLog(Arc<Mutex<Vec<String>>>);

impl EventLog {
    pub fn push(&self, event: impl Into<String>) {
        self.0.lock().expect("事件日志").push(event.into());
    }

    pub fn entries(&self) -> Vec<String> {
        self.0.lock().expect("事件日志").clone()
    }

    /// 第一次出现的位置。
    pub fn position(&self, event: &str) -> Option<usize> {
        self.entries().iter().position(|e| e == event)
    }
}

/// 假服务端的脾气。
#[derive(Debug, Clone, Default)]
pub struct Behavior {
    /// 取 token 一律 `code: 99991663`（app secret 不对）。
    pub refuse_token: bool,
    /// 发消息一律被拒。
    pub refuse_sends: bool,
    /// PATCH 一律被拒（Run 卡片换不成结果）。
    pub refuse_patch: bool,
    /// token 的 `expire` 只给 1 秒。
    pub short_lived_token: bool,
    /// 回复一律被拒（处理中卡片发不出去）。
    pub refuse_replies: bool,
    /// 加表情一律被拒。
    pub refuse_reactions: bool,
    /// 这些会话是外部群（`chats/{id}` 返回 `external: true`）。
    pub external_chats: Vec<String>,
    /// 查群信息一律被拒。
    pub refuse_chat_info: bool,
    /// 每次回复先睡这么久——让"终态先于卡片"变得可复现。
    pub reply_delay: Duration,
    /// 读话题消息一律被拒（没有 `im:message` 读权限之类）。
    pub refuse_thread_list: bool,
    /// 读话题消息先睡这么久——让超时变得可复现。
    pub thread_list_delay: Duration,
}

impl Behavior {
    pub fn refuse_token() -> Self {
        Self {
            refuse_token: true,
            ..Self::default()
        }
    }

    pub fn refuse_sends() -> Self {
        Self {
            refuse_sends: true,
            ..Self::default()
        }
    }

    pub fn refuse_patch() -> Self {
        Self {
            refuse_patch: true,
            ..Self::default()
        }
    }

    pub fn short_lived_token() -> Self {
        Self {
            short_lived_token: true,
            ..Self::default()
        }
    }

    pub fn refuse_replies() -> Self {
        Self {
            refuse_replies: true,
            ..Self::default()
        }
    }

    pub fn refuse_reactions() -> Self {
        Self {
            refuse_reactions: true,
            ..Self::default()
        }
    }

    pub fn external_chats(chats: &[&str]) -> Self {
        Self {
            external_chats: chats.iter().map(|chat| chat.to_string()).collect(),
            ..Self::default()
        }
    }

    pub fn refuse_chat_info() -> Self {
        Self {
            refuse_chat_info: true,
            ..Self::default()
        }
    }

    pub fn refuse_thread_list() -> Self {
        Self {
            refuse_thread_list: true,
            ..Self::default()
        }
    }

    pub fn slow_thread_list(delay: Duration) -> Self {
        Self {
            thread_list_delay: delay,
            ..Self::default()
        }
    }

    pub fn slow_replies(delay: Duration) -> Self {
        Self {
            reply_delay: delay,
            ..Self::default()
        }
    }
}

struct FakeState {
    behavior: Behavior,
    calls: Mutex<Vec<(String, Value)>>,
    patched: Mutex<Vec<String>>,
    log: EventLog,
    next_message_id: AtomicI64,
    /// 消息里的图片：`image_key` → 字节。
    resources: Mutex<HashMap<String, Vec<u8>>>,
    /// 话题里的消息：`thread_id` → 按时间正序。
    threads: Mutex<HashMap<String, Vec<ThreadMessage>>>,
}

/// 一个跑起来的假开放平台。
pub struct FakeOpenApi {
    addr: SocketAddr,
    state: Arc<FakeState>,
    task: JoinHandle<()>,
}

impl Drop for FakeOpenApi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeOpenApi {
    pub async fn start(behavior: Behavior) -> Self {
        let state = Arc::new(FakeState {
            behavior,
            calls: Mutex::new(Vec::new()),
            patched: Mutex::new(Vec::new()),
            log: EventLog::default(),
            next_message_id: AtomicI64::new(1000),
            resources: Mutex::new(HashMap::new()),
            threads: Mutex::new(HashMap::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定 loopback");
        let addr = listener.local_addr().expect("本地地址");
        let app = Router::new()
            .route(
                "/open-apis/im/v1/messages/{message_id}/resources/{key}",
                axum::routing::get(resource),
            )
            .fallback(handle)
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, state, task }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 指向这个假服务端的 [`FeishuApi`]。
    pub fn api(&self) -> Arc<FeishuApi> {
        Arc::new(FeishuApi::with_endpoint(
            &self.endpoint(),
            "cli_test",
            "test-app-secret",
            reqwest::Client::new(),
        ))
    }

    pub fn log(&self) -> EventLog {
        self.state.log.clone()
    }

    /// 某个接口收到的每一个请求体，按顺序。
    pub fn calls_to(&self, what: &str) -> Vec<Value> {
        self.state
            .calls
            .lock()
            .expect("调用记录")
            .iter()
            .filter(|(name, _)| name == what)
            .map(|(_, body)| body.clone())
            .collect()
    }

    /// 放一张图，`download_image` 按 `image_key` 取得到它。
    pub fn host_image(&self, image_key: &str, bytes: &[u8]) {
        self.state
            .resources
            .lock()
            .expect("图片")
            .insert(image_key.to_string(), bytes.to_vec());
    }

    /// 放一个话题的全部消息（按时间正序），`list_thread_messages` 按倒序分页读它。
    pub fn host_thread(&self, thread_id: &str, items: Vec<ThreadMessage>) {
        self.state
            .threads
            .lock()
            .expect("话题")
            .insert(thread_id.to_string(), items);
    }

    /// 被 PATCH 过的 `message_id`，按顺序。
    pub fn patched_message_ids(&self) -> Vec<String> {
        self.state.patched.lock().expect("PATCH 记录").clone()
    }
}

/// 下载图片。日志记 `resource:{message_id}/{key}`。不存在时像真平台一样回一个 JSON 信封。
async fn resource(
    State(state): State<Arc<FakeState>>,
    Path((message_id, key)): Path<(String, String)>,
) -> axum::response::Response {
    state.log.push(format!("resource:{message_id}/{key}"));
    match state.resources.lock().expect("图片").get(&key) {
        Some(bytes) => bytes.clone().into_response(),
        None => (
            StatusCode::BAD_REQUEST,
            Json(refuse(234003, "File not in msg.")),
        )
            .into_response(),
    }
}

async fn handle(
    State(state): State<Arc<FakeState>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Json<Value> {
    let path = uri.path().to_string();
    let last = path.rsplit('/').next().unwrap_or_default().to_string();
    let body: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));

    let reply = if path.ends_with("/auth/v3/tenant_access_token/internal") {
        state.record("tenant_access_token", &body);
        if state.behavior.refuse_token {
            refuse(99991663, "app secret 不对")
        } else {
            json!({
                "code": 0,
                "msg": "ok",
                "tenant_access_token": "t-fake",
                "expire": if state.behavior.short_lived_token { 1 } else { 7200 },
            })
        }
    } else if path.ends_with("/bot/v3/info") {
        state.record("bot", &body);
        json!({
            "code": 0,
            "msg": "ok",
            "bot": { "open_id": BOT_OPEN_ID, "app_name": "komo" },
        })
    } else if method == Method::PATCH {
        state.record("patch", &body);
        state.patched.lock().expect("PATCH 记录").push(last.clone());
        if state.behavior.refuse_patch {
            refuse(230098, "message is not a card")
        } else {
            json!({ "code": 0, "msg": "ok", "data": {} })
        }
    } else if path.ends_with("/reply") {
        // 记下来的请求体多一个 `target`：被回复的是哪一条。
        state.record("reply", &with_target(&body, &path));
        if !state.behavior.reply_delay.is_zero() {
            tokio::time::sleep(state.behavior.reply_delay).await;
        }
        if state.behavior.refuse_replies {
            refuse(230002, "bot can not reply this message")
        } else {
            let id = state.next_message_id.fetch_add(1, Ordering::SeqCst);
            json!({ "code": 0, "msg": "ok", "data": { "message_id": format!("om_{id}") } })
        }
    } else if path.ends_with("/reactions") {
        state.record("reactions", &with_target(&body, &path));
        if state.behavior.refuse_reactions {
            refuse(231001, "reaction type is invalid")
        } else {
            json!({ "code": 0, "msg": "ok", "data": { "reaction_id": "r_1" } })
        }
    } else if path.contains("/im/v1/chats/") {
        state.record("chat_info", &body);
        if state.behavior.refuse_chat_info {
            refuse(232011, "no permission to read the chat")
        } else {
            let external = state.behavior.external_chats.contains(&last);
            json!({ "code": 0, "msg": "ok", "data": { "name": "群", "external": external } })
        }
    } else if method == Method::GET && path.ends_with("/im/v1/messages") {
        let query = parse_query(uri.query().unwrap_or_default());
        state.record("thread_list", &json!(query));
        if !state.behavior.thread_list_delay.is_zero() {
            tokio::time::sleep(state.behavior.thread_list_delay).await;
        }
        if state.behavior.refuse_thread_list {
            refuse(230027, "Lack of necessary permissions")
        } else {
            thread_page(&state, &query)
        }
    } else if path.ends_with("/im/v1/messages") {
        state.record("messages", &body);
        if state.behavior.refuse_sends {
            refuse(230001, "bot is not in the chat")
        } else {
            let id = state.next_message_id.fetch_add(1, Ordering::SeqCst);
            json!({ "code": 0, "msg": "ok", "data": { "message_id": format!("om_{id}") } })
        }
    } else {
        refuse(404, "no such fake endpoint")
    };
    Json(reply)
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// 话题的一页：`ByCreateTimeDesc` 倒序，`page_token` 就是下一页从第几条开始。
fn thread_page(state: &FakeState, query: &HashMap<String, String>) -> Value {
    let threads = state.threads.lock().expect("话题");
    let mut items: Vec<&ThreadMessage> = threads
        .get(query.get("container_id").map_or("", String::as_str))
        .map(|items| items.iter().collect())
        .unwrap_or_default();
    if query.get("sort_type").map(String::as_str) == Some("ByCreateTimeDesc") {
        items.reverse();
    }
    let size: usize = query
        .get("page_size")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let start: usize = query
        .get("page_token")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let end = (start + size).min(items.len());
    let page = &items[start.min(end)..end];
    let mut data = json!({ "items": page, "has_more": end < items.len() });
    if end < items.len() {
        data["page_token"] = json!(end.to_string());
    }
    json!({ "code": 0, "msg": "success", "data": data })
}

impl FakeState {
    fn record(&self, what: &str, body: &Value) {
        self.calls
            .lock()
            .expect("调用记录")
            .push((what.to_string(), body.clone()));
        self.log.push(what);
    }
}

/// `/im/v1/messages/{id}/reply` 之类路径里的那个 `{id}`，并进请求体的 `target`。
fn with_target(body: &Value, path: &str) -> Value {
    let target = path.rsplit('/').nth(1).unwrap_or_default();
    let mut body = body.clone();
    if let Value::Object(map) = &mut body {
        map.insert("target".into(), json!(target));
    }
    body
}

fn refuse(code: i64, msg: &str) -> Value {
    json!({ "code": code, "msg": msg })
}

// ---------------------------------------------------------------- 事件构造

/// 一条 `im.message.receive_v1` 的原始负载。
pub fn text_event(
    event_id: &str,
    chat_id: &str,
    chat_type: &str,
    sender: &str,
    text: &str,
    mentions: &[(&str, &str)],
) -> Vec<u8> {
    text_event_replying(event_id, chat_id, chat_type, sender, text, mentions, "", "")
}

/// 发送者在 `sender_tenant` 租户的消息；`header.tenant_key` 恒为 [`APP_TENANT`]。
/// 空串 = 平台没带这个键。
pub fn text_event_from_tenant(
    event_id: &str,
    chat_id: &str,
    chat_type: &str,
    sender: &str,
    sender_tenant: &str,
    text: &str,
    mentions: &[(&str, &str)],
) -> Vec<u8> {
    let mut payload: Value = serde_json::from_slice(&text_event(
        event_id, chat_id, chat_type, sender, text, mentions,
    ))
    .expect("自己构造的负载");
    if sender_tenant.is_empty() {
        payload["event"]["sender"]
            .as_object_mut()
            .expect("sender")
            .remove("tenant_key");
    } else {
        payload["event"]["sender"]["tenant_key"] = json!(sender_tenant);
    }
    payload.to_string().into_bytes()
}

/// 带 `root_id` / `thread_id` 的消息：群里话题内的消息两个都有，引用回复只有 `root_id`。
/// 空串 = 平台没带这个键。
#[allow(clippy::too_many_arguments)]
pub fn text_event_replying(
    event_id: &str,
    chat_id: &str,
    chat_type: &str,
    sender: &str,
    text: &str,
    mentions: &[(&str, &str)],
    root_id: &str,
    thread_id: &str,
) -> Vec<u8> {
    let mentions: Vec<Value> = mentions
        .iter()
        .map(|(key, open_id)| json!({ "key": key, "id": { "open_id": open_id }, "name": "komo" }))
        .collect();
    let mut message = json!({
        "message_id": format!("om_{event_id}"),
        "chat_id": chat_id,
        "chat_type": chat_type,
        "message_type": "text",
        "content": json!({ "text": text }).to_string(),
        "mentions": mentions,
    });
    for (key, value) in [("root_id", root_id), ("thread_id", thread_id)] {
        if !value.is_empty() {
            message[key] = json!(value);
        }
    }
    json!({
        "schema": "2.0",
        "header": {
            "event_id": event_id,
            "event_type": "im.message.receive_v1",
            "token": "v-token",
            "create_time": "1760000000000",
            "tenant_key": APP_TENANT,
        },
        "event": {
            "sender": {
                "sender_id": { "open_id": sender },
                "sender_type": "user",
                "tenant_key": APP_TENANT,
            },
            "message": message,
        },
    })
    .to_string()
    .into_bytes()
}

/// 话题列表里的一条消息。`sender` 以 `cli_` 打头是应用（机器人），否则是用户；
/// `mentions` 是 `(占位符, open_id, 名字)`。
pub fn thread_item(
    message_id: &str,
    created_ms: i64,
    sender: &str,
    msg_type: &str,
    content: &Value,
    mentions: &[(&str, &str, &str)],
) -> ThreadMessage {
    let is_app = sender.starts_with("cli_");
    serde_json::from_value(json!({
        "message_id": message_id,
        "msg_type": msg_type,
        "create_time": created_ms.to_string(),
        "deleted": false,
        "sender": {
            "id": sender,
            "id_type": if is_app { "app_id" } else { "open_id" },
            "sender_type": if is_app { "app" } else { "user" },
        },
        "mentions": mentions
            .iter()
            .map(|(key, id, name)| json!({ "key": key, "id": id, "id_type": "open_id", "name": name }))
            .collect::<Vec<_>>(),
        "body": { "content": content.to_string() },
    }))
    .expect("自己造的话题消息")
}

/// 给事件里的消息补上 `create_time`（毫秒）。
pub fn with_message_time(payload: Vec<u8>, created_ms: i64) -> Vec<u8> {
    let mut payload: Value = serde_json::from_slice(&payload).expect("自己构造的负载");
    payload["event"]["message"]["create_time"] = json!(created_ms.to_string());
    payload.to_string().into_bytes()
}

/// 一条私聊图片消息的原始负载。`message_id` 是 `om_{event_id}`。
pub fn image_event(event_id: &str, chat_id: &str, sender: &str, image_key: &str) -> Vec<u8> {
    let mut payload: Value =
        serde_json::from_slice(&text_event(event_id, chat_id, "p2p", sender, "", &[]))
            .expect("自己造的负载");
    payload["event"]["message"]["message_type"] = json!("image");
    payload["event"]["message"]["content"] = json!(json!({ "image_key": image_key }).to_string());
    payload.to_string().into_bytes()
}

/// 一条 `card.action.trigger` 的原始负载。komo 已经不发带按钮的卡片（§7），留着它只为
/// 断言"平台还推来的回调不会变成一条消息"。
pub fn card_action_event(event_id: &str, chat_id: &str, operator: &str) -> Vec<u8> {
    json!({
        "schema": "2.0",
        "header": {
            "event_id": event_id,
            "event_type": "card.action.trigger",
            "token": "v-token",
            "create_time": "1760000000000",
        },
        "event": {
            "operator": { "open_id": operator, "tenant_key": "t" },
            "token": "c-callback-token",
            "action": {
                "tag": "button",
                "value": { "action": "approve", "short_id": "7K2M" },
            },
            "host": "im_message",
            "context": {
                "open_chat_id": chat_id,
                "open_message_id": format!("om_{event_id}"),
            },
        },
    })
    .to_string()
    .into_bytes()
}

// ---------------------------------------------------------------- Inbound 替身

/// 记下每一条 `handle`，并在事件日志里留下开始与结束——"回执在 handle 返回之后才发"
/// 这条断言靠的就是这两个点。
pub struct RecordingInbound {
    received: Mutex<Vec<InboundMessage>>,
    log: EventLog,
    ack: InboundAck,
    delay: Duration,
    fail: bool,
}

impl RecordingInbound {
    pub fn new(log: EventLog) -> Arc<Self> {
        Self::with_ack(
            log,
            InboundAck::Queued {
                session: SessionId::from_raw("s-1"),
                run: RunId::from_raw("run-1"),
            },
        )
    }

    pub fn with_ack(log: EventLog, ack: InboundAck) -> Arc<Self> {
        Arc::new(Self {
            received: Mutex::new(Vec::new()),
            log,
            ack,
            delay: Duration::ZERO,
            fail: false,
        })
    }

    /// 让 `handle` 慢下来，好让"谁先谁后"变得可见。
    pub fn slow(log: EventLog, delay: Duration, ack: InboundAck) -> Arc<Self> {
        Arc::new(Self {
            received: Mutex::new(Vec::new()),
            log,
            ack,
            delay,
            fail: false,
        })
    }

    /// Dispatcher 自己出错的那条路。
    pub fn failing(log: EventLog) -> Arc<Self> {
        Arc::new(Self {
            received: Mutex::new(Vec::new()),
            log,
            ack: InboundAck::Ignored,
            delay: Duration::ZERO,
            fail: true,
        })
    }

    pub fn received(&self) -> Vec<InboundMessage> {
        self.received.lock().expect("入站记录").clone()
    }
}

#[async_trait]
impl Inbound for RecordingInbound {
    async fn handle(&self, msg: InboundMessage) -> Result<InboundAck, GatewayError> {
        self.log.push("handle:start");
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.received.lock().expect("入站记录").push(msg);
        self.log.push("handle:end");
        if self.fail {
            return Err(GatewayError::Internal("测试".into()));
        }
        Ok(self.ack.clone())
    }
}
