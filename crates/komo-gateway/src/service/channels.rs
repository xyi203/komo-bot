//! 渠道的起停：**只重启变了的那一个**（§3 第 3 步）。
//!
//! 每个渠道一个 `serve` 任务，手里一个 [`Shutdown`] 令牌。重载时按平台停掉再按新快照
//! 造一个——「其他渠道不受影响，未 ack 的入站消息按平台的至少一次投递重来」。

use std::collections::BTreeMap;
use std::sync::Arc;

use komo_kernel::traits::Shutdown;
use komo_kernel::types::chat::ChannelPlatform;

use crate::channels::ChannelFactory;

use super::state::GatewayState;

/// 一个起着的渠道。
struct Running {
    shutdown: Shutdown,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::test_support::harness::{
        FixedFactory, GatewayBuilder, MemSender, eventually, telegram_config,
    };
    use async_trait::async_trait;
    use komo_kernel::traits::{Channel, ChannelError, Inbound};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Consumers {
        active: AtomicUsize,
        maximum: AtomicUsize,
    }
    struct Active<'a>(&'a AtomicUsize);
    impl Drop for Active<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    #[async_trait]
    impl Channel for Consumers {
        fn name(&self) -> &'static str {
            "counting"
        }
        async fn serve(&self, _: Arc<dyn Inbound>, stop: Shutdown) -> Result<(), ChannelError> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            let _guard = Active(&self.active);
            self.maximum.fetch_max(active, Ordering::SeqCst);
            while !stop.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Ok(())
        }
    }
    #[tokio::test]
    async fn concurrent_reloads_wait_for_the_old_consumer_before_starting_another() {
        let consumers = Arc::new(Consumers::default());
        let sender = MemSender::new(ChannelPlatform::Telegram);
        let factory = FixedFactory::new(ChannelPlatform::Telegram, consumers.clone(), sender);
        let mut gateway = GatewayBuilder::new(&telegram_config("111"))
            .factory(factory)
            .start()
            .await;
        eventually("one consumer", || {
            consumers.active.load(Ordering::SeqCst) == 1
        })
        .await;
        let state = gateway.state();
        tokio::join!(
            state.supervisor.restart(state, ChannelPlatform::Telegram),
            state.supervisor.restart(state, ChannelPlatform::Telegram)
        );
        eventually("one replacement", || {
            consumers.active.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(consumers.maximum.load(Ordering::SeqCst), 1);
        gateway.stop().await;
        assert_eq!(consumers.active.load(Ordering::SeqCst), 0);
    }
}

/// 渠道的起停。
pub struct ChannelSupervisor {
    factories: Vec<Arc<dyn ChannelFactory>>,
    lifecycle: tokio::sync::Mutex<()>,
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
            lifecycle: tokio::sync::Mutex::new(()),
            running: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    pub async fn platforms(&self) -> Vec<ChannelPlatform> {
        self.running.lock().await.keys().cloned().collect()
    }

    pub fn factories(&self) -> &[Arc<dyn ChannelFactory>] {
        &self.factories
    }

    /// 起全部配置里 enabled 的渠道。
    pub async fn start_all(&self, state: &Arc<GatewayState>) {
        let mut platforms = self
            .factories
            .iter()
            .map(|f| f.platform())
            .collect::<Vec<_>>();
        platforms.extend(
            state
                .snapshot()
                .channels
                .extra
                .keys()
                .filter_map(|id| ChannelPlatform::parse(id)),
        );
        platforms.sort();
        platforms.dedup();
        for platform in platforms {
            self.start(state, platform).await;
        }
    }

    /// 起一个。已经起着就先停掉。
    pub async fn start(&self, state: &Arc<GatewayState>, platform: ChannelPlatform) {
        // HTTP reload and the file watcher can overlap; never leave an untracked consumer.
        let _lifecycle = self.lifecycle.lock().await;
        self.stop_locked(state, platform.clone()).await;
        self.start_locked(state, platform).await;
    }

    async fn start_locked(&self, state: &Arc<GatewayState>, platform: ChannelPlatform) {
        let factory = self
            .factories
            .iter()
            .find(|factory| factory.platform() == platform)
            .cloned()
            .unwrap_or_else(|| crate::channels::process_factory(platform.clone()));
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
        let stop = shutdown.clone();
        let owner = state.clone();
        let id = platform.clone();
        let task = tokio::spawn(async move {
            let mut built = built;
            let mut delay = std::time::Duration::from_secs(1);
            loop {
                owner.channels.register(Arc::clone(&built.sender));
                {
                    let state = owner.clone();
                    let platform = id.clone();
                    tokio::spawn(async move {
                        state.notifier.flush_platform(platform).await;
                    });
                }
                let result = built.channel.serve(inbound.clone(), stop.clone()).await;
                owner.channels.remove(id.clone());
                if stop.is_cancelled() {
                    break;
                }
                tracing::warn!(platform=%id, ?result, "渠道退出，退避后重启");
                // Rebuild against the current snapshot, including credentials written by login.
                loop {
                    for _ in 0..delay.as_millis().div_ceil(50) {
                        if stop.is_cancelled() {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if stop.is_cancelled() {
                        return;
                    }
                    match factory.build(&owner.snapshot(), &owner.config.secrets()) {
                        Ok(Some(next)) => {
                            built = next;
                            break;
                        }
                        Ok(None) => return,
                        Err(error) => tracing::warn!(platform=%id, %error,"渠道重建失败"),
                    }
                    delay = (delay * 2).min(std::time::Duration::from_secs(60));
                }
                delay = (delay * 2).min(std::time::Duration::from_secs(60));
            }
        });
        self.running
            .lock()
            .await
            .insert(platform, Running { shutdown, task });
    }

    /// 停一个：撤掉发送口，按下它自己的令牌。
    pub async fn stop(&self, state: &Arc<GatewayState>, platform: ChannelPlatform) {
        let _lifecycle = self.lifecycle.lock().await;
        self.stop_locked(state, platform).await;
    }

    async fn stop_locked(&self, state: &Arc<GatewayState>, platform: ChannelPlatform) {
        state.channels.remove(platform.clone());
        let running = self.running.lock().await.remove(&platform);
        if let Some(mut running) = running {
            running.shutdown.cancel();
            if tokio::time::timeout(std::time::Duration::from_secs(12), &mut running.task)
                .await
                .is_err()
            {
                running.task.abort();
                let _ = running.task.await;
            }
        }
    }

    /// 停掉再起一个。
    pub async fn restart(&self, state: &Arc<GatewayState>, platform: ChannelPlatform) {
        tracing::info!(%platform, "配置变了，重启这个渠道");
        self.start(state, platform).await;
    }

    /// 全停（停机时）。
    pub async fn stop_all(&self, state: &Arc<GatewayState>) {
        let platforms: Vec<ChannelPlatform> = self.running.lock().await.keys().cloned().collect();
        for platform in platforms {
            self.stop(state, platform).await;
        }
    }
}
