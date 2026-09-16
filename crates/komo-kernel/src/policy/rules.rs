//! 规则表与默认规则（§7.1）。
//!
//! 规则是**数据**：config（policy.toml）解析出一张表，引擎按固定顺序扫描它。模型给
//! 的"风险低"、Python 模块自称"安全"、Skill 里写的"可以直接执行"都不进这张表——
//! Policy 只看 [`ExecutionPlan`]。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{PolicyContext, PolicyDecision};
use crate::types::chat::ApprovalScope;
use crate::types::plan::{ExecutionPlan, Operation, SourceKind};

/// 一条规则的效果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

/// 执行环境能不能约束任意代码（§7.3）。
///
/// 「若配置要求强制禁止某类文件或网络访问，而当前执行环境无法约束任意代码，Policy
/// **必须拒绝**不受约束的 shell / Python」——不能一面声明绝对禁止，一面让脚本任意
/// 访问。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationCapability {
    /// 有真进程沙箱。首版是 `false`：cwd 和参数检查不是完整进程沙箱。
    #[serde(default)]
    pub confines_arbitrary_code: bool,
}

/// 按操作类别匹配。这是 [`Operation`] 去掉负载后的判别式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationMatch {
    ReadFile,
    WriteFile,
    ShellCommand,
    PythonCode,
    PythonCall,
    ToolboxChange,
    PythonEnvChange,
    MemoryChange,
    PolicyChange,
}

impl OperationMatch {
    pub fn of(operation: &Operation) -> Self {
        match operation {
            Operation::ReadFile => OperationMatch::ReadFile,
            Operation::WriteFile => OperationMatch::WriteFile,
            Operation::ShellCommand { .. } => OperationMatch::ShellCommand,
            Operation::PythonCode => OperationMatch::PythonCode,
            Operation::PythonCall { .. } => OperationMatch::PythonCall,
            Operation::ToolboxChange { .. } => OperationMatch::ToolboxChange,
            Operation::PythonEnvChange => OperationMatch::PythonEnvChange,
            Operation::MemoryChange => OperationMatch::MemoryChange,
            Operation::PolicyChange => OperationMatch::PolicyChange,
        }
    }
}

/// 按真实目标路径匹配。看的是 [`crate::types::plan::PlanTarget::path`]（已解析符号
/// 链接的真实路径），不是模型给的原始参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PathMatch {
    /// **所有**目标都落在已授权根里。
    WithinRoots {
        /// 还要求那个根是可写的。
        #[serde(default)]
        writable: bool,
    },
    /// **至少一个**目标落在所有已授权根之外。
    OutsideRoots,
    /// 所有目标都落在给定前缀里。
    WithinPrefixes { prefixes: Vec<PathBuf> },
    /// 至少一个目标碰到了给定前缀——敏感路径的禁用规则用它。
    TouchesPrefixes { prefixes: Vec<PathBuf> },
}

/// 一条规则的匹配条件。字段都是 `Option`，`None` = 这一维不约束；给出的条件全部成立
/// 才算命中（逻辑与）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Matcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<SourceKind>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operations: Option<Vec<OperationMatch>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<PathMatch>,
    /// 按 toolbox 模块名匹配（`python` 的 call 模式、toolbox 变更）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modules: Option<Vec<String>>,
    /// 命令必须以其中之一开头——"明确命令模板"的范围授权用它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_prefixes: Option<Vec<String>>,
}

impl Matcher {
    /// 只按操作类别匹配。
    pub fn operations<I: IntoIterator<Item = OperationMatch>>(operations: I) -> Self {
        Matcher {
            operations: Some(operations.into_iter().collect()),
            ..Default::default()
        }
    }

    /// 只按模块名匹配。
    pub fn modules<I: IntoIterator<Item = S>, S: Into<String>>(modules: I) -> Self {
        Matcher {
            modules: Some(modules.into_iter().map(Into::into).collect()),
            ..Default::default()
        }
    }

    /// 完整匹配，路径条件按上下文的已授权根判定。
    pub fn matches(&self, plan: &ExecutionPlan, ctx: &PolicyContext<'_>) -> bool {
        self.matches_without_paths(plan) && self.paths_match(plan, Some(ctx))
    }

    /// 授权用的匹配：没有上下文，所以 `WithinRoots` / `OutsideRoots` 这两个**相对于
    /// 运行时才知道的根**的条件一律不命中。授权是写死在数据库里的一句话，不能因为
    /// 后来有人改宽了根目录就跟着变宽。
    pub fn matches_scope(&self, plan: &ExecutionPlan) -> bool {
        self.matches_without_paths(plan) && self.paths_match(plan, None)
    }

    fn matches_without_paths(&self, plan: &ExecutionPlan) -> bool {
        if let Some(tools) = &self.tools
            && !tools.iter().any(|t| t == &plan.tool)
        {
            return false;
        }
        if let Some(sources) = &self.sources
            && !sources.contains(&plan.source.kind())
        {
            return false;
        }
        if let Some(operations) = &self.operations
            && !operations.contains(&OperationMatch::of(&plan.operation))
        {
            return false;
        }
        if let Some(modules) = &self.modules {
            let module = match &plan.operation {
                Operation::PythonCall { module, .. } | Operation::ToolboxChange { module } => {
                    Some(module)
                }
                _ => None,
            };
            match module {
                Some(name) if modules.iter().any(|m| m == name) => {}
                _ => return false,
            }
        }
        if let Some(prefixes) = &self.command_prefixes {
            let Operation::ShellCommand { command } = &plan.operation else {
                return false;
            };
            if !prefixes.iter().any(|p| command.starts_with(p.as_str())) {
                return false;
            }
        }
        true
    }

