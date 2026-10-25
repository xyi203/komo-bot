//! 集成测试用的一整台 Gateway：真数据目录、真 state.db、真 HTTP 监听、真 Dispatcher；
//! 模型与渠道是替身。
//!
//! 用 [`super::start`] 而不是另起一套装配——验收要证的是**这条启动路径**本身对，
//! 而不是一条只在测试里存在的近似路径。
//!
//! 这个模块里的 `TestGateway` 是**单元测试**（`service::tests`）用的那一份；四个集成
//! 测试目标（chat / cron / memory / recovery）共用的那一套在 [`harness`] 里——它们原先
//! 各自抄了一份"真 Gateway + MemSender + FakeLlm + 数据目录"，四份的差别只在各自特有的
//! 那几个助手上。

pub mod harness;

#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
use komo_kernel::protocol::config::{
    ChannelsConfig, ConfigSnapshot, MemoryConfig, PathsConfig, RetrievalConfig, StartOnly,
    TypesafeConfig,
};
#[cfg(test)]
use komo_kernel::traits::LlmClient;
#[cfg(test)]
use komo_kernel::types::agent::{AgentConfig, AgentProfile};
#[cfg(test)]
use komo_kernel::types::chat::ChannelPlatform;
#[cfg(test)]
use komo_kernel::types::model::{CatalogModel, ModelCatalog, ModelConfig};

#[cfg(test)]
use crate::channels::{ChannelSender, test_channel::MemChannel};

#[cfg(test)]
use komo_runtime::llm::ModelStream;

#[cfg(test)]
use super::{Running, ServiceOptions, StorageMode, start};

