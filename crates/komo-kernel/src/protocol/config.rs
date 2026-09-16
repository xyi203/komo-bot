//! `ConfigSnapshot`：config.toml + .env + policy.toml 解析出来的**一份完整快照**（§3）。
//!
//! 进程里只有一个 `Arc<ConfigSnapshot>`，热重载时原子替换。读配置的地方按用途读当前
//! 快照、不缓存——名单改完下一条消息就按新名单判。
//!
//! 两条性质写在类型里：
//!
//! - **凭证不进快照**。只有 `api_key_env`（变量名）和 [`ConfigSnapshot::credentials`]
//!   里的指纹（值的哈希）。指纹让"凭证变了 → 换掉对应的 LlmClient"判断得出来，而
//!   重载日志仍然只出键名（§3 第 3 步）。
//! - **只在启动时生效的键单独成一个结构**（[`StartOnly`]）。改了它们，重载照常完成
//!   其余部分，然后明确报告"以下键需要 `komo gateway restart`"——不静默忽略，也不假
//!   装已生效。

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::policy::RuleTable;
use crate::types::chat::{ChannelPlatform, PeerId};
use crate::types::digest::ContentHash;
use crate::types::memory::RetrievalMode;
use crate::types::model::{EmbeddingConfig, ModelConfig};

/// 一个配置键的路径，例如 `channels.feishu.allow_from`。**只有键名，没有值。**
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyPath(String);

impl KeyPath {
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn child(&self, segment: &str) -> Self {
        if self.0.is_empty() {
            Self(segment.to_string())
        } else {
            Self(format!("{}.{segment}", self.0))
        }
    }
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 改了要重启 Gateway 才生效的那一小组键（§3 第 4 步）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartOnly {
    /// 数据目录 / `KOMO_HOME`。
    pub data_dir: PathBuf,
    /// 监听地址与端口。
    pub listen: String,
    pub db_path: PathBuf,
    pub python_env_root: PathBuf,
}

/// 一个聊天渠道的行为键（§11.2）。凭证在 .env，不在这里。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 操作者的平台 id。**为空 = 只出不进**：还能作 home chat 收投递，但没人能通过它
    /// 下指令。
    #[serde(default)]
    pub allow_from: Vec<PeerId>,
    /// 主动投递（审批请求等）的目标会话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_chat: Option<PeerId>,
    /// 允许响应的群。缺省 = 只响应 `allow_from` 的私聊。
    #[serde(default)]
    pub groups: Vec<PeerId>,
}

impl ChannelConfig {
    /// 这个发送者是不是操作者。
    pub fn is_operator(&self, sender: &PeerId) -> bool {
        self.allow_from.contains(sender)
    }

    /// 这个群会不会被响应。
    pub fn responds_in_group(&self, chat: &PeerId) -> bool {
        self.groups.contains(chat)
    }

    /// enabled 但没人能说话——`komo doctor` 把它列为警告（§11.2）。
    pub fn is_outbound_only(&self) -> bool {
        self.enabled && self.allow_from.is_empty()
    }
}

/// 三个渠道。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelsConfig {
    #[serde(default)]
    pub feishu: ChannelConfig,
    #[serde(default)]
    pub telegram: ChannelConfig,
    #[serde(default)]
    pub wechat: ChannelConfig,
}

impl ChannelsConfig {
    pub fn get(&self, platform: ChannelPlatform) -> Option<&ChannelConfig> {
        match platform {
            ChannelPlatform::Feishu => Some(&self.feishu),
            ChannelPlatform::Telegram => Some(&self.telegram),
            ChannelPlatform::Wechat => Some(&self.wechat),
            ChannelPlatform::Api => None,
        }
    }

    /// home chat 的候选，按 §11.4 的默认顺序：**飞书 > Telegram > WeChat**。前两者能
    /// 对任意已加入的会话主动推送，微信不能。
    pub fn home_chats(&self) -> Vec<(ChannelPlatform, PeerId)> {
        [
            (ChannelPlatform::Feishu, &self.feishu),
            (ChannelPlatform::Telegram, &self.telegram),
            (ChannelPlatform::Wechat, &self.wechat),
        ]
        .into_iter()
        .filter(|(_, c)| c.enabled)
        .filter_map(|(platform, c)| c.home_chat.clone().map(|chat| (platform, chat)))
        .collect()
    }
}

