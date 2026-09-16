//! 飞书 ws 长连接与 `event_id` 去重（§11.1、§11.5）。
//!
//! 一条 ws 长连接（在自己的线程上，见 [`ws`]）把原始事件负载交给 [`FeishuChannel::serve`]，
//! 三条规矩：
//!
//! 1. **ack 的位置与 §11.1 有一条已知偏差**。§11.1 要求 ack 在 `Inbound::handle` 返回
//!    之后发；openlark 0.20.0 在帧层就应答了——`EventDispatcherHandler` 的
//!    `payload_sender` 分支把负载丢进 channel 就返回 `Ok`，帧层据此写回
//!    `EventAck { code: 200 }`。改不动的理由与后果写在 [`ws`] 的模块注释里；这里的补偿
//!    是下面这条。
//! 2. **进程内按 `event_id` 去重**。飞书是至少一次投递，ws 断线重连与 3 秒超时都会重推
//!    （spike callbacks.md 1b/1c/1f），而 ack 已经先发了，所以同一条事件在同一个进程里
//!    还是可能来两次。durable 的那一层在 Dispatcher（§8.5 的请求键），这里这一层只挡
//!    同进程重投，让 `Inbound` 只看见一次。
//! 3. **去重键只管平台重投**。同一个人连点两次「批准」是两条合法输入、两个不同的
//!    `event_id`；挡住它的是审批按 `approval_id` 的幂等，不是这里（spike callbacks.md §2）。
//!
//! 渠道只依赖 kernel 的类型与 `Channel` / `Inbound` 两个 trait（§11.5）：它不知道
//! Session 是什么，也不读环境变量——`app_id` / `app_secret` 由 [`FeishuFactory`] 从
//! secrets 取出来传进构造函数。
//!
//! **未在真机验证**（本机没有飞书凭证）：ws 上卡片回调的投递形态。openlark 0.20.0 的
//! `FrameHandler::handle_data_frame` 对 `type: "card"` 的数据帧是 `debug!("Card frame
//! received, skipping")` 后直接丢弃，**不进事件分发**；只有 `type: "event"` 的帧会到
//! 我们手里。所以按钮回调能不能通过 ws 到达，取决于飞书把 `card.action.trigger` 归到哪
//! 一种帧。本模块的解析对两条路都成立（它只看负载里的 `header.event_type`），但若飞书
//! 走的是 `card` 帧，那就要 openlark 先支持——见报告的"需要编排者做"。文本命令
//! `/approve <short_id>` 不受影响，它走的是 `im.message.receive_v1`。

pub mod api;
pub mod inbound;
pub mod send;
pub mod ws;

#[cfg(test)]
pub mod fake;

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc;

use komo_kernel::protocol::InboundAck;
use komo_kernel::protocol::config::{ChannelConfig, ConfigSnapshot};
use komo_kernel::traits::{Channel, ChannelError, Inbound, Shutdown};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform};
use komo_runtime::config::Secrets;

use crate::channels::{BuiltChannel, ChannelFactory, ChannelSender};

pub use api::{BotIdentity, FeishuApi, FeishuError, TokenProbe};
pub use send::FeishuSender;

use inbound::{EventKind, ParsedEvent};

/// `.env` 里的两个键（§11.2：凭证只在 `.env`，配置只在 config.toml）。
pub const APP_ID_KEY: &str = "FEISHU_APP_ID";
pub const APP_SECRET_KEY: &str = "FEISHU_APP_SECRET";

/// 查一次停机标志的间隔。`Shutdown` 是个可查询的标志而不是一个可等待的信号
/// （kernel 不依赖 tokio），所以每个 await 都和它赛跑。
const CANCEL_POLL: Duration = Duration::from_millis(100);

pub(crate) const RETRY_BASE: Duration = Duration::from_secs(1);
pub(crate) const RETRY_CAP: Duration = Duration::from_secs(60);

