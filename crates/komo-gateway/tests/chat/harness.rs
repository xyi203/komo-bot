//! 一整台真 Gateway：真数据目录、真 state.db、真 HTTP 监听、真 Dispatcher、真
//! `HomeNotifier`，加上三个渠道的假平台服务端。
//!
//! 与 `src/service/test_support.rs` 形状相同，但那一份是 `#[cfg(test)]` 的，集成测试
//! 拿不到（见报告"需要编排者做"）。这里是逐条重写的精简版，**不改任何 src**。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use komo_gateway::channels::{
    BuiltChannel, ChannelFactory, ChannelRegistry, ChannelSender, SendOutcome,
};
use komo_gateway::service::state::GatewayState;
use komo_gateway::service::{Running, ServiceOptions, start};
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::traits::{Channel, ChannelError, DeliverError, Inbound, LlmClient};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, Outbound};
use komo_kernel::types::ids::RunId;
use komo_runtime::config::Secrets;

// ---------------------------------------------------------------- 配置文本

/// 一份能通过校验的最小 config.toml，`extra` 追加在后面。
pub fn config_toml(extra: &str) -> String {
    format!(
        r#"
[model]
provider = "openai_responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[memory]
enabled = false
{extra}
"#
    )
}

/// `[channels.telegram]`：操作者 111，home chat 111，允许的群 222。
pub fn telegram_block(allow_from: &str) -> String {
    format!(
        r#"
[channels.telegram]
enabled = true
allow_from = [{allow_from}]
home_chat = 111
groups = [222]
"#
    )
}

/// `[channels.feishu]`：操作者 `ou_op`，home chat `oc_home`，允许的群 `oc_group`。
pub fn feishu_block(allow_from: &str) -> String {
    format!(
        r#"
[channels.feishu]
enabled = true
allow_from = [{allow_from}]
home_chat = "oc_home"
groups = ["oc_group"]
"#
    )
}

/// `[channels.wechat]`：操作者 `wxid_op`，home chat 也是他（微信只有 DM）。
pub fn wechat_block(allow_from: &str) -> String {
    format!(
        r#"
[channels.wechat]
enabled = true
allow_from = [{allow_from}]
home_chat = "wxid_op"
"#
    )
}

/// 默认：只有 Telegram 一个渠道。
pub fn telegram_config(allow_from: &str) -> String {
    config_toml(&telegram_block(allow_from))
}

// ---------------------------------------------------------------- 渠道替身

/// 送出去的一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    pub peer: ChannelPeer,
    pub outbound: Outbound,
}

/// 一个内存发送口：`send` 记在 `sent` 里，可以被调成"此刻推不出去"。
#[derive(Debug)]
pub struct MemSender {
    platform: ChannelPlatform,
    sent: std::sync::Mutex<Vec<SentMessage>>,
    deferring: std::sync::atomic::AtomicBool,
}

