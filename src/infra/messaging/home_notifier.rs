//! The home-channel notifier: a single [`Notifier`] that delivers all proactive
//! output (reminders, task due notices, the gateway's shutdown notice) to the
//! current home chat.
//!
//! The home is resolved at notify-time, so a `/sethome` command takes effect on
//! the next notification without a restart: the runtime override (db) wins over
//! the config `home_chat` fallback. With no home resolved there is nowhere to
//! deliver, and the caller is told so rather than left wondering.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::warn;

use crate::{
    domain::{home::HomeRepository, notify::Notifier},
    infra::messaging::{feishu::FeishuSender, telegram::TelegramSender},
};

/// Outbound text to one chat, abstracted over the concrete channel senders so
/// the notifier can route by platform.
#[async_trait]
pub trait TextSender: Send + Sync {
    async fn send_text(&self, chat_id: &str, text: &str) -> anyhow::Result<()>;
}

#[async_trait]
impl TextSender for FeishuSender {
    async fn send_text(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
        FeishuSender::send_text(self, chat_id, text).await
    }
}

#[async_trait]
impl TextSender for TelegramSender {
    async fn send_text(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
        TelegramSender::send_text(self, chat_id, text).await
    }
}

pub struct HomeNotifier {
    /// Channel senders keyed by platform (`feishu`, `telegram`).
    senders: HashMap<String, Arc<dyn TextSender>>,
    /// Runtime `/sethome` override, looked up fresh on every notify.
    home: Arc<dyn HomeRepository>,
    /// Config `home_chat`, as a `{platform}:{chat_id}` session id. Used when no
    /// `/sethome` override is set.
    fallback: Option<String>,
}

impl HomeNotifier {
    pub fn new(
        senders: HashMap<String, Arc<dyn TextSender>>,
        home: Arc<dyn HomeRepository>,
        fallback: Option<String>,
    ) -> Self {
        Self {
            senders,
            home,
            fallback,
        }
    }

    /// Resolve the active home to `(sender, chat_id)`: the `/sethome` override
    /// wins over the config fallback. `None` when nothing resolves to a channel
    /// we can send through (no home set, or its platform has no sender).
    async fn resolve(&self) -> Option<(Arc<dyn TextSender>, String)> {
        let target = match self.home.get().await {
            Ok(Some(target)) => target,
            Ok(None) => self.fallback.clone()?,
            Err(error) => {
                warn!(%error, "home lookup failed; falling back to config home_chat");
                self.fallback.clone()?
            }
        };
        let (platform, chat_id) = target.split_once(':')?;
        let sender = self.senders.get(platform)?.clone();
        Some((sender, chat_id.to_string()))
    }
}

#[async_trait]
impl Notifier for HomeNotifier {
    async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()> {
        match self.resolve().await {
            Some((sender, chat_id)) => {
                let text = if body.is_empty() {
                    title.to_string()
                } else {
                    format!("{title}\n{body}")
                };
                // Surface a delivery failure (e.g. a WeChat home with no
                // in-memory context_token yet after a restart) — the caller
                // still sees the Err, but silence made "why didn't the reminder
                // arrive" unanswerable.
                if let Err(error) = sender.send_text(&chat_id, &text).await {
                    warn!(%error, %chat_id, "home notification delivery failed");
                    return Err(error);
                }
                Ok(())
            }
            None => anyhow::bail!("no home chat is configured; nothing to notify"),
        }
    }
}
