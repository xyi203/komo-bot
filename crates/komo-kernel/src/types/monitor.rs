//! `monitor` / `monitor_stop`：挂一个长时间跑的脚本，它往 stdout 每打一行，就作为一条
//! 内部输入交回挂它的会话（`docs/monitor.md`）。
//!
//! 时限常量放在这里：`monitor` 工具（runtime）按它收紧模型给的时限。

/// 省略 `timeout_minutes` 时监听跑多久。
pub const DEFAULT_MONITOR_MINUTES: u64 = 30;
/// 一个监听最多跑多久。到点终止进程组并交回一条"超时"，要接着盯就重新挂。
pub const MAX_MONITOR_MINUTES: u64 = 24 * 60;

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
    fn the_timeout_is_clamped_into_its_range() {
        assert_eq!(clamp_minutes(None), DEFAULT_MONITOR_MINUTES);
        assert_eq!(clamp_minutes(Some(0)), 1);
        assert_eq!(clamp_minutes(Some(90)), 90);
        assert_eq!(clamp_minutes(Some(10_000)), MAX_MONITOR_MINUTES);
    }
}
