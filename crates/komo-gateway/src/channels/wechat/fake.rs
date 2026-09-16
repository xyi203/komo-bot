//! 测试用的假 iLink：一个真的 HTTP 服务端，跑在 loopback 上。
//!
//! 为什么是真服务端而不是给渠道挖一个接缝：微信这一侧要断言的事全在**请求的形状与
//! 次序**上——游标是不是在处理之后才推进、没有回复令牌时到底有没有联网、`errcode -14`
//! 之后还拉不拉。这些只有在请求真的发出去之后才看得见。
//!
//! 能这么做是因为 [`ILinkClient`] 的每个方法都把 `base_url` 当参数收（而不是在构造时
//! 定死），所以把它指向 loopback 不需要动 SDK 一行。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::extract::{RawQuery, State};
use axum::http::Uri;
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use wechatbot::protocol::ILinkClient;
use wechatbot::types::{
    Credentials, ImageItem, MessageItemType, MessageState, MessageType, TextItem, WireMessage,
    WireMessageItem,
};

use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::traits::{GatewayError, Inbound};
use komo_kernel::types::chat::{ApprovalPresentation, ApprovalScope};
use komo_kernel::types::ids::{ApprovalId, OperationId, RunId, SessionId, ShortId};
use komo_kernel::types::plan::{
    ExecutionPlan, Operation, PlanSource, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
};

use super::send::{Auth, ContextTokens, WeChatSender};
use super::{AlertSink, WeChatChannel};

/// 假服务端上这个机器人的 token。**不是真凭证**。
pub const TEST_TOKEN: &str = "test-bot-token";

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
    /// 无视游标、永远回空的 `get_updates_buf`——SDK 因此原地重取同一批（spike §2.5）。
    pub replay: bool,
    /// `sendmessage` 一律被拒。
    pub refuse_sends: bool,
    /// `getupdates` 一律 `errcode: -14`（会话过期）。
    pub session_expired: bool,
    /// `getupdates` 回一个枚举外的 `message_type`——整批反序列化失败（spike §2.5 末）。
    pub bad_enum: bool,
    /// `notifystart` 一律 `errcode: -14`。
    pub probe_expired: bool,
}

impl Behavior {
    pub fn replaying() -> Self {
        Self {
            replay: true,
            ..Self::default()
        }
    }

    pub fn refuse_sends() -> Self {
        Self {
            refuse_sends: true,
            ..Self::default()
        }
    }

    pub fn session_expired() -> Self {
        Self {
            session_expired: true,
            ..Self::default()
        }
    }

    pub fn bad_enum() -> Self {
        Self {
            bad_enum: true,
            ..Self::default()
        }
    }

    pub fn probe_expired() -> Self {
        Self {
            probe_expired: true,
            ..Self::default()
        }
    }
}

struct FakeState {
    behavior: Behavior,
    calls: Mutex<Vec<(String, Value)>>,
    msgs: Mutex<Vec<Value>>,
    /// 扫码状态的剧本，一次取一条；取空之后重复最后一条。
    qr_statuses: Mutex<Vec<Value>>,
    log: EventLog,
}

