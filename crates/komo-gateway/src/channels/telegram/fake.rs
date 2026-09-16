//! 测试用的假 Bot API：一个真的 HTTP 服务端，跑在 loopback 上。
//!
//! 为什么是真服务端而不是给 `BotApi` 挖一个接缝：长轮询这件事的行为全在"什么时候发
//! 请求、请求里的 `offset` 是几"上，而这两件事只有在真的发出去之后才看得见。假服务端
//! 按 `offset` 语义供货（**未确认的 update 原样再给一次**），于是"ack 在处理之后"就是
//! 一条可断言的事实，而不是一段注释。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::Uri;
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

use super::api::BotApi;

/// 测试里的机器人用户名。
pub const BOT_USERNAME: &str = "komo_test_bot";

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
    /// 带 `parse_mode` 的 `sendMessage` 一律 400（§13.2 的回退路径）。
    pub refuse_markdown: bool,
    /// 所有 `sendMessage` 都 400。
    pub refuse_sends: bool,
    /// 两个 `editMessage*` 都 400（决定后的界面回写失败）。
    pub refuse_edits: bool,
    /// 无视 `offset`，永远把整个队列原样再给一次——模拟平台重投。
    pub ignore_offset: bool,
    /// `getMe` 一律 401（token 不对）。
    pub refuse_get_me: bool,
}

impl Behavior {
    pub fn refuse_markdown() -> Self {
        Self {
            refuse_markdown: true,
            ..Self::default()
        }
    }

    pub fn refuse_everything() -> Self {
        Self {
            refuse_sends: true,
            ..Self::default()
        }
    }

    pub fn refuse_edits() -> Self {
        Self {
            refuse_edits: true,
            ..Self::default()
        }
    }

    pub fn replaying() -> Self {
        Self {
            ignore_offset: true,
            ..Self::default()
        }
    }

    pub fn refuse_get_me() -> Self {
        Self {
            refuse_get_me: true,
            ..Self::default()
        }
    }
}

struct FakeState {
    behavior: Behavior,
    calls: Mutex<Vec<(String, Value)>>,
    updates: Mutex<Vec<Value>>,
    log: EventLog,
    next_message_id: AtomicI64,
}

/// 一个跑起来的假 Bot API。
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

    /// 指向这个假服务端的 `BotApi`。
    pub fn api(&self) -> Arc<BotApi> {
        Arc::new(BotApi::with_endpoint(
            &self.endpoint(),
            "test-token",
            reqwest::Client::new(),
        ))
    }

    pub fn log(&self) -> EventLog {
        self.state.log.clone()
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

    /// 往队列里再放一条 update（测试运行中"平台又来了一条"）。
    pub fn push(&self, update: Value) {
        self.state.updates.lock().expect("update 队列").push(update);
    }

    /// 发给某个会话的每一段文本，按顺序。
    pub fn texts_to(&self, chat_id: &str) -> Vec<String> {
        self.calls_to("sendMessage")
            .into_iter()
            .filter(|body| body["chat_id"] == chat_id)
            .filter_map(|body| body["text"].as_str().map(str::to_string))
            .collect()
    }

    /// 每次 `getUpdates` 带的 `offset`（没带的是 `None`）。
    pub fn polled_offsets(&self) -> Vec<Option<i64>> {
        self.calls_to("getUpdates")
            .iter()
            .map(|body| body.get("offset").and_then(Value::as_i64))
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
        "getMe" if state.behavior.refuse_get_me => refuse(401, "Unauthorized"),
        "getMe" => ok(json!({
            "id": 42,
            "is_bot": true,
            "first_name": "komo",
            "username": BOT_USERNAME,
        })),
        "getUpdates" => {
            let offset = body.get("offset").and_then(Value::as_i64);
            state
                .log
                .push(format!("getUpdates:{}", offset.unwrap_or(-1)));
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
            let markdown = body.get("parse_mode").is_some();
            if state.behavior.refuse_sends || (state.behavior.refuse_markdown && markdown) {
                refuse(400, "Bad Request: can't parse entities")
            } else {
                let message_id = state.next_message_id.fetch_add(1, Ordering::SeqCst);
                ok(json!({
                    "message_id": message_id,
                    "date": 0,
                    "chat": { "id": 11, "type": "private" },
                }))
            }
        }
        "editMessageText" | "editMessageReplyMarkup" => {
            if state.behavior.refuse_edits {
                refuse(400, "Bad Request: message is not modified")
            } else {
                ok(json!(true))
            }
        }
        "answerCallbackQuery" => ok(json!(true)),
        other => refuse(404, &format!("Not Found: {other}")),
    };
    Json(reply)
}

fn ok(result: Value) -> Value {
    json!({ "ok": true, "result": result })
}

fn refuse(code: i64, description: &str) -> Value {
    json!({ "ok": false, "error_code": code, "description": description })
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

// ---------------------------------------------------------------- Inbound 替身

/// 记下每一条 `handle`，并在事件日志里留下开始与结束——"offset 在 handle 返回之后才
/// 推进"这条断言靠的就是这两个点。
pub struct RecordingInbound {
    received: Mutex<Vec<InboundMessage>>,
    log: EventLog,
    ack: InboundAck,
    delay: Duration,
}

impl RecordingInbound {
    pub fn new(log: EventLog) -> Arc<Self> {
        Arc::new(Self {
            received: Mutex::new(Vec::new()),
            log,
            ack: InboundAck::Queued {
                session: SessionId::from_raw("s-1"),
                run: RunId::from_raw("run-1"),
            },
            delay: Duration::ZERO,
        })
    }

    pub fn with_ack(log: EventLog, ack: InboundAck) -> Arc<Self> {
        Arc::new(Self {
            received: Mutex::new(Vec::new()),
            log,
            ack,
            delay: Duration::ZERO,
        })
    }

    /// 让 `handle` 慢下来，好让"第二次 getUpdates 有没有抢跑"变得可见。
    pub fn slow(log: EventLog, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            received: Mutex::new(Vec::new()),
            log,
            ack: InboundAck::Ignored,
            delay,
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
        targets: vec![PlanTarget {
            path: std::path::PathBuf::from("/tmp/scratch"),
            access: TargetAccess::Write,
            expected_version: None,
        }],
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
