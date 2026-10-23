//! 飞书的出站一侧：`Outbound` → 已发出的消息（§11.3、§11.4）。
//!
//! 飞书**从不 `Deferred`**：开放平台能对任何机器人已加入的会话主动推送，没有微信那种
//! 回复令牌（§11.4）。所以这里只有 `Sent` 与失败两种结果。
//!
//! 审批卡片与"决定之后原地换卡"随审批一起删掉了（§7，2026-10-11）。
//!
//! 一份进程内的小账记 Run 的回复卡片：收到消息时回一张"处理中"，`RunFinished` 到了
//! 就把它 PATCH 成结果（§11.3）。槽位在 `Queued` 那一刻、任何网络调用之前登记，卡片发出
//! 去之后才填上 `message_id`——终态投递来自 Run 的看客任务，与发卡片并发，可能先到。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;

use komo_kernel::traits::DeliverError;
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, Outbound};
use komo_kernel::types::ids::RunId;

use super::api::{FeishuApi, FeishuError};
use crate::channels::{ChannelSender, SendOutcome};
use crate::render::feishu::{self, RenderedMessage};

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
    peer: ChannelPeer,
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
            run_cards: Mutex::new(VecDeque::new()),
        }
    }

    pub fn api(&self) -> &Arc<FeishuApi> {
        &self.api
    }

    /// 一个 `Outbound` 送到一个会话。
    ///
    /// `RunFinished` 有回复卡片就换掉那张卡片，没有才发文本；`RunAbsorbed` 只收尾卡片。
    pub async fn deliver(
        &self,
        peer: &ChannelPeer,
        message: Outbound,
    ) -> Result<SendOutcome, FeishuError> {
        match message {
            Outbound::RunFinished { run, summary, .. } => {
                self.finish_run(peer, &run, &summary).await?;
                Ok(SendOutcome::Sent)
            }
            Outbound::RunAbsorbed { run, .. } => {
                self.absorb_run(peer, &run).await;
                Ok(SendOutcome::Sent)
            }
            other => {
                self.send_all(peer, &feishu::render(&other)).await?;
                Ok(SendOutcome::Sent)
            }
        }
    }

    /// 一段纯文本（命令回执、被拒绝的提示、Run 的最终回复）。超长的在渲染那一步就已经
    /// 切好段，这里按顺序发出去，返回每一段的 `message_id`。
    pub async fn send_text(
        &self,
        peer: &ChannelPeer,
        text: &str,
    ) -> Result<Vec<String>, FeishuError> {
        self.send_all(peer, &feishu::text_segments(text)).await
    }

    /// 给原消息加一个"收到了"的表情。
    pub async fn react_received(&self, message_id: &str) -> Result<(), FeishuError> {
        self.api.add_reaction(message_id, RECEIVED_EMOJI).await
    }

    /// 登记一张即将发出的 Run 回复卡片。**必须在任何网络调用之前**：终态可能在卡片发出去
    /// 之前就到，那时它要看得见"有一张卡在路上"。
    pub fn expect_run_card(&self, run: &RunId, peer: &ChannelPeer) -> RunCardTicket {
        let slot = Arc::new(watch::Sender::new(CardSlot::Pending));
        let mut cards = self.run_cards.lock().expect("Run 卡片备忘录锁");
        while cards.len() >= RUN_CARD_MEMO_CAP {
            cards.pop_front();
        }
        cards.push_back(RunCard {
            run: run.clone(),
            peer: peer.clone(),
            slot: Arc::clone(&slot),
        });
        RunCardTicket { slot }
    }

    /// 回复原消息发"处理中"卡片，并把结果填进登记好的槽位。返回卡片的 `message_id`。
    ///
    /// `in_thread`：群里的会话都是话题。主流里原消息就是话题根，这一回复开出话题；话题里
    /// 的消息则直接落进话题。
    ///
    /// 终态已经等不及、改发了文本时，把刚发出去的卡片收尾成"结果见下方消息"。
    pub async fn reply_run_card(
        &self,
        ticket: RunCardTicket,
        origin: &str,
        in_thread: bool,
    ) -> Result<String, FeishuError> {
        let pending = RenderedMessage::card(&feishu::run_pending_card());
        let message_id = match self
            .api
            .reply_message(origin, pending.msg_type, &pending.content, in_thread)
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
        let card_id = match self.take_run_card(run, peer) {
            Some(slot) => settle(&slot, CARD_SETTLE_WAIT).await,
            None => None,
        };
        let Some(message_id) = card_id else {
            self.send_all(peer, &feishu::text_segments(summary)).await?;
            return Ok(());
        };
        let (card, follow) = feishu::run_result_card(summary);
        if let Err(error) = self.api.patch_message(&message_id, &card.to_string()).await {
            tracing::warn!(%run, %error, "飞书：更新 Run 卡片失败，改发文本");
            self.send_all(peer, &feishu::text_segments(summary)).await?;
            return Ok(());
        }
        self.send_all(peer, &follow).await?;
        Ok(())
    }

    /// Run 并进了正在跑的另一条（§6 插话）：它的"处理中"卡片换成灰色的"已并入"。
    ///
    /// 没有卡片（重启后账没了、卡片没发出去）就什么都不发——回答在宿主那条 Run 上，
    /// 这里补一条文本只是刷屏。PATCH 失败也只记日志：重试时账里的卡片已经取走，重投
    /// 什么也做不了。
    async fn absorb_run(&self, peer: &ChannelPeer, run: &RunId) {
        let Some(slot) = self.take_run_card(run, peer) else {
            return;
        };
        let Some(message_id) = settle(&slot, CARD_SETTLE_WAIT).await else {
            return;
        };
        if let Err(error) = self
            .api
            .patch_message(&message_id, &feishu::run_absorbed_card().to_string())
            .await
        {
            tracing::warn!(%run, %error, "飞书：把被并入的 Run 卡片收尾失败");
        }
    }

    fn take_run_card(
        &self,
        run: &RunId,
        peer: &ChannelPeer,
    ) -> Option<Arc<watch::Sender<CardSlot>>> {
        let mut cards = self.run_cards.lock().expect("Run 卡片备忘录锁");
        let index = cards
            .iter()
            .position(|card| &card.run == run && &card.peer == peer)?;
        cards.remove(index).map(|card| card.slot)
    }

    async fn send_all(
        &self,
        peer: &ChannelPeer,
        messages: &[RenderedMessage],
    ) -> Result<Vec<String>, FeishuError> {
        let mut ids = Vec::with_capacity(messages.len());
        for message in messages {
            ids.push(self.send_one(peer, message).await?);
        }
        Ok(ids)
    }

    /// 话题只能靠回复话题根进去：发消息接口不能指定话题。
    async fn send_one(
        &self,
        peer: &ChannelPeer,
        message: &RenderedMessage,
    ) -> Result<String, FeishuError> {
        match &peer.thread {
            Some(root) => {
                self.api
                    .reply_message(root.as_str(), message.msg_type, &message.content, true)
                    .await
            }
            None => {
                self.api
                    .send_message(peer.chat_id.as_str(), message.msg_type, &message.content)
                    .await
            }
        }
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

    use serde_json::{Value, json};

    use super::super::fake::{Behavior, FakeOpenApi};

    fn peer(chat: &str) -> ChannelPeer {
        ChannelPeer::new(ChannelPlatform::Feishu, chat)
    }

    fn thread_peer(chat: &str, root: &str) -> ChannelPeer {
        peer(chat).in_thread(root)
    }

    fn sender(fake: &FakeOpenApi) -> FeishuSender {
        FeishuSender::with_api(fake.api())
    }

    // ⑧ 超长分段：一条超长正文发成多条。
    #[tokio::test]
    async fn an_oversized_body_goes_out_in_segments() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);

        let body = "x".repeat(feishu::MESSAGE_LIMIT * 2 + 1);
        let ids = sender.send_text(&peer("oc_1"), &body).await.unwrap();
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
        let ticket = sender.expect_run_card(&RunId::from_raw("run-1"), &peer("oc_1"));
        let message_id = sender
            .reply_run_card(ticket, "om_origin", false)
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
        let ticket = sender.expect_run_card(&RunId::from_raw("run-1"), &peer("oc_1"));

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
        let card_id = sender
            .reply_run_card(ticket, "om_origin", false)
            .await
            .unwrap();
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
        let ticket = sender.expect_run_card(&RunId::from_raw("run-1"), &peer("oc_1"));
        assert!(
            sender
                .reply_run_card(ticket, "om_origin", false)
                .await
                .is_err()
        );

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
        let ticket = sender.expect_run_card(&RunId::from_raw("run-1"), &peer("oc_1"));

        // 终态等满了也没等到：槽位标成 GaveUp，改发文本。
        let slot = sender
            .take_run_card(&RunId::from_raw("run-1"), &peer("oc_1"))
            .expect("登记过");
        assert_eq!(settle(&slot, Duration::from_millis(10)).await, None);

        // 卡片这才发出去：它自己收尾成"结果见下方消息"。
        let card_id = sender
            .reply_run_card(ticket, "om_origin", false)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn a_text_to_a_thread_peer_replies_on_the_root() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        sender(&fake)
            .send(
                &thread_peer("oc_g", "om_root"),
                Outbound::Text { text: "在".into() },
            )
            .await
            .unwrap();
        let replies = fake.calls_to("reply");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["target"], "om_root");
        assert_eq!(replies[0]["reply_in_thread"], json!(true));
        assert_eq!(replies[0]["content"], json!({ "text": "在" }).to_string());
        assert!(fake.calls_to("messages").is_empty(), "不能发到群主流");
    }

    #[tokio::test]
    async fn a_text_to_a_peer_without_a_thread_still_creates_a_message() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        sender(&fake)
            .send(&peer("oc_1"), Outbound::Text { text: "在".into() })
            .await
            .unwrap();
        assert_eq!(fake.calls_to("messages").len(), 1);
        assert!(fake.calls_to("reply").is_empty());
    }

    #[tokio::test]
    async fn a_thread_result_without_a_card_falls_back_to_a_threaded_reply() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        sender(&fake)
            .send(&thread_peer("oc_g", "om_root"), run_finished("跑完了"))
            .await
            .unwrap();
        let replies = fake.calls_to("reply");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["target"], "om_root");
        assert_eq!(replies[0]["reply_in_thread"], json!(true));
        assert!(fake.calls_to("messages").is_empty());
    }

    fn run_absorbed() -> Outbound {
        Outbound::RunAbsorbed {
            session: komo_kernel::types::ids::SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            into: RunId::from_raw("run-0"),
        }
    }

    #[tokio::test]
    async fn an_absorbed_run_turns_its_card_into_a_grey_absorbed_card() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);
        let card_id = open_card(&fake, &sender).await;

        let outcome = sender.send(&peer("oc_1"), run_absorbed()).await.unwrap();

        assert_eq!(outcome, SendOutcome::Sent);
        assert_eq!(fake.patched_message_ids(), vec![card_id]);
        let card = patched_card(&fake, 0);
        assert_eq!(card["config"]["update_multi"], json!(true));
        assert_eq!(card["header"]["template"], json!("grey"));
        assert!(card.to_string().contains("已并入正在进行的回答"), "{card}");
        assert!(fake.calls_to("messages").is_empty(), "不该发文本刷屏");
    }

    #[tokio::test]
    async fn an_absorbed_run_without_a_card_sends_nothing() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let outcome = sender(&fake)
            .send(&peer("oc_1"), run_absorbed())
            .await
            .unwrap();
        assert_eq!(outcome, SendOutcome::Sent);
        assert!(fake.calls_to("patch").is_empty());
        assert!(fake.calls_to("messages").is_empty());
    }

    #[tokio::test]
    async fn a_pending_card_for_a_thread_peer_replies_in_thread() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);
        let target = thread_peer("oc_g", "om_root");
        let ticket = sender.expect_run_card(&RunId::from_raw("run-1"), &target);
        let card_id = sender
            .reply_run_card(ticket, "om_root", true)
            .await
            .unwrap();
        let replies = fake.calls_to("reply");
        assert_eq!(replies[0]["target"], "om_root");
        assert_eq!(replies[0]["reply_in_thread"], json!(true));

        sender.send(&target, run_finished("跑完了")).await.unwrap();
        assert_eq!(fake.patched_message_ids(), vec![card_id]);
        assert_eq!(fake.calls_to("reply").len(), 1, "结果换卡，不再回复");
    }

    #[tokio::test]
    async fn a_result_in_another_thread_does_not_touch_this_threads_card() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = sender(&fake);
        let ticket =
            sender.expect_run_card(&RunId::from_raw("run-1"), &thread_peer("oc_g", "om_a"));
        sender.reply_run_card(ticket, "om_a", true).await.unwrap();

        sender
            .send(&thread_peer("oc_g", "om_b"), run_finished("跑完了"))
            .await
            .unwrap();
        assert!(fake.calls_to("patch").is_empty());
    }

    #[tokio::test]
    async fn an_absorbed_run_that_beats_its_card_waits_and_still_closes_the_card() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let sender = Arc::new(sender(&fake));
        let ticket = sender.expect_run_card(&RunId::from_raw("run-1"), &peer("oc_1"));

        let absorbing = {
            let sender = Arc::clone(&sender);
            tokio::spawn(async move { sender.send(&peer("oc_1"), run_absorbed()).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!absorbing.is_finished(), "卡片还在路上时并入通知该等着");
        let card_id = sender
            .reply_run_card(ticket, "om_origin", false)
            .await
            .unwrap();
        absorbing.await.unwrap();

        assert_eq!(fake.patched_message_ids(), vec![card_id]);
        assert!(patched_card(&fake, 0).to_string().contains("已并入"));
        assert!(fake.calls_to("messages").is_empty());
    }

    #[tokio::test]
    async fn a_failed_absorbed_card_update_is_not_resent_as_text() {
        let fake = FakeOpenApi::start(Behavior::refuse_patch()).await;
        let sender = sender(&fake);
        open_card(&fake, &sender).await;

        sender
            .send(&peer("oc_1"), run_absorbed())
            .await
            .expect("收尾失败只记日志");
        assert_eq!(fake.calls_to("patch").len(), 1);
        assert!(fake.calls_to("messages").is_empty(), "不发文本刷屏");
    }
}
