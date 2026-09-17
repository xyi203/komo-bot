//! 范围授权（§7.2 的三种，`policy_grants` 表）。
//!
//! 「同意一次 Python」不解释为「今后任意脚本均可执行」：每条授权都绑定一个范围**和**
//! 一组版本，范围、计划、版本或有效期变化就重新审核。

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::rules::Matcher;
use crate::types::ids::{ApprovalId, CronJobId, GrantId, RunId, ToolCallId};
use crate::types::plan::{ExecutionPlan, PlanHash, PlanSource, PlanVersions};

/// 一条授权能覆盖多大范围。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GrantScope {
    /// **本次调用授权**：只批准眼前的这份执行计划，按计划哈希绑定。
    Once {
        plan_hash: PlanHash,
        /// 绑定的逻辑 ToolCall。重启不清除它（§7.4）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<ToolCallId>,
    },
    /// **本次 Run 的范围授权**：例如指定目录的写入，或明确的命令模板。
    Run {
        run: RunId,
        matcher: Matcher,
        #[serde(default)]
        versions: PlanVersions,
    },
    /// **Cron Job 授权**：绑定 Job 版本、工具或模块版本、参数范围和权限。
    CronJob {
        job: CronJobId,
        job_version: u64,
        matcher: Matcher,
        #[serde(default)]
        versions: PlanVersions,
    },
}

/// 一条已生效的授权。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub id: GrantId,
    /// 它来自哪条审批。
    pub approval: ApprovalId,
    pub scope: GrantScope,
    #[serde(with = "time::serde::rfc3339")]
    pub granted_at: OffsetDateTime,
    /// 有时限的动作先检查有效期；过期不能按旧指令直接产生新的外部影响（§8.4）。
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub valid_until: Option<OffsetDateTime>,
    /// 一次性授权是否已经被消费过。**已经消费授权本身不是重试依据**（§7.4）。
    #[serde(default)]
    pub consumed: bool,
    pub reason: String,
}

impl Grant {
    /// 这条授权在 `now` 这一刻还有效吗。
    pub fn is_valid_at(&self, now: OffsetDateTime) -> bool {
        self.valid_until.is_none_or(|until| until > now)
    }

    /// 这条授权覆不覆盖这份计划。
    ///
    /// 三个范围的共同要求：没过期。各自的要求：
    ///
    /// - `Once`：计划哈希逐字相同，且**还没被消费**。计划变了一个字节就不是它了。
    /// - `Run`：同一个 Run、匹配器命中、绑定的版本全部对得上。
    /// - `CronJob`：同一个 Job 的**同一个版本**——Job 改了，旧授权失效（§10）。
    pub fn covers(&self, plan: &ExecutionPlan, now: OffsetDateTime) -> bool {
        if !self.is_valid_at(now) {
            return false;
        }
        match &self.scope {
            GrantScope::Once { plan_hash, call } => {
                !self.consumed
                    && *plan_hash == plan.plan_hash()
                    && call
                        .as_ref()
                        .is_none_or(|c| plan.tool_call.as_ref() == Some(c))
            }
            GrantScope::Run {
                run,
                matcher,
                versions,
            } => {
                plan.run.as_ref() == Some(run)
                    && versions_cover(versions, &plan.versions)
                    && matcher.matches_scope(plan)
            }
            GrantScope::CronJob {
                job,
                job_version,
                matcher,
                versions,
            } => {
                matches!(
                    &plan.source,
                    PlanSource::Cron { job: j, job_version: v } if j == job && v == job_version
                ) && versions_cover(versions, &plan.versions)
                    && matcher.matches_scope(plan)
            }
        }
    }
}

