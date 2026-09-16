//! 假 iLink：一个真的 HTTP 服务端，跑在 loopback 上。
//!
//! `src/channels/wechat/fake.rs` 的精简版（那一份是 `#[cfg(test)]`）。能这么做是因为
//! `ILinkClient` 的每个方法都把 `base_url` 当参数收，所以把凭证里的 `base_url` 指向
//! loopback 不需要动 SDK 一行。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{RawQuery, State};
use axum::http::Uri;
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use wechatbot::types::{
    Credentials, MessageItemType, MessageState, MessageType, TextItem, WireMessage, WireMessageItem,
};

use komo_gateway::channels::wechat::WeChatChannel;

/// 假服务端上这个机器人的 token。**不是真凭证**。
pub const TEST_TOKEN: &str = "test-bot-token";

#[derive(Debug, Clone, Default)]
pub struct Behavior {
    /// 回了消息却回空游标：SDK 不推进游标，下一轮原样再来一次（spike §2.5）。
    pub replay: bool,
}

impl Behavior {
    pub fn replaying() -> Self {
        Behavior { replay: true }
    }
}

struct FakeState {
    behavior: Behavior,
    calls: Mutex<Vec<(String, Value)>>,
    msgs: Mutex<Vec<Value>>,
}

pub struct FakeILink {
    addr: SocketAddr,
    state: Arc<FakeState>,
    task: JoinHandle<()>,
}

impl Drop for FakeILink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeILink {
    pub async fn start(behavior: Behavior) -> Self {
        Self::start_with(behavior, Vec::new()).await
    }

    pub async fn start_with(behavior: Behavior, msgs: Vec<WireMessage>) -> Self {
        let msgs = msgs
            .iter()
            .map(|wire| serde_json::to_value(wire).expect("WireMessage 可序列化"))
            .collect();
        let state = Arc::new(FakeState {
            behavior,
            calls: Mutex::new(Vec::new()),
            msgs: Mutex::new(msgs),
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
        FakeILink { addr, state, task }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 之后再塞一条入站消息（"用户又说话了"）。
    pub fn push(&self, wire: &WireMessage) {
        self.state
            .msgs
            .lock()
            .expect("消息")
            .push(serde_json::to_value(wire).expect("WireMessage 可序列化"));
    }

    /// 指向这个假服务端的凭证。
    pub fn credentials(&self) -> Credentials {
        Credentials {
            token: TEST_TOKEN.into(),
            base_url: self.endpoint(),
            account_id: "acct".into(),
            user_id: "bot".into(),
            saved_at: Some("0Z".into()),
        }
    }

    /// 一个指向这个假服务端的渠道。
    pub fn channel(&self) -> Arc<WeChatChannel> {
        Arc::new(WeChatChannel::with_credentials(
            &self.credentials(),
            Default::default(),
        ))
    }

    pub fn calls_to(&self, endpoint: &str) -> Vec<Value> {
        self.state
            .calls
            .lock()
            .expect("调用记录")
            .iter()
            .filter(|(path, _)| path == endpoint)
            .map(|(_, body)| body.clone())
            .collect()
    }

    /// 已经发出去的每一段文本，按顺序——"积压的审批先到"靠它断言。
    pub fn sent_texts(&self) -> Vec<String> {
        self.calls_to("/ilink/bot/sendmessage")
            .into_iter()
            .filter_map(|body| {
                body.pointer("/msg/item_list/0/text_item/text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }
}

async fn handle(
    State(state): State<Arc<FakeState>>,
    uri: Uri,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Json<Value> {
    let path = uri.path().to_string();
    let parsed: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| json!({ "query": query.clone().unwrap_or_default() }));
    state
        .calls
        .lock()
        .expect("调用记录")
        .push((path.clone(), parsed.clone()));

    match path.as_str() {
        "/ilink/bot/getupdates" => {
            let mut msgs = state.msgs.lock().expect("消息");
            if state.behavior.replay {
                // 回了消息却回空游标：SDK 不推进游标，下一轮把同一批原样再取一次。
                return Json(json!({ "ret": 0, "get_updates_buf": "", "msgs": msgs.clone() }));
            }
            // 正常的一次拉取：把队列里还没交过的都交出去，游标推进。
            let batch: Vec<Value> = std::mem::take(&mut *msgs);
            Json(json!({ "ret": 0, "get_updates_buf": "c1", "msgs": batch }))
        }
        _ => Json(json!({ "errcode": 0 })),
    }
}

// ---------------------------------------------------------------- wire 样例

/// 一条文本 wire 消息。
pub fn text_wire(from: &str, client_id: &str, text: &str) -> WireMessage {
    WireMessage {
        from_user_id: from.to_string(),
        to_user_id: "bot".to_string(),
        client_id: client_id.to_string(),
        create_time_ms: 1_760_000_000_000,
        message_type: MessageType::User,
        message_state: MessageState::Finish,
        context_token: format!("ct-{from}"),
        item_list: vec![WireMessageItem {
            item_type: MessageItemType::Text,
            text_item: Some(TextItem {
                text: text.to_string(),
            }),
            image_item: None,
            voice_item: None,
            file_item: None,
            video_item: None,
            ref_msg: None,
        }],
    }
}
