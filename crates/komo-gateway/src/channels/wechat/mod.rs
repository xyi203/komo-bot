//! 微信 iLink：DM only，用户未发消息前投递 Deferred（§11.1、§11.4、§11.5）。
//!
//! 一条拉取循环，四条规矩：
//!
//! 1. **ack 在处理之后**。iLink 没有独立的 ack——唯一的确认手段是把服务端回的
//!    `get_updates_buf` 带进下一次请求，所以"推进游标"就是签收。游标在这一批全部交给
//!    `Inbound::handle` 之后才推进（spike §2.5）。
//! 2. **去重是必需的，不是以防万一**。微信的重投有两个来源：拉取游标只活在进程内存
//!    里、重启从空游标开始；以及服务端回了消息却回空游标时，下一轮原样再交一次。durable
//!    的那一层在 Dispatcher（§8.5 的请求键），这里这一层只挡同进程重投，让 `Inbound`
//!    只看见一次（§11.1）。
//! 3. **`errcode -14` 停下来，不自己重登**。SDK 的 `WeChatBot::run()` 撞上会话过期时会
//!    自动 `login(true)` 重走二维码流程，而那条流程要一个能显示二维码、能读配对码的
//!    终端——在一个后台 Gateway 里它只会挂住。所以这里改为：清空令牌表、报一条
//!    "需要重新 `komo channel wechat login`" 的告警，`serve` 返回 `Err`。
//! 4. **连续 N 次 JSON 错误升级为告警**。SDK 的 `MessageType` / `MessageState` 用
//!    `Deserialize_repr` 且**没有 unknown 兜底变体**（spike §2.5 末），服务端多一个枚举
//!    值就会让整批反序列化失败；只退避重试的话渠道会静默卡死。所以数着它，
//!    [`JSON_ALERT_THRESHOLD`] 次之后报一次 home chat（§11.4 末），**只报一次**——
//!    告警刷屏和静默卡死一样没用。
//!
//! **与 SDK 的一处刻意偏差**：这里不用 `WeChatBot::run()`，直接驱动
//! [`ILinkClient`]（SDK 文档给 [`IncomingMessage::from_wire`](wechatbot::types::IncomingMessage::from_wire)
//! 的定位就是"自己驱动 `get_updates` 的调用方"的稳定入口）。理由是上面第 3、4 条：
//! `run()` 的错误分支只会 1s/2s/…/10s 无限退避，既数不出 JSON 错误，也把 `-14` 变成一次
//! 无人看管的重登；而它的 handler 是同步 `Fn`，把一条消息交给 `async` 的 Dispatcher 还
//! 得再架一层 channel。顺带的好处是 `base_url` 是每次调用的参数，于是假服务端可以就是
//! 一个 loopback 上的 axum（见 [`fake`]）。
//!
//! 渠道只依赖 kernel 的类型、`Channel` / `Inbound` 两个 trait 与 wechatbot（§11.5）：
//! 它不知道 Session 是什么，也不读环境变量——凭证路径由 [`WeChatFactory`] 从配置快照的
//! 数据目录算出来传进构造函数。

pub mod inbound;
pub mod login;
pub mod send;

#[cfg(any(test, feature = "test-support"))]
pub mod fake;

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use komo_kernel::protocol::InboundAck;
use komo_kernel::protocol::config::{ChannelConfig, ConfigSnapshot};
use komo_kernel::traits::{Channel, ChannelError, Inbound, Shutdown};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform};
use komo_runtime::config::Secrets;
use wechatbot::error::WeChatBotError;
use wechatbot::protocol::ILinkClient;
use wechatbot::types::{Credentials, WireMessage};

use crate::channels::{BuiltChannel, ChannelFactory, ChannelSender};

pub use login::{LoginError, LoginOptions, LoginStep, credentials_path, login, login_with};
pub use send::{Auth, ContextTokens, WeChatSender};

/// 查一次停机标志的间隔。`Shutdown` 是个可查询的标志而不是一个可等待的信号
/// （kernel 不依赖 tokio），所以每个 await 都和它赛跑。
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// 两次空拉取之间的地板。真实的 `getupdates` 会挂满 45 秒，立刻返回空的只可能是对端
/// 不正常——没有这条地板，那种时候循环会变成一个忙等。
const DEFAULT_IDLE_FLOOR: Duration = Duration::from_millis(250);

const DEFAULT_RETRY_BASE: Duration = Duration::from_secs(1);
const DEFAULT_RETRY_CAP: Duration = Duration::from_secs(60);

/// 进程内记住多少个去重键。
const SEEN_KEYS_CAP: usize = 4096;

/// 连续多少次 JSON 错误算"卡死了"（§11.4 末）。
pub const JSON_ALERT_THRESHOLD: usize = 5;

