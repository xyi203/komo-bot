//! 运行时侧的 Policy：装配与上下文组装（§7.1）。
//!
//! 判决本身在 kernel——[`RuleTable`] 是**数据驱动的同步纯函数**，生产实现与测试实现
//! 是同一个。这里只做两件运行时才做得了的事：
//!
//! 1. 把配置解析出来的规则表装成 `Arc<dyn Policy>`（规则表的**解析**是 `config` 的，
//!    这个模块只消费 [`RuleTable`]）。
//! 2. 组装 [`PolicyContext`]：授权从 [`ApprovalRepo`] 取、principal 与 roots 由调用
//!    方给、`now` 从 [`Clock`] 取、isolation 是这台机器的事实。
//!
//! 授权**按来源取**：交互 Run 取 `grants_for_run`，Cron 取 `grants_for_job` 且带上
//! Job 版本——一条 Cron 授权不该漏进交互 Run，反过来也一样（§10）。

use std::sync::Arc;

use komo_kernel::policy::{Grant, IsolationCapability, PolicyContext, PolicyDecision, RuleTable};
use komo_kernel::traits::{ApprovalRepo, Clock, Policy, RepoError};
use komo_kernel::types::chat::Principal;
use komo_kernel::types::plan::{ExecutionPlan, PlanSource};
use komo_kernel::types::tool::WorkspaceRoot;
use time::OffsetDateTime;

/// 决策入口：一张规则表 + 这台机器能不能约束任意代码。
#[derive(Clone)]
pub struct PolicyEngine {
    policy: Arc<dyn Policy>,
    isolation: IsolationCapability,
}

impl std::fmt::Debug for PolicyEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyEngine")
            .field("isolation", &self.isolation)
            .finish_non_exhaustive()
    }
}

impl PolicyEngine {
    pub fn new(policy: Arc<dyn Policy>, isolation: IsolationCapability) -> Self {
        Self { policy, isolation }
    }

    /// 配置侧给一张表就够了。
    ///
    /// `isolation` 默认是 `confines_arbitrary_code: false`——§7.3 说得很清楚，cwd 和
    /// 参数检查不是完整进程沙箱，所以默认值就是首版的事实。有了真沙箱再由构造方改。
    pub fn from_rules(table: RuleTable) -> Self {
        Self::new(Arc::new(table), IsolationCapability::default())
    }

    /// 最保守的起点：什么都问。
    pub fn conservative() -> Self {
        Self::from_rules(RuleTable::empty())
    }

    /// §7.1 那张初始建议表。
    pub fn initial() -> Self {
        Self::from_rules(RuleTable::initial())
    }

    pub fn isolation(&self) -> IsolationCapability {
        self.isolation
    }

    pub fn decide(&self, plan: &ExecutionPlan, env: &DecisionEnv<'_>) -> PolicyDecision {
        self.policy.decide(plan, &env.context(self.isolation))
    }

    /// 命中的那条授权。
    ///
    /// Policy 的 `Allow` 只说"已有授权 {id}"，而 executor 要**消费**它才换得到带
    /// approval / grant 的 [`Proof`](komo_kernel::types::plan::Proof)，所以这里再找
    /// 一次。顺序上它永远在 `decide` **之后**被问：Deny 在梯子上高于授权，先判决就
    /// 不存在"拿着授权绕过 Deny"的路径。
    pub fn covering_grant<'a>(
        &self,
        plan: &ExecutionPlan,
        env: &DecisionEnv<'a>,
    ) -> Option<&'a Grant> {
        env.grants.iter().find(|grant| grant.covers(plan, env.now))
    }
}

/// 一次判决要的上下文。和 [`PolicyContext`] 的区别只在生命周期方便：它是运行时这边
/// 组装出来的那几样东西的容器。
pub struct DecisionEnv<'a> {
    pub grants: &'a [Grant],
    pub principal: Option<&'a Principal>,
    pub roots: &'a [WorkspaceRoot],
    pub now: OffsetDateTime,
}

impl<'a> DecisionEnv<'a> {
    pub fn context(&self, isolation: IsolationCapability) -> PolicyContext<'a> {
        PolicyContext {
            grants: self.grants,
            principal: self.principal,
            roots: self.roots,
            now: self.now,
            isolation,
        }
    }
}

/// 这份计划的来源对应的**有效范围授权**。
///
/// 来源决定问哪个集合：Cron 的授权绑定 Job 的那个版本，交互 / Memory / 核对走 Run。
/// 一个没有 Run 也没有 Job 的计划（例如 Memory 的内部变更）没有范围授权可言，返回
/// 空集合而不是"全部"。
pub async fn grants_for(
    repo: &dyn ApprovalRepo,
    plan: &ExecutionPlan,
    now: OffsetDateTime,
) -> Result<Vec<Grant>, RepoError> {
    match &plan.source {
        PlanSource::Cron { job, job_version } => repo.grants_for_job(job, *job_version, now).await,
        PlanSource::Interactive { .. }
        | PlanSource::Memory { .. }
        | PlanSource::Verification { .. } => match &plan.run {
            Some(run) => repo.grants_for_run(run, now).await,
            None => Ok(Vec::new()),
        },
    }
}

