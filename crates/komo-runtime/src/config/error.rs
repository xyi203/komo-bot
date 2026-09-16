//! 配置加载与重载的失败。

use std::path::PathBuf;

use komo_kernel::protocol::config::{ConfigIssue, IssueSeverity, KeyPath};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{path} 读不出来：{message}")]
    Io { path: PathBuf, message: String },
    #[error("{file} 解析失败：{message}")]
    Parse { file: PathBuf, message: String },
    /// 必填的一段不在文件里。
    #[error("{file}：缺少 `{key}` —— {message}")]
    Missing {
        key: KeyPath,
        file: PathBuf,
        message: String,
    },
    /// 校验不过。**旧快照原样保留**（§3 第 1 步）。
    #[error("配置校验不通过：{}", summarize(.issues))]
    Invalid { issues: Vec<ConfigIssue> },
    #[error(transparent)]
    Home(#[from] super::env::HomeError),
}

impl ConfigError {
    /// 校验失败时带回来的那些问题；其余失败没有。
    pub fn issues(&self) -> &[ConfigIssue] {
        match self {
            ConfigError::Invalid { issues } => issues,
            _ => &[],
        }
    }
}

fn summarize(issues: &[ConfigIssue]) -> String {
    issues
        .iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .map(|issue| format!("{}：{}", issue.key, issue.message))
        .collect::<Vec<_>>()
        .join("；")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invalid_config_error_names_the_keys_it_tripped_on() {
        let error = ConfigError::Invalid {
            issues: vec![
                ConfigIssue {
                    key: KeyPath::new("model.effort"),
                    severity: IssueSeverity::Error,
                    message: "不支持".into(),
                },
                ConfigIssue {
                    key: KeyPath::new("channels.feishu.allow_from"),
                    severity: IssueSeverity::Warning,
                    message: "为空".into(),
                },
            ],
        };
        let text = error.to_string();
        assert!(text.contains("model.effort"), "{text}");
        assert!(!text.contains("allow_from"), "警告不算失败原因：{text}");
        assert_eq!(error.issues().len(), 2);
    }
}
