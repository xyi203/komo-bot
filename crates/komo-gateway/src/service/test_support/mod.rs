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
use komo_kernel::policy::RuleTable;
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

/// 配置里有一个 Telegram 渠道：操作者 111，home chat 111，允许的群 222。
#[cfg(test)]
pub fn telegram_config(allow_from: &str) -> String {
    config_toml(&format!(
        r#"
[channels.telegram]
enabled = true
allow_from = [{allow_from}]
home_chat = 111
groups = [222]
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
        effort: None,
        efforts: None,
        timeout_secs: 120,
    };
    ConfigSnapshot {
        execution: Default::default(),
        start_only: StartOnly {
            data_dir: "/tmp/komo".into(),
            listen: "127.0.0.1:7777".into(),
            db_path: "/tmp/komo/state.db".into(),
            python_env_root: "/tmp/komo/python-envs".into(),
        },
        model_catalog: ModelCatalog {
            default: "main".into(),
            entries: std::collections::BTreeMap::from([(
                "main".into(),
                CatalogModel::Completion {
                    name: "main".into(),
                    model_provider: None,
                    context_window: None,
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
        policy: RuleTable::initial(),
        paths: PathsConfig {
            sessions_dir: "/tmp/komo/sessions".into(),
            toolbox_dir: "/tmp/komo/toolbox".into(),
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
        install_crypto();
        let home = tempfile::tempdir().expect("临时数据目录");
        write_home(home.path(), config);

        let running = start(ServiceOptions {
            home: Some(home.path().to_path_buf()),
            // 端口 0：内核挑一个空的，发现文件里写的是真实地址。
            listen: Some("127.0.0.1:0".into()),
            channels: Vec::new(),
            llm,
            embeddings: None,
            // 共享 skill 目录钉在临时数据目录上（§5.6）：测试不受这台机器上装过什么影响。
            shared_home: Some(home.path().to_path_buf()),
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
    std::fs::write(home.join("policy.toml"), "rules = []\n").expect("写 policy.toml");
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
