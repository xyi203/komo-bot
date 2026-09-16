//! 配置测试的公用夹具。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use komo_kernel::policy::RuleTable;
use komo_kernel::protocol::config::{
    ChannelConfig, ChannelsConfig, ConfigSnapshot, MemoryConfig, PathsConfig, RetrievalConfig,
    StartOnly,
};
use komo_kernel::types::chat::PeerId;
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::model::{Effort, EmbeddingConfig, ModelConfig};

use super::{ConfigHolder, LoadOptions, Sources};

/// `.env` 里那个值——泄漏测试拿它当探针。
pub const SECRET_VALUE: &str = "sk-do-not-log-this-0xdeadbeef";

pub fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

/// 一个真实的数据目录：config.toml + .env + policy.toml。
pub struct Fixture {
    home: tempfile::TempDir,
}

impl Fixture {
    pub fn valid() -> Self {
        let home = tempfile::tempdir().unwrap();
        let sources = Sources::under(home.path());
        write(&sources.config, &Self::config_text("chat-a", "medium"));
        write(&sources.env, &Self::env_text());
        write(&sources.policy, Self::POLICY_TEXT);
        Fixture { home }
    }

    /// 只有 config.toml 与 .env，没有 policy.toml——这时用 §7.1 的初始建议。
    pub fn without_policy() -> Self {
        let fixture = Self::valid();
        std::fs::remove_file(&fixture.sources().policy).unwrap();
        fixture
    }

    pub fn path(&self) -> &Path {
        self.home.path()
    }

    pub fn sources(&self) -> Sources {
        Sources::under(self.home.path())
    }

    pub fn options(&self) -> LoadOptions {
        LoadOptions::at(self.home.path())
    }

    pub fn holder(&self) -> ConfigHolder {
        ConfigHolder::load(&self.options()).unwrap()
    }

    pub fn config_text(model: &str, effort: &str) -> String {
        format!(
            r#"
[model]
provider = "openai_responses"
base_url = "https://llm.example.com/v1"
model = "{model}"
api_key_env = "KOMO_LLM_API_KEY"
effort = "{effort}"

[memory]
enabled = true

[memory.model]
provider = "openai_responses"
base_url = "https://memory-llm.example.com/v1"
model = "memory-a"
api_key_env = "KOMO_MEMORY_API_KEY"
effort = "low"

[memory.embedding]
provider = "openai_compatible"
base_url = "https://embedding.example.com/v1"
model = "embed-a"
api_key_env = "KOMO_EMBEDDING_API_KEY"
dimensions = 1024

[memory.retrieval]
mode = "hybrid"
candidate_limit = 40
top_k = 8
max_tokens = 1500

[channels.feishu]
enabled = true
allow_from = ["ou_operator"]
home_chat = "oc_home"
"#
        )
    }

    pub fn env_text() -> String {
        format!(
            "KOMO_LLM_API_KEY={SECRET_VALUE}\n\
             KOMO_MEMORY_API_KEY={SECRET_VALUE}-memory\n\
             KOMO_EMBEDDING_API_KEY={SECRET_VALUE}-embed\n\
             FEISHU_APP_ID=cli_fixture\n\
             FEISHU_APP_SECRET={SECRET_VALUE}-feishu\n"
        )
    }

    pub const POLICY_TEXT: &'static str = r#"
default = "ask"

[[rules]]
id = "deny-policy-change"
effect = "deny"
reason = "权限扩大只能走操作者的配置流程"

[rules.matcher]
operations = ["policy_change"]

[[rules]]
id = "allow-read-in-roots"
effect = "allow"
reason = "已授权范围内读取普通文件"

[rules.matcher]
operations = ["read_file"]

[rules.matcher.paths]
kind = "within_roots"
writable = false
"#;
}

fn model(provider: &str, name: &str, key_env: &str, effort: Option<&str>) -> ModelConfig {
    ModelConfig {
        provider: provider.into(),
        base_url: "https://llm.example.com/v1".into(),
        model: name.into(),
        api_key_env: key_env.into(),
        effort: effort.map(Effort::new),
        efforts: None,
        timeout_secs: 120,
    }
}

/// 一份**校验得过**的内存快照，给 `validate` 的测试用。
pub fn snapshot_fixture() -> ConfigSnapshot {
    let home = PathBuf::from("/home/u/.komo");
    ConfigSnapshot {
        start_only: StartOnly {
            data_dir: home.clone(),
            listen: super::DEFAULT_LISTEN.into(),
            db_path: home.join("state.db"),
            python_env_root: home.join("python-envs"),
        },
        model: model(
            "openai_responses",
            "chat-a",
            "KOMO_LLM_API_KEY",
            Some("medium"),
        ),
        memory: MemoryConfig {
            enabled: true,
            model: model(
                "openai_responses",
                "memory-a",
                "KOMO_MEMORY_API_KEY",
                Some("low"),
            ),
            embedding: Some(EmbeddingConfig {
                // 向量协议仍是 OpenAI 兼容的 `/embeddings`（§13.2）。
                model: model(
                    "openai_compatible",
                    "embed-a",
                    "KOMO_EMBEDDING_API_KEY",
                    None,
                ),
                revision: None,
                dimensions: Some(1024),
                document_prefix: None,
                query_prefix: None,
            }),
            retrieval: RetrievalConfig::default(),
        },
        channels: ChannelsConfig {
            feishu: ChannelConfig {
                enabled: true,
                allow_from: vec![PeerId::new("ou_operator")],
                home_chat: Some(PeerId::new("oc_home")),
                groups: vec![],
            },
            telegram: ChannelConfig::default(),
            wechat: ChannelConfig::default(),
        },
        policy: RuleTable::initial(),
        paths: PathsConfig {
            sessions_dir: home.join("sessions"),
            toolbox_dir: home.join("toolbox"),
            skill_dirs: vec![home.join("skills")],
            runtime_dir: home.join("runtime"),
            logs_dir: home.join("logs"),
            workspaces_dir: home.join("workspaces"),
        },
        credentials: BTreeMap::from([
            ("KOMO_LLM_API_KEY".into(), ContentHash::of_str("a")),
            ("KOMO_MEMORY_API_KEY".into(), ContentHash::of_str("b")),
            ("KOMO_EMBEDDING_API_KEY".into(), ContentHash::of_str("c")),
            ("FEISHU_APP_ID".into(), ContentHash::of_str("d")),
            ("FEISHU_APP_SECRET".into(), ContentHash::of_str("e")),
        ]),
        loaded_at: time::macros::datetime!(2026-09-15 08:00:00 UTC),
        sources: vec![],
    }
}
