//! Channel / Inbound 实现：飞书、Telegram、WeChat（§11.1、§11.5、§13.5）。
//!
//! 这个文件只放**接线用的两个东西**，渠道自己的实现各在各的 feature 门控模块里：
//!
//! - [`ChannelSender`]：Gateway 往外推一条消息的那一口。`Notifier` 的实现
//!   （[`crate::notifier::HomeNotifier`]）按平台在注册表里找它。
//! - [`ChannelFactory`]：按**当前配置快照**造一个渠道。热重载时
//!   `channels.<x>.*` 变了就只重造那一个（§3 第 3 步），所以造的动作必须是一个可以
//!   反复调用的函数，而不是启动时的一次性代码。
//!
//! 两者都只用 kernel 的类型（加上配置快照与凭证），渠道模块因此不必认识 runtime、
//! store 或 axum（§11.5）。

#[cfg(feature = "feishu")]
pub mod feishu;
#[cfg(feature = "telegram")]
pub mod telegram;
#[cfg(feature = "wechat")]
pub mod wechat;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::traits::{Channel, ChannelError, DeliverError};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, Outbound};
use komo_runtime::config::Secrets;

/// 一次发送的结果。
///
/// **`Deferred` 不是错误**（§11.4）：微信没有回复令牌时推不出去，投递记录留在
/// `pending`，由下一条入站消息触发冲刷。按**错误变体**判定，不按字符串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    Deferred { reason: String },
}

/// Gateway 往一个渠道推消息的那一口。
///
/// 渲染（飞书卡片、Telegram 内联按钮、微信纯文本）是实现的事；**决定不在这里**
/// （§11.3）。
#[async_trait]
pub trait ChannelSender: Send + Sync {
    fn platform(&self) -> ChannelPlatform;

    /// 把一条 [`Outbound`] 送到这个平台的某个会话。
    async fn send(&self, peer: &ChannelPeer, msg: Outbound) -> Result<SendOutcome, DeliverError>;
}

/// 一个渠道造出来的两半：收（`Channel::serve`）与发（[`ChannelSender`]）。
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
            .copied()
            .collect()
    }
}

/// 这台机器上接得起来的渠道工厂（按 feature）。
///
/// **接线只在这里一处**：`service::start` 拿到这个清单，热重载时按平台重造
/// （§3 第 3 步）。渠道自己不读环境变量、不读 config.toml——凭证与行为键都从这里的
/// 快照与 [`Secrets`] 传进去（§11.2、§11.5）。
#[allow(unused_mut, clippy::vec_init_then_push)]
pub fn factories() -> Vec<Arc<dyn ChannelFactory>> {
    // 每条都带 `#[cfg]`，`vec![]` 写不出来（属性不能挂在元素上）。
    let mut out: Vec<Arc<dyn ChannelFactory>> = Vec::new();
    #[cfg(feature = "telegram")]
    out.push(Arc::new(TelegramFactory::new()));
    #[cfg(feature = "feishu")]
    out.push(Arc::new(feishu::FeishuFactory::new()));
    #[cfg(feature = "wechat")]
    out.push(Arc::new(wechat::WeChatFactory::new()));
    out
}

/// 一个还没接上的渠道。
///
// TODO(decide: 飞书与微信的渠道实现是另外两个子代理的文件
// （`channels/{feishu,wechat}.rs`），W4 落地时它们还是一行模块注释。这里先占一个位置：
// `build` 答 `None`（于是 `start` 只记一行日志），`probe` 说"还没接上"。它们落地后把这
// 两行换成真的工厂即可——接口就是 `ChannelFactory`。)
#[derive(Debug)]
pub struct NotWired {
    platform: ChannelPlatform,
}

impl NotWired {
    pub fn new(platform: ChannelPlatform) -> Self {
        NotWired { platform }
    }
}

#[async_trait]
impl ChannelFactory for NotWired {
    fn platform(&self) -> ChannelPlatform {
        self.platform
    }

    fn build(
        &self,
        snapshot: &ConfigSnapshot,
        _secrets: &Secrets,
    ) -> Result<Option<BuiltChannel>, ChannelError> {
        if snapshot
            .channels
            .get(self.platform)
            .is_some_and(|channel| channel.enabled)
        {
            tracing::warn!(platform = %self.platform, "这个渠道的实现还没接上，这次不启用");
        }
        Ok(None)
    }

    async fn probe(
        &self,
        _snapshot: &ConfigSnapshot,
        _secrets: &Secrets,
    ) -> Result<String, ChannelError> {
        Err(ChannelError {
            channel: self.platform.to_string(),
            message: "实现还没接上".into(),
        })
    }
}

/// Telegram：长轮询 + `getMe`（§11.5）。
#[cfg(feature = "telegram")]
#[derive(Debug, Default)]
pub struct TelegramFactory {
    http: reqwest::Client,
}

#[cfg(feature = "telegram")]
impl TelegramFactory {
    pub fn new() -> Self {
        TelegramFactory {
            http: reqwest::Client::new(),
        }
    }
}

#[cfg(feature = "telegram")]
#[async_trait]
impl ChannelFactory for TelegramFactory {
    fn platform(&self) -> ChannelPlatform {
        ChannelPlatform::Telegram
    }

    fn build(
        &self,
        snapshot: &ConfigSnapshot,
        secrets: &Secrets,
    ) -> Result<Option<BuiltChannel>, ChannelError> {
        let Some(config) = snapshot.channels.get(ChannelPlatform::Telegram) else {
            return Ok(None);
        };
        if !config.enabled {
            return Ok(None);
        }
        let token = token(secrets, "TELEGRAM_BOT_TOKEN", "telegram")?;
        let channel = Arc::new(telegram::TelegramChannel::new(
            token,
            config.clone(),
            self.http.clone(),
        ));
        let sender = channel.sender() as Arc<dyn ChannelSender>;
        Ok(Some(BuiltChannel {
            channel: channel as Arc<dyn Channel>,
            sender,
        }))
    }

    async fn probe(
        &self,
        _snapshot: &ConfigSnapshot,
        secrets: &Secrets,
    ) -> Result<String, ChannelError> {
        let token = token(secrets, "TELEGRAM_BOT_TOKEN", "telegram")?;
        telegram::probe(&token, self.http.clone())
            .await
            .map(|identity| {
                format!(
                    "getMe 通过：@{}",
                    identity.username.as_deref().unwrap_or("（没有用户名）")
                )
            })
            .map_err(|error| ChannelError {
                channel: "telegram".into(),
                message: error.to_string(),
            })
    }
}

/// 从 `.env` 取一个凭证。**只取变量名对应的值**，快照里从来没有它。
#[allow(dead_code)]
fn token(secrets: &Secrets, var: &str, channel: &str) -> Result<String, ChannelError> {
    secrets
        .get(var)
        .map(str::to_string)
        .ok_or_else(|| ChannelError {
            channel: channel.to_string(),
            message: format!(".env 里没有 {var}"),
        })
}

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
            self.platform.unwrap_or(ChannelPlatform::Api)
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
