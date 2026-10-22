//! `WireMessage` → [`InboundMessage`]（§11.1、§11.2）。
//!
//! 渠道在这一步只回答三个问题：**哪个会话**（`peer`）、**是不是私聊**、**谁说的**。
//! 哪个 Session、是不是操作者，全都是 Dispatcher 与配置快照的事——渠道手里的
//! [`ChannelConfig`](komo_kernel::protocol::config::ChannelConfig) 是构造那一刻的快照，
//! 拿它去做准入判定会在热重载之后悄悄按旧名单丢消息。
//!
//! 微信这一列比另外两个简单，因为它**只有 DM**（§11.2）：没有群、没有 @提及要剥，
//! `is_private` 恒为 `true`，会话 id 就是对方的 `from_user_id`。留下的只有两件事：
//!
//! 1. **只处理文本与图片**。图片由渠道先存成本地文件（[`crate::channels::inbox`]），
//!    正文里只写路径；语音 / 文件 / 视频一律 `None`（不进 Dispatcher）。
//! 2. **去重键**。`wechat:{from_user_id}:{client_id}`，`client_id` 为空时回退到
//!    `(from, 内容哈希, 60s 窗口)`——两者都住在 [`crate::render::wechat`] 里，因为它们
//!    是纯函数，要能在没有 SDK 的情况下逐字断言。

use komo_kernel::protocol::InboundMessage;
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, PeerId};
use komo_kernel::types::ids::RequestKey;
use wechatbot::types::{ContentType, ImageItem, IncomingMessage, MessageType, WireMessage};

use crate::channels::inbox::{self, SavedImage};
use crate::render::wechat;

/// 这条 wire 消息的去重键（§11.1）。
///
/// **在 `from_wire` 之外单独给出**，因为它要先于"这条消息我们处不处理"算出来：一条
/// 图片消息也可能被重投，而重投的判定不该取决于内容类型。
pub fn request_key(wire: &WireMessage) -> RequestKey {
    if wire.client_id.trim().is_empty() {
        // 回退：`client_id` 是**发送方客户端**生成的 UUID，类型是 `String` 而不是
        // `Option<String>`，所以"空串"是一种必须处理的情形而不是一种异常。
        RequestKey::new(wechat::fallback_request_key(
            &wire.from_user_id,
            &text_of(wire),
            wire.create_time_ms,
        ))
    } else {
        RequestKey::new(wechat::primary_request_key(
            &wire.from_user_id,
            wire.client_id.trim(),
        ))
    }
}

/// 这条消息里要下载的图片。机器人自己的回流不算——那条不会进 Dispatcher，下载它是白费。
pub fn images_of(wire: &WireMessage) -> Vec<&ImageItem> {
    if wire.message_type != MessageType::User {
        return Vec::new();
    }
    wire.item_list
        .iter()
        .filter_map(|item| item.image_item.as_ref())
        .collect()
}

/// 一条 wire 消息。返回 `None` = 这条不进 Dispatcher。
///
/// `images` 是渠道已经替 [`images_of`] 下载过的结果；没接收件箱时它是空的，一条只有图片的
/// 消息于是照旧不进 Dispatcher。
///
/// 解析走 SDK 的 [`IncomingMessage::from_wire`]——它是 crate 文档里给"自己驱动
/// `get_updates` 的调用方"留的稳定入口，也是唯一一处知道 `message_type == Bot` 的消息
/// （机器人自己发出去的那条回流）要被滤掉的地方。
pub fn from_wire(
    wire: &WireMessage,
    request_key: RequestKey,
    images: &[SavedImage],
) -> Option<InboundMessage> {
    let incoming = IncomingMessage::from_wire(wire)?;
    if !matches!(
        incoming.content_type,
        ContentType::Text | ContentType::Image
    ) {
        return None;
    }
    // 不用 `incoming.text`：SDK 把图片也拼进去了（`[image]` 或它的 URL）。
    let text = inbox::compose_text(images, &text_of(wire));
    if text.is_empty() {
        return None;
    }
    if incoming.user_id.is_empty() {
        // 没有发送者就没有 Principal 可判，也没有地方回执。
        return None;
    }
    Some(InboundMessage {
        peer: peer_of(&incoming.user_id),
        // 微信只有 DM（§11.2）。这不是一个默认值，是这个平台的全部形态。
        is_private: true,
        sender: PeerId::new(incoming.user_id.clone()),
        text,
        request_key,
    })
}

/// 微信的"会话"就是对方那个人（DM only）。
pub fn peer_of(user_id: &str) -> ChannelPeer {
    ChannelPeer::new(ChannelPlatform::Wechat, user_id)
}

