//! Out-of-band delivery for proactive output.
//!
//! Implemented by the binary's home notifier (config `home_chat` else a macOS
//! notification); consumed by the sweeps and the dispatcher, which have prose
//! to deliver with no conversation to answer into.

use async_trait::async_trait;

#[async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()>;
}