/// Dispatcher 自己出错时回给发送者的话。**不静默丢弃**（§11.4 的同一条理由）。
const INTERNAL_ERROR_HINT: &str = "komo 没能处理这条消息（内部错误），请稍后再说一次。";

/// 会话过期时报给操作者的话。
pub const SESSION_EXPIRED_HINT: &str = "微信会话已过期（errcode -14），微信渠道已经停了：请在终端上跑 `komo channel wechat login` 重新登录。";

/// 把一句话报给操作者（home chat）。
///
/// 为什么是渠道自己的一个小 trait，而不是 kernel 的 `Notifier`：`Notifier::deliver` 要
/// 一个已经解析好的 [`DeliveryTarget`](komo_kernel::types::chat::DeliveryTarget)，而
/// "home chat 是哪个"只看当前配置快照（§11.4），是 `HomeNotifier` 的事——渠道手里既没有
/// 快照也不该有。所以这里只留一口"说一句话"，由接线那一侧把它接到
/// `HomeNotifier::deliver_home` 上。
///
/// **告警不能挡住拉取**：实现要么很快返回，要么自己 spawn。
#[async_trait]
pub trait AlertSink: Send + Sync {
    async fn alert(&self, text: String);
}

/// `komo channel probe` 的连通性核对（§11.5）。
///
/// **不消费消息**：`getupdates` 会推进游标，拿它做探针等于把没处理的消息签收掉。最便宜
/// 且无副作用的探针是 `notifystart`（只要 token），代价是它会给服务端留一个"机器人上线
/// 了"的状态，所以成功之后补一次 `notifystop`（spike §2.7）。
///
/// 返回的那句话里**没有 token / userId / accountId**。
pub async fn probe(credentials_path: &Path) -> Result<String, ChannelError> {
    let credentials = login::load_credentials(credentials_path)?;
    let client = ILinkClient::new();
    match client
        .notify_start(&credentials.base_url, &credentials.token)
        .await
    {
        Ok(()) => {
            // 探针自己留下的在线状态要收掉；收不掉不改变"凭证有效"这个结论。
            if let Err(error) = client
                .notify_stop(&credentials.base_url, &credentials.token)
                .await
            {
                tracing::warn!(%error, "微信：probe 之后的 notify_stop 失败，按非致命处理");
            }
            Ok(format!(
                "notify_start / notify_stop 通过：凭证仍然有效（{}）",
                saved_at_note(&credentials)
            ))
        }
        Err(error) if error.is_session_expired() => Err(channel_error(
            "凭证已失效（errcode -14）：请跑 `komo channel wechat login` 重新登录",
        )),
        Err(error) => Err(channel_error(error.to_string())),
    }
}

/// 凭证什么时候写的。`saved_at` 是 `"{unix 秒}Z"`，不是 ISO 8601（spike §2.7）。
fn saved_at_note(credentials: &Credentials) -> String {
    match credentials.saved_at.as_deref() {
        Some(stamp) => format!("凭证保存于 {stamp}"),
        None => "凭证没有记录保存时间".into(),
    }
}

pub struct WeChatChannel {
    sender: Arc<WeChatSender>,
    /// 构造那一刻的配置快照。**不拿它做准入判定**——见 [`inbound`] 的模块注释。
    config: ChannelConfig,
    alerts: Option<Arc<dyn AlertSink>>,
    /// 同进程的去重（§11.1 的两条重投来源）。
    seen: Mutex<SeenKeys>,
    idle_floor: Duration,
    retry_base: Duration,
    retry_cap: Duration,
}

impl std::fmt::Debug for WeChatChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeChatChannel").finish_non_exhaustive()
    }
}

impl WeChatChannel {
    /// 从凭证文件造一个渠道。
    ///
    /// **同步读文件**：`ChannelFactory::build` 是同步的（热重载时按平台重造，§3 第 3
    /// 步），而这份文件是几百字节的本地 JSON。
    ///
    /// 没有 `http` 参数：[`ILinkClient`] 自己带一个装好了超时的 reqwest 客户端，而它没有
    /// 收一个外来 client 的构造函数——硬塞一个进去要改 SDK。
    pub fn new(credentials_path: &Path, config: ChannelConfig) -> Result<Self, ChannelError> {
        let credentials = login::load_credentials(credentials_path)?;
        Ok(Self::with_credentials(&credentials, config))
    }

