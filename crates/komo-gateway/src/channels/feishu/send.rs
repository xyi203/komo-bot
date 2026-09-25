//! 飞书的出站一侧：`Outbound` → 已发出的消息（§11.3、§11.4）。
//!
//! 飞书**从不 `Deferred`**：开放平台能对任何机器人已加入的会话主动推送，没有微信那种
//! 回复令牌（§11.4）。所以这里只有 `Sent` 与失败两种结果。
//!
//! 一个进程内的小账：审批卡片发出去之后要记住它落在哪条消息上、当时长什么样，否则决定
//! 之后就没法原地把它换成无按钮的那张（§11.3）。这份账是**缓存，不是权威**——权威是
//! `deliveries` 表，重启后账没了，`update_after_decision` 退化成补发一条结论文本，而
//! 旧卡片上仍然长着按钮的那一种情形由审批本身的幂等兜住（同一 `approval_id` 的第二次
//! 决定返回原决定，spike callbacks.md §2）。
//!
//! 为什么连**整张卡**一起记而不是只记 `message_id`：飞书没有"只去掉按钮"的接口，PATCH
//! 换的是整张卡，所以要把五项原样写回去就必须手里还有它。
//!
//! 第二份同样性质的小账记 Run 的回复卡片：收到消息时回一张"处理中"，`RunFinished` 到了
//! 就把它 PATCH 成结果（§11.3）。槽位在 `Queued` 那一刻、任何网络调用之前登记，卡片发出
//! 去之后才填上 `message_id`——终态投递来自 Run 的看客任务，与发卡片并发，可能先到。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::watch;

use komo_kernel::traits::DeliverError;
use komo_kernel::types::chat::{
    ApprovalPresentation, ChannelPeer, ChannelPlatform, Outbound, PeerId,
};
use komo_kernel::types::ids::{ApprovalId, RunId, ShortId};

use super::api::{FeishuApi, FeishuError};
use crate::channels::{ChannelSender, SendOutcome};
use crate::render::feishu::{self, RenderedMessage};

/// 记住一张已发出的审批卡片落在哪儿、长什么样。
#[derive(Debug, Clone)]
struct SentApproval {
    approval: ApprovalId,
    chat_id: String,
    message_id: String,
    /// 原卡片。决定之后由它派生出无按钮的那张——原来的五项还得读。
    card: Value,
}

/// 这份进程内的账最多记多少条。
const APPROVAL_MEMO_CAP: usize = 256;

/// Run 回复卡片的账最多记多少条。同时在跑的聊天 Run 远少于这个数；挤出去的那条只是
/// 终态退化成发文本。
const RUN_CARD_MEMO_CAP: usize = 256;

/// 投递 `RunFinished` 时卡片还在路上，最多等它多久。
///
/// 加表情 + 回复卡片正常是两次平台往返（飞书一次约 300ms，§14），5 秒是十倍以上的余量；
/// 再长就是拿最终回复的到达时间去赌一次卡住的调用。等不到就发文本，卡片之后到了由发卡片
/// 的那一侧自己收尾（[`RunCardTicket::sent`]），不会停在"处理中"。
const CARD_SETTLE_WAIT: Duration = Duration::from_secs(5);

/// 一张 Run 回复卡片此刻的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
enum CardSlot {
    /// 登记了，卡片还在路上。
    Pending,
    Sent(String),
    /// 卡片没发出去：终态照旧发文本。
    Failed,
    /// 终态等不到卡片，已经改发文本；卡片之后到了要自己收尾。
    GaveUp,
}

/// 记住一张 Run 回复卡片落在哪个会话、此刻到了哪一步。
#[derive(Debug)]
struct RunCard {
    run: RunId,
    chat_id: String,
    slot: Arc<watch::Sender<CardSlot>>,
}

/// 发卡片那一侧手里的凭据：卡片发出去（或没发出去）之后由它填槽位。
///
/// 丢掉而没填（中途 panic、提前返回）按"没发出去"算，否则终态会白等满
/// [`CARD_SETTLE_WAIT`]。
#[derive(Debug)]
pub struct RunCardTicket {
    slot: Arc<watch::Sender<CardSlot>>,
}