    fn paths_match(&self, plan: &ExecutionPlan, ctx: Option<&PolicyContext<'_>>) -> bool {
        let Some(condition) = &self.paths else {
            return true;
        };
        match condition {
            PathMatch::WithinRoots { writable } => {
                let Some(ctx) = ctx else { return false };
                // 一个目标都没有的计划谈不上"落在根里"。
                !plan.targets.is_empty()
                    && plan.targets.iter().all(|target| {
                        ctx.root_for(&target.path)
                            .is_some_and(|root| !*writable || root.writable)
                    })
            }
            PathMatch::OutsideRoots => {
                let Some(ctx) = ctx else { return false };
                ctx.touches_outside_roots(plan)
            }
            PathMatch::WithinPrefixes { prefixes } => {
                !plan.targets.is_empty()
                    && plan
                        .targets
                        .iter()
                        .all(|t| prefixes.iter().any(|p| t.path.starts_with(p)))
            }
            PathMatch::TouchesPrefixes { prefixes } => plan
                .targets
                .iter()
                .any(|t| prefixes.iter().any(|p| t.path.starts_with(p))),
        }
    }
}

/// 一条规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRule {
    /// 规则名，出现在决策理由里——事后要能指着一条规则说"是它放的行"。
    pub id: String,
    pub effect: Effect,
    pub reason: String,
    #[serde(default)]
    pub matcher: Matcher,
    /// `Ask` 命中时这条请求可以批到哪些范围。其他效果下忽略。
    #[serde(default)]
    pub scopes: Vec<ApprovalScope>,
    /// 这条 `Deny` 要求执行环境真的能约束任意代码；做不到时，落在它范围内的任意
    /// shell / Python 一律拒绝（§7.3）。
    #[serde(default)]
    pub requires_isolation: bool,
}

/// 规则表 = Policy 的全部配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleTable {
    pub rules: Vec<PolicyRule>,
    /// 什么都没命中时的结论。默认 `Ask`。
    #[serde(default = "default_effect")]
    pub default: Effect,
}

fn default_effect() -> Effect {
    Effect::Ask
}

impl RuleTable {
    /// 一张什么都不允许自动放行的空表——测试与"最保守"的起点。
    pub fn empty() -> Self {
        RuleTable {
            rules: Vec::new(),
            default: Effect::Ask,
        }
    }

    /// **同步纯函数**（§13.5）。梯子见模块文档。
    pub fn decide(&self, plan: &ExecutionPlan, ctx: &PolicyContext<'_>) -> PolicyDecision {
        // 0. 执行环境不可突破的限制。
        if let Some(reason) = self.isolation_hardline(plan, ctx) {
            return PolicyDecision::deny(reason);
        }

        // 1. 明确 Deny。全表扫描，且在授权之前——**Deny 不可被任何授权覆盖**。
        if let Some(rule) = self.first_match(Effect::Deny, plan, ctx) {
            return PolicyDecision::deny(format!("{}（规则 {}）", rule.reason, rule.id));
        }

        // 2. 有效的范围授权。
        if let Some(grant) = ctx.grants.iter().find(|g| g.covers(plan, ctx.now)) {
            return PolicyDecision::allow(format!("已有授权 {}：{}", grant.id, grant.reason));
        }

        // 3. 配置 Allow。
        if let Some(rule) = self.first_match(Effect::Allow, plan, ctx) {
            return PolicyDecision::allow(format!("{}（规则 {}）", rule.reason, rule.id));
        }

        // 4. 配置 Ask。
        if let Some(rule) = self.first_match(Effect::Ask, plan, ctx) {
            return PolicyDecision::Ask {
                reason: format!("{}（规则 {}）", rule.reason, rule.id),
                scopes: normalize_scopes(&rule.scopes),
            };
        }

        // 5. 默认。
        match self.default {
            Effect::Allow => PolicyDecision::allow("默认 Allow"),
            Effect::Ask => PolicyDecision::ask("默认 Ask：没有规则覆盖这个操作"),
            Effect::Deny => PolicyDecision::deny("默认 Deny"),
        }
    }

    /// 命中了一条"需要隔离才谈得上有效"的禁用规则，而执行环境约束不了任意代码。
    fn isolation_hardline(&self, plan: &ExecutionPlan, ctx: &PolicyContext<'_>) -> Option<String> {
        if ctx.isolation.confines_arbitrary_code || !plan.operation.is_arbitrary_code() {
            return None;
        }
        let rule = self
            .rules
            .iter()
            .find(|r| r.effect == Effect::Deny && r.requires_isolation)?;
        Some(format!(
            "规则 {} 要求强制禁止，而当前执行环境无法约束任意 shell / Python（§7.3）",
            rule.id
        ))
    }

    fn first_match(
        &self,
        effect: Effect,
        plan: &ExecutionPlan,
        ctx: &PolicyContext<'_>,
    ) -> Option<&PolicyRule> {
        self.rules
            .iter()
            .find(|rule| rule.effect == effect && rule.matcher.matches(plan, ctx))
    }
}

pub(super) fn normalize_scopes(scopes: &[ApprovalScope]) -> Vec<ApprovalScope> {
    if scopes.is_empty() {
        vec![ApprovalScope::Once]
    } else {
        let mut out = scopes.to_vec();
        if !out.contains(&ApprovalScope::Once) {
            out.insert(0, ApprovalScope::Once);
        }
        out
    }
}

/// 让调用方可以把一张表直接当 [`crate::traits::Policy`] 用。
impl crate::traits::Policy for RuleTable {
    fn decide(&self, plan: &ExecutionPlan, ctx: &PolicyContext<'_>) -> PolicyDecision {
        RuleTable::decide(self, plan, ctx)
    }
}
