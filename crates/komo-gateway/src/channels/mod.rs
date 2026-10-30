//! Generic channel host: process factories, send registry and in-memory test channels.
//! Platform SDKs, login and rendering live in the komo-channel crate and its standalone plugin executables.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::traits::ChannelError;
#[cfg(any(test, feature = "test-support"))]
use komo_kernel::traits::{Channel, DeliverError};
use komo_kernel::types::chat::ChannelPlatform;
#[cfg(any(test, feature = "test-support"))]
use komo_kernel::types::chat::{ChannelPeer, Outbound};
use komo_runtime::config::Secrets;

pub use komo_channel::{BuiltChannel, ChannelSender, SendOutcome};

/// 按当前配置造一个渠道。
///
/// 热重载时 Gateway 拿着新快照再调一次它：`Ok(None)` = 这份配置下该渠道不启用
/// （`enabled = false` 或凭证缺失），于是只停掉它，不影响别的渠道。
#[async_trait]
pub trait ChannelFactory: Send + Sync {
    fn platform(&self) -> ChannelPlatform;

    fn build(
        &self,
        snapshot: &ConfigSnapshot,
        secrets: &Secrets,
    ) -> Result<Option<BuiltChannel>, ChannelError>;

    /// `komo channel probe` 的连通性核对（飞书拿 tenant token、Telegram `getMe`、
    /// 微信检查凭证文件，§11.5）。**不经 Gateway**——它只要配置与凭证。
    async fn probe(
        &self,
        _snapshot: &ConfigSnapshot,
        _secrets: &Secrets,
    ) -> Result<String, ChannelError> {
        Err(ChannelError {
            channel: self.platform().to_string(),
            message: "这个渠道没有实现连通性核对".into(),
        })
    }
}

/// 平台 → 发送口。渠道起来时登记，停掉时注销。
#[derive(Default)]
pub struct ChannelRegistry {
    senders: RwLock<BTreeMap<ChannelPlatform, Arc<dyn ChannelSender>>>,
}

impl std::fmt::Debug for ChannelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelRegistry")
            .field("platforms", &self.platforms())
            .finish()
    }
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, sender: Arc<dyn ChannelSender>) {
        self.senders
            .write()
            .expect("渠道注册表")
            .insert(sender.platform(), sender);
    }

    pub fn remove(&self, platform: ChannelPlatform) {
        self.senders.write().expect("渠道注册表").remove(&platform);
    }

    pub fn get(&self, platform: ChannelPlatform) -> Option<Arc<dyn ChannelSender>> {
        self.senders
            .read()
            .expect("渠道注册表")
            .get(&platform)
            .cloned()
    }

    pub fn platforms(&self) -> Vec<ChannelPlatform> {
        self.senders
            .read()
            .expect("渠道注册表")
            .keys()
            .cloned()
            .collect()
    }
}

mod process;
pub(crate) use process::process_factory;
pub use process::{factories, login};

#[cfg(any(test, feature = "test-support"))]
pub mod test_channel {
    //! 内存渠道：集成测试的收发两半。

    use super::*;
    use komo_kernel::protocol::{InboundAck, InboundMessage};
    use komo_kernel::traits::{Inbound, Shutdown};
    use std::sync::Mutex;

    /// 送出去的一条。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SentMessage {
        pub peer: ChannelPeer,
        pub outbound: Outbound,
    }

    /// 一个内存渠道：`send` 记在 `sent` 里，可以被调成"此刻推不出去"。
    #[derive(Debug, Default)]
    pub struct MemChannel {
        platform: Option<ChannelPlatform>,
        sent: Mutex<Vec<SentMessage>>,
        deferring: std::sync::atomic::AtomicBool,
    }

    impl MemChannel {
        pub fn new(platform: ChannelPlatform) -> Arc<Self> {
            Arc::new(MemChannel {
                platform: Some(platform),
                sent: Mutex::new(Vec::new()),
                deferring: std::sync::atomic::AtomicBool::new(false),
            })
        }

        pub fn sent(&self) -> Vec<SentMessage> {
            self.sent.lock().expect("发送记录").clone()
        }

        /// 微信那条路径：没有回复令牌时 `Deferred`。
        pub fn defer(&self, deferring: bool) {
            self.deferring
                .store(deferring, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl ChannelSender for MemChannel {
        fn platform(&self) -> ChannelPlatform {
            self.platform.clone().unwrap_or(ChannelPlatform::Api)
        }

        async fn send(
            &self,
            peer: &ChannelPeer,
            msg: Outbound,
        ) -> Result<SendOutcome, DeliverError> {
            if self.deferring.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(SendOutcome::Deferred {
                    reason: "没有回复令牌".into(),
                });
            }
            self.sent.lock().expect("发送记录").push(SentMessage {
                peer: peer.clone(),
                outbound: msg,
            });
            Ok(SendOutcome::Sent)
        }
    }

    #[async_trait]
    impl Channel for MemChannel {
        fn name(&self) -> &'static str {
            "mem"
        }

        async fn serve(
            &self,
            _inbound: Arc<dyn Inbound>,
            shutdown: Shutdown,
        ) -> Result<(), ChannelError> {
            // 入站由测试直接调 `Dispatcher::handle`；这里只是把 serve 的形状补全。
            while !shutdown.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Ok(())
        }
    }

    /// 让编译器替我们证明这两个 trait 都是对象安全的。
    #[allow(dead_code)]
    fn assert_object_safe(_: &dyn ChannelSender, _: &dyn ChannelFactory) {}

    #[allow(dead_code)]
    async fn unused(inbound: Arc<dyn Inbound>, msg: InboundMessage) -> Option<InboundAck> {
        inbound.handle(msg).await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_channel::MemChannel;

    #[tokio::test]
    async fn a_registry_routes_by_platform() {
        let registry = ChannelRegistry::new();
        let telegram = MemChannel::new(ChannelPlatform::Telegram);
        registry.register(telegram.clone() as Arc<dyn ChannelSender>);

        let sender = registry.get(ChannelPlatform::Telegram).expect("登记过了");
        let outcome = sender
            .send(
                &ChannelPeer::new(ChannelPlatform::Telegram, "123"),
                Outbound::Text { text: "在".into() },
            )
            .await
            .unwrap();
        assert_eq!(outcome, SendOutcome::Sent);
        assert_eq!(telegram.sent().len(), 1);
        assert!(registry.get(ChannelPlatform::Feishu).is_none());

        registry.remove(ChannelPlatform::Telegram);
        assert!(registry.get(ChannelPlatform::Telegram).is_none());
    }
}
