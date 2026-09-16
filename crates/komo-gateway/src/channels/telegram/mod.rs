//! Telegram 长轮询与 `update_id` 去重（§11.1、§11.5）。
//!
//! 一条长轮询循环，三条规矩：
//!
//! 1. **ack 在处理之后**。`offset` 只在 `Inbound::handle` 返回之后才推进——Telegram
//!    把"用一个更大的 offset 再调一次 `getUpdates`"当作确认（官方原文），所以提前推进
//!    就等于在还没处理时先签收。进程死在中间时那条 `Update` 会原样再来一次，由
//!    Dispatcher 按 `telegram:{update_id}` 去重（§11.1）。
//! 2. **去重键只管平台重投**。同一个人连点两次「批准」是两条合法输入、两个不同的
//!    `update_id`；挡住它的是审批按 `approval_id` 的幂等，不是这里
//!    （spike callbacks.md §2）。
//! 3. **`update_id` 不保证连续**。闲置一周后下一个 id 是随机选的（官方明文），所以这
//!    里只用 `offset = 最后处理的 + 1`，从不假设步长是 1。
//!
//! 渠道只依赖 kernel 的类型与 `Channel` / `Inbound` 两个 trait（§11.5）：它不知道
//! Session 是什么，也不读环境变量——token 由接线在构造时传进来。

pub mod api;
pub mod inbound;
pub mod send;

#[cfg(test)]
pub mod fake;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use komo_kernel::protocol::InboundAck;
use komo_kernel::protocol::config::ChannelConfig;
use komo_kernel::traits::{Channel, ChannelError, Inbound, Shutdown};

pub use api::{BotApi, BotIdentity, TelegramError};
pub use send::TelegramSender;

use api::Update;

/// 长轮询挂起的秒数。
const DEFAULT_POLL_TIMEOUT_SECS: u64 = 30;

/// 查一次停机标志的间隔。`Shutdown` 是个可查询的标志而不是一个可等待的信号
/// （kernel 不依赖 tokio），所以每个 await 都和它赛跑。
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// 两次空轮询之间的地板。真实的 `getUpdates` 会挂满 30 秒，立刻返回空的只可能是对端
/// 不正常——没有这条地板，那种时候循环会变成一个忙等。
const DEFAULT_IDLE_FLOOR: Duration = Duration::from_millis(250);

const DEFAULT_RETRY_BASE: Duration = Duration::from_secs(1);
const DEFAULT_RETRY_CAP: Duration = Duration::from_secs(60);

/// Dispatcher 自己出错时回给发送者的话。**不静默丢弃**（§11.4 的同一条理由）。
const INTERNAL_ERROR_HINT: &str = "komo 没能处理这条消息（内部错误），请稍后再说一次。";

/// `komo channel probe` 的连通性核对：`getMe`（§11.5）。
pub async fn probe(token: &str, http: reqwest::Client) -> Result<BotIdentity, TelegramError> {
    BotApi::new(token, http).get_me().await
}

pub struct TelegramChannel {
    sender: Arc<TelegramSender>,
    /// 构造那一刻的配置快照。**不拿它做准入判定**——见 [`inbound`] 的模块注释。
    config: ChannelConfig,
    poll_timeout_secs: u64,
    idle_floor: Duration,
    retry_base: Duration,
    retry_cap: Duration,
}

impl std::fmt::Debug for TelegramChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramChannel")
            .field("poll_timeout_secs", &self.poll_timeout_secs)
            .finish_non_exhaustive()
    }
}

impl TelegramChannel {
    /// token 由接线（gateway-core）从 `.env` 的 `TELEGRAM_BOT_TOKEN` 读出来传进来；
    /// **渠道自己不读环境变量**（§11.2：凭证只在 `.env`，配置只在 config.toml）。
    pub fn new(token: String, config: ChannelConfig, http: reqwest::Client) -> Self {
        Self::with_api(Arc::new(BotApi::new(&token, http)), config)
    }