    /// 用一份已经读出来的凭证造一个渠道。
    pub fn with_credentials(credentials: &Credentials, config: ChannelConfig) -> Self {
        let sender = Arc::new(WeChatSender::new(
            Arc::new(ILinkClient::new()),
            Auth::from(credentials),
            Arc::new(ContextTokens::new()),
        ));
        WeChatChannel {
            sender,
            config,
            alerts: None,
            seen: Mutex::new(SeenKeys::default()),
            idle_floor: DEFAULT_IDLE_FLOOR,
            retry_base: DEFAULT_RETRY_BASE,
            retry_cap: DEFAULT_RETRY_CAP,
        }
    }

    /// 接上告警口（§11.4 末）。没接就只写日志。
    pub fn with_alerts(mut self, alerts: Option<Arc<dyn AlertSink>>) -> Self {
        self.alerts = alerts;
        self
    }

    /// 出站的那一半。Notifier 用它主动投递（§11.4）；它与入站共用同一张回复令牌表，
    /// 这正是"用户下一条消息到达之后 pending 的投递就能冲刷出去"的原因。
    pub fn sender(&self) -> Arc<WeChatSender> {
        Arc::clone(&self.sender)
    }

    pub fn config(&self) -> &ChannelConfig {
        &self.config
    }

    /// 把重试与空转节奏调快，给测试用。
    #[cfg(any(test, feature = "test-support"))]
    pub fn tuned(mut self) -> Self {
        self.idle_floor = Duration::from_millis(5);
        self.retry_base = Duration::from_millis(5);
        self.retry_cap = Duration::from_millis(20);
        self
    }

    async fn alert(&self, text: String) {
        match &self.alerts {
            Some(sink) => sink.alert(text).await,
            None => tracing::error!(%text, "微信：没有接告警口，这条只进日志"),
        }
    }

    /// 一条 wire 消息。返回 `false` = 这条是重投，已经处理过了。
    async fn handle_wire(&self, inbound: &dyn Inbound, wire: &WireMessage) {
        // **先记令牌，再判断处不处理**：与 SDK 的 `remember_context` 同位置——一条我们
        // 不处理的消息（图片、机器人自己的回流）同样带着一个能回推的令牌，丢掉它就等于
        // 白白多一次 `Deferred`。
        self.sender.contexts().remember_wire(wire);

        let request_key = inbound::request_key(wire);
        // 平台重投在这里挡住。**在分发之前**：进了 Dispatcher 才发现是重投，那一层给的
        // 是 `Duplicate`，而这一层要的是"根本没来过"。
        if !self
            .seen
            .lock()
            .expect("去重集合")
            .remember(request_key.as_str())
        {
            tracing::debug!(%request_key, "微信：这条消息已经处理过了");
            return;
        }

        let Some(message) = inbound::from_wire(wire, request_key.clone()) else {
            // 非文本、机器人自己的回流、空正文：`Ignored`，不进 Dispatcher。
            tracing::debug!(%request_key, "微信：忽略");
            return;
        };

        let peer = message.peer.clone();
        match inbound.handle(message).await {
            Ok(InboundAck::Replied { text }) => self.reply(&peer, &text).await,
            Ok(InboundAck::Rejected { hint }) => self.reply(&peer, &hint).await,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%request_key, %error, "微信：Dispatcher 出错");
                // TODO(decide: 出错时这条键照样留在去重集合里——否则一个确定性的错误会让
                // 同一条消息在重投时无限重跑。代价是这条消息只剩这句回执。)
                self.reply(&peer, INTERNAL_ERROR_HINT).await;
            }
        }
    }

    async fn reply(&self, peer: &ChannelPeer, text: &str) {
        if let Err(error) = self.sender.send_text(&peer.chat_id, text).await {
            tracing::warn!(%peer, %error, "微信：回执没送出去");
        }
    }
}

