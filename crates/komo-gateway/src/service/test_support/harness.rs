//! 四个集成测试目标共用的那一台 Gateway。
//!
//! `tests/{chat,cron,memory,recovery}` 原先各抄了一份「真数据目录、真 `service::start`、
//! 内存发送口、脚本化模型」。四份的差别只在各自特有的那几个助手上——recovery 的故障账
//! 本、cron 的触发记录断言、memory 的假向量后端。共用件搬到这里，各目标的 `harness.rs`
//! 只留自己那几个。
//!
//! **真的东西一个没少**：真 config.toml、真 state.db、真 TcpListener、真调度器、真
//! Dispatcher、真 `HomeNotifier`。替身只有两个——模型与渠道，因为这两样一个要钱、一个
//! 要网。
//!
//! 两套门面，因为两种测试问的是两个问题：
//!
//! - [`GatewayBuilder`] / [`TestGateway`]：**一台起着的 Gateway**，围着它发 HTTP、喂
//!   入站消息（chat / memory）。
//! - [`Home`] / [`Gw`]：**一个数据目录**，可以反复起落（cron / recovery 的"重启之后"）。
//!
//! 它们共用同一个 `start`，分别只在"谁持有临时目录"。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::protocol::http::{ApprovalRecord, RunDetail, SubmitRunResponse};
use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::traits::{
    Channel, ChannelError, DeliverError, EmbeddingClient, Inbound, LlmClient, TurnDriver,
};
use komo_kernel::types::chat::{ApprovalPresentation, ChannelPeer, ChannelPlatform, Outbound};
use komo_kernel::types::ids::{ApprovalId, RunId, SessionId};
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::status::RunStatus;
use komo_kernel::types::turn::{LlmError, ProviderToolCall, Round, RoundInput, TurnRequest};
use komo_runtime::config::Secrets;

use crate::channels::{BuiltChannel, ChannelFactory, ChannelRegistry, ChannelSender, SendOutcome};
use crate::service::state::GatewayState;
use crate::service::{Running, ServiceOptions, start};

// ---------------------------------------------------------------- 配置文本