/// `[memory.retrieval]`（§9.4）。这些是可调整的初始预算，不强行填满。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalConfig {
    #[serde(default)]
    pub mode: RetrievalMode,
    #[serde(default = "default_candidate_limit")]
    pub candidate_limit: u32,
    #[serde(default = "default_top_k")]
    pub top_k: u32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_candidate_limit() -> u32 {
    40
}
fn default_top_k() -> u32 {
    8
}
fn default_max_tokens() -> u32 {
    1500
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        RetrievalConfig {
            mode: RetrievalMode::default(),
            candidate_limit: default_candidate_limit(),
            top_k: default_top_k(),
            max_tokens: default_max_tokens(),
        }
    }
}

/// `[memory]`（§13.3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 整段省略时继承主模型的**完整**配置，包括 effort；解析时就填好，不在使用处
    /// 拼接（§13.3）。
    pub model: ModelConfig,
    /// embedding 必须独立指定；明确选择 keyword 模式时可以没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingConfig>,
    #[serde(default)]
    pub retrieval: RetrievalConfig,
}

fn default_true() -> bool {
    true
}

/// 数据目录下的各个位置（§12）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathsConfig {
    pub sessions_dir: PathBuf,
    pub toolbox_dir: PathBuf,
    /// Skills 的有序搜索路径，**同名先到先得**（§5.6）。
    #[serde(default)]
    pub skill_dirs: Vec<PathBuf>,
    pub runtime_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub workspaces_dir: PathBuf,
}

/// 一个来源文件及其 mtime——`komo doctor` 拿它和"当前生效配置的加载时间"比对，
/// 不一致就是"文件改了但没装上"（§3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: PathBuf,
    #[serde(with = "time::serde::rfc3339")]
    pub mtime: OffsetDateTime,
}

/// 一份完整的配置快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub start_only: StartOnly,
    pub model: ModelConfig,
    pub memory: MemoryConfig,
    pub channels: ChannelsConfig,
    pub policy: RuleTable,
    pub paths: PathsConfig,
    /// 每个凭证**值**的哈希，按变量名索引。值本身不在快照里，这里只够回答"它变了
    /// 没有"。
    #[serde(default)]
    pub credentials: BTreeMap<String, ContentHash>,
    #[serde(with = "time::serde::rfc3339")]
    pub loaded_at: OffsetDateTime,
    #[serde(default)]
    pub sources: Vec<SourceFile>,
}

impl ConfigSnapshot {
    /// 两份快照的差异，**只出键名不出值**（§3 第 3 步）。
    pub fn diff(&self, other: &ConfigSnapshot) -> Vec<KeyPath> {
        let left = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        let right = serde_json::to_value(other).unwrap_or(serde_json::Value::Null);
        let mut out = Vec::new();
        diff_value(&KeyPath::new(""), &left, &right, &mut out);
        // `loaded_at` 和 `sources` 每次重载都不同，它们不是配置内容。
        out.retain(|key| {
            !key.as_str().starts_with("loaded_at") && !key.as_str().starts_with("sources")
        });
        out
    }

    /// 差异里有哪些是**只在启动时生效**的键。重载要把它们原样报给操作者。
    pub fn start_only_changes(&self, other: &ConfigSnapshot) -> Vec<KeyPath> {
        self.diff(other)
            .into_iter()
            .filter(|key| key.as_str().starts_with("start_only."))
            .collect()
    }
}

fn diff_value(
    path: &KeyPath,
    left: &serde_json::Value,
    right: &serde_json::Value,
    out: &mut Vec<KeyPath>,
) {
    use serde_json::Value;
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let child = path.child(key);
                match (a.get(key), b.get(key)) {
                    (Some(l), Some(r)) => diff_value(&child, l, r, out),
                    _ => out.push(child),
                }
            }
        }
        // 数组整体比较：出键名就够了，出到下标反而把值的形状泄出来。
        _ if left != right => out.push(path.clone()),
        _ => {}
    }
}