#[async_trait]
impl Channel for WeChatChannel {
    fn name(&self) -> &'static str {
        "wechat"
    }

    async fn serve(
        &self,
        inbound: Arc<dyn Inbound>,
        shutdown: Shutdown,
    ) -> Result<(), ChannelError> {
        let client = Arc::clone(self.sender.client());
        let auth = self.sender.auth().clone();

        // 上线通知失败不致命：它只影响服务端那边的在线状态，不影响拉取。
        if let Err(error) = client.notify_start(&auth.base_url, &auth.token).await {
            if error.is_session_expired() {
                self.sender.contexts().clear();
                self.alert(SESSION_EXPIRED_HINT.to_string()).await;
                return Err(channel_error(SESSION_EXPIRED_HINT));
            }
            tracing::warn!(%error, "微信：notify_start 失败，照常开始拉取");
        }
        tracing::info!("微信：开始拉取");

        let mut cursor = String::new();
        let mut backoff = self.retry_base;
        let mut json_failures = 0usize;
        let mut alerted = false;
        let mut outcome = Ok(());

        while !shutdown.is_cancelled() {
            let started = Instant::now();
            let polled = race_cancel(
                client.get_updates(&auth.base_url, &auth.token, &cursor),
                &shutdown,
            )
            .await;
            // 停机时结束当前拉取直接返回：游标没推进，这一批下次还在。
            let Some(polled) = polled else { break };

            let updates = match polled {
                Ok(updates) => {
                    backoff = self.retry_base;
                    json_failures = 0;
                    alerted = false;
                    updates
                }
                Err(error) if error.is_session_expired() => {
                    // **停下来，不自己重登**（模块注释第 3 条）。
                    tracing::warn!("微信：会话过期，停止拉取");
                    self.sender.contexts().clear();
                    self.alert(SESSION_EXPIRED_HINT.to_string()).await;
                    outcome = Err(channel_error(SESSION_EXPIRED_HINT));
                    break;
                }
                Err(error) => {
                    if matches!(error, WeChatBotError::Json(_)) {
                        json_failures += 1;
                        tracing::warn!(%error, json_failures, "微信：服务端返回解析不了");
                        if json_failures >= JSON_ALERT_THRESHOLD && !alerted {
                            alerted = true;
                            self.alert(json_alert(json_failures)).await;
                        }
                    } else {
                        tracing::warn!(%error, "微信：拉取失败，退避后重试");
                    }
                    if sleep_or_cancel(backoff, &shutdown).await {
                        break;
                    }
                    backoff = (backoff * 2).min(self.retry_cap);
                    continue;
                }
            };

            let handled = updates.msgs.len();
            for wire in &updates.msgs {
                self.handle_wire(inbound.as_ref(), wire).await;
                if shutdown.is_cancelled() {
                    break;
                }
            }

            // **这一行必须在处理之后**：推进游标就是向 iLink 签收（模块注释第 1 条）。
            // 空的 `get_updates_buf` 不推进——那正是 SDK 会把同一批原样再取一次的那条
            // 路径，去重集合挡住它。
            if !updates.get_updates_buf.is_empty() {
                cursor = updates.get_updates_buf;
            }

            // 一条都没有（空批，或者对端把处理过的又给了一遍）：踩一下地板，别把一个
            // 不正常的对端变成一个忙等。
            if handled == 0 {
                let elapsed = started.elapsed();
                if elapsed < self.idle_floor
                    && sleep_or_cancel(self.idle_floor - elapsed, &shutdown).await
                {
                    break;
                }
            }
        }

        // 下线通知：会话已经过期时就不必再打一次了。
        if outcome.is_ok()
            && let Err(error) = client.notify_stop(&auth.base_url, &auth.token).await
        {
            tracing::warn!(%error, "微信：notify_stop 失败，按非致命处理");
        }
        tracing::info!("微信：停止拉取");
        outcome
    }
}

/// 连续解析失败的告警文案。
fn json_alert(failures: usize) -> String {
    format!(
        "微信渠道连续 {failures} 次解析不了 iLink 的返回（多半是服务端新增了 \
         message_type / message_state 的枚举值，SDK 没有 unknown 兜底）：拉取仍在退避重试，\
         但消息可能一直进不来。见 komo 日志里的 wechat 行。"
    )
}

fn channel_error(message: impl Into<String>) -> ChannelError {
    ChannelError {
        channel: "wechat".into(),
        message: message.into(),
    }
}

/// 见过的去重键：一个有上限的集合 + 一条淘汰顺序。
#[derive(Debug, Default)]
struct SeenKeys {
    keys: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenKeys {
    /// 记下它。返回 `false` = 之前见过。
    fn remember(&mut self, key: &str) -> bool {
        if self.keys.contains(key) {
            return false;
        }
        while self.order.len() >= SEEN_KEYS_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.keys.remove(&oldest);
            }
        }
        self.keys.insert(key.to_string());
        self.order.push_back(key.to_string());
        true
    }
}

/// 把一个 future 和停机标志放在一起等。返回 `None` = 该停了。
async fn race_cancel<F: std::future::Future>(future: F, shutdown: &Shutdown) -> Option<F::Output> {
    tokio::pin!(future);
    loop {
        if shutdown.is_cancelled() {
            return None;
        }
        tokio::select! {
            output = &mut future => return Some(output),
            _ = tokio::time::sleep(CANCEL_POLL) => {}
        }
    }
}

/// 睡一会儿，中途可被停机打断。返回 `true` = 该停了。
async fn sleep_or_cancel(duration: Duration, shutdown: &Shutdown) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if shutdown.is_cancelled() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(CANCEL_POLL.min(deadline - now)).await;
    }
}

// ---------------------------------------------------------------- 工厂

