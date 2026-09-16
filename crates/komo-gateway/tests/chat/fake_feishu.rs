//! 假飞书开放平台：一个真的 HTTP 服务端，跑在 loopback 上。
//!
//! `src/channels/feishu/fake.rs` 的精简版（那一份是 `#[cfg(test)]`）。只保留这一组测试
//! 要断言的东西：tenant token、`/im/v1/messages` 发送、`PATCH` 原地更新、会话类型。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{Method, Uri};
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use komo_gateway::channels::feishu::FeishuApi;

/// 假平台上机器人自己的 open_id。
pub const BOT_OPEN_ID: &str = "ou_komo_bot";

struct FakeState {
    calls: Mutex<Vec<(String, Value)>>,
    patched: Mutex<Vec<String>>,
    next_message_id: AtomicI64,
}

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
    pub async fn start() -> Self {
        let state = Arc::new(FakeState {
            calls: Mutex::new(Vec::new()),
            patched: Mutex::new(Vec::new()),
            next_message_id: AtomicI64::new(1000),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定 loopback");
        let addr = listener.local_addr().expect("本地地址");
        let app = Router::new()
            .fallback(handle)
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeOpenApi { addr, state, task }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn api(&self) -> Arc<FeishuApi> {
        Arc::new(FeishuApi::with_endpoint(
            &self.endpoint(),
            "cli_test",
            "test-app-secret",
            reqwest::Client::new(),
        ))
    }

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

    /// 被 PATCH 过的 `message_id`，按顺序。
    pub fn patched_message_ids(&self) -> Vec<String> {
        self.state.patched.lock().expect("PATCH 记录").clone()
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
        json!({
            "code": 0,
            "msg": "ok",
            "tenant_access_token": "t-fake",
            "expire": 7200,
        })
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
        json!({ "code": 0, "msg": "ok", "data": {} })
    } else if path.ends_with("/im/v1/messages") {
        state.record("messages", &body);
        let id = state.next_message_id.fetch_add(1, Ordering::SeqCst);
        json!({ "code": 0, "msg": "ok", "data": { "message_id": format!("om_{id}") } })
    } else if path.contains("/im/v1/chats/") {
        state.record("chats", &json!({ "chat_id": last }));
        // 约定：`oc_dm` 是单聊，别的是群。
        let mode = if last == "oc_dm" { "p2p" } else { "group" };
        json!({ "code": 0, "msg": "ok", "data": { "chat_mode": mode } })
    } else {
        json!({ "code": 404, "msg": "no such fake endpoint" })
    };
    Json(reply)
}

impl FakeState {
    fn record(&self, what: &str, body: &Value) {
        self.calls
            .lock()
            .expect("调用记录")
            .push((what.to_string(), body.clone()));
    }
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
    let mentions: Vec<Value> = mentions
        .iter()
        .map(|(key, open_id)| json!({ "key": key, "id": { "open_id": open_id }, "name": "komo" }))
        .collect();
    json!({
        "schema": "2.0",
        "header": {
            "event_id": event_id,
            "event_type": "im.message.receive_v1",
            "token": "v-token",
            "create_time": "1760000000000",
        },
        "event": {
            "sender": { "sender_id": { "open_id": sender }, "sender_type": "user" },
            "message": {
                "message_id": format!("om_{event_id}"),
                "chat_id": chat_id,
                "chat_type": chat_type,
                "message_type": "text",
                "content": json!({ "text": text }).to_string(),
                "mentions": mentions,
            },
        },
    })
    .to_string()
    .into_bytes()
}

/// 一条 `card.action.trigger` 的原始负载。
pub fn card_action_event(
    event_id: &str,
    chat_id: &str,
    operator: &str,
    action: &str,
    short_id: &str,
) -> Vec<u8> {
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
                "value": { "action": action, "short_id": short_id },
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
