//! §7.1「操作 → 初始策略建议」那张表。
//!
//! 单独成一个文件是因为它是**数据**，而 [`super::rules`] 是**引擎**：换一套默认建议
//! 不该碰梯子的实现，改梯子也不该顺手动到默认值。表里每一行在下面有一个对应的
//! 测试。

use super::rules::{Effect, Matcher, OperationMatch, PathMatch, PolicyRule, RuleTable};
use crate::types::chat::ApprovalScope;
use crate::types::plan::SourceKind;

impl Default for RuleTable {
    fn default() -> Self {
        RuleTable::initial()
    }
}

impl RuleTable {
    /// §7.1「初始策略建议」那张表，逐行。
    pub fn initial() -> Self {
        let rule = |id: &str, effect, reason: &str, matcher| PolicyRule {
            id: id.into(),
            effect,
            reason: reason.into(),
            matcher,
            scopes: vec![ApprovalScope::Once],
            requires_isolation: false,
        };

        RuleTable {
            rules: vec![
                // 第 9 行：权限扩大或修改 Policy —— 通过操作者配置流程处理，
                // 不能由模型自行放宽。
                rule(
                    "policy-change",
                    Effect::Deny,
                    "权限扩大或修改 Policy 只能走操作者的配置流程",
                    Matcher::operations([OperationMatch::PolicyChange]),
                ),
                // 第 1 行：已授权范围内读取普通文件 → Allow。
                rule(
                    "read-within-roots",
                    Effect::Allow,
                    "已授权范围内读取普通文件",
                    Matcher {
                        operations: Some(vec![OperationMatch::ReadFile]),
                        paths: Some(PathMatch::WithinRoots { writable: false }),
                        ..Default::default()
                    },
                ),
                // 第 2 行：已授权 workspace / artifacts 范围内写入和修改 → Allow，
                // 覆盖时检查版本（版本检查在工具里，见 §4）。
                rule(
                    "write-within-roots",
                    Effect::Allow,
                    "已授权 workspace / artifacts 范围内写入和修改",
                    Matcher {
                        operations: Some(vec![OperationMatch::WriteFile]),
                        paths: Some(PathMatch::WithinRoots { writable: true }),
                        ..Default::default()
                    },
                ),
                // 第 7 行：自动提取记忆与生成索引 → 在配置的来源、模型端点和记忆
                // 范围内 Allow。推断不能自行升级为用户确认（那是 §9.2 的事）。
                rule(
                    "memory-maintenance",
                    Effect::Allow,
                    "配置范围内的自动记忆提取与索引生成",
                    Matcher {
                        operations: Some(vec![OperationMatch::MemoryChange]),
                        sources: Some(vec![SourceKind::Memory]),
                        ..Default::default()
                    },
                ),
                // 第 3 行：访问范围外文件或敏感内容 → Ask。
                rule(
                    "outside-roots",
                    Effect::Ask,
                    "访问已授权范围之外的文件",
                    Matcher {
                        paths: Some(PathMatch::OutsideRoots),
                        ..Default::default()
                    },
                ),
                // 第 4 行：修改启用中的 toolbox 或 Python 环境 → Ask，展示具体差异。
                rule(
                    "toolbox-or-env-change",
                    Effect::Ask,
                    "修改启用中的 toolbox 或 Python 环境",
                    Matcher::operations([
                        OperationMatch::ToolboxChange,
                        OperationMatch::PythonEnvChange,
                    ]),
                ),
                // 第 5 行：任意 shell / Python code → Ask；有匹配的明确执行授权时
                // 允许（授权在梯子上比配置 Allow 更高，见 decide）。
                rule(
                    "arbitrary-code",
                    Effect::Ask,
                    "任意 shell / Python 代码",
                    Matcher::operations([OperationMatch::ShellCommand, OperationMatch::PythonCode]),
                ),
                // §8.6：恢复流程调用模块自带的核对函数。核对是只读的、绑定已审核的模块
                // 版本、由执行器而不是模型发起（`PlanSource::Verification`），默认放行——
                // 否则默认配置下每次核对都答 Unknown，核对函数等于没有。这条排在
                // `python-call` 之前，只匹配核对来源；模型自己发起的 call 走下一条。
                rule(
                    "verification-call",
                    Effect::Allow,
                    "恢复流程的核对调用：只读，且绑定已审核的模块版本",
                    Matcher {
                        sources: Some(vec![SourceKind::Verification]),
                        operations: Some(vec![OperationMatch::PythonCall]),
                        ..Default::default()
                    },
                ),
                // 第 6 行与第 8 行：Python call（含 Memos 的写入 / 修改 / 删除）
                // → 按已审核版本、导出函数、参数与授权范围判断。默认要人看。
                rule(
                    "python-call",
                    Effect::Ask,
                    "调用已保存模块：按已审核版本、导出函数与参数范围判断",
                    Matcher::operations([OperationMatch::PythonCall]),
                ),
            ],
            default: Effect::Ask,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::policy::grants::{Grant, GrantScope};
    use crate::policy::rules::normalize_scopes;
    use crate::policy::{IsolationCapability, PolicyContext, PolicyDecision};
    use crate::types::ids::{
        ApprovalId, CronJobId, GrantId, OperationId, RunId, SessionId, ToolCallId,
    };
    use crate::types::plan::{ExecutionPlan, Operation};
    use crate::types::plan::{PlanSource, PlanTarget, PlanVersions, RecoveryMode, TargetAccess};
    use crate::types::tool::WorkspaceRoot;
    use time::OffsetDateTime;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    fn roots() -> Vec<WorkspaceRoot> {
        vec![
            WorkspaceRoot {
                path: PathBuf::from("/home/u/.komo/workspaces/p"),
                writable: true,
                label: "workspace".into(),
            },
            WorkspaceRoot {
                path: PathBuf::from("/home/u/.komo/skills"),
                writable: false,
                label: "skills".into(),
            },
        ]
    }

    struct Fixture {
        roots: Vec<WorkspaceRoot>,
        grants: Vec<Grant>,
        isolation: IsolationCapability,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                roots: roots(),
                grants: vec![],
                isolation: IsolationCapability::default(),
            }
        }

        fn ctx(&self) -> PolicyContext<'_> {
            PolicyContext {
                grants: &self.grants,
                principal: None,
                roots: &self.roots,
                now: NOW,
                isolation: self.isolation,
            }
        }
    }

    fn plan(tool: &str, operation: Operation, targets: Vec<PlanTarget>) -> ExecutionPlan {
        ExecutionPlan {
            operation_id: OperationId::from_raw("op-1"),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            tool: tool.into(),
            operation,
            run: Some(RunId::from_raw("run-1")),
            tool_call: Some(ToolCallId::from_raw("call-1")),
            args: serde_json::json!({}),
            cwd: Some(PathBuf::from("/home/u/.komo/workspaces/p")),
            targets,
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: RecoveryMode::SafeReread,
        }
    }

    fn target(path: &str, access: TargetAccess) -> PlanTarget {
        PlanTarget {
            path: PathBuf::from(path),
            access,
            expected_version: None,
        }
    }

    // ---- §7.1「操作 → 初始策略建议」逐行 ----

    /// 第 1 行：已授权范围内读取普通文件 → Allow。
    #[test]
    fn row_1_reading_an_ordinary_file_inside_an_authorized_root_is_allowed() {
        let f = Fixture::new();
        let plan = plan(
            "read",
            Operation::ReadFile,
            vec![target(
                "/home/u/.komo/workspaces/p/src/main.rs",
                TargetAccess::Read,
            )],
        );
        let decision = RuleTable::initial().decide(&plan, &f.ctx());
        assert!(decision.is_allow(), "{decision:?}");
        assert!(decision.reason().contains("read-within-roots"));
    }

    /// 第 2 行：已授权 workspace / artifacts 范围内写入和修改 → Allow。
    #[test]
    fn row_2_writing_inside_a_writable_root_is_allowed() {
        let f = Fixture::new();
        let plan = plan(
            "write",
            Operation::WriteFile,
            vec![target(
                "/home/u/.komo/workspaces/p/out.txt",
                TargetAccess::Write,
            )],
        );
        assert!(RuleTable::initial().decide(&plan, &f.ctx()).is_allow());
    }

    /// 第 2 行的反面：只读根里写不进去——那是"范围外"，要问。
    #[test]
    fn row_2_writing_into_a_read_only_root_is_not_allowed_outright() {
        let f = Fixture::new();
        let plan = plan(
            "write",
            Operation::WriteFile,
            vec![target(
                "/home/u/.komo/skills/x/SKILL.md",
                TargetAccess::Write,
            )],
        );
        let decision = RuleTable::initial().decide(&plan, &f.ctx());
        assert!(
            matches!(decision, PolicyDecision::Ask { .. }),
            "{decision:?}"
        );
    }

    /// 第 3 行（前半）：访问范围外文件 → Ask。
    #[test]
    fn row_3_touching_a_file_outside_every_root_asks() {
        let f = Fixture::new();
        let plan = plan(
            "read",
            Operation::ReadFile,
            vec![target("/etc/shadow", TargetAccess::Read)],
        );
        let decision = RuleTable::initial().decide(&plan, &f.ctx());
        let PolicyDecision::Ask { reason, .. } = &decision else {
            panic!("{decision:?}")
        };
        assert!(reason.contains("outside-roots"), "{reason}");
    }

    /// 第 3 行（后半）：命中显式禁用规则则 Deny，**且授权盖不过去**。
    #[test]
    fn row_3_an_explicit_deny_rule_beats_every_grant() {
        let plan = plan(
            "read",
            Operation::ReadFile,
            vec![target("/home/u/.ssh/id_ed25519", TargetAccess::Read)],
        );
        let mut table = RuleTable::initial();
        table.rules.insert(
            0,
            PolicyRule {
                id: "no-secrets".into(),
                effect: Effect::Deny,
                reason: "敏感内容".into(),
                matcher: Matcher {
                    paths: Some(PathMatch::TouchesPrefixes {
                        prefixes: vec![PathBuf::from("/home/u/.ssh")],
                    }),
                    ..Default::default()
                },
                scopes: vec![],
                requires_isolation: false,
            },
        );

        let mut f = Fixture::new();
        assert!(table.decide(&plan, &f.ctx()).is_deny());

        // 给它一条正好覆盖这份计划的一次性授权，仍然是 Deny。
        f.grants.push(Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: GrantScope::Once {
                plan_hash: plan.plan_hash(),
                call: None,
            },
            granted_at: NOW,
            valid_until: None,
            consumed: false,
            reason: "操作者批准".into(),
        });
        let decision = table.decide(&plan, &f.ctx());
        assert!(decision.is_deny(), "审批不覆盖显式 Deny：{decision:?}");
    }

    /// 第 4 行：修改启用中的 toolbox 或 Python 环境 → Ask。
    #[test]
    fn row_4_changing_the_live_toolbox_or_python_env_asks() {
        let f = Fixture::new();
        for operation in [
            Operation::ToolboxChange {
                module: "toolbox.ha".into(),
            },
            Operation::PythonEnvChange,
        ] {
            let plan = plan("python", operation, vec![]);
            let decision = RuleTable::initial().decide(&plan, &f.ctx());
            let PolicyDecision::Ask { reason, .. } = &decision else {
                panic!("{decision:?}")
            };
            assert!(reason.contains("toolbox-or-env-change"), "{reason}");
        }
    }

    /// 第 5 行（前半）：任意 shell / Python code → Ask。
    #[test]
    fn row_5_arbitrary_shell_or_python_asks() {
        let f = Fixture::new();
        for operation in [
            Operation::ShellCommand {
                command: "cargo test".into(),
            },
            Operation::PythonCode,
        ] {
            let plan = plan("shell", operation, vec![]);
            let decision = RuleTable::initial().decide(&plan, &f.ctx());
            let PolicyDecision::Ask { reason, .. } = &decision else {
                panic!("{decision:?}")
            };
            assert!(reason.contains("arbitrary-code"), "{reason}");
        }
    }

    /// 第 5 行（后半）：有匹配的明确执行授权时允许。
    #[test]
    fn row_5_a_matching_command_grant_allows_it() {
        let mut f = Fixture::new();
        f.grants.push(Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: GrantScope::Run {
                run: RunId::from_raw("run-1"),
                matcher: Matcher {
                    command_prefixes: Some(vec!["cargo test".into()]),
                    ..Default::default()
                },
                versions: PlanVersions::default(),
            },
            granted_at: NOW,
            valid_until: None,
            consumed: false,
            reason: "本次 Run 内可以跑测试".into(),
        });

        let allowed = plan(
            "shell",
            Operation::ShellCommand {
                command: "cargo test --workspace".into(),
            },
            vec![],
        );
        assert!(RuleTable::initial().decide(&allowed, &f.ctx()).is_allow());

        // 「同意一次」不等于「今后任意脚本均可执行」。
        let other = plan(
            "shell",
            Operation::ShellCommand {
                command: "rm -rf /".into(),
            },
            vec![],
        );
        assert!(matches!(
            RuleTable::initial().decide(&other, &f.ctx()),
            PolicyDecision::Ask { .. }
        ));
    }

    /// 第 6 行：Python call 按已审核版本、导出函数、参数与授权范围判断。
    #[test]
    fn row_6_a_python_call_is_judged_by_its_module_version_and_scope() {
        let mut f = Fixture::new();
        let mut plan = plan(
            "python",
            Operation::PythonCall {
                module: "toolbox.ha".into(),
                function: "turn_off".into(),
            },
            vec![],
        );
        plan.versions.module = Some("v3".into());

        // 没有授权 → Ask。
        let decision = RuleTable::initial().decide(&plan, &f.ctx());
        let PolicyDecision::Ask { reason, .. } = &decision else {
            panic!("{decision:?}")
        };
        assert!(reason.contains("python-call"), "{reason}");

        // 绑定到 v3 的授权 → Allow。
        f.grants.push(Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: GrantScope::Run {
                run: RunId::from_raw("run-1"),
                matcher: Matcher::modules(["toolbox.ha"]),
                versions: PlanVersions {
                    module: Some("v3".into()),
                    ..Default::default()
                },
            },
            granted_at: NOW,
            valid_until: None,
            consumed: false,
            reason: "已审核 v3".into(),
        });
        assert!(RuleTable::initial().decide(&plan, &f.ctx()).is_allow());

        // 模块升到 v4 → 旧授权失效，重新问。
        plan.versions.module = Some("v4".into());
        assert!(matches!(
            RuleTable::initial().decide(&plan, &f.ctx()),
            PolicyDecision::Ask { .. }
        ));
    }

    /// 第 7 行：自动提取记忆与生成索引，在配置范围内 Allow。
    #[test]
    fn a_verification_call_from_the_recovery_flow_is_allowed_without_a_grant() {
        let f = Fixture::new();
        let mut plan = plan(
            "python",
            Operation::PythonCall {
                module: "toolbox.ha".into(),
                function: "__komo_verify__".into(),
            },
            vec![],
        );
        plan.versions.module = Some("v3".into());
        plan.source = PlanSource::Verification {
            of: ToolCallId::from_raw("call-1"),
        };
        let decision = RuleTable::initial().decide(&plan, &f.ctx());
        assert!(decision.is_allow(), "{decision:?}");
        assert!(
            decision.reason().contains("verification-call"),
            "{}",
            decision.reason()
        );

        // 同一个调用换成模型自己发起的来源 → 仍是第 6 行的 Ask。
        plan.source = PlanSource::Interactive {
            session: SessionId::from_raw("sess-1"),
        };
        let decision = RuleTable::initial().decide(&plan, &f.ctx());
        assert!(
            matches!(decision, PolicyDecision::Ask { .. }),
            "{decision:?}"
        );
    }

    #[test]
    fn row_7_memory_maintenance_is_allowed_within_its_configured_scope() {
        let f = Fixture::new();
        let mut plan = plan("memory", Operation::MemoryChange, vec![]);
        plan.source = PlanSource::Memory {
            session: Some(SessionId::from_raw("sess-1")),
        };
        assert!(RuleTable::initial().decide(&plan, &f.ctx()).is_allow());

        // 同样的操作从别处发起就不在这条规则的范围里了。
        plan.source = PlanSource::Interactive {
            session: SessionId::from_raw("sess-1"),
        };
        assert!(matches!(
            RuleTable::initial().decide(&plan, &f.ctx()),
            PolicyDecision::Ask { .. }
        ));
    }

    /// 第 8 行：Memos 的写入、修改或删除按模块版本、函数、参数与用户指令范围审核。
    #[test]
    fn row_8_writing_to_memos_is_judged_like_any_reviewed_module_call() {
        let mut f = Fixture::new();
        let plan = plan(
            "python",
            Operation::PythonCall {
                module: "toolbox.memos".into(),
                function: "create".into(),
            },
            vec![],
        );
        assert!(matches!(
            RuleTable::initial().decide(&plan, &f.ctx()),
            PolicyDecision::Ask { .. }
        ));

        // 一条只覆盖 `toolbox.ha` 的授权不会顺手把 Memos 也放行。
        f.grants.push(Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: GrantScope::Run {
                run: RunId::from_raw("run-1"),
                matcher: Matcher::modules(["toolbox.ha"]),
                versions: PlanVersions::default(),
            },
            granted_at: NOW,
            valid_until: None,
            consumed: false,
            reason: "只批了 HA".into(),
        });
        assert!(matches!(
            RuleTable::initial().decide(&plan, &f.ctx()),
            PolicyDecision::Ask { .. }
        ));
    }

    /// 第 9 行：权限扩大或修改 Policy —— 不能由模型自行放宽。
    #[test]
    fn row_9_the_model_can_never_widen_its_own_policy() {
        let mut f = Fixture::new();
        let plan = plan("python", Operation::PolicyChange, vec![]);
        assert!(RuleTable::initial().decide(&plan, &f.ctx()).is_deny());

        f.grants.push(Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: GrantScope::Once {
                plan_hash: plan.plan_hash(),
                call: None,
            },
            granted_at: NOW,
            valid_until: None,
            consumed: false,
            reason: "有人点了批准".into(),
        });
        assert!(
            RuleTable::initial().decide(&plan, &f.ctx()).is_deny(),
            "一条审批也不能把它放开"
        );
    }

    // ---- 梯子本身 ----

    #[test]
    fn a_deny_rule_requiring_isolation_refuses_arbitrary_code_when_there_is_none() {
        let mut table = RuleTable::initial();
        table.rules.insert(
            0,
            PolicyRule {
                id: "no-network".into(),
                effect: Effect::Deny,
                reason: "禁止网络访问".into(),
                matcher: Matcher::default(),
                scopes: vec![],
                requires_isolation: true,
            },
        );
        let mut f = Fixture::new();
        let plan = plan(
            "shell",
            Operation::ShellCommand {
                command: "curl example.com".into(),
            },
            vec![],
        );
        let decision = table.decide(&plan, &f.ctx());
        assert!(decision.is_deny(), "{decision:?}");
        assert!(
            decision.reason().contains("无法约束"),
            "{}",
            decision.reason()
        );

        // 有了真沙箱，这条硬线就不再兜底，落回普通规则。
        f.isolation.confines_arbitrary_code = true;
        assert!(table.decide(&plan, &f.ctx()).is_deny(), "规则本身仍是 Deny");
    }

    #[test]
    fn an_empty_table_asks_about_everything() {
        let f = Fixture::new();
        let plan = plan(
            "read",
            Operation::ReadFile,
            vec![target(
                "/home/u/.komo/workspaces/p/a.txt",
                TargetAccess::Read,
            )],
        );
        let decision = RuleTable::empty().decide(&plan, &f.ctx());
        assert!(matches!(decision, PolicyDecision::Ask { .. }));
    }

    #[test]
    fn a_cron_grant_does_not_leak_to_an_interactive_run() {
        let mut f = Fixture::new();
        f.grants.push(Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: GrantScope::CronJob {
                job: CronJobId::from_raw("job-1"),
                job_version: 1,
                matcher: Matcher::modules(["toolbox.memos"]),
                versions: PlanVersions::default(),
            },
            granted_at: NOW,
            valid_until: None,
            consumed: false,
            reason: "早报任务".into(),
        });
        let plan = plan(
            "python",
            Operation::PythonCall {
                module: "toolbox.memos".into(),
                function: "create".into(),
            },
            vec![],
        );
        assert!(matches!(
            RuleTable::initial().decide(&plan, &f.ctx()),
            PolicyDecision::Ask { .. }
        ));
    }

    #[test]
    fn an_ask_rule_that_forgot_once_still_offers_it() {
        let scopes = normalize_scopes(&[ApprovalScope::Run]);
        assert_eq!(scopes, vec![ApprovalScope::Once, ApprovalScope::Run]);
        assert_eq!(normalize_scopes(&[]), vec![ApprovalScope::Once]);
    }
}
