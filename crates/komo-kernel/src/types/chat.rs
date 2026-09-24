//! 渠道侧的身份与投递类型（§11）。
//!
//! 谁是操作者、哪个会话是 home chat 全部在 config.toml 里（§11.2），所以这里只有
//! 值类型：渠道把平台的 id 包成 [`ChannelPeer`]，Dispatcher 对着当前配置快照判定
//! [`Principal`]。**渠道不知道 Session 是什么。**

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;

use super::ids::{ApprovalId, DeliveryId, RunId, SessionId, ShortId};
use super::plan::{ExecutionPlan, PlanHash};

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
    /// （`Display` 那份），重启后要把它解回来（`docs/home-dispatcher.md` §8）。
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

/// 一个平台上的一个会话。`{platform}:{chat_id}` 是它的规范写法，`/id` 回显的就是
/// 这个串（§11.2）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChannelPeer {
    pub platform: ChannelPlatform,
    pub chat_id: PeerId,
}

impl ChannelPeer {
    pub fn new(platform: ChannelPlatform, chat_id: impl Into<String>) -> Self {
        Self {
            platform,
            chat_id: PeerId::new(chat_id),
        }
    }

    /// [`Display`](fmt::Display) 的反过来：把 `runs.peer` 里存的 `"{platform}:{chat_id}"`
    /// 解回一个 [`ChannelPeer`]。重启后重挂交互 Run 的看客要用它（`docs/home-dispatcher.md`
    /// §8 Fix 1）——`peer` 存的从来就是这个 `Display` 出来的串，这里只是原样切回去。
    ///
    /// 只切**第一个**冒号：`chat_id` 本身可能带冒号（不是已知渠道会这么写，但不排除），
    /// `platform` 那几个词都不带。空的 `chat_id` 认不出来。
    pub fn parse(raw: &str) -> Option<Self> {
        let (platform, chat_id) = raw.split_once(':')?;
        let platform = ChannelPlatform::parse(platform)?;
        if chat_id.is_empty() {
            return None;
        }
        Some(ChannelPeer::new(platform, chat_id))
    }
}

impl fmt::Display for ChannelPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.platform, self.chat_id)
    }
}

/// 发送者是不是操作者（§11.2）。
///
/// 只有两种：在该渠道 `allow_from` 里的是操作者，其余一律不是——不是"待配对"，也
/// 不是"受限用户"，没有第三档，因为没有配对流程。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    /// 在 `allow_from` 里。**审批命令只接受它。**
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

/// 一条审批可以被批到多大范围（§7.2）。
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    /// 只批准眼前这一份执行计划。聊天里的按钮只给这一种（§11.3）。**默认**：把一个
    /// 动作预批到更大范围，等于替一个还没人看过的后续动作签字。
    #[default]
    Once,
    /// 本次 Run 的范围授权。
    Run,
    /// 绑定 Job 版本的 Cron 授权。
    CronJob,
}

/// 一条审批请求在界面上要显示的东西（§7.2 / §11.3）。正文由 executor 组装，**渲染**
/// 由各渠道实现。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPresentation {
    pub approval: ApprovalId,
    pub short_id: ShortId,
    pub plan_hash: PlanHash,
    /// 具体动作：工具、命令 / 代码、真实目标路径、cwd、版本。
    pub plan: ExecutionPlan,
    /// `PolicyDecision::Ask` 给出的原因。
    pub reason: String,
    /// 改动：write / edit 的 diff（已截断），或 toolbox 启用的版本差异。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<String>,
    /// 已有验证结果：例如候选模块的测试输出。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// 这条请求可以被批到哪些范围。总是包含 [`ApprovalScope::Once`]。
    #[serde(default)]
    pub scopes: Vec<ApprovalScope>,
    /// 审批本身的有效期。过期不能按旧指令直接产生新的外部影响（§8.4）。
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub valid_until: Option<OffsetDateTime>,
}

/// 主动投递的内容（§11.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outbound {
    /// 一段纯文本：Run 的最终回复、命令的回执、被拒绝的提示。
    Text { text: String },
    /// 一条待处理的审批（§11.3 渲染表）。
    ApprovalRequest(Box<ApprovalPresentation>),
    /// 审批已经有结论了——用来把原消息原地更新成"已批准 / 已拒绝 · 谁 · 何时"。
    ApprovalSettled {
        approval: ApprovalId,
        short_id: ShortId,
        approved: bool,
        by: PeerId,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
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
}

/// 投递目标。home chat 的解析只看配置（§11.4），所以这里是一个已解析出来的会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryTarget {
    pub peer: ChannelPeer,
    /// 这条投递是不是"补送到 home chat 的那一份"。来源会话与 home chat 都会收到审批
    /// 请求，第二个答复得到"已决定"（§11.4）。
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
    fn wechat_cannot_push_unsolicited() {
        assert!(!ChannelPlatform::Wechat.supports_unsolicited_push());
        assert!(ChannelPlatform::Feishu.supports_unsolicited_push());
        assert!(ChannelPlatform::Telegram.supports_unsolicited_push());
    }
}
