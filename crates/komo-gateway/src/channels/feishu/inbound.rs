//! ws 事件负载 → [`InboundMessage`]（§11.1、§11.2）。
//!
//! 渠道在这一步只回答三个问题：**哪个会话**（`peer`）、**是不是私聊**、**谁说的**。
//! 哪个 Session、是不是操作者、这个群响不响应，全都是 Dispatcher 与配置快照的事——
//! 渠道手里的 [`ChannelConfig`](komo_kernel::protocol::config::ChannelConfig) 是构造那
//! 一刻的快照，拿它去做准入判定会在热重载之后悄悄按旧名单丢消息。
//!
//! 唯一留在这里的过滤是 **@提及**：它要知道机器人自己的 `open_id`（`bot/v3/info`），
//! Dispatcher 不认识平台，所以只能是渠道做。群里没 @机器人 的消息 `Ignored`，**不进
//! Dispatcher**。
//!
//! 解析一律宽容：`#[serde(default)]` 到底，认不出的事件类型返回 `None` 而不是错误。
//! 开放平台每月加字段，而一个多出来的键不应该让一条事件失败——但**缺 `event_id` 是另
//! 一回事**：没有去重键的事件宁可不处理，也不能拿一个编出来的键去骗过去重。

use serde::Deserialize;
use serde_json::Value;

use komo_kernel::protocol::InboundMessage;
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, PeerId};
use komo_kernel::types::ids::RequestKey;

use crate::render::feishu;

/// 去重键：`feishu:{event_id}`。
///
/// 消息事件与卡片回调**共用**这一个字段（`header.event_id`）：官方对 2.0 事件的去重
/// 建议就是"通过事件结构中的 `event_id` 字段判断事件唯一性"，而卡片回调的 header 里
/// 那个 `event_id` 的定义正是"回调的唯一标识"（§11.1、spike callbacks.md 1a/1d）。
pub fn request_key(event_id: &str) -> RequestKey {
    RequestKey::new(format!("feishu:{event_id}"))
}

/// 一条从 ws 上来的事件，已经认出了类型。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    /// `header.event_id`。去重键就是它。
    pub event_id: String,
    pub kind: EventKind,
}