/// 进程内记住多少个 `event_id`。飞书最多重推 4 次、跨度 6 小时，但在那之前这条事件早就
/// 进了 durable 的请求键；这里只需要盖住"连接抖一下、同一批再来一次"。
const SEEN_EVENTS_CAP: usize = 4096;

/// Dispatcher 自己出错时回给发送者的话。**不静默丢弃**（§11.4 的同一条理由）。
const INTERNAL_ERROR_HINT: &str = "komo 没能处理这条消息（内部错误），请稍后再说一次。";

/// `komo channel probe` 的连通性核对：拿一次 tenant token（§11.5）。
///
/// **不回报 token 本身**，只回报它还能用多久。
pub async fn probe(
    app_id: &str,
    app_secret: &str,
    http: reqwest::Client,
) -> Result<TokenProbe, FeishuError> {
    FeishuApi::new(app_id, app_secret, http).probe_token().await
}

/// `serve` 的事件入口。
///
/// 生产是 ws 线程；测试直接塞——ws 那一侧是 openlark 的协议实现，本机又没有凭证，把
/// "事件从哪来"做成一个接缝，`serve` 的去重、@提及、回执与停机就都能在没有网络的情况
/// 下断言。
enum EventSource {
    /// openlark 的 ws 长连接。
    Ws { app_id: String, app_secret: String },
    /// 测试注入。
    #[cfg(test)]
    Injected(mpsc::UnboundedReceiver<Vec<u8>>),
}

pub struct FeishuChannel {
    sender: Arc<FeishuSender>,
    /// 构造那一刻的配置快照。**不拿它做准入判定**——见 [`inbound`] 的模块注释。
    config: ChannelConfig,
    /// `serve` 取走它。取过第二次就是"同一个渠道跑了两遍"。
    source: Mutex<Option<EventSource>>,
    /// 同进程的 `event_id` 去重。
    seen: Mutex<SeenEvents>,
}

impl std::fmt::Debug for FeishuChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeishuChannel").finish_non_exhaustive()
    }
}

impl FeishuChannel {
    /// `app_id` / `app_secret` 由接线（[`FeishuFactory`]）从 `.env` 读出来传进来；
    /// **渠道自己不读环境变量**（§11.2）。
    pub fn new(
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        config: ChannelConfig,
        http: reqwest::Client,
    ) -> Self {
        let app_id = app_id.into();
        let app_secret = app_secret.into();
        let api = Arc::new(FeishuApi::new(app_id.clone(), app_secret.clone(), http));
        Self::with_api(api, config, EventSource::Ws { app_id, app_secret })
    }

    fn with_api(api: Arc<FeishuApi>, config: ChannelConfig, source: EventSource) -> Self {
        Self {
            sender: Arc::new(FeishuSender::with_api(api)),
            config,
            source: Mutex::new(Some(source)),
            seen: Mutex::new(SeenEvents::default()),
        }
    }

    /// 测试用：事件从一个 channel 来，不建 ws。
    #[cfg(test)]
    fn injected(api: Arc<FeishuApi>, events: mpsc::UnboundedReceiver<Vec<u8>>) -> Self {
        Self::with_api(api, ChannelConfig::default(), EventSource::Injected(events))
    }

    /// 出站的那一半。Notifier 用它主动投递（§11.4）；它与入站共用同一个 [`FeishuApi`]，
    /// 所以也共用那份 tenant token 缓存与"审批落在哪条消息上"的进程内小账。
    pub fn sender(&self) -> Arc<FeishuSender> {
        Arc::clone(&self.sender)
    }

    pub fn config(&self) -> &ChannelConfig {
        &self.config
    }

    fn api(&self) -> &Arc<FeishuApi> {
        self.sender.api()
    }

