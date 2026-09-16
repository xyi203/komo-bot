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
use komo_kernel::protocol::config::{ConfigIssue, ConfigSnapshot};
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

    let file = read_config(&sources.config)?;
    let policy = read_policy(&sources.policy)?;

    let snapshot = file::assemble(
        file,
        &home,
        &sources,
        policy,
        &secrets,
        OffsetDateTime::now_utc(),
    )?;

    let issues = validate_with(&snapshot, &options.caps);
    if has_errors(&issues) {
        return Err(ConfigError::Invalid { issues });
    }

    Ok(Loaded {
        snapshot: Arc::new(snapshot),
        secrets: Arc::new(secrets),
        issues,
        home,
        sources,
    })
}

fn read_config(path: &Path) -> Result<file::FileConfig, ConfigError> {
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
    toml::from_str(&text).map_err(|e| ConfigError::Parse {
        file: path.to_path_buf(),
        message: e.to_string(),
    })
}

/// policy.toml 直接反序列化成 kernel 的 [`RuleTable`]——规则是**数据**（§7.1），所以
/// 这里没有第二套形状。文件不在就用 §7.1 的初始建议。
fn read_policy(path: &Path) -> Result<RuleTable, ConfigError> {
    if !path.exists() {
        return Ok(RuleTable::initial());
    }
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    toml::from_str(&text).map_err(|e| ConfigError::Parse {
        file: path.to_path_buf(),
        message: e.to_string(),
    })
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