/// 按当前配置造微信渠道（§3 热重载第 3 步）。
pub struct WeChatFactory {
    alerts: Option<Arc<dyn AlertSink>>,
}

impl std::fmt::Debug for WeChatFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeChatFactory")
            .field("alerts", &self.alerts.is_some())
            .finish()
    }
}

impl Default for WeChatFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl WeChatFactory {
    pub fn new() -> Self {
        WeChatFactory { alerts: None }
    }

    /// 接上告警口——连续 JSON 错误与会话过期从这里报到 home chat（§11.4 末）。
    pub fn with_alerts(alerts: Arc<dyn AlertSink>) -> Self {
        WeChatFactory {
            alerts: Some(alerts),
        }
    }

    /// 凭证在哪（§12：`~/.komo/wechat/credentials.json`）。
    ///
    /// **不在 `.env` 里**：微信的凭证是 `komo channel wechat login` 写出来的一个文件，
    /// 不是一个变量（§11.2 的 dotenv 注释说的就是这件事）。
    pub fn credentials_path(snapshot: &ConfigSnapshot) -> PathBuf {
        login::credentials_path(&snapshot.start_only.data_dir)
    }
}

#[async_trait]
impl ChannelFactory for WeChatFactory {
    fn platform(&self) -> ChannelPlatform {
        ChannelPlatform::Wechat
    }

    /// `Ok(None)` = 这份配置下微信不启用。
    ///
    /// 凭证文件不存在也是 `None` 而不是错误：`[channels.wechat] enabled = true` 但还没
    /// 扫过码是装机途中的正常一刻，它应该让微信这一个渠道不起来，而不是让整个 gateway
    /// 起不来。文件**存在却读不出来**才是错误——那是一个要人去看的坏文件。
    fn build(
        &self,
        snapshot: &ConfigSnapshot,
        _secrets: &Secrets,
    ) -> Result<Option<BuiltChannel>, ChannelError> {
        let config = snapshot.channels.wechat.clone();
        if !config.enabled {
            return Ok(None);
        }
        let path = Self::credentials_path(snapshot);
        if !path.exists() {
            tracing::warn!(
                path = %path.display(),
                "微信：还没有凭证文件，这个渠道不起来（跑 `komo channel wechat login`）"
            );
            return Ok(None);
        }
        let channel = Arc::new(WeChatChannel::new(&path, config)?.with_alerts(self.alerts.clone()));
        let sender = channel.sender() as Arc<dyn ChannelSender>;
        Ok(Some(BuiltChannel {
            channel: channel as Arc<dyn Channel>,
            sender,
        }))
    }