    pub fn with_api(api: Arc<BotApi>, config: ChannelConfig) -> Self {
        Self {
            sender: Arc::new(TelegramSender::with_api(api)),
            config,
            poll_timeout_secs: DEFAULT_POLL_TIMEOUT_SECS,
            idle_floor: DEFAULT_IDLE_FLOOR,
            retry_base: DEFAULT_RETRY_BASE,
            retry_cap: DEFAULT_RETRY_CAP,
        }
    }

    /// 出站的那一半。Notifier 用它主动投递（§11.4）；它与入站共用同一个
    /// [`BotApi`]，所以也共用那份"审批落在哪条消息上"的进程内小账。
    pub fn sender(&self) -> Arc<TelegramSender> {
        Arc::clone(&self.sender)
    }

    pub fn config(&self) -> &ChannelConfig {
        &self.config
    }

    #[cfg(test)]
    fn tuned(mut self, poll_timeout_secs: u64, idle_floor: Duration) -> Self {
        self.poll_timeout_secs = poll_timeout_secs;
        self.idle_floor = idle_floor;
        self.retry_base = Duration::from_millis(20);
        self.retry_cap = Duration::from_millis(80);
        self
    }

    fn api(&self) -> &BotApi {
        self.sender.api()
    }

    /// `getMe`，失败就退避重试。**认证失败不重试**：token 错了不会自己变对。
    async fn resolve_identity(&self, shutdown: &Shutdown) -> Result<BotIdentity, ChannelError> {
        let mut backoff = self.retry_base;
        loop {
            if shutdown.is_cancelled() {
                return Err(channel_error("停机时还没拿到 getMe"));
            }
            match self.api().get_me().await {
                Ok(identity) => return Ok(identity),
                Err(error) if error.is_retryable() => {
                    tracing::warn!(%error, "telegram：getMe 失败，稍后重试");
                    if sleep_or_cancel(backoff, shutdown).await {
                        return Err(channel_error("停机时还没拿到 getMe"));
                    }
                    backoff = (backoff * 2).min(self.retry_cap);
                }
                Err(error) => return Err(channel_error(error.to_string())),
            }
        }
    }

