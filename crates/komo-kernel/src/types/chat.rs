//! 聊天渠道删除之后（2026-11-13，`docs/redesign.md` P17）留下的两样东西：
//!
//! - [`PeerId`]：事件里 `by`（谁答的介入 / 确认）字段的类型。存量事件里写的是渠道 peer 的 id（飞书
//!   `ou_xxx`、Telegram 整数、微信 `wxid_xxx`），读回来要照旧能解，所以类型与"字符串或整数都收"的
//!   反序列化都留着。
//! - [`SILENT_REPLY`]：内部输入开的 Run 回答整段只写这个记号时不起标题、不转告。

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// 一个发送者 / 答复者的 id：渠道时期是飞书的 `ou_xxx`、Telegram 的整数 id、微信的 `wxid_xxx`，
/// 之后是 `cli` 之类。整数与字符串两种写法都收。
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

/// 内部输入（任务结果、监听事件）或 Cron 起的 Run 不值得转告时，回答整段只写这个记号
/// （`docs/background-tasks.md` §2d，roadmap-muse S1）：Run 照常收尾、entry 照常落，只是不投递。
pub const SILENT_REPLY: &str = "NO_REPLY";

/// 这段回答是不是 [`SILENT_REPLY`]：去掉首尾空白，再去掉一层反引号 / 引号后整段相等。
/// 记号夹在别的话里不算——那是在说话。
pub fn is_silent_reply(text: &str) -> bool {
    let trimmed = text.trim();
    let unwrapped = trimmed
        .trim_matches(|c| matches!(c, '`' | '"' | '\'' | '「' | '」'))
        .trim();
    unwrapped == SILENT_REPLY
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 整段是记号才算不说话；包一层反引号 / 引号、带首尾空白也认；夹在话里不算。
    #[test]
    fn only_a_reply_that_is_the_marker_alone_is_silent() {
        for silent in ["NO_REPLY", "  NO_REPLY\n", "`NO_REPLY`", "\"NO_REPLY\""] {
            assert!(is_silent_reply(silent), "{silent:?}");
        }
        for spoken in ["", "no_reply", "NO_REPLY，没什么要说的", "结论：NO_REPLY"] {
            assert!(!is_silent_reply(spoken), "{spoken:?}");
        }
    }

    #[test]
    fn a_peer_id_reads_from_a_string_or_an_integer() {
        let ids: Vec<PeerId> = serde_json::from_str(r#"["ou_xxx", 123456789]"#).unwrap();
        assert_eq!(ids[0].as_str(), "ou_xxx");
        assert_eq!(ids[1].as_str(), "123456789");
    }
}
