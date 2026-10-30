//! Telegram 的出站一侧：`Outbound` → 已发出的消息（§11.3、§11.4）。
//!
//! Telegram **从不 `Deferred`**：Bot API 能对任何已加入的会话主动推送，没有微信那种
//! 回复令牌（§11.4）。所以这里只有 `Sent` 与失败两种结果。
//!
//! 审批请求的按钮与"决定之后原地改消息"随审批一起删掉了（§7，2026-10-11）：出站只剩
//! 文本，所以这里没有进程内的消息账，也没有 `editMessage*`。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::DeliverError;
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, Outbound, PeerId};

use super::api::{BotApi, TelegramError};
use crate::telegram::render::{self as telegram, RenderedMessage};
use crate::{ChannelSender, SendOutcome};

pub struct TelegramSender {
    api: Arc<BotApi>,
}

impl std::fmt::Debug for TelegramSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramSender").finish_non_exhaustive()
    }
}

impl TelegramSender {
    pub fn new(token: &str, http: reqwest::Client) -> Self {
        Self::with_api(Arc::new(BotApi::new(token, http)))
    }

    pub fn with_api(api: Arc<BotApi>) -> Self {
        Self { api }
    }

    pub fn api(&self) -> &Arc<BotApi> {
        &self.api
    }

    /// 一个 `Outbound` 送到一个会话。
    pub async fn deliver(
        &self,
        peer: &ChannelPeer,
        message: Outbound,
    ) -> Result<SendOutcome, TelegramError> {
        // 并入别的 Run 的那条没有回复卡片可收尾：视为已处理，不发消息。
        if matches!(message, Outbound::RunAbsorbed { .. }) {
            return Ok(SendOutcome::Sent);
        }
        self.send_rendered_all(peer.chat_id.as_str(), &telegram::render(&message))
            .await?;
        Ok(SendOutcome::Sent)
    }

    /// 一段纯文本（命令回执、被拒绝的提示、Run 的最终回复）。返回每一段的
    /// `message_id`。
    pub async fn send_text(&self, chat_id: &PeerId, text: &str) -> Result<Vec<i64>, TelegramError> {
        let rendered = telegram::render(&Outbound::Text {
            text: text.to_string(),
        });
        self.send_rendered_all(chat_id.as_str(), &rendered).await
    }

    async fn send_rendered_all(
        &self,
        chat_id: &str,
        messages: &[RenderedMessage],
    ) -> Result<Vec<i64>, TelegramError> {
        let mut ids = Vec::with_capacity(messages.len());
        for message in messages {
            ids.push(self.send_rendered(chat_id, message).await?);
        }
        Ok(ids)
    }

    /// 发一条，MarkdownV2 被拒绝就原样重发一次纯文本（§13.2）。
    ///
    /// 触发条件是"服务端拒绝了一条带 `parse_mode` 的消息"，**不是某个具体的
    /// `error_code`**：官方明说错误码内容会变，而这里赌错的代价是一条消息永远发不出去。
    async fn send_rendered(
        &self,
        chat_id: &str,
        message: &RenderedMessage,
    ) -> Result<i64, TelegramError> {
        match self.api.send_message(chat_id, message).await {
            Ok(message_id) => Ok(message_id),
            Err(error) if message.parse_mode.is_some() && error.is_api_refusal() => {
                tracing::debug!(%error, "telegram：MarkdownV2 被拒绝，改发纯文本");
                self.api
                    .send_message(chat_id, &message.without_parse_mode())
                    .await
            }
            Err(error) => Err(error),
        }
    }
}

/// Telegram **从不 `Deferred`**：Bot API 能对任何已加入的会话主动推送（§11.4）。
#[async_trait]
impl ChannelSender for TelegramSender {
    fn platform(&self) -> ChannelPlatform {
        ChannelPlatform::Telegram
    }

    async fn send(&self, peer: &ChannelPeer, msg: Outbound) -> Result<SendOutcome, DeliverError> {
        self.deliver(peer, msg).await.map_err(|error| match error {
            // 平台明确拒绝了这条消息；传输与解析失败是我们这边的事。
            rejected @ TelegramError::Api { .. } => DeliverError::Rejected(rejected.to_string()),
            other => DeliverError::Other(other.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::fake::{Behavior, FakeBotApi};

    // ⑤ MarkdownV2 400 → 纯文本重发。
    #[tokio::test]
    async fn a_refused_markdown_message_is_resent_as_plain_text() {
        let fake = FakeBotApi::start(Behavior::refuse_markdown()).await;
        let sender = TelegramSender::with_api(fake.api());

        let ids = sender
            .send_text(&PeerId::new("11"), "记得 *不要* 忘了 1.5")
            .await
            .expect("退回纯文本之后应当成功");
        assert_eq!(ids.len(), 1);

        let sends = fake.calls_to("sendMessage");
        assert_eq!(sends.len(), 2, "一次 MarkdownV2 + 一次纯文本");
        assert_eq!(sends[0]["parse_mode"], "MarkdownV2");
        assert!(sends[1].get("parse_mode").is_none(), "重发不带 parse_mode");
        assert_eq!(sends[0]["text"], sends[1]["text"], "正文原样重发");
    }

    /// 并入别的 Run 的那条没有回复卡片：视为已处理，一条消息也不发。
    #[tokio::test]
    async fn an_absorbed_run_is_settled_without_sending_anything() {
        let fake = FakeBotApi::start(Behavior::default()).await;
        let sender = TelegramSender::with_api(fake.api());
        let outcome = sender
            .send(
                &ChannelPeer::new(ChannelPlatform::Telegram, "11"),
                Outbound::RunAbsorbed {
                    session: komo_kernel::types::ids::SessionId::from_raw("s-1"),
                    run: komo_kernel::types::ids::RunId::from_raw("run-2"),
                    into: komo_kernel::types::ids::RunId::from_raw("run-1"),
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome, SendOutcome::Sent);
        assert!(fake.calls_to("sendMessage").is_empty());
    }

    #[tokio::test]
    async fn a_plain_text_refusal_is_not_retried() {
        let fake = FakeBotApi::start(Behavior::refuse_everything()).await;
        let sender = TelegramSender::with_api(fake.api());
        let error = sender
            .send_text(&PeerId::new("11"), "hi")
            .await
            .expect_err("服务端一直拒绝");
        assert!(error.is_api_refusal());
        // MarkdownV2 一次 + 纯文本一次，就此打住，不会无限退。
        assert_eq!(fake.calls_to("sendMessage").len(), 2);
    }
}
