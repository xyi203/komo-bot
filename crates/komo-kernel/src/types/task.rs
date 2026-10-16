//! 后台任务（`task`，`docs/background-tasks.md`）：把耗时的事派给独立的**任务会话**在
//! 后台跑。2026-10-16 之前是 `dispatch` / `follow` 两个工具，旧计划照旧解码。
//!
//! 值类型放在这里，理由与 [`super::delegate`] 一样：编排操作（`Operation::Task`，以及只为
//! 旧计划保留的 `Operation::Dispatch` / `Operation::Follow`）要能落进
//! [`super::plan::ExecutionPlan`]，而 kernel 不依赖 runtime / gateway。短号那几个纯函数
//! 也放这里——回执、`task_id` 的解析与 `komo session list` 共用同一份实现，撞号与解析
//! 不能有第二种口径。

use serde::{Deserialize, Serialize};

use super::delegate::DelegateContract;
use super::ids::SessionId;
use super::model::Effort;

/// 建一个任务要交代的事：自包含的正文 + 给人看的标题（§4.1）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    /// 交给任务会话的自包含任务；它就是那条 Run 的第一条输入正文。
    pub task: String,
    /// 给人看的标题（≤ 30 字，建会话时写死，不因为后续消息改动）。
    pub title: String,
    #[serde(default)]
    pub options: TaskOptions,
}

/// 任务那条 Run 的模型、档位与结果契约（`docs/komo_bot.md` §4）。新派与接着做共用。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOptions {
    /// `[model.<alias>]` 的 alias。`None`：新任务用 Agent 的默认，接着做沿用那个任务上一条
    /// Run 的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// 结果契约：任务会话的提示带上它，交回之前用同一份校验器复验最后回复。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<DelegateContract>,
}

/// 新派一个任务成功后的句柄：模型把它念给操作者听。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskHandle {
    pub session: SessionId,
    /// 任务短号（[`short_id`]）。
    pub short_id: String,
    pub title: String,
}

/// 接着做（带 `task_id`）成功后的句柄。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowOutcome {
    pub session: SessionId,
    pub short_id: String,
    /// 提交的这一刻，那个任务会话已经有一条没跑完的 Run——模型据此措辞
    /// "正在跑，这句排在它后面"而不是"已转给"（§4.1）。
    pub queued_behind: bool,
}

/// 任务短号：任务会话 id 末 4 位（UUIDv7 的随机段），回执与 `task_id` 解析都用它
/// （§4.3）。
pub fn short_id(session: &SessionId) -> String {
    suffix(session, 4)
}

/// 撞号时延长到的那个长度（§4.3）。
pub fn extended_short_id(session: &SessionId) -> String {
    suffix(session, 6)
}

fn suffix(session: &SessionId, len: usize) -> String {
    let raw = session.as_str();
    let take = raw.len().saturating_sub(len);
    raw[take..].to_string()
}

/// 这个会话的 id 是不是以 `want` 结尾（大小写不敏感）。空串永远不匹配——一个空的
/// `task_id` 不该被解成"随便哪个都行"。
pub fn matches_short_id(session: &SessionId, want: &str) -> bool {
    let want = want.trim();
    if want.is_empty() {
        return false;
    }
    session
        .as_str()
        .to_ascii_lowercase()
        .ends_with(&want.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(raw: &str) -> SessionId {
        SessionId::from_raw(raw)
    }

    #[test]
    fn short_id_is_the_last_four_characters() {
        let id = session("0190f000-aaaa-7000-8000-0000003f2a9c");
        assert_eq!(short_id(&id), "2a9c");
        assert_eq!(extended_short_id(&id), "3f2a9c");
    }

    #[test]
    fn matching_is_case_insensitive_and_rejects_empty() {
        let id = session("0190f000-aaaa-7000-8000-0000003f2a9c");
        assert!(matches_short_id(&id, "2a9c"));
        assert!(matches_short_id(&id, "2A9C"));
        assert!(matches_short_id(&id, "3f2a9c"));
        assert!(!matches_short_id(&id, "  "));
        assert!(!matches_short_id(&id, "zzzz"));
    }
}
