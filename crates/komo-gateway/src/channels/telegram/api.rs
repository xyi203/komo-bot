//! Telegram Bot API 的线格式与 HTTP 调用（§13.2：reqwest 手写长轮询，无 SDK）。
//!
//! 只解析 komo 用得到的字段，其余一律忽略——Bot API 每月加字段，而一个多出来的键
//! 不应该让一整批 update 反序列化失败（§11.4 记的正是微信那边被这件事咬过一次）。
//!
//! **错误里永远没有 URL**：base URL 里带着 bot token，而 reqwest 的错误默认会把 URL
//! 打出来。所有传输错误都过一遍 `without_url()`。

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::render::telegram::{InlineKeyboard, RenderedMessage};

/// 官方端点。
const DEFAULT_ENDPOINT: &str = "https://api.telegram.org";

/// 长轮询请求在 `timeout` 之外还要留给传输的余量。
const POLL_SLACK: Duration = Duration::from_secs(15);

/// 一次普通（非长轮询）调用的超时。
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// Bot API 的失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TelegramError {
    /// 连不上 / 超时 / 读不完。可重试，**不带 URL**（URL 里有 token）。
    #[error("telegram 传输失败：{0}")]
    Transport(String),
    /// 服务端明确拒绝（`ok: false`）。
    #[error("telegram 拒绝（{code}）：{description}")]
    Api { code: i64, description: String },
    /// 响应不是我们认识的形状。
    #[error("telegram 响应无法解析：{0}")]
    Decode(String),
}

impl TelegramError {
    /// 值得再试一次吗。
    ///
    /// 只按 HTTP 语义分档，**不按 `description` 的文案分**：官方明说 `error_code` 的
    /// 内容将来会变（spike callbacks.md 5c），按文案匹配的代码会在某个早上悄悄失灵。
    pub fn is_retryable(&self) -> bool {
        match self {
            TelegramError::Transport(_) => true,
            TelegramError::Decode(_) => false,
            TelegramError::Api { code, .. } => *code == 429 || (500..600).contains(code),
        }
    }

    /// 服务端拒绝（而不是没连上）。MarkdownV2 退回纯文本只看这个（§13.2）。
    pub fn is_api_refusal(&self) -> bool {
        matches!(self, TelegramError::Api { .. })
    }
}

/// `getMe` 的结果。`komo channel probe` 要的就是它（§11.5）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BotIdentity {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub username: Option<String>,
}

/// 一个 `Update`。**投递与去重的单位就是它**：`offset` 未推进时同一个 `Update`
/// 原样再取一次，`callback_query` 是它的一个字段而不是更细的粒度（§11.1）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Chat {
    pub id: i64,
    /// `private` / `group` / `supergroup` / `channel`。
    #[serde(rename = "type", default)]
    pub kind: String,
}

impl Chat {
    /// 操作者的私聊全部落到同一个 home session——**哪个**会话是 Dispatcher 的事，
    /// 渠道只负责把这个布尔填对（§11.2）。
    pub fn is_private(&self) -> bool {
        self.kind == "private"
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    /// inline 模式的回调没有 message（也就没有 chat）。komo 不用 inline 模式。
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub data: Option<String>,
}

/// Bot API 的 HTTP 客户端。
pub struct BotApi {
    http: reqwest::Client,
    /// `{endpoint}/bot{token}`。**不进任何日志与错误**。
    base: String,
}

impl std::fmt::Debug for BotApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // base 里有 token。
        f.debug_struct("BotApi").finish_non_exhaustive()
    }
}

impl BotApi {
    pub fn new(token: &str, http: reqwest::Client) -> Self {
        Self::with_endpoint(DEFAULT_ENDPOINT, token, http)
    }

    /// 换一个端点（测试里的假 Bot API，或一个自建代理）。
    pub fn with_endpoint(endpoint: &str, token: &str, http: reqwest::Client) -> Self {
        Self {
            http,
            base: format!("{}/bot{}", endpoint.trim_end_matches('/'), token),
        }
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        body: Value,
        timeout: Duration,
    ) -> Result<T, TelegramError> {
        let response = self
            .http
            .post(format!("{}/{}", self.base, method))
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(transport)?;
        let bytes = response.bytes().await.map_err(transport)?;
        let envelope: Envelope = serde_json::from_slice(&bytes)
            .map_err(|err| TelegramError::Decode(format!("{method}: {err}")))?;
        if !envelope.ok {
            return Err(TelegramError::Api {
                code: envelope.error_code.unwrap_or(0),
                description: envelope
                    .description
                    .unwrap_or_else(|| "（服务端没有说原因）".into()),
            });
        }
        let result = envelope
            .result
            .ok_or_else(|| TelegramError::Decode(format!("{method}: ok 但没有 result")))?;
        serde_json::from_value(result)
            .map_err(|err| TelegramError::Decode(format!("{method}: {err}")))
    }

    /// `getMe`。`komo channel probe` 的连通性核对。
    pub async fn get_me(&self) -> Result<BotIdentity, TelegramError> {
        self.call("getMe", json!({}), CALL_TIMEOUT).await
    }