    /// `bot/v3/info`，失败就退避重试。**认证失败不重试**：app secret 错了不会自己变对。
    ///
    /// 群里剥 @提及 要机器人自己的 `open_id`，所以这一步在开连接之前——没有它，群消息只
    /// 能一律不响应。
    async fn resolve_identity(&self, shutdown: &Shutdown) -> Result<BotIdentity, ChannelError> {
        let mut backoff = RETRY_BASE;
        loop {
            if shutdown.is_cancelled() {
                return Err(channel_error("停机时还没拿到 bot/v3/info"));
            }
            match self.api().bot_info().await {
                Ok(identity) => return Ok(identity),
                Err(error) if error.is_retryable() => {
                    tracing::warn!(%error, "飞书：bot/v3/info 失败，稍后重试");
                    if sleep_or_cancel(backoff, shutdown).await {
                        return Err(channel_error("停机时还没拿到 bot/v3/info"));
                    }
                    backoff = (backoff * 2).min(RETRY_CAP);
                }
                Err(error) => return Err(channel_error(error.to_string())),
            }
        }
    }

    fn take_source(
        &self,
        shutdown: &Shutdown,
    ) -> Result<(mpsc::UnboundedReceiver<Vec<u8>>, Option<ws::WsThread>), ChannelError> {
        let source = self
            .source
            .lock()
            .expect("事件源锁")
            .take()
            .ok_or_else(|| channel_error("同一个渠道不能 serve 两次"))?;
        Ok(match source {
            EventSource::Ws { app_id, app_secret } => {
                let (events, thread) = ws::spawn(app_id, app_secret, shutdown.clone());
                (events, Some(thread))
            }
            #[cfg(test)]
            EventSource::Injected(events) => (events, None),
        })
    }

    async fn handle_payload(&self, inbound: &dyn Inbound, bot: &BotIdentity, payload: &[u8]) {
        let Some(ParsedEvent { event_id, kind }) = inbound::parse(payload) else {
            return;
        };
        // 平台重投（至少一次投递 + ws 重连补推）在这里挡住。**在分发之前**：进了
        // Dispatcher 才发现是重投，那一层给的是 `Duplicate`，而这一层要的是"根本没来过"。
        if !self.seen.lock().expect("去重集合").remember(&event_id) {
            tracing::debug!(%event_id, "飞书：这条事件已经处理过了");
            return;
        }

        let request_key = inbound::request_key(&event_id);
        let message = match kind {
            EventKind::Message(event) => inbound::from_message(&event, &bot.open_id, request_key),
            EventKind::CardAction(event) => {
                // 回调载荷里没有会话类型，只能问一次平台（见 `api::chat_is_private`）。
                let is_private = self.chat_is_private(&event.context.open_chat_id).await;
                inbound::from_card_action(&event, is_private, request_key)
            }
        };

        let Some(message) = message else {
            // 群里没 @机器人、非文本消息、认不出的回调负载：`Ignored`，不进 Dispatcher。
            tracing::debug!(%event_id, "飞书：忽略");
            return;
        };

        let peer = message.peer.clone();
        match inbound.handle(message).await {
            Ok(InboundAck::Replied { text }) => self.reply(&peer, &text).await,
            Ok(InboundAck::Rejected { hint }) => self.reply(&peer, &hint).await,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%event_id, %error, "飞书：Dispatcher 出错");
                // TODO(decide: 出错时这条 event_id 照样留在去重集合里——否则一个确定性的
                // 错误会让同一条消息在重推时无限重跑。代价是这条消息只剩这句回执。)
                self.reply(&peer, INTERNAL_ERROR_HINT).await;
            }
        }
    }

    /// 这个会话是不是私聊。查不到时按**群**算。
    ///
    /// 猜"私聊"会让一个没有列进 `groups` 的群里的按钮也能下命令（§11.2 要求群必须在
    /// 名单里），那是准入被绕过；猜"群"最坏只是这次按钮不生效，而同一条审批还能用
    /// `/approve <short_id>` 回答——一条普通消息带着真正的 `chat_type`。
    async fn chat_is_private(&self, chat_id: &str) -> bool {
        match self.api().chat_is_private(chat_id).await {
            Ok(is_private) => is_private,
            Err(error) => {
                tracing::warn!(%error, "飞书：问不到会话类型，按群会话处理");
                false
            }
        }
    }

    async fn reply(&self, peer: &ChannelPeer, text: &str) {
        if let Err(error) = self.sender.send_text(&peer.chat_id, text).await {
            tracing::warn!(%peer, %error, "飞书：回执没送出去");
        }
    }
}

