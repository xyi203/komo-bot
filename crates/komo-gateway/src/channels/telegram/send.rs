//! Telegram 的出站一侧：`Outbound` → 已发出的消息（§11.3、§11.4）。
//!
//! Telegram **从不 `Deferred`**：Bot API 能对任何已加入的会话主动推送，没有微信那种
//! 回复令牌（§11.4）。所以这里只有 `Sent` 与失败两种结果。
//!
//! 一个进程内的小账：审批请求发出去之后要记住它落在哪条消息上，否则决定之后就没法
//! 原地去掉按钮（§11.3）。这份账是**缓存，不是权威**——权威是 `deliveries` 表，重启
//! 后账没了，`update_after_decision` 退化成补发一条结论文本，而旧消息上仍然长着按钮
//! 的那一种情形由审批本身的幂等兜住（同一 `approval_id` 的第二次决定返回原决定，
//! spike callbacks.md §2）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use time::OffsetDateTime;

use async_trait::async_trait;
use komo_kernel::traits::DeliverError;
use komo_kernel::types::chat::{
    ApprovalPresentation, ChannelPeer, ChannelPlatform, Outbound, PeerId,
};
use komo_kernel::types::ids::{ApprovalId, ShortId};

use super::api::{BotApi, TelegramError};
use crate::channels::{ChannelSender, SendOutcome};
use crate::render::telegram::{self, InlineKeyboard, RenderedMessage};

/// 记住一条已发出的审批请求落在哪儿。
#[derive(Debug, Clone)]
struct SentApproval {
    approval: ApprovalId,
    chat_id: String,
    message_id: i64,
    /// 原正文。决定之后在它后面**追加**结论，而不是把整条换掉——原来的五项还得读。
    text: String,
}

/// 这份进程内的账最多记多少条。
const APPROVAL_MEMO_CAP: usize = 256;

