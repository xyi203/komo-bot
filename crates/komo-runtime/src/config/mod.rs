//! ConfigSnapshot：config.toml / .env 的解析与校验（§3、§11.2）。
//!
//! 三件事，一个入口：
//!
//! - [`load_config`]：两个文件 → 一份 kernel [`ConfigSnapshot`] + 一份留在进程里的
//!   [`Secrets`]。凭证只以**变量名**出现在快照里，值不进快照（§3）。
//! - [`validate`]：`komo config check` 与热重载共用的**同一个**校验函数。
//! - [`ConfigHolder`]：进程内唯一的 `Arc<ConfigSnapshot>`，arc-swap 原子替换；
//!   [`ConfigHolder::changed_since`] 与 [`ConfigHolder::reload`] 是 Gateway 接热重载
//!   的两个入口（mtime 轮询 / `SIGHUP` / `komo config reload` 都落到它们上）。
//!
//! **这是整个 crate 里唯一读环境变量的地方**（§13.4 的编码规则），而其中
//! `KOMO_HOME` 是唯一被直接读的那一个（§12）。

mod edit;
mod effort;
mod env;
mod error;
mod file;
mod holder;
mod init;
mod settings;
mod validate;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use komo_kernel::protocol::config::{ConfigIssue, ConfigSnapshot, IssueSeverity, KeyPath};
use time::OffsetDateTime;

pub use edit::{
    EditError, EmbeddingFields, ModelFields, ProviderFields, config_version, env_view, models_view,
    remove_embedding_model, remove_env_value, remove_model, remove_provider,
    rename_embedding_model, rename_provider, set_default_model, set_env_value, set_env_values,
    upsert_embedding_model, upsert_model, upsert_provider,
};
pub use effort::{EffortCapabilities, EffortProblem, EffortSupport};
pub use env::{HomeError, KOMO_HOME, KOMO_LISTEN, Secrets, secrets_from_text, user_home};
pub use error::ConfigError;
pub use file::{DEFAULT_LISTEN, Sources, channel_credentials};
pub use holder::{ConfigHolder, ReloadReport};
pub use init::{API_KEY_VARIABLE, InitError, InitReport, config_template, env_template, init_home};
pub use settings::{set_setting, settings_view};
pub use validate::{has_errors, validate, validate_with};

/// 从哪里加载。
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// 覆盖数据目录；`None` = `KOMO_HOME`，再没有就是 `~/.komo`。
    pub home: Option<PathBuf>,
    /// effort 档位声明。默认是内建表。
    pub caps: EffortCapabilities,
}

impl LoadOptions {
    pub fn at(home: impl Into<PathBuf>) -> Self {
        LoadOptions {
            home: Some(home.into()),
            caps: EffortCapabilities::builtin(),
        }
    }
}

/// 一次加载的结果：快照 + 凭证 + 校验里那些**不致命**的问题。
///
/// 致命问题不在这里——它们让 [`load_config`] 直接失败，于是"校验不过的配置永远不会被
/// 装上"在类型上就成立了（§3 第 1 步）。
#[derive(Debug, Clone)]
pub struct Loaded {
    pub snapshot: Arc<ConfigSnapshot>,
    pub secrets: Arc<Secrets>,
    /// 只有警告——错误会让加载失败。
    pub issues: Vec<ConfigIssue>,
    pub home: PathBuf,
    pub sources: Sources,
}

/// Login bootstraps channel credentials, so unrelated model validation must not block it.
pub fn channel_login_config(
    home: &Path,
    platform: &komo_kernel::types::chat::ChannelPlatform,
) -> Result<
    (
        Option<komo_kernel::protocol::config::ChannelPlugin>,
        Secrets,
    ),
    ConfigError,
> {
    let sources = Sources::under(home);
    let secrets = env::load_secrets(&sources.env).map_err(|error| ConfigError::Io {
        path: sources.env.clone(),
        message: error.to_string(),
    })?;
    if !sources.config.exists() {
        return Ok((None, secrets));
    }
    let (file, _) = read_config(&sources.config)?;
    use komo_kernel::types::chat::ChannelPlatform;
    let plugin = match platform {
        ChannelPlatform::Feishu => file.channels.feishu.plugin,
        ChannelPlatform::Telegram => file.channels.telegram.plugin,
        ChannelPlatform::Wechat => file.channels.wechat.plugin,
        ChannelPlatform::Custom(id) => file.channels.extra.get(id).and_then(|c| c.plugin.clone()),
        ChannelPlatform::Api => None,
    };
    Ok((plugin, secrets))
}