    /// `komo channel probe`：`notify_start` + `notify_stop`（§11.5）。**不消费消息**。
    async fn probe(
        &self,
        snapshot: &ConfigSnapshot,
        _secrets: &Secrets,
    ) -> Result<String, ChannelError> {
        probe(&Self::credentials_path(snapshot)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use fake::{Behavior, FakeILink, RecordingAlerts, RecordingInbound, TEST_TOKEN, text_wire};
    use komo_kernel::types::chat::PeerId;

    fn channel(fake: &FakeILink) -> WeChatChannel {
        fake.channel().tuned()
    }

    /// 跑 `serve`，等到 `done` 为真（或超时），然后停机并等它返回。
    async fn serve_until<F>(
        channel: WeChatChannel,
        inbound: Arc<dyn Inbound>,
        done: F,
    ) -> Result<(), ChannelError>
    where
        F: Fn() -> bool,
    {
        let shutdown = Shutdown::new();
        let task = {
            let shutdown = shutdown.clone();
            tokio::spawn(async move { channel.serve(inbound, shutdown).await })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("serve 要在停机后返回")
            .expect("serve 不该 panic")
    }

    // ① 同一个 client_id 被重投两次 → Inbound 只收一次。
    #[tokio::test]
    async fn a_redelivered_message_reaches_the_dispatcher_once() {
        // 这个假服务端回了消息却回空游标：SDK 不推进游标，下一轮原样再来一次
        //（spike §2.5 的第二条重投来源）。
        let fake = FakeILink::start_with(
            Behavior::replaying(),
            vec![text_wire("wxid_op", "cid-1", "在吗")],
        )
        .await;
        let recorder = RecordingInbound::new(fake.log());

        serve_until(channel(&fake), recorder.clone(), || {
            fake.call_count("/ilink/bot/getupdates") >= 4
        })
        .await
        .expect("重投不是错误");

        let received = recorder.received();
        assert_eq!(received.len(), 1, "重投的那几次都被跳过了：{received:?}");
        assert_eq!(received[0].request_key.as_str(), "wechat:wxid_op:cid-1");
        assert_eq!(received[0].peer.to_string(), "wechat:wxid_op");
        assert!(received[0].is_private, "微信只有 DM");
    }

    #[tokio::test]
    async fn a_new_message_is_handled_before_the_cursor_moves() {
        let fake = FakeILink::start_with(
            Behavior::default(),
            vec![text_wire("wxid_op", "cid-1", "在吗")],
        )
        .await;
        let recorder = RecordingInbound::new(fake.log());

        serve_until(channel(&fake), recorder.clone(), || {
            fake.call_count("/ilink/bot/getupdates") >= 2
        })
        .await
        .expect("正常拉取");

        assert_eq!(recorder.received().len(), 1);
        let entries = fake.log().entries();
        let handled = entries
            .iter()
            .position(|e| e == "handle:end")
            .expect("处理过");
        // 第二次 getupdates（带上新游标的那一次）在处理之后——ack 在处理之后（§11.1）。
        let acks = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| *e == "call:/ilink/bot/getupdates")
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert!(acks.len() >= 2, "{entries:?}");
        assert!(acks[1] > handled, "推进游标就是签收：{entries:?}");

        let polls = fake.calls_to("/ilink/bot/getupdates");
        assert_eq!(polls[0]["get_updates_buf"], "", "第一次是空游标");
        assert_eq!(polls[1]["get_updates_buf"], "c1", "处理完才带上新游标");
    }

    #[tokio::test]
    async fn a_command_reply_goes_back_to_the_sender() {
        let fake = FakeILink::start_with(
            Behavior::default(),
            vec![text_wire("wxid_op", "cid-1", "/pending")],
        )
        .await;
        let recorder = RecordingInbound::with_ack(
            fake.log(),
            InboundAck::Replied {
                text: "没有待处理的审批".into(),
            },
        );

        serve_until(channel(&fake), recorder.clone(), || {
            fake.call_count("/ilink/bot/sendmessage") >= 1
        })
        .await
        .expect("正常拉取");

        let sends = fake.calls_to("/ilink/bot/sendmessage");
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0]["msg"]["to_user_id"], "wxid_op");
        assert_eq!(
            sends[0]["msg"]["item_list"][0]["text_item"]["text"],
            "没有待处理的审批"
        );
        // 回执用的是这条消息自带的令牌——入站与出站共用同一张表。
        assert_eq!(sends[0]["msg"]["context_token"], "ct-wxid_op");
    }

    #[tokio::test]
    async fn a_dispatcher_error_still_says_something_back() {
        let fake = FakeILink::start_with(
            Behavior::default(),
            vec![text_wire("wxid_op", "cid-1", "在吗")],
        )
        .await;
        let recorder = RecordingInbound::failing(fake.log());

        serve_until(channel(&fake), recorder.clone(), || {
            fake.call_count("/ilink/bot/sendmessage") >= 1
        })
        .await
        .expect("Dispatcher 出错不该让渠道退出");

        let sends = fake.calls_to("/ilink/bot/sendmessage");
        assert_eq!(
            sends[0]["msg"]["item_list"][0]["text_item"]["text"],
            INTERNAL_ERROR_HINT
        );
    }

    // ⑥ 连续 5 次 JSON 错误触发一次告警、之后不重复刷屏。
    #[tokio::test]
    async fn five_unparseable_replies_raise_exactly_one_alert() {
        let fake = FakeILink::start(Behavior::bad_enum()).await;
        let alerts = RecordingAlerts::new();
        let recorder = RecordingInbound::new(fake.log());
        let channel = channel(&fake).with_alerts(Some(alerts.clone() as Arc<dyn AlertSink>));

        serve_until(channel, recorder.clone(), || {
            fake.call_count("/ilink/bot/getupdates") >= JSON_ALERT_THRESHOLD * 3
        })
        .await
        .expect("解析不了不是致命错误，退避重试");

        let raised = alerts.alerts();
        assert_eq!(raised.len(), 1, "只报一次，不刷屏：{raised:?}");
        assert!(raised[0].contains("message_type"), "{}", raised[0]);
        assert!(recorder.received().is_empty(), "一条都没解析出来");
    }

    #[tokio::test]
    async fn an_unparseable_reply_without_an_alert_sink_only_logs() {
        let fake = FakeILink::start(Behavior::bad_enum()).await;
        let recorder = RecordingInbound::new(fake.log());
        // 没接告警口：不 panic，照常退避重试。
        serve_until(channel(&fake), recorder, || {
            fake.call_count("/ilink/bot/getupdates") > JSON_ALERT_THRESHOLD
        })
        .await
        .expect("没有告警口也照常跑");
    }