#[async_trait]
impl Channel for FeishuChannel {
    fn name(&self) -> &'static str {
        "feishu"
    }

    async fn serve(
        &self,
        inbound: Arc<dyn Inbound>,
        shutdown: Shutdown,
    ) -> Result<(), ChannelError> {
        let bot = self.resolve_identity(&shutdown).await?;
        tracing::info!(bot = %bot.app_name, "飞书：开始接收事件");

        let (mut events, thread) = self.take_source(&shutdown)?;
        while !shutdown.is_cancelled() {
            // 停机时结束等待直接返回：没取走的负载随 channel 一起丢掉——它们还没被处理，
            // 但 ack 早已发出（见模块注释第 1 条），所以这里没有"先确认后丢失"的新问题。
            let Some(payload) = race_cancel(events.recv(), &shutdown).await else {
                break;
            };
            let Some(payload) = payload else {
                // ws 线程没了（重连循环退出）。
                tracing::info!("飞书：事件源结束");
                break;
            };
            self.handle_payload(inbound.as_ref(), &bot, &payload).await;
        }

        if let Some(thread) = thread {
            thread.join().await;
        }
        tracing::info!("飞书：停止接收事件");
        Ok(())
    }
}

/// 见过的 `event_id`：一个有上限的集合 + 一条淘汰顺序。
#[derive(Debug, Default)]
struct SeenEvents {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenEvents {
    /// 记下它。返回 `false` = 之前见过。
    fn remember(&mut self, event_id: &str) -> bool {
        if self.ids.contains(event_id) {
            return false;
        }
        while self.order.len() >= SEEN_EVENTS_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }
        self.ids.insert(event_id.to_string());
        self.order.push_back(event_id.to_string());
        true
    }
}

/// 按当前配置造飞书渠道（§3 热重载第 3 步）。
#[derive(Debug, Default)]
pub struct FeishuFactory {
    http: reqwest::Client,
}

