//! 飞书开放平台 REST 接口（§13.2：回复走 reqwest，不走 SDK）。
//!
//! openlark 只用来跑 ws 长连接；所有发出去的东西都在这里手写：
//!
//! | 接口 | 用处 |
//! |---|---|
//! | `POST /open-apis/auth/v3/tenant_access_token/internal` | 拿 tenant token（`komo channel probe` 也是它） |
//! | `GET /open-apis/bot/v3/info` | 机器人自己的 `open_id`——群里认 @提及 要用 |
//! | `POST /open-apis/im/v1/messages` | 发文本 / 发卡片 |
//! | `PATCH /open-apis/im/v1/messages/{id}` | 决定之后原地换掉审批卡、Run 终态时换掉处理中卡片（§11.3） |
//! | `POST /open-apis/im/v1/messages/{id}/reply` | 回复原消息发一张处理中卡片 |
//! | `POST /open-apis/im/v1/messages/{id}/reactions` | 给原消息加表情回复（收到了） |
//! | `GET /open-apis/im/v1/chats/{id}` | 这个会话是不是私聊——卡片回调载荷里没有这一项 |
//!
//! 只解析用得到的字段，其余一律忽略：开放平台每月加字段，而一个多出来的键不应该让一
//! 条事件或一次回复整体失败。
//!
//! **token 不进日志也不进错误**：`Debug` 是 `finish_non_exhaustive`，传输错误过一遍
//! `without_url()`。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// 官方端点。
const DEFAULT_ENDPOINT: &str = "https://open.feishu.cn";

/// 一次调用的超时。
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// 提前多久换一张新 token。
///
/// 服务端给的是有效期，不是"还能用多久"：一张只剩几秒的 token 会在网络慢一点的那次
/// 调用上过期，而那一次正是要把审批送出去的那一次。
const TOKEN_SKEW: Duration = Duration::from_secs(300);

/// token 至少缓存这么久。服务端给出一个荒唐的小有效期时不至于变成每次调用取一次。
const TOKEN_MIN_TTL: Duration = Duration::from_secs(30);

/// 开放平台的失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeishuError {
    /// 连不上 / 超时 / 读不完。可重试。
    #[error("飞书传输失败：{0}")]
    Transport(String),
    /// 服务端明确拒绝（`code != 0`）。
    #[error("飞书拒绝（http {status} / code {code}）：{msg}")]
    Api { status: u16, code: i64, msg: String },
    /// 响应不是我们认识的形状。
    #[error("飞书响应无法解析：{0}")]
    Decode(String),
}

impl FeishuError {
    /// 值得再试一次吗。
    ///
    /// 只按 **HTTP 状态**分档，不按 `msg` 的文案分：飞书的业务码表比 HTTP 状态长得多
    /// 也变得多，而按文案匹配的代码会在某个早上悄悄失灵。代价是一个 200 + 业务码的临时
    /// 故障不会被重试——那条路上失败的是一次投递，不是一次决定。
    pub fn is_retryable(&self) -> bool {
        match self {
            FeishuError::Transport(_) => true,
            FeishuError::Decode(_) => false,
            FeishuError::Api { status, .. } => *status == 429 || (500..600).contains(status),
        }
    }

    /// 服务端拒绝（而不是没连上）。
    pub fn is_api_refusal(&self) -> bool {
        matches!(self, FeishuError::Api { .. })
    }
}

/// `bot/v3/info` 里我们要的那两项。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BotIdentity {
    /// 机器人自己的 `open_id`。群里认 @提及 靠它。
    #[serde(default)]
    pub open_id: String,
    #[serde(default)]
    pub app_name: String,
}

/// `komo channel probe` 的连通性核对结果。**不含 token 本身。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenProbe {
    /// 服务端说这张 token 还能用多少秒。
    pub expires_in: i64,
}