/// 解析两个文件并校验。
///
/// 相对路径按**配置文件所在目录**解析（§12）。`KOMO_HOME` 是唯一直接读的环境变量；
/// `KOMO_LISTEN` 是另一个被认识的 `KOMO_*` 覆盖，优先级是 默认 < 文件 < 环境。
pub fn load_config(options: &LoadOptions) -> Result<Loaded, ConfigError> {
    let home = env::resolve_home(options.home.as_deref())?;
    let sources = Sources::under(&home);
    let secrets = env::load_secrets(&sources.env).map_err(|e| ConfigError::Io {
        path: sources.env.clone(),
        message: e.to_string(),
    })?;

    let (file, unknown) = read_config(&sources.config)?;
    assemble_checked(file, unknown, home, sources, secrets, &options.caps)
}

/// 拿**还没落盘**的两份文本走一遍与 [`load_config`] 相同的解析 + 校验：Web 改配置时先过
/// 这一关再写文件（§3「从 Web 改模型配置」）。成功返回警告；有错误就是 `Invalid`。
pub fn check_candidate(
    home: &Path,
    config_text: &str,
    env_text: Option<&str>,
    caps: &EffortCapabilities,
) -> Result<Vec<ConfigIssue>, ConfigError> {
    let sources = Sources::under(home);
    let secrets = env::secrets_from_text(env_text, &sources.env).map_err(|e| ConfigError::Io {
        path: sources.env.clone(),
        message: e.to_string(),
    })?;
    let (file, unknown) = parse_config(&sources.config, config_text)?;
    assemble_checked(file, unknown, home.to_path_buf(), sources, secrets, caps)
        .map(|loaded| loaded.issues)
}

fn assemble_checked(
    file: file::FileConfig,
    unknown: Vec<String>,
    home: PathBuf,
    sources: Sources,
    secrets: Secrets,
    caps: &EffortCapabilities,
) -> Result<Loaded, ConfigError> {
    let snapshot = file::assemble(file, &home, &sources, &secrets, OffsetDateTime::now_utc())?;

    let mut issues = validate_with(&snapshot, caps);
    if has_errors(&issues) {
        return Err(ConfigError::Invalid { issues });
    }
    issues.extend(unknown.iter().map(|key| unknown_key(key)));
    issues.extend(retired_policy(&home));

    Ok(Loaded {
        snapshot: Arc::new(snapshot),
        secrets: Arc::new(secrets),
        issues,
        home,
        sources,
    })
}

/// 读 config.toml，连同**被忽略的键路径**一起返回。
///
/// 不认识的键不让加载失败：新旧版本之间多一个、少一个键是常态（升级后旧配置里留着已经
/// 退役的键，或者先写好下一版才认识的键），为此整份配置装不上、连带聊天入口一起停摆，
/// 代价远大于忽略一个键。但**忽略不等于沉默**：每个被忽略的键都变成一条警告，
/// `komo config check` 与 `komo doctor` 都会列出来——拼错的键（或者写到了错误的表下面，
/// 比如 `default_agent` 落进了前面那个 `[agents.x]`）照样看得见。
///
/// 键认识、值的类型不对仍然是解析错误：那种情况没有"忽略"这个安全的解释。
fn read_config(path: &Path) -> Result<(file::FileConfig, Vec<String>), ConfigError> {
    if !path.exists() {
        return Err(ConfigError::Io {
            path: path.to_path_buf(),
            message: "配置文件不存在（`komo init` 会生成一份）".into(),
        });
    }
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    parse_config(path, &text)
}

fn parse_config(path: &Path, text: &str) -> Result<(file::FileConfig, Vec<String>), ConfigError> {
    let parse_error = |e: toml::de::Error| ConfigError::Parse {
        file: path.to_path_buf(),
        message: e.to_string(),
    };
    let deserializer = toml::Deserializer::parse(text).map_err(parse_error)?;
    let mut unknown = Vec::new();
    let file = serde_ignored::deserialize(deserializer, |key| unknown.push(key.to_string()))
        .map_err(parse_error)?;
    Ok((file, unknown))
}

/// 一个被忽略的键。只是警告：配置照常装上（§3）。
fn unknown_key(key: &str) -> ConfigIssue {
    ConfigIssue {
        key: KeyPath::new(key),
        severity: IssueSeverity::Warning,
        message: "不认识这个键，已忽略（拼错了？或者写到了别的表下面？）".into(),
    }
}

/// 已经退役的 `policy.toml`（§7，2026-10-11 删除 Policy 与审批）。
pub const RETIRED_POLICY_FILE: &str = "policy.toml";

/// 文件还在就报一句（警告，配置照常装上）：不读它，但也**不静默忽略**——操作者以为它还
/// 管着什么，就得有地方告诉他不管了。与被忽略的键走同一条路，`komo config check`、
/// 启动日志与热重载报告都看得见。
fn retired_policy(home: &Path) -> Option<ConfigIssue> {
    let path = home.join(RETIRED_POLICY_FILE);
    path.exists().then(|| ConfigIssue {
        key: KeyPath::new(RETIRED_POLICY_FILE),
        severity: IssueSeverity::Warning,
        message: format!(
            "{} 已不再使用（§7，2026-10-11 删除），可以删掉",
            path.display()
        ),
    })
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