/// komo 认识的两种事件。**别的类型一律不解析**——订阅里多出来的东西不该变成一条消息。
#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    /// `im.message.receive_v1`
    Message(MessageEvent),
    /// `card.action.trigger`：审批卡片上的按钮。
    CardAction(CardActionEvent),
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct MessageEvent {
    #[serde(default)]
    pub sender: Sender,
    #[serde(default)]
    pub message: Message,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Sender {
    #[serde(default)]
    pub sender_id: SenderId,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct SenderId {
    #[serde(default)]
    pub open_id: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Message {
    /// 这条消息自己的 id。加表情、回复卡片都指向它；不进 [`InboundMessage`]——那是三个
    /// 渠道共用的类型。
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub chat_id: String,
    /// `p2p` / `group`。
    #[serde(default)]
    pub chat_type: String,
    /// `text` / `image` / `post` / …——首版只处理 `text`。
    #[serde(default)]
    pub message_type: String,
    /// 一个 **JSON 字符串**，不是对象。
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub mentions: Vec<Mention>,
}

impl Message {
    /// 操作者的私聊全部落到同一个 home session——**哪个**会话是 Dispatcher 的事，
    /// 渠道只负责把这个布尔填对（§11.2）。
    pub fn is_private(&self) -> bool {
        self.chat_type == "p2p"
    }
}

/// 一次 @提及。`key` 是正文里的占位符（`@_user_1`），`id.open_id` 才是被提到的人。
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Mention {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub id: MentionId,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct MentionId {
    #[serde(default)]
    pub open_id: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct CardActionEvent {
    #[serde(default)]
    pub operator: Operator,
    #[serde(default)]
    pub action: CardAction,
    #[serde(default)]
    pub context: CardContext,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Operator {
    #[serde(default)]
    pub open_id: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct CardAction {
    /// 按钮上挂的那份 JSON。对象或字符串，两种都收（见 `render::feishu`）。
    #[serde(default)]
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct CardContext {
    #[serde(default)]
    pub open_chat_id: String,
    #[serde(default)]
    pub open_message_id: String,
}

/// 原始负载 → 一条认识的事件。认不出返回 `None`。
pub fn parse(payload: &[u8]) -> Option<ParsedEvent> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        header: Header,
        #[serde(default)]
        event: Value,
    }
    #[derive(Default, Deserialize)]
    struct Header {
        #[serde(default)]
        event_id: String,
        #[serde(default)]
        event_type: String,
    }

    let envelope: Envelope = serde_json::from_slice(payload)
        .map_err(|error| tracing::debug!(%error, "飞书：认不出的事件负载"))
        .ok()?;
    if envelope.header.event_id.trim().is_empty() {
        // 没有去重键就不处理：编一个出来只会让重推变成第二次执行（§11.1）。
        tracing::warn!(
            event_type = %envelope.header.event_type,
            "飞书：事件没有 event_id，跳过"
        );
        return None;
    }

    let kind = match envelope.header.event_type.as_str() {
        "im.message.receive_v1" => {
            EventKind::Message(decode(envelope.event, "im.message.receive_v1")?)
        }
        "card.action.trigger" => {
            EventKind::CardAction(decode(envelope.event, "card.action.trigger")?)
        }
        other => {
            tracing::debug!(event_type = other, "飞书：不处理这个事件类型");
            return None;
        }
    };
    Some(ParsedEvent {
        event_id: envelope.header.event_id,
        kind,
    })
}

fn decode<T: serde::de::DeserializeOwned>(event: Value, what: &str) -> Option<T> {
    serde_json::from_value(event)
        .map_err(|error| tracing::warn!(%error, event_type = what, "飞书：事件正文解不开"))
        .ok()
}

/// 一条普通消息。返回 `None` = 这条不进 Dispatcher。
pub fn from_message(
    event: &MessageEvent,
    bot_open_id: &str,
    request_key: RequestKey,
) -> Option<InboundMessage> {
    // 首版只处理文本；图片、语音、富文本一律 Ignored。
    if event.message.message_type != "text" {
        return None;
    }
    let sender = event.sender.sender_id.open_id.trim();
    let chat_id = event.message.chat_id.trim();
    if sender.is_empty() || chat_id.is_empty() {
        return None;
    }

    let raw = text_of(&event.message.content)?;
    let is_private = event.message.is_private();
    let text = if is_private {
        raw.trim().to_string()
    } else {
        // 群里只响应 @机器人 的消息并剥掉提及（§11.2）。
        strip_bot_mentions(&raw, &event.message.mentions, bot_open_id)?
    };
    if text.is_empty() {
        return None;
    }

    Some(InboundMessage {
        peer: peer_of(chat_id),
        is_private,
        sender: PeerId::new(sender),
        text,
        request_key,
    })
}

/// 一次按钮回调。
///
/// 负载只用来**定位**：它被映射回那条文本命令，批准与否仍由 Dispatcher 核对 Principal
/// 后决定（§11.3）。认不出的负载返回 `None`——不编一条命令出来。
///
/// `is_private` 不在载荷里（回调只给 `open_chat_id`），由调用方查出来后传进来。
pub fn from_card_action(
    event: &CardActionEvent,
    is_private: bool,
    request_key: RequestKey,
) -> Option<InboundMessage> {
    let text = feishu::command_for_action(&event.action.value)?;
    let sender = event.operator.open_id.trim();
    let chat_id = event.context.open_chat_id.trim();
    if sender.is_empty() || chat_id.is_empty() {
        return None;
    }
    Some(InboundMessage {
        peer: peer_of(chat_id),
        is_private,
        sender: PeerId::new(sender),
        text,
        request_key,
    })
}

pub fn peer_of(chat_id: &str) -> ChannelPeer {
    ChannelPeer::new(ChannelPlatform::Feishu, chat_id)
}

/// `message.content` 是一个 JSON 字符串；文本消息里只有 `text` 一个字段。
fn text_of(content: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct TextContent {
        #[serde(default)]
        text: String,
    }
    serde_json::from_str::<TextContent>(content)
        .ok()
        .map(|content| content.text)
}

/// 剥掉 `@机器人`。没提到机器人就返回 `None`。
///
/// 按 `mentions` 给的**占位符**剥，不按名字找：正文里写的是 `@_user_1` 而不是机器人的
/// 名字，而哪个占位符是谁只有 `mentions` 说得清——两个同名的人、改过名的机器人、名字里
/// 带空格的机器人，靠名字匹配全都会错。
///
/// 别人的占位符**原样留着**：把它换成名字是在替用户改写他说的话，而把它删掉会让"@张三
/// 那件事"变成"那件事"。
// TODO(decide: 别人的 `@_user_N` 占位符要不要替换成 `@名字`。留着 = 模型看到一个没有
// 意义的占位；替换 = 改写用户原话。文档没写，先留着。)
fn strip_bot_mentions(text: &str, mentions: &[Mention], bot_open_id: &str) -> Option<String> {
    if bot_open_id.is_empty() {
        // 认不出自己就认不出提及。**不**退化成"群里什么都回"。
        return None;
    }
    let keys: Vec<&str> = mentions
        .iter()
        .filter(|mention| mention.id.open_id == bot_open_id)
        .map(|mention| mention.key.as_str())
        .filter(|key| !key.is_empty())
        .collect();
    if keys.is_empty() {
        return None;
    }
    let mut out = text.to_string();
    for key in keys {
        // 连着后面那个空格一起吃掉，否则"@_user_1 看日志"会剩下一个头空格。
        out = out.replace(&format!("{key} "), "");
        out = out.replace(key, "");
    }
    Some(out.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::fake::{BOT_OPEN_ID, card_action_event, text_event};

    fn message_event(chat_id: &str, chat_type: &str, text: &str) -> MessageEvent {
        MessageEvent {
            sender: Sender {
                sender_id: SenderId {
                    open_id: "ou_op".into(),
                },
            },
            message: Message {
                message_id: "om_1".into(),
                chat_id: chat_id.into(),
                chat_type: chat_type.into(),
                message_type: "text".into(),
                content: serde_json::json!({ "text": text }).to_string(),
                mentions: Vec::new(),
            },
        }
    }

    fn mention(key: &str, open_id: &str) -> Mention {
        Mention {
            key: key.into(),
            id: MentionId {
                open_id: open_id.into(),
            },
            name: "komo".into(),
        }
    }

    #[test]
    fn a_message_event_parses_into_its_pieces() {
        let payload = text_event("evt-1", "oc_1", "p2p", "ou_op", "在吗", &[]);
        let parsed = parse(&payload).expect("认得出");
        assert_eq!(parsed.event_id, "evt-1");
        let EventKind::Message(event) = parsed.kind else {
            panic!("应当是消息事件");
        };
        assert!(event.message.is_private());
        assert_eq!(event.message.chat_id, "oc_1");
        assert_eq!(event.message.message_id, "om_evt-1");
    }

    #[test]
    fn an_unknown_field_does_not_break_the_parse() {
        let payload = serde_json::json!({
            "schema": "2.0",
            "header": {
                "event_id": "evt-9",
                "event_type": "im.message.receive_v1",
                "brand_new_field": { "x": 1 },
            },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_op", "union_id": "on_x" } },
                "message": {
                    "chat_id": "oc_1",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"嗨\"}",
                    "brand_new_field": 1,
                },
            },
        })
        .to_string()
        .into_bytes();
        let parsed = parse(&payload).expect("多一个键不该让事件失败");
        assert_eq!(parsed.event_id, "evt-9");
    }

    #[test]
    fn an_event_without_an_event_id_is_not_processed() {
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {},
        })
        .to_string()
        .into_bytes();
        assert!(parse(&payload).is_none(), "没有去重键就不处理");
    }

    #[test]
    fn an_unsubscribed_event_type_goes_nowhere() {
        let payload = serde_json::json!({
            "header": { "event_id": "evt-2", "event_type": "im.chat.member.bot.added_v1" },
            "event": {},
        })
        .to_string()
        .into_bytes();
        assert!(parse(&payload).is_none());
        assert!(parse(b"not json").is_none());
    }

    #[test]
    fn a_private_message_keeps_its_text_and_says_so() {
        let inbound = from_message(
            &message_event("oc_1", "p2p", "  在吗  "),
            BOT_OPEN_ID,
            request_key("evt-1"),
        )
        .expect("私聊总是进 Dispatcher");
        assert!(inbound.is_private);
        assert_eq!(inbound.text, "在吗");
        assert_eq!(inbound.peer.to_string(), "feishu:oc_1");
        assert_eq!(inbound.sender.as_str(), "ou_op");
        assert_eq!(inbound.request_key.as_str(), "feishu:evt-1");
    }

    // ③ 群里非 @机器人 的消息 Ignored、@机器人 的消息剥掉占位。
    #[test]
    fn a_group_message_needs_a_mention() {
        let plain = message_event("oc_g", "group", "大家早");
        assert!(
            from_message(&plain, BOT_OPEN_ID, request_key("evt-2")).is_none(),
            "群里没 @机器人 的消息不进 Dispatcher"
        );

        let mut mentioned = message_event("oc_g", "group", "@_user_1 看一下日志");
        mentioned.message.mentions = vec![mention("@_user_1", BOT_OPEN_ID)];
        let inbound = from_message(&mentioned, BOT_OPEN_ID, request_key("evt-3")).expect("@了就进");
        assert!(!inbound.is_private);
        assert_eq!(inbound.text, "看一下日志", "占位符要剥掉");
        assert_eq!(inbound.peer.to_string(), "feishu:oc_g");
    }

    #[test]
    fn a_mention_of_somebody_else_is_not_a_mention_of_us() {
        let mut event = message_event("oc_g", "group", "@_user_1 你看下");
        event.message.mentions = vec![mention("@_user_1", "ou_someone_else")];
        assert!(from_message(&event, BOT_OPEN_ID, request_key("evt-4")).is_none());
    }

    #[test]
    fn only_our_own_placeholder_is_stripped() {
        let mut event = message_event("oc_g", "group", "@_user_1 帮 @_user_2 看下");
        event.message.mentions = vec![
            mention("@_user_1", BOT_OPEN_ID),
            mention("@_user_2", "ou_other"),
        ];
        let inbound = from_message(&event, BOT_OPEN_ID, request_key("evt-5")).expect("@了就进");
        assert_eq!(inbound.text, "帮 @_user_2 看下");
    }

    #[test]
    fn a_mention_is_stripped_wherever_it_sits() {
        let mut event = message_event("oc_g", "group", "看一下日志 @_user_1");
        event.message.mentions = vec![mention("@_user_1", BOT_OPEN_ID)];
        let inbound = from_message(&event, BOT_OPEN_ID, request_key("evt-6")).expect("@了就进");
        assert_eq!(inbound.text, "看一下日志");
    }

    #[test]
    fn only_mentioning_the_bot_says_nothing_and_goes_nowhere() {
        let mut event = message_event("oc_g", "group", "@_user_1");
        event.message.mentions = vec![mention("@_user_1", BOT_OPEN_ID)];
        assert!(from_message(&event, BOT_OPEN_ID, request_key("evt-7")).is_none());
    }

    #[test]
    fn a_bot_that_does_not_know_its_own_id_answers_nothing_in_a_group() {
        let mut event = message_event("oc_g", "group", "@_user_1 嗨");
        event.message.mentions = vec![mention("@_user_1", BOT_OPEN_ID)];
        assert!(from_message(&event, "", request_key("evt-8")).is_none());
        // 私聊不需要提及，所以照常进。
        assert!(
            from_message(
                &message_event("oc_1", "p2p", "嗨"),
                "",
                request_key("evt-9")
            )
            .is_some()
        );
    }

    #[test]
    fn a_non_text_message_is_ignored() {
        let mut event = message_event("oc_1", "p2p", "");
        event.message.message_type = "image".into();
        assert!(from_message(&event, BOT_OPEN_ID, request_key("evt-10")).is_none());
    }

    #[test]
    fn a_message_missing_its_chat_or_sender_goes_nowhere() {
        let mut no_chat = message_event("", "p2p", "嗨");
        no_chat.message.chat_id = String::new();
        assert!(from_message(&no_chat, BOT_OPEN_ID, request_key("evt-11")).is_none());

        let mut no_sender = message_event("oc_1", "p2p", "嗨");
        no_sender.sender.sender_id.open_id = String::new();
        assert!(from_message(&no_sender, BOT_OPEN_ID, request_key("evt-12")).is_none());

        let mut bad_content = message_event("oc_1", "p2p", "嗨");
        bad_content.message.content = "not json".into();
        assert!(from_message(&bad_content, BOT_OPEN_ID, request_key("evt-13")).is_none());
    }

    // ② card.action.trigger 转成 /approve <id>。
    #[test]
    fn a_button_press_becomes_the_same_command_a_message_would() {
        let payload = card_action_event("evt-20", "oc_1", "ou_op", "approve", "7K2M");
        let parsed = parse(&payload).expect("认得出");
        assert_eq!(parsed.event_id, "evt-20");
        let EventKind::CardAction(event) = parsed.kind else {
            panic!("应当是卡片回调");
        };
        let inbound = from_card_action(&event, true, request_key(&parsed.event_id)).expect("回调");
        assert_eq!(inbound.text, "/approve 7K2M");
        assert_eq!(inbound.request_key.as_str(), "feishu:evt-20");
        assert_eq!(inbound.peer.to_string(), "feishu:oc_1");
        assert_eq!(inbound.sender.as_str(), "ou_op");
        assert!(inbound.is_private);
    }

    #[test]
    fn a_reject_button_becomes_the_reject_command() {
        let payload = card_action_event("evt-21", "oc_g", "ou_op", "reject", "7K2M");
        let EventKind::CardAction(event) = parse(&payload).unwrap().kind else {
            panic!()
        };
        let inbound = from_card_action(&event, false, request_key("evt-21")).unwrap();
        assert_eq!(inbound.text, "/reject 7K2M");
        assert!(!inbound.is_private);
    }

    #[test]
    fn a_callback_with_an_unknown_payload_or_no_chat_goes_nowhere() {
        let mut event = CardActionEvent {
            operator: Operator {
                open_id: "ou_op".into(),
            },
            action: CardAction {
                value: serde_json::json!({ "action": "drop", "short_id": "7K2M" }),
            },
            context: CardContext {
                open_chat_id: "oc_1".into(),
                open_message_id: "om_1".into(),
            },
        };
        assert!(from_card_action(&event, true, request_key("evt-22")).is_none());

        event.action.value = serde_json::json!({ "action": "approve", "short_id": "7K2M" });
        event.context.open_chat_id = String::new();
        assert!(from_card_action(&event, true, request_key("evt-23")).is_none());
    }
}
