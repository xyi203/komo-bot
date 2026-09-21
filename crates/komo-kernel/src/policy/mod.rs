//! Policy：统一决策（§7）。
//!
//! 一个**同步纯函数**。授权、来源、当前有效范围都通过 [`PolicyContext`] 传入，内部
//! 不查库、不读时钟、不访问网络（§13.5）。
//!
//! 优先级（§7.1）：
//!
//! ```text
//! 执行环境不可突破的限制
//!   > 明确 Deny
//!   > 有效的范围授权
//!   > 配置 Allow
//!   > 默认 Ask
//! ```
//!
//! **Deny 不可被任何授权覆盖**：Deny 规则是全表扫描，先于授权判定，且没有任何一条
//! 路径能在它之后把结论改回 Allow。

mod defaults;
mod grants;
mod rules;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use grants::{Grant, GrantScope, scope_for};
pub use rules::{
    Effect, IsolationCapability, Matcher, OperationMatch, PathMatch, PolicyRule, RuleTable,
};

use crate::types::chat::{ApprovalScope, Principal};
use crate::types::plan::{ExecutionPlan, Proof};
use crate::types::tool::WorkspaceRoot;

/// 决策结果。三个变体各带 `reason`——界面要显示"为什么"（§7.2），而事后问"这东西
/// 当初怎么过的"只有一个 `true` 是答不出来的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow {
        reason: String,
    },
    Ask {
        reason: String,
        /// 这条请求可以被批到哪些范围（§7.2、§11.3 的 `/approve <id> run`）。
        /// 默认只有 [`ApprovalScope::Once`]：把一个动作预批到更大范围，等于替一个还
        /// 没人看过的后续动作签字。
        #[serde(default = "once_only")]
        scopes: Vec<ApprovalScope>,
    },
    /// 明确拒绝。
    ///
    /// **拿到它的 executor 不去消费任何授权**——不调
    /// [`ApprovalRepo::consume`](crate::traits::ApprovalRepo::consume)，不写
    /// `GrantUse`，直接把拒绝作为结果交回模型（§7.4）。"审批不覆盖显式 Deny"在引擎里
    /// 是梯子的顺序（Deny 在授权之前、全表扫描），在执行侧就是这一条：**没有一条代码
    /// 路径能从 Deny 走到一次消费**。
    Deny {
        reason: String,
    },
}

fn once_only() -> Vec<ApprovalScope> {
    vec![ApprovalScope::Once]
}

impl PolicyDecision {
    /// 只批这一次的 Ask。
    pub fn ask(reason: impl Into<String>) -> Self {
        PolicyDecision::Ask {
            reason: reason.into(),
            scopes: once_only(),
        }
    }

    pub fn allow(reason: impl Into<String>) -> Self {
        PolicyDecision::Allow {
            reason: reason.into(),
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        PolicyDecision::Deny {
            reason: reason.into(),
        }
    }

    /// **只有 `Allow` 换得出** [`Proof`]（§4）。
    pub fn into_proof(self) -> Option<Proof> {
        match self {
            PolicyDecision::Allow { .. } => Some(Proof::policy_allow()),
            PolicyDecision::Ask { .. } | PolicyDecision::Deny { .. } => None,
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            PolicyDecision::Allow { reason }
            | PolicyDecision::Ask { reason, .. }
            | PolicyDecision::Deny { reason } => reason,
        }
    }

    pub fn is_allow(&self) -> bool {
        matches!(self, PolicyDecision::Allow { .. })
    }

    pub fn is_deny(&self) -> bool {
        matches!(self, PolicyDecision::Deny { .. })
    }
}

/// 决策的上下文。
///
/// 注意这里**没有**来源、工具、真实路径、cwd、版本的副本——它们全在
/// [`ExecutionPlan`] 里。审批绑定的是那份不可变计划，界面显示的也是它；在上下文里
/// 再放一份，就有了两个可能不一致的答案，而规则会去匹配审批没有展示过的那一个。
pub struct PolicyContext<'a> {
    /// 已有的有效授权（`policy_grants`）。由调用方从数据库读出并按当前时刻过滤前的
    /// 全集传入——是否过期由 [`PolicyContext::now`] 判定。
    pub grants: &'a [Grant],
    /// 谁在请求。无人值守的来源没有 Principal。
    pub principal: Option<&'a Principal>,
    /// 当前已授权的根目录：workspace、artifacts、skills（只读）等。
    pub roots: &'a [WorkspaceRoot],
    /// 现在几点。kernel 不读时钟——授权有效期按它判定。
    pub now: OffsetDateTime,
    /// 执行环境能不能约束任意代码（§7.3）。
    pub isolation: IsolationCapability,
    /// **受保护路径**：komo 自己的状态（数据目录下的 `sessions/`、`state.db*`、`runtime/`、
    /// `.env`）。工具不许写它们（§8.10 第 4 条）。名单由调用方按本机数据目录算出来，
    /// 不写进规则表——规则表是配置，不该背着某一台机器的 home 目录。
    pub protected: &'a [PathBuf],
}

impl PolicyContext<'_> {
    /// 这个路径落在哪个已授权根里；落不进任何一个 → None。
    pub fn root_for(&self, path: &Path) -> Option<&WorkspaceRoot> {
        self.roots
            .iter()
            .filter(|root| path.starts_with(&root.path))
            .max_by_key(|root| root.path.as_os_str().len())
    }

    /// 这个路径碰到了受保护范围吗（§8.10）。
    pub fn touches_protected(&self, path: &Path) -> bool {
        self.protected.iter().any(|prefix| path.starts_with(prefix))
    }

    /// 计划有没有碰到已授权根之外的东西。
    pub fn touches_outside_roots(&self, plan: &ExecutionPlan) -> bool {
        plan.targets
            .iter()
            .filter_map(|target| target.path())
            .any(|path| self.root_for(path).is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allow_turns_into_a_proof() {
        assert!(PolicyDecision::allow("r").into_proof().is_some());
        assert!(PolicyDecision::ask("r").into_proof().is_none());
        assert!(PolicyDecision::deny("r").into_proof().is_none());
    }

    #[test]
    fn an_ask_defaults_to_this_call_only() {
        let PolicyDecision::Ask { scopes, .. } = PolicyDecision::ask("r") else {
            panic!()
        };
        assert_eq!(scopes, vec![ApprovalScope::Once]);
    }

    #[test]
    fn a_stored_ask_without_scopes_reads_as_this_call_only() {
        let decision: PolicyDecision =
            serde_json::from_str(r#"{"decision":"ask","reason":"任意 shell"}"#).unwrap();
        assert_eq!(decision, PolicyDecision::ask("任意 shell"));
    }
}
