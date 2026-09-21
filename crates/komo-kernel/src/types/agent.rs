//! 助手（Agent）的身份与能力：长期定义，以及一次 Run 冻结下来的快照。
//!
//! 三者分得很清（`docs/bot.md` §四）：**Profile** 是操作者写在配置文件里的长期定义，
//! **Session** 绑定一个 Profile 并承载一段持续对话，**RunSnapshot** 是某一次执行受理时
//! 冻结的身份与能力——它必须**可恢复**：审批可能一小时之后才答复，而那时 Profile 与
//! 磁盘上的文件都可能已经改过，恢复出来的 Run 不能因此换一副面孔。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::digest::ContentHash;
use crate::types::memory::MemoryScope;
use crate::types::model::ModelConfig;
use crate::types::refs::PayloadRef;
use crate::types::surface::AgentSurface;

/// 一个助手的长期定义。
///
/// **不含运行现场**：没有"当前会话"、没有正在跑的调用、没有取消令牌。并发跑两个助手时，
/// 任何"进程里存一份 current_agent 让大家去读"的做法都会串用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    /// 配置里的名字，例如 `assistant`。会话与 Run 都按它归属。
    pub id: String,
    /// 身份指令正文（系统提示里最靠前的那一段）。`None` = 用基础正文，不额外加。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// 模型 alias（进 `model_catalog` 解析）。`None` = 主模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 这次能给模型看的工具。`None` = 目录里装着的那几个（默认面）；`Some(vec![])` =
    /// 一个都不给——**空数组是"不给"，不是"全部"**，两者写在配置文件里的意思完全不同。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// 允许出现的 Skill（名字或来源）。`None` = 全部来源。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// 工作目录。`None` = `[paths] workspaces_dir`；相对路径按数据目录解析。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// 记忆作用域：只召回这个作用域（外加显式共享的用户资料）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_scope: Option<MemoryScope>,
}

impl AgentProfile {
    /// 一个只有名字的 Profile：其余字段全是"用默认"。它是配置里最常见的形状。
    pub fn new(id: impl Into<String>) -> Self {
        AgentProfile {
            id: id.into(),
            instructions: None,
            model: None,
            tools: None,
            skills: None,
            workspace: None,
            memory_scope: None,
        }
    }

    /// 这次运行的能力面。
    ///
    /// `catalog` 是**全局工具目录**（网关启动时装进来的那些名字）。`tools` 里写了目录里
    /// 没有的名字时，它不是"以后再说"，而是**不算数**——能力面只留真的存在的那些；调用
    /// 方要把被丢掉的名字报出来，而不是静默采纳一份写错的配置。
    pub fn surface(&self, catalog: &[String]) -> (AgentSurface, Vec<String>) {
        match &self.tools {
            None => (AgentSurface::new(catalog.iter().cloned()), Vec::new()),
            Some(wanted) => {
                let mut kept = Vec::new();
                let mut unknown = Vec::new();
                for name in wanted {
                    if catalog.iter().any(|known| known == name) {
                        kept.push(name.clone());
                    } else {
                        unknown.push(name.clone());
                    }
                }
                (AgentSurface::new(kept), unknown)
            }
        }
    }

    /// 这份 Profile 的内容指纹。它进 [`RunSnapshot`]，用来回答"这一条 Run 当时用的是哪一版
    /// 定义"——改过 Profile 与没改过，事后看得出区别。
    pub fn revision(&self) -> ContentHash {
        ContentHash::of_json(self).expect("AgentProfile 的每个字段都可序列化")
    }
}

/// 一次 Run 冻结下来的身份与能力（`docs/bot.md` §4.3）。
///
/// 两个约束写在字段里：
///
/// - **可恢复**：`instructions` 存的是**内容引用**（`payloads/` 里的正文），不是文件路径
///   ——审批期间那个文件可能已经被改过，恢复出来的 Run 不能换一副面孔；
/// - **固定配置不等于冻结安全策略**：这里冻结的是身份与能力，撤权、Deny 规则与停用状态
///   仍然在执行时读当前配置（§7）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub agent_id: String,
    /// 受理时那份 [`AgentProfile`] 的内容指纹。
    pub profile_revision: ContentHash,
    pub model: ModelConfig,
    /// 解析过的真实工作目录。
    pub workspace: PathBuf,
    /// 这次允许调用的工具。
    pub surface: AgentSurface,
    /// 身份指令正文的引用（`payloads/`）。`None` = 这份 Profile 没有额外指令。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_ref: Option<PayloadRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_scope: Option<MemoryScope>,
}

/// 配置里的一整套助手定义。
///
/// **没有隐含的默认 Agent**：`[agents.<id>]` 是唯一写法，一个都不写就是配置不完整
/// （校验会拒绝，并说清楚要在 `[agents.<id>]` 里写谁）。`default_agent` 指名"没有归属的
/// 入口（TUI、CLI、Cron）走谁"——它必须指向一个真的声明过的 id。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// 没有明确归属的入口用哪个 Agent。必须是 `agents` 里的一个 key。
    pub default_agent: String,
    pub agents: BTreeMap<String, AgentProfile>,
}

impl AgentConfig {
    pub fn get(&self, id: &str) -> Option<&AgentProfile> {
        self.agents.get(id)
    }

    /// 这一份配置里的默认 Agent。校验过了就一定在（[`AgentConfig::get`] 的 `unwrap` 只在
    /// "装配前没校验"时才会炸，那是程序错误，不是配置错误）。
    pub fn default_profile(&self) -> &AgentProfile {
        self.agents
            .get(&self.default_agent)
            .expect("default_agent 指向一个声明过的 Agent；校验保证")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Vec<String> {
        ["read", "rg", "write"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn a_surface_is_the_catalog_when_no_tools_are_listed() {
        let profile = AgentProfile::new("assistant");
        let (surface, unknown) = profile.surface(&catalog());
        assert_eq!(surface.names(), ["read", "rg", "write"]);
        assert!(unknown.is_empty());
    }

    #[test]
    fn an_empty_tool_list_means_no_tools_at_all() {
        let profile = AgentProfile {
            tools: Some(Vec::new()),
            ..AgentProfile::new("reader")
        };
        let (surface, unknown) = profile.surface(&catalog());
        assert!(surface.names().is_empty(), "{:?}", surface.names());
        assert!(unknown.is_empty());
    }

    #[test]
    fn a_tool_that_is_not_installed_is_reported_not_adopted() {
        let profile = AgentProfile {
            tools: Some(vec!["read".into(), "teleport".into()]),
            ..AgentProfile::new("reader")
        };
        let (surface, unknown) = profile.surface(&catalog());
        assert_eq!(surface.names(), ["read"]);
        assert_eq!(unknown, ["teleport"]);
    }

    #[test]
    fn the_revision_follows_the_content() {
        let mut profile = AgentProfile::new("assistant");
        let before = profile.revision();
        assert_eq!(profile.revision(), before, "内容没变，指纹不变");
        profile.instructions = Some("你是 komo。".into());
        assert_ne!(profile.revision(), before, "改了指令就是另一版");
    }

    #[test]
    fn the_default_profile_must_be_declared() {
        let config = AgentConfig {
            default_agent: "assistant".into(),
            agents: BTreeMap::from([("assistant".into(), AgentProfile::new("assistant"))]),
        };
        assert_eq!(config.default_profile().id, "assistant");
        assert!(config.get("coder").is_none());
    }
}