/// 操作者答应的那个**范围**，落成一条绑定这份计划的授权（§7.2）。
///
/// 三件事让它不至于变成"今后任意脚本均可执行"：
///
/// 1. **匹配器从这份计划长出来**，不是一张空表——同一个工具、同一类操作，shell 还要
///    同一条命令的前缀。批准一次 `cargo test` 不等于批准 `rm -rf`。
/// 2. **版本逐字绑定**（[`PlanVersions`]）：代码、模块或环境版本变了就不再覆盖
///    （§5.4「模块更新使旧授权失效」）。
/// 3. **范围本身有边界**：`Run` 绑这一个 Run，`CronJob` 绑这个 Job 的**这一个版本**
///    ——改定义就失效（§10）。
///
/// 答 `None` = 这份计划落不成这个范围：`Once` 本来就不需要一条授权（审批自己按哈希
/// 绑定），而一个既没有 Run 也不是 Cron 的计划没有范围可言。
pub fn scope_for(
    plan: &ExecutionPlan,
    scope: crate::types::chat::ApprovalScope,
) -> Option<GrantScope> {
    use crate::types::chat::ApprovalScope;

    let matcher = matcher_for(plan);
    let versions = plan.versions.clone();
    match scope {
        ApprovalScope::Once => None,
        ApprovalScope::Run => plan.run.clone().map(|run| GrantScope::Run {
            run,
            matcher,
            versions,
        }),
        ApprovalScope::CronJob => match &plan.source {
            PlanSource::Cron { job, job_version } => Some(GrantScope::CronJob {
                job: job.clone(),
                job_version: *job_version,
                matcher,
                versions,
            }),
            // 交互 Run 里说 "cron" 没有意义：它绑不到任何 Job，而**默默降级成 Run 范围
            // 会把一句说错的话变成一条比它宽的授权**。
            _ => None,
        },
    }
}

/// 从一份计划长出一个匹配器。
fn matcher_for(plan: &ExecutionPlan) -> Matcher {
    use crate::policy::rules::OperationMatch;
    use crate::types::plan::Operation;

    Matcher {
        tools: Some(vec![plan.tool.clone()]),
        operations: Some(vec![OperationMatch::of(&plan.operation)]),
        // 「明确命令模板」（§7.2）：整条命令本身就是最窄的前缀。放宽到"第一个词"是
        // 另一个决定，要由写规则的人显式做，不该由一次批准隐式做。
        command_prefixes: match &plan.operation {
            Operation::ShellCommand { command } => Some(vec![command.clone()]),
            _ => None,
        },
        modules: match &plan.operation {
            Operation::PythonCall { module, .. } | Operation::ToolboxChange { module } => {
                Some(vec![module.clone()])
            }
            _ => None,
        },
        sources: None,
        paths: None,
    }
}

