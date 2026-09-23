//! `config.toml` / `policy.toml` 的文件形状 → 一份 kernel [`ConfigSnapshot`]（§3、§13.3）。
//!
//! 文件结构在这里单独写一遍，不直接 `Deserialize` 快照本身：`model_providers` 只是
//! 连接默认值，`model.<alias>` 才是模型目录项，角色最后解析成完整配置。运行时不再做
//! 继承，也不会把一个模型名拼到另一个模型的端点上。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use komo_kernel::policy::RuleTable;
use komo_kernel::protocol::config::{
    ChannelConfig, ChannelsConfig, ConfigSnapshot, ExecutionConfig, KeyPath, MemoryConfig,
    PathsConfig, RetrievalConfig, SourceFile, StartOnly, TypesafeConfig,
};
use komo_kernel::types::agent::{AgentConfig, AgentProfile};
use komo_kernel::types::chat::{ChannelPlatform, PeerId};
use komo_kernel::types::memory::{MemoryScopeParseError, RetrievalMode};
use komo_kernel::types::model::{
    CatalogModel, Effort, EmbeddingConfig, ModelCatalog, ModelConfig, ModelType,
};
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
    #[serde(default)]
    pub model_providers: BTreeMap<String, ModelProviderSection>,
    #[serde(default)]
    pub model: BTreeMap<String, ModelSection>,
    pub models: Option<ModelsSection>,
    #[serde(default)]
    pub memory: MemorySection,
    #[serde(default)]
    pub execution: ExecutionSection,
    #[serde(default)]
    pub typesafe: TypesafeSection,
    /// 没有明确归属的入口（TUI / CLI / Cron）走哪个 Agent。**必须指向一个声明过的
    /// `[agents.<id>]`**；一个 Agent 都不声明就是配置不完整（校验会拒绝）。
    pub default_agent: Option<String>,
    #[serde(default)]
    pub agents: std::collections::BTreeMap<String, AgentSection>,
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelProviderSection {
    pub base_url: Option<String>,
    #[serde(alias = "env_key")]
    pub api_key_env: Option<String>,
    pub api_backend: Option<String>,
    pub timeout_secs: Option<u64>,
    /// 凭证来源不是 `.env` 变量（目前只有 `"chatgpt"`，§13.3）；与 `api_key_env` 互斥，
    /// 校验阶段判。
    pub auth: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelSection {
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub model: String,
    pub name: Option<String>,
    pub model_provider: Option<String>,
    pub base_url: Option<String>,
    #[serde(alias = "env_key")]
    pub api_key_env: Option<String>,
    pub api_backend: Option<String>,
    /// 凭证来源不是 `.env` 变量（目前只有 `"chatgpt"`，§13.3）；与 `api_key_env` 互斥，
    /// 校验阶段判。
    pub auth: Option<String>,
    pub effort: Option<String>,
    /// 操作者显式声明这个模型支持哪些档位（§13.3 的"显式能力声明"）。省略 = 问适配器
    /// 自己的内建表；写成空表 = 这个模型一档都不支持。
    pub efforts: Option<Vec<String>>,
    pub timeout_secs: Option<u64>,
    pub context_window: Option<u64>,
    pub revision: Option<String>,
    pub dimensions: Option<u32>,
    pub document_prefix: Option<String>,
    pub query_prefix: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelsSection {
    pub default: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemorySection {
    pub enabled: Option<bool>,
    pub model: Option<String>,
    pub embedding: Option<String>,
    #[serde(default)]
    pub retrieval: RetrievalSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecutionSection {
    pub model_result_bytes: Option<usize>,
}

impl ExecutionSection {
    fn into_execution(self) -> ExecutionConfig {
        let base = ExecutionConfig::default();
        ExecutionConfig {
            model_result_bytes: self.model_result_bytes.unwrap_or(base.model_result_bytes),
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
    pub rerank: Option<bool>,
    pub rerank_shortlist: Option<u32>,
}

impl RetrievalSection {
    fn into_retrieval(self) -> RetrievalConfig {
        let base = RetrievalConfig::default();
        RetrievalConfig {
            mode: self.mode.unwrap_or(base.mode),
            candidate_limit: self.candidate_limit.unwrap_or(base.candidate_limit),
            top_k: self.top_k.unwrap_or(base.top_k),
            max_tokens: self.max_tokens.unwrap_or(base.max_tokens),
            rerank: self.rerank.unwrap_or(base.rerank),
            rerank_shortlist: self.rerank_shortlist.unwrap_or(base.rerank_shortlist),
        }
    }
}

/// `[agents.<id>]`：一个助手的长期定义（§四）。键省着写——缺的就是"用默认"。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentSection {
    pub instructions: Option<String>,
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub workspace: Option<PathBuf>,
    pub memory_scope: Option<String>,
}

impl AgentSection {
    fn into_profile(self, id: &str, base: &Path) -> Result<AgentProfile, ConfigError> {
        // 作用域写成什么样是**结构**问题，在解析这一步就报出来；"这个 id 有没有意义"
        // 属于校验。
        let memory_scope = match self.memory_scope.as_deref() {
            Some(raw) => Some(raw.trim().parse().map_err(|error: MemoryScopeParseError| {
                ConfigError::Io {
                    path: base.join("config.toml"),
                    message: format!("[agents.{id}] memory_scope {error}"),
                }
            })?),
            None => None,
        };
        Ok(AgentProfile {
            id: id.to_string(),
            instructions: self.instructions.filter(|text| !text.trim().is_empty()),
            model: self.model,
            tools: self.tools,
            skills: self.skills,
            workspace: self.workspace.map(|path| resolve(base, path)),
            memory_scope,
        })
    }
}

/// `[typesafe]`：可选的判断后端（§9.4）。键省着写——默认值都在 kernel 那边。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TypesafeSection {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub timeout_secs: Option<u64>,
}

impl TypesafeSection {
    fn into_typesafe(self) -> TypesafeConfig {
        let base = TypesafeConfig::default();
        TypesafeConfig {
            enabled: self.enabled.unwrap_or(base.enabled),
            endpoint: self.endpoint.unwrap_or(base.endpoint),
            model: self.model.unwrap_or(base.model),
            api_key: self.api_key.unwrap_or(base.api_key),
            timeout_secs: self.timeout_secs.unwrap_or(base.timeout_secs),
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

/// `[agents.<id>]` → [`AgentConfig`]。
///
/// **没有隐含默认**：一个 Agent 都没声明时这里不编一个出来，而是报"配置不完整"；`default_agent`
/// 指向不存在的 id 同样报错。两句话都要指出**该写什么**，否则操作者只知道"不行"。
fn build_agents(
    default_agent: Option<String>,
    sections: BTreeMap<String, AgentSection>,
    base: &Path,
) -> Result<AgentConfig, ConfigError> {
    let source = base.join("config.toml");
    if sections.is_empty() {
        return Err(ConfigError::Missing {
            key: KeyPath::new("agents"),
            file: source,
            message: "至少要声明一个 Agent，而且要写在**文件最前面**（任何 `[section]` 之前——TOML 的表会把它后面的键吞进去）：\n\n  default_agent = \"assistant\"\n\n  [agents.assistant]\n  instructions = \"…\""
                .into(),
        });
    }
    let mut agents = BTreeMap::new();
    for (id, section) in sections {
        if id.trim().is_empty() {
            return Err(ConfigError::Missing {
                key: KeyPath::new("agents"),
                file: source,
                message: "Agent 的 id 不能是空的（`[agents.<id>]`）".into(),
            });
        }
        agents.insert(id.clone(), section.into_profile(&id, base)?);
    }
    let default_agent = default_agent.unwrap_or_else(|| {
        // 只有一个 Agent 时它就是默认——这不是"隐含默认"，是无歧义。
        if agents.len() == 1 {
            agents.keys().next().expect("刚判过非空").clone()
        } else {
            String::new()
        }
    });
    Ok(AgentConfig {
        default_agent,
        agents,
    })
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

    let Some(models) = file.models else {
        return Err(ConfigError::Missing {
            key: KeyPath::new("models"),
            file: sources.config.clone(),
            message: "没有 [models] 段：需要用 default 指定主模型 alias".into(),
        });
    };
    let model_catalog = assemble_catalog(
        file.model_providers,
        file.model,
        models.default,
        &sources.config,
    )?;
    let model = model_catalog
        .completion(&model_catalog.default)
        .cloned()
        .ok_or_else(|| ConfigError::Missing {
            key: KeyPath::new("models.default"),
            file: sources.config.clone(),
            message: format!(
                "`{}` 不是一个已配置的 completion 模型",
                model_catalog.default
            ),
        })?;

    // memory.model 省略时继承 default alias；显式写时必须引用 completion。
    let memory_alias = file
        .memory
        .model
        .as_deref()
        .unwrap_or(&model_catalog.default);
    let memory_model = model_catalog
        .completion(memory_alias)
        .cloned()
        .ok_or_else(|| ConfigError::Missing {
            key: KeyPath::new("memory.model"),
            file: sources.config.clone(),
            message: format!("`{memory_alias}` 不是一个已配置的 completion 模型"),
        })?;
    let memory_embedding =
        match file.memory.embedding.as_deref() {
            Some(alias) => Some(model_catalog.embedding(alias).cloned().ok_or_else(|| {
                ConfigError::Missing {
                    key: KeyPath::new("memory.embedding"),
                    file: sources.config.clone(),
                    message: format!("`{alias}` 不是一个已配置的 embedding 模型"),
                }
            })?),
            None => None,
        };

    let memory = MemoryConfig {
        enabled: file.memory.enabled.unwrap_or(true),
        model: memory_model,
        embedding: memory_embedding,
        retrieval: file.memory.retrieval.into_retrieval(),
    };

    let channels = ChannelsConfig {
        feishu: file.channels.feishu.into_channel(),
        telegram: file.channels.telegram.into_channel(),
        wechat: file.channels.wechat.into_channel(),
    };

    let credentials = credential_fingerprints(&model_catalog, &channels, secrets, &data_dir);

    Ok(ConfigSnapshot {
        start_only,
        model_catalog,
        model,
        memory,
        agent: build_agents(file.default_agent, file.agents, &base)?,
        typesafe: file.typesafe.into_typesafe(),
        execution: file.execution.into_execution(),
        channels,
        policy,
        paths,
        credentials,
        loaded_at: now,
        sources: sources.stamps(),
    })
}

fn assemble_catalog(
    providers: BTreeMap<String, ModelProviderSection>,
    models: BTreeMap<String, ModelSection>,
    default: String,
    config_file: &Path,
) -> Result<ModelCatalog, ConfigError> {
    if models.is_empty() {
        return Err(ConfigError::Missing {
            key: KeyPath::new("model"),
            file: config_file.to_path_buf(),
            message: "至少需要一个 [model.<alias>]".into(),
        });
    }

    let mut entries = BTreeMap::new();
    for (alias, section) in models {
        let provider = match section.model_provider.as_deref() {
            Some(name) => Some(providers.get(name).ok_or_else(|| ConfigError::Missing {
                key: KeyPath::new(format!("model.{alias}.model_provider")),
                file: config_file.to_path_buf(),
                message: format!("没有 [model_providers.{name}]"),
            })?),
            None => None,
        };

        let base_url = inherited_string(
            section.base_url.as_deref(),
            provider.and_then(|p| p.base_url.as_deref()),
            &format!("model.{alias}.base_url"),
            config_file,
        )?;
        let api_backend = inherited_string(
            section.api_backend.as_deref(),
            provider.and_then(|p| p.api_backend.as_deref()),
            &format!("model.{alias}.api_backend"),
            config_file,
        )?;
        // `auth` 与 `api_key_env` 互斥（校验阶段判，§13.3）；配了 `auth` 时凭证不从
        // `.env` 变量取，`api_key_env` 允许留空——不在这里报"必填字段缺失"。
        let auth = inherited_optional(
            section.auth.as_deref(),
            provider.and_then(|p| p.auth.as_deref()),
        );
        let api_key_env = if auth.is_some() {
            section
                .api_key_env
                .as_deref()
                .or_else(|| provider.and_then(|p| p.api_key_env.as_deref()))
                .map(str::trim)
                .unwrap_or_default()
                .to_string()
        } else {
            inherited_string(
                section.api_key_env.as_deref(),
                provider.and_then(|p| p.api_key_env.as_deref()),
                &format!("model.{alias}.api_key_env"),
                config_file,
            )?
        };
        let timeout_secs = section
            .timeout_secs
            .or_else(|| provider.and_then(|p| p.timeout_secs))
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let common = ModelConfig {
            // 运行时 provider 字段仍表示协议 adapter；供应商 alias 单独留在 catalog。
            provider: api_backend.trim().to_ascii_lowercase(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: section.model.trim().to_string(),
            api_key_env: api_key_env.trim().to_string(),
            auth,
            effort: section.effort.map(Effort::new),
            efforts: section
                .efforts
                .map(|levels| levels.into_iter().map(Effort::new).collect()),
            timeout_secs,
        };
        let name = section.name.unwrap_or_else(|| alias.clone());
        let model_provider = section.model_provider;
        let entry = match section.model_type {
            ModelType::Completion => CatalogModel::Completion {
                name,
                model_provider,
                context_window: section.context_window,
                config: common,
            },
            ModelType::Embedding => CatalogModel::Embedding {
                name,
                model_provider,
                config: EmbeddingConfig {
                    model: common,
                    revision: section.revision,
                    dimensions: section.dimensions,
                    document_prefix: section.document_prefix,
                    query_prefix: section.query_prefix,
                },
            },
        };
        entries.insert(alias, entry);
    }

    Ok(ModelCatalog {
        default: default.trim().to_string(),
        entries,
    })
}

fn inherited_string(
    own: Option<&str>,
    inherited: Option<&str>,
    key: &str,
    config_file: &Path,
) -> Result<String, ConfigError> {
    own.or(inherited)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ConfigError::Missing {
            key: KeyPath::new(key),
            file: config_file.to_path_buf(),
            message: "model 与 model_provider 都没有提供这个必填字段".into(),
        })
}

/// 同 [`inherited_string`]，但这一项是可选的——没写就是 `None`，不报错（`auth` 用它）。
fn inherited_optional(own: Option<&str>, inherited: Option<&str>) -> Option<String> {
    own.or(inherited)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 快照里的凭证**指纹**：这份配置引用到的变量名，加上已启用渠道的固定变量名，
/// 再加上（用到了 `auth = "chatgpt"` 时）ChatGPT 凭证文件本身的指纹（§13.3）。
fn credential_fingerprints(
    catalog: &ModelCatalog,
    channels: &ChannelsConfig,
    secrets: &Secrets,
    data_dir: &Path,
) -> BTreeMap<String, komo_kernel::types::digest::ContentHash> {
    let mut names: Vec<String> = catalog
        .entries
        .values()
        .filter(|model| model.common().auth.is_none())
        .map(|model| model.common().api_key_env.clone())
        .collect();
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
    let mut fingerprints = secrets.fingerprints(names.iter().map(String::as_str));

    let uses_chatgpt_auth = catalog
        .entries
        .values()
        .any(|model| model.common().auth.as_deref() == Some(crate::llm::CHATGPT_AUTH));
    if uses_chatgpt_auth {
        let path = crate::llm::codex_auth::credentials_path(data_dir);
        if let Some(hash) = crate::llm::codex_auth::fingerprint(&path) {
            fingerprints.insert(
                crate::llm::codex_auth::CREDENTIAL_FINGERPRINT_KEY.to_string(),
                hash,
            );
        }
    }
    fingerprints
}
