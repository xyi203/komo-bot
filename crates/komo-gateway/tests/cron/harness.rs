//! W6 Cron 验收的脚手架：一台**真** Gateway（真数据目录、真 state.db、真监听、真
//! `HomeNotifier`），加一个内存渠道当 home chat，加一个脚本化的模型。
//!
//! 与 `tests/chat/harness.rs` 和 `tests/recovery/harness.rs` 形状相同——那两份是
//! 另外两个测试二进制的 `mod`，集成测试之间拿不到彼此的模块，所以这里是第三份精简
//! 重写，**不改任何 src**（见报告"需要编排者做"）。
//!
//! 与另外两份的差别只有一处：这里的 config.toml **配了 home chat**，因为 Cron 没有
//! 来源会话，「投递到 Run 的来源会话与 home chat」（§7.4）在这一侧只剩 home chat。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use komo_gateway::channels::{
    BuiltChannel, ChannelFactory, ChannelRegistry, ChannelSender, SendOutcome,
};
use komo_gateway::service::state::GatewayState;
use komo_gateway::service::{Running, ServiceOptions, start};
use komo_kernel::cron::CronJob;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::traits::{Channel, ChannelError, DeliverError, Inbound, LlmClient, TurnDriver};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, Outbound};
use komo_kernel::types::ids::RunId;
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::status::RunStatus;
use komo_kernel::types::turn::{
    AcceptInput, LlmError, ProviderToolCall, Round, RoundInput, TurnRequest,
};
use komo_runtime::config::Secrets;

// ---------------------------------------------------------------- 配置

/// **配了 home chat 的** Telegram 渠道：Cron 的审批请求只有这一个出口。
pub fn config_toml() -> String {
    r#"
[model]
provider = "openai_responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[memory]
enabled = false

[channels.telegram]
enabled = true
allow_from = [111]
home_chat = 111
"#
    .to_string()
}

const DEFAULT_ENV: &str = "KOMO_LLM_API_KEY=test-key\nTELEGRAM_BOT_TOKEN=test-bot-token\n";

// ---------------------------------------------------------------- 渠道替身

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    pub peer: ChannelPeer,
    pub outbound: Outbound,
}

/// 一个内存发送口：`send` 记在 `sent` 里。
#[derive(Debug)]
pub struct MemSender {
    platform: ChannelPlatform,
    sent: Mutex<Vec<SentMessage>>,
}

impl MemSender {
    pub fn new(platform: ChannelPlatform) -> Arc<Self> {
        Arc::new(MemSender {
            platform,
            sent: Mutex::new(Vec::new()),
        })
    }

    pub fn sent(&self) -> Vec<SentMessage> {
        self.sent.lock().expect("发送记录").clone()
    }

    /// 送出去的审批请求。
    pub fn approvals(&self) -> Vec<komo_kernel::types::chat::ApprovalPresentation> {
        self.sent()
            .into_iter()
            .filter_map(|m| match m.outbound {
                Outbound::ApprovalRequest(p) => Some(*p),
                _ => None,
            })
            .collect()
    }