impl RunCardTicket {
    /// 卡片发出去了。返回 `false` = 终态已经等不及改发了文本，这张卡要调用方收尾。
    fn sent(self, message_id: String) -> bool {
        let mut claimed = false;
        self.slot.send_if_modified(|slot| {
            if *slot == CardSlot::Pending {
                *slot = CardSlot::Sent(message_id);
                claimed = true;
                true
            } else {
                false
            }
        });
        claimed
    }

    fn failed(self) {}
}

impl Drop for RunCardTicket {
    fn drop(&mut self) {
        self.slot.send_if_modified(|slot| {
            if *slot == CardSlot::Pending {
                *slot = CardSlot::Failed;
                true
            } else {
                false
            }
        });
    }
}

/// 收到消息时给原消息加的表情：飞书官方表情列表里的 `Get`。
const RECEIVED_EMOJI: &str = "Get";

pub struct FeishuSender {
    api: Arc<FeishuApi>,
    approvals: Mutex<VecDeque<SentApproval>>,
    run_cards: Mutex<VecDeque<RunCard>>,
}

impl std::fmt::Debug for FeishuSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeishuSender").finish_non_exhaustive()
    }
}

impl FeishuSender {
    pub fn with_api(api: Arc<FeishuApi>) -> Self {
        Self {
            api,
            approvals: Mutex::new(VecDeque::new()),
            run_cards: Mutex::new(VecDeque::new()),
        }
    }

    pub fn api(&self) -> &Arc<FeishuApi> {
        &self.api
    }

