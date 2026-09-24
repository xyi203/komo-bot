//! 一份**校验得过**的内存 [`ConfigSnapshot`](crate::protocol::config::ConfigSnapshot)。
//!
//! 原来住在 `komo-runtime` 的 `config::testing` 里，`komo-agent` 的 skill 测试也要它——
//! 跨 crate 的测试替身按 §13.4 住在这里，只作为 dev-dependency 启用。

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::policy::RuleTable;
use crate::protocol::config::{
    ChannelConfig, ChannelsConfig, ConfigSnapshot, HomeConfig, MemoryConfig, PathsConfig,
    RetrievalConfig, StartOnly, TypesafeConfig,
};
use crate::types::agent::{AgentConfig, AgentProfile};
use crate::types::chat::PeerId;
use crate::types::digest::ContentHash;
use crate::types::model::{CatalogModel, Effort, EmbeddingConfig, ModelCatalog, ModelConfig};

fn model(provider: &str, name: &str, key_env: &str, effort: Option<&str>) -> ModelConfig {
    ModelConfig {
        provider: provider.into(),
        base_url: "https://llm.example.com/v1".into(),
        model: name.into(),
        api_key_env: key_env.into(),
        auth: None,
        effort: effort.map(Effort::new),
        efforts: None,
        timeout_secs: 120,
    }
}

/// 一份**校验得过**的内存快照，给各 crate 的测试用。
pub fn snapshot_fixture() -> ConfigSnapshot {
    let home = PathBuf::from("/home/u/.komo");
    let main = model("responses", "chat-a", "KOMO_LLM_API_KEY", Some("medium"));
    let memory_model = model("responses", "memory-a", "KOMO_MEMORY_API_KEY", Some("low"));
    let embedding = EmbeddingConfig {
        model: model("embeddings", "embed-a", "KOMO_EMBEDDING_API_KEY", None),
        revision: None,
        dimensions: Some(1024),
        document_prefix: None,
        query_prefix: None,
    };
    let model_catalog = ModelCatalog {
        default: "chat".into(),
        entries: BTreeMap::from([
            (
                "chat".into(),
                CatalogModel::Completion {
                    name: "chat".into(),
                    model_provider: Some("openrouter".into()),
                    context_window: None,
                    config: main.clone(),
                },
            ),
            (
                "memory".into(),
                CatalogModel::Completion {
                    name: "memory".into(),
                    model_provider: None,
                    context_window: None,
                    config: memory_model.clone(),
                },
            ),
            (
                "embedding".into(),
                CatalogModel::Embedding {
                    name: "embedding".into(),
                    model_provider: None,
                    config: embedding.clone(),
                },
            ),
        ]),
    };
    ConfigSnapshot {
        execution: Default::default(),
        start_only: StartOnly {
            data_dir: home.clone(),
            // 与 `komo-runtime` 的 `DEFAULT_LISTEN` 同值；夹具的监听地址没有测试断言它，
            // 改默认值时这里不必跟。
            listen: "127.0.0.1:7777".into(),
            db_path: home.join("state.db"),
            python_env_root: home.join("python-envs"),
        },
        model_catalog,
        model: main,
        memory: MemoryConfig {
            enabled: true,
            model: memory_model,
            embedding: Some(embedding),
            retrieval: RetrievalConfig::default(),
        },
        agent: AgentConfig {
            default_agent: "assistant".into(),
            agents: std::collections::BTreeMap::from([(
                "assistant".into(),
                AgentProfile::new("assistant"),
            )]),
        },
        home: HomeConfig::default(),
        typesafe: TypesafeConfig::default(),
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
