//! OTLP 导出的现状（docs/observability.md §4）：bin 里包着 span exporter 的那一层把每次导出
//! 的结果记到这里，`/healthz` 读出来给 `komo doctor`。只是状态，没有任何功能读它。

use std::sync::Mutex;

use komo_kernel::protocol::http::TelemetryHealth;
use time::OffsetDateTime;

#[derive(Debug)]
pub struct TelemetryStatus {
    inner: Mutex<TelemetryHealth>,
}

impl TelemetryStatus {
    pub fn new(endpoint: Option<String>) -> Self {
        Self {
            inner: Mutex::new(TelemetryHealth {
                endpoint,
                ..TelemetryHealth::default()
            }),
        }
    }

    /// 记一次成功。返回 `true` = 上一次是失败（这是"恢复"）。
    pub fn record_ok(&self, at: OffsetDateTime) -> bool {
        let mut health = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let recovered = health.last_error_at.is_some() && health.last_ok_at < health.last_error_at;
        health.last_ok_at = Some(at);
        recovered
    }

    /// 记一次失败。返回 `true` = 此前没在失败（这是"第一次失败"）。
    pub fn record_err(&self, at: OffsetDateTime, reason: String) -> bool {
        let mut health = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let first = health.last_error_at.is_none() || health.last_ok_at >= health.last_error_at;
        health.last_error_at = Some(at);
        health.last_error = Some(reason);
        first
    }

    pub fn health(&self) -> TelemetryHealth {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn only_the_first_failure_and_the_recovery_are_edges() {
        let status = TelemetryStatus::new(Some("http://127.0.0.1:4318".into()));
        let t = |s: i64| datetime!(2026-10-19 08:00:00 UTC) + time::Duration::seconds(s);

        assert!(!status.record_ok(t(0)), "没失败过就不算恢复");
        assert!(status.record_err(t(1), "refused".into()));
        assert!(!status.record_err(t(2), "refused".into()), "连续失败不再报");
        assert!(status.record_ok(t(3)), "失败之后的第一次成功是恢复");
        assert!(!status.record_ok(t(4)));
        assert!(
            status.record_err(t(5), "refused".into()),
            "恢复后再失败又是新的一次"
        );
        assert_eq!(status.health().last_error.as_deref(), Some("refused"));
    }
}
