//! 凭证的**值**（§3）：`.env` 与进程环境里读到的那些，由 `config` 装填。
//!
//! 快照里只有变量名与值哈希；值只在这里，给各模型 / 向量 / 判断后端与 MCP 取用。

use std::collections::BTreeMap;
use std::fmt;

use komo_kernel::types::digest::ContentHash;

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

    /// `.env` 与进程环境的装填（`config::secrets_from_text`）。
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.values.insert(name.into(), value.into());
    }
}

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