/// 一份能通过校验的最小 config.toml。
#[cfg(test)]
pub fn config_toml(extra: &str) -> String {
    format!(
        r#"
default_agent = "assistant"

[agents.assistant]
[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

[memory]
enabled = false
{extra}
"#
    )
}

/// 配置里有一个 Telegram 渠道：操作者 111，home chat 111。
#[cfg(test)]
pub fn telegram_config(allow_from: &str) -> String {
    config_toml(&format!(
        r#"
[channels.telegram]
enabled = true
allow_from = [{allow_from}]
home_chat = 111
"#
    ))
}

/// 一份手搭的快照（纯函数的测试用它，不落文件）。
#[cfg(test)]
pub fn sample_snapshot() -> ConfigSnapshot {
    let model = ModelConfig {
        provider: "responses".into(),
        base_url: "https://llm.example.com/v1".into(),
        model: "gpt-test".into(),
        api_key_env: "KOMO_LLM_API_KEY".into(),
        auth: None,
        effort: None,
        efforts: None,
        timeout_secs: 120,
        context_window: None,
    };
    ConfigSnapshot {
        execution: Default::default(),
        compaction: Default::default(),
        start_only: StartOnly {
            data_dir: "/tmp/komo".into(),
            listen: "127.0.0.1:7777".into(),
            db_path: "/tmp/komo/state.db".into(),
            otlp_endpoint: None,
        },
        model_catalog: ModelCatalog {
            default: "main".into(),
            entries: std::collections::BTreeMap::from([(
                "main".into(),
                CatalogModel::Completion {
                    name: "main".into(),
                    model_provider: None,
                    config: model.clone(),
                },
            )]),
        },
        model: model.clone(),
        memory: MemoryConfig {
            enabled: false,
            model,
            embedding: None,
            retrieval: RetrievalConfig::default(),
            consolidate: Default::default(),
            extract_idle_secs: 600,
            max_observations: 8,
        },
        agent: AgentConfig {
            default_agent: "assistant".into(),
            agents: std::collections::BTreeMap::from([(
                "assistant".into(),
                AgentProfile::new("assistant"),
            )]),
        },
        typesafe: TypesafeConfig::default(),
        channels: ChannelsConfig::default(),
        mcp: Default::default(),
        paths: PathsConfig {
            sessions_dir: "/tmp/komo/sessions".into(),
            skill_dirs: vec![],
            runtime_dir: "/tmp/komo/runtime".into(),
            logs_dir: "/tmp/komo/logs".into(),
            workspaces_dir: "/tmp/komo/workspaces".into(),
        },
        credentials: Default::default(),
        loaded_at: time::macros::datetime!(2026-09-16 08:00:00 UTC),
        sources: vec![],
    }
}

/// 一台起着的 Gateway，外加一个内存渠道。
#[cfg(test)]
pub struct TestGateway {
    pub home: tempfile::TempDir,
    pub running: Running,
    pub channel: Arc<MemChannel>,
}

#[cfg(test)]
impl TestGateway {
    /// 默认配置：一个 Telegram 渠道，操作者 111。
    pub async fn start() -> TestGateway {
        TestGateway::with(&telegram_config("111"), None).await
    }

    /// 配置与模型后端自己给。
    pub async fn with(config: &str, llm: Option<Arc<dyn LlmClient>>) -> TestGateway {
        TestGateway::boot(config, StorageMode::Legacy, llm, None).await
    }

    /// 跑在新存储上（`docs/storage-durable-d5.md` §1）：`llm` 同时当 `LlmClient`（辅助调用）与
    /// `ModelStream`（harness），脚本与 `requests` / `turns()` 的观察口共用。
    pub async fn durable(config: &str, llm: Arc<harness::FakeLlm>) -> TestGateway {
        TestGateway::boot(
            config,
            StorageMode::Durable,
            Some(Arc::clone(&llm) as Arc<dyn LlmClient>),
            Some(llm as Arc<dyn ModelStream>),
        )
        .await
    }

    async fn boot(
        config: &str,
        storage: StorageMode,
        llm: Option<Arc<dyn LlmClient>>,
        stream: Option<Arc<dyn ModelStream>>,
    ) -> TestGateway {
        install_crypto();
        let home = tempfile::tempdir().expect("临时数据目录");
        write_home(home.path(), config);

        let running = start(ServiceOptions {
            home: Some(home.path().to_path_buf()),
            // 端口 0：内核挑一个空的，发现文件里写的是真实地址。
            listen: Some("127.0.0.1:0".into()),
            channels: Vec::new(),
            storage,
            llm,
            stream,
            embeddings: None,
            // 共享 skill 目录钉在临时数据目录上（§5.6）：测试不受这台机器上装过什么影响。
            shared_home: Some(home.path().to_path_buf()),
            telemetry: None,
        })
        .await
        .expect("Gateway 起得来");

        let channel = MemChannel::new(ChannelPlatform::Telegram);
        running
            .state
            .channels
            .register(Arc::clone(&channel) as Arc<dyn ChannelSender>);

        TestGateway {
            home,
            running,
            channel,
        }
    }

    /// 等这条 run 的输入 submission 结算（`done` 或 `unanswered`，最多 20 秒）。Durable 下用。
    pub async fn wait_run(
        &self,
        run: &komo_kernel::types::ids::RunId,
    ) -> komo_kernel::durable::SubmissionRecord {
        harness::wait_run(self.state(), run).await
    }

    pub fn state(&self) -> &Arc<super::state::GatewayState> {
        &self.running.state
    }

    pub fn dispatcher(&self) -> &Arc<crate::dispatcher::Dispatcher> {
        &self.running.dispatcher
    }

    pub fn base_url(&self) -> &str {
        &self.running.base_url
    }

    pub fn token(&self) -> &str {
        &self.running.state.token
    }

    /// 改 config.toml（热重载的测试用）。
    pub fn write_config(&self, config: &str) {
        let path = self.home.path().join("config.toml");
        std::fs::write(&path, config).expect("写 config.toml");
        // mtime 的粒度可能是秒；把时间推一下，`changed_since` 才看得出来。
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let _ = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_times(std::fs::FileTimes::new().set_modified(later)));
    }

    /// 带上 Bearer 的一个 HTTP 客户端调用。
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
}

#[cfg(test)]
fn write_home(home: &Path, config: &str) {
    std::fs::create_dir_all(home).expect("数据目录");
    std::fs::write(home.join("config.toml"), config).expect("写 config.toml");
    std::fs::write(
        home.join(".env"),
        "KOMO_LLM_API_KEY=test-key\nTELEGRAM_BOT_TOKEN=test-bot-token\n",
    )
    .expect("写 .env");
}