pub struct TelegramSender {
    api: Arc<BotApi>,
    approvals: Mutex<VecDeque<SentApproval>>,
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
        Self {
            api,
            approvals: Mutex::new(VecDeque::new()),
        }
    }

    pub fn api(&self) -> &Arc<BotApi> {
        &self.api
    }

    /// 一个 `Outbound` 送到一个会话。
    ///
    /// `ApprovalSettled` 不是"再发一条"：它把原来那条审批消息**原地**改掉（§11.3）。
    pub async fn deliver(
        &self,
        peer: &ChannelPeer,
        message: Outbound,
    ) -> Result<SendOutcome, TelegramError> {
        match message {
            Outbound::ApprovalSettled {
                approval,
                short_id,
                approved,
                by,
                at,
            } => {
                self.update_after_decision(peer, &approval, &short_id, approved, &by, at)
                    .await?;
                Ok(SendOutcome::Sent)
            }
            Outbound::ApprovalRequest(presentation) => {
                self.send_approval_request(&peer.chat_id, &presentation)
                    .await?;
                Ok(SendOutcome::Sent)
            }
            other => {
                self.send_rendered_all(peer.chat_id.as_str(), &telegram::render(&other))
                    .await?;
                Ok(SendOutcome::Sent)
            }
        }
    }

    /// 一段纯文本（命令回执、被拒绝的提示、Run 的最终回复）。返回每一段的
    /// `message_id`。
    pub async fn send_text(&self, chat_id: &PeerId, text: &str) -> Result<Vec<i64>, TelegramError> {
        let rendered = telegram::render(&Outbound::Text {
            text: text.to_string(),
        });
        self.send_rendered_all(chat_id.as_str(), &rendered).await
    }

    /// 一条审批请求：五项 + 两个按钮。返回**带按钮那一段**的 `message_id`。
    pub async fn send_approval_request(
        &self,
        chat_id: &PeerId,
        presentation: &ApprovalPresentation,
    ) -> Result<i64, TelegramError> {
        let rendered = telegram::render(&Outbound::ApprovalRequest(Box::new(presentation.clone())));
        let sent = self.send_rendered_all(chat_id.as_str(), &rendered).await?;
        let message_id = *sent
            .last()
            .ok_or_else(|| TelegramError::Decode("审批请求渲染成了零条消息".into()))?;
        let text = rendered
            .last()
            .map(|message| message.text.clone())
            .unwrap_or_default();
        self.remember(SentApproval {
            approval: presentation.approval.clone(),
            chat_id: chat_id.as_str().to_string(),
            message_id,
            text,
        });
        Ok(message_id)
    }

    /// 决定之后：去掉按钮，并在原消息末尾写上"已批准 / 已拒绝 · 谁 · 何时"（§11.3）。
    ///
    /// **编辑失败非致命**——决定已经在 Ledger 里，界面回写失败不改变结论，也不去匹配
    /// 错误文案（官方明示 `error_code` 内容将来会变）。只有"这条审批我们根本没记过、
    /// 于是改为补发一条结论文本"这条路上的失败才会返回错误：那一次是真的没送到。
    pub async fn update_after_decision(
        &self,
        peer: &ChannelPeer,
        approval: &ApprovalId,
        short_id: &ShortId,
        approved: bool,
        by: &PeerId,
        at: OffsetDateTime,
    ) -> Result<(), TelegramError> {
        let line = telegram::settled_line(short_id, approved, by, at);
        let remembered = self.recall(approval, peer.chat_id.as_str());
        if remembered.is_empty() {
            // 重启之后这份进程内的账就没了。补发一条结论文本，旧消息上的按钮留着——
            // 再点一次得到的是"已决定"，不是第二次执行。
            tracing::debug!(%approval, "telegram：没有记住这条审批的消息，改为补发结论");
            self.send_rendered_all(peer.chat_id.as_str(), &[RenderedMessage::plain(line)])
                .await?;
            return Ok(());
        }
        for memo in remembered {
            if let Err(error) = self
                .api
                .edit_message_reply_markup(&memo.chat_id, memo.message_id, &InlineKeyboard::empty())
                .await
            {
                tracing::warn!(%approval, %error, "telegram：去掉按钮失败，按非致命处理");
            }
            let updated = RenderedMessage::plain(format!("{}\n\n{}", memo.text, line));
            if let Err(error) = self
                .api
                .edit_message_text(&memo.chat_id, memo.message_id, &updated)
                .await
            {
                tracing::warn!(%approval, %error, "telegram：写回结论失败，按非致命处理");
            }
        }
        Ok(())
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

    fn remember(&self, memo: SentApproval) {
        let mut approvals = self.approvals.lock().expect("审批备忘录锁");
        while approvals.len() >= APPROVAL_MEMO_CAP {
            approvals.pop_front();
        }
        approvals.push_back(memo);
    }

    fn recall(&self, approval: &ApprovalId, chat_id: &str) -> Vec<SentApproval> {
        let approvals = self.approvals.lock().expect("审批备忘录锁");
        approvals
            .iter()
            .filter(|memo| &memo.approval == approval && memo.chat_id == chat_id)
            .cloned()
            .collect()
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

    use komo_kernel::types::chat::ChannelPlatform;

    use super::super::fake::{Behavior, FakeBotApi, presentation};

    fn peer(chat: &str) -> ChannelPeer {
        ChannelPeer::new(ChannelPlatform::Telegram, chat)
    }

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

    // ⑥ 审批消息含五项与两个按钮，callback_data ≤ 64 字节（线上形态）。
    #[tokio::test]
    async fn an_approval_request_goes_out_with_two_buttons() {
        let fake = FakeBotApi::start(Behavior::default()).await;
        let sender = TelegramSender::with_api(fake.api());

        sender
            .send_approval_request(&PeerId::new("11"), &presentation())
            .await
            .unwrap();

        let sends = fake.calls_to("sendMessage");
        assert_eq!(sends.len(), 1);
        let buttons = sends[0]["reply_markup"]["inline_keyboard"][0]
            .as_array()
            .expect("两个按钮");
        assert_eq!(buttons.len(), 2);
        for button in buttons {
            let data = button["callback_data"].as_str().unwrap();
            assert!(data.len() <= telegram::CALLBACK_DATA_LIMIT, "{data}");
        }
        let text = sends[0]["text"].as_str().unwrap();
        for item in ["7K2M", "*动作*", "*改动*", "*原因*", "*范围*"] {
            assert!(text.contains(item), "少了 {item}：{text}");
        }
    }

    // ⑦ 决定后编辑去掉按钮；编辑失败不返回错误。
    #[tokio::test]
    async fn a_decision_removes_the_buttons_in_place() {
        let fake = FakeBotApi::start(Behavior::default()).await;
        let sender = TelegramSender::with_api(fake.api());
        let presentation = presentation();
        let message_id = sender
            .send_approval_request(&PeerId::new("11"), &presentation)
            .await
            .unwrap();

        sender
            .update_after_decision(
                &peer("11"),
                &presentation.approval,
                &presentation.short_id,
                true,
                &PeerId::new("99"),
                OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
            )
            .await
            .unwrap();

        let markup = fake.calls_to("editMessageReplyMarkup");
        assert_eq!(markup.len(), 1);
        assert_eq!(markup[0]["message_id"], message_id);
        assert_eq!(
            markup[0]["reply_markup"]["inline_keyboard"]
                .as_array()
                .unwrap()
                .len(),
            0,
            "按钮必须被去掉"
        );

        let edits = fake.calls_to("editMessageText");
        assert_eq!(edits.len(), 1);
        let text = edits[0]["text"].as_str().unwrap();
        assert!(text.contains("已批准 · 7K2M · 99 · "), "{text}");
        assert!(text.contains("*动作*"), "原来的五项还得留着：{text}");
        assert!(
            fake.calls_to("sendMessage").len() == 1,
            "不该再发一条新消息"
        );
    }

    #[tokio::test]
    async fn a_failed_edit_is_not_an_error() {
        let fake = FakeBotApi::start(Behavior::refuse_edits()).await;
        let sender = TelegramSender::with_api(fake.api());
        let presentation = presentation();
        sender
            .send_approval_request(&PeerId::new("11"), &presentation)
            .await
            .unwrap();

        sender
            .update_after_decision(
                &peer("11"),
                &presentation.approval,
                &presentation.short_id,
                false,
                &PeerId::new("99"),
                OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
            )
            .await
            .expect("界面回写失败不改变结论");

        assert_eq!(fake.calls_to("editMessageReplyMarkup").len(), 1);
        assert_eq!(fake.calls_to("editMessageText").len(), 1);
    }

    #[tokio::test]
    async fn an_unremembered_approval_falls_back_to_a_fresh_line() {
        // 重启之后：账没了，改为补发一条结论文本。
        let fake = FakeBotApi::start(Behavior::default()).await;
        let sender = TelegramSender::with_api(fake.api());
        let presentation = presentation();

        sender
            .update_after_decision(
                &peer("11"),
                &presentation.approval,
                &presentation.short_id,
                true,
                &PeerId::new("99"),
                OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
            )
            .await
            .unwrap();

        assert!(fake.calls_to("editMessageText").is_empty());
        let sends = fake.calls_to("sendMessage");
        assert_eq!(sends.len(), 1);
        assert!(
            sends[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("已批准 · 7K2M · 99 · ")
        );
    }

    // ⑧ 4096 分段：一条超长正文发成多条。
    #[tokio::test]
    async fn an_oversized_body_goes_out_in_segments() {
        let fake = FakeBotApi::start(Behavior::default()).await;
        let sender = TelegramSender::with_api(fake.api());

        let body = "x".repeat(telegram::MESSAGE_LIMIT * 2 + 1);
        let ids = sender.send_text(&PeerId::new("11"), &body).await.unwrap();
        assert_eq!(ids.len(), 3);

        let sends = fake.calls_to("sendMessage");
        assert_eq!(sends.len(), 3);
        let mut rejoined = String::new();
        for send in &sends {
            let text = send["text"].as_str().unwrap();
            assert!(telegram::utf16_len(text) <= telegram::MESSAGE_LIMIT);
            rejoined.push_str(text);
        }
        assert_eq!(rejoined, body);
    }

    #[tokio::test]
    async fn telegram_never_defers() {
        let fake = FakeBotApi::start(Behavior::default()).await;
        let sender = TelegramSender::with_api(fake.api());
        let outcome = sender
            .send(&peer("11"), Outbound::Text { text: "hi".into() })
            .await
            .unwrap();
        assert_eq!(outcome, SendOutcome::Sent);
        assert_eq!(sender.platform(), ChannelPlatform::Telegram);
    }
}
