//! Messaging infra: ingress channels, outbound senders, and proactive notifiers.
pub mod api;
pub mod feishu;
pub mod home_notifier;
pub mod telegram;
pub mod wechat;

use std::time::Duration;

/// Reconnect backoff schedule (seconds), clamped at the last step. Shared by the
/// long-lived ingress channels (feishu/telegram WebSocket + poll loops, the HA
/// event socket) so a persistent failure — a revoked token, an auth error —
/// backs off instead of hammering the API every few seconds. Callers reset the
/// index to 0 after a successful connection.
const RECONNECT_BACKOFF_STEPS: [u64; 4] = [5, 10, 30, 60];

/// The reconnect delay for the `idx`-th consecutive failure (0-based), clamped
/// at the last step of [`RECONNECT_BACKOFF_STEPS`].
pub fn reconnect_backoff(idx: usize) -> Duration {
    Duration::from_secs(RECONNECT_BACKOFF_STEPS[idx.min(RECONNECT_BACKOFF_STEPS.len() - 1)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backoff_escalates_then_clamps() {
        assert_eq!(reconnect_backoff(0), Duration::from_secs(5));
        assert_eq!(reconnect_backoff(1), Duration::from_secs(10));
        assert_eq!(reconnect_backoff(3), Duration::from_secs(60));
        // Past the last step, it clamps rather than panicking on the index.
        assert_eq!(reconnect_backoff(99), Duration::from_secs(60));
    }
}