/// reqwest 用 `rustls-no-provider`：provider 由 `komo` 的 `main` 装（§13.4）。测试进程
/// 里没有那个 `main`，所以这里装一次——**只装一次**，装第二次会失败。
#[cfg(test)]
pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

// ---------------------------------------------------------------- 故障注入口
//
// W5「恢复故障注入验收」（§14）要的是"在 §8.5 的步骤之间停下"：一台**真** Gateway
// 跑到某一步，账本的那一步半途而废，然后同一个数据目录重启，断言恢复扫描做了
// §8.4 / §14 那两张表右列的事。
//
// 注入口做成一张按数据目录索引的表，而不是 `ServiceOptions` 上的一个字段：字段会让
// 每一处 `ServiceOptions { .. }` 字面量在 feature 打开时编译不过（bin 的 `main` 也
// 在内），而这里只要装配那一行读一次表。**表是空的时候什么都不发生**，生产路径上
// 连一次哈希查找都不做（`is_empty` 就退）。

/// 把装配出来的 `Arc<dyn Ledger>` 换成另一个（通常是包一层故障装饰器）。
pub type LedgerWrap = std::sync::Arc<
    dyn Fn(
            std::sync::Arc<dyn komo_kernel::traits::Ledger>,
        ) -> std::sync::Arc<dyn komo_kernel::traits::Ledger>
        + Send
        + Sync,
>;

type WrapTable = std::sync::Mutex<Vec<(std::path::PathBuf, LedgerWrap)>>;

fn wraps() -> &'static WrapTable {
    static TABLE: std::sync::OnceLock<WrapTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// 下一次在 `home` 起 Gateway 时，用 `wrap` 包住账本。同一个 `home` 再装一次覆盖前一次。
pub fn install_ledger_wrap(home: &std::path::Path, wrap: LedgerWrap) {
    let mut table = wraps().lock().expect("故障注入表");
    table.retain(|(path, _)| path != home);
    table.push((home.to_path_buf(), wrap));
}

/// 撤掉 `home` 上的注入（重启一台"好的"之前调它）。
pub fn clear_ledger_wrap(home: &std::path::Path) {
    wraps()
        .lock()
        .expect("故障注入表")
        .retain(|(p, _)| p != home);
}

/// 装配时的那一次查表。**没装过就原样返回**。
pub(crate) fn wrap_ledger(
    home: &std::path::Path,
    ledger: std::sync::Arc<dyn komo_kernel::traits::Ledger>,
) -> std::sync::Arc<dyn komo_kernel::traits::Ledger> {
    let found = {
        let table = wraps().lock().expect("故障注入表");
        if table.is_empty() {
            return ledger;
        }
        table
            .iter()
            .find(|(path, _)| path == home)
            .map(|(_, wrap)| std::sync::Arc::clone(wrap))
    };
    match found {
        Some(wrap) => wrap(ledger),
        None => ledger,
    }
}

// ---------------------------------------------------------------- 缓存时钟注入口
//
// 提示缓存冷没冷（`compaction::cache_cold`）按"现在"与上一次请求的时间差算，而
// 缓存存活时间是五分钟：集成测试不能真等。同故障注入口一个办法——按数据目录索引的一张
// 表，只拨**这一处判断**用的时钟（租约等别处的"现在"不动）。表是空的时候什么都
// 不发生。

type SkewTable = std::sync::Mutex<Vec<(std::path::PathBuf, std::time::Duration)>>;

fn skews() -> &'static SkewTable {
    static TABLE: std::sync::OnceLock<SkewTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// 下一次在 `home` 起 Gateway 时，判缓存冷没冷的"现在"往后拨 `skew`。
pub fn install_cache_clock_skew(home: &std::path::Path, skew: std::time::Duration) {
    let mut table = skews().lock().expect("缓存时钟注入表");
    table.retain(|(path, _)| path != home);
    table.push((home.to_path_buf(), skew));
}

struct SkewedClock {
    inner: std::sync::Arc<dyn komo_kernel::traits::Clock>,
    skew: std::time::Duration,
}

impl komo_kernel::traits::Clock for SkewedClock {
    fn now(&self) -> time::OffsetDateTime {
        self.inner.now() + self.skew
    }
}

