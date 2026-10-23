//! 渠道侧的身份与投递类型（§11）。
//!
//! 谁是操作者、哪个会话是 home chat 全部在 config.toml 里（§11.2），所以这里只有
//! 值类型：渠道把平台的 id 包成 [`ChannelPeer`]，Dispatcher 对着当前配置快照判定
//! [`Principal`]。**渠道不知道 Session 是什么。**

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use super::ids::{DeliveryId, RunId, SessionId};

/// 三个聊天渠道，加上 TUI / CLI 走的 HTTP API。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelPlatform {
    Feishu,
    Telegram,
    Wechat,
    /// TUI / CLI 的 HTTP 入口，与聊天渠道走同一个 Dispatcher（§11.1）。
    Api,
}

impl ChannelPlatform {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelPlatform::Feishu => "feishu",
            ChannelPlatform::Telegram => "telegram",
            ChannelPlatform::Wechat => "wechat",
            ChannelPlatform::Api => "api",
        }
    }

    /// 微信只有 DM，且进程启动后用户没发过消息就无法主动推送（§11.4）。
    pub fn supports_unsolicited_push(self) -> bool {
        !matches!(self, ChannelPlatform::Wechat)
    }

    /// [`ChannelPlatform::as_str`] 的反过来：认不出的词是 `None`，不是别的渠道。
    ///
    /// 给 [`ChannelPeer::parse`] 用——`runs.peer` 存的就是 `{platform}:{chat_id}`
    /// （`Display` 那份），重启后要把它解回来（`docs/background-tasks.md`）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "feishu" => Some(ChannelPlatform::Feishu),
            "telegram" => Some(ChannelPlatform::Telegram),
            "wechat" => Some(ChannelPlatform::Wechat),
            "api" => Some(ChannelPlatform::Api),
            _ => None,
        }
    }
}

impl fmt::Display for ChannelPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 平台侧的一个 id：飞书的 `ou_xxx` / `oc_xxx`、Telegram 的整数 user / chat id、
/// 微信的 `wxid_xxx`。
///
/// config.toml 里 Telegram 的 id 写成整数（`allow_from = [123456789]`），飞书写成
/// 字符串——同一个字段要能收下两种写法，否则配置文件就得为平台分叉。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PeerId(String);

impl PeerId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PeerId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Signed(i64),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Text(s) => PeerId(s),
            Raw::Signed(n) => PeerId(n.to_string()),
        })
    }
}

/// 一个平台上的一个会话，群里还可以再落到某个话题。规范写法 `{platform}:{chat_id}`，
/// 有话题时 `{platform}:{chat_id}#{thread}`；`/id` 回显的就是这个串（§11.2）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChannelPeer {
    pub platform: ChannelPlatform,
    pub chat_id: PeerId,
    /// 群里的一个话题：话题根消息的 id（飞书 `om_*`）。`None` = 整个会话（私聊、没开话题
    /// 的渠道）。`runs.peer` 与 `sessions.origin`（`chat:{peer}`）存的都是 `Display` 串，
    /// 所以每个话题自然是自己的 Session。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<PeerId>,
}

impl ChannelPeer {
    pub fn new(platform: ChannelPlatform, chat_id: impl Into<String>) -> Self {
        Self {
            platform,
            chat_id: PeerId::new(chat_id),
            thread: None,
        }
    }

    /// 落到群里的某个话题（`thread` 是话题根消息 id）。
    pub fn in_thread(mut self, thread: impl Into<String>) -> Self {
        self.thread = Some(PeerId::new(thread));
        self
    }

    /// [`Display`](fmt::Display) 的反过来：把 `runs.peer` 里存的
    /// `"{platform}:{chat_id}"` / `"{platform}:{chat_id}#{thread}"` 解回一个
    /// [`ChannelPeer`]。重启后重挂交互 Run 的看客要用它——`peer` 存的从来就是这个
    /// `Display` 出来的串，这里只是原样切回去。
    ///
    /// 只切**第一个**冒号：`chat_id` 本身可能带冒号（不是已知渠道会这么写，但不排除），
    /// `platform` 那几个词都不带；其后再切**第一个** `#` 取话题。空的 `chat_id`、`#` 后
    /// 面是空的话题，都认不出来。
    pub fn parse(raw: &str) -> Option<Self> {
        let (platform, rest) = raw.split_once(':')?;
        let platform = ChannelPlatform::parse(platform)?;
        let (chat_id, thread) = match rest.split_once('#') {
            Some((chat_id, thread)) => (chat_id, Some(thread)),
            None => (rest, None),
        };
        if chat_id.is_empty() || thread == Some("") {
            return None;
        }
        let peer = ChannelPeer::new(platform, chat_id);
        Some(match thread {
            Some(thread) => peer.in_thread(thread),
            None => peer,
        })
    }
}

