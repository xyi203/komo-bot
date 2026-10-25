//! `SessionKind`：会话的用途（`docs/bot.md` §4.2）。新存储里它存在 `sessions` 行的 record 里。

/// 会话用途的那几个词（`docs/bot.md` §4.2：「先保持简单」——不要一开始枚举所有
/// 平台和业务场景）。
///
/// 它是**用途**，不是身份：身份是 `agent_id`。同一个 Agent 的主会话与任务会话只有 `kind`
/// 不同，两者都归它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// 一个 Agent 的**主会话**：稳定的入口（§4.2 的 `main_session(agent_id)`）。每个
    /// Agent 至多一条（活着的），由 gateway 的 `main_session` 建会话时保证。
    Main,
    /// 普通会话：平台会话、`POST /v1/sessions` 建的、操作者的私聊。
    Normal,
    /// 独立任务会话：跨 Agent 委派时给目标 Agent 建的**任务**会话——不是它的主聊天
    /// （§4.2 最后一段：不要默认把子任务塞进目标 Bot 的主会话）。
    Task,
}

impl SessionKind {
    /// 存下来的那三个词。
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Main => "main",
            SessionKind::Normal => "normal",
            SessionKind::Task => "task",
        }
    }

    /// 存下来的值 → 词汇表里的词。认不出 = `None`，调用方按**损坏**处理（与
    /// [`komo_kernel::types::status::SessionState::parse`] 一个规矩）：挑个默认值会把
    /// 一条主会话读成普通会话。
    pub fn parse(raw: &str) -> Option<SessionKind> {
        match raw {
            "main" => Some(SessionKind::Main),
            "normal" => Some(SessionKind::Normal),
            "task" => Some(SessionKind::Task),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionKind;

    #[test]
    fn every_kind_round_trips_through_its_word() {
        for kind in [SessionKind::Main, SessionKind::Normal, SessionKind::Task] {
            assert_eq!(SessionKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(SessionKind::parse("stranger"), None);
    }
}
