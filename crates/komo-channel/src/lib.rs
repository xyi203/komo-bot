//! Channel protocol, process runner and feature-gated platform plugins.
//! Gateway uses the common surface with all platform features disabled.
pub mod inbox;
pub mod rpc;
mod runner;

use async_trait::async_trait;
use komo_kernel::{
    protocol::config::ChannelConfig,
    traits::{Channel, ChannelError, DeliverError},
    types::chat::{ChannelPeer, ChannelPlatform, Outbound},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

pub use runner::{alerts, login_requested, main};
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelContext {
    pub config: ChannelConfig,
    pub data_dir: PathBuf,
    pub workspaces_dir: PathBuf,
}

/// Values cross the private pipe only; Debug never prints credentials.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Secrets(BTreeMap<String, String>);
impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("names", &self.0.keys())
            .finish()
    }
}
impl Secrets {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some_and(|s| !s.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SendOutcome {
    Sent,
    Deferred { reason: String },
}

#[async_trait]
pub trait ChannelSender: Send + Sync {
    fn platform(&self) -> ChannelPlatform;
    async fn send(&self, peer: &ChannelPeer, msg: Outbound) -> Result<SendOutcome, DeliverError>;
    /// Stable across retries. Platforms without a deduplication key remain at-least-once.
    async fn send_delivery(
        &self,
        _id: &str,
        peer: &ChannelPeer,
        msg: Outbound,
    ) -> Result<SendOutcome, DeliverError> {
        self.send(peer, msg).await
    }
}
pub struct BuiltChannel {
    pub channel: Arc<dyn Channel>,
    pub sender: Arc<dyn ChannelSender>,
}
impl std::fmt::Debug for BuiltChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltChannel")
            .field("channel", &self.channel.name())
            .finish()
    }
}

#[async_trait]
pub trait ChannelFactory: Send + Sync + 'static {
    fn platform(&self) -> ChannelPlatform;
    fn credentials(&self) -> &'static [&'static str] {
        &[]
    }
    fn build(
        &self,
        context: &ChannelContext,
        secrets: &Secrets,
    ) -> Result<Option<BuiltChannel>, ChannelError>;
    async fn probe(
        &self,
        context: &ChannelContext,
        secrets: &Secrets,
    ) -> Result<String, ChannelError>;
}

#[async_trait]
pub trait AlertSink: Send + Sync {
    async fn alert(&self, text: String);
}

pub fn token(secrets: &Secrets, name: &str, channel: &str) -> Result<String, ChannelError> {
    secrets
        .get(name)
        .map(str::to_owned)
        .ok_or_else(|| ChannelError {
            channel: channel.into(),
            message: format!(".env 里没有 {name}"),
        })
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Initialize {
    pub version: u32,
    pub channel: ChannelPlatform,
    pub context: ChannelContext,
    pub secrets: Secrets,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Descriptor {
    pub version: u32,
    pub channel: ChannelPlatform,
    pub credentials: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct SendRequest {
    pub delivery_id: String,
    pub peer: ChannelPeer,
    pub outbound: Outbound,
}

/// Small fixture independent of Gateway and runtime.
#[cfg(any(test, feature = "test-support"))]
pub fn test_context() -> ChannelContext {
    ChannelContext {
        config: ChannelConfig::default(),
        data_dir: PathBuf::from("/tmp/komo-channel-test"),
        workspaces_dir: PathBuf::from("/tmp/komo-channel-test/workspaces"),
    }
}

#[cfg(feature = "feishu")]
pub mod feishu;
#[cfg(feature = "telegram")]
pub mod telegram;
#[cfg(feature = "wechat")]
pub mod wechat;