impl fmt::Display for ChannelPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.platform, self.chat_id)?;
        if let Some(thread) = &self.thread {
            write!(f, "#{thread}")?;
        }
        Ok(())
    }
}

/// 发送者是不是操作者（§11.2）。
///
/// 只有两种：在该渠道 `allow_from` 里的是操作者，其余一律不是——不是"待配对"，也
/// 不是"受限用户"，没有第三档，因为没有配对流程。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    /// 在 `allow_from` 里。**下指令的命令只接受它。**
    Operator {
        platform: ChannelPlatform,
        id: PeerId,
    },
    /// 不在名单里：消息不进入 Run，也不留任何记录；只有 `/id` 对它可用。
    Stranger {
        platform: ChannelPlatform,
        id: PeerId,
    },
}

impl Principal {
    pub fn is_operator(&self) -> bool {
        matches!(self, Principal::Operator { .. })
    }

    pub fn id(&self) -> &PeerId {
        match self {
            Principal::Operator { id, .. } | Principal::Stranger { id, .. } => id,
        }
    }

    pub fn platform(&self) -> ChannelPlatform {
        match self {
            Principal::Operator { platform, .. } | Principal::Stranger { platform, .. } => {
                *platform
            }
        }
    }
}

/// 主动投递的内容（§11.4）。
///
/// `deliveries.outbound` 里存的就是它的 JSON。**读回库里的行用 [`Outbound::from_stored`]**，
/// 不要直接 `serde_json::from_str`：2026-10-11 删掉审批之前写下的
/// `approval_request` / `approval_settled` 两类行还在老库里（D3），直接解码会报错，一条
/// 旧行就能卡死补发循环。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outbound {
    /// 一段纯文本：Run 的最终回复、命令的回执、被拒绝的提示。
    Text { text: String },
    /// Run 结束了。
    RunFinished {
        session: SessionId,
        run: RunId,
        summary: String,
    },
    /// 需要操作者判断（§8.6）。
    NeedsAttention {
        session: SessionId,
        run: RunId,
        reason: String,
    },
    /// 这条 Run 并进了正在跑的另一条（`into`，§6 插话）：回答由宿主给，这条自己没有终态
    /// 回复。只有带"处理中"回复卡片的渠道（飞书）要据此把卡片收尾；其余渠道视为已处理。
    RunAbsorbed {
        session: SessionId,
        run: RunId,
        into: RunId,
    },
}

impl Outbound {
    /// 已经不再产生、但老库里还可能读到的 `kind`（审批在 2026-10-11 删掉了）。
    pub const RETIRED_KINDS: [&'static str; 2] = ["approval_request", "approval_settled"];

    /// 解码一条落盘的投递正文。
    ///
    /// `Ok(None)` = 这是一条**已作废**的旧行（`kind` 在 [`Self::RETIRED_KINDS`] 里）：正文
    /// 不再有意义，调用方把它标成不再投递、跳过，不能当错误重试。真正坏掉的 JSON 仍然是
    /// `Err`。
    pub fn from_stored(json: &str) -> Result<Option<Outbound>, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        let retired = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| Self::RETIRED_KINDS.contains(&kind));
        if retired {
            return Ok(None);
        }
        serde_json::from_value(value).map(Some)
    }
}

/// 投递目标。home chat 的解析只看配置（§11.4），所以这里是一个已解析出来的会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryTarget {
    pub peer: ChannelPeer,
    /// 这条投递是不是"补送到 home chat 的那一份"（§11.4）。
    #[serde(default)]
    pub is_home: bool,
}

impl DeliveryTarget {
    pub fn to_peer(peer: ChannelPeer) -> Self {
        Self {
            peer,
            is_home: false,
        }
    }

    pub fn home(peer: ChannelPeer) -> Self {
        Self {
            peer,
            is_home: true,
        }
    }
}

/// `deliveries` 表的三个状态（§11.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// 行已写、还没送到。重启后补发，按 [`DeliveryId`] 幂等。
    Pending,
    Sent,
    /// 渠道此刻无法推送（微信没有回复令牌）。行留在 pending，由下一条入站消息触发
    /// 冲刷。
    Deferred,
}

