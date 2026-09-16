//! 渠道的起停：**只重启变了的那一个**（§3 第 3 步）。
//!
//! 每个渠道一个 `serve` 任务，手里一个 [`Shutdown`] 令牌。重载时按平台停掉再按新快照
//! 造一个——「其他渠道不受影响，未 ack 的入站消息按平台的至少一次投递重来」。

use std::collections::BTreeMap;
use std::sync::Arc;

use komo_kernel::traits::Shutdown;
use komo_kernel::types::chat::ChannelPlatform;

use crate::channels::{ChannelFactory, ChannelSender};

use super::state::GatewayState;

/// 一个起着的渠道。
struct Running {
    shutdown: Shutdown,
}

/// 渠道的起停。
pub struct ChannelSupervisor {
    factories: Vec<Arc<dyn ChannelFactory>>,
    running: tokio::sync::Mutex<BTreeMap<ChannelPlatform, Running>>,
}

impl std::fmt::Debug for ChannelSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelSupervisor").finish_non_exhaustive()
    }
}

impl ChannelSupervisor {
    pub fn new(factories: Vec<Arc<dyn ChannelFactory>>) -> Self {
        ChannelSupervisor {
            factories,
            running: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    pub fn factories(&self) -> &[Arc<dyn ChannelFactory>] {
        &self.factories
    }

    /// 起全部配置里 enabled 的渠道。
    pub async fn start_all(&self, state: &Arc<GatewayState>) {
        for factory in &self.factories {
            self.start(state, factory.platform()).await;
        }
    }

    /// 起一个。已经起着就先停掉。
    pub async fn start(&self, state: &Arc<GatewayState>, platform: ChannelPlatform) {
        let Some(factory) = self
            .factories
            .iter()
            .find(|factory| factory.platform() == platform)
        else {
            return;
        };
        let snapshot = state.snapshot();
        let secrets = state.config.secrets();
        let built = match factory.build(&snapshot, &secrets) {
            Ok(Some(built)) => built,
            Ok(None) => {
                tracing::info!(%platform, "这份配置下不启用这个渠道");
                return;
            }
            Err(error) => {
                // 「渠道起不来是警告，不是致命」——别的入口照常工作。
                tracing::warn!(%platform, %error, "渠道起不来");
                return;
            }
        };

        let Some(inbound) = state.inbound.get().cloned() else {
            tracing::warn!(%platform, "Dispatcher 还没接上，这个渠道先不起");
            return;
        };

        let shutdown = Shutdown::new();
        state
            .channels
            .register(Arc::clone(&built.sender) as Arc<dyn ChannelSender>);
        self.running.lock().await.insert(
            platform,
            Running {
                shutdown: shutdown.clone(),
            },
        );

        // 这个平台**此刻**才有了发送口：把它名下还没送到的投递补发掉（§11.4「重启后
        // pending 的行补发，按 DeliveryId 幂等」）。热重载重启某个渠道之后同样走这里。
        let flushed = state.notifier.flush_platform(platform).await;
        if flushed > 0 {
            tracing::info!(%platform, flushed, "渠道起来后补发了积压的投递");
        }

        let channel = Arc::clone(&built.channel);
        tokio::spawn(async move {
            let name = channel.name();
            tracing::info!(channel = name, "渠道开始服务");
            if let Err(error) = channel.serve(inbound, shutdown).await {
                tracing::warn!(channel = name, %error, "渠道退出了");
            }
        });
    }

    /// 停一个：撤掉发送口，按下它自己的令牌。
    pub async fn stop(&self, state: &Arc<GatewayState>, platform: ChannelPlatform) {
        if let Some(running) = self.running.lock().await.remove(&platform) {
            running.shutdown.cancel();
        }
        state.channels.remove(platform);
    }

    /// 停掉再起一个。
    pub async fn restart(&self, state: &Arc<GatewayState>, platform: ChannelPlatform) {
        tracing::info!(%platform, "配置变了，重启这个渠道");
        self.stop(state, platform).await;
        self.start(state, platform).await;
    }

    /// 全停（停机时）。
    pub async fn stop_all(&self, state: &Arc<GatewayState>) {
        let platforms: Vec<ChannelPlatform> = self.running.lock().await.keys().copied().collect();
        for platform in platforms {
            self.stop(state, platform).await;
        }
    }
}