    async fn handle_update(&self, inbound: &dyn Inbound, identity: &BotIdentity, update: Update) {
        let request_key = inbound::request_key(update.update_id);
        let message = if let Some(callback) = &update.callback_query {
            // 先答一声：不调用 `answerCallbackQuery` 客户端会一直转圈
            // （spike callbacks.md 4e）。失败非致命——转圈比丢一次决定轻。
            if let Err(error) = self.api().answer_callback_query(&callback.id).await {
                tracing::warn!(%error, "telegram：answerCallbackQuery 失败，继续处理回调");
            }
            inbound::from_callback(callback, request_key)
        } else if let Some(message) = &update.message {
            inbound::from_message(message, identity, request_key)
        } else {
            None
        };

        let Some(message) = message else {
            // 群里没 @机器人、非文本消息、认不出的回调负载：`Ignored`，不进 Dispatcher。
            tracing::debug!(update_id = update.update_id, "telegram：忽略");
            return;
        };

        let peer = message.peer.clone();
        match inbound.handle(message).await {
            Ok(InboundAck::Replied { text }) => self.reply(&peer, &text).await,
            Ok(InboundAck::Rejected { hint }) => self.reply(&peer, &hint).await,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(update_id = update.update_id, %error, "telegram：Dispatcher 出错");
                // TODO(decide: 出错时 offset 照样推进——否则一个确定性的错误会让同一条
                // 消息无限重投。代价是这条消息只剩这句回执。)
                self.reply(&peer, INTERNAL_ERROR_HINT).await;
            }
        }
    }

    async fn reply(&self, peer: &komo_kernel::types::chat::ChannelPeer, text: &str) {
        if let Err(error) = self.sender.send_text(&peer.chat_id, text).await {
            tracing::warn!(%peer, %error, "telegram：回执没送出去");
        }
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn serve(
        &self,
        inbound: Arc<dyn Inbound>,
        shutdown: Shutdown,
    ) -> Result<(), ChannelError> {
        let identity = self.resolve_identity(&shutdown).await?;
        tracing::info!(bot = %identity.first_name, "telegram：开始长轮询");

        let mut offset: Option<i64> = None;
        let mut backoff = self.retry_base;

        while !shutdown.is_cancelled() {
            let started = Instant::now();
            let polled = race_cancel(
                self.api().get_updates(offset, self.poll_timeout_secs),
                &shutdown,
            )
            .await;
            // 停机时结束当前轮询直接返回：offset 没推进，这一批下次还在。
            let Some(polled) = polled else { break };

            let updates = match polled {
                Ok(updates) => {
                    backoff = self.retry_base;
                    updates
                }
                Err(error) => {
                    tracing::warn!(%error, "telegram：getUpdates 失败，退避后重试");
                    if sleep_or_cancel(backoff, &shutdown).await {
                        break;
                    }
                    backoff = (backoff * 2).min(self.retry_cap);
                    continue;
                }
            };

            let mut handled = 0usize;
            for update in updates {
                // 对端不认 offset 时的兜底：已经处理过的不再处理一次。
                if offset.is_some_and(|next| update.update_id < next) {
                    tracing::debug!(update_id = update.update_id, "telegram：这条已经处理过了");
                    continue;
                }
                let update_id = update.update_id;
                self.handle_update(inbound.as_ref(), &identity, update)
                    .await;
                // **这一行必须在 handle 之后**：推进 offset 就是向 Telegram 签收。
                offset = Some(update_id + 1);
                handled += 1;
                if shutdown.is_cancelled() {
                    break;
                }
            }

            // 一条都没处理（空批，或者对端把处理过的又给了一遍）：踩一下地板，别把一个
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

        tracing::info!("telegram：长轮询结束");
        Ok(())
    }
}

fn channel_error(message: impl Into<String>) -> ChannelError {
    ChannelError {
        channel: "telegram".into(),
        message: message.into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    use komo_kernel::types::ids::{RunId, SessionId};

    use super::fake::{
        BOT_USERNAME, Behavior, FakeBotApi, RecordingInbound, callback_update, text_update,
    };

    fn channel(fake: &FakeBotApi) -> TelegramChannel {
        TelegramChannel::with_api(fake.api(), ChannelConfig::default())
            .tuned(0, Duration::from_millis(10))
    }

    /// 跑 `serve`，等到 `done` 为真（或超时），然后停机并等它返回。
    async fn serve_until<F>(
        channel: TelegramChannel,
        inbound: Arc<dyn Inbound>,
        done: F,
    ) -> Shutdown
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

    // ① 同一个 update_id 被重投两次 → Inbound 只收到一次。
    #[tokio::test]
    async fn a_redelivered_update_reaches_the_dispatcher_once() {
        // 这个假服务端无视 offset，永远把同一条 update 再给一次。
        let fake = FakeBotApi::start_with(
            Behavior::replaying(),
            vec![text_update(7, 11, "private", 777, "在吗")],
        )
        .await;
        let recorder = RecordingInbound::new(fake.log());

        serve_until(channel(&fake), recorder.clone(), || {
            fake.calls_to("getUpdates").len() >= 4
        })
        .await;

        let received = recorder.received();
        assert_eq!(received.len(), 1, "重投的那几次都被跳过了：{received:?}");
        assert_eq!(received[0].request_key.as_str(), "telegram:7");
    }

    // ② callback_query 转成 /approve <id>，request_key 同源，answerCallbackQuery 被调。
    #[tokio::test]
    async fn a_button_press_becomes_the_same_command_a_message_would() {
        let fake = FakeBotApi::start_with(
            Behavior::default(),
            vec![callback_update(31, 11, 777, "approve:7K2M")],
        )
        .await;
        let recorder = RecordingInbound::new(fake.log());

        serve_until(channel(&fake), recorder.clone(), || {
            !recorder.received().is_empty()
        })
        .await;

        let received = recorder.received();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].text, "/approve 7K2M");
        // 去重键与普通消息同源：`telegram:{update_id}`（§11.1）。
        assert_eq!(received[0].request_key.as_str(), "telegram:31");
        assert_eq!(received[0].peer.to_string(), "telegram:11");
        assert_eq!(received[0].sender.as_str(), "777");

        let answered = fake.calls_to("answerCallbackQuery");
        assert_eq!(answered.len(), 1, "不调用它客户端会一直转圈");
        assert_eq!(answered[0]["callback_query_id"], "cb-31");
    }

    // ③ 群里非 @机器人 的消息 Ignored，@机器人 的剥掉提及。
    #[tokio::test]
    async fn a_group_message_only_counts_when_it_names_the_bot() {
        let fake = FakeBotApi::start_with(
            Behavior::default(),
            vec![
                text_update(41, -100, "supergroup", 777, "大家早"),
                text_update(
                    42,
                    -100,
                    "supergroup",
                    777,
                    &format!("@{BOT_USERNAME} 看一下日志"),
                ),
            ],
        )
        .await;
        let recorder = RecordingInbound::new(fake.log());

        serve_until(channel(&fake), recorder.clone(), || {
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
        assert_eq!(received[0].request_key.as_str(), "telegram:42");
    }

    // ④ offset 在 handle 返回之后才推进。
    #[tokio::test]
    async fn the_offset_only_moves_after_the_dispatcher_answers() {
        let fake = FakeBotApi::start_with(
            Behavior::default(),
            vec![text_update(50, 11, "private", 777, "在吗")],
        )
        .await;
        let log = fake.log();
        // handle 慢下来，好让"第二次 getUpdates 抢跑"变得看得见。
        let recorder = RecordingInbound::slow(log.clone(), Duration::from_millis(200));

        serve_until(channel(&fake), recorder.clone(), || {
            fake.polled_offsets().contains(&Some(51))
        })
        .await;

        let entries = log.entries();
        let handled = log.position("handle:end").expect("handle 跑过了");
        let advanced = entries
            .iter()
            .position(|event| event == "getUpdates:51")
            .expect("offset 应当推进到 51");
        assert!(
            advanced > handled,
            "offset 在 handle 返回之前就推进了：{entries:?}"
        );
        // 在此之前的每一次轮询都还带着旧 offset。
        for event in &entries[..advanced] {
            assert_ne!(event, "getUpdates:51", "{entries:?}");
        }
    }

    // InboundAck::Replied / Rejected 直接回发送者。
    #[tokio::test]
    async fn a_reply_goes_straight_back_to_the_chat() {
        let fake = FakeBotApi::start_with(
            Behavior::default(),
            vec![text_update(60, 11, "private", 777, "/pending")],
        )
        .await;
        let recorder = RecordingInbound::with_ack(
            fake.log(),
            InboundAck::Replied {
                text: "没有待处理的审批".into(),
            },
        );

        serve_until(channel(&fake), recorder.clone(), || {
            !fake.calls_to("sendMessage").is_empty()
        })
        .await;

        let sends = fake.calls_to("sendMessage");
        assert_eq!(sends[0]["chat_id"], "11");
        assert_eq!(sends[0]["text"], "没有待处理的审批");
    }

    #[tokio::test]
    async fn a_rejection_hint_goes_back_too() {
        let fake = FakeBotApi::start_with(
            Behavior::default(),
            vec![text_update(61, 11, "private", 888, "你好")],
        )
        .await;
        let recorder = RecordingInbound::with_ack(
            fake.log(),
            InboundAck::Rejected {
                hint: "telegram:11 · 888 不在 allow_from 里".into(),
            },
        );

        serve_until(channel(&fake), recorder.clone(), || {
            !fake.calls_to("sendMessage").is_empty()
        })
        .await;

        assert!(
            fake.calls_to("sendMessage")[0]["text"]
                .as_str()
                .unwrap()
                .contains("allow_from")
        );
    }

    #[tokio::test]
    async fn a_queued_message_gets_no_immediate_reply() {
        let fake = FakeBotApi::start_with(
            Behavior::default(),
            vec![text_update(62, 11, "private", 777, "帮我查一下")],
        )
        .await;
        let recorder = RecordingInbound::with_ack(
            fake.log(),
            InboundAck::Queued {
                session: SessionId::from_raw("s-1"),
                run: RunId::from_raw("run-1"),
            },
        );

        serve_until(channel(&fake), recorder.clone(), || {
            !recorder.received().is_empty()
        })
        .await;

        assert!(
            fake.calls_to("sendMessage").is_empty(),
            "排队的消息由 Run 结束时的投递回答，不在这里"
        );
    }

    #[tokio::test]
    async fn the_poll_asks_for_both_kinds_of_update() {
        let fake = FakeBotApi::start(Behavior::default()).await;
        let recorder = RecordingInbound::new(fake.log());
        serve_until(channel(&fake), recorder, || {
            !fake.calls_to("getUpdates").is_empty()
        })
        .await;

        let polls = fake.calls_to("getUpdates");
        assert_eq!(
            polls[0]["allowed_updates"],
            serde_json::json!(["message", "callback_query"])
        );
        assert!(polls[0].get("offset").is_none(), "第一次不带 offset");
    }

    // ⑨ shutdown 之后 serve 返回。
    #[tokio::test]
    async fn serve_returns_once_it_is_told_to_stop() {
        let fake = FakeBotApi::start(Behavior::default()).await;
        let recorder = RecordingInbound::new(fake.log());
        // serve_until 自己就断言了"停机后 serve 在 5 秒内返回且不是错误"。
        serve_until(channel(&fake), recorder, || {
            !fake.calls_to("getUpdates").is_empty()
        })
        .await;
    }

    #[tokio::test]
    async fn a_bad_token_is_not_retried_forever() {
        // 401 是不可重试的：token 错了不会自己变对，所以 serve 立刻带错误返回。
        let fake = FakeBotApi::start(Behavior::refuse_get_me()).await;
        let recorder = RecordingInbound::new(fake.log());
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            channel(&fake).serve(recorder, Shutdown::new()),
        )
        .await
        .expect("不该无限重试")
        .expect_err("认证失败是个错误");
        assert_eq!(error.channel, "telegram");
        assert_eq!(fake.calls_to("getMe").len(), 1, "只问一次");
        assert!(
            fake.calls_to("getUpdates").is_empty(),
            "没认出自己就不开始轮询"
        );
    }

    #[tokio::test]
    async fn probe_reports_the_bot_behind_the_token() {
        let fake = FakeBotApi::start(Behavior::default()).await;
        let identity = fake.api().get_me().await.expect("getMe");
        assert!(identity.is_bot);
        assert_eq!(identity.username.as_deref(), Some(BOT_USERNAME));
    }

    /// 只在本机有真 token 时跑：`cargo test -p komo-gateway --lib -- --ignored probe_`。
    /// **只调 getMe**：不发消息、不消费真实更新，输出里不出现 token 或任何 id。
    #[tokio::test]
    #[ignore = "需要 ~/.komo/.env 里的真实 TELEGRAM_BOT_TOKEN"]
    async fn probe_reaches_the_real_bot_api() {
        let token = std::env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN");
        let identity = probe(&token, reqwest::Client::new())
            .await
            .expect("getMe 应当成功");
        println!(
            "probe ok: is_bot={} has_username={}",
            identity.is_bot,
            identity.username.is_some()
        );
        assert!(identity.is_bot);
    }
}