    /// 长轮询。`offset` 是**上一条处理完的** `update_id + 1`（§11.1）。
    ///
    /// 认不出的 update 降级成一个只剩 `update_id` 的壳而不是整批失败——否则队列会卡
    /// 在它上面，offset 永远推不过去。
    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u64,
    ) -> Result<Vec<Update>, TelegramError> {
        let mut body = json!({
            "timeout": timeout_secs,
            "allowed_updates": ["message", "callback_query"],
        });
        if let Some(offset) = offset {
            body["offset"] = json!(offset);
        }
        let raw: Vec<Value> = self
            .call(
                "getUpdates",
                body,
                Duration::from_secs(timeout_secs) + POLL_SLACK,
            )
            .await?;
        Ok(raw
            .into_iter()
            .filter_map(
                |value| match serde_json::from_value::<Update>(value.clone()) {
                    Ok(update) => Some(update),
                    Err(error) => {
                        let update_id = value.get("update_id").and_then(Value::as_i64)?;
                        tracing::warn!(update_id, %error, "telegram：跳过认不出的 update");
                        Some(Update {
                            update_id,
                            message: None,
                            callback_query: None,
                        })
                    }
                },
            )
            .collect())
    }

    /// `sendMessage`，返回新消息的 `message_id`——决定之后要按它原地改（§11.3）。
    pub async fn send_message(
        &self,
        chat_id: &str,
        message: &RenderedMessage,
    ) -> Result<i64, TelegramError> {
        let mut body = json!({ "chat_id": chat_id, "text": message.text });
        if let Some(parse_mode) = message.parse_mode {
            body["parse_mode"] = json!(parse_mode.as_str());
        }
        if let Some(keyboard) = &message.keyboard {
            body["reply_markup"] = serde_json::to_value(keyboard)
                .map_err(|err| TelegramError::Decode(err.to_string()))?;
        }
        let sent: Message = self.call("sendMessage", body, CALL_TIMEOUT).await?;
        Ok(sent.message_id)
    }

    /// `editMessageText`。
    pub async fn edit_message_text(
        &self,
        chat_id: &str,
        message_id: i64,
        message: &RenderedMessage,
    ) -> Result<(), TelegramError> {
        let mut body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": message.text,
        });
        if let Some(parse_mode) = message.parse_mode {
            body["parse_mode"] = json!(parse_mode.as_str());
        }
        self.call::<Value>("editMessageText", body, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// `editMessageReplyMarkup`。传空键盘 = 去掉按钮（§11.3：决定过的请求不该还长着
    /// 可点的按钮）。
    pub async fn edit_message_reply_markup(
        &self,
        chat_id: &str,
        message_id: i64,
        keyboard: &InlineKeyboard,
    ) -> Result<(), TelegramError> {
        let body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reply_markup": serde_json::to_value(keyboard)
                .map_err(|err| TelegramError::Decode(err.to_string()))?,
        });
        self.call::<Value>("editMessageReplyMarkup", body, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// `answerCallbackQuery`，无参。不调用客户端会一直转圈（spike callbacks.md 4e）。
    pub async fn answer_callback_query(&self, query_id: &str) -> Result<(), TelegramError> {
        self.call::<Value>(
            "answerCallbackQuery",
            json!({ "callback_query_id": query_id }),
            CALL_TIMEOUT,
        )
        .await
        .map(|_| ())
    }
}

/// reqwest 的错误里带 URL，而 URL 里带 token。
fn transport(error: reqwest::Error) -> TelegramError {
    TelegramError::Transport(error.without_url().to_string())
}

#[derive(Debug, Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_with_unknown_fields_still_parses() {
        let update: Update = serde_json::from_value(json!({
            "update_id": 42,
            "message": {
                "message_id": 7,
                "date": 1,
                "chat": { "id": -100, "type": "supergroup", "title": "g" },
                "from": { "id": 5, "is_bot": false, "first_name": "op" },
                "text": "hi",
                "brand_new_field": { "x": 1 },
            },
        }))
        .unwrap();
        assert_eq!(update.update_id, 42);
        let message = update.message.unwrap();
        assert!(!message.chat.is_private());
        assert_eq!(message.text.as_deref(), Some("hi"));
        assert_eq!(message.from.unwrap().id, 5);
    }

    #[test]
    fn a_callback_query_without_a_message_still_parses() {
        // inline 模式：`message` 是 Optional（spike callbacks.md 4d）。
        let update: Update = serde_json::from_value(json!({
            "update_id": 9,
            "callback_query": {
                "id": "cb-1",
                "from": { "id": 5, "is_bot": false, "first_name": "op" },
                "data": "approve:7K2M",
                "inline_message_id": "inline-1",
            },
        }))
        .unwrap();
        let callback = update.callback_query.unwrap();
        assert!(callback.message.is_none());
        assert_eq!(callback.data.as_deref(), Some("approve:7K2M"));
    }

    #[test]
    fn a_stale_callback_message_without_from_or_text_still_parses() {
        // 太老的消息在回调里退化成 MaybeInaccessibleMessage：只剩 chat 与 message_id。
        let message: Message = serde_json::from_value(json!({
            "message_id": 3,
            "date": 0,
            "chat": { "id": 11, "type": "private" },
        }))
        .unwrap();
        assert!(message.from.is_none());
        assert!(message.chat.is_private());
    }

    #[test]
    fn retryability_reads_the_code_not_the_description() {
        assert!(TelegramError::Transport("超时".into()).is_retryable());
        assert!(
            TelegramError::Api {
                code: 429,
                description: "Too Many Requests".into()
            }
            .is_retryable()
        );
        assert!(
            TelegramError::Api {
                code: 502,
                description: "Bad Gateway".into()
            }
            .is_retryable()
        );
        assert!(
            !TelegramError::Api {
                code: 400,
                description: "can't parse entities".into()
            }
            .is_retryable()
        );
        assert!(
            !TelegramError::Api {
                code: 401,
                description: "Unauthorized".into()
            }
            .is_retryable()
        );
    }
}