/// 装配时的那一次查表。**没装过就原样返回**。
pub(crate) fn cache_clock(
    home: &std::path::Path,
    clock: std::sync::Arc<dyn komo_kernel::traits::Clock>,
) -> std::sync::Arc<dyn komo_kernel::traits::Clock> {
    let skew = skews()
        .lock()
        .expect("缓存时钟注入表")
        .iter()
        .find(|(path, _)| path == home)
        .map(|(_, skew)| *skew);
    match skew {
        Some(skew) => std::sync::Arc::new(SkewedClock { inner: clock, skew }),
        None => clock,
    }
}

// ---------------------------------------------------------------- 测试工具注入口
//
// 审批删掉之后（§7，2026-10-11），测试再没有"默认 strict → shell 要问"这条路把一个 Run
// 停住。停法换成两件测试专用的工具，同前两个注入口一个办法——按数据目录索引的一张表，
// 装配时 `build_tools` 查一次，**表是空的时候什么都不挂**，生产路径上的工具目录一个不多：
//
// - [`Gate`]（工具名 `gate`）：进去之后**一直等**，直到测试放行——一个 Run 停在 `running`
//   上占着名额（次序、取消、并发的测试要的就是它）。
// - [`Unsure`]（工具名 `unsure`）：一调就答"结果不明"，正文带 `TEST_PARK` 前缀——Run 当场
//   停在 `waiting + intervention`（升级前的 `verify` 形状）上。生产里结果不明交给模型判断
//   （§8.6），这条停法只在 `test-support` 下有；测的是清单、投递、答复与依赖等待。

type ToolTable = std::sync::Mutex<
    Vec<(
        std::path::PathBuf,
        Vec<std::sync::Arc<dyn komo_kernel::traits::Tool>>,
    )>,
>;

fn tool_table() -> &'static ToolTable {
    static TABLE: std::sync::OnceLock<ToolTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn legacy_table() -> &'static std::sync::Mutex<Vec<std::path::PathBuf>> {
    static TABLE: std::sync::OnceLock<std::sync::Mutex<Vec<std::path::PathBuf>>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// 在 `home` 起 Gateway 时照生产那样，把停在结果不明上的旧 Run 放回队列交给模型。
///
/// 测试构建里**默认不放**：[`Unsure`] 停住的 Run 与升级前的 `verify` 是同一个形状，而
/// "停着的 Run 扛过重启、答复之后接着跑"那一批测试要它重启后还停着。专测迁移的那条打开它。
pub fn requeue_legacy_verify_on_start(home: &std::path::Path) {
    let mut table = legacy_table().lock().expect("迁移开关表");
    if !table.iter().any(|path| path == home) {
        table.push(home.to_path_buf());
    }
}

pub(crate) fn requeues_legacy_verify(home: &std::path::Path) -> bool {
    legacy_table()
        .lock()
        .expect("迁移开关表")
        .iter()
        .any(|path| path == home)
}

/// 下一次在 `home` 起 Gateway 时，多挂这几个工具。同一个 `home` 再装一次覆盖前一次。
pub fn install_test_tools(
    home: &std::path::Path,
    tools: Vec<std::sync::Arc<dyn komo_kernel::traits::Tool>>,
) {
    let mut table = tool_table().lock().expect("测试工具表");
    table.retain(|(path, _)| path != home);
    table.push((home.to_path_buf(), tools));
}

/// 装配时的那一次查表。**没装过就是空的**。
pub(crate) fn test_tools(
    home: &std::path::Path,
) -> Vec<std::sync::Arc<dyn komo_kernel::traits::Tool>> {
    tool_table()
        .lock()
        .expect("测试工具表")
        .iter()
        .find(|(path, _)| path == home)
        .map(|(_, tools)| tools.clone())
        .unwrap_or_default()
}