/// `now` 走时钟，不走 `OffsetDateTime::now_utc()`——判"授权过期没有"要能在测试里拨。
pub fn now(clock: &dyn Clock) -> OffsetDateTime {
    clock.now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::policy::{Effect, Matcher, OperationMatch, PathMatch, PolicyRule};
    use komo_kernel::test_support::{MemApprovalRepo, TestClock, block_on};
    use komo_kernel::types::ids::{
        ApprovalId, CronJobId, GrantId, OperationId, RunId, SessionId, ToolCallId,
    };
    use komo_kernel::types::plan::{
        Operation, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
    };
    use std::path::PathBuf;

    fn plan(operation: Operation, targets: Vec<PlanTarget>) -> ExecutionPlan {
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
            targets,
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: RecoveryMode::NoSafeRecovery,
        }
    }

    fn roots() -> Vec<WorkspaceRoot> {
        vec![WorkspaceRoot {
            path: PathBuf::from("/ws"),
            writable: true,
            label: "workspace".into(),
        }]
    }

    #[test]
    fn the_initial_table_asks_about_an_arbitrary_command() {
        let engine = PolicyEngine::initial();
        let roots = roots();
        let env = DecisionEnv {
            grants: &[],
            principal: None,
            roots: &roots,
            now: TestClock::fixed().now(),
        };
        let decision = engine.decide(
            &plan(
                Operation::ShellCommand {
                    command: "rm -rf /".into(),
                },
                vec![],
            ),
            &env,
        );
        assert!(
            matches!(decision, PolicyDecision::Ask { .. }),
            "{decision:?}"
        );
    }

    #[test]
    fn a_covering_grant_is_named_so_the_executor_can_consume_it() {
        let now = TestClock::fixed().now();
        let p = plan(
            Operation::ShellCommand {
                command: "cargo test".into(),
            },
            vec![],
        );
        let grants = vec![Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: komo_kernel::policy::GrantScope::Run {
                run: RunId::from_raw("run-1"),
                matcher: Matcher {
                    command_prefixes: Some(vec!["cargo test".into()]),
                    ..Default::default()
                },
                versions: PlanVersions::default(),
            },
            granted_at: now,
            valid_until: None,
            consumed: false,
            reason: "本次 Run 内可以跑测试".into(),
        }];
        let roots = roots();
        let env = DecisionEnv {
            grants: &grants,
            principal: None,
            roots: &roots,
            now,
        };
        let engine = PolicyEngine::initial();
        assert!(engine.decide(&p, &env).is_allow());
        assert_eq!(
            engine.covering_grant(&p, &env).map(|g| g.id.as_str()),
            Some("g-1")
        );
    }

    #[test]
    fn a_deny_rule_is_still_deny_with_a_grant_in_hand() {
        let now = TestClock::fixed().now();
        let p = plan(
            Operation::ReadFile,
            vec![PlanTarget {
                path: PathBuf::from("/home/u/.ssh/id_ed25519"),
                access: TargetAccess::Read,
                expected_version: None,
            }],
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
        let grants = vec![Grant {
            id: GrantId::from_raw("g-1"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: komo_kernel::policy::GrantScope::Once {
                plan_hash: p.plan_hash(),
                call: None,
            },
            granted_at: now,
            valid_until: None,
            consumed: false,
            reason: "有人点了批准".into(),
        }];
        let roots = roots();
        let env = DecisionEnv {
            grants: &grants,
            principal: None,
            roots: &roots,
            now,
        };
        assert!(PolicyEngine::from_rules(table).decide(&p, &env).is_deny());
    }

    #[test]
    fn a_hardline_deny_refuses_arbitrary_code_without_a_sandbox() {
        let mut table = RuleTable::initial();
        table.rules.insert(
            0,
            PolicyRule {
                id: "no-network".into(),
                effect: Effect::Deny,
                reason: "禁止网络访问".into(),
                matcher: Matcher::operations([OperationMatch::ShellCommand]),
                scopes: vec![],
                requires_isolation: true,
            },
        );
        let roots = roots();
        let env = DecisionEnv {
            grants: &[],
            principal: None,
            roots: &roots,
            now: TestClock::fixed().now(),
        };
        let decision = PolicyEngine::from_rules(table).decide(
            &plan(
                Operation::ShellCommand {
                    command: "curl example.com".into(),
                },
                vec![],
            ),
            &env,
        );
        assert!(decision.is_deny(), "{decision:?}");
    }

    #[test]
    fn cron_grants_come_from_the_job_and_never_from_the_run() {
        let repo = MemApprovalRepo::new();
        let now = TestClock::fixed().now();
        repo.add_grant(Grant {
            id: GrantId::from_raw("g-cron"),
            approval: ApprovalId::from_raw("ap-1"),
            scope: komo_kernel::policy::GrantScope::CronJob {
                job: CronJobId::from_raw("job-1"),
                job_version: 3,
                matcher: Matcher::modules(["toolbox.memos"]),
                versions: PlanVersions::default(),
            },
            granted_at: now,
            valid_until: None,
            consumed: false,
            reason: "早报任务".into(),
        });

        let mut cron = plan(Operation::PythonCode, vec![]);
        cron.source = PlanSource::Cron {
            job: CronJobId::from_raw("job-1"),
            job_version: 3,
        };
        let found = block_on(grants_for(&repo, &cron, now)).unwrap();
        assert_eq!(found.len(), 1);

        // Job 版本变了就取不到了。
        cron.source = PlanSource::Cron {
            job: CronJobId::from_raw("job-1"),
            job_version: 4,
        };
        assert!(block_on(grants_for(&repo, &cron, now)).unwrap().is_empty());

        // 交互 Run 问的是另一个集合，Cron 授权不在里面。
        let interactive = plan(Operation::PythonCode, vec![]);
        assert!(
            block_on(grants_for(&repo, &interactive, now))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_plan_with_neither_run_nor_job_has_no_scope_grants() {
        let repo = MemApprovalRepo::new();
        let mut p = plan(Operation::MemoryChange, vec![]);
        p.run = None;
        p.source = PlanSource::Memory { session: None };
        assert!(
            block_on(grants_for(&repo, &p, TestClock::fixed().now()))
                .unwrap()
                .is_empty()
        );
    }
}