impl FeishuFactory {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// 共用一个 HTTP 客户端（连接池、超时都由接线那一侧定）。
    pub fn with_http(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[async_trait]
impl ChannelFactory for FeishuFactory {
    fn platform(&self) -> ChannelPlatform {
        ChannelPlatform::Feishu
    }

    /// `Ok(None)` = 这份配置下飞书不启用。
    ///
    /// 缺凭证也是 `None` 而不是错误：`[channels.feishu] enabled = true` 但 `.env` 里还没
    /// 填 app secret 是装机途中的正常一刻，它应该让飞书这一个渠道不起来，而不是让整个
    /// gateway 起不来。
    fn build(
        &self,
        snapshot: &ConfigSnapshot,
        secrets: &Secrets,
    ) -> Result<Option<BuiltChannel>, ChannelError> {
        let config = snapshot.channels.feishu.clone();
        if !config.enabled {
            return Ok(None);
        }
        if !secrets.has(APP_ID_KEY) || !secrets.has(APP_SECRET_KEY) {
            tracing::warn!("飞书：{APP_ID_KEY} / {APP_SECRET_KEY} 还没配，这个渠道不起来");
            return Ok(None);
        }
        let app_id = secrets.get(APP_ID_KEY).unwrap_or_default().trim();
        let app_secret = secrets.get(APP_SECRET_KEY).unwrap_or_default().trim();

        let channel = Arc::new(FeishuChannel::new(
            app_id,
            app_secret,
            config,
            self.http.clone(),
        ));
        let sender = channel.sender() as Arc<dyn ChannelSender>;
        Ok(Some(BuiltChannel {
            channel: channel as Arc<dyn Channel>,
            sender,
        }))
    }

    /// `komo channel probe`：拿一次 tenant token（§11.5）。
    ///
    /// **不建 ws**：一次长连接会真的占掉一个连接名额，而"凭证对不对、网络通不通"这两件
    /// 事 token 已经答完了。
    async fn probe(
        &self,
        _snapshot: &ConfigSnapshot,
        secrets: &Secrets,
    ) -> Result<String, ChannelError> {
        if !secrets.has(APP_ID_KEY) || !secrets.has(APP_SECRET_KEY) {
            return Err(channel_error(format!(
                ".env 里没有 {APP_ID_KEY} / {APP_SECRET_KEY}"
            )));
        }
        let app_id = secrets.get(APP_ID_KEY).unwrap_or_default().trim();
        let app_secret = secrets.get(APP_SECRET_KEY).unwrap_or_default().trim();
        probe(app_id, app_secret, self.http.clone())
            .await
            .map(|probe| format!("tenant token 通过：{} 秒后过期", probe.expires_in))
            .map_err(|error| channel_error(error.to_string()))
    }
}

fn channel_error(message: impl Into<String>) -> ChannelError {
    ChannelError {
        channel: "feishu".into(),
        message: message.into(),
    }
}

/// 把一个 future 和停机标志放在一起等。返回 `None` = 该停了。
pub(crate) async fn race_cancel<F: std::future::Future>(
    future: F,
    shutdown: &Shutdown,
) -> Option<F::Output> {
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
pub(crate) async fn sleep_or_cancel(duration: Duration, shutdown: &Shutdown) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    use komo_kernel::types::ids::{RunId, SessionId};

    use super::fake::{
        BOT_OPEN_ID, Behavior, FakeOpenApi, RecordingInbound, card_action_event, text_event,
    };

    /// 一个事件源可注入的渠道 + 往里塞事件的那一头。
    struct Harness {
        channel: FeishuChannel,
        events: mpsc::UnboundedSender<Vec<u8>>,
    }

    fn harness(fake: &FakeOpenApi) -> Harness {
        let (events, rx) = mpsc::unbounded_channel();
        Harness {
            channel: FeishuChannel::injected(fake.api(), rx),
            events,
        }
    }

    /// 跑 `serve`，等到 `done` 为真（或超时），然后停机并等它返回。
    async fn serve_until<F>(channel: FeishuChannel, inbound: Arc<dyn Inbound>, done: F) -> Shutdown
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
        let served = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("serve 要在停机后返回")
            .expect("serve 不该 panic");
        assert!(served.is_ok(), "{served:?}");
        shutdown
    }

    // ① 同一个 event_id 被重推两次 → Inbound 只收到一次。
    #[tokio::test]
    async fn a_redelivered_event_reaches_the_dispatcher_once() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let harness = harness(&fake);
        let recorder = RecordingInbound::new(fake.log());

        let event = text_event("evt-7", "oc_1", "p2p", "ou_op", "在吗", &[]);
        harness.events.send(event.clone()).unwrap();
        harness.events.send(event).unwrap();

        serve_until(harness.channel, recorder.clone(), || {
            fake.log().position("handle:end").is_some()
        })
        .await;

        let received = recorder.received();
        assert_eq!(received.len(), 1, "重推的那一次被跳过了：{received:?}");
        assert_eq!(received[0].request_key.as_str(), "feishu:evt-7");
        assert_eq!(received[0].text, "在吗");
        assert!(received[0].is_private);
    }

