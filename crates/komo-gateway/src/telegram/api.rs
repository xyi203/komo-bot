//! Telegram Bot API 的线格式与 HTTP 调用：长轮询 `getUpdates` 与 `sendMessage`，reqwest 手写、没有 SDK。
//!
//! 从 2026-11-13 删掉的 `komo-channel` 里拿回来的那一小块（`docs/telegram.md` §3），图片、文件、按钮都没带：
//! 备用入口只收发文字。
//!
//! 只解析用得到的字段，其余一律忽略——Bot API 常加字段，多一个键不该让整批 update 反序列化失败。
//! **错误里永远没有 URL**：base URL 里带着 bot token，reqwest 的错误默认会把 URL 打出来，所有传输错误都过一遍
//! `without_url()`。

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// 官方端点。
pub const DEFAULT_ENDPOINT: &str = "https://api.telegram.org";

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
    /// 响应不是认识的形状。
    #[error("telegram 响应无法解析：{0}")]
    Decode(String),
}

impl TelegramError {
    /// token 不对（401）或 bot 被删（404）：再轮询也没用，停下并说清楚。按 HTTP 语义分，不按文案分——
    /// 官方说 `description` 的内容会变。
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            TelegramError::Api {
                code: 401 | 404,
                ..
            }
        )
    }
}

/// 一个 `Update`：去重的单位。`offset` 没推进时同一个 update 会原样再来一次。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
    /// 带图 / 文件的消息没有 `text`，写的字在这里。
    #[serde(default)]
    pub caption: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Chat {
    pub id: i64,
    /// `private` / `group` / `supergroup` / `channel`。
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct User {
    pub id: i64,
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
    pub fn new(endpoint: &str, token: &str, http: reqwest::Client) -> Self {
        BotApi {
            http,
            base: format!("{}/bot{token}", endpoint.trim_end_matches('/')),
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

    /// 长轮询。`offset` 是**上一条处理完的** `update_id + 1`。
    ///
    /// 认不出的 update 降级成只剩 `update_id` 的壳，不让整批失败——否则队列卡在它上面，offset 永远推不过去。
    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u64,
    ) -> Result<Vec<Update>, TelegramError> {
        let mut body = json!({ "timeout": timeout_secs, "allowed_updates": ["message"] });
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
                        })
                    }
                },
            )
            .collect())
    }

    /// `sendMessage`，纯文本（不用 MarkdownV2：模型的回复里到处是要转义的字符，转错一个整条发不出去）。
    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), TelegramError> {
        let _: Value = self
            .call(
                "sendMessage",
                json!({ "chat_id": chat_id, "text": text }),
                CALL_TIMEOUT,
            )
            .await?;
        Ok(())
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
                "chat": { "id": 5, "type": "private", "first_name": "op" },
                "from": { "id": 5, "is_bot": false, "first_name": "op" },
                "text": "hi",
                "brand_new_field": { "x": 1 },
            },
        }))
        .unwrap();
        assert_eq!(update.update_id, 42);
        let message = update.message.unwrap();
        assert_eq!(message.chat.kind, "private");
        assert_eq!(message.text.as_deref(), Some("hi"));
    }

    #[test]
    fn only_a_bad_token_or_a_deleted_bot_stops_the_poller() {
        let api = |code| TelegramError::Api {
            code,
            description: String::new(),
        };
        assert!(api(401).is_fatal());
        assert!(api(404).is_fatal());
        assert!(!api(429).is_fatal());
        assert!(!api(502).is_fatal());
        assert!(!TelegramError::Transport("x".into()).is_fatal());
    }
}