/// 缓存下来的 tenant token。
#[derive(Debug, Clone)]
struct CachedToken {
    value: String,
    /// 到这一刻之前都可以直接用（已经减掉了 [`TOKEN_SKEW`]）。
    good_until: Instant,
}

/// 开放平台的 HTTP 客户端。
pub struct FeishuApi {
    http: reqwest::Client,
    base: String,
    app_id: String,
    /// **不进任何日志与错误。**
    app_secret: String,
    token: Mutex<Option<CachedToken>>,
}

impl std::fmt::Debug for FeishuApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeishuApi").finish_non_exhaustive()
    }
}

impl FeishuApi {
    pub fn new(
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        http: reqwest::Client,
    ) -> Self {
        Self::with_endpoint(DEFAULT_ENDPOINT, app_id, app_secret, http)
    }

    /// 换一个端点（测试里的假开放平台，或一个自建代理）。
    pub fn with_endpoint(
        endpoint: &str,
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            http,
            base: endpoint.trim_end_matches('/').to_string(),
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            token: Mutex::new(None),
        }
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// 取一张可用的 tenant token，命中缓存就不联网。
    ///
    /// 两个请求同时错过缓存时会各取一张——第二张覆盖第一张，两张都有效，代价是一次多余
    /// 的调用。换成"取 token 时持锁"就得把锁跨过一次网络往返，那会让所有发送排在一次
    /// 取 token 后面。
    pub async fn tenant_token(&self) -> Result<String, FeishuError> {
        if let Some(cached) = self.cached_token() {
            return Ok(cached);
        }
        Ok(self.fetch_token().await?.0)
    }

    /// 无条件取一张新 token，只回报它还能用多久。`komo channel probe` 走的就是它
    /// （§11.5）——**不回报 token 本身**。
    pub async fn probe_token(&self) -> Result<TokenProbe, FeishuError> {
        Ok(self.fetch_token().await?.1)
    }

    async fn fetch_token(&self) -> Result<(String, TokenProbe), FeishuError> {
        #[derive(Deserialize)]
        struct TokenBody {
            #[serde(default)]
            tenant_access_token: String,
            #[serde(default)]
            expire: i64,
        }

        // 这个接口的 token 与有效期在**信封根部**，不在 `data` 里。
        let body: TokenBody = self
            .call_flat(
                reqwest::Method::POST,
                "/open-apis/auth/v3/tenant_access_token/internal",
                None,
                Some(json!({ "app_id": self.app_id, "app_secret": self.app_secret })),
            )
            .await?;
        if body.tenant_access_token.is_empty() {
            return Err(FeishuError::Decode("tenant_access_token 为空".into()));
        }

        let ttl = Duration::from_secs(body.expire.max(0) as u64);
        let good_for = ttl.saturating_sub(TOKEN_SKEW).max(TOKEN_MIN_TTL);
        *self.token.lock().expect("token 缓存") = Some(CachedToken {
            value: body.tenant_access_token.clone(),
            good_until: Instant::now() + good_for,
        });
        Ok((
            body.tenant_access_token,
            TokenProbe {
                expires_in: body.expire,
            },
        ))
    }

    /// 机器人自己是谁。群里剥 @提及 要它的 `open_id`。
    pub async fn bot_info(&self) -> Result<BotIdentity, FeishuError> {
        #[derive(Deserialize)]
        struct BotBody {
            #[serde(default)]
            bot: Option<BotIdentity>,
        }
        let token = self.tenant_token().await?;
        // `bot` 也在信封根部。
        let body: BotBody = self
            .call_flat(
                reqwest::Method::GET,
                "/open-apis/bot/v3/info",
                Some(&token),
                None,
            )
            .await?;
        body.bot
            .ok_or_else(|| FeishuError::Decode("bot/v3/info 没有 bot".into()))
    }

