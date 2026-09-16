//! 微信的出站一侧：`Outbound` → 一条已发出的纯文本（§11.3、§11.4）。
//!
//! 这是三个渠道里唯一会答 [`SendOutcome::Deferred`] 的一个，而 `Deferred` 的触发条件
//! 是一件**结构性**的事，不是一次失败：
//!
//! iLink 的每条入站消息自带一个 `context_token`，回推必须带着它。令牌只活在进程内存
//! 里（spike §2.6：`WeChatBot` 的 `context_tokens: RwLock<HashMap<..>>`，源码里没有任何
//! TTL），所以进程一重启就全没了。没有令牌时 SDK 的 `send` 直接返回
//! [`WeChatBotError::NoContext`]，**不会联网去补**——这就是 §11.4 说的"进程启动后用户
//! 没发过消息时无法主动推送"。
//!
//! 因此这里有两条硬规矩：
//!
//! 1. **按错误变体匹配，不按字符串**（§11.4 的原话）。`NoContext` 是一个变体；
//!    `is_session_expired()` 是 SDK 自己给的谓词。两者都不经 `to_string()`。
//! 2. **`Deferred` 不联网补**。它把投递记录留在 `pending`，由下一条入站消息触发冲刷
//!    （§11.1 第 4 步）——那是 Dispatcher 的事，这里只要如实答"此刻推不出去"。
//!
//! `ApprovalSettled` 在这里是**补发一行**结论文本，不是原地更新：微信没有可编辑的卡片
//! （§11.3）。旧消息上也没有按钮可点，所以"决定过的请求还长着可点的按钮"这个问题在这
//! 个渠道不存在。

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::traits::DeliverError;
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, Outbound, PeerId};
use wechatbot::error::WeChatBotError;
use wechatbot::protocol::{ILinkClient, build_text_message};
use wechatbot::types::{Credentials, WireMessage};

use crate::channels::{ChannelSender, SendOutcome};
use crate::render::wechat;

/// 进程内最多记多少个人的回复令牌。
///
/// 微信是 DM-only 且只有操作者说得上话（§11.2），所以这张表实际只有个位数条目；上限
/// 存在只是为了让一个被陌生人刷消息的机器人不会无限长大。
const CONTEXT_CAP: usize = 256;

/// `user_id` → `context_token`，进程内。
///
/// **这是缓存，不是权威**：它没有磁盘、没有过期，重启即空。唯一的清空路径是会话过期
/// （`errcode -14`），与 SDK 一致。
#[derive(Debug, Default)]
pub struct ContextTokens {
    inner: Mutex<Tokens>,
}

#[derive(Debug, Default)]
struct Tokens {
    by_user: HashMap<String, String>,
    order: VecDeque<String>,
}

impl ContextTokens {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记下一条入站消息带来的令牌。空的 `user_id` / 令牌不记——记一个空串会让后面的
    /// `get` 答 `Some("")`，那比没有更糟。
    pub fn remember(&self, user_id: &str, token: &str) {
        if user_id.is_empty() || token.is_empty() {
            return;
        }
        let mut tokens = self.inner.lock().expect("回复令牌表");
        if tokens
            .by_user
            .insert(user_id.to_string(), token.to_string())
            .is_none()
        {
            tokens.order.push_back(user_id.to_string());
            while tokens.order.len() > CONTEXT_CAP {
                if let Some(oldest) = tokens.order.pop_front() {
                    tokens.by_user.remove(&oldest);
                }
            }
        }
    }

    /// 从一条 wire 消息里记令牌。
    ///
    /// 与 SDK 的 `remember_context` 同构，包括**方向**：用户发来的消息记
    /// `from_user_id`，机器人自己的回流记 `to_user_id`——后者也能刷新令牌。
    pub fn remember_wire(&self, wire: &WireMessage) {
        let user_id = if wire.message_type == wechatbot::types::MessageType::User {
            &wire.from_user_id
        } else {
            &wire.to_user_id
        };
        self.remember(user_id, &wire.context_token);
    }

    pub fn get(&self, user_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("回复令牌表")
            .by_user
            .get(user_id)
            .cloned()
    }

