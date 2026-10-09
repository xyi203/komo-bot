//! 组合计划（`edit` / `write` 带 `then_run`）的判决（§7.1）。
//!
//! 两步**合并授权、没有半截**：任何一步不放行，两步都不跑。顺序：
//!
//! ```text
//! 形状不对（validate_steps）                          → Deny
//! 每一步：执行环境限制 + Deny 全表扫描               → 任一命中即 Deny（理由点名那一步）
//! 绑定整份组合计划哈希的一次性授权（无 grant_proof）  → Allow
//! 每一步：grant_proof Ask > 范围授权 > 配置 Allow > 配置 Ask > 默认
//!   → 取最严（Deny > Ask > Allow），Ask 只给 Once
//! ```
//!
//! 范围授权**逐步**匹配：每一步要么自己被某条授权盖住、要么自己被配置 Allow——一步的
//! 授权替不了另一步。

use super::grants::GrantScope;
use super::rules::RuleTable;
use super::{PolicyContext, PolicyDecision};
use crate::types::chat::ApprovalScope;
use crate::types::plan::ExecutionPlan;

/// 理由里每一步的称呼，按 [`ExecutionPlan::steps`] 的顺序。
const LABELS: [&str; 2] = ["改动", "然后运行"];

pub(super) fn decide(
    table: &RuleTable,
    plan: &ExecutionPlan,
    ctx: &PolicyContext<'_>,
) -> PolicyDecision {
    if let Err(error) = plan.validate_steps() {
        return PolicyDecision::deny(format!("组合计划不合法：{error}"));
    }
    let steps: Vec<(&str, &ExecutionPlan)> = LABELS.into_iter().zip(plan.steps()).collect();

    for (label, step) in &steps {
        if let Some(reason) = table.hard_deny(step, ctx) {
            return PolicyDecision::deny(format!("{label}：{reason}"));
        }
    }

    // 一次性授权绑的是整份计划的哈希（含 `then_run`），所以它盖住的正是操作者看过的
    // 那两步。`grant_proof` 的 Ask 照单一计划的规矩：任何授权都不替它作答。
    let gated = steps
        .iter()
        .any(|(_, step)| table.grant_proof_ask(step, ctx).is_some());
    if !gated
        && let Some(grant) = ctx.grants.iter().find(|grant| {
            matches!(grant.scope, GrantScope::Once { .. }) && grant.covers(plan, ctx.now)
        })
    {
        return PolicyDecision::allow(format!("已有授权 {}：{}", grant.id, grant.reason));
    }

    let decided = steps
        .into_iter()
        .map(|(label, step)| (label, step_decision(table, step, ctx)))
        .collect();
    strictest(decided)
}

/// 一步在 Deny 之后的梯子。
fn step_decision(
    table: &RuleTable,
    step: &ExecutionPlan,
    ctx: &PolicyContext<'_>,
) -> PolicyDecision {
    if let Some(ask) = table.grant_proof_ask(step, ctx) {
        return ask;
    }
    if let Some(grant) = ctx
        .grants
        .iter()
        .find(|grant| grant.covers_step(step, ctx.now))
    {
        return PolicyDecision::allow(format!("已有授权 {}：{}", grant.id, grant.reason));
    }
    table.configured(step, ctx)
}