/// 一份能通过校验的最小 config.toml，`extra` 追加在后面。
pub fn config_toml(extra: &str) -> String {
    format!(
        r#"
[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[model.job]
type = "completion"
api_backend = "responses"
base_url = "https://jobs.example.com/v1"
model = "job-model"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

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

/// [`Home`] 的默认配置：**配了 home chat 的** Telegram 渠道，没有群。
///
/// 无人值守的那些测试（cron / recovery）要的正是这一份：审批请求有一个出口，而"群里
/// 说话"不在它们的范围里。
pub fn home_config() -> String {
    config_toml(
        r#"
[channels.telegram]
enabled = true
allow_from = [111]
home_chat = 111
"#,
    )
}

pub const DEFAULT_ENV: &str = "KOMO_LLM_API_KEY=test-key\nTELEGRAM_BOT_TOKEN=test-bot-token\n";

/// 写出一个数据目录。
///
/// `policy` 是 policy.toml 的正文；**`None` = 不写这个文件**，那样 §7.1 的初始建议生效
/// （根内写入 Allow，任意 shell 是 Ask）——无人值守的那两组测试要的正是这一条。写
/// `rules = []` 则是"一条规则都没有"，默认 Ask。
pub fn write_home_with(home: &Path, config: &str, env: &str, policy: Option<&str>) {
    std::fs::create_dir_all(home).expect("数据目录");
    std::fs::write(home.join("config.toml"), config).expect("写 config.toml");
    std::fs::write(home.join(".env"), env).expect("写 .env");
    match policy {
        Some(rules) => std::fs::write(home.join("policy.toml"), rules).expect("写 policy.toml"),
        None => {
            let _ = std::fs::remove_file(home.join("policy.toml"));
        }
    }
    std::fs::create_dir_all(home.join("workspaces")).expect("工作目录");
}

/// 同上，带一份空规则表（`rules = []`）。
pub fn write_home(home: &Path, config: &str, env: &str) {
    write_home_with(home, config, env, Some("rules = []\n"));
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
    sent: Mutex<Vec<SentMessage>>,
    deferring: std::sync::atomic::AtomicBool,
}

impl MemSender {
    pub fn new(platform: ChannelPlatform) -> Arc<Self> {
        Arc::new(MemSender {
            platform,
            sent: Mutex::new(Vec::new()),
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
                Outbound::RunFinished { summary, .. } => Some(summary),
                Outbound::NeedsAttention { reason, .. } => Some(reason),
                _ => None,
            })
            .collect()
    }

    /// 收到的每一条审批请求。
    pub fn approvals(&self) -> Vec<ApprovalPresentation> {
        self.sent()
            .into_iter()
            .filter_map(|message| match message.outbound {
                Outbound::ApprovalRequest(presentation) => Some(*presentation),
                _ => None,
            })
            .collect()
    }

    /// 送到某个会话的那些。
    pub fn to_chat(&self, chat: &str) -> Vec<SentMessage> {
        self.sent()
            .into_iter()
            .filter(|message| message.peer.chat_id.as_str() == chat)
            .collect()
    }

    /// 微信那条路径：没有回复令牌时 `Deferred`。
    pub fn defer(&self, deferring: bool) {
        self.deferring
            .store(deferring, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
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

#[async_trait]
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
/// `build` 照 `ChannelFactory` 的契约看当前快照：`enabled = false` → `None`，于是热重载
/// 时"停掉这一个"这条路也走得通。
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

#[async_trait]
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

// ---------------------------------------------------------------- 脚本化模型

pub fn text_round(n: u32, text: &str) -> Result<Round, LlmError> {
    Ok(Round {
        round: n,
        text: Some(text.to_string()),
        tool_calls: Vec::new(),
        provider_blocks: None,
        usage: TokenUsage::default(),
        truncated: false,
    })
}

pub fn call_round(
    n: u32,
    provider_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> Result<Round, LlmError> {
    Ok(Round {
        round: n,
        text: None,
        tool_calls: vec![ProviderToolCall {
            provider_call_id: provider_id.to_string(),
            name: tool.to_string(),
            arguments: args,
        }],
        provider_blocks: None,
        usage: TokenUsage::default(),
        truncated: false,
    })
}

/// 脚本化的 `LlmClient`：每次 `begin_turn` 取一段脚本。
///
/// 脚本用完之后给的是**一句收尾**，不是一个错误，也不是把上一段重放一遍。两边都踩过：
/// 报错会让故障注入之后的重领变成一个终态失败，把要测的东西盖掉；重放会让续跑的那一轮
/// 把同一个工具调用**又要一次**，于是"只跑一次"根本无从断言。
pub struct FakeLlm {
    scripts: Mutex<std::collections::VecDeque<Vec<Result<Round, LlmError>>>>,
    /// 脚本用完之后每一段都用它。
    fallback: Mutex<Vec<Result<Round, LlmError>>>,
    turns: AtomicUsize,
    pub requests: Mutex<Vec<TurnRequest>>,
}

impl FakeLlm {
    pub fn new(scripts: Vec<Vec<Result<Round, LlmError>>>) -> Arc<FakeLlm> {
        Arc::new(FakeLlm {
            scripts: Mutex::new(scripts.into_iter().collect()),
            fallback: Mutex::new(vec![text_round(99, "（没有别的要做了）")]),
            turns: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    /// **每一段**都是这个脚本（模型一直给同一个答复的情形，比如一直"回复未收齐"）。
    pub fn always(rounds: Vec<Result<Round, LlmError>>) -> Arc<FakeLlm> {
        let llm = FakeLlm::new(vec![]);
        *llm.fallback.lock().expect("脚本模型") = rounds;
        llm
    }

    /// 一段"什么都不做，直接收尾"的脚本——续跑的测试用它：这一段里**模型不该再要求
    /// 任何调用**，跑起来的调用只能来自日志里那份原计划。
    pub fn finisher(text: &str) -> Arc<FakeLlm> {
        FakeLlm::always(vec![text_round(1, text)])
    }

    /// 到目前为止开了几个 turn（= 向模型请求了几次）。
    pub fn turns(&self) -> usize {
        self.turns.load(Ordering::SeqCst)
    }

    /// 这些回合各自用的是哪个模型（§13.3：不串用模型配置，断言的就是它）。
    pub fn models(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("脚本模型")
            .iter()
            .map(|request| request.model.model.clone())
            .collect()
    }
}

#[async_trait]
impl LlmClient for FakeLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        self.turns.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().expect("脚本模型").push(req);
        let rounds = {
            let mut scripts = self.scripts.lock().expect("脚本模型");
            match scripts.pop_front() {
                Some(rounds) => rounds,
                None => self.fallback.lock().expect("脚本模型").clone(),
            }
        };
        Ok(Box::new(FakeDriver {
            rounds: rounds.into_iter().collect(),
            usage: TokenUsage::default(),
        }))
    }
}

struct FakeDriver {
    rounds: std::collections::VecDeque<Result<Round, LlmError>>,
    usage: TokenUsage,
}

#[async_trait]
impl TurnDriver for FakeDriver {
    async fn next(&mut self, _input: RoundInput) -> Result<Round, LlmError> {
        match self.rounds.pop_front() {
            Some(round) => round,
            // 脚本演完：给一句收尾，而不是一个错误（理由见 `FakeLlm` 的注释）。
            None => text_round(99, "（脚本演完了，收尾）"),
        }
    }

    fn usage(&self) -> TokenUsage {
        self.usage
    }
}

// ---------------------------------------------------------------- 起一台

/// 起一台 Gateway 要给的东西。
pub struct GatewayBuilder {
    config: String,
    env: String,
    llm: Option<Arc<dyn LlmClient>>,
    embeddings: Option<Arc<dyn EmbeddingClient>>,
    factories: Vec<Arc<dyn ChannelFactory>>,
    home: Option<PathBuf>,
}

impl GatewayBuilder {
    pub fn new(config: &str) -> Self {
        GatewayBuilder {
            config: config.to_string(),
            env: DEFAULT_ENV.to_string(),
            llm: None,
            embeddings: None,
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

    /// 注入向量后端（`tests/memory` 用）。
    pub fn embeddings(mut self, client: Arc<dyn EmbeddingClient>) -> Self {
        self.embeddings = Some(client);
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
            embeddings: self.embeddings,
        })
        .await
        .expect("Gateway 起得来");

        TestGateway {
            _home_dir: home_dir,
            home,
            env: self.env,
            running: Some(running),
            factories: self.factories,
        }
    }
}

/// 一台起着的 Gateway。
pub struct TestGateway {
    /// 临时目录的所有权；`None` = 目录是别人的（`at` 复用的那一种）。
    _home_dir: Option<tempfile::TempDir>,
    pub home: PathBuf,
    /// 起这一台时用的 `.env`——重启要用**同一份**，否则第二次起来会缺钥匙。
    env: String,
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

    pub fn dispatcher(&self) -> &Arc<crate::Dispatcher> {
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
        crate::reload::reload(self.state())
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
        let mut builder = GatewayBuilder::new(config).env(&self.env).at(&self.home);
        for factory in &factories {
            builder = builder.factory(Arc::clone(factory));
        }
        let TestGateway {
            running, factories, ..
        } = builder.start().await;
        self.running = running;
        self.factories = factories;
    }

    /// 停机再起来，换一个向量后端（`tests/memory` 的"重启之后"）。
    pub async fn restart_with_embeddings(
        &mut self,
        config: &str,
        embeddings: Arc<dyn EmbeddingClient>,
    ) {
        self.stop().await;
        let TestGateway { running, .. } = GatewayBuilder::new(config)
            .env(&self.env)
            .at(&self.home)
            .embeddings(embeddings)
            .start()
            .await;
        self.running = running;
    }

    pub async fn get(&self, path: &str) -> (u16, String) {
        self.request(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: serde_json::Value) -> (u16, String) {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }

    /// 同上，但把响应体当 JSON 读（`tests/memory` 断言的是字段）。
    pub async fn get_json(&self, path: &str) -> (u16, serde_json::Value) {
        let (code, body) = self.get(path).await;
        (code, as_json(body))
    }

    pub async fn post_json(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let (code, body) = self.post(path, body).await;
        (code, as_json(body))
    }

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        request(self.base_url(), self.token(), method, path, body).await
    }

    /// 造一条待处理的审批（executor 真跑起来时做的就是这件事）。
    pub async fn pending_approval(&self) -> ApprovalRecord {
        let session = self.state().home_session().await.expect("home session");
        self.pending_approval_in(&session, None).await
    }

    pub async fn pending_approval_in(
        &self,
        session: &SessionId,
        run: Option<RunId>,
    ) -> ApprovalRecord {
        pending_approval_in(self.state(), session, run).await
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

    pub async fn runs_of(&self, session: &SessionId) -> Vec<komo_store::repos::runs::RunRecord> {
        komo_store::repos::runs::list_for_session(&self.state().db, session)
            .await
            .expect("读得出")
    }
}

// ---------------------------------------------------------------- 一个数据目录

/// 一个真实的数据目录，可以反复起落。
///
/// **故意写一份 `rules = []` 的 policy.toml**——那样 §7.1 的初始建议生效：根内写入是
/// Allow，任意 shell 是 Ask，正好是恢复与 Cron 两组验收要的两种放行方式。
pub struct Home {
    dir: tempfile::TempDir,
}

impl Default for Home {
    fn default() -> Self {
        Home::new()
    }
}

impl Home {
    pub fn new() -> Home {
        Home::with_config(&home_config())
    }

    pub fn with_config(config: &str) -> Home {
        install_crypto();
        let dir = tempfile::tempdir().expect("临时数据目录");
        // **故意不写 policy.toml**：§7.1 的初始建议生效（根内写入 Allow、任意 shell
        // 是 Ask），正是无人值守那两组验收要的两种放行方式。
        write_home_with(dir.path(), config, DEFAULT_ENV, None);
        Home { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn workspace(&self) -> PathBuf {
        self.dir.path().join("workspaces")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.dir.path().join("sessions")
    }

    pub fn session_dir(&self, session: &SessionId) -> PathBuf {
        self.sessions_dir().join(session.as_str())
    }

    pub fn events_path(&self, session: &SessionId) -> PathBuf {
        self.session_dir(session).join("events.jsonl")
    }

    pub fn quarantine_path(&self, session: &SessionId) -> PathBuf {
        self.session_dir(session).join("events.jsonl.quarantine")
    }

    /// 起一台 Gateway。**同一个数据目录**，所以再调一次就是"重启"。
    pub async fn start(&self, llm: Arc<dyn LlmClient>) -> Gw {
        let sender = MemSender::new(ChannelPlatform::Telegram);
        self.start_with(llm, sender).await
    }

    /// 同上，但发送口自己给（要断言投递内容时用）。
    pub async fn start_with(&self, llm: Arc<dyn LlmClient>, sender: Arc<MemSender>) -> Gw {
        let running = start(ServiceOptions {
            home: Some(self.dir.path().to_path_buf()),
            listen: Some("127.0.0.1:0".into()),
            channels: vec![FixedFactory::sender_only(
                Arc::clone(&sender) as Arc<dyn ChannelSender>
            )],
            llm: Some(llm),
            embeddings: None,
        })
        .await
        .expect("Gateway 起得来");
        let base = running.base_url.clone();
        let token = running.state.token.clone();
        Gw {
            running,
            base,
            token,
            home: sender,
        }
    }

    /// 停机之后单独打开 state.db（Turso 对 db 文件持进程独占锁，所以**必须先 stop**）。
    pub async fn open_db(&self) -> komo_store::Db {
        komo_store::Db::connect(self.dir.path().join("state.db"))
            .await
            .expect("打开 state.db")
    }

    /// 这个 Session 的全部事件，按 seq。
    pub fn events(&self, session: &SessionId) -> Vec<komo_kernel::events::Event> {
        let text = std::fs::read_to_string(self.events_path(session)).unwrap_or_default();
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| komo_kernel::events::Event::from_line(line).ok())
            .collect()
    }

    pub fn event_types(&self, session: &SessionId) -> Vec<String> {
        self.events(session)
            .iter()
            .map(|event| event.type_name().to_string())
            .collect()
    }
}

/// 一台起着的 Gateway（`Home` 起的那一种）。
pub struct Gw {
    pub running: Running,
    base: String,
    token: String,
    /// home chat 的那一半——无人值守的投递全在这里。
    pub home: Arc<MemSender>,
}

impl Gw {
    pub fn state(&self) -> &Arc<GatewayState> {
        &self.running.state
    }

    pub fn dispatcher(&self) -> &Arc<crate::Dispatcher> {
        &self.running.dispatcher
    }

    pub fn channels(&self) -> &Arc<ChannelRegistry> {
        &self.state().channels
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub fn executor_id(&self) -> String {
        self.running.state.instance_id.clone()
    }

    /// 这台 Gateway 的地址与令牌——`tests/recovery` 拿它建一个真的 `KomoClient`
    /// （client 是 gateway 的 dev-dependency，不能从 lib 里用）。
    pub fn address(&self) -> (String, String) {
        (self.base.clone(), self.token.clone())
    }

    pub async fn get(&self, path: &str) -> (u16, String) {
        self.request(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: serde_json::Value) -> (u16, String) {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: serde_json::Value) -> (u16, String) {
        self.request(reqwest::Method::PATCH, path, Some(body)).await
    }

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        request(&self.base, &self.token, method, path, body).await
    }

    /// 开一个会话。
    pub async fn open_session(&self) -> SessionId {
        let (code, body) = self.post("/v1/sessions", serde_json::json!({})).await;
        assert_eq!(code, 200, "{body}");
        let summary: komo_kernel::protocol::http::SessionSummary =
            serde_json::from_str(&body).expect("会话");
        summary.session
    }

    /// 提交一条输入。
    pub async fn submit(&self, session: &SessionId, key: &str, text: &str) -> SubmitRunResponse {
        let (code, body) = self
            .post(
                &format!("/v1/sessions/{session}/runs"),
                serde_json::json!({ "request_key": key, "text": text }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        serde_json::from_str(&body).expect("提交")
    }

    pub async fn run_detail(&self, run: &RunId) -> RunDetail {
        self.try_run_detail(run).await.expect("读得到运行详情")
    }

    /// 同上，但读不出来时把**原话**带回来——一个中间损坏的会话该报损坏，不该是
    /// 一个 `None`（§8.5）。
    pub async fn try_run_detail(&self, run: &RunId) -> Result<RunDetail, String> {
        let (status, body) = self.get(&format!("/v1/runs/{run}")).await;
        if status != 200 {
            return Err(body);
        }
        serde_json::from_str(&body).map_err(|error| error.to_string())
    }

    /// 直接问账本这个 Run 现在什么状态（HTTP 那条路读的是同一行）。
    pub async fn db_status(&self, run: &RunId) -> RunStatus {
        komo_store::repos::runs::get(&self.state().db, run)
            .await
            .expect("读得到")
            .expect("有这个 Run")
            .status
    }

    pub async fn wait_db_status<F: Fn(RunStatus) -> bool>(
        &self,
        run: &RunId,
        done: F,
        what: &str,
    ) -> RunStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut last = RunStatus::Queued;
        while std::time::Instant::now() < deadline {
            last = self.db_status(run).await;
            if done(last) {
                return last;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("等了 20 秒 run {run} 还没到「{what}」，现在是 {last:?}");
    }

    pub async fn run_status(&self, run: &RunId) -> RunStatus {
        self.db_status(run).await
    }

    pub async fn wait_status<F: Fn(RunStatus) -> bool>(
        &self,
        run: &RunId,
        done: F,
        what: &str,
    ) -> RunDetail {
        self.wait_db_status(run, done, what).await;
        self.run_detail(run).await
    }

    pub async fn wait_terminal(&self, run: &RunId) -> RunDetail {
        self.wait_status(run, |status| status.is_terminal(), "终态")
            .await
    }

    pub async fn approvals(&self) -> Vec<ApprovalRecord> {
        self.state()
            .approval_repo
            .list_pending(None)
            .await
            .expect("读得出")
    }

    /// 等一条待处理的审批出现。
    pub async fn wait_approval(&self) -> ApprovalRecord {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if let Some(record) = self.approvals().await.into_iter().next() {
                return record;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("等了 20 秒也没有待处理的审批");
    }

    /// 走 HTTP 那条路做决定（四个界面共用的那一个接口，§13.5）。
    pub async fn decide(&self, approval: &ApprovalId, approved: bool) -> String {
        let (code, body) = self
            .post(
                &format!("/v1/approvals/{approval}/decision"),
                serde_json::json!({ "approved": approved }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        body
    }

    /// 造一条待处理的审批。
    pub async fn pending_approval_in(
        &self,
        session: &SessionId,
        run: Option<RunId>,
    ) -> ApprovalRecord {
        pending_approval_in(self.state(), session, run).await
    }

    /// 停机（放锁、删发现文件）。数据目录留着。
    pub async fn stop(self) {
        self.running.stop().await;
    }
}

// ---------------------------------------------------------------- 零碎

async fn request(
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, String) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("请求发得出去");
    let status = response.status().as_u16();
    (status, response.text().await.unwrap_or_default())
}

fn as_json(body: String) -> serde_json::Value {
    serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body))
}

/// 造一条待处理的审批（executor 真跑起来时做的就是这件事）。
pub async fn pending_approval_in(
    state: &Arc<GatewayState>,
    session: &SessionId,
    run: Option<RunId>,
) -> ApprovalRecord {
    let plan = komo_kernel::test_support::sample_plan("shell", session);
    state
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

/// reqwest 用 `rustls-no-provider`：provider 由 `komo` 的 `main` 装（§13.4）。测试进程里
/// 没有那个 `main`，所以这里装一次——**只装一次**。
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
    crate::dispatcher::inbound(platform, chat, sender, text, request_key, is_private)
}

/// 等一个条件成真（或超时）。
pub async fn eventually<F: Fn() -> bool>(what: &str, done: F) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("等了 5 秒还没有：{what}");
}