/// 配置校验的一条问题。`komo config check` 与重载共用同一套校验（§11.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigIssue {
    pub key: KeyPath,
    pub severity: IssueSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    /// 校验不过——**旧快照原样保留**，哪怕只错一个键（§3 第 1 步）。
    Error,
    /// 例如"某渠道 enabled 但 allow_from 为空"。
    Warning,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::model::Effort;
    use time::macros::datetime;

    fn model(name: &str) -> ModelConfig {
        ModelConfig {
            provider: "openai_compatible".into(),
            base_url: "https://llm.example.com/v1".into(),
            model: name.into(),
            api_key_env: "KOMO_LLM_API_KEY".into(),
            effort: Some(Effort::new("medium")),
            efforts: None,
            timeout_secs: 120,
        }
    }

    fn snapshot() -> ConfigSnapshot {
        ConfigSnapshot {
            start_only: StartOnly {
                data_dir: PathBuf::from("/home/u/.komo"),
                listen: "127.0.0.1:7777".into(),
                db_path: PathBuf::from("/home/u/.komo/state.db"),
                python_env_root: PathBuf::from("/home/u/.komo/python-envs"),
            },
            model: model("chat-a"),
            memory: MemoryConfig {
                enabled: true,
                model: model("memory-a"),
                embedding: None,
                retrieval: RetrievalConfig::default(),
            },
            channels: ChannelsConfig {
                feishu: ChannelConfig {
                    enabled: true,
                    allow_from: vec![PeerId::new("ou_xxx")],
                    home_chat: Some(PeerId::new("oc_xxx")),
                    groups: vec![],
                },
                telegram: ChannelConfig::default(),
                wechat: ChannelConfig::default(),
            },
            policy: RuleTable::initial(),
            paths: PathsConfig {
                sessions_dir: PathBuf::from("/home/u/.komo/sessions"),
                toolbox_dir: PathBuf::from("/home/u/.komo/toolbox"),
                skill_dirs: vec![PathBuf::from("/home/u/.komo/skills")],
                runtime_dir: PathBuf::from("/home/u/.komo/runtime"),
                logs_dir: PathBuf::from("/home/u/.komo/logs"),
                workspaces_dir: PathBuf::from("/home/u/.komo/workspaces"),
            },
            credentials: BTreeMap::from([(
                "KOMO_LLM_API_KEY".into(),
                ContentHash::of_str("secret-1"),
            )]),
            loaded_at: datetime!(2026-09-15 08:00:00 UTC),
            sources: vec![],
        }
    }

    #[test]
    fn an_identical_snapshot_has_no_diff() {
        let mut later = snapshot();
        later.loaded_at = datetime!(2026-09-15 09:00:00 UTC);
        assert_eq!(snapshot().diff(&later), vec![]);
    }

    #[test]
    fn a_diff_names_keys_and_never_values() {
        let before = snapshot();
        let mut after = snapshot();
        after.model.model = "chat-b".into();
        after.channels.feishu.allow_from.push(PeerId::new("ou_yyy"));

        let keys = before.diff(&after);
        assert_eq!(
            keys,
            vec![
                KeyPath::new("channels.feishu.allow_from"),
                KeyPath::new("model.model"),
            ]
        );
        for key in &keys {
            assert!(!key.as_str().contains("chat-b"), "{key}");
            assert!(!key.as_str().contains("ou_yyy"), "{key}");
        }
    }

    #[test]
    fn a_changed_credential_shows_up_as_a_key_not_a_secret() {
        let before = snapshot();
        let mut after = snapshot();
        after
            .credentials
            .insert("KOMO_LLM_API_KEY".into(), ContentHash::of_str("secret-2"));
        assert_eq!(
            before.diff(&after),
            vec![KeyPath::new("credentials.KOMO_LLM_API_KEY")]
        );
    }

    #[test]
    fn start_only_keys_are_reported_separately() {
        let before = snapshot();
        let mut after = snapshot();
        after.start_only.listen = "0.0.0.0:7777".into();
        after.model.model = "chat-b".into();

        assert_eq!(
            before.start_only_changes(&after),
            vec![KeyPath::new("start_only.listen")]
        );
        assert_eq!(before.diff(&after).len(), 2, "其余部分照常报告");
    }

    #[test]
    fn home_chat_candidates_follow_feishu_then_telegram_then_wechat() {
        let mut snapshot = snapshot();
        snapshot.channels.telegram = ChannelConfig {
            enabled: true,
            allow_from: vec![PeerId::new("123")],
            home_chat: Some(PeerId::new("123")),
            groups: vec![],
        };
        snapshot.channels.wechat = ChannelConfig {
            enabled: true,
            allow_from: vec![PeerId::new("wxid_x")],
            home_chat: Some(PeerId::new("wxid_x")),
            groups: vec![],
        };
        let order: Vec<ChannelPlatform> = snapshot
            .channels
            .home_chats()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(
            order,
            vec![
                ChannelPlatform::Feishu,
                ChannelPlatform::Telegram,
                ChannelPlatform::Wechat
            ]
        );
    }

    #[test]
    fn an_enabled_channel_with_an_empty_allow_list_is_outbound_only() {
        let channel = ChannelConfig {
            enabled: true,
            allow_from: vec![],
            home_chat: Some(PeerId::new("oc_x")),
            groups: vec![],
        };
        assert!(channel.is_outbound_only());
        assert!(!channel.is_operator(&PeerId::new("ou_x")));
    }
}
