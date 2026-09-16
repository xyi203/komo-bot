//! `Update` → [`InboundMessage`]（§11.1、§11.2）。
//!
//! 渠道在这一步只回答三个问题：**哪个会话**（`peer`）、**是不是私聊**、**谁说的**。
//! 哪个 Session、是不是操作者、这个群响不响应，全都是 Dispatcher 与配置快照的事——
//! 渠道手里的 [`ChannelConfig`](komo_kernel::protocol::config::ChannelConfig) 是构造那
//! 一刻的快照，拿它去做准入判定会在热重载之后悄悄按旧名单丢消息。
//!
//! 唯一留在这里的过滤是 **@提及**：它要知道机器人自己的用户名（`getMe`），Dispatcher
//! 不认识平台，所以只能是渠道做。群里没 @机器人 的消息 `Ignored`，**不进 Dispatcher**。

use komo_kernel::protocol::InboundMessage;
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, PeerId};
use komo_kernel::types::ids::RequestKey;

use super::api::{BotIdentity, CallbackQuery, Message};
use crate::render::telegram;

/// 去重键：`telegram:{update_id}`。
///
/// 普通消息与 `callback_query` **共用**这一个键：`callback_query` 是 `Update` 的一个
/// 字段，不是比 `update_id` 更细的投递单位（§11.1、spike callbacks.md 4d）。
pub fn request_key(update_id: i64) -> RequestKey {
    RequestKey::new(format!("telegram:{update_id}"))
}

/// 一条普通消息。返回 `None` = 这条不进 Dispatcher。
pub fn from_message(
    message: &Message,
    bot: &BotIdentity,
    request_key: RequestKey,
) -> Option<InboundMessage> {
    // 频道帖子没有 `from`；非文本消息首版不处理。
    let from = message.from.as_ref()?;
    let raw = message.text.as_deref()?;
    let is_private = message.chat.is_private();
    let text = if is_private {
        raw.trim().to_string()
    } else {
        // 群里只响应 @机器人 的消息并剥掉提及（§11.2）。没有用户名就认不出提及。
        strip_bot_mention(raw, bot.username.as_deref()?)?
    };
    if text.is_empty() {
        return None;
    }
    Some(InboundMessage {
        peer: peer_of(message.chat.id),
        is_private,
        sender: PeerId::new(from.id.to_string()),
        text,
        request_key,
    })
}

/// 一次按钮回调。
///
/// 负载只用来**定位**：它被映射回那条文本命令，批准与否仍由 Dispatcher 核对 Principal
/// 后决定（§11.3）。认不出的负载返回 `None`——不编一条命令出来。
pub fn from_callback(callback: &CallbackQuery, request_key: RequestKey) -> Option<InboundMessage> {
    let text = telegram::command_for_callback(callback.data.as_deref()?)?;
    // inline 模式的回调没有 message，也就没有 chat。komo 从不用 inline 模式，所以这
    // 不是一种要兜的情形，而是一条"不该发生"——硬拼一个占位 chat 才是真的错。
    let message = callback.message.as_ref()?;
    Some(InboundMessage {
        peer: peer_of(message.chat.id),
        is_private: message.chat.is_private(),
        sender: PeerId::new(callback.from.id.to_string()),
        text,
        request_key,
    })
}

pub fn peer_of(chat_id: i64) -> ChannelPeer {
    ChannelPeer::new(ChannelPlatform::Telegram, chat_id.to_string())
}

/// 剥掉 `@机器人`。没提到机器人就返回 `None`。
///
/// 按文本找而不是按 `entities` 的偏移找：`entities` 的 offset 是 **UTF-16 码元**，一条
/// 中文消息里每算错一次就是切在半个字上；而 `@用户名` 在文本里是唯一的、大小写不敏感
/// 的 ASCII 串，直接找它不会有这个问题。`/approve@komo_bot 7K2M` 这种把用户名缀在命令
/// 后面的写法也一并覆盖到了。
pub fn strip_bot_mention(text: &str, username: &str) -> Option<String> {
    let handle = format!("@{}", username.trim_start_matches('@')).to_ascii_lowercase();
    if handle.len() <= 1 {
        return None;
    }
    // `to_ascii_lowercase` 只动 ASCII 字节，字节布局与原串逐字节对齐。
    let haystack = text.to_ascii_lowercase();

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut mentioned = false;
    while let Some(hit) = haystack[cursor..].find(&handle) {
        let start = cursor + hit;
        let end = start + handle.len();
        let boundary_before = match text[..start].chars().next_back() {
            None => true,
            Some(ch) if !is_handle_char(ch) => true,
            // `/approve@komo_bot`：命令后缀写法，Telegram 自己也把整串算作一个
            // `bot_command`，所以 `@` 紧跟在命令字母后面**是**一次提及。
            Some(_) => {
                let prefix = &text[..start];
                prefix.starts_with('/') && !prefix.chars().any(char::is_whitespace)
            }
        };
        let boundary_after = text[end..]
            .chars()
            .next()
            .is_none_or(|ch| !is_handle_char(ch));
        if boundary_before && boundary_after {
            out.push_str(&text[cursor..start]);
            mentioned = true;
        } else {
            // `@komo_bot2` 不是在叫我们。
            out.push_str(&text[cursor..end]);
        }
        cursor = end;
    }
    if !mentioned {
        return None;
    }
    out.push_str(&text[cursor..]);
    Some(out.trim().to_string())
}