impl MemSender {
    pub fn new(platform: ChannelPlatform) -> Arc<Self> {
        Arc::new(MemSender {
            platform,
            sent: std::sync::Mutex::new(Vec::new()),
            deferring: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn sent(&self) -> Vec<SentMessage> {
        self.sent.lock().expect("发送记录").clone()
    }

    pub fn texts(&self) -> Vec<String> {
        self.sent()
            .into_iter()
            .filter_map(|message| match message.outbound {
                Outbound::Text { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    /// 微信那条路径：没有回复令牌时 `Deferred`。
    pub fn defer(&self, deferring: bool) {
        self.deferring
            .store(deferring, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ChannelSender for MemSender {
    fn platform(&self) -> ChannelPlatform {
        self.platform
    }

    async fn send(&self, peer: &ChannelPeer, msg: Outbound) -> Result<SendOutcome, DeliverError> {
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

/// 一个什么都不收的 `Channel`：`serve` 只是把形状补全，入站由测试自己驱动。
pub struct IdleChannel;

#[async_trait::async_trait]
impl Channel for IdleChannel {
    fn name(&self) -> &'static str {
        "idle"
    }

    async fn serve(
        &self,
        _inbound: Arc<dyn Inbound>,
        shutdown: komo_kernel::traits::Shutdown,
    ) -> Result<(), ChannelError> {
        while !shutdown.is_cancelled() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(())
    }
}

/// 把一对**已经造好**的收发两半当成一个工厂交给 `service::start`。
///
/// `build` 照 `ChannelFactory` 的契约看当前快照：`enabled = false` → `None`，于是
/// 热重载时"停掉这一个"这条路也走得通。
pub struct FixedFactory {
    platform: ChannelPlatform,
    channel: Arc<dyn Channel>,
    sender: Arc<dyn ChannelSender>,
}

impl FixedFactory {
    pub fn new(
        platform: ChannelPlatform,
        channel: Arc<dyn Channel>,
        sender: Arc<dyn ChannelSender>,
    ) -> Arc<Self> {
        Arc::new(FixedFactory {
            platform,
            channel,
            sender,
        })
    }

    /// 只有出站一半（入站由测试直接调 Dispatcher）。
    pub fn sender_only(sender: Arc<dyn ChannelSender>) -> Arc<Self> {
        let platform = sender.platform();
        Self::new(platform, Arc::new(IdleChannel), sender)
    }
}

#[async_trait::async_trait]
impl ChannelFactory for FixedFactory {
    fn platform(&self) -> ChannelPlatform {
        self.platform
    }

    fn build(
        &self,
        snapshot: &ConfigSnapshot,
        _secrets: &Secrets,
    ) -> Result<Option<BuiltChannel>, ChannelError> {
        if !snapshot
            .channels
            .get(self.platform)
            .is_some_and(|channel| channel.enabled)
        {
            return Ok(None);
        }
        Ok(Some(BuiltChannel {
            channel: Arc::clone(&self.channel),
            sender: Arc::clone(&self.sender),
        }))
    }
}

// ---------------------------------------------------------------- Gateway

/// 起一台 Gateway 要给的东西。
pub struct GatewayBuilder {
    config: String,
    env: String,
    llm: Option<Arc<dyn LlmClient>>,
    factories: Vec<Arc<dyn ChannelFactory>>,
    home: Option<PathBuf>,
}

impl GatewayBuilder {
    pub fn new(config: &str) -> Self {
        GatewayBuilder {
            config: config.to_string(),
            env: DEFAULT_ENV.to_string(),
            llm: None,
            factories: Vec::new(),
            home: None,
        }
    }

    pub fn env(mut self, env: &str) -> Self {
        self.env = env.to_string();
        self
    }

    pub fn llm(mut self, llm: Arc<dyn LlmClient>) -> Self {
        self.llm = Some(llm);
        self
    }

    pub fn factory(mut self, factory: Arc<dyn ChannelFactory>) -> Self {
        self.factories.push(factory);
        self
    }

    /// 复用一个已有的数据目录（"停机再起来"的测试用）。
    pub fn at(mut self, home: &Path) -> Self {
        self.home = Some(home.to_path_buf());
        self
    }

    pub async fn start(self) -> TestGateway {
        install_crypto();
        let (home_dir, home) = match self.home {
            Some(path) => (None, path),
            None => {
                let dir = tempfile::tempdir().expect("临时数据目录");
                let path = dir.path().to_path_buf();
                (Some(dir), path)
            }
        };
        write_home(&home, &self.config, &self.env);

        let running = start(ServiceOptions {
            home: Some(home.clone()),
            // 端口 0：内核挑一个空的，发现文件里写的是真实地址。
            listen: Some("127.0.0.1:0".into()),
            channels: self.factories.clone(),
            llm: self.llm,
        })
        .await
        .expect("Gateway 起得来");

        TestGateway {
            _home_dir: home_dir,
            home,
            running: Some(running),
            factories: self.factories,
        }
    }
}

const DEFAULT_ENV: &str = "KOMO_LLM_API_KEY=test-key\nTELEGRAM_BOT_TOKEN=test-bot-token\n";

/// 一台起着的 Gateway。
pub struct TestGateway {
    /// 临时目录的所有权；`None` = 目录是别人的（`at` 复用的那一种）。
    _home_dir: Option<tempfile::TempDir>,
    pub home: PathBuf,
    running: Option<Running>,
    factories: Vec<Arc<dyn ChannelFactory>>,
}

impl TestGateway {
    /// 默认配置：一个 Telegram 渠道（操作者 111）+ 一个内存发送口。
    pub async fn start() -> (TestGateway, Arc<MemSender>) {
        let sender = MemSender::new(ChannelPlatform::Telegram);
        let gateway = GatewayBuilder::new(&telegram_config("111"))
            .factory(FixedFactory::sender_only(
                Arc::clone(&sender) as Arc<dyn ChannelSender>
            ))
            .start()
            .await;
        (gateway, sender)
    }

    fn running(&self) -> &Running {
        self.running.as_ref().expect("Gateway 还在跑")
    }

    pub fn state(&self) -> &Arc<GatewayState> {
        &self.running().state
    }

    pub fn dispatcher(&self) -> &Arc<komo_gateway::Dispatcher> {
        &self.running().dispatcher
    }

    pub fn channels(&self) -> &Arc<ChannelRegistry> {
        &self.state().channels
    }

    pub fn base_url(&self) -> &str {
        &self.running().base_url
    }

    pub fn token(&self) -> &str {
        &self.running().state.token
    }

    pub async fn handle(&self, msg: InboundMessage) -> InboundAck {
        self.dispatcher()
            .handle(msg)
            .await
            .expect("Dispatcher 处理")
    }

    /// 改 config.toml（热重载的测试用）。
    pub fn write_config(&self, config: &str) {
        let path = self.home.join("config.toml");
        std::fs::write(&path, config).expect("写 config.toml");
        // mtime 的粒度可能是秒；把时间推一下，`changed_since` 才看得出来。
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let _ = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_times(std::fs::FileTimes::new().set_modified(later)));
    }

    pub async fn reload(&self) {
        komo_gateway::reload::reload(self.state())
            .await
            .expect("新配置装得上");
    }

    /// 停机（放锁、删发现文件）。数据目录留着。
    pub async fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop().await;
        }
    }

    /// 停机再按**同一个数据目录**起一台（"重启后 pending 补发"的测试用）。
    pub async fn restart_with(&mut self, config: &str, factories: Vec<Arc<dyn ChannelFactory>>) {
        self.stop().await;
        let mut builder = GatewayBuilder::new(config).at(&self.home);
        for factory in &factories {
            builder = builder.factory(Arc::clone(factory));
        }
        // `at` 之后 `_home_dir` 是 `None`（目录是我们的），解构掉不影响。
        let TestGateway {
            running, factories, ..
        } = builder.start().await;
        self.running = running;
        self.factories = factories;
    }

    pub async fn get(&self, path: &str) -> (u16, String) {
        self.request(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: serde_json::Value) -> (u16, String) {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base_url()))
            .bearer_auth(self.token());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("请求发得出去");
        let status = response.status().as_u16();
        (status, response.text().await.unwrap_or_default())
    }

    /// 造一条待处理的审批（executor 真跑起来时做的就是这件事）。
    pub async fn pending_approval(&self) -> komo_kernel::protocol::http::ApprovalRecord {
        let session = self.state().home_session().await.expect("home session");
        self.pending_approval_in(&session, None).await
    }

    pub async fn pending_approval_in(
        &self,
        session: &komo_kernel::types::ids::SessionId,
        run: Option<RunId>,
    ) -> komo_kernel::protocol::http::ApprovalRecord {
        let plan = komo_kernel::test_support::sample_plan("shell", session);
        self.state()
            .approvals
            .request(komo_runtime::approvals::ApprovalRequest {
                session: session.clone(),
                run,
                call: None,
                plan,
                reason: "任意 shell 命令要人看一眼".into(),
                changes: None,
                evidence: None,
                scopes: vec![komo_kernel::types::chat::ApprovalScope::Once],
            })
            .await
            .expect("写得下")
    }

    /// 还没送到的投递。
    pub async fn pending_deliveries(&self) -> Vec<komo_store::DeliveryRecord> {
        self.state()
            .notifier
            .log()
            .pending(None)
            .await
            .expect("读得出")
    }

    pub async fn sessions(&self) -> Vec<komo_store::repos::session::SessionRecord> {
        komo_store::repos::session::list(&self.state().db)
            .await
            .expect("读得出")
    }

    pub async fn runs_of(
        &self,
        session: &komo_kernel::types::ids::SessionId,
    ) -> Vec<komo_store::repos::runs::RunRecord> {
        komo_store::repos::runs::list_for_session(&self.state().db, session)
            .await
            .expect("读得出")
    }
}

fn write_home(home: &Path, config: &str, env: &str) {
    std::fs::create_dir_all(home).expect("数据目录");
    std::fs::write(home.join("config.toml"), config).expect("写 config.toml");
    std::fs::write(home.join(".env"), env).expect("写 .env");
    std::fs::write(home.join("policy.toml"), "rules = []\n").expect("写 policy.toml");
}

/// reqwest 用 `rustls-no-provider`：provider 由 `komo` 的 `main` 装（§13.4）。测试进程
/// 里没有那个 `main`，所以这里装一次——**只装一次**。
pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 一条来自某个平台的入站消息。
pub fn inbound(
    platform: ChannelPlatform,
    chat: &str,
    sender: &str,
    text: &str,
    request_key: &str,
    is_private: bool,
) -> InboundMessage {
    komo_gateway::dispatcher::inbound(platform, chat, sender, text, request_key, is_private)
}

/// 等一个条件成真（或超时）。
pub async fn eventually<F: Fn() -> bool>(what: &str, done: F) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("等了 5 秒还没有：{what}");
}