    // ⑦ errcode -14 → serve 返回 Err 且提示重登。
    #[tokio::test]
    async fn an_expired_session_stops_the_channel_and_asks_for_a_new_login() {
        let fake = FakeILink::start(Behavior::session_expired()).await;
        let alerts = RecordingAlerts::new();
        let recorder = RecordingInbound::new(fake.log());
        let channel = channel(&fake).with_alerts(Some(alerts.clone() as Arc<dyn AlertSink>));
        let sender = channel.sender();
        sender
            .contexts()
            .remember_wire(&text_wire("wxid_op", "cid-1", "在吗"));

        let shutdown = Shutdown::new();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            channel.serve(recorder, shutdown.clone()),
        )
        .await
        .expect("会话过期要让 serve 立刻返回")
        .expect_err("会话过期是 serve 的错误");

        assert!(
            error.to_string().contains("komo channel wechat login"),
            "{error}"
        );
        assert_eq!(alerts.alerts().len(), 1, "操作者要被告知");
        assert!(alerts.alerts()[0].contains("-14"));
        assert!(sender.contexts().is_empty(), "-14 清空整张令牌表");
        // **不自己重登**：一个后台进程没有终端可以扫码。
        assert_eq!(fake.call_count("/ilink/bot/get_bot_qrcode"), 0);
        assert!(
            fake.call_count("/ilink/bot/getupdates") <= 1,
            "停下来，不接着拉"
        );
    }

    // ⑧ shutdown 后 serve 返回。
    #[tokio::test]
    async fn serve_returns_after_shutdown() {
        let fake = FakeILink::start(Behavior::default()).await;
        let recorder = RecordingInbound::new(fake.log());
        serve_until(channel(&fake), recorder, || {
            fake.call_count("/ilink/bot/getupdates") >= 1
        })
        .await
        .expect("停机是正常退出");

        // 走的是"下线"那条路：服务端知道机器人不在了。
        assert_eq!(fake.call_count("/ilink/bot/msg/notifystart"), 1);
        assert_eq!(fake.call_count("/ilink/bot/msg/notifystop"), 1);
    }

    #[tokio::test]
    async fn an_idle_channel_does_not_busy_loop() {
        let fake = FakeILink::start(Behavior::default()).await;
        let recorder = RecordingInbound::new(fake.log());
        let started = Instant::now();
        serve_until(channel(&fake), recorder, || {
            fake.call_count("/ilink/bot/getupdates") >= 3
        })
        .await
        .expect("空拉取");
        // 地板是 5ms（tuned），三轮至少 10ms——没有地板时这里会是几微秒。
        assert!(started.elapsed() >= Duration::from_millis(10));
    }

    // probe：只探活，不消费消息。
    #[tokio::test]
    async fn probe_pings_and_unpings_without_consuming_messages() {
        let fake = FakeILink::start_with(
            Behavior::default(),
            vec![text_wire("wxid_op", "cid-1", "在吗")],
        )
        .await;
        let home = tempfile::tempdir().expect("临时目录");
        let path = login::credentials_path(home.path());
        login::save_credentials(&path, &fake.credentials()).expect("写凭证");

        let report = probe(&path).await.expect("凭证有效");
        assert!(report.contains("notify_start"), "{report}");
        assert!(!report.contains(TEST_TOKEN), "报告里不许有 token：{report}");
        assert!(!report.contains("acct"), "也不许有 accountId：{report}");
        assert_eq!(fake.call_count("/ilink/bot/msg/notifystart"), 1);
        assert_eq!(fake.call_count("/ilink/bot/msg/notifystop"), 1);
        assert_eq!(
            fake.call_count("/ilink/bot/getupdates"),
            0,
            "probe 绝不能推进游标"
        );
    }

    #[tokio::test]
    async fn probe_says_so_when_the_credentials_expired() {
        let fake = FakeILink::start(Behavior::probe_expired()).await;
        let home = tempfile::tempdir().expect("临时目录");
        let path = login::credentials_path(home.path());
        login::save_credentials(&path, &fake.credentials()).expect("写凭证");

        let error = probe(&path).await.expect_err("凭证失效");
        assert!(
            error.to_string().contains("komo channel wechat login"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn probe_without_a_credentials_file_names_the_path() {
        let home = tempfile::tempdir().expect("临时目录");
        let error = probe(&login::credentials_path(home.path()))
            .await
            .expect_err("没有凭证");
        assert!(error.to_string().contains("credentials.json"), "{error}");
    }

    // 工厂：enabled + 凭证文件在 → Some；否则 None。
    #[test]
    fn the_factory_builds_only_when_enabled_and_logged_in() {
        let home = tempfile::tempdir().expect("临时目录");
        let mut snapshot = crate::service::test_support::sample_snapshot();
        snapshot.start_only.data_dir = home.path().to_path_buf();
        let secrets = Secrets::new();
        let factory = WeChatFactory::new();

        // 没 enabled。
        assert!(
            factory
                .build(&snapshot, &secrets)
                .expect("不报错")
                .is_none()
        );

        // enabled 但还没扫过码。
        snapshot.channels.wechat = ChannelConfig {
            enabled: true,
            allow_from: vec![PeerId::new("wxid_op")],
            home_chat: Some(PeerId::new("wxid_op")),
            groups: Vec::new(),
        };
        assert!(
            factory
                .build(&snapshot, &secrets)
                .expect("不报错")
                .is_none(),
            "没有凭证文件时这一个渠道不起来，而不是整个 gateway 起不来"
        );

        // 扫过码了。
        let path = WeChatFactory::credentials_path(&snapshot);
        assert!(path.ends_with("wechat/credentials.json"), "§12 的位置");
        login::save_credentials(
            &path,
            &Credentials {
                token: "tok".into(),
                base_url: "https://idc.example".into(),
                account_id: "acct".into(),
                user_id: "wxid_bot".into(),
                saved_at: Some("0Z".into()),
            },
        )
        .expect("写凭证");
        let built = factory
            .build(&snapshot, &secrets)
            .expect("不报错")
            .expect("起得来");
        assert_eq!(built.channel.name(), "wechat");
        assert_eq!(built.sender.platform(), ChannelPlatform::Wechat);
    }

    #[test]
    fn a_corrupt_credentials_file_is_an_error_not_a_silent_none() {
        let home = tempfile::tempdir().expect("临时目录");
        let mut snapshot = crate::service::test_support::sample_snapshot();
        snapshot.start_only.data_dir = home.path().to_path_buf();
        snapshot.channels.wechat = ChannelConfig {
            enabled: true,
            ..ChannelConfig::default()
        };
        let path = WeChatFactory::credentials_path(&snapshot);
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("建目录");
        std::fs::write(&path, "这不是 JSON").expect("写坏文件");

        let error = WeChatFactory::new()
            .build(&snapshot, &Secrets::new())
            .expect_err("坏文件要有人去看");
        assert!(error.to_string().contains("credentials.json"), "{error}");
    }

    #[test]
    fn the_seen_set_has_a_ceiling_and_keeps_its_order() {
        let mut seen = SeenKeys::default();
        assert!(seen.remember("a"));
        assert!(!seen.remember("a"), "第二次就是重投");
        for index in 0..SEEN_KEYS_CAP {
            seen.remember(&format!("k{index}"));
        }
        assert!(seen.remember("a"), "淘汰之后它又是新的了");
        assert!(seen.keys.len() <= SEEN_KEYS_CAP);
    }
}

#[cfg(test)]
mod live {
    //! 唯一一条会真的联网的测试——spike §5 的 V1（凭证是否仍然有效）。
    //!
    //! `#[ignore]` **加**一个环境变量：`cargo test` 不会碰它，`--ignored` 也不会，只有
    //! 人明确地跑
    //!
    //! ```text
    //! KOMO_WECHAT_LIVE_PROBE=1 cargo test -p komo-gateway --lib -- --ignored live_probe
    //! ```
    //!
    //! 才会打到真的 iLink。它只做 `notify_start` + `notify_stop`：**不拉取消息**（那会
    //! 推进游标、把没处理的消息签收掉），**不发送**。
    //!
    //! 断言里有一条是"报告里不许出现凭证"——这条测试自己会把报告打到 stdout，所以它
    //! 必须先证明那句话是安全的。

    use super::*;

    #[tokio::test]
    #[ignore = "会真的联网：KOMO_WECHAT_LIVE_PROBE=1 才跑"]
    async fn live_probe_against_the_real_credentials() {
        if std::env::var("KOMO_WECHAT_LIVE_PROBE").as_deref() != Ok("1") {
            eprintln!("跳过：没有 KOMO_WECHAT_LIVE_PROBE=1");
            return;
        }
        let home = std::env::var("KOMO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").expect("HOME")).join(".komo"));
        let path = login::credentials_path(&home);
        let credentials = login::load_credentials(&path).expect("读凭证");

        match probe(&path).await {
            Ok(report) => {
                assert!(!report.contains(&credentials.token), "报告里有 token");
                assert!(!report.contains(&credentials.user_id), "报告里有 userId");
                assert!(
                    !report.contains(&credentials.account_id),
                    "报告里有 accountId"
                );
                println!("probe: {report}");
            }
            Err(error) => {
                let message = error.to_string();
                assert!(!message.contains(&credentials.token), "错误里有 token");
                println!("probe 失败: {message}");
            }
        }
    }
}
