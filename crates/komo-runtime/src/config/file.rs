//! `config.toml` / `policy.toml` 的文件形状 → 一份 kernel [`ConfigSnapshot`]（§3、§13.3）。
//!
//! 文件结构在这里单独写一遍，不直接 `Deserialize` 快照本身，有两个理由：文件里很多段
//! 可以省略（`[memory.model]` 整段省略要继承主模型的**完整**配置，§13.3），而快照里
//! 它们是必填；以及错误要定位到**键路径**，这需要逐字段自己装配。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use komo_kernel::policy::RuleTable;
use komo_kernel::protocol::config::{
    ChannelConfig, ChannelsConfig, ConfigSnapshot, KeyPath, MemoryConfig, PathsConfig,
    RetrievalConfig, SourceFile, StartOnly,
};
use komo_kernel::types::chat::{ChannelPlatform, PeerId};
use komo_kernel::types::memory::RetrievalMode;
use komo_kernel::types::model::{Effort, EmbeddingConfig, ModelConfig};
use serde::Deserialize;
use time::OffsetDateTime;

use super::env::Secrets;
use super::error::ConfigError;

/// 默认监听地址：回环（§3「默认监听回环地址」）。端口取 kernel 示例里的那个。
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7777";
/// 默认模型超时，和 kernel 的 `ModelConfig` 默认一致。
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// 一个渠道的凭证变量名（§11.2 的 `.env` 示例）。微信的凭证是
/// `~/.komo/wechat/credentials.json`，不在 `.env` 里，所以它没有变量。
pub fn channel_credentials(platform: ChannelPlatform) -> &'static [&'static str] {
    match platform {
        ChannelPlatform::Feishu => &["FEISHU_APP_ID", "FEISHU_APP_SECRET"],
        ChannelPlatform::Telegram => &["TELEGRAM_BOT_TOKEN"],
        ChannelPlatform::Wechat | ChannelPlatform::Api => &[],
    }
}