/// 这条 wire 消息的正文，**不管它是不是用户发的**——回退去重键要在过滤之前算。
fn text_of(wire: &WireMessage) -> String {
    wire.item_list
        .iter()
        .filter_map(|item| item.text_item.as_ref().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use super::super::fake::{image_wire, text_wire};
    use wechatbot::types::{MessageItemType, MessageType, TextItem, WireMessageItem};

    #[test]
    fn a_dm_keeps_its_text_and_says_it_is_private() {
        let wire = text_wire("wxid_op", "cid-1", "  在吗  ");
        let inbound = from_wire(&wire, request_key(&wire), &[]).expect("文本消息进 Dispatcher");
        assert!(inbound.is_private, "微信只有 DM（§11.2）");
        assert_eq!(inbound.text, "在吗");
        assert_eq!(inbound.peer.to_string(), "wechat:wxid_op");
        assert_eq!(inbound.sender.as_str(), "wxid_op");
        assert_eq!(inbound.request_key.as_str(), "wechat:wxid_op:cid-1");
    }

    #[test]
    fn an_image_nobody_downloaded_is_ignored() {
        let wire = image_wire("wxid_op", "cid-2");
        assert_eq!(images_of(&wire).len(), 1);
        assert!(from_wire(&wire, request_key(&wire), &[]).is_none());
    }

    #[test]
    fn a_saved_image_becomes_a_path_in_the_text() {
        let wire = image_wire("wxid_op", "cid-2");
        let saved = [Ok(PathBuf::from("/w/inbox/wechat/ab.jpg"))];
        let inbound = from_wire(&wire, request_key(&wire), &saved).expect("图片进 Dispatcher");
        assert_eq!(
            inbound.text,
            "[用户发来一张图片，已存到 /w/inbox/wechat/ab.jpg，用 read 查看]"
        );
        assert!(
            !inbound.text.contains("img.example"),
            "SDK 拼的 URL 不进正文"
        );
    }

    #[test]
    fn a_voice_message_is_still_ignored() {
        let mut wire = image_wire("wxid_op", "cid-5");
        wire.item_list[0].item_type = MessageItemType::Voice;
        wire.item_list[0].image_item = None;
        assert!(images_of(&wire).is_empty());
        assert!(from_wire(&wire, request_key(&wire), &[]).is_none());
    }

    #[test]
    fn a_message_the_bot_sent_itself_is_ignored() {
        let mut wire = text_wire("wxid_op", "cid-3", "回声");
        wire.message_type = MessageType::Bot;
        assert!(
            from_wire(&wire, request_key(&wire), &[]).is_none(),
            "机器人自己的消息回流不是入站消息"
        );

        let mut echo = image_wire("wxid_op", "cid-3b");
        echo.message_type = MessageType::Bot;
        assert!(images_of(&echo).is_empty(), "回流里的图片不下载");
    }

    #[test]
    fn an_empty_body_is_ignored() {
        let wire = text_wire("wxid_op", "cid-4", "   ");
        assert!(from_wire(&wire, request_key(&wire), &[]).is_none());
    }

    // ② `client_id` 为空 → 回退键。
    #[test]
    fn an_empty_client_id_falls_back_to_the_content_hash_key() {
        let mut wire = text_wire("wxid_op", "", "在吗");
        wire.create_time_ms = 29_333_333 * 60_000;
        let key = request_key(&wire);
        assert_eq!(
            key.as_str(),
            wechat::fallback_request_key("wxid_op", "在吗", wire.create_time_ms)
        );
        assert!(key.as_str().starts_with("wechat:wxid_op:"), "{key}");

        // 同一分钟内的同一句话是同一个键；下一分钟不是。
        let mut later = wire.clone();
        later.create_time_ms += 59_999;
        assert_eq!(request_key(&later), key);
        let mut next_minute = wire.clone();
        next_minute.create_time_ms += 60_000;
        assert_ne!(request_key(&next_minute), key);
    }

    #[test]
    fn a_whitespace_only_client_id_counts_as_empty() {
        let wire = text_wire("wxid_op", "   ", "在吗");
        assert!(
            !request_key(&wire).as_str().ends_with("   "),
            "空白的 client_id 不是标识"
        );
    }

    #[test]
    fn the_fallback_key_reads_a_multi_item_body() {
        let mut wire = text_wire("wxid_op", "", "第一段");
        wire.item_list.push(WireMessageItem {
            item_type: MessageItemType::Text,
            text_item: Some(TextItem {
                text: "第二段".into(),
            }),
            image_item: None,
            voice_item: None,
            file_item: None,
            video_item: None,
            ref_msg: None,
        });
        assert_eq!(text_of(&wire), "第一段\n第二段");
    }
}