/// 两件测试工具共用的计划：一条"没有安全恢复办法"的调用——重启之后 `started` 而无结果
/// 的它只能停下来问人（§8.6），与一次真的外部写入同一个待遇。
fn test_plan(
    name: &str,
    args: serde_json::Value,
    ctx: &komo_kernel::types::tool::ToolContext,
) -> komo_kernel::types::plan::ExecutionPlan {
    use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode};
    ExecutionPlan {
        operation_id: komo_kernel::types::ids::OperationId::from_raw(format!(
            "op-{name}-{}",
            ctx.call
        )),
        source: ctx.source.clone(),
        tool: name.into(),
        operation: Operation::McpCall {
            server: "test".into(),
            tool: name.into(),
            read_only: false,
        },
        run: Some(ctx.run.clone()),
        tool_call: Some(ctx.call.clone()),
        args,
        cwd: Some(ctx.cwd.clone()),
        targets: vec![],
        versions: PlanVersions::default(),
        recovery: RecoveryMode::NoSafeRecovery,
        then_run: None,
    }
}

fn test_definition(name: &str, description: &str) -> komo_kernel::types::tool::ToolDefinition {
    komo_kernel::types::tool::ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters: serde_json::json!({ "type": "object" }),
        doc: None,
    }
}

/// 一道门：`gate` 调用进去就等，直到 [`Gate::release`]。放行之后一直开着。
///
/// 等的时候认取消（`/cancel`、停机）：那时答 `Cancelled`，不让一次取消卡在测试的门上。
pub struct Gate {
    open: tokio::sync::watch::Sender<bool>,
    entered: tokio::sync::watch::Sender<usize>,
}

impl Gate {
    pub fn new() -> std::sync::Arc<Gate> {
        std::sync::Arc::new(Gate {
            open: tokio::sync::watch::Sender::new(false),
            entered: tokio::sync::watch::Sender::new(0),
        })
    }

    /// 当成工具挂上去（[`install_test_tools`]）。
    pub fn tool(self: &std::sync::Arc<Self>) -> std::sync::Arc<dyn komo_kernel::traits::Tool> {
        std::sync::Arc::new(GateTool(std::sync::Arc::clone(self)))
    }

    /// 放行：等着的、以后来的，都直接过去。
    pub fn release(&self) {
        self.open.send_replace(true);
    }

    /// 进过门几次（= `gate` 真的开始执行了几次）。
    pub fn entered(&self) -> usize {
        *self.entered.borrow()
    }

    /// 等到至少有 `n` 次进门（最多 20 秒）。
    pub async fn wait_entered(&self, n: usize) {
        let mut entered = self.entered.subscribe();
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            entered.wait_for(|count| *count >= n),
        )
        .await
        .unwrap_or_else(|_| panic!("等了 20 秒也没有第 {n} 次进门"))
        .expect("门还在");
    }
}

struct GateTool(std::sync::Arc<Gate>);

#[async_trait::async_trait]
impl komo_kernel::traits::Tool for GateTool {
    fn definition(&self) -> komo_kernel::types::tool::ToolDefinition {
        test_definition("gate", "测试用：等测试放行再返回")
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &komo_kernel::types::tool::ToolContext,
    ) -> Result<komo_kernel::types::plan::ExecutionPlan, komo_kernel::types::tool::ToolError> {
        Ok(test_plan("gate", args, ctx))
    }

    async fn execute(
        &self,
        _plan: komo_kernel::types::plan::ExecutionPlan,
        ctx: &komo_kernel::types::tool::ToolContext,
        _sink: &mut dyn komo_kernel::traits::OutputWriter,
    ) -> Result<komo_kernel::types::tool::ToolOutput, komo_kernel::types::tool::ToolError> {
        self.0.entered.send_modify(|count| *count += 1);
        let mut open = self.0.open.subscribe();
        // `CancelToken` 只能问不能等：一小拍一小拍地看。
        loop {
            if ctx.cancel.is_cancelled() {
                return Err(komo_kernel::types::tool::ToolError::Cancelled);
            }
            let tick = std::time::Duration::from_millis(20);
            if tokio::time::timeout(tick, open.wait_for(|open| *open))
                .await
                .is_ok()
            {
                break;
            }
        }
        Ok(komo_kernel::types::tool::ToolOutput {
            status: komo_kernel::types::refs::ToolResultStatus::Completed,
            result: serde_json::json!({ "gate": "open" }),
            exit_code: Some(0),
            artifacts: vec![],
            images: vec![],
            preview: Some("门开了".into()),
        })
    }
}