fn is_handle_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::api::{Chat, User};
    use super::super::fake::BOT_USERNAME;

    fn bot() -> BotIdentity {
        BotIdentity {
            id: 42,
            is_bot: true,
            first_name: "komo".into(),
            username: Some(BOT_USERNAME.into()),
        }
    }

    fn message(chat_id: i64, kind: &str, text: &str) -> Message {
        Message {
            message_id: 1,
            chat: Chat {
                id: chat_id,
                kind: kind.into(),
            },
            from: Some(User {
                id: 777,
                is_bot: false,
                username: None,
            }),
            text: Some(text.into()),
        }
    }

    #[test]
    fn a_private_message_keeps_its_text_and_says_so() {
        let inbound = from_message(&message(777, "private", "  在吗  "), &bot(), request_key(5))
            .expect("私聊总是进 Dispatcher");
        assert!(inbound.is_private);
        assert_eq!(inbound.text, "在吗");
        assert_eq!(inbound.peer.to_string(), "telegram:777");
        assert_eq!(inbound.sender.as_str(), "777");
        assert_eq!(inbound.request_key.as_str(), "telegram:5");
    }

    // ③ 群里非 @机器人 的消息 Ignored、@机器人 的消息剥掉提及。
    #[test]
    fn a_group_message_needs_a_mention() {
        assert!(
            from_message(
                &message(-100, "supergroup", "大家早"),
                &bot(),
                request_key(6)
            )
            .is_none(),
            "群里没 @机器人 的消息不进 Dispatcher"
        );

        let mentioned = from_message(
            &message(
                -100,
                "supergroup",
                &format!("@{BOT_USERNAME} 帮我看一下日志"),
            ),
            &bot(),
            request_key(7),
        )
        .expect("@了就进");
        assert!(!mentioned.is_private);
        assert_eq!(mentioned.text, "帮我看一下日志");
        assert_eq!(mentioned.peer.to_string(), "telegram:-100");
    }

    #[test]
    fn a_mention_is_stripped_wherever_it_sits() {
        assert_eq!(
            strip_bot_mention(&format!("@{BOT_USERNAME} 你好"), BOT_USERNAME).as_deref(),
            Some("你好")
        );
        assert_eq!(
            strip_bot_mention(&format!("你好 @{BOT_USERNAME}"), BOT_USERNAME).as_deref(),
            Some("你好")
        );
        // 命令后缀写法。
        assert_eq!(
            strip_bot_mention(&format!("/approve@{BOT_USERNAME} 7K2M"), BOT_USERNAME).as_deref(),
            Some("/approve 7K2M")
        );
        // 大小写不敏感。
        assert_eq!(
            strip_bot_mention(
                &format!("@{} 嗨", BOT_USERNAME.to_uppercase()),
                BOT_USERNAME
            )
            .as_deref(),
            Some("嗨")
        );
        // 前缀相同的另一个机器人不算。
        assert_eq!(
            strip_bot_mention(&format!("@{BOT_USERNAME}2 嗨"), BOT_USERNAME),
            None
        );
        assert_eq!(strip_bot_mention("没提到谁", BOT_USERNAME), None);
        // 只 @ 了机器人、没说别的：空正文不进 Dispatcher。
        assert_eq!(
            strip_bot_mention(&format!("@{BOT_USERNAME}"), BOT_USERNAME).as_deref(),
            Some("")
        );
    }

    #[test]
    fn a_bot_without_a_username_cannot_recognize_a_mention() {
        let mut bot = bot();
        bot.username = None;
        assert!(from_message(&message(-100, "group", "@x 嗨"), &bot, request_key(8)).is_none());
        // 私聊不需要提及，所以照常进。
        assert!(from_message(&message(9, "private", "嗨"), &bot, request_key(9)).is_some());
    }

    #[test]
    fn a_non_text_message_is_ignored() {
        let mut message = message(9, "private", "");
        message.text = None;
        assert!(from_message(&message, &bot(), request_key(10)).is_none());
    }

    #[test]
    fn a_callback_becomes_the_text_command_it_stands_for() {
        let callback = CallbackQuery {
            id: "cb-1".into(),
            from: User {
                id: 777,
                is_bot: false,
                username: None,
            },
            message: Some(message(11, "private", "待审批")),
            data: Some("approve:7K2M".into()),
        };
        let inbound = from_callback(&callback, request_key(12)).expect("按钮回调");
        assert_eq!(inbound.text, "/approve 7K2M");
        assert_eq!(inbound.request_key.as_str(), "telegram:12");
        assert_eq!(inbound.peer.to_string(), "telegram:11");
        assert_eq!(inbound.sender.as_str(), "777");
    }

    #[test]
    fn a_callback_without_a_chat_or_with_an_unknown_payload_goes_nowhere() {
        let base = CallbackQuery {
            id: "cb-1".into(),
            from: User {
                id: 777,
                is_bot: false,
                username: None,
            },
            message: Some(message(11, "private", "待审批")),
            data: Some("approve:7K2M".into()),
        };

        let mut inline = base.clone();
        inline.message = None;
        assert!(from_callback(&inline, request_key(13)).is_none());

        let mut unknown = base.clone();
        unknown.data = Some("drop:everything".into());
        assert!(from_callback(&unknown, request_key(14)).is_none());

        let mut empty = base;
        empty.data = None;
        assert!(from_callback(&empty, request_key(15)).is_none());
    }
}
