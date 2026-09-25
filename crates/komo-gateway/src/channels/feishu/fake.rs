//! 测试用的假开放平台：一个真的 HTTP 服务端，跑在 loopback 上。
//!
//! 为什么是真服务端而不是给 [`FeishuApi`] 挖一个接缝：token 缓存这件事的行为全在"什么
//! 时候真的去取一张"上，而那只有在请求真的发出去之后才看得见。同理，"决定之后 PATCH 的
//! 是哪一条消息"要的是那条 `message_id` 的**来回**，不是一个由替身编出来的值。
//!
//! ws 那一侧没有替身：openlark 的协议实现不在我们手里，本机也没有凭证。`serve` 的事件
//! 入口做成了一个可注入的 channel（`FeishuChannel::injected`），测试直接塞负载。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::{Method, Uri};
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::traits::{GatewayError, Inbound};
use komo_kernel::types::chat::{ApprovalPresentation, ApprovalScope};
use komo_kernel::types::ids::{ApprovalId, OperationId, RunId, SessionId, ShortId};
use komo_kernel::types::plan::{
    ExecutionPlan, Operation, PlanSource, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
};

use super::api::FeishuApi;

/// 假平台上机器人自己的 open_id。
pub const BOT_OPEN_ID: &str = "ou_komo_bot";

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
    /// PATCH 一律被拒（决定之后的界面回写失败）。
    pub refuse_patch: bool,
    /// 问不到会话类型。
    pub refuse_chats: bool,
    /// token 的 `expire` 只给 1 秒。
    pub short_lived_token: bool,
    /// 回复一律被拒（处理中卡片发不出去）。
    pub refuse_replies: bool,
    /// 加表情一律被拒。
    pub refuse_reactions: bool,
    /// 每次回复先睡这么久——让"终态先于卡片"变得可复现。
    pub reply_delay: Duration,
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

    pub fn refuse_chats() -> Self {
        Self {
            refuse_chats: true,
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
    } else if path.ends_with("/im/v1/messages") {
        state.record("messages", &body);
        if state.behavior.refuse_sends {
            refuse(230001, "bot is not in the chat")
        } else {
            let id = state.next_message_id.fetch_add(1, Ordering::SeqCst);
            json!({ "code": 0, "msg": "ok", "data": { "message_id": format!("om_{id}") } })
        }
    } else if path.contains("/im/v1/chats/") {
        state.record("chats", &json!({ "chat_id": last }));
        if state.behavior.refuse_chats {
            refuse(232000, "chat not found")
        } else {
            // 约定：`oc_dm` 与 `oc_1` 是单聊，别的是群。
            let mode = if last == "oc_dm" || last == "oc_1" {
                "p2p"
            } else {
                "group"
            };
            json!({ "code": 0, "msg": "ok", "data": { "chat_mode": mode } })
        }
    } else {
        refuse(404, "no such fake endpoint")
    };
    Json(reply)
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

// ---------------------------------------------------------------- 审批样例

pub fn plan() -> ExecutionPlan {
    ExecutionPlan {
        operation_id: OperationId::from_raw("op-1"),
        source: PlanSource::Interactive {
            session: SessionId::from_raw("s-1"),
        },
        tool: "shell".into(),
        operation: Operation::ShellCommand {
            command: "rm -rf /tmp/scratch".into(),
        },
        run: Some(RunId::from_raw("run-1")),
        tool_call: None,
        args: json!({ "command": "rm -rf /tmp/scratch" }),
        cwd: Some(std::path::PathBuf::from("/home/op/work")),
        targets: vec![PlanTarget::local("/tmp/scratch", TargetAccess::Write)],
        versions: PlanVersions::default(),
        resources: Vec::new(),
        recovery: RecoveryMode::NoSafeRecovery,
    }
}

pub fn presentation() -> ApprovalPresentation {
    ApprovalPresentation {
        approval: ApprovalId::from_raw("ap-1"),
        short_id: ShortId::parse("7K2M").unwrap(),
        plan_hash: plan().plan_hash(),
        plan: plan(),
        reason: "写入 workspace 之外的路径".into(),
        changes: Some("--- a/x\n+++ b/x".into()),
        evidence: None,
        scopes: vec![ApprovalScope::Once, ApprovalScope::Run],
        valid_until: None,
    }
}