    pub fn texts(&self) -> Vec<String> {
        self.sent()
            .into_iter()
            .filter_map(|m| match m.outbound {
                Outbound::Text { text } => Some(text),
                Outbound::RunFinished { summary, .. } => Some(summary),
                Outbound::NeedsAttention { reason, .. } => Some(reason),
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl ChannelSender for MemSender {
    fn platform(&self) -> ChannelPlatform {
        self.platform
    }

    async fn send(&self, peer: &ChannelPeer, msg: Outbound) -> Result<SendOutcome, DeliverError> {
        self.sent.lock().expect("发送记录").push(SentMessage {
            peer: peer.clone(),
            outbound: msg,
        });
        Ok(SendOutcome::Sent)
    }
}

/// 什么都不收的 `Channel`：入站由测试自己驱动。
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

pub struct FixedFactory {
    platform: ChannelPlatform,
    sender: Arc<dyn ChannelSender>,
}

impl FixedFactory {
    pub fn sender_only(sender: Arc<dyn ChannelSender>) -> Arc<Self> {
        Arc::new(FixedFactory {
            platform: sender.platform(),
            sender,
        })
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
            channel: Arc::new(IdleChannel),
            sender: Arc::clone(&self.sender),
        }))
    }
}

// ---------------------------------------------------------------- 数据目录 + Gateway

/// 一个真实的数据目录。**故意不写 policy.toml**——那样 §7.1 的初始建议生效：任意
/// shell 是 Ask，正是"新增危险操作暂停等待审批"要的那一条。
pub struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    pub fn new() -> Home {
        install_crypto();
        let dir = tempfile::tempdir().expect("临时数据目录");
        std::fs::write(dir.path().join("config.toml"), config_toml()).expect("写 config.toml");
        std::fs::write(dir.path().join(".env"), DEFAULT_ENV).expect("写 .env");
        std::fs::create_dir_all(dir.path().join("workspaces")).expect("工作目录");
        Home { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn workspace(&self) -> PathBuf {
        self.dir.path().join("workspaces")
    }

    /// 起一台 Gateway。**同一个数据目录**，所以再调一次就是"重启"。
    pub async fn start(&self, llm: Arc<dyn LlmClient>) -> Gw {
        let sender = MemSender::new(ChannelPlatform::Telegram);
        self.start_with(llm, sender).await
    }

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
}

pub struct Gw {
    pub running: Running,
    base: String,
    token: String,
    /// home chat 的那一半——Cron 的投递全在这里。
    pub home: Arc<MemSender>,
}

impl Gw {
    pub fn state(&self) -> &Arc<GatewayState> {
        &self.running.state
    }

    pub fn dispatcher(&self) -> &Arc<komo_gateway::Dispatcher> {
        &self.running.dispatcher
    }

    pub fn channels(&self) -> &Arc<ChannelRegistry> {
        &self.running.state.channels
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
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("请求发得出去");
        let status = response.status().as_u16();
        (status, response.text().await.unwrap_or_default())
    }

    // ---- cron ----

    /// 造一个 Job，`schedule` 按 UTC。
    pub async fn add_job(&self, body: serde_json::Value) -> CronJob {
        let (status, text) = self.post("/v1/cron", body).await;
        assert_eq!(status, 200, "{text}");
        serde_json::from_str(&text).expect("cron job")
    }

    pub async fn cron_list(&self) -> komo_kernel::protocol::http::CronListResponse {
        let (status, text) = self.get("/v1/cron").await;
        assert_eq!(status, 200, "{text}");
        serde_json::from_str(&text).expect("cron list")
    }

    pub async fn job(&self, id: &komo_kernel::types::ids::CronJobId) -> CronJob {
        self.cron_list()
            .await
            .jobs
            .into_iter()
            .find(|job| &job.id == id)
            .expect("这个 Job 还在")
    }

    pub async fn firings(
        &self,
        id: &komo_kernel::types::ids::CronJobId,
    ) -> Vec<komo_kernel::cron::CronFiring> {
        self.state().cron.firings(id, 50).await.expect("读得出")
    }

    /// 把一个 Job 的槽位拨到过去，再扫一轮——"到点了"在测试里就是这个意思。
    ///
    /// 走 `advance` 而不是 `put`：**推进槽位不是定义变更**，用 `put` 会让版本 +1，
    /// 把「授权按版本绑」那几条测试的前提悄悄改掉（§10）。
    pub async fn make_due(&self, id: &komo_kernel::types::ids::CronJobId) {
        let job = self.job(id).await;
        self.state()
            .cron
            .advance(
                id,
                Some(time::OffsetDateTime::now_utc() - time::Duration::seconds(1)),
                job.status,
                None,
            )
            .await
            .expect("拨得动");
    }

    /// 扫一轮 Cron，并**像生产那样**给投出去的每一次触发挂上盯梢
    /// （`service::spawn_background` 里那两行）。
    pub async fn tick(&self) -> komo_runtime::scheduler::CronTick {
        let tick = self.state().cron_scheduler().tick().await.expect("扫得动");
        self.state().watch_fired(&tick.fired).await;
        self.state().waker().wake();
        tick
    }

    // ---- 观察 ----

    pub async fn run_status(&self, run: &RunId) -> RunStatus {
        komo_store::repos::runs::get(&self.state().db, run)
            .await
            .expect("读得到")
            .expect("有这一行")
            .status
    }

    pub async fn wait_status(
        &self,
        run: &RunId,
        want: impl Fn(RunStatus) -> bool,
        what: &str,
    ) -> RunStatus {
        for _ in 0..400 {
            let status = self.run_status(run).await;
            if want(status) {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!(
            "等了 20 秒 run {run} 还没到「{what}」，现在是 {:?}",
            self.run_status(run).await
        );
    }

    pub async fn approvals(&self) -> Vec<komo_kernel::protocol::http::ApprovalRecord> {
        let (status, body) = self.get("/v1/approvals").await;
        assert_eq!(status, 200, "{body}");
        let list: komo_kernel::protocol::http::ApprovalListResponse =
            serde_json::from_str(&body).expect("审批列表");
        list.approvals
    }

    pub async fn wait_approval(&self) -> komo_kernel::protocol::http::ApprovalRecord {
        for _ in 0..400 {
            if let Some(record) = self.approvals().await.into_iter().next() {
                return record;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("等了 20 秒也没有待处理审批");
    }

    /// 在聊天里答一条审批（`/approve <short_id> [run|cron]` 那条路，§11.3）。
    pub async fn approve_in_chat(&self, short_id: &str, scope: &str) -> String {
        let text = if scope.is_empty() {
            format!("/approve {short_id}")
        } else {
            format!("/approve {short_id} {scope}")
        };
        let ack = self
            .dispatcher()
            .handle(komo_gateway::dispatcher::inbound(
                ChannelPlatform::Telegram,
                "111",
                "111",
                &text,
                &format!("telegram:{}", uuid_like()),
                true,
            ))
            .await
            .expect("Dispatcher 处理");
        format!("{ack:?}")
    }

    pub async fn stop(self) {
        self.running.stop().await;
    }
}

fn uuid_like() -> String {
    static N: AtomicUsize = AtomicUsize::new(1);
    format!("k{}", N.fetch_add(1, Ordering::SeqCst))
}

/// reqwest 用 `rustls-no-provider`：provider 由 `komo` 的 `main` 装（§13.4）。
pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub async fn eventually<F: Fn() -> bool>(what: &str, done: F) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("等了 10 秒还没有：{what}");
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

/// 一条 shell 调用：往计数文件里追加一行。**断言的是次数**（§14 最后一段）。
pub fn shell_append(
    n: u32,
    provider_id: &str,
    counter: &Path,
    what: &str,
) -> Result<Round, LlmError> {
    call_round(
        n,
        provider_id,
        "shell",
        serde_json::json!({
            "command": format!("printf '{what}\\n' >> {}", counter.display())
        }),
    )
}

/// 脚本化的 `LlmClient`：每次 `begin_turn` 取一段脚本，用完之后给一句收尾。
pub struct FakeLlm {
    scripts: Mutex<std::collections::VecDeque<Vec<Result<Round, LlmError>>>>,
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

    /// **每一段**都是这个脚本。续跑的那一段也会拿到它——这正是
    /// "同一类动作再来一次"要的（③）。
    pub fn always(rounds: Vec<Result<Round, LlmError>>) -> Arc<FakeLlm> {
        let llm = FakeLlm::new(vec![]);
        *llm.fallback.lock().expect("脚本模型") = rounds;
        llm
    }

    pub fn turns(&self) -> usize {
        self.turns.load(Ordering::SeqCst)
    }

    /// 这些回合里模型看到的系统提示与模型身份。
    pub fn models(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("脚本模型")
            .iter()
            .map(|r| r.model.model.clone())
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
        }))
    }
}

struct FakeDriver {
    rounds: std::collections::VecDeque<Result<Round, LlmError>>,
}

#[async_trait]
impl TurnDriver for FakeDriver {
    async fn next(&mut self, _input: RoundInput) -> Result<Round, LlmError> {
        match self.rounds.pop_front() {
            Some(round) => round,
            None => text_round(99, "（脚本演完了，收尾）"),
        }
    }

    fn usage(&self) -> TokenUsage {
        TokenUsage::default()
    }
}

/// 计数文件里有几行 = 副作用发生了几次。
pub fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// 让 `AcceptInput` 在这个文件里"用得上"，省掉一条 unused import。
#[allow(unused)]
fn _touch(_: AcceptInput) {}