/// 一调就"结果不明"的工具：Run 停在 `verify` 上等人核对（§7.5）。
pub struct Unsure {
    ran: std::sync::atomic::AtomicUsize,
}

impl Unsure {
    pub fn new() -> std::sync::Arc<Unsure> {
        std::sync::Arc::new(Unsure {
            ran: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn tool(self: &std::sync::Arc<Self>) -> std::sync::Arc<dyn komo_kernel::traits::Tool> {
        std::sync::Arc::new(UnsureTool(std::sync::Arc::clone(self)))
    }

    /// 真的执行了几次——"核对之后不重跑"的判据。
    pub fn ran(&self) -> usize {
        self.ran.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct UnsureTool(std::sync::Arc<Unsure>);

#[async_trait::async_trait]
impl komo_kernel::traits::Tool for UnsureTool {
    fn definition(&self) -> komo_kernel::types::tool::ToolDefinition {
        test_definition("unsure", "测试用：一调就结果不明")
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &komo_kernel::types::tool::ToolContext,
    ) -> Result<komo_kernel::types::plan::ExecutionPlan, komo_kernel::types::tool::ToolError> {
        Ok(test_plan("unsure", args, ctx))
    }

    async fn execute(
        &self,
        _plan: komo_kernel::types::plan::ExecutionPlan,
        _ctx: &komo_kernel::types::tool::ToolContext,
        _sink: &mut dyn komo_kernel::traits::OutputWriter,
    ) -> Result<komo_kernel::types::tool::ToolOutput, komo_kernel::types::tool::ToolError> {
        self.0.ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(komo_kernel::types::tool::ToolError::Uncertain {
            message: format!(
                "{}测试：远端没有回话，不知道做没做成",
                komo_runtime::executor::TEST_PARK
            ),
        })
    }
}

/// 2026-10-16 删掉之前的 `delegate`：计划照当时的 `DelegateTool::prepare` 写（`task` /
/// `output_schema` / `schema_mode` → `Operation::Delegate`）。生产目录里已经没有它，测试装上
/// 它在真实 Gateway 上重走旧委派调用的收尾路径（子 Run、领取豁免、父按 `run_end` 收尾，§4）。
pub fn legacy_delegate() -> std::sync::Arc<dyn komo_kernel::traits::Tool> {
    std::sync::Arc::new(LegacyDelegateTool)
}

struct LegacyDelegateTool;

#[async_trait::async_trait]
impl komo_kernel::traits::Tool for LegacyDelegateTool {
    fn definition(&self) -> komo_kernel::types::tool::ToolDefinition {
        test_definition(komo_agent::DELEGATE_TOOL, "测试用：升级前的同步委派")
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &komo_kernel::types::tool::ToolContext,
    ) -> Result<komo_kernel::types::plan::ExecutionPlan, komo_kernel::types::tool::ToolError> {
        use komo_kernel::types::delegate::DelegateSpec;
        use komo_kernel::types::plan::{Operation, RecoveryMode};
        let mut spec = DelegateSpec::new(
            ctx.run.clone(),
            ctx.call.clone(),
            args["task"].as_str().unwrap_or_default(),
        );
        if let Some(schema) = args.get("output_schema") {
            let mode = args
                .get("schema_mode")
                .and_then(|mode| serde_json::from_value(mode.clone()).ok())
                .unwrap_or_default();
            spec = spec.with_contract(schema.clone(), mode);
        }
        Ok(komo_kernel::types::plan::ExecutionPlan {
            operation: Operation::Delegate { spec },
            recovery: RecoveryMode::VerifyTarget,
            ..test_plan(komo_agent::DELEGATE_TOOL, args, ctx)
        })
    }

    async fn execute(
        &self,
        _plan: komo_kernel::types::plan::ExecutionPlan,
        _ctx: &komo_kernel::types::tool::ToolContext,
        _sink: &mut dyn komo_kernel::traits::OutputWriter,
    ) -> Result<komo_kernel::types::tool::ToolOutput, komo_kernel::types::tool::ToolError> {
        Err(komo_kernel::types::tool::ToolError::Failed {
            message: "delegate 不由工具执行".into(),
        })
    }
}
