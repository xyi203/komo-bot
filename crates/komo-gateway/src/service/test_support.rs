//! 集成测试用的一整台 Gateway：真数据目录、真 state.db、真 HTTP 监听、真 Dispatcher；
//! 模型与渠道是替身。
//!
//! 用 [`super::start`] 而不是另起一套装配——验收要证的是**这条启动路径**本身对，
//! 而不是一条只在测试里存在的近似路径。

use std::path::Path;
use std::sync::Arc;

use komo_kernel::policy::RuleTable;
use komo_kernel::protocol::config::{
    ChannelsConfig, ConfigSnapshot, MemoryConfig, PathsConfig, RetrievalConfig, StartOnly,
};
use komo_kernel::traits::LlmClient;
use komo_kernel::types::chat::ChannelPlatform;
use komo_kernel::types::model::ModelConfig;

use crate::channels::{ChannelSender, test_channel::MemChannel};

use super::{Running, ServiceOptions, start};

/// 一份能通过校验的最小 config.toml。
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

/// 配置里有一个 Telegram 渠道：操作者 111，home chat 111，允许的群 222。
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
pub fn sample_snapshot() -> ConfigSnapshot {
    let model = ModelConfig {
        provider: "openai_responses".into(),
        base_url: "https://llm.example.com/v1".into(),
        model: "gpt-test".into(),
        api_key_env: "KOMO_LLM_API_KEY".into(),
        effort: None,
        efforts: None,
        timeout_secs: 120,
    };
    ConfigSnapshot {
        start_only: StartOnly {
            data_dir: "/tmp/komo".into(),
            listen: "127.0.0.1:7777".into(),
            db_path: "/tmp/komo/state.db".into(),
            python_env_root: "/tmp/komo/python-envs".into(),
        },
        model: model.clone(),
        memory: MemoryConfig {
            enabled: false,
            model,
            embedding: None,
            retrieval: RetrievalConfig::default(),
        },
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
pub struct TestGateway {
    pub home: tempfile::TempDir,
    pub running: Running,
    pub channel: Arc<MemChannel>,
}

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
pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