    /// 一个 `Outbound` 送到一个会话。
    ///
    /// `ApprovalSettled` 不是"再发一条"：它把原来那张卡片**原地**换掉（§11.3）；
    /// `RunFinished` 有回复卡片就换掉那张卡片，没有才发文本。
    pub async fn deliver(
        &self,
        peer: &ChannelPeer,
        message: Outbound,
    ) -> Result<SendOutcome, FeishuError> {
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
            Outbound::RunFinished { run, summary, .. } => {
                self.finish_run(peer, &run, &summary).await?;
                Ok(SendOutcome::Sent)
            }
            Outbound::ApprovalRequest(presentation) => {
                self.send_approval_request(&peer.chat_id, &presentation)
                    .await?;
                Ok(SendOutcome::Sent)
            }
            other => {
                self.send_all(peer.chat_id.as_str(), &feishu::render(&other))
                    .await?;
                Ok(SendOutcome::Sent)
            }
        }
    }

    /// 一段纯文本（命令回执、被拒绝的提示、Run 的最终回复）。超长的在渲染那一步就已经
    /// 切好段，这里按顺序发出去，返回每一段的 `message_id`。
    pub async fn send_text(
        &self,
        chat_id: &PeerId,
        text: &str,
    ) -> Result<Vec<String>, FeishuError> {
        self.send_all(chat_id.as_str(), &feishu::text_segments(text))
            .await
    }

    /// 一张审批卡片：五项 + 两个按钮。返回它的 `message_id`。
    pub async fn send_approval_request(
        &self,
        chat_id: &PeerId,
        presentation: &ApprovalPresentation,
    ) -> Result<String, FeishuError> {
        let card = feishu::approval_card(presentation);
        let rendered = RenderedMessage::card(&card);
        let message_id = self.send_one(chat_id.as_str(), &rendered).await?;
        self.remember(SentApproval {
            approval: presentation.approval.clone(),
            chat_id: chat_id.as_str().to_string(),
            message_id: message_id.clone(),
            card,
        });
        Ok(message_id)
    }

    /// 决定之后：把卡片原地换成"已批准 / 已拒绝 · 谁 · 何时"的无按钮版本（§11.3）。
    ///
    /// **PATCH 失败非致命**——决定已经在 Ledger 里，界面回写失败不改变结论，也不去匹配
    /// 错误文案。只有"这张卡我们根本没记过、于是改为补发一条结论文本"这条路上的失败才
    /// 会返回错误：那一次是真的没送到。
    pub async fn update_after_decision(
        &self,
        peer: &ChannelPeer,
        approval: &ApprovalId,
        short_id: &ShortId,
        approved: bool,
        by: &PeerId,
        at: OffsetDateTime,
    ) -> Result<(), FeishuError> {
        let remembered = self.recall(approval, peer.chat_id.as_str());
        if remembered.is_empty() {
            // 重启之后这份进程内的账就没了。补发一条结论文本，旧卡片上的按钮留着——
            // 再点一次得到的是"已决定"，不是第二次执行。
            tracing::debug!(%approval, "飞书：没有记住这条审批的卡片，改为补发结论");
            let line = feishu::settled_line(short_id, approved, by, at);
            self.send_all(peer.chat_id.as_str(), &feishu::text_segments(&line))
                .await?;
            return Ok(());
        }
        for memo in remembered {
            let settled = feishu::settled_card(Some(&memo.card), short_id, approved, by, at);
            if let Err(error) = self
                .api
                .patch_message(&memo.message_id, &settled.to_string())
                .await
            {
                tracing::warn!(%approval, %error, "飞书：更新审批卡片失败，按非致命处理");
            }
        }
        Ok(())
    }

    /// 给原消息加一个"收到了"的表情。
    pub async fn react_received(&self, message_id: &str) -> Result<(), FeishuError> {
        self.api.add_reaction(message_id, RECEIVED_EMOJI).await
    }

    /// 登记一张即将发出的 Run 回复卡片。**必须在任何网络调用之前**：终态可能在卡片发出去
    /// 之前就到，那时它要看得见"有一张卡在路上"。
    pub fn expect_run_card(&self, run: &RunId, chat_id: &PeerId) -> RunCardTicket {
        let slot = Arc::new(watch::Sender::new(CardSlot::Pending));
        let mut cards = self.run_cards.lock().expect("Run 卡片备忘录锁");
        while cards.len() >= RUN_CARD_MEMO_CAP {
            cards.pop_front();
        }
        cards.push_back(RunCard {
            run: run.clone(),
            chat_id: chat_id.as_str().to_string(),
            slot: Arc::clone(&slot),
        });
        RunCardTicket { slot }
    }

    /// 回复原消息发"处理中"卡片，并把结果填进登记好的槽位。返回卡片的 `message_id`。
    ///
    /// 终态已经等不及、改发了文本时，把刚发出去的卡片收尾成"结果见下方消息"。
    pub async fn reply_run_card(
        &self,
        ticket: RunCardTicket,
        origin: &str,
    ) -> Result<String, FeishuError> {
        let pending = RenderedMessage::card(&feishu::run_pending_card());
        let message_id = match self
            .api
            .reply_message(origin, pending.msg_type, &pending.content)
            .await
        {
            Ok(message_id) => message_id,
            Err(error) => {
                ticket.failed();
                return Err(error);
            }
        };
        if !ticket.sent(message_id.clone()) {
            self.api
                .patch_message(&message_id, &feishu::run_moved_card().to_string())
                .await?;
        }
        Ok(message_id)
    }

    /// Run 终态：有回复卡片就原地换成结果，没有（重启后账没了、cron 投 home、卡片没发
    /// 出去）就发文本。
    ///
    /// PATCH 失败退化成发文本，这一次必须真的送到——`deliveries` 那一行靠它结算。
    async fn finish_run(
        &self,
        peer: &ChannelPeer,
        run: &RunId,
        summary: &str,
    ) -> Result<(), FeishuError> {
        let chat_id = peer.chat_id.as_str();
        let card_id = match self.take_run_card(run, chat_id) {
            Some(slot) => settle(&slot, CARD_SETTLE_WAIT).await,
            None => None,
        };
        let Some(message_id) = card_id else {
            self.send_all(chat_id, &feishu::text_segments(summary))
                .await?;
            return Ok(());
        };
        let (card, follow) = feishu::run_result_card(summary);
        if let Err(error) = self.api.patch_message(&message_id, &card.to_string()).await {
            tracing::warn!(%run, %error, "飞书：更新 Run 卡片失败，改发文本");
            self.send_all(chat_id, &feishu::text_segments(summary))
                .await?;
            return Ok(());
        }
        self.send_all(chat_id, &follow).await?;
        Ok(())
    }

    fn take_run_card(&self, run: &RunId, chat_id: &str) -> Option<Arc<watch::Sender<CardSlot>>> {
        let mut cards = self.run_cards.lock().expect("Run 卡片备忘录锁");
        let index = cards
            .iter()
            .position(|card| &card.run == run && card.chat_id == chat_id)?;
        cards.remove(index).map(|card| card.slot)
    }

    async fn send_all(
        &self,
        chat_id: &str,
        messages: &[RenderedMessage],
    ) -> Result<Vec<String>, FeishuError> {
        let mut ids = Vec::with_capacity(messages.len());
        for message in messages {
            ids.push(self.send_one(chat_id, message).await?);
        }
        Ok(ids)
    }

    async fn send_one(
        &self,
        chat_id: &str,
        message: &RenderedMessage,
    ) -> Result<String, FeishuError> {
        self.api
            .send_message(chat_id, message.msg_type, &message.content)
            .await
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

/// 等卡片落定，最多 `wait`。返回卡片的 `message_id`；`None` = 发文本。
///
/// 等不到时把槽位标成 `GaveUp`，与 [`RunCardTicket::sent`] 在同一把锁里比较并交换：
/// 两边谁先到都只有一方认领那张卡片，不会"文本发了、卡片也没人收尾"。
async fn settle(slot: &watch::Sender<CardSlot>, wait: Duration) -> Option<String> {
    let mut watcher = slot.subscribe();
    // 结果立刻丢掉：`wait_for` 返回的是读锁守卫，拿着它去 `send_if_modified` 会死锁。
    let _ = tokio::time::timeout(wait, watcher.wait_for(|state| *state != CardSlot::Pending)).await;
    let mut message_id = None;
    slot.send_if_modified(|state| match state {
        CardSlot::Pending => {
            *state = CardSlot::GaveUp;
            true
        }
        CardSlot::Sent(id) => {
            message_id = Some(id.clone());
            false
        }
        CardSlot::Failed | CardSlot::GaveUp => false,
    });
    message_id
}

/// 飞书**从不 `Deferred`**：开放平台能对任何已加入的会话主动推送（§11.4）。
#[async_trait]
impl ChannelSender for FeishuSender {
    fn platform(&self) -> ChannelPlatform {
        ChannelPlatform::Feishu
    }

    async fn send(&self, peer: &ChannelPeer, msg: Outbound) -> Result<SendOutcome, DeliverError> {
        self.deliver(peer, msg).await.map_err(|error| match error {
            // 平台明确拒绝了这条消息；传输与解析失败是我们这边的事。
            refused @ FeishuError::Api { .. } => DeliverError::Rejected(refused.to_string()),
            other => DeliverError::Other(other.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    use super::super::fake::{Behavior, FakeOpenApi, presentation};

    fn peer(chat: &str) -> ChannelPeer {
        ChannelPeer::new(ChannelPlatform::Feishu, chat)
    }

    fn sender(fake: &FakeOpenApi) -> FeishuSender {
        FeishuSender::with_api(fake.api())
    }

    // ⑤ 审批卡片含五项、两个按钮、update_multi: true（线上形态）。
    #[tokio::test]
    async fn an_approval_card_goes_out_as_an_interactive_message() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);

        let message_id = sender
            .send_approval_request(&PeerId::new("oc_1"), &presentation())
            .await
            .expect("发卡片");
        assert!(message_id.starts_with("om_"));

        let sends = fake.calls_to("messages");
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0]["receive_id"], "oc_1");
        assert_eq!(sends[0]["msg_type"], "interactive");

        let card: Value = serde_json::from_str(sends[0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(card["config"]["update_multi"], json!(true));
        assert!(card.to_string().contains("7K2M"));
        let actions = card["body"]["elements"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .get("columns")
            .and_then(Value::as_array)
            .expect("两个按钮");
        assert_eq!(actions.len(), 2);
    }

    // ⑥ 决定之后 PATCH 成无按钮卡片。
    #[tokio::test]
    async fn a_decision_replaces_the_card_in_place() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);
        let presentation = presentation();
        let message_id = sender
            .send_approval_request(&PeerId::new("oc_1"), &presentation)
            .await
            .unwrap();

        sender
            .update_after_decision(
                &peer("oc_1"),
                &presentation.approval,
                &presentation.short_id,
                true,
                &PeerId::new("ou_op"),
                OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
            )
            .await
            .unwrap();

        let patches = fake.calls_to("patch");
        assert_eq!(patches.len(), 1);
        assert_eq!(fake.patched_message_ids(), vec![message_id]);
        let card: Value = serde_json::from_str(patches[0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            card["config"]["update_multi"],
            json!(true),
            "PATCH 前后都要"
        );
        assert!(
            !card["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["tag"] == json!("action")),
            "决定过的请求不该还长着可点的按钮"
        );
        let flat = card.to_string();
        assert!(flat.contains("已批准 · 7K2M · ou_op · "), "{flat}");
        assert!(flat.contains("工具: shell"), "原来的五项还得留着：{flat}");
        assert_eq!(fake.calls_to("messages").len(), 1, "不该再发一条新消息");
    }

    // ⑥ PATCH 失败不报错。
    #[tokio::test]
    async fn a_failed_patch_is_not_an_error() {
        let fake = FakeOpenApi::start(Behavior::refuse_patch()).await;
        let sender = sender(&fake);
        let presentation = presentation();
        sender
            .send_approval_request(&PeerId::new("oc_1"), &presentation)
            .await
            .unwrap();

        sender
            .update_after_decision(
                &peer("oc_1"),
                &presentation.approval,
                &presentation.short_id,
                false,
                &PeerId::new("ou_op"),
                OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
            )
            .await
            .expect("界面回写失败不改变结论");

        assert_eq!(fake.calls_to("patch").len(), 1);
    }

    // ⑥ 找不到 message_id 就补发文本。
    #[tokio::test]
    async fn an_unremembered_approval_falls_back_to_a_fresh_line() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);
        let presentation = presentation();

        sender
            .update_after_decision(
                &peer("oc_1"),
                &presentation.approval,
                &presentation.short_id,
                true,
                &PeerId::new("ou_op"),
                OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
            )
            .await
            .unwrap();

        assert!(fake.calls_to("patch").is_empty());
        let sends = fake.calls_to("messages");
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0]["msg_type"], "text");
        let content: Value = serde_json::from_str(sends[0]["content"].as_str().unwrap()).unwrap();
        assert!(
            content["text"]
                .as_str()
                .unwrap()
                .starts_with("已批准 · 7K2M · ou_op · ")
        );
    }

    #[tokio::test]
    async fn a_decision_in_another_chat_does_not_touch_this_ones_card() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);
        let presentation = presentation();
        sender
            .send_approval_request(&PeerId::new("oc_1"), &presentation)
            .await
            .unwrap();

        // 同一条审批也投到了 home chat；那一边的卡片不在这份账里。
        sender
            .update_after_decision(
                &peer("oc_home"),
                &presentation.approval,
                &presentation.short_id,
                true,
                &PeerId::new("ou_op"),
                OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
            )
            .await
            .unwrap();
        assert!(fake.calls_to("patch").is_empty(), "别改别人会话里的卡");
    }

    // ⑧ 超长分段：一条超长正文发成多条。
    #[tokio::test]
    async fn an_oversized_body_goes_out_in_segments() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);

        let body = "x".repeat(feishu::MESSAGE_LIMIT * 2 + 1);
        let ids = sender.send_text(&PeerId::new("oc_1"), &body).await.unwrap();
        assert_eq!(ids.len(), 3);

        let sends = fake.calls_to("messages");
        assert_eq!(sends.len(), 3);
        let mut rejoined = String::new();
        for send in &sends {
            let content: Value = serde_json::from_str(send["content"].as_str().unwrap()).unwrap();
            let text = content["text"].as_str().unwrap();
            assert!(text.chars().count() <= feishu::MESSAGE_LIMIT);
            rejoined.push_str(text);
        }
        assert_eq!(rejoined, body);
    }

    #[tokio::test]
    async fn a_refused_send_is_a_rejection_not_an_internal_error() {
        let fake = FakeOpenApi::start(Behavior::refuse_sends()).await;
        let error = sender(&fake)
            .send(&peer("oc_1"), Outbound::Text { text: "在".into() })
            .await
            .expect_err("平台拒绝");
        assert!(matches!(error, DeliverError::Rejected(_)), "{error:?}");
    }

    #[tokio::test]
    async fn feishu_never_defers() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);
        let outcome = sender
            .send(&peer("oc_1"), Outbound::Text { text: "在".into() })
            .await
            .unwrap();
        assert_eq!(outcome, SendOutcome::Sent);
        assert_eq!(sender.platform(), ChannelPlatform::Feishu);
    }

    fn run_finished(summary: &str) -> Outbound {
        Outbound::RunFinished {
            session: komo_kernel::types::ids::SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            summary: summary.into(),
        }
    }

    /// 登记并发出一张处理中卡片，返回它的 `message_id`。
    async fn open_card(fake: &FakeOpenApi, sender: &FeishuSender) -> String {
        let ticket = sender.expect_run_card(&RunId::from_raw("run-1"), &PeerId::new("oc_1"));
        let message_id = sender
            .reply_run_card(ticket, "om_origin")
            .await
            .expect("处理中卡片");
        let replies = fake.calls_to("reply");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["target"], "om_origin");
        assert_eq!(replies[0]["msg_type"], "interactive");
        let card: Value = serde_json::from_str(replies[0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(card["config"]["update_multi"], json!(true));
        assert!(card.to_string().contains("处理中"), "{card}");
        message_id
    }

    fn patched_card(fake: &FakeOpenApi, index: usize) -> Value {
        serde_json::from_str(fake.calls_to("patch")[index]["content"].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_run_finished_replaces_its_card_instead_of_sending_a_message() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);
        let card_id = open_card(&fake, &sender).await;

        sender
            .send(&peer("oc_1"), run_finished("**跑完了**"))
            .await
            .unwrap();

        assert_eq!(fake.patched_message_ids(), vec![card_id]);
        let card = patched_card(&fake, 0);
        assert_eq!(
            card["config"]["update_multi"],
            json!(true),
            "PATCH 前后都要"
        );
        assert_eq!(card["body"]["elements"][0]["content"], json!("**跑完了**"));
        assert!(fake.calls_to("messages").is_empty(), "不该再发一条新消息");
    }

    #[tokio::test]
    async fn an_oversized_result_fills_the_card_and_sends_the_rest_as_text() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);
        open_card(&fake, &sender).await;

        let summary = format!(
            "{}{}",
            "a".repeat(feishu::RESULT_CARD_LIMIT),
            "b".repeat(20)
        );
        sender
            .send(&peer("oc_1"), run_finished(&summary))
            .await
            .unwrap();

        let card = patched_card(&fake, 0);
        assert_eq!(
            card["body"]["elements"][0]["content"],
            json!("a".repeat(feishu::RESULT_CARD_LIMIT))
        );
        assert!(card.to_string().contains("其余见下方消息"));
        let sends = fake.calls_to("messages");
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0]["receive_id"], "oc_1");
        assert_eq!(
            sends[0]["content"],
            json!({ "text": "b".repeat(20) }).to_string()
        );
    }

    #[tokio::test]
    async fn a_failed_card_update_falls_back_to_text() {
        let fake = FakeOpenApi::start(Behavior::refuse_patch()).await;
        let sender = sender(&fake);
        open_card(&fake, &sender).await;

        sender
            .send(&peer("oc_1"), run_finished("跑完了"))
            .await
            .expect("文本送到了就算送到");

        assert_eq!(fake.calls_to("patch").len(), 1);
        let sends = fake.calls_to("messages");
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0]["content"], json!({ "text": "跑完了" }).to_string());
    }

    #[tokio::test]
    async fn a_result_that_beats_its_card_waits_and_still_updates_the_card() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = Arc::new(sender(&fake));
        let ticket = sender.expect_run_card(&RunId::from_raw("run-1"), &PeerId::new("oc_1"));

        let finishing = {
            let sender = Arc::clone(&sender);
            tokio::spawn(async move {
                sender
                    .send(&peer("oc_1"), run_finished("跑完了"))
                    .await
                    .unwrap()
            })
        };
        // 终态先到、在槽位上等着；卡片随后才发出去。
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!finishing.is_finished(), "卡片还在路上时终态该等着");
        let card_id = sender.reply_run_card(ticket, "om_origin").await.unwrap();
        finishing.await.unwrap();

        assert_eq!(fake.patched_message_ids(), vec![card_id]);
        assert!(
            fake.calls_to("messages").is_empty(),
            "不该既发文本又留着卡片"
        );
    }

    #[tokio::test]
    async fn a_card_that_never_went_out_lets_the_result_go_as_text_at_once() {
        let fake = FakeOpenApi::start(Behavior::refuse_replies()).await;
        let sender = sender(&fake);
        let ticket = sender.expect_run_card(&RunId::from_raw("run-1"), &PeerId::new("oc_1"));
        assert!(sender.reply_run_card(ticket, "om_origin").await.is_err());

        let started = std::time::Instant::now();
        sender
            .send(&peer("oc_1"), run_finished("跑完了"))
            .await
            .unwrap();
        assert!(
            started.elapsed() < CARD_SETTLE_WAIT,
            "没发出去的卡片不值得等"
        );
        assert!(fake.calls_to("patch").is_empty());
        assert_eq!(fake.calls_to("messages").len(), 1);
    }

    #[tokio::test]
    async fn a_card_that_arrives_after_the_result_gave_up_is_closed_not_left_pending() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);
        let ticket = sender.expect_run_card(&RunId::from_raw("run-1"), &PeerId::new("oc_1"));

        // 终态等满了也没等到：槽位标成 GaveUp，改发文本。
        let slot = sender
            .take_run_card(&RunId::from_raw("run-1"), "oc_1")
            .expect("登记过");
        assert_eq!(settle(&slot, Duration::from_millis(10)).await, None);

        // 卡片这才发出去：它自己收尾成"结果见下方消息"。
        let card_id = sender.reply_run_card(ticket, "om_origin").await.unwrap();
        assert_eq!(fake.patched_message_ids(), vec![card_id]);
        assert!(
            patched_card(&fake, 0)
                .to_string()
                .contains("结果见下方消息")
        );
    }

    #[tokio::test]
    async fn a_result_for_another_chat_does_not_touch_this_card() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);
        open_card(&fake, &sender).await;

        sender
            .send(&peer("oc_home"), run_finished("跑完了"))
            .await
            .unwrap();
        assert!(fake.calls_to("patch").is_empty(), "别改别人会话里的卡");
        assert_eq!(fake.calls_to("messages")[0]["receive_id"], "oc_home");
    }

    #[tokio::test]
    async fn a_run_finished_without_a_card_goes_out_as_text() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        sender(&fake)
            .send(
                &peer("oc_1"),
                Outbound::RunFinished {
                    session: komo_kernel::types::ids::SessionId::from_raw("s-1"),
                    run: komo_kernel::types::ids::RunId::from_raw("run-1"),
                    summary: "跑完了".into(),
                },
            )
            .await
            .unwrap();
        let sends = fake.calls_to("messages");
        assert_eq!(sends[0]["msg_type"], "text");
        assert_eq!(sends[0]["content"], json!({ "text": "跑完了" }).to_string());
        assert!(fake.calls_to("patch").is_empty());
    }
}