/// `Notifier::deliver` 的返回。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub id: DeliveryId,
    pub state: DeliveryState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_renders_as_platform_colon_chat_id() {
        let peer = ChannelPeer::new(ChannelPlatform::Feishu, "oc_xxx");
        assert_eq!(peer.to_string(), "feishu:oc_xxx");
    }

    /// `parse` 是 `Display` 的反过来：`runs.peer` 存的就是这个串，重启后要能原样解回去
    /// （§8 Fix 1）。
    #[test]
    fn a_peer_round_trips_through_its_display_form() {
        for peer in [
            ChannelPeer::new(ChannelPlatform::Feishu, "oc_xxx"),
            ChannelPeer::new(ChannelPlatform::Telegram, "123456789"),
            ChannelPeer::new(ChannelPlatform::Wechat, "wxid_op"),
            ChannelPeer::new(ChannelPlatform::Api, "tui"),
        ] {
            assert_eq!(ChannelPeer::parse(&peer.to_string()), Some(peer));
        }
    }

    #[test]
    fn a_thread_peer_renders_with_a_hash_and_round_trips() {
        let peer = ChannelPeer::new(ChannelPlatform::Feishu, "oc_x").in_thread("om_root");
        assert_eq!(peer.to_string(), "feishu:oc_x#om_root");
        assert_eq!(ChannelPeer::parse("feishu:oc_x#om_root"), Some(peer));
    }

    #[test]
    fn an_old_peer_string_parses_without_a_thread() {
        let peer = ChannelPeer::parse("feishu:oc_x").unwrap();
        assert_eq!(peer.thread, None);
        assert_eq!(peer.chat_id.as_str(), "oc_x");
    }

    #[test]
    fn parse_rejects_an_empty_thread_after_the_hash() {
        assert_eq!(ChannelPeer::parse("feishu:oc_x#"), None);
        assert_eq!(ChannelPeer::parse("feishu:#om_root"), None);
    }

    #[test]
    fn a_peer_without_a_thread_serializes_without_the_key() {
        let peer = ChannelPeer::new(ChannelPlatform::Feishu, "oc_x");
        assert_eq!(
            serde_json::to_string(&peer).unwrap(),
            r#"{"platform":"feishu","chat_id":"oc_x"}"#
        );
        let back: ChannelPeer =
            serde_json::from_str(r#"{"platform":"feishu","chat_id":"oc_x"}"#).unwrap();
        assert_eq!(back.thread, None);

        let threaded = peer.in_thread("om_root");
        let json = serde_json::to_string(&threaded).unwrap();
        assert!(json.contains(r#""thread":"om_root""#));
        assert_eq!(
            serde_json::from_str::<ChannelPeer>(&json).unwrap(),
            threaded
        );
    }

    #[test]
    fn parse_rejects_an_unknown_platform_or_a_missing_chat_id() {
        assert_eq!(ChannelPeer::parse("carrier-pigeon:oc_xxx"), None);
        assert_eq!(ChannelPeer::parse("feishu:"), None);
        assert_eq!(ChannelPeer::parse("feishu"), None);
    }

    #[test]
    fn a_peer_id_reads_from_a_string_or_an_integer() {
        let ids: Vec<PeerId> = serde_json::from_str(r#"["ou_xxx", 123456789]"#).unwrap();
        assert_eq!(ids[0].as_str(), "ou_xxx");
        assert_eq!(ids[1].as_str(), "123456789");
    }

    #[test]
    fn only_the_operator_is_an_operator() {
        let operator = Principal::Operator {
            platform: ChannelPlatform::Telegram,
            id: PeerId::new("1"),
        };
        let stranger = Principal::Stranger {
            platform: ChannelPlatform::Telegram,
            id: PeerId::new("2"),
        };
        assert!(operator.is_operator());
        assert!(!stranger.is_operator());
    }

    #[test]
    fn an_old_approval_delivery_reads_back_as_retired() {
        // 删掉审批之前落盘的两类行：正文照旧，读回来是"已作废"，不是错误。
        let request = r#"{"kind":"approval_request","approval":"ap-1","short_id":"7K2M","plan_hash":"abc","plan":{},"reason":"shell","scopes":["once"]}"#;
        let settled = r#"{"kind":"approval_settled","approval":"ap-1","short_id":"7K2M","approved":true,"by":"ou_1","at":"2026-09-18T08:00:00Z"}"#;
        assert_eq!(Outbound::from_stored(request).unwrap(), None);
        assert_eq!(Outbound::from_stored(settled).unwrap(), None);

        let text = serde_json::to_string(&Outbound::Text { text: "hi".into() }).unwrap();
        assert_eq!(
            Outbound::from_stored(&text).unwrap(),
            Some(Outbound::Text { text: "hi".into() })
        );
        assert!(Outbound::from_stored(r#"{"kind":"carrier_pigeon"}"#).is_err());
        assert!(Outbound::from_stored("not json").is_err());
    }

    #[test]
    fn a_run_absorbed_delivery_round_trips_through_storage() {
        let absorbed = Outbound::RunAbsorbed {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("r-2"),
            into: RunId::from_raw("r-1"),
        };
        let json = serde_json::to_string(&absorbed).unwrap();
        assert!(json.contains(r#""kind":"run_absorbed""#), "{json}");
        assert_eq!(Outbound::from_stored(&json).unwrap(), Some(absorbed));
    }

    #[test]
    fn wechat_cannot_push_unsolicited() {
        assert!(!ChannelPlatform::Wechat.supports_unsolicited_push());
        assert!(ChannelPlatform::Feishu.supports_unsolicited_push());
        assert!(ChannelPlatform::Telegram.supports_unsolicited_push());
    }
}