    /// 会话过期（`errcode -14`）时清空，与 SDK 一致。
    pub fn clear(&self) {
        let mut tokens = self.inner.lock().expect("回复令牌表");
        tokens.by_user.clear();
        tokens.order.clear();
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("回复令牌表").by_user.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 凭证里与调用有关的那两样：打哪个 IDC、拿什么授权。
///
/// **不实现 `Debug`/`Display` 里带值的任何东西**——`token` 是凭证。
#[derive(Clone)]
pub struct Auth {
    pub base_url: String,
    pub token: String,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // base_url 也不印：它是登录时服务端指定的 IDC 主机，属于这台机器的凭证材料。
        f.debug_struct("Auth").finish_non_exhaustive()
    }
}

impl From<&Credentials> for Auth {
    fn from(credentials: &Credentials) -> Self {
        Auth {
            base_url: credentials.base_url.clone(),
            token: credentials.token.clone(),
        }
    }
}

pub struct WeChatSender {
    client: Arc<ILinkClient>,
    auth: Auth,
    contexts: Arc<ContextTokens>,
}

impl std::fmt::Debug for WeChatSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeChatSender")
            .field("known_contexts", &self.contexts.len())
            .finish_non_exhaustive()
    }
}

impl WeChatSender {
    pub fn new(client: Arc<ILinkClient>, auth: Auth, contexts: Arc<ContextTokens>) -> Self {
        WeChatSender {
            client,
            auth,
            contexts,
        }
    }

    pub fn client(&self) -> &Arc<ILinkClient> {
        &self.client
    }

    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// 入站与出站共用的那张令牌表。
    pub fn contexts(&self) -> &Arc<ContextTokens> {
        &self.contexts
    }

    /// 一个 `Outbound` 送到一个会话。
    pub async fn deliver(
        &self,
        peer: &ChannelPeer,
        message: Outbound,
    ) -> Result<SendOutcome, WeChatBotError> {
        // `ApprovalSettled` 与别的没有分支：微信没有可编辑的卡片，结论就是再说一句话
        //（§11.3）。渲染层已经把它渲染成那一行。
        let text = wechat::render(&message);
        self.send_text(&peer.chat_id, &text).await?;
        Ok(SendOutcome::Sent)
    }

    /// 一段纯文本。**没有令牌就报 [`WeChatBotError::NoContext`]，不联网补**（§11.4）。
    pub async fn send_text(&self, user_id: &PeerId, text: &str) -> Result<(), WeChatBotError> {
        let user_id = user_id.as_str();
        let context_token = self
            .contexts
            .get(user_id)
            .ok_or_else(|| WeChatBotError::NoContext(user_id.to_string()))?;
        let payload = build_text_message(user_id, &context_token, text);
        self.client
            .send_message(&self.auth.base_url, &self.auth.token, &payload)
            .await
    }
}

/// 微信是三个渠道里唯一会答 `Deferred` 的一个（§11.4）。
#[async_trait]
impl ChannelSender for WeChatSender {
    fn platform(&self) -> ChannelPlatform {
        ChannelPlatform::Wechat
    }

    async fn send(&self, peer: &ChannelPeer, msg: Outbound) -> Result<SendOutcome, DeliverError> {
        match self.deliver(peer, msg).await {
            Ok(outcome) => Ok(outcome),
            // **按变体匹配**（§11.4）：`NoContext` 不是失败，是"此刻推不出去"。
            Err(WeChatBotError::NoContext(user)) => Ok(SendOutcome::Deferred {
                reason: deferred_reason(&user),
            }),
            // 平台明确拒绝了这条消息；传输与解析失败是我们这边的事。
            Err(rejected @ WeChatBotError::Api { .. }) => {
                Err(DeliverError::Rejected(rejected.to_string()))
            }
            Err(other) => Err(DeliverError::Other(other.to_string())),
        }
    }
}