    // ② card.action.trigger 转成 /approve <id> 且 request_key = feishu:{event_id}。
    #[tokio::test]
    async fn a_button_press_becomes_the_same_command_a_message_would() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let harness = harness(&fake);
        let recorder = RecordingInbound::new(fake.log());

        harness
            .events
            .send(card_action_event(
                "evt-31", "oc_dm", "ou_op", "approve", "7K2M",
            ))
            .unwrap();

        serve_until(harness.channel, recorder.clone(), || {
            !recorder.received().is_empty()
        })
        .await;

        let received = recorder.received();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].text, "/approve 7K2M");
        // 去重键与普通消息同源：`feishu:{event_id}`（§11.1）。
        assert_eq!(received[0].request_key.as_str(), "feishu:evt-31");
        assert_eq!(received[0].peer.to_string(), "feishu:oc_dm");
        assert_eq!(received[0].sender.as_str(), "ou_op");
        // 回调载荷里没有会话类型，所以问了一次平台。
        assert!(received[0].is_private, "oc_dm 在假平台上是单聊");
        assert_eq!(fake.calls_to("chats").len(), 1);
    }

    #[tokio::test]
    async fn a_button_press_in_a_chat_we_cannot_classify_counts_as_a_group() {
        let fake = FakeOpenApi::start(Behavior::refuse_chats()).await;
        let harness = harness(&fake);
        let recorder = RecordingInbound::new(fake.log());

        harness
            .events
            .send(card_action_event(
                "evt-32", "oc_dm", "ou_op", "approve", "7K2M",
            ))
            .unwrap();

        serve_until(harness.channel, recorder.clone(), || {
            !recorder.received().is_empty()
        })
        .await;

        assert!(
            !recorder.received()[0].is_private,
            "猜不出来时按群算：准入宁可严一点"
        );
    }

    // ③ 群里非 @机器人 的消息 Ignored、@机器人 的消息剥掉占位。
    #[tokio::test]
    async fn a_group_message_only_counts_when_it_names_the_bot() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let harness = harness(&fake);
        let recorder = RecordingInbound::new(fake.log());

        harness
            .events
            .send(text_event(
                "evt-41",
                "oc_g",
                "group",
                "ou_op",
                "大家早",
                &[],
            ))
            .unwrap();
        harness
            .events
            .send(text_event(
                "evt-42",
                "oc_g",
                "group",
                "ou_op",
                "@_user_1 看一下日志",
                &[("@_user_1", BOT_OPEN_ID)],
            ))
            .unwrap();

        serve_until(harness.channel, recorder.clone(), || {
            !recorder.received().is_empty()
        })
        .await;

        let received = recorder.received();
        assert_eq!(
            received.len(),
            1,
            "没 @ 的那条不进 Dispatcher：{received:?}"
        );
        assert_eq!(received[0].text, "看一下日志");
        assert!(!received[0].is_private);
        assert_eq!(received[0].request_key.as_str(), "feishu:evt-42");
    }

    // ④ 处理顺序：回执在 handle 返回之后才发。
    //
    // ack 本身不在 komo 手里（openlark 在帧层就应答了，见模块注释），所以这里能断言的是
    // "渠道自己的动作没有抢在 Dispatcher 前面"。
    #[tokio::test]
    async fn the_reply_only_goes_out_after_the_dispatcher_answers() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let harness = harness(&fake);
        let log = fake.log();
        let recorder = RecordingInbound::slow(
            log.clone(),
            Duration::from_millis(150),
            InboundAck::Replied {
                text: "没有待处理的审批".into(),
            },
        );

        harness
            .events
            .send(text_event(
                "evt-50",
                "oc_1",
                "p2p",
                "ou_op",
                "/pending",
                &[],
            ))
            .unwrap();

        serve_until(harness.channel, recorder.clone(), || {
            !fake.calls_to("messages").is_empty()
        })
        .await;

        let entries = log.entries();
        let handled = log.position("handle:end").expect("handle 跑过了");
        let replied = entries
            .iter()
            .position(|event| event == "messages")
            .expect("回执发出去了");
        assert!(replied > handled, "回执抢在 handle 之前：{entries:?}");
    }

    #[tokio::test]
    async fn a_reply_goes_straight_back_to_the_chat() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let harness = harness(&fake);
        let recorder = RecordingInbound::with_ack(
            fake.log(),
            InboundAck::Replied {
                text: "没有待处理的审批".into(),
            },
        );

        harness
            .events
            .send(text_event(
                "evt-60",
                "oc_1",
                "p2p",
                "ou_op",
                "/pending",
                &[],
            ))
            .unwrap();

        serve_until(harness.channel, recorder, || {
            !fake.calls_to("messages").is_empty()
        })
        .await;

        let sends = fake.calls_to("messages");
        assert_eq!(sends[0]["receive_id"], "oc_1");
        assert!(
            sends[0]["content"]
                .as_str()
                .unwrap()
                .contains("没有待处理的审批")
        );
    }

    #[tokio::test]
    async fn a_rejection_hint_goes_back_too() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let harness = harness(&fake);
        let recorder = RecordingInbound::with_ack(
            fake.log(),
            InboundAck::Rejected {
                hint: "feishu:oc_1 · ou_stranger 不在 allow_from 里".into(),
            },
        );

        harness
            .events
            .send(text_event(
                "evt-61",
                "oc_1",
                "p2p",
                "ou_stranger",
                "你好",
                &[],
            ))
            .unwrap();

        serve_until(harness.channel, recorder, || {
            !fake.calls_to("messages").is_empty()
        })
        .await;

        assert!(
            fake.calls_to("messages")[0]["content"]
                .as_str()
                .unwrap()
                .contains("allow_from")
        );
    }

    #[tokio::test]
    async fn a_queued_message_gets_no_immediate_reply() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let harness = harness(&fake);
        let recorder = RecordingInbound::with_ack(
            fake.log(),
            InboundAck::Queued {
                session: SessionId::from_raw("s-1"),
                run: RunId::from_raw("run-1"),
            },
        );

        harness
            .events
            .send(text_event(
                "evt-62",
                "oc_1",
                "p2p",
                "ou_op",
                "帮我查一下",
                &[],
            ))
            .unwrap();

        serve_until(harness.channel, recorder.clone(), || {
            !recorder.received().is_empty()
        })
        .await;

        assert!(
            fake.calls_to("messages").is_empty(),
            "排队的消息由 Run 结束时的投递回答，不在这里"
        );
    }

    #[tokio::test]
    async fn a_dispatcher_error_still_says_something_back() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let harness = harness(&fake);
        let recorder = RecordingInbound::failing(fake.log());

        harness
            .events
            .send(text_event("evt-63", "oc_1", "p2p", "ou_op", "在吗", &[]))
            .unwrap();

        serve_until(harness.channel, recorder, || {
            !fake.calls_to("messages").is_empty()
        })
        .await;

        assert!(
            fake.calls_to("messages")[0]["content"]
                .as_str()
                .unwrap()
                .contains("内部错误")
        );
    }

    // ⑨ shutdown 之后 serve 返回。
    #[tokio::test]
    async fn serve_returns_once_it_is_told_to_stop() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let harness = harness(&fake);
        let recorder = RecordingInbound::new(fake.log());
        // serve_until 自己就断言了"停机后 serve 在 5 秒内返回且不是错误"。
        serve_until(harness.channel, recorder, || {
            !fake.calls_to("bot").is_empty()
        })
        .await;
    }

    #[tokio::test]
    async fn bad_credentials_are_not_retried_forever() {
        // code 99991663 是不可重试的：app secret 错了不会自己变对，serve 立刻带错误返回。
        let fake = FakeOpenApi::start(Behavior::refuse_token()).await;
        let harness = harness(&fake);
        let recorder = RecordingInbound::new(fake.log());
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            harness.channel.serve(recorder, Shutdown::new()),
        )
        .await
        .expect("不该无限重试")
        .expect_err("认证失败是个错误");
        assert_eq!(error.channel, "feishu");
        assert_eq!(fake.calls_to("tenant_access_token").len(), 1, "只问一次");
    }

    #[test]
    fn the_seen_set_forgets_the_oldest_first() {
        let mut seen = SeenEvents::default();
        assert!(seen.remember("a"));
        assert!(!seen.remember("a"));
        for index in 0..SEEN_EVENTS_CAP {
            seen.remember(&format!("e{index}"));
        }
        assert!(seen.remember("a"), "淘汰之后它就是一条新事件了");
        assert!(seen.order.len() <= SEEN_EVENTS_CAP);
    }

    // ---------------------------------------------------------------- 工厂

    fn snapshot_with(enabled: bool) -> ConfigSnapshot {
        let mut snapshot = crate::service::test_support::sample_snapshot();
        snapshot.channels.feishu = ChannelConfig {
            enabled,
            ..ChannelConfig::default()
        };
        snapshot
    }

    fn credentials() -> Secrets {
        Secrets::from_pairs([(APP_ID_KEY, "cli_test"), (APP_SECRET_KEY, "shh")])
    }

    #[test]
    fn the_factory_builds_only_when_enabled_and_credentialed() {
        let factory = FeishuFactory::new();
        assert_eq!(factory.platform(), ChannelPlatform::Feishu);

        assert!(
            factory
                .build(&snapshot_with(false), &credentials())
                .unwrap()
                .is_none(),
            "enabled = false 就不起来"
        );
        assert!(
            factory
                .build(&snapshot_with(true), &Secrets::new())
                .unwrap()
                .is_none(),
            "没凭证也是不起来，不是启动失败"
        );
        assert!(
            factory
                .build(
                    &snapshot_with(true),
                    &Secrets::from_pairs([(APP_ID_KEY, "cli_test"), (APP_SECRET_KEY, "  ")])
                )
                .unwrap()
                .is_none(),
            "空白的 secret 当没配"
        );

        let built = factory
            .build(&snapshot_with(true), &credentials())
            .unwrap()
            .expect("配齐了就起来");
        assert_eq!(built.channel.name(), "feishu");
        assert_eq!(built.sender.platform(), ChannelPlatform::Feishu);
    }

    #[test]
    fn a_built_channel_keeps_the_config_snapshot_it_was_built_with() {
        let factory = FeishuFactory::new();
        let mut snapshot = snapshot_with(true);
        snapshot.channels.feishu.groups = vec![komo_kernel::types::chat::PeerId::new("oc_g")];
        let built = factory.build(&snapshot, &credentials()).unwrap().unwrap();
        // 渠道**不拿**它做准入判定（见 `inbound` 的模块注释），但热重载要靠它判断
        // "这个渠道的配置变了没有"。
        assert_eq!(built.channel.name(), "feishu");
    }

    /// 只在本机有真凭证时跑：
    /// `cargo test -p komo-gateway --lib -- --ignored probe_`。
    /// **只取一次 tenant token**：不发消息、不建 ws，输出里不出现任何凭证。
    #[tokio::test]
    #[ignore = "需要 ~/.komo/.env 里的真实 FEISHU_APP_ID / FEISHU_APP_SECRET"]
    async fn probe_reaches_the_real_open_platform() {
        let app_id = std::env::var(APP_ID_KEY).expect(APP_ID_KEY);
        let app_secret = std::env::var(APP_SECRET_KEY).expect(APP_SECRET_KEY);
        let probe = probe(&app_id, &app_secret, reqwest::Client::new())
            .await
            .expect("tenant token 应当取得到");
        println!("probe ok: expires_in={}", probe.expires_in);
        assert!(probe.expires_in > 0);
    }
}
