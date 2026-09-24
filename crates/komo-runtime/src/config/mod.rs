//! ConfigSnapshot：config.toml / .env / policy.toml 的解析与校验（§3、§11.2）。
//!
//! 三件事，一个入口：
//!
//! - [`load_config`]：三个文件 → 一份 kernel [`ConfigSnapshot`] + 一份留在进程里的
//!   [`Secrets`]。凭证只以**变量名**出现在快照里，值不进快照（§3）。
//! - [`validate`]：`komo config check` 与热重载共用的**同一个**校验函数。
//! - [`ConfigHolder`]：进程内唯一的 `Arc<ConfigSnapshot>`，arc-swap 原子替换；
//!   [`ConfigHolder::changed_since`] 与 [`ConfigHolder::reload`] 是 Gateway 接热重载
//!   的两个入口（mtime 轮询 / `SIGHUP` / `komo config reload` 都落到它们上）。
//!
//! **这是整个 crate 里唯一读环境变量的地方**（§13.4 的编码规则），而其中
//! `KOMO_HOME` 是唯一被直接读的那一个（§12）。

mod effort;
mod env;
mod error;
mod file;
mod holder;
mod validate;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use komo_kernel::policy::RuleTable;
use komo_kernel::protocol::config::{ConfigIssue, ConfigSnapshot, IssueSeverity, KeyPath};
use time::OffsetDateTime;

pub use effort::{EffortCapabilities, EffortProblem, EffortSupport};
pub use env::{HomeError, KOMO_HOME, KOMO_LISTEN, Secrets, user_home};
pub use error::ConfigError;
pub use file::{DEFAULT_LISTEN, Sources, channel_credentials};
pub use holder::{ConfigHolder, ReloadReport};
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

/// 解析三个文件并校验。
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
    let policy = read_policy(&sources.policy)?;

    let snapshot = file::assemble(
        file,
        &home,
        &sources,
        policy,
        &secrets,
        OffsetDateTime::now_utc(),
    )?;

    let mut issues = validate_with(&snapshot, &options.caps);
    if has_errors(&issues) {
        return Err(ConfigError::Invalid { issues });
    }
    issues.extend(unknown.iter().map(|key| unknown_key(key)));

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
/// 键认识、值的类型不对仍然是解析错误：那种情况没有"忽略"这个安全的解释。policy.toml
/// 不走这条路（见 [`read_policy`]）。
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
    let parse_error = |e: toml::de::Error| ConfigError::Parse {
        file: path.to_path_buf(),
        message: e.to_string(),
    };
    let deserializer = toml::Deserializer::parse(&text).map_err(parse_error)?;
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

/// policy.toml 直接反序列化成 kernel 的 [`RuleTable`]——规则是**数据**（§7.1），所以
/// 这里没有第二套形状。文件不在就用 §7.1 的初始建议。
///
/// 这里**仍然拒绝未知键**（与 config.toml 不同）：规则里拼错一个键被悄悄忽略，这条规则
/// 就不再按操作者以为的样子生效——授权面上宁可装不上，也不要"看起来配了"。
fn read_policy(path: &Path) -> Result<RuleTable, ConfigError> {
    if !path.exists() {
        return Ok(RuleTable::initial());
    }
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let file: PolicyFile = toml::from_str(&text).map_err(|e| ConfigError::Parse {
        file: path.to_path_buf(),
        message: e.to_string(),
    })?;
    file.resolve().map_err(|message| ConfigError::Parse {
        file: path.to_path_buf(),
        message,
    })
}

/// `policy.toml` 的**文件形状**：比 [`RuleTable`] 多一个可选的 `mode`。
///
/// 两条路二选一，不允许同时写：
///
/// - `mode = "strict" | "auto"`：选一张**基表**（§7.1 的两套建议），文件里的
///   `[[rules]]` **追加在基表之后**。基表自带默认结论，所以这条路里不许再写 `default`。
/// - 不写 `mode`：文件本身就是整张表（`default` + `[[rules]]`），与 §7.1 的内建建议
///   无关——"我全都要自己写"那条路，也是这个键出现之前唯一的行为。
///
/// 规则本身的形状还是 kernel 的 [`PolicyRule`]，这里多出来的只有"选哪张基表"这一个决定。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<PolicyMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default: Option<komo_kernel::policy::Effect>,
    #[serde(default)]
    rules: Vec<komo_kernel::policy::PolicyRule>,
}

/// 一张**基表**（§7.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum PolicyMode {
    /// §7.1 的初始建议：任意 shell / Python 都要人看一眼。
    Strict,
    /// 只问危险形状（`RuleTable::auto`）：日常命令不再打扰。
    Auto,
}

impl PolicyFile {
    fn resolve(self) -> Result<RuleTable, String> {
        match (self.mode, self.default) {
            // 基表自带默认结论，再写一个就是两个互相打架的答案。
            (Some(mode), Some(_)) => Err(format!(
                "`mode = \"{}\"` 与 `default` 只能留一个：`mode` 选一张基表（默认结论跟着基表走），\
                 `default` 是自己写整张表时的默认结论",
                match mode {
                    PolicyMode::Strict => "strict",
                    PolicyMode::Auto => "auto",
                }
            )),
            (Some(mode), None) => {
                let mut base = match mode {
                    PolicyMode::Strict => RuleTable::initial(),
                    PolicyMode::Auto => RuleTable::auto(),
                };
                base.rules.extend(self.rules);
                Ok(base)
            }
            (None, default) => Ok(RuleTable {
                rules: self.rules,
                default: default.unwrap_or(komo_kernel::policy::Effect::Ask),
            }),
        }
    }
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