/// `Deferred` 的理由。写清楚"在等什么"，因为这条会进投递记录，操作者事后要看懂。
pub fn deferred_reason(user_id: &str) -> String {
    format!(
        "微信还没有 {user_id} 的回复令牌（进程启动后他还没说过话）：这条留在 pending，等他下一条消息到达时冲刷"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::fake::{FakeILink, presentation, text_wire};
    use komo_kernel::types::ids::{ApprovalId, ShortId};
    use time::OffsetDateTime;

    fn peer(user: &str) -> ChannelPeer {
        ChannelPeer::new(ChannelPlatform::Wechat, user)
    }

    // ③ 无令牌发送 → Deferred{reason}（按变体匹配）。
    #[tokio::test]
    async fn a_send_without_a_reply_token_is_deferred() {
        let fake = FakeILink::start(Default::default()).await;
        let sender = fake.sender();

        let outcome = sender
            .send(&peer("wxid_op"), Outbound::Text { text: "在".into() })
            .await
            .expect("Deferred 不是错误");
        match outcome {
            SendOutcome::Deferred { reason } => {
                assert!(reason.contains("wxid_op"), "{reason}");
                assert!(reason.contains("pending"), "{reason}");
            }
            other => panic!("没有令牌时必须 Deferred：{other:?}"),
        }
        assert!(
            fake.calls_to("/ilink/bot/sendmessage").is_empty(),
            "Deferred 不联网补（§11.4）"
        );
    }

    /// `NoContext` 这个**变体**就是触发条件，不是某段文案。
    #[tokio::test]
    async fn the_deferred_trigger_is_the_error_variant() {
        let error = WeChatBotError::NoContext("wxid_op".into());
        assert!(matches!(error, WeChatBotError::NoContext(_)));
        // 同一句话换成别的变体就不再是 Deferred——这正是"不按字符串"的意思。
        let lookalike = WeChatBotError::Other("No context_token for user wxid_op".into());
        assert!(!matches!(lookalike, WeChatBotError::NoContext(_)));
    }

    // ③（后半）有令牌 → Sent。
    #[tokio::test]
    async fn a_send_with_a_reply_token_goes_out() {
        let fake = FakeILink::start(Default::default()).await;
        let sender = fake.sender();
        sender
            .contexts()
            .remember_wire(&text_wire("wxid_op", "cid-1", "在吗"));

        let outcome = sender
            .send(
                &peer("wxid_op"),
                Outbound::Text {
                    text: "收到".into(),
                },
            )
            .await
            .expect("有令牌就发得出去");
        assert_eq!(outcome, SendOutcome::Sent);

        let sends = fake.calls_to("/ilink/bot/sendmessage");
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0]["msg"]["to_user_id"], "wxid_op");
        assert_eq!(sends[0]["msg"]["item_list"][0]["text_item"]["text"], "收到");
        assert_eq!(sender.platform(), ChannelPlatform::Wechat);
    }

    // ④ 审批请求纯文本含五项与两条命令提示（走到线上的那一份）。
    #[tokio::test]
    async fn an_approval_request_goes_out_as_plain_text() {
        let fake = FakeILink::start(Default::default()).await;
        let sender = fake.sender();
        sender
            .contexts()
            .remember_wire(&text_wire("wxid_op", "cid-1", "在吗"));

        sender
            .deliver(
                &peer("wxid_op"),
                Outbound::ApprovalRequest(Box::new(presentation())),
            )
            .await
            .expect("发得出去");

        let sends = fake.calls_to("/ilink/bot/sendmessage");
        assert_eq!(sends.len(), 1);
        let text = sends[0]["msg"]["item_list"][0]["text_item"]["text"]
            .as_str()
            .expect("纯文本");
        for item in ["7K2M", "动作", "改动", "原因", "范围"] {
            assert!(text.contains(item), "少了 {item}：{text}");
        }
        assert!(text.contains("/approve 7K2M"), "{text}");
        assert!(text.contains("/reject 7K2M"), "{text}");
    }

    // ④（后半）ApprovalSettled 补发一行结论文本。
    #[tokio::test]
    async fn a_decision_is_one_more_message_not_an_edit() {
        let fake = FakeILink::start(Default::default()).await;
        let sender = fake.sender();
        sender
            .contexts()
            .remember_wire(&text_wire("wxid_op", "cid-1", "在吗"));

        sender
            .deliver(
                &peer("wxid_op"),
                Outbound::ApprovalSettled {
                    approval: ApprovalId::from_raw("ap-1"),
                    short_id: ShortId::parse("7K2M").expect("短 ID"),
                    approved: true,
                    by: PeerId::new("wxid_op"),
                    at: OffsetDateTime::from_unix_timestamp(1_760_000_000).expect("时间"),
                },
            )
            .await
            .expect("发得出去");

        let sends = fake.calls_to("/ilink/bot/sendmessage");
        assert_eq!(sends.len(), 1, "微信没有可编辑的卡片，只能再说一句");
        let text = sends[0]["msg"]["item_list"][0]["text_item"]["text"]
            .as_str()
            .expect("纯文本");
        assert!(text.starts_with("已批准 · 7K2M · wxid_op · "), "{text}");
    }

    // ⑤ 超长正文截断（走到线上的那一份）。
    #[tokio::test]
    async fn an_oversized_body_is_truncated_on_the_wire() {
        let fake = FakeILink::start(Default::default()).await;
        let sender = fake.sender();
        sender
            .contexts()
            .remember_wire(&text_wire("wxid_op", "cid-1", "在吗"));

        sender
            .deliver(
                &peer("wxid_op"),
                Outbound::Text {
                    text: "长".repeat(wechat::MESSAGE_LIMIT * 3),
                },
            )
            .await
            .expect("发得出去");

        let sends = fake.calls_to("/ilink/bot/sendmessage");
        assert_eq!(sends.len(), 1, "微信这一列是截断，不是分段（§11.3）");
        let text = sends[0]["msg"]["item_list"][0]["text_item"]["text"]
            .as_str()
            .expect("纯文本");
        assert!(text.ends_with(wechat::TRUNCATION_NOTE), "{text}");
        assert!(text.contains("TUI"));
    }

    #[tokio::test]
    async fn a_platform_refusal_is_rejected_not_deferred() {
        let fake = FakeILink::start(super::super::fake::Behavior::refuse_sends()).await;
        let sender = fake.sender();
        sender
            .contexts()
            .remember_wire(&text_wire("wxid_op", "cid-1", "在吗"));

        let error = sender
            .send(&peer("wxid_op"), Outbound::Text { text: "在".into() })
            .await
            .expect_err("服务端拒绝了");
        assert!(matches!(error, DeliverError::Rejected(_)), "{error:?}");
    }

    #[test]
    fn the_token_table_forgets_nothing_it_was_never_told() {
        let contexts = ContextTokens::new();
        contexts.remember("", "ct");
        contexts.remember("wxid_op", "");
        assert!(contexts.is_empty(), "空串不是令牌");

        contexts.remember("wxid_op", "ct-1");
        assert_eq!(contexts.get("wxid_op").as_deref(), Some("ct-1"));
        contexts.remember("wxid_op", "ct-2");
        assert_eq!(contexts.get("wxid_op").as_deref(), Some("ct-2"), "后到的赢");
        assert_eq!(contexts.len(), 1, "同一个人只占一格");

        contexts.clear();
        assert!(contexts.get("wxid_op").is_none(), "-14 清空整张表");
    }

    #[test]
    fn the_token_table_has_a_ceiling() {
        let contexts = ContextTokens::new();
        for index in 0..CONTEXT_CAP + 10 {
            contexts.remember(&format!("wxid_{index}"), "ct");
        }
        assert_eq!(contexts.len(), CONTEXT_CAP);
        assert!(contexts.get("wxid_0").is_none(), "最早的被淘汰");
        assert!(contexts.get(&format!("wxid_{}", CONTEXT_CAP + 9)).is_some());
    }

    #[test]
    fn a_bot_echo_refreshes_the_token_for_its_recipient() {
        let contexts = ContextTokens::new();
        let mut wire = text_wire("wxid_op", "cid-1", "回声");
        wire.message_type = wechatbot::types::MessageType::Bot;
        wire.from_user_id = "bot".into();
        wire.to_user_id = "wxid_op".into();
        contexts.remember_wire(&wire);
        assert!(
            contexts.get("wxid_op").is_some(),
            "机器人自己的回流也能刷新令牌（spike §2.6）"
        );
    }

    #[test]
    fn auth_never_prints_its_token() {
        let auth = Auth {
            base_url: "https://idc.example".into(),
            token: "super-secret".into(),
        };
        let printed = format!("{auth:?}");
        assert!(!printed.contains("super-secret"), "{printed}");
        assert!(!printed.contains("idc.example"), "{printed}");
    }
}