/// Deny > Ask > Allow。Ask 只给 Once：更大的范围等于替以后每一次改动预签了这条命令。
fn strictest(decided: Vec<(&str, PolicyDecision)>) -> PolicyDecision {
    if let Some((label, deny)) = decided.iter().find(|(_, decision)| decision.is_deny()) {
        return PolicyDecision::deny(format!("{label}：{}", deny.reason()));
    }
    let reason = decided
        .iter()
        .map(|(label, decision)| format!("{label}：{}", decision.reason()))
        .collect::<Vec<_>>()
        .join("；");
    if decided.iter().all(|(_, decision)| decision.is_allow()) {
        PolicyDecision::allow(reason)
    } else {
        PolicyDecision::Ask {
            reason,
            scopes: vec![ApprovalScope::Once],
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::OffsetDateTime;
    use time::macros::datetime;

    use super::*;
    use crate::policy::grants::{Grant, scope_for};
    use crate::policy::rules::{Effect, IsolationCapability, Matcher, OperationMatch, PolicyRule};
    use crate::types::digest::ContentHash;
    use crate::types::ids::ToolCallId;
    use crate::types::ids::{ApprovalId, CronJobId, GrantId, OperationId, RunId, SessionId};
    use crate::types::plan::{
        Operation, PlanSource, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
    };
    use crate::types::tool::WorkspaceRoot;

    const NOW: OffsetDateTime = datetime!(2026-10-10 08:00:00 UTC);

    fn roots() -> Vec<WorkspaceRoot> {
        vec![WorkspaceRoot {
            path: PathBuf::from("/ws"),
            writable: true,
            label: "workspace".into(),
        }]
    }

    fn ctx<'a>(grants: &'a [Grant], roots: &'a [WorkspaceRoot]) -> PolicyContext<'a> {
        PolicyContext {
            grants,
            principal: None,
            roots,
            now: NOW,
            isolation: IsolationCapability::default(),
            protected: &[],
        }
    }

    fn edit(path: &str) -> ExecutionPlan {
        ExecutionPlan {
            operation_id: OperationId::from_raw("op-1"),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            tool: "edit".into(),
            operation: Operation::WriteFile,
            run: Some(RunId::from_raw("run-1")),
            tool_call: Some(ToolCallId::from_raw("call-1")),
            args: serde_json::json!({ "path": path }),
            cwd: Some(PathBuf::from("/ws")),
            targets: vec![PlanTarget::local(path, TargetAccess::Write)],
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: RecoveryMode::VerifyTarget,
            then_run: None,
        }
    }

    fn shell(command: &str) -> ExecutionPlan {
        ExecutionPlan {
            tool: "shell".into(),
            operation: Operation::ShellCommand {
                command: command.into(),
            },
            args: serde_json::json!({ "command": command }),
            targets: vec![],
            versions: PlanVersions {
                code: Some(ContentHash::of_str(command)),
                ..Default::default()
            },
            recovery: RecoveryMode::NoSafeRecovery,
            ..edit("/ws/unused")
        }
    }

    fn fused(path: &str, command: &str) -> ExecutionPlan {
        ExecutionPlan {
            then_run: Some(Box::new(shell(command))),
            ..edit(path)
        }
    }

    fn rule(id: &str, effect: Effect, operation: OperationMatch) -> PolicyRule {
        PolicyRule {
            id: id.into(),
            effect,
            reason: format!("{id} {effect:?}"),
            matcher: Matcher::operations([operation]),
            scopes: vec![ApprovalScope::Once, ApprovalScope::Run],
            requires_isolation: false,
            grant_proof: false,
        }
    }

    /// 写进根里的放行、shell 要问——strict 的那两行，足够这几个测试用。
    fn strictish() -> RuleTable {
        RuleTable {
            rules: vec![
                PolicyRule {
                    matcher: Matcher {
                        operations: Some(vec![OperationMatch::WriteFile]),
                        paths: Some(crate::policy::PathMatch::WithinRoots { writable: true }),
                        ..Default::default()
                    },
                    ..rule("write-in-roots", Effect::Allow, OperationMatch::WriteFile)
                },
                rule("shell-asks", Effect::Ask, OperationMatch::ShellCommand),
            ],
            default: Effect::Ask,
        }
    }

    fn grant(id: &str, scope: GrantScope) -> Grant {
        Grant {
            id: GrantId::from_raw(id),
            approval: ApprovalId::from_raw(format!("ap-{id}")),
            scope,
            granted_at: NOW,
            valid_until: None,
            consumed: false,
            reason: "操作者批准".into(),
        }
    }

    fn once(plan: &ExecutionPlan) -> Grant {
        grant(
            "g-once",
            GrantScope::Once {
                plan_hash: plan.plan_hash(),
                call: None,
            },
        )
    }

    fn run_scope(id: &str, step: &ExecutionPlan) -> Grant {
        grant(
            id,
            scope_for(step, ApprovalScope::Run).expect("落得成 Run 范围"),
        )
    }

    #[test]
    fn the_combined_decision_is_the_strictest_of_both_steps() {
        use Effect::{Allow, Ask, Deny};
        let roots = roots();
        let plan = fused("/ws/a.rs", "cargo test");
        for mutation in [Allow, Ask, Deny] {
            for command in [Allow, Ask, Deny] {
                let table = RuleTable {
                    rules: vec![
                        rule("edit-rule", mutation, OperationMatch::WriteFile),
                        rule("cmd-rule", command, OperationMatch::ShellCommand),
                    ],
                    default: Ask,
                };
                let decision = table.decide(&plan, &ctx(&[], &roots));
                let both = format!(
                    "改动：edit-rule {mutation:?}（规则 edit-rule）；然后运行：cmd-rule {command:?}（规则 cmd-rule）"
                );
                let case = format!("{mutation:?} × {command:?}: {decision:?}");
                match (mutation, command) {
                    (Deny, _) => {
                        assert!(decision.is_deny(), "{case}");
                        assert_eq!(decision.reason(), "改动：edit-rule Deny（规则 edit-rule）");
                    }
                    (_, Deny) => {
                        assert!(decision.is_deny(), "{case}");
                        assert_eq!(
                            decision.reason(),
                            "然后运行：cmd-rule Deny（规则 cmd-rule）"
                        );
                    }
                    (Allow, Allow) => {
                        assert!(decision.is_allow(), "{case}");
                        assert_eq!(decision.reason(), both);
                    }
                    _ => {
                        assert_eq!(
                            decision,
                            PolicyDecision::Ask {
                                reason: both,
                                scopes: vec![ApprovalScope::Once],
                            },
                            "{case}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_once_grant_for_the_composite_does_not_cover_a_denied_command() {
        let roots = roots();
        let mut table = strictish();
        table.rules.insert(
            0,
            PolicyRule {
                matcher: Matcher {
                    operations: Some(vec![OperationMatch::ShellCommand]),
                    command_patterns: Some(vec!["rm -rf".into()]),
                    ..Default::default()
                },
                ..rule("no-rm", Effect::Deny, OperationMatch::ShellCommand)
            },
        );

        let denied = fused("/ws/a.rs", "rm -rf /");
        let grants = [once(&denied)];
        let decision = table.decide(&denied, &ctx(&grants, &roots));
        assert!(decision.is_deny(), "{decision:?}");
        assert!(decision.reason().starts_with("然后运行："), "{decision:?}");

        // 同样一条绑定组合哈希的一次性授权，命令没被禁时就盖住两步。
        let asked = fused("/ws/a.rs", "cargo test");
        let grants = [once(&asked)];
        let decision = table.decide(&asked, &ctx(&grants, &roots));
        assert!(decision.is_allow(), "{decision:?}");
        assert!(decision.reason().contains("g-once"), "{decision:?}");
    }

    #[test]
    fn a_once_grant_for_another_command_does_not_cover_the_composite() {
        let roots = roots();
        let approved = fused("/ws/a.rs", "cargo test");
        let replanned = fused("/ws/a.rs", "cargo test --release");
        let grants = [once(&approved)];
        let decision = strictish().decide(&replanned, &ctx(&grants, &roots));
        assert!(
            matches!(decision, PolicyDecision::Ask { .. }),
            "{decision:?}"
        );
    }

    #[test]
    fn a_run_scoped_grant_for_the_command_lets_a_fused_edit_through() {
        let roots = roots();
        let cargo = run_scope("g-cmd", &shell("cargo test"));
        let grants = [cargo.clone()];

        // 改动在根里（配置 Allow），命令有 Run 授权：放行。
        let inside = fused("/ws/a.rs", "cargo test");
        let decision = strictish().decide(&inside, &ctx(&grants, &roots));
        assert!(decision.is_allow(), "{decision:?}");
        assert!(
            decision.reason().contains("然后运行：已有授权 g-cmd"),
            "{decision:?}"
        );

        // 改动出了根（要问）：命令的授权替不了它。
        let outside = fused("/etc/hosts", "cargo test");
        let decision = strictish().decide(&outside, &ctx(&grants, &roots));
        assert!(
            matches!(&decision, PolicyDecision::Ask { reason, .. } if reason.starts_with("改动：默认 Ask")),
            "{decision:?}"
        );

        // 改动自己也有一条 Run 授权：两步各有各的，放行。
        let grants = [cargo, run_scope("g-edit", &edit("/etc/hosts"))];
        let decision = strictish().decide(&outside, &ctx(&grants, &roots));
        assert!(decision.is_allow(), "{decision:?}");
        assert!(
            decision.reason().contains("改动：已有授权 g-edit"),
            "{decision:?}"
        );
    }

    #[test]
    fn a_grant_for_one_step_never_covers_the_other() {
        let roots = roots();
        let table = RuleTable::empty();
        let plan = fused("/ws/a.rs", "cargo test");

        let edit_grant = run_scope("g-edit", &edit("/ws/a.rs"));
        let decision = table.decide(&plan, &ctx(std::slice::from_ref(&edit_grant), &roots));
        assert!(
            matches!(&decision, PolicyDecision::Ask { reason, .. } if reason.contains("然后运行：默认 Ask")),
            "{decision:?}"
        );
        assert!(!edit_grant.covers(&plan, NOW));

        let command_grant = run_scope("g-cmd", &shell("cargo test"));
        let decision = table.decide(&plan, &ctx(std::slice::from_ref(&command_grant), &roots));
        assert!(
            matches!(&decision, PolicyDecision::Ask { reason, .. } if reason.starts_with("改动：默认 Ask")),
            "{decision:?}"
        );
        assert!(!command_grant.covers(&plan, NOW));

        // 一次性授权绑的是单独那一步的哈希：组合计划里哪一步都不认它。
        let step_once = once(&edit("/ws/a.rs"));
        let decision = table.decide(&plan, &ctx(std::slice::from_ref(&step_once), &roots));
        assert!(
            matches!(decision, PolicyDecision::Ask { .. }),
            "{decision:?}"
        );
    }

    #[test]
    fn composite_plans_offer_only_once() {
        let roots = roots();
        let table = RuleTable {
            rules: vec![
                rule("edit-asks", Effect::Ask, OperationMatch::WriteFile),
                rule("cmd-asks", Effect::Ask, OperationMatch::ShellCommand),
            ],
            default: Effect::Ask,
        };
        let mut plan = fused("/ws/a.rs", "cargo test");
        let source = PlanSource::Cron {
            job: CronJobId::from_raw("job-1"),
            job_version: 1,
        };
        plan.source = source.clone();
        plan.then_run.as_mut().unwrap().source = source;

        let PolicyDecision::Ask { scopes, .. } = table.decide(&plan, &ctx(&[], &roots)) else {
            panic!("两步都要问")
        };
        assert_eq!(scopes, vec![ApprovalScope::Once]);
        for scope in [ApprovalScope::Run, ApprovalScope::CronJob] {
            assert!(scope_for(&plan, scope).is_none(), "{scope:?}");
        }
    }

    #[test]
    fn a_malformed_composite_is_denied() {
        let roots = roots();
        let table = RuleTable {
            rules: vec![],
            default: Effect::Allow,
        };

        let mut nested = fused("/ws/a.rs", "cargo test");
        nested.then_run.as_mut().unwrap().then_run = Some(Box::new(shell("ls")));
        let mut not_shell = edit("/ws/a.rs");
        not_shell.then_run = Some(Box::new(edit("/ws/b.rs")));
        let mut other_run = fused("/ws/a.rs", "cargo test");
        other_run.then_run.as_mut().unwrap().run = Some(RunId::from_raw("run-2"));

        for plan in [nested, not_shell, other_run] {
            let decision = table.decide(&plan, &ctx(&[], &roots));
            assert!(decision.is_deny(), "{decision:?}");
            assert!(
                decision.reason().starts_with("组合计划不合法"),
                "{decision:?}"
            );
        }
    }

    #[test]
    fn the_isolation_hardline_applies_to_the_command_step() {
        let roots = roots();
        let mut table = strictish();
        table.rules.push(PolicyRule {
            requires_isolation: true,
            ..rule("no-network", Effect::Deny, OperationMatch::ShellCommand)
        });
        let plan = fused("/ws/a.rs", "cargo test");
        let grants = [once(&plan)];
        let decision = table.decide(&plan, &ctx(&grants, &roots));
        assert!(decision.is_deny(), "{decision:?}");
        assert!(
            decision.reason().starts_with("然后运行：规则 no-network"),
            "{decision:?}"
        );
    }

    #[test]
    fn a_grant_proof_command_is_not_answered_by_a_composite_once_grant() {
        let roots = roots();
        let mut table = strictish();
        table.rules.insert(
            0,
            PolicyRule {
                matcher: Matcher {
                    operations: Some(vec![OperationMatch::ShellCommand]),
                    command_patterns: Some(vec!["komo cron add".into()]),
                    ..Default::default()
                },
                grant_proof: true,
                scopes: vec![],
                ..rule("cron-add", Effect::Ask, OperationMatch::ShellCommand)
            },
        );
        let plan = fused("/ws/a.rs", "komo cron add --every 1h 'ls'");
        let grants = [once(&plan)];
        let decision = table.decide(&plan, &ctx(&grants, &roots));
        assert!(
            matches!(&decision, PolicyDecision::Ask { reason, scopes } if reason.contains("然后运行：cron-add") && scopes == &vec![ApprovalScope::Once]),
            "{decision:?}"
        );
    }
}
