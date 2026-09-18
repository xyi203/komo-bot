//! 协议线格式：HTTP 接口、SSE、渠道入站、配置快照。
//!
//! 聊天渠道**不经 HTTP**：它们在 Gateway 进程内通过 `Inbound` / `Notifier` 调用与这些
//! 接口相同的函数，Dispatcher 是两边共用的入口（§13.1）。所以 [`InboundMessage`] 和
//! HTTP 的请求体住在同一个模块——它们是同一件事的两种到达方式。

pub mod config;
pub mod http;
pub mod sse;

use serde::{Deserialize, Serialize};

pub use config::{
    ChannelConfig, ChannelsConfig, ConfigIssue, ConfigSnapshot, IssueSeverity, KeyPath,
    MemoryConfig, PathsConfig, RetrievalConfig, SourceFile, StartOnly,
};
pub use http::*;
pub use sse::{Cursor, SseEvent, SseFrame};

use crate::types::chat::{ChannelPeer, PeerId};
use crate::types::ids::{RequestKey, RunId, SessionId};

/// 协议版本。`GET /healthz` 回它；对不上就不是同一个 komo。
pub const PROTOCOL_VERSION: u32 = 1;

/// 渠道交给 `Dispatcher::handle` 的东西（§11.1）。
///
/// **不是 Session ID**：哪个会话属于 Dispatcher 与存储。渠道只知道"某个平台上的某个
/// 会话里，某个人说了一句话"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundMessage {
    /// 消息来自哪个会话。
    pub peer: ChannelPeer,
    /// 是不是私聊。操作者的私聊全部落到同一个 home session（§11.2）。
    pub is_private: bool,
    /// 发送者在该平台的 id。判定 Principal 用它。
    pub sender: PeerId,
    /// 正文。群里已经剥掉 @机器人 的提及。
    pub text: String,
    /// 去重键：`feishu:{event_id}` / `telegram:{update_id}` / `wechat:{from}:{msg_id}`。
    /// **命令也去重**——重发的 `/approve` 不会批准两次（§11.1）。
    pub request_key: RequestKey,
}

/// Dispatcher 的回执。渠道拿到它之后才 ack，不是收到就 ack（§11.1）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InboundAck {
    /// 平台重投，这条已经处理过了。
    Duplicate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<RunId>,
    },
    /// 命令已经处理完，把这段文本回给发送者。
    Replied { text: String },
    /// 普通文本已经排队。
    Queued { session: SessionId, run: RunId },
    /// 发送者不在 `allow_from` 里。消息不进入 Run，也不留任何记录；回执里带上他在
    /// 这个平台的 id，操作者抄进 `allow_from` 即可（§11.2）。
    Rejected { hint: String },
    /// 什么都不做（例如群里一条没有 @机器人 的消息）。
    Ignored,
}

/// 一条 `/approve` / `/reject` 指的是哪些待处理请求。
///
/// 三态而不是 `Option<ShortId>`：`/approve`（不带 ID）与 `/approve all` 是两件**不同**
/// 的事——前者说的是"只有一条时就是它"，后者是操作者明确说的"全都答了"。把它俩合成
/// 一个 `None`，就会让"我以为只有一条"变成一个批量决定。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalTarget {
    /// `/approve 7K2M`：就这一条。
    One(crate::types::ids::ShortId),
    /// `/approve`：待处理**恰好一条**时的那一条；多于一条就列出来要求指明。
    #[default]
    Only,
    /// `/approve all`：此刻待处理的全部，各按**本次调用**答（§7.2 的范围授权绑单份计划，
    /// 批量不替操作者猜一个范围）。
    All,
}

/// 三个渠道都认的聊天命令（§11.3）。解析在 Dispatcher，渲染在渠道。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ChatCommand {
    /// `/approve [short_id|all] [run]`。
    Approve {
        #[serde(default)]
        target: ApprovalTarget,
        #[serde(default)]
        scope: crate::types::chat::ApprovalScope,
    },
    /// `/reject [short_id|all]`。
    Reject {
        #[serde(default)]
        target: ApprovalTarget,
    },
    /// `/pending`
    Pending,
    /// `/new`：当前 Session 追加 `conversation.boundary`，不切 Session。
    New,
    /// `/cancel`：取消该 Session 当前 Run。
    Cancel,
    /// `/status`
    Status,
    /// `/id`：回显 `{platform}:{chat_id}` 与发送者 id。**唯一不要求操作者身份的命令。**
    Id,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::chat::ChannelPlatform;

    #[test]
    fn an_inbound_message_carries_a_peer_not_a_session() {
        let message = InboundMessage {
            peer: ChannelPeer::new(ChannelPlatform::Telegram, "123456789"),
            is_private: true,
            sender: PeerId::new("123456789"),
            text: "/approve 7K2M".into(),
            request_key: RequestKey::new("telegram:42"),
        };
        let text = serde_json::to_string(&message).unwrap();
        assert!(!text.contains("session"), "{text}");
        assert_eq!(
            serde_json::from_str::<InboundMessage>(&text).unwrap(),
            message
        );
    }

    #[test]
    fn an_ack_round_trips_by_kind() {
        let ack = InboundAck::Duplicate {
            run: Some(RunId::from_raw("run-1")),
        };
        let text = serde_json::to_string(&ack).unwrap();
        assert!(text.contains(r#""kind":"duplicate""#), "{text}");
        assert_eq!(serde_json::from_str::<InboundAck>(&text).unwrap(), ack);
    }

    #[test]
    fn approve_without_a_scope_means_this_call_only() {
        let command: ChatCommand =
            serde_json::from_str(r#"{"command":"approve","target":{"One":"7K2M"}}"#).unwrap();
        assert_eq!(
            command,
            ChatCommand::Approve {
                target: ApprovalTarget::One(
                    crate::types::ids::ShortId::parse("7K2M").expect("短 ID")
                ),
                scope: crate::types::chat::ApprovalScope::Once,
            }
        );
    }

    /// `Approve` 不带 `target` 时是 `Only`（"恰好一条时就是它"），**不是** `All`。
    ///
    /// 这两个的默认值反了的话，一条字段缺失的旧消息会把所有待处理审批一起答掉。
    #[test]
    fn a_missing_target_means_exactly_one_not_every_one() {
        let command: ChatCommand = serde_json::from_str(r#"{"command":"approve"}"#).unwrap();
        assert_eq!(
            command,
            ChatCommand::Approve {
                target: ApprovalTarget::Only,
                scope: crate::types::chat::ApprovalScope::Once,
            }
        );
        let reject: ChatCommand = serde_json::from_str(r#"{"command":"reject"}"#).unwrap();
        assert_eq!(
            reject,
            ChatCommand::Reject {
                target: ApprovalTarget::Only
            }
        );
    }
}
