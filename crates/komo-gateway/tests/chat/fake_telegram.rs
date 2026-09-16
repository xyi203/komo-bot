//! 假 Bot API：一个真的 HTTP 服务端，跑在 loopback 上。
//!
//! `src/channels/telegram/fake.rs` 的精简版——那一份是 `#[cfg(test)]`，集成测试拿不到。
//! 只保留这一组测试要断言的东西：按 `offset` 供货（**未确认的 update 原样再给一次**）、
//! 记下每一次调用的请求体。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::Uri;
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use komo_gateway::channels::telegram::BotApi;

/// 测试里的机器人用户名。
pub const BOT_USERNAME: &str = "komo_test_bot";

#[derive(Debug, Clone, Default)]
pub struct Behavior {
    /// 无视 `offset`，永远把整个队列原样再给一次——模拟平台重投。
    pub ignore_offset: bool,
}

impl Behavior {
    pub fn replaying() -> Self {
        Behavior {
            ignore_offset: true,
        }
    }
}

struct FakeState {
    behavior: Behavior,
    calls: Mutex<Vec<(String, Value)>>,
    updates: Mutex<Vec<Value>>,
    next_message_id: AtomicI64,
}

pub struct FakeBotApi {
    addr: SocketAddr,
    state: Arc<FakeState>,
    task: JoinHandle<()>,
}

impl Drop for FakeBotApi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeBotApi {
    pub async fn start(behavior: Behavior) -> Self {
        Self::start_with(behavior, Vec::new()).await
    }

    pub async fn start_with(behavior: Behavior, updates: Vec<Value>) -> Self {
        let state = Arc::new(FakeState {
            behavior,
            calls: Mutex::new(Vec::new()),
            updates: Mutex::new(updates),
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
        FakeBotApi { addr, state, task }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 指向这个假服务端的 `BotApi`。
    pub fn api(&self) -> Arc<BotApi> {
        Arc::new(BotApi::with_endpoint(
            &self.endpoint(),
            "test-token",
            reqwest::Client::new(),
        ))
    }

    /// 之后再往队列里塞一条 update（"用户又点了一次"）。
    pub fn push(&self, update: Value) {
        self.state.updates.lock().expect("update 队列").push(update);
    }

    /// 某个方法收到的每一个请求体，按顺序。
    pub fn calls_to(&self, method: &str) -> Vec<Value> {
        self.state
            .calls
            .lock()
            .expect("调用记录")
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, body)| body.clone())
            .collect()
    }

    /// 所有调用的方法名，按顺序——断言"谁在谁之后"用它。
    pub fn call_order(&self) -> Vec<String> {
        self.state
            .calls
            .lock()
            .expect("调用记录")
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// 发给某个会话的每一段文本，按顺序。
    pub fn texts_to(&self, chat_id: &str) -> Vec<String> {
        self.calls_to("sendMessage")
            .into_iter()
            .filter(|body| body["chat_id"] == chat_id)
            .filter_map(|body| body["text"].as_str().map(str::to_string))
            .collect()
    }
}

async fn handle(State(state): State<Arc<FakeState>>, uri: Uri, body: Bytes) -> Json<Value> {
    let method = uri
        .path()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let body: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    state
        .calls
        .lock()
        .expect("调用记录")
        .push((method.clone(), body.clone()));

    let reply = match method.as_str() {
        "getMe" => ok(json!({
            "id": 42,
            "is_bot": true,
            "first_name": "komo",
            "username": BOT_USERNAME,
        })),
        "getUpdates" => {
            let offset = body.get("offset").and_then(Value::as_i64);
            let updates = state.updates.lock().expect("update 队列");
            let served: Vec<Value> = updates
                .iter()
                .filter(|update| {
                    if state.behavior.ignore_offset {
                        return true;
                    }
                    match offset {
                        None => true,
                        Some(offset) => {
                            update.get("update_id").and_then(Value::as_i64).unwrap_or(0) >= offset
                        }
                    }
                })
                .cloned()
                .collect();
            ok(json!(served))
        }
        "sendMessage" => {
            let message_id = state.next_message_id.fetch_add(1, Ordering::SeqCst);
            ok(json!({
                "message_id": message_id,
                "date": 0,
                "chat": { "id": 11, "type": "private" },
            }))
        }
        "editMessageText" | "editMessageReplyMarkup" => ok(json!(true)),
        "answerCallbackQuery" => ok(json!(true)),
        other => {
            json!({ "ok": false, "error_code": 404, "description": format!("Not Found: {other}") })
        }
    };
    Json(reply)
}

fn ok(result: Value) -> Value {
    json!({ "ok": true, "result": result })
}

// ---------------------------------------------------------------- update 构造

pub fn text_update(update_id: i64, chat_id: i64, chat_type: &str, from: i64, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {
            "message_id": update_id * 10,
            "date": 1,
            "chat": { "id": chat_id, "type": chat_type },
            "from": { "id": from, "is_bot": false, "first_name": "op" },
            "text": text,
        },
    })
}

pub fn callback_update(update_id: i64, chat_id: i64, from: i64, data: &str) -> Value {
    json!({
        "update_id": update_id,
        "callback_query": {
            "id": format!("cb-{update_id}"),
            "from": { "id": from, "is_bot": false, "first_name": "op" },
            "data": data,
            "message": {
                "message_id": update_id * 10,
                "date": 1,
                "chat": { "id": chat_id, "type": "private" },
            },
        },
    })
}