/// 一个跑起来的假 iLink。
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

    /// 带一批待投递的 wire 消息起来。
    pub async fn start_with(behavior: Behavior, msgs: Vec<WireMessage>) -> Self {
        let msgs = msgs
            .iter()
            .map(|wire| serde_json::to_value(wire).expect("WireMessage 可序列化"))
            .collect();
        Self::start_raw(behavior, msgs, Vec::new()).await
    }

    /// 带一份扫码状态剧本起来（登录流程用）。
    pub async fn start_with_qr(behavior: Behavior, qr_statuses: Vec<Value>) -> Self {
        Self::start_raw(behavior, Vec::new(), qr_statuses).await
    }

    async fn start_raw(behavior: Behavior, msgs: Vec<Value>, qr_statuses: Vec<Value>) -> Self {
        let state = Arc::new(FakeState {
            behavior,
            calls: Mutex::new(Vec::new()),
            msgs: Mutex::new(msgs),
            qr_statuses: Mutex::new(qr_statuses),
            log: EventLog::default(),
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

    pub fn log(&self) -> EventLog {
        self.state.log.clone()
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

    pub fn auth(&self) -> Auth {
        Auth::from(&self.credentials())
    }

    /// 一个指向这个假服务端的发送口。
    pub fn sender(&self) -> WeChatSender {
        WeChatSender::new(
            Arc::new(ILinkClient::new()),
            self.auth(),
            Arc::new(ContextTokens::new()),
        )
    }

    /// 一个指向这个假服务端的渠道。
    pub fn channel(&self) -> WeChatChannel {
        WeChatChannel::with_credentials(&self.credentials(), Default::default())
    }

    /// 打到某个 endpoint 的请求体（GET 则是它的查询串）。
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

    pub fn call_count(&self, endpoint: &str) -> usize {
        self.calls_to(endpoint).len()
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
    state.log.push(format!("call:{path}"));

    match path.as_str() {
        "/ilink/bot/getupdates" => {
            if state.behavior.session_expired {
                return Json(json!({ "ret": 0, "errcode": -14, "errmsg": "session expired" }));
            }
            if state.behavior.bad_enum {
                // 枚举外的 `message_type`：`serde_repr` 认不出来，整批反序列化失败，
                // SDK 抛 `WeChatBotError::Json`（spike §2.5 末）。
                return Json(json!({
                    "ret": 0,
                    "get_updates_buf": "c1",
                    "msgs": [{
                        "from_user_id": "wxid_op",
                        "to_user_id": "bot",
                        "client_id": "cid-x",
                        "create_time_ms": 1_760_000_000_000i64,
                        "message_type": 99,
                        "message_state": 2,
                        "context_token": "ct",
                        "item_list": [],
                    }],
                }));
            }
            let msgs = state.msgs.lock().expect("消息").clone();
            if state.behavior.replay {
                // 回了消息却回空游标：SDK 不推进游标，下一轮原样再来一次。
                return Json(json!({ "ret": 0, "get_updates_buf": "", "msgs": msgs }));
            }
            let cursor = parsed["get_updates_buf"].as_str().unwrap_or_default();
            if cursor.is_empty() {
                Json(json!({ "ret": 0, "get_updates_buf": "c1", "msgs": msgs }))
            } else {
                Json(json!({ "ret": 0, "get_updates_buf": "c1", "msgs": [] }))
            }
        }
        "/ilink/bot/sendmessage" => {
            if state.behavior.refuse_sends {
                Json(json!({ "errcode": 40001, "errmsg": "refused" }))
            } else {
                Json(json!({ "errcode": 0 }))
            }
        }
        "/ilink/bot/msg/notifystart" => {
            if state.behavior.probe_expired {
                Json(json!({ "errcode": -14, "errmsg": "session expired" }))
            } else {
                Json(json!({ "errcode": 0 }))
            }
        }
        "/ilink/bot/msg/notifystop" => Json(json!({ "errcode": 0 })),
        "/ilink/bot/get_bot_qrcode" => Json(json!({
            "qrcode": "qr-ticket",
            "qrcode_img_content": "https://ilinkai.weixin.qq.com/qr/abc",
        })),
        "/ilink/bot/get_qrcode_status" => {
            let mut statuses = state.qr_statuses.lock().expect("扫码剧本");
            let next = if statuses.len() > 1 {
                statuses.remove(0)
            } else {
                statuses
                    .first()
                    .cloned()
                    .unwrap_or_else(|| json!({ "status": "expired" }))
            };
            Json(next)
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

/// 一条图片 wire 消息——渠道只处理文本，它该被忽略。
pub fn image_wire(from: &str, client_id: &str) -> WireMessage {
    let mut wire = text_wire(from, client_id, "");
    wire.item_list = vec![WireMessageItem {
        item_type: MessageItemType::Image,
        text_item: None,
        image_item: Some(ImageItem {
            media: None,
            thumb_media: None,
            aeskey: None,
            url: Some("https://img.example/a.png".into()),
            mid_size: None,
            thumb_width: None,
            thumb_height: None,
        }),
        voice_item: None,
        file_item: None,
        video_item: None,
        ref_msg: None,
    }];
    wire
}

// ---------------------------------------------------------------- Inbound 替身

/// 记下每一条到达的入站消息。
pub struct RecordingInbound {
    received: Mutex<Vec<InboundMessage>>,
    log: EventLog,
    ack: InboundAck,
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
            fail: false,
        })
    }

    /// Dispatcher 自己出错的那条路。
    pub fn failing(log: EventLog) -> Arc<Self> {
        Arc::new(Self {
            received: Mutex::new(Vec::new()),
            log,
            ack: InboundAck::Ignored,
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
        self.received.lock().expect("入站记录").push(msg);
        self.log.push("handle:end");
        if self.fail {
            return Err(GatewayError::Internal("测试".into()));
        }
        Ok(self.ack.clone())
    }
}

// ---------------------------------------------------------------- 告警替身

/// 记下每一条告警。
#[derive(Debug, Default)]
pub struct RecordingAlerts {
    alerts: Mutex<Vec<String>>,
}

impl RecordingAlerts {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn alerts(&self) -> Vec<String> {
        self.alerts.lock().expect("告警记录").clone()
    }
}

#[async_trait]
impl AlertSink for RecordingAlerts {
    async fn alert(&self, text: String) {
        self.alerts.lock().expect("告警记录").push(text);
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
        short_id: ShortId::parse("7K2M").expect("短 ID"),
        plan_hash: plan().plan_hash(),
        plan: plan(),
        reason: "写入 workspace 之外的路径（第 3 条规则）".into(),
        changes: Some("--- a/x\n+++ b/x\n-1\n+2".into()),
        evidence: None,
        scopes: vec![ApprovalScope::Once, ApprovalScope::Run],
        valid_until: None,
    }
}