/// 授权绑定的版本覆不覆盖计划的版本。
///
/// 授权**没有**绑定某个维度时，那个维度不构成约束；绑定了就必须逐字相同——「版本变化
/// 使旧版本授权失效」（§5.4）。
pub fn versions_cover(granted: &PlanVersions, planned: &PlanVersions) -> bool {
    fn same<T: PartialEq>(granted: &Option<T>, planned: &Option<T>) -> bool {
        match granted {
            None => true,
            Some(expected) => planned.as_ref() == Some(expected),
        }
    }
    same(&granted.code, &planned.code)
        && same(&granted.module, &planned.module)
        && same(&granted.env, &planned.env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::rules::{Matcher, OperationMatch};
    use crate::types::digest::ContentHash;
    use crate::types::ids::{OperationId, SessionId};
    use crate::types::plan::{EnvVersion, Operation, RecoveryMode};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    fn plan(operation: Operation) -> ExecutionPlan {
        ExecutionPlan {
            operation_id: OperationId::from_raw("op-1"),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            tool: "shell".into(),
            operation,
            run: Some(RunId::from_raw("run-1")),
            tool_call: Some(ToolCallId::from_raw("call-1")),
            args: serde_json::json!({}),
            cwd: None,
            targets: vec![],
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: RecoveryMode::NoSafeRecovery,
        }
    }

    fn grant(scope: GrantScope) -> Grant {
        Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope,
            granted_at: NOW,
            valid_until: None,
            consumed: false,
            reason: "操作者批准".into(),
        }
    }

    #[test]
    fn a_once_grant_covers_exactly_the_plan_it_was_given_for() {
        let p = plan(Operation::ShellCommand {
            command: "ls".into(),
        });
        let g = grant(GrantScope::Once {
            plan_hash: p.plan_hash(),
            call: None,
        });
        assert!(g.covers(&p, NOW));

        let other = plan(Operation::ShellCommand {
            command: "rm -rf /".into(),
        });
        assert!(!g.covers(&other, NOW), "换个命令就不是同一份计划了");
    }

    #[test]
    fn a_consumed_once_grant_is_not_a_reason_to_retry() {
        let p = plan(Operation::ShellCommand {
            command: "ls".into(),
        });
        let mut g = grant(GrantScope::Once {
            plan_hash: p.plan_hash(),
            call: None,
        });
        g.consumed = true;
        assert!(!g.covers(&p, NOW));
    }

    #[test]
    fn an_expired_grant_covers_nothing() {
        let p = plan(Operation::ShellCommand {
            command: "ls".into(),
        });
        let mut g = grant(GrantScope::Once {
            plan_hash: p.plan_hash(),
            call: None,
        });
        g.valid_until = Some(datetime!(2026-09-15 07:00:00 UTC));
        assert!(!g.covers(&p, NOW));
    }

    #[test]
    fn a_run_grant_stops_at_the_run_it_names() {
        let p = plan(Operation::ShellCommand {
            command: "cargo test".into(),
        });
        let g = grant(GrantScope::Run {
            run: RunId::from_raw("run-1"),
            matcher: Matcher::operations([OperationMatch::ShellCommand]),
            versions: PlanVersions::default(),
        });
        assert!(g.covers(&p, NOW));

        let mut other_run = p.clone();
        other_run.run = Some(RunId::from_raw("run-2"));
        assert!(!g.covers(&other_run, NOW));
    }

    /// `/approve <id> cron` 落下的那条授权：**绑 Job 的这一版 + 这条命令**，
    /// 而不是"今后任意 shell"。
    #[test]
    fn a_cron_scope_binds_the_job_version_and_the_command() {
        use crate::types::chat::ApprovalScope;

        let mut p = plan(Operation::ShellCommand {
            command: "cargo test".into(),
        });
        p.source = PlanSource::Cron {
            job: CronJobId::from_raw("job-1"),
            job_version: 3,
        };

        let scope = scope_for(&p, ApprovalScope::CronJob).expect("落得成 Job 范围");
        let g = grant(scope);
        assert!(g.covers(&p, NOW));

        // 换一条命令：这条授权覆盖不到它。批准一次 `cargo test` 不等于批准 `rm -rf`。
        let mut other = p.clone();
        other.operation = Operation::ShellCommand {
            command: "rm -rf /".into(),
        };
        assert!(!g.covers(&other, NOW), "另一条命令不在这条授权里");

        // 换一个工具：同样覆盖不到。
        let mut other_tool = p.clone();
        other_tool.tool = "python".into();
        assert!(!g.covers(&other_tool, NOW));

        // 改定义（版本 +1）：失效（§10）。
        let mut bumped = p.clone();
        bumped.source = PlanSource::Cron {
            job: CronJobId::from_raw("job-1"),
            job_version: 4,
        };
        assert!(!g.covers(&bumped, NOW), "Job 改了，旧授权失效");
    }

    /// 在交互 Run 里说 `cron` 落不成任何授权——**不静默降级成 Run 范围**：一句说错的
    /// 话不该变成一条比它宽的授权。
    #[test]
    fn a_cron_scope_on_an_interactive_plan_grants_nothing() {
        use crate::types::chat::ApprovalScope;

        let p = plan(Operation::ShellCommand {
            command: "cargo test".into(),
        });
        assert!(matches!(p.source, PlanSource::Interactive { .. }));
        assert!(scope_for(&p, ApprovalScope::CronJob).is_none());
        // `Once` 从来不产生授权：审批自己按计划哈希绑定。
        assert!(scope_for(&p, ApprovalScope::Once).is_none());
        // Run 范围照常。
        assert!(matches!(
            scope_for(&p, ApprovalScope::Run),
            Some(GrantScope::Run { .. })
        ));
    }

    #[test]
    fn a_cron_grant_dies_with_the_job_version() {
        let mut p = plan(Operation::PythonCall {
            module: "toolbox.memos".into(),
            function: "create".into(),
        });
        p.source = PlanSource::Cron {
            job: CronJobId::from_raw("job-1"),
            job_version: 3,
        };
        let g = grant(GrantScope::CronJob {
            job: CronJobId::from_raw("job-1"),
            job_version: 3,
            matcher: Matcher::modules(["toolbox.memos"]),
            versions: PlanVersions::default(),
        });
        assert!(g.covers(&p, NOW));

        p.source = PlanSource::Cron {
            job: CronJobId::from_raw("job-1"),
            job_version: 4,
        };
        assert!(!g.covers(&p, NOW), "Job 改了，旧授权失效");
    }

    #[test]
    fn a_grant_bound_to_a_module_version_does_not_cover_the_next_one() {
        let mut p = plan(Operation::PythonCall {
            module: "toolbox.ha".into(),
            function: "turn_off".into(),
        });
        p.versions.module = Some("v3".into());
        let g = grant(GrantScope::Run {
            run: RunId::from_raw("run-1"),
            matcher: Matcher::modules(["toolbox.ha"]),
            versions: PlanVersions {
                module: Some("v3".into()),
                ..Default::default()
            },
        });
        assert!(g.covers(&p, NOW));

        p.versions.module = Some("v4".into());
        assert!(!g.covers(&p, NOW));
    }

    /// §7.2：授权绑定"脚本或模块版本、环境版本"；§5.4：版本变化使旧版本授权失效。三个
    /// 维度逐个：改哪一个，旧授权都覆盖不到了。
    #[test]
    fn changing_any_bound_version_invalidates_the_grant() {
        let bound = PlanVersions {
            code: Some(ContentHash::of_str("print(1)")),
            module: Some("v3".into()),
            env: Some(EnvVersion("py-1".into())),
        };
        let mut p = plan(Operation::PythonCall {
            module: "toolbox.ha".into(),
            function: "turn_off".into(),
        });
        p.versions = bound.clone();

        let g = grant(GrantScope::Run {
            run: RunId::from_raw("run-1"),
            matcher: Matcher::modules(["toolbox.ha"]),
            versions: bound.clone(),
        });
        assert!(g.covers(&p, NOW), "原样不动当然覆盖得到");

        // 代码改了。
        let mut changed = p.clone();
        changed.versions.code = Some(ContentHash::of_str("print(2)"));
        assert!(!g.covers(&changed, NOW), "代码版本变了");

        // 模块升级了。
        let mut changed = p.clone();
        changed.versions.module = Some("v4".into());
        assert!(!g.covers(&changed, NOW), "模块版本变了");

        // Python 环境换了。
        let mut changed = p.clone();
        changed.versions.env = Some(EnvVersion("py-2".into()));
        assert!(!g.covers(&changed, NOW), "环境版本变了");

        // 绑定了某个维度，而计划根本没声明它——也不算覆盖。
        let mut unversioned = p.clone();
        unversioned.versions.module = None;
        assert!(!g.covers(&unversioned, NOW));
    }

    #[test]
    fn an_unbound_version_dimension_does_not_constrain() {
        let granted = PlanVersions::default();
        let planned = PlanVersions {
            code: Some(ContentHash::of_str("x")),
            module: None,
            env: Some(EnvVersion("py-1".into())),
        };
        assert!(versions_cover(&granted, &planned));
    }
}