    /// 发一条消息，返回它的 `message_id`——决定之后要按它原地改（§11.3）。
    pub async fn send_message(
        &self,
        chat_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<String, FeishuError> {
        #[derive(Deserialize)]
        struct Sent {
            #[serde(default)]
            message_id: String,
        }
        let token = self.tenant_token().await?;
        let sent: Sent = self
            .call_data(
                reqwest::Method::POST,
                "/open-apis/im/v1/messages?receive_id_type=chat_id",
                Some(&token),
                Some(json!({
                    "receive_id": chat_id,
                    "msg_type": msg_type,
                    "content": content,
                })),
            )
            .await?;
        if sent.message_id.is_empty() {
            return Err(FeishuError::Decode("发送成功但没有 message_id".into()));
        }
        Ok(sent.message_id)
    }

    /// 回复一条消息，返回回复那条的 `message_id`——Run 终态时要按它原地改。
    pub async fn reply_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<String, FeishuError> {
        #[derive(Deserialize)]
        struct Sent {
            #[serde(default)]
            message_id: String,
        }
        let token = self.tenant_token().await?;
        let sent: Sent = self
            .call_data(
                reqwest::Method::POST,
                &format!("/open-apis/im/v1/messages/{message_id}/reply"),
                Some(&token),
                Some(json!({ "msg_type": msg_type, "content": content })),
            )
            .await?;
        if sent.message_id.is_empty() {
            return Err(FeishuError::Decode("回复成功但没有 message_id".into()));
        }
        Ok(sent.message_id)
    }

    /// 给一条消息加一个表情回复。`emoji_type` 是飞书表情列表里的字面量（如 `Get`）。
    pub async fn add_reaction(
        &self,
        message_id: &str,
        emoji_type: &str,
    ) -> Result<(), FeishuError> {
        let token = self.tenant_token().await?;
        self.call_data::<Value>(
            reqwest::Method::POST,
            &format!("/open-apis/im/v1/messages/{message_id}/reactions"),
            Some(&token),
            Some(json!({ "reaction_type": { "emoji_type": emoji_type } })),
        )
        .await
        .map(|_| ())
    }

    /// 原地更新一张卡片（§11.3）。
    ///
    /// 只对 `interactive` 消息有效，只覆盖 14 天内发出的消息，单条 5 QPS，且更新前后的
    /// `config.update_multi` 都必须为 `true`（spike callbacks.md 3d——后一条由
    /// [`crate::render::feishu`] 保证）。
    pub async fn patch_message(&self, message_id: &str, content: &str) -> Result<(), FeishuError> {
        let token = self.tenant_token().await?;
        self.call::<Value>(
            reqwest::Method::PATCH,
            &format!("/open-apis/im/v1/messages/{message_id}"),
            Some(&token),
            Some(json!({ "content": content })),
            Extract::Whole,
        )
        .await
        .map(|_| ())
    }

    /// 这个会话是不是单聊。
    ///
    /// 卡片回调的载荷里只有 `open_chat_id`，没有会话类型（spike callbacks.md 2a），而
    /// Dispatcher 要靠 `is_private` 决定这条命令落到 home session 还是群会话
    /// （§11.2）。所以只能问一次平台。
    pub async fn chat_is_private(&self, chat_id: &str) -> Result<bool, FeishuError> {
        #[derive(Deserialize)]
        struct Chat {
            #[serde(default)]
            chat_mode: String,
        }
        let token = self.tenant_token().await?;
        let chat: Chat = self
            .call_data(
                reqwest::Method::GET,
                &format!("/open-apis/im/v1/chats/{chat_id}"),
                Some(&token),
                None,
            )
            .await?;
        Ok(chat.chat_mode == "p2p")
    }

    #[cfg(test)]
    fn cached_token_deadline(&self) -> Option<Instant> {
        self.token
            .lock()
            .expect("token 缓存")
            .as_ref()
            .map(|cached| cached.good_until)
    }

    /// 测试用：把缓存的到期时刻往前搬，模拟"快过期了"。
    #[cfg(test)]
    fn expire_token_in(&self, remaining: Duration) {
        if let Some(cached) = self.token.lock().expect("token 缓存").as_mut() {
            cached.good_until = Instant::now() + remaining;
        }
    }

    fn cached_token(&self) -> Option<String> {
        self.token
            .lock()
            .expect("token 缓存")
            .as_ref()
            .filter(|cached| Instant::now() < cached.good_until)
            .map(|cached| cached.value.clone())
    }

    async fn call_data<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Result<T, FeishuError> {
        self.call(method, path, token, body, Extract::Data).await
    }

    async fn call_flat<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Result<T, FeishuError> {
        self.call(method, path, token, body, Extract::Whole).await
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        extract: Extract,
    ) -> Result<T, FeishuError> {
        let mut request = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .timeout(CALL_TIMEOUT);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(transport)?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(transport)?;

        let envelope: Value = serde_json::from_slice(&bytes)
            .map_err(|err| FeishuError::Decode(format!("{path}: {err}")))?;
        let code = envelope.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 || !(200..300).contains(&status) {
            return Err(FeishuError::Api {
                status,
                code,
                msg: envelope
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("（服务端没有说原因）")
                    .to_string(),
            });
        }

        let payload = match extract {
            Extract::Whole => envelope,
            // `data` 缺席按空对象读：PATCH 之类的接口成功时不带它，而全默认的结构体
            // 本来就能从 `{}` 读出来。
            Extract::Data => envelope
                .get("data")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        };
        serde_json::from_value(payload).map_err(|err| FeishuError::Decode(format!("{path}: {err}")))
    }
}

/// 从信封里取哪一半。
#[derive(Debug, Clone, Copy)]
enum Extract {
    /// 整个信封——`tenant_access_token` / `bot` 这类字段在根部。
    Whole,
    /// `data` 字段。
    Data,
}

/// reqwest 的错误里带 URL。飞书的 URL 里没有凭证，但错误会进日志，少一样是一样。
fn transport(error: reqwest::Error) -> FeishuError {
    FeishuError::Transport(error.without_url().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::fake::{Behavior, FakeOpenApi};

    #[tokio::test]
    async fn a_token_is_fetched_once_and_then_cached() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let api = fake.api();

        let first = api.tenant_token().await.expect("第一次取 token");
        let second = api.tenant_token().await.expect("第二次命中缓存");
        assert_eq!(first, second);
        assert_eq!(
            fake.calls_to("tenant_access_token").len(),
            1,
            "第二次不该联网"
        );
    }

    // ⑦ tenant token 缓存与提前刷新。
    #[tokio::test]
    async fn a_token_close_to_expiry_is_replaced_before_it_dies() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let api = fake.api();
        api.tenant_token().await.unwrap();

        // 假服务端给的是 7200 秒，减掉 5 分钟的提前量。
        let deadline = api.cached_token_deadline().expect("缓存里有一张");
        let good_for = deadline.saturating_duration_since(Instant::now());
        assert!(
            good_for <= Duration::from_secs(7200) - TOKEN_SKEW,
            "提前量没扣：还能用 {good_for:?}"
        );
        assert!(
            good_for > Duration::from_secs(6800),
            "扣得太多：{good_for:?}"
        );

        // 走到提前量之内：下一次调用换一张新的，而不是把旧的一直用到真过期。
        api.expire_token_in(Duration::ZERO);
        api.tenant_token().await.unwrap();
        assert_eq!(fake.calls_to("tenant_access_token").len(), 2);
    }