// ---------------------------------------------------------------- 文件形状

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileConfig {
    #[serde(default)]
    pub gateway: GatewaySection,
    #[serde(default)]
    pub paths: PathsSection,
    pub model: Option<ModelSection>,
    #[serde(default)]
    pub memory: MemorySection,
    #[serde(default)]
    pub channels: ChannelsSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GatewaySection {
    pub listen: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PathsSection {
    pub data_dir: Option<PathBuf>,
    pub db_path: Option<PathBuf>,
    pub python_env_root: Option<PathBuf>,
    pub sessions_dir: Option<PathBuf>,
    pub toolbox_dir: Option<PathBuf>,
    pub runtime_dir: Option<PathBuf>,
    pub logs_dir: Option<PathBuf>,
    pub workspaces_dir: Option<PathBuf>,
    #[serde(default)]
    pub skill_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelSection {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub effort: Option<String>,
    /// 操作者显式声明这个模型支持哪些档位（§13.3 的"显式能力声明"）。省略 = 问适配器
    /// 自己的内建表；写成空表 = 这个模型一档都不支持。
    pub efforts: Option<Vec<String>>,
    pub timeout_secs: Option<u64>,
}

impl ModelSection {
    fn into_model(self) -> ModelConfig {
        ModelConfig {
            provider: self.provider.trim().to_string(),
            base_url: self.base_url.trim_end_matches('/').to_string(),
            model: self.model.trim().to_string(),
            api_key_env: self.api_key_env.trim().to_string(),
            effort: self.effort.map(Effort::new),
            efforts: self
                .efforts
                .map(|levels| levels.into_iter().map(Effort::new).collect()),
            timeout_secs: self.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemorySection {
    pub enabled: Option<bool>,
    pub model: Option<ModelSection>,
    pub embedding: Option<EmbeddingSection>,
    #[serde(default)]
    pub retrieval: RetrievalSection,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EmbeddingSection {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub effort: Option<String>,
    pub efforts: Option<Vec<String>>,
    pub timeout_secs: Option<u64>,
    pub revision: Option<String>,
    pub dimensions: Option<u32>,
    /// 文档侧 / 查询侧的输入前缀（§9.5）。两条都进空间指纹。
    pub document_prefix: Option<String>,
    pub query_prefix: Option<String>,
}

impl EmbeddingSection {
    fn into_embedding(self) -> EmbeddingConfig {
        EmbeddingConfig {
            model: ModelSection {
                provider: self.provider,
                base_url: self.base_url,
                model: self.model,
                api_key_env: self.api_key_env,
                effort: self.effort,
                efforts: self.efforts,
                timeout_secs: self.timeout_secs,
            }
            .into_model(),
            revision: self.revision,
            dimensions: self.dimensions,
            document_prefix: self.document_prefix,
            query_prefix: self.query_prefix,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RetrievalSection {
    pub mode: Option<RetrievalMode>,
    pub candidate_limit: Option<u32>,
    pub top_k: Option<u32>,
    pub max_tokens: Option<u32>,
}

impl RetrievalSection {
    fn into_retrieval(self) -> RetrievalConfig {
        let base = RetrievalConfig::default();
        RetrievalConfig {
            mode: self.mode.unwrap_or(base.mode),
            candidate_limit: self.candidate_limit.unwrap_or(base.candidate_limit),
            top_k: self.top_k.unwrap_or(base.top_k),
            max_tokens: self.max_tokens.unwrap_or(base.max_tokens),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChannelsSection {
    #[serde(default)]
    pub feishu: ChannelSection,
    #[serde(default)]
    pub telegram: ChannelSection,
    #[serde(default)]
    pub wechat: ChannelSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChannelSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allow_from: Vec<PeerId>,
    pub home_chat: Option<PeerId>,
    #[serde(default)]
    pub groups: Vec<PeerId>,
}

impl ChannelSection {
    fn into_channel(self) -> ChannelConfig {
        ChannelConfig {
            enabled: self.enabled,
            allow_from: self.allow_from,
            home_chat: self.home_chat,
            groups: self.groups,
        }
    }
}

// ---------------------------------------------------------------- 装配

/// 三个来源文件的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sources {
    pub config: PathBuf,
    pub env: PathBuf,
    pub policy: PathBuf,
}

impl Sources {
    pub fn under(home: &Path) -> Self {
        Sources {
            config: home.join("config.toml"),
            env: home.join(".env"),
            policy: home.join("policy.toml"),
        }
    }

    pub fn all(&self) -> [&Path; 3] {
        [&self.config, &self.env, &self.policy]
    }

    /// 存在的那些文件的 mtime——`komo doctor` 与 `changed_since` 比的就是它（§3）。
    pub fn stamps(&self) -> Vec<SourceFile> {
        self.all()
            .into_iter()
            .filter_map(|path| {
                let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
                Some(SourceFile {
                    path: path.to_path_buf(),
                    mtime: OffsetDateTime::from(mtime),
                })
            })
            .collect()
    }
}

/// 相对路径按**配置文件所在目录**解析（§12）。
fn resolve(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

/// 一份完整快照。**解析不做校验**：问题由 [`super::validate`] 给出，才能"一次报全"
/// 而不是遇到第一个就停（§3）。
pub(super) fn assemble(
    file: FileConfig,
    home: &Path,
    sources: &Sources,
    policy: RuleTable,
    secrets: &Secrets,
    now: OffsetDateTime,
) -> Result<ConfigSnapshot, ConfigError> {
    let base = sources
        .config
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.to_path_buf());

    let data_dir = file
        .paths
        .data_dir
        .clone()
        .map(|dir| resolve(&base, dir))
        .unwrap_or_else(|| home.to_path_buf());

    let listen = super::env::env_override(super::env::KOMO_LISTEN)
        .or(file.gateway.listen)
        .unwrap_or_else(|| DEFAULT_LISTEN.to_string());

    let start_only = StartOnly {
        db_path: resolve(
            &base,
            file.paths
                .db_path
                .clone()
                .unwrap_or_else(|| data_dir.join("state.db")),
        ),
        python_env_root: resolve(
            &base,
            file.paths
                .python_env_root
                .clone()
                .unwrap_or_else(|| data_dir.join("python-envs")),
        ),
        data_dir: data_dir.clone(),
        listen,
    };

    let paths = PathsConfig {
        sessions_dir: resolve(
            &base,
            file.paths
                .sessions_dir
                .unwrap_or_else(|| data_dir.join("sessions")),
        ),
        toolbox_dir: resolve(
            &base,
            file.paths
                .toolbox_dir
                .unwrap_or_else(|| data_dir.join("toolbox")),
        ),
        skill_dirs: file
            .paths
            .skill_dirs
            .into_iter()
            .map(|dir| resolve(&base, dir))
            .collect(),
        runtime_dir: resolve(
            &base,
            file.paths
                .runtime_dir
                .unwrap_or_else(|| data_dir.join("runtime")),
        ),
        logs_dir: resolve(
            &base,
            file.paths.logs_dir.unwrap_or_else(|| data_dir.join("logs")),
        ),
        workspaces_dir: resolve(
            &base,
            file.paths
                .workspaces_dir
                .unwrap_or_else(|| data_dir.join("workspaces")),
        ),
    };

    let Some(model) = file.model else {
        return Err(ConfigError::Missing {
            key: KeyPath::new("model"),
            file: sources.config.clone(),
            message: "没有 [model] 段：主模型是必填的（§13.3）".into(),
        });
    };
    let model = model.into_model();

    // §13.3：「memory.model 整段省略时，继承配置文件中主模型的**完整**配置，包括
    // effort」——在解析时就填好，不在使用处拼接。
    let memory_model = file
        .memory
        .model
        .map(ModelSection::into_model)
        .unwrap_or_else(|| model.clone());

    let memory = MemoryConfig {
        enabled: file.memory.enabled.unwrap_or(true),
        model: memory_model,
        embedding: file.memory.embedding.map(EmbeddingSection::into_embedding),
        retrieval: file.memory.retrieval.into_retrieval(),
    };

    let channels = ChannelsConfig {
        feishu: file.channels.feishu.into_channel(),
        telegram: file.channels.telegram.into_channel(),
        wechat: file.channels.wechat.into_channel(),
    };

    let credentials = credential_fingerprints(&model, &memory, &channels, secrets);

    Ok(ConfigSnapshot {
        start_only,
        model,
        memory,
        channels,
        policy,
        paths,
        credentials,
        loaded_at: now,
        sources: sources.stamps(),
    })
}

/// 快照里的凭证**指纹**：这份配置引用到的变量名，加上已启用渠道的固定变量名。
fn credential_fingerprints(
    model: &ModelConfig,
    memory: &MemoryConfig,
    channels: &ChannelsConfig,
    secrets: &Secrets,
) -> BTreeMap<String, komo_kernel::types::digest::ContentHash> {
    let mut names: Vec<String> = vec![model.api_key_env.clone(), memory.model.api_key_env.clone()];
    if let Some(embedding) = &memory.embedding {
        names.push(embedding.model.api_key_env.clone());
    }
    for platform in [
        ChannelPlatform::Feishu,
        ChannelPlatform::Telegram,
        ChannelPlatform::Wechat,
    ] {
        if channels.get(platform).is_some_and(|c| c.enabled) {
            names.extend(channel_credentials(platform).iter().map(|s| s.to_string()));
        }
    }
    names.sort();
    names.dedup();
    secrets.fingerprints(names.iter().map(String::as_str))
}
