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

/// 建一条规则的小工厂：`scopes` 与 `requires_isolation` 两套默认建议里都一样。
fn rule_maker() -> impl Fn(&str, Effect, &str, Matcher) -> PolicyRule {
    |id: &str, effect, reason: &str, matcher| PolicyRule {
        id: id.into(),
        effect,
        reason: reason.into(),
        matcher,
        scopes: vec![ApprovalScope::Once],
        requires_isolation: false,
        grant_proof: false,
    }
}

impl RuleTable {
    /// §7.1「初始策略建议」那张表，逐行。
    pub fn initial() -> Self {
        let rule = rule_maker();

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
                // §8.10 第 4 条：komo 自己的状态不是工具的目标。
                //
                // 这条是**网**，不是墙：文件工具的目标路径是解析后的真实路径，拦得住；
                // 任意 shell / Python 只有 §7.3 那一层（没有沙箱时它本来就拥有这个账号
                // 的全部权限），再加 §7.1 的 `command_patterns` 兜手滑形状。真正的保证
                // 在 reconcile：目录没了而状态不是 `purged`，它会报出来（§8.9）。
                rule(
                    "komo-state-write",
                    Effect::Deny,
                    "komo 的状态目录与数据库不是工具的写入目标（§8.10）",
                    Matcher {
                        operations: Some(vec![OperationMatch::WriteFile]),
                        paths: Some(PathMatch::TouchesProtected),
                        ..Default::default()
                    },
                ),
                // 第 1 行（虚拟入口那一半）：读这次的**工具说明与 schema**（`tool://`，§六）
                // → Allow。虚拟入口没有磁盘目标，"落在哪个根里"对它不成立；真正管住它的是
                // 计划那一刻的能力面检查：不在这次面上的工具根本进不了计划。
                rule(
                    "read-virtual-resource",
                    Effect::Allow,
                    "读取这次能力面里的工具说明与 schema（tool://）",
                    Matcher {
                        operations: Some(vec![OperationMatch::ReadFile]),
                        paths: Some(PathMatch::Virtual),
                        ..Default::default()
                    },
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
                // 第 10 行（新增，堵口子）：`komo cron add` 会给命令 Job 自己签发一条
                // 执行授权（§10、§7.2）——模型经 shell 调它，就是在给自己写将来能免问
                // 的许可。**任何 Run / Cron 范围授权都不能替这一步作答**：`grant_proof`
                // 让它排在"有效的范围授权"那一步之前，答复也永远只能是一次性的
                // （`scopes` 不给 `offered_scopes` 留追加 Cron 范围的机会）。
                PolicyRule {
                    id: "cron-add-always-asks".into(),
                    effect: Effect::Ask,
                    reason: "`cron add` 会为命令 Job 自己签发一条执行授权，这一步必须每次都问"
                        .into(),
                    matcher: Matcher {
                        operations: Some(vec![OperationMatch::ShellCommand]),
                        command_patterns: Some(vec!["komo cron add".into()]),
                        ..Default::default()
                    },
                    scopes: Vec::new(),
                    requires_isolation: false,
                    grant_proof: true,
                },
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
                // 新增两行（`docs/home-dispatcher.md` §4.2、`docs/komo_bot.md` §7.1）：
                // dispatch / follow 只是"开一个任务会话、提交一句话"——strict 与 auto
                // 下都 Allow，任务会话里的每一次调用仍然照常过 Policy 与审批，不放宽
                // 任何一层。`auto()` 不必重复这两条：它的默认结论本来就是 Allow。
                rule(
                    "dispatch-allow",
                    Effect::Allow,
                    "dispatch 只是建一个任务会话并提交第一条输入，任务内的调用仍照常过 Policy",
                    Matcher::operations([OperationMatch::Dispatch]),
                ),
                rule(
                    "follow-allow",
                    Effect::Allow,
                    "follow 只是把一句话提交进已有的任务会话，任务内的调用仍照常过 Policy",
                    Matcher::operations([OperationMatch::Follow]),
                ),
            ],
            default: Effect::Ask,
        }
    }

    /// 「auto 模式」：**不审批**（§7.1）。
    ///
    /// 与 [`Self::initial`] 的差别就是把"要人看一眼"整个去掉：`default` 从 `Ask` 变成
    /// `Allow`，并且一条 `Ask` 规则都不留——日常的 shell / Python、已授权范围之外的
    /// 文件、toolbox 变更、模型发起的模块调用，都不再停在等人回答上。
    ///
    /// **剩下唯一一条 `Deny` 是 `policy-change`**，它不是操作者设的边界，而是设计里那句
    /// 「模型不能自行放宽自己的权限」（§7.1 第 9 行）：今天没有哪个工具会产生这个操作，
    /// 留着是为了那天它出现时不必再想一遍。
    ///
    /// **代价要说清楚**：首版没有能约束任意代码的执行环境（`confines_arbitrary_code =
    /// false`，§7.3），所以「不审批」= **这个进程能做的事，agent 都能做**：读 `.env`、
    /// `rm -rf`、把数据发出去，一个都不问。§7.3 那句话的另一面正是这一条：没有沙箱时，
    /// "别打扰我"和"这里有真正的禁区"不可能同时成立。想要一张网，只能操作者自己在文件里
    /// 加规则（`effect = "deny" | "ask"` + `Matcher::command_patterns`），而且得知道那只是
    /// 手滑网、不是边界。
    pub fn auto() -> Self {
        let rule = rule_maker();

        RuleTable {
            rules: vec![rule(
                "policy-change",
                Effect::Deny,
                "权限扩大或修改 Policy 只能走操作者的配置流程",
                Matcher::operations([OperationMatch::PolicyChange]),
            )],
            default: Effect::Allow,
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

    /// §8.10 第 4 条要拦的那些路径。名字与 `WorkspaceRoot` 里的工作区**故意重叠**：
    /// 数据目录下的 `sessions/` 从来不是可写根，两者不冲突，但测试要能分辨。
    fn protected() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/home/u/.komo/sessions"),
            PathBuf::from("/home/u/.komo/state.db"),
            PathBuf::from("/home/u/.komo/runtime"),
            PathBuf::from("/home/u/.komo/.env"),
        ]
    }

    struct Fixture {
        roots: Vec<WorkspaceRoot>,
        grants: Vec<Grant>,
        isolation: IsolationCapability,
        protected: Vec<PathBuf>,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                roots: roots(),
                grants: vec![],
                isolation: IsolationCapability::default(),
                protected: protected(),
            }
        }

        fn ctx(&self) -> PolicyContext<'_> {
            PolicyContext {
                grants: &self.grants,
                principal: None,
                roots: &self.roots,
                now: NOW,
                isolation: self.isolation,
                protected: &self.protected,
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
        PlanTarget::local(path, access)
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

    /// §8.10 第 4 条：工具不许写 komo 自己的状态。**而且授权盖不过去**——它是一条
    /// `Deny`，Deny 在梯子上高于任何授权（§7.1）。
    #[test]
    fn komo_state_is_never_a_write_target_even_with_a_grant() {
        let into_state = plan(
            "write",
            Operation::WriteFile,
            vec![target(
                "/home/u/.komo/sessions/sess-1/events.jsonl",
                TargetAccess::Write,
            )],
        );
        let mut f = Fixture::new();
        let decision = RuleTable::initial().decide(&into_state, &f.ctx());
        assert!(decision.is_deny(), "{decision:?}");
        assert!(decision.reason().contains("komo-state-write"));

        f.grants.push(Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: GrantScope::Once {
                plan_hash: into_state.plan_hash(),
                call: None,
            },
            granted_at: NOW,
            valid_until: None,
            consumed: false,
            reason: "操作者批准".into(),
        });
        assert!(
            RuleTable::initial().decide(&into_state, &f.ctx()).is_deny(),
            "数据目录不是靠审批就能写的"
        );

        // 反过来：这条规则不能顺手把工作区也拦掉——那是第 2 行的地盘。
        let into_workspace = plan(
            "write",
            Operation::WriteFile,
            vec![target(
                "/home/u/.komo/workspaces/p/out.txt",
                TargetAccess::Write,
            )],
        );
        assert!(
            RuleTable::initial()
                .decide(&into_workspace, &f.ctx())
                .is_allow()
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
                grant_proof: false,
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

    /// 新增两行（`docs/home-dispatcher.md` §4.2）：dispatch / follow 在 strict 下也是
    /// Allow——它们只是"开一个任务会话、提交一句话"，不放宽任何一层。
    #[test]
    fn dispatch_and_follow_are_allowed_even_under_the_strict_table() {
        let f = Fixture::new();
        let dispatch = plan(
            "dispatch",
            Operation::Dispatch {
                task: "查一下空调状态".into(),
                title: "查空调".into(),
            },
            vec![],
        );
        let decision = RuleTable::initial().decide(&dispatch, &f.ctx());
        assert!(decision.is_allow(), "{decision:?}");
        assert!(decision.reason().contains("dispatch-allow"));

        let follow = plan(
            "follow",
            Operation::Follow {
                task_id: "3f2a".into(),
                text: "再看看功耗".into(),
            },
            vec![],
        );
        let decision = RuleTable::initial().decide(&follow, &f.ctx());
        assert!(decision.is_allow(), "{decision:?}");
        assert!(decision.reason().contains("follow-allow"));
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
                grant_proof: false,
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
    // ---- auto 模式（§7.1） ----

    fn shell(command: &str) -> ExecutionPlan {
        plan(
            "shell",
            Operation::ShellCommand {
                command: command.into(),
            },
            vec![],
        )
    }

    /// auto 模式**一条都不问**——这是它存在的全部理由。
    ///
    /// 名单里刻意放了初始建议会拦下来的那些（范围外文件、toolbox 变更、模型发起的模块
    /// 调用），以及最危险的那几条命令：**它们也不问**。"不审批"就是这个意思，不能只在
    /// 顺手的命令上成立。
    #[test]
    fn auto_never_asks() {
        let f = Fixture::new();
        let table = RuleTable::auto();
        let plans = vec![
            shell("cargo test --workspace"),
            shell("rm -rf /"),
            shell("sudo systemctl restart nginx"),
            shell("cat /home/u/.komo/.env"),
            plan("python", Operation::PythonCode, vec![]),
            plan(
                "python",
                Operation::PythonCall {
                    module: "toolbox.memos".into(),
                    function: "create".into(),
                },
                vec![],
            ),
            plan(
                "toolbox",
                Operation::ToolboxChange {
                    module: "memos".into(),
                },
                vec![],
            ),
            plan(
                "read",
                Operation::ReadFile,
                vec![target("/etc/shadow", TargetAccess::Read)],
            ),
            plan(
                "write",
                Operation::WriteFile,
                vec![target("/etc/hosts", TargetAccess::Write)],
            ),
            plan("memory", Operation::MemoryChange, vec![]),
            plan(
                "dispatch",
                Operation::Dispatch {
                    task: "查一下空调状态".into(),
                    title: "查空调".into(),
                },
                vec![],
            ),
            plan(
                "follow",
                Operation::Follow {
                    task_id: "3f2a".into(),
                    text: "再看看功耗".into(),
                },
                vec![],
            ),
        ];
        for plan in plans {
            let decision = table.decide(&plan, &f.ctx());
            assert!(
                matches!(decision, PolicyDecision::Allow { .. }),
                "{:?}：{decision:?}",
                plan.operation
            );
        }
    }

    /// 唯一的 `Deny` 是"模型不能改自己的权限"（§7.1 第 9 行）。它不是操作者设的边界，
    /// 所以两套建议里都在，也不随 `auto` 放开。
    #[test]
    fn auto_still_refuses_a_policy_change() {
        let f = Fixture::new();
        let plan = plan("policy", Operation::PolicyChange, vec![]);
        let decision = RuleTable::auto().decide(&plan, &f.ctx());
        let PolicyDecision::Deny { reason } = &decision else {
            panic!("{decision:?}")
        };
        assert!(reason.contains("policy-change"), "{reason}");
    }

    /// `mode = "auto"` 与 `mode = "strict"` 的差别是**整张表**：同一个计划一个放、一个问。
    #[test]
    fn auto_and_strict_differ_on_exactly_the_asking() {
        let f = Fixture::new();
        let plan = shell("cargo test --workspace");
        assert!(matches!(
            RuleTable::auto().decide(&plan, &f.ctx()),
            PolicyDecision::Allow { .. }
        ));
        assert!(matches!(
            RuleTable::initial().decide(&plan, &f.ctx()),
            PolicyDecision::Ask { .. }
        ));
    }

    /// 想自己加一张网的操作者用的形状匹配（`command_patterns`，§7.1）：归一后的子串
    /// ——连续空白压成一个空格、两边降为小写，所以 `RM   -RF /` 与 `rm -rf /` 是同一件事。
    ///
    /// **它挡住的是手滑，不是有意绕过**：下面那条断言把这件事写死了。
    #[test]
    fn a_command_pattern_rule_matches_a_normalized_substring_and_is_only_a_net() {
        let f = Fixture::new();
        let rule = PolicyRule {
            id: "no-wipe".into(),
            effect: Effect::Deny,
            reason: "别把根删了".into(),
            matcher: Matcher {
                operations: Some(vec![OperationMatch::ShellCommand]),
                command_patterns: Some(vec!["rm -rf /".into()]),
                ..Default::default()
            },
            scopes: vec![ApprovalScope::Once],
            requires_isolation: false,
            grant_proof: false,
        };
        let table = RuleTable {
            rules: vec![rule],
            default: Effect::Allow,
        };

        for command in [
            "rm -rf /",
            "RM   -RF /etc",
            "sudo rm -rf / --no-preserve-root",
        ] {
            let decision = table.decide(&shell(command), &f.ctx());
            assert!(
                matches!(decision, PolicyDecision::Deny { .. }),
                "{command}：{decision:?}"
            );
        }
        // 换个写法就绕过去了——形状清单不是边界（§7.3）。
        for evasion in ["rm -r -f /", "find / -delete", "X=rm; $X -rf /"] {
            let decision = table.decide(&shell(evasion), &f.ctx());
            assert!(
                matches!(decision, PolicyDecision::Allow { .. }),
                "{evasion} 确实在网外：{decision:?}"
            );
        }
    }

    // ---- 第 10 行（堵口子）：`komo cron add` 永远问一次，答复永远是一次性的 ----

    /// strict 下 `cron add` 永远 Ask，而且只能批一次（不给 Run / Cron 范围）；
    /// auto 基表不变——这一条只加在 `initial()` 里。
    #[test]
    fn cron_add_always_asks_in_strict_and_only_once() {
        let f = Fixture::new();
        let plan = shell(r#"komo cron add --name x --command "ls""#);

        let decision = RuleTable::initial().decide(&plan, &f.ctx());
        let PolicyDecision::Ask { reason, scopes } = &decision else {
            panic!("{decision:?}")
        };
        assert!(reason.contains("cron-add-always-asks"), "{reason}");
        assert_eq!(
            scopes,
            &vec![ApprovalScope::Once],
            "范围只能一次性——不给 Run / Cron，哪怕这份计划来自 Cron"
        );

        // auto 基表不变：这一条不在 `auto()` 里，日常的 `cron add` 照样不问。
        assert!(matches!(
            RuleTable::auto().decide(&plan, &f.ctx()),
            PolicyDecision::Allow { .. }
        ));
    }

    /// **任何 Run / Cron 范围授权都盖不住它**：即便已经有一条授权的匹配器正好覆盖了
    /// 这份 `cron add` 计划，`grant_proof` 也要它排在"有效的范围授权"那一步之前，
    /// 结论仍然是 Ask（§7.1「cron add」那一条的存在理由）。
    #[test]
    fn no_run_or_cron_scope_grant_can_cover_the_cron_add_ask() {
        let mut f = Fixture::new();
        let plan = shell(r#"komo cron add --name x --command "ls""#);

        // 先证明：按普通匹配规则，这条 Run 范围授权**确实**覆盖得到这份计划——
        // 不是因为它凑巧覆盖不到，而是 `grant_proof` 让 Policy 压根不走到这一步。
        let broad_grant = Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: GrantScope::Run {
                run: RunId::from_raw("run-1"),
                matcher: Matcher {
                    operations: Some(vec![OperationMatch::ShellCommand]),
                    command_prefixes: Some(vec!["komo cron".into()]),
                    ..Default::default()
                },
                versions: PlanVersions::default(),
            },
            granted_at: NOW,
            valid_until: None,
            consumed: false,
            reason: "此前批准过 `komo cron`".into(),
        };
        assert!(
            broad_grant.covers(&plan, NOW),
            "这条授权按普通规则确实覆盖得到，测试才有意义"
        );

        f.grants.push(broad_grant);
        let decision = RuleTable::initial().decide(&plan, &f.ctx());
        assert!(
            matches!(decision, PolicyDecision::Ask { .. }),
            "有一条覆盖得到的范围授权，仍然要问：{decision:?}"
        );
    }

    /// 触发的计划来自 Cron 本身（一条 prompt Job 的模型经 shell 调 `cron add` 给自己
    /// 签发下一个命令 Job 的授权）：同样只问一次，不因为来源是 Cron 就多给一档范围。
    #[test]
    fn a_cron_sourced_cron_add_plan_still_only_offers_once() {
        let f = Fixture::new();
        let mut plan = shell(r#"komo cron add --name x --command "ls""#);
        plan.source = PlanSource::Cron {
            job: CronJobId::from_raw("job-1"),
            job_version: 3,
        };
        let decision = RuleTable::initial().decide(&plan, &f.ctx());
        let PolicyDecision::Ask { scopes, .. } = &decision else {
            panic!("{decision:?}")
        };
        assert_eq!(
            scopes,
            &vec![ApprovalScope::Once],
            "`offered_scopes` 平时会给 Cron 来源多加一档，这一条规则不吃这一套"
        );
    }
}