    #[tokio::test]
    async fn a_useless_expiry_still_gets_cached_for_a_while() {
        let fake = FakeOpenApi::start(Behavior::short_lived_token()).await;
        let api = fake.api();
        api.tenant_token().await.unwrap();
        api.tenant_token().await.unwrap();
        assert_eq!(
            fake.calls_to("tenant_access_token").len(),
            1,
            "expire=1 也不该变成每次调用取一次"
        );
    }

    #[tokio::test]
    async fn a_refused_token_is_an_api_error_and_names_the_code() {
        let fake = FakeOpenApi::start(Behavior::refuse_token()).await;
        let error = fake.api().tenant_token().await.expect_err("凭证不对");
        assert!(error.is_api_refusal());
        assert!(!error.is_retryable(), "app secret 不会自己变对");
        assert!(error.to_string().contains("99991663"), "{error}");
    }

    #[tokio::test]
    async fn a_secret_never_shows_up_in_the_error_or_the_debug() {
        let fake = FakeOpenApi::start(Behavior::refuse_token()).await;
        let api = fake.api();
        let error = api.tenant_token().await.expect_err("凭证不对");
        assert!(!error.to_string().contains("test-app-secret"), "{error}");
        assert!(!format!("{api:?}").contains("test-app-secret"), "{api:?}");
    }

