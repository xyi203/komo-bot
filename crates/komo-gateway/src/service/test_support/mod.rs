//! 集成测试用的一整台 Gateway：真数据目录、真 durable.db / aux.db、真 HTTP 监听、真 Dispatcher；
//! 模型与渠道是替身。
//!
//! 用 [`super::start`] 而不是另起一套装配——验收要证的是**这条启动路径**本身对，
//! 而不是一条只在测试里存在的近似路径。
//!
//! 这个模块里的 `TestGateway` 是**单元测试**（`service::tests`）用的那一份；四个集成
//! 测试目标（chat / cron / memory / recovery）共用的那一套在 [`harness`] 里——它们原先
//! 各自抄了一份"真 Gateway + MemSender + FakeLlm + 数据目录"，四份的差别只在各自特有的
//! 那几个助手上。

pub mod driver_stream;
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
use super::{Running, ServiceOptions, start};

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
    /// 默认配置：一个 Telegram 渠道，操作者 111；不注入模型（按配置造，用不到就不会被调用）。
    pub async fn start() -> TestGateway {
        TestGateway::boot(&telegram_config("111"), None, None).await
    }

    /// 配置自己给，不注入模型。
    pub async fn configured(config: &str) -> TestGateway {
        TestGateway::boot(config, None, None).await
    }

    /// 配置与模型替身自己给：`llm` 同时当 `LlmClient`（辅助调用）与 `ModelStream`（harness），
    /// 脚本与 `requests` / `turns()` 的观察口共用。
    pub async fn with(config: &str, llm: Arc<harness::FakeLlm>) -> TestGateway {
        TestGateway::boot(
            config,
            Some(Arc::clone(&llm) as Arc<dyn LlmClient>),
            Some(llm as Arc<dyn ModelStream>),
        )
        .await
    }

    async fn boot(
        config: &str,
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

    /// 等这条 run 的输入 submission 结算（`done` 或 `unanswered`，最多 20 秒）。
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

// ---------------------------------------------------------------- 测试工具注入口
//
// 审批删掉之后（§7，2026-10-11），测试没有"默认 strict → shell 要问"这条路把一个 Run
// 停住。停法是一件测试专用的工具——按数据目录索引的一张表，装配时 `build_tools` 查一次，
// **表是空的时候什么都不挂**，生产路径上的工具目录一个不多：
//
// - [`Gate`]（工具名 `gate`）：进去之后**一直等**，直到测试放行——一个 Run 停在 `running`
//   上占着名额（次序、取消、并发的测试要的就是它）。

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

/// 测试工具的计划：一条"没有安全恢复办法"的调用——重启之后 `started` 而无结果
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
