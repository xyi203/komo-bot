//! `monitor` / `monitor_stop`：挂一个长时间跑的脚本，它往 stdout 每打一行，就作为一条
//! 内部输入交回挂它的会话（`docs/monitor.md`）。
//!
//! 值类型放在这里，理由与 [`super::task`] 一样：编排操作（`Operation::Monitor` /
//! `Operation::MonitorStop`）要能落进 [`super::plan::ExecutionPlan`]，而 kernel 不依赖
//! runtime / gateway。

use serde::{Deserialize, Serialize};

use super::ids::SessionId;

/// 省略 `timeout_minutes` 时监听跑多久。
pub const DEFAULT_MONITOR_MINUTES: u64 = 30;
/// 一个监听最多跑多久。到点终止进程组并交回一条"超时"，要接着盯就重新挂。
pub const MAX_MONITOR_MINUTES: u64 = 24 * 60;

/// 挂一个监听要交代的事。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorSpec {
    /// 交给 `bash -c` 的命令。只有 stdout 的行算事件。
    pub command: String,
    /// 给人看的标题，交回的每条事件都带着它。
    pub title: String,
    /// 工作目录（计划那一刻按会话 cwd 解析好）。
    pub cwd: std::path::PathBuf,
    /// 最长跑多久，分钟（已按 [`MAX_MONITOR_MINUTES`] 收紧）。
    pub timeout_minutes: u64,
    /// Gateway 重启后自动重挂（同一个 id 与短号，剩余时限接着算）。只有模型声明脚本只读、
    /// 重跑无副作用时才是 `true`——任意命令默认不自动重跑（§8.6）。
    #[serde(default)]
    pub resume_after_restart: bool,
}

/// 一次 `monitor` 成功后的句柄：模型把它念给操作者听。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorHandle {
    /// 监听的完整 id（UUIDv7）。
    pub id: String,
    /// 短号（[`short_id`]），`monitor_stop` 按它找。
    pub short_id: String,
    pub title: String,
    /// 事件交回哪个会话。
    pub session: SessionId,
    pub timeout_minutes: u64,
}

/// 一次 `monitor_stop` 成功后的句柄。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorStopped {
    pub short_id: String,
    pub title: String,
}

/// 监听短号：id 末 4 位（UUIDv7 的随机段），与任务短号同一个取法。
pub fn short_id(id: &str) -> String {
    let take = id.len().saturating_sub(4);
    id[take..].to_string()
}

/// `id` 是不是以 `want` 结尾（大小写不敏感，允许带 `#`）。空串永远不匹配。
pub fn matches_short_id(id: &str, want: &str) -> bool {
    let want = want.trim().trim_start_matches('#');
    !want.is_empty()
        && id
            .to_ascii_lowercase()
            .ends_with(&want.to_ascii_lowercase())
}

/// 收紧模型给的时限：省略用默认值，0 按 1 分钟，超过上限按上限。
pub fn clamp_minutes(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(DEFAULT_MONITOR_MINUTES)
        .clamp(1, MAX_MONITOR_MINUTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_is_the_last_four_characters() {
        assert_eq!(short_id("0199e2a4-aaaa-7bbb-8ccc-1234567890ab"), "90ab");
    }

    #[test]
    fn a_short_id_matches_with_or_without_the_hash_and_in_any_case() {
        let id = "0199e2a4-aaaa-7bbb-8ccc-1234567890ab";
        assert!(matches_short_id(id, "90ab"));
        assert!(matches_short_id(id, "#90AB"));
        assert!(!matches_short_id(id, "  "));
        assert!(!matches_short_id(id, "beef"));
    }

    #[test]
    fn the_timeout_is_clamped_into_its_range() {
        assert_eq!(clamp_minutes(None), DEFAULT_MONITOR_MINUTES);
        assert_eq!(clamp_minutes(Some(0)), 1);
        assert_eq!(clamp_minutes(Some(90)), 90);
        assert_eq!(clamp_minutes(Some(10_000)), MAX_MONITOR_MINUTES);
    }
}
