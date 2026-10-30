//! 渠道的起停：**只重启变了的那一个**（§3 第 3 步）。
//!
//! 每个渠道一个 `serve` 任务，手里一个 [`Shutdown`] 令牌。重载时按平台停掉再按新快照
//! 造一个——「其他渠道不受影响，未 ack 的入站消息按平台的至少一次投递重来」。

use std::collections::BTreeMap;
use std::sync::Arc;

use komo_kernel::protocol::http::ChannelRunState;
use komo_kernel::traits::Shutdown;
use komo_kernel::types::chat::ChannelPlatform;
use time::OffsetDateTime;

use crate::channels::ChannelFactory;

use super::state::GatewayState;

/// 渠道此刻的样子（`GET /v1/channels`）：只是 Gateway 看得见的那一层——插件进程起没起、
/// 退了几次、为什么退。平台那头的长连接在插件里，掉了由插件自己告警。
#[derive(Debug, Clone)]
pub struct ChannelHealth {
    pub state: ChannelRunState,
    pub since: OffsetDateTime,
    pub detail: Option<String>,
    pub restarts: u32,
}
type HealthMap = Arc<std::sync::Mutex<BTreeMap<ChannelPlatform, ChannelHealth>>>;
fn mark(
    health: &HealthMap,
    platform: &ChannelPlatform,
    state: ChannelRunState,
    detail: Option<String>,
) {
    let mut map = health.lock().expect("channel health");
    let restarts =
        map.get(platform).map_or(0, |h| h.restarts) + u32::from(state == ChannelRunState::Retrying);
    map.insert(
        platform.clone(),
        ChannelHealth {
            state,
            since: OffsetDateTime::now_utc(),
            detail,
            restarts,
        },
    );
}

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
    struct Flaky;
    #[async_trait]
    impl Channel for Flaky {
        fn name(&self) -> &'static str {
            "flaky"
        }
        async fn serve(&self, _: Arc<dyn Inbound>, _: Shutdown) -> Result<(), ChannelError> {
            Err(ChannelError {
                channel: "telegram".into(),
                message: "插件连接已关闭".into(),
            })
        }
    }
    #[tokio::test]
    async fn the_channel_view_reports_why_a_plugin_keeps_exiting() {
        let sender = MemSender::new(ChannelPlatform::Telegram);
        let factory = FixedFactory::new(ChannelPlatform::Telegram, Arc::new(Flaky), sender);
        let mut gateway = GatewayBuilder::new(&telegram_config("111"))
            .factory(factory)
            .start()
            .await;
        let state = gateway.state();
        eventually("telegram retrying", || {
            state
                .supervisor
                .health()
                .get(&ChannelPlatform::Telegram)
                .is_some_and(|h| h.state == ChannelRunState::Retrying)
        })
        .await;
        let (status, body) = gateway.get("/v1/channels").await;
        assert_eq!(status, 200, "{body}");
        let view: komo_kernel::protocol::http::ChannelsResponse =
            serde_json::from_str(&body).unwrap();
        let telegram = view
            .channels
            .iter()
            .find(|c| c.channel == "telegram")
            .unwrap();
        assert_eq!(telegram.state, ChannelRunState::Retrying);
        assert_eq!(telegram.detail.as_deref(), Some("插件连接已关闭"));
        assert!(telegram.restarts >= 1 && telegram.since.is_some());
        assert_eq!(telegram.allow_from, 1);
        let feishu = view
            .channels
            .iter()
            .find(|c| c.channel == "feishu")
            .unwrap();
        assert_eq!(
            (feishu.enabled, feishu.state),
            (false, ChannelRunState::Off)
        );
        let (status, _) = gateway
            .post("/v1/channels/nope/probe", serde_json::json!({}))
            .await;
        assert_eq!(status, 404);
        gateway.stop().await;
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
    health: HealthMap,
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
            health: HealthMap::default(),
        }
    }

    pub async fn platforms(&self) -> Vec<ChannelPlatform> {
        self.running.lock().await.keys().cloned().collect()
    }

    pub fn factories(&self) -> &[Arc<dyn ChannelFactory>] {
        &self.factories
    }

    /// 这个平台的工厂：官方三个在表里，其余（自定义渠道）走通用的进程工厂。
    pub fn factory(&self, platform: &ChannelPlatform) -> Arc<dyn ChannelFactory> {
        self.factories
            .iter()
            .find(|factory| factory.platform() == *platform)
            .cloned()
            .unwrap_or_else(|| crate::channels::process_factory(platform.clone()))
    }

    /// 每个起过的渠道此刻的状态；停掉（未启用）的不在里面。
    pub fn health(&self) -> BTreeMap<ChannelPlatform, ChannelHealth> {
        self.health.lock().expect("channel health").clone()
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
        let factory = self.factory(&platform);
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
                mark(
                    &self.health,
                    &platform,
                    ChannelRunState::Failed,
                    Some(error.message),
                );
                return;
            }
        };

        let Some(inbound) = state.inbound.get().cloned() else {
            tracing::warn!(%platform, "Dispatcher 还没接上，这个渠道先不起");
            mark(
                &self.health,
                &platform,
                ChannelRunState::Failed,
                Some("Dispatcher 还没接上".into()),
            );
            return;
        };
        let health = self.health.clone();

        let shutdown = Shutdown::new();
        let stop = shutdown.clone();
        let owner = state.clone();
        let id = platform.clone();
        let task = tokio::spawn(async move {
            let mut built = built;
            let mut delay = std::time::Duration::from_secs(1);
            loop {
                mark(&health, &id, ChannelRunState::Running, None);
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
                let why = match result {
                    Err(error) => error.message,
                    Ok(()) => "渠道自己退出了".into(),
                };
                mark(&health, &id, ChannelRunState::Retrying, Some(why));
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
                        Ok(None) => {
                            health.lock().expect("channel health").remove(&id);
                            return;
                        }
                        Err(error) => {
                            tracing::warn!(platform=%id, %error,"渠道重建失败");
                            let mut map = health.lock().expect("channel health");
                            if let Some(h) = map.get_mut(&id) {
                                h.detail = Some(error.message);
                            }
                        }
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
        // 任务收完之后再清，免得它在停下前又记一笔"在跑"
        self.health
            .lock()
            .expect("channel health")
            .remove(&platform);
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