    #[tokio::test]
    async fn the_bot_reports_its_own_open_id() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let bot = fake.api().bot_info().await.expect("bot/v3/info");
        assert_eq!(bot.open_id, super::super::fake::BOT_OPEN_ID);
    }

    #[tokio::test]
    async fn sending_a_message_returns_its_id() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let id = fake
            .api()
            .send_message("oc_1", "text", r#"{"text":"在"}"#)
            .await
            .expect("发消息");
        assert!(id.starts_with("om_"), "{id}");

        let sends = fake.calls_to("messages");
        assert_eq!(sends[0]["receive_id"], "oc_1");
        assert_eq!(sends[0]["msg_type"], "text");
        assert_eq!(sends[0]["content"], r#"{"text":"在"}"#);
    }

    #[tokio::test]
    async fn patching_a_card_sends_the_content_as_a_string() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        fake.api()
            .patch_message("om_1", r#"{"schema":"2.0"}"#)
            .await
            .expect("PATCH");
        let patches = fake.calls_to("patch");
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0]["content"], r#"{"schema":"2.0"}"#);
    }

    #[tokio::test]
    async fn replying_to_a_message_targets_it_and_returns_the_new_id() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let id = fake
            .api()
            .reply_message("om_origin", "interactive", r#"{"schema":"2.0"}"#)
            .await
            .expect("回复");
        assert!(id.starts_with("om_"), "{id}");
        let replies = fake.calls_to("reply");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["target"], "om_origin");
        assert_eq!(replies[0]["msg_type"], "interactive");
        assert_eq!(replies[0]["content"], r#"{"schema":"2.0"}"#);
    }

    #[tokio::test]
    async fn a_reaction_names_the_message_and_the_emoji() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        fake.api()
            .add_reaction("om_origin", "Get")
            .await
            .expect("加表情");
        let reactions = fake.calls_to("reactions");
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0]["target"], "om_origin");
        assert_eq!(reactions[0]["reaction_type"]["emoji_type"], "Get");
    }

    #[tokio::test]
    async fn a_chat_reports_whether_it_is_a_dm() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        assert!(fake.api().chat_is_private("oc_dm").await.unwrap());
        assert!(!fake.api().chat_is_private("oc_group").await.unwrap());
    }

    #[test]
    fn retryability_reads_the_status_not_the_message() {
        assert!(FeishuError::Transport("超时".into()).is_retryable());
        assert!(
            FeishuError::Api {
                status: 429,
                code: 99991400,
                msg: "too many request".into()
            }
            .is_retryable()
        );
        assert!(
            FeishuError::Api {
                status: 503,
                code: 0,
                msg: "unavailable".into()
            }
            .is_retryable()
        );
        assert!(
            !FeishuError::Api {
                status: 200,
                code: 230001,
                msg: "bot is not in the chat".into()
            }
            .is_retryable()
        );
        assert!(!FeishuError::Decode("少了 data".into()).is_retryable());
    }
}
