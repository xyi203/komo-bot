//! 环境：`KOMO_HOME`、`.env` 里的凭证，以及 `KOMO_*` 覆盖（§3、§11.2、§12）。
//!
//! **这是整个 crate 里唯一读环境变量的地方。**别处一律读当前 [`ConfigSnapshot`]
//! （§3 第 2 步），凭证则只以变量名出现在快照里，值留在这里的 [`Secrets`]。

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use komo_kernel::types::digest::ContentHash;

/// 数据目录的环境变量。**唯一直接读的那个**（§12：「数据目录可通过配置或 KOMO_HOME
/// 改变」）。
pub const KOMO_HOME: &str = "KOMO_HOME";

/// `KOMO_*` 覆盖：文档只定义了 `KOMO_HOME`，这里再认一个监听地址，因为它和数据目录
/// 一样属于「启动时才定得下来、部署环境说了算」的那一小组键（§3 第 4 步）。
///
// TODO(decide: §3 只点名 KOMO_HOME。模型 / 渠道键要不要也接受 KOMO_* 覆盖没有定论；
// 在定下来之前只认这两个，其余一律以文件为准——多认一个键就是多一处"配置文件里写的
// 不算数"的地方。优先级按任务书：默认 < config.toml < 环境。
pub const KOMO_LISTEN: &str = "KOMO_LISTEN";

/// `.env` 与进程环境里的**值**。
///
/// [`fmt::Debug`] 只印变量名：快照、diff、重载报告、日志里都不能出现凭证（§3 第 3
/// 步），而一个会把自己整个印出来的结构迟早会被 `{:?}` 到某条日志里。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secrets {
    values: BTreeMap<String, String>,
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secrets")
            .field("names", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Secrets {
    pub fn new() -> Self {
        Self::default()
    }

    /// 测试与显式装填用。
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Secrets {
            values: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// 变量存在且非空。空字符串当作没配——一个空的 API key 只会在第一次请求时才失败。
    pub fn has(&self, name: &str) -> bool {
        self.values
            .get(name)
            .is_some_and(|value| !value.trim().is_empty())
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    /// 给定这些变量名的**值哈希**——快照里放的就是它（§3：指纹够回答"它变了没有"）。
    pub fn fingerprints<'a, I: IntoIterator<Item = &'a str>>(
        &self,
        names: I,
    ) -> BTreeMap<String, ContentHash> {
        names
            .into_iter()
            .filter_map(|name| {
                // 空值当作没配（和 `has` 一致）：一个空的 key 只会在第一次请求时才失败，
                // 而校验要在装上之前就说出来。
                self.values
                    .get(name)
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| (name.to_string(), ContentHash::of_str(value)))
            })
            .collect()
    }

    fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.values.insert(name.into(), value.into());
    }
}

/// 读 `.env`，再让**进程环境**覆盖它（默认 < 文件 < 环境）。
///
/// 用 `from_path_iter` 而不是 `dotenvy::from_path`：后者会往进程环境里写，于是"谁读了
/// 环境"这件事就散到了整个进程里，而热重载还会让它越写越多。
pub fn load_secrets(env_file: &Path) -> Result<Secrets, std::io::Error> {
    let mut secrets = Secrets::new();
    if env_file.exists() {
        let iter = dotenvy::from_path_iter(env_file)
            .map_err(|e| std::io::Error::other(format!("{}：{e}", env_file.display())))?;
        for entry in iter {
            let (key, value) =
                entry.map_err(|e| std::io::Error::other(format!("{}：{e}", env_file.display())))?;
            secrets.insert(key, value);
        }
    }
    for (key, value) in std::env::vars() {
        // 进程环境只补 `.env` 里出现过的凭证名与我们认识的 KOMO_* 键，别的不搬——
        // 整个进程环境搬进来等于把无关变量也变成"配置的一部分"。
        if secrets.get(&key).is_some() || is_credential_name(&key) {
            secrets.insert(key, value);
        }
    }
    Ok(secrets)
}

/// 看起来像凭证名的变量：`.env` 里没有、但操作者从环境里喂进来的那种。
fn is_credential_name(name: &str) -> bool {
    name.ends_with("_API_KEY")
        || name.ends_with("_TOKEN")
        || name.ends_with("_SECRET")
        || name.ends_with("_APP_ID")
}

/// 数据目录：`override_home` > `KOMO_HOME` > `~/.komo`。
pub fn resolve_home(override_home: Option<&Path>) -> Result<PathBuf, HomeError> {
    if let Some(home) = override_home {
        return Ok(home.to_path_buf());
    }
    if let Ok(home) = std::env::var(KOMO_HOME)
        && !home.trim().is_empty()
    {
        return Ok(PathBuf::from(home));
    }
    Ok(user_home()?.join(".komo"))
}

/// 当前用户的家目录。skills 的共享搜索路径（`~/.agents/skills`）也要它（§5.6）。
pub fn user_home() -> Result<PathBuf, HomeError> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
    match home {
        Ok(home) if !home.trim().is_empty() => Ok(PathBuf::from(home)),
        _ => Err(HomeError),
    }
}

/// `KOMO_*` 覆盖：只有我们认识的那几个键。
pub fn env_override(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("找不到家目录（HOME / USERPROFILE 都没有设置），请显式设置 KOMO_HOME")]
pub struct HomeError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_debug_prints_names_and_never_values() {
        let secrets = Secrets::from_pairs([("KOMO_LLM_API_KEY", "sk-super-secret")]);
        let printed = format!("{secrets:?}");
        assert!(printed.contains("KOMO_LLM_API_KEY"), "{printed}");
        assert!(!printed.contains("sk-super-secret"), "{printed}");
    }

    #[test]
    fn an_empty_credential_counts_as_missing() {
        let secrets = Secrets::from_pairs([("A", "  "), ("B", "x")]);
        assert!(!secrets.has("A"));
        assert!(secrets.has("B"));
    }

    #[test]
    fn a_fingerprint_changes_with_the_value_and_is_not_the_value() {
        let one = Secrets::from_pairs([("K", "secret-1")]).fingerprints(["K"]);
        let two = Secrets::from_pairs([("K", "secret-2")]).fingerprints(["K"]);
        assert_ne!(one, two);
        assert!(!format!("{one:?}").contains("secret-1"));
    }

    #[test]
    fn a_missing_variable_has_no_fingerprint_row() {
        let secrets = Secrets::from_pairs([("K", "v")]);
        let prints = secrets.fingerprints(["K", "MISSING"]);
        assert_eq!(prints.len(), 1);
    }
}
