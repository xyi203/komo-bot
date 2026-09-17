//! 审批请求的创建与续跑（§7.2、§7.4）。
//!
//! 这里**没有 `Approver` trait**（§13.5）：Policy 答 `Ask` 之后 executor 写一条
//! `approval_requests`、Run 让出名额，四个界面都打到同一个决策接口，渠道之间的差别
//! 只在渲染。所以这个模块只有两件事：
//!
//! - **创建**：把一份计划、一个原因和可批的范围装成 [`ApprovalRecord`]。
//! - **续跑**：`/approve` 之后拿决定 → `consume` 换 [`ConsumedApproval`] → 交回
//!   executor 继续执行；拒绝就把"被拒绝"当结果交回模型（§7.4 最后一段）。
//!
//! 一条铁律在这里体现为"根本没有这条路径"：[`PolicyDecision::Deny`] 的计划不会走到
//! `settle`，所以**没有任何一次消费是从 Deny 出发的**。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use komo_kernel::protocol::http::{
    ApprovalDecisionRecord, ApprovalDecisionResponse, ApprovalRecord,
};
use komo_kernel::traits::{ApprovalRepo, Clock, RepoError};
use komo_kernel::types::chat::{ApprovalPresentation, ApprovalScope, PeerId};
use komo_kernel::types::ids::{ApprovalId, RunId, SessionId, ShortId, ToolCallId};
use komo_kernel::types::plan::{ConsumeIntent, ConsumedApproval, ExecutionPlan};
use time::{Duration, OffsetDateTime};

/// 一条审批默认多久失效。
///
/// 有时限的动作恢复时先查有效期，"过期不能按旧指令直接产生新的外部影响"（§8.4）。
/// 一天是"人睡一觉起来还能批"和"批过的东西不会在下周突然执行"之间的那条线。
pub const DEFAULT_VALIDITY: Duration = Duration::hours(24);

/// 组装一条审批请求要的东西。短 ID、时间与有效期由 [`ApprovalGate`] 补。
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub session: SessionId,
    pub run: Option<RunId>,
    pub call: Option<ToolCallId>,
    pub plan: ExecutionPlan,
    /// `PolicyDecision::Ask` 给的理由，原样转述。
    pub reason: String,
    /// 改动：write / edit 的 diff、toolbox 启用的版本差异（§7.2 要求界面显示）。
    pub changes: Option<String>,
    /// 已有验证结果，例如候选模块的测试输出。
    pub evidence: Option<String>,
    /// 这条请求可以批到哪些范围。总是含 [`ApprovalScope::Once`]。
    pub scopes: Vec<ApprovalScope>,
}

/// 问一条审批"现在怎么样了"的答案。
#[derive(Debug, Clone)]
pub enum ApprovalOutcome {
    /// 请求还在，没人答——Run 继续等，不要再问一次人（§7.4）。
    Pending(Box<ApprovalRecord>),
    /// 已批准并已消费，换到了可以执行这份计划的凭据。
    Approved {
        approval: ApprovalId,
        consumed: ConsumedApproval,
    },
    /// 已拒绝。拒绝作为明确结果交回模型，不是错误。
    Denied {
        approval: ApprovalId,
        reason: String,
    },
    /// 已答复，但这条决定覆盖不到眼前这份计划——范围、计划、版本或有效期变了，
    /// 要重新审核（§7.4）。
    Stale {
        approval: ApprovalId,
        reason: String,
    },
    /// 数据库里没有这一行。JSONL 的审计副本不能反向创建授权（§8.5），所以只能重问。
    Missing { approval: ApprovalId },
}

impl ApprovalOutcome {
    pub fn approval(&self) -> &ApprovalId {
        match self {
            ApprovalOutcome::Pending(record) => &record.approval,
            ApprovalOutcome::Approved { approval, .. }
            | ApprovalOutcome::Denied { approval, .. }
            | ApprovalOutcome::Stale { approval, .. }
            | ApprovalOutcome::Missing { approval } => approval,
        }
    }
}

/// 审批的运行时入口。
#[derive(Clone)]
pub struct ApprovalGate {
    repo: Arc<dyn ApprovalRepo>,
    clock: Arc<dyn Clock>,
    validity: Option<Duration>,
    /// 短 ID 的滚动起点。真正的唯一性由"避开当前待处理集合已用的那些"保证。
    cursor: Arc<AtomicU32>,
}

impl std::fmt::Debug for ApprovalGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalGate")
            .field("validity", &self.validity)
            .finish_non_exhaustive()
    }
}

impl ApprovalGate {
    pub fn new(repo: Arc<dyn ApprovalRepo>, clock: Arc<dyn Clock>) -> Self {
        Self {
            repo,
            clock,
            validity: Some(DEFAULT_VALIDITY),
            cursor: Arc::new(AtomicU32::new(0)),
        }
    }

    /// 改这条审批的有效期；`None` = 不设期限。
    pub fn with_validity(mut self, validity: Option<Duration>) -> Self {
        self.validity = validity;
        self
    }

    pub fn repo(&self) -> &Arc<dyn ApprovalRepo> {
        &self.repo
    }

    /// 写一条待处理的审批请求。
    ///
    /// 短 ID 在**待处理集合内**唯一（§11.3），所以分配时先看一眼那个集合，跳过已经
    /// 用掉的编号。仓储实现有权给出自己的编号——**以 `create` 返回的那条为准**，这里
    /// 给的是一个不与现有冲突的候选。
    pub async fn request(&self, request: ApprovalRequest) -> Result<ApprovalRecord, RepoError> {
        let now = self.clock.now();
        let short_id = self.allocate_short_id().await?;
        let record = ApprovalRecord {
            approval: ApprovalId::new_at(now),
            short_id,
            session: request.session,
            run: request.run,
            call: request.call,
            plan_hash: request.plan.plan_hash(),
            plan: request.plan,
            reason: request.reason,
            changes: request.changes,
            evidence: request.evidence,
            scopes: normalize_scopes(request.scopes),
            requested_at: now,
            valid_until: self.validity.map(|window| now + window),
            decision: None,
        };
        self.repo.create(record).await
    }

    /// 这条审批现在是什么状态；已批准的话**顺手消费**，换出执行凭据。
    ///
    /// 这就是"继续前重新校验并消费授权"那一步（§7.4）：消费的参数是**整份计划**而
    /// 不是它的哈希，因为范围授权覆盖的是一族计划，每份都有自己的哈希。
    pub async fn settle(
        &self,
        approval: &ApprovalId,
        plan: &ExecutionPlan,
        intent: ConsumeIntent,
    ) -> Result<ApprovalOutcome, RepoError> {
        let now = self.clock.now();
        let Some(record) = self.repo.get(approval).await? else {
            return Ok(ApprovalOutcome::Missing {
                approval: approval.clone(),
            });
        };
        let Some(decision) = record.decision.clone() else {
            return Ok(ApprovalOutcome::Pending(Box::new(record)));
        };
        if !decision.approved {
            return Ok(ApprovalOutcome::Denied {
                approval: approval.clone(),
                reason: format!("操作者拒绝了这次执行（{}）", record.reason),
            });
        }
        // 「已经用掉的一次性授权换不来第二次执行」不在这里判——`ApprovalRepo::consume`
        // 的契约要求 repo 自己守，`intent` 就是问它的那句话。在这里再写一遍，两处规则
        // 迟早会漂移，而漂移的方向里有一个是"放行了第二次副作用"。
        match self.repo.consume(approval, plan, intent, now).await {
            Ok(consumed) => Ok(ApprovalOutcome::Approved {
                approval: approval.clone(),
                consumed,
            }),
            Err(RepoError::GrantMismatch(reason)) => Ok(ApprovalOutcome::Stale {
                approval: approval.clone(),
                reason,
            }),
            Err(other) => Err(other),
        }
    }

    /// 等待中的 Run 被 `/approve` 唤醒后的续跑入口。
    ///
    /// 它做的事和 [`ApprovalGate::settle`] 一样——**故意的**：唤醒路径与首次执行路径
    /// 走同一段代码，否则"重启后不必再答一次"和"答过了就按原决定继续"会在两处各实现
    /// 一遍，然后在某一处忘掉有效期。继续执行由 executor 完成：它拿着这里换出来的
    /// [`ConsumedApproval`] 组装 `ApprovedPlan`。
    pub async fn resume_after_decision(
        &self,
        approval: &ApprovalId,
        plan: &ExecutionPlan,
        intent: ConsumeIntent,
    ) -> Result<ApprovalOutcome, RepoError> {
        self.settle(approval, plan, intent).await
    }

    /// 记录一个决定。**重复回答幂等**：已决定的返回原决定（§11.3）。
    pub async fn decide(
        &self,
        approval: &ApprovalId,
        approved: bool,
        scope: ApprovalScope,
        by: Option<PeerId>,
    ) -> Result<ApprovalDecisionResponse, RepoError> {
        self.decide_with_grant(approval, approved, scope, by, None)
            .await
    }

    /// 同上，外加这次批准落成的那条范围授权（§7.2）。
    ///
    /// 授权本身由调用方写下（它知道 Policy 在这条请求上给了哪些范围）；这里只把它的
    /// ID 记到决定上——`ApprovalRepo::consume` 读的就是这个字段，没有它，一条范围授权
    /// 写下了也没人用得上。
    pub async fn decide_with_grant(
        &self,
        approval: &ApprovalId,
        approved: bool,
        scope: ApprovalScope,
        by: Option<PeerId>,
        grant: Option<komo_kernel::policy::Grant>,
    ) -> Result<ApprovalDecisionResponse, RepoError> {
        self.repo
            .decide(
                approval,
                ApprovalDecisionRecord {
                    approved,
                    scope,
                    by,
                    decided_at: self.clock.now(),
                    grant: grant.map(|g| g.id),
                    consumed: false,
                },
            )
            .await
    }

    /// 聊天里 `/approve <短 ID>` 的那一步。只在待处理集合里找（§11.3）。
    pub async fn find_pending(&self, short: &ShortId) -> Result<Option<ApprovalRecord>, RepoError> {
        self.repo.find_by_short_id(short).await
    }

    async fn allocate_short_id(&self) -> Result<ShortId, RepoError> {
        let taken: Vec<ShortId> = self
            .repo
            .list_pending(None)
            .await?
            .into_iter()
            .map(|record| record.short_id)
            .collect();
        // 一圈找不到空位就说明待处理集合有 32^4 条，那不是短 ID 的问题。
        for _ in 0..=u32::from(u16::MAX) {
            let index = self.cursor.fetch_add(1, Ordering::SeqCst);
            let candidate = ShortId::from_index(index);
            if !taken.contains(&candidate) {
                return Ok(candidate);
            }
        }
        Err(RepoError::Other("待处理审批太多，短 ID 用完了".into()))
    }
}

/// 界面要显示的那份东西（§7.2 / §11.3）。渲染是各渠道的事，组装在这里。
pub fn presentation(record: &ApprovalRecord) -> ApprovalPresentation {
    ApprovalPresentation {
        approval: record.approval.clone(),
        short_id: record.short_id.clone(),
        plan_hash: record.plan_hash.clone(),
        plan: record.plan.clone(),
        reason: record.reason.clone(),
        changes: record.changes.clone(),
        evidence: record.evidence.clone(),
        scopes: record.scopes.clone(),
        valid_until: record.valid_until,
    }
}

/// 这条审批在这一刻还能用吗。
pub fn is_valid_at(record: &ApprovalRecord, now: OffsetDateTime) -> bool {
    record.valid_until.is_none_or(|until| until > now)
}

fn normalize_scopes(scopes: Vec<ApprovalScope>) -> Vec<ApprovalScope> {
    let mut out = scopes;
    if !out.contains(&ApprovalScope::Once) {
        out.insert(0, ApprovalScope::Once);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::policy::{Grant, GrantScope};
    use komo_kernel::test_support::{MemApprovalRepo, TestClock, block_on, sample_plan};
    use komo_kernel::types::ids::GrantId;

    fn gate(repo: &MemApprovalRepo, clock: &TestClock) -> ApprovalGate {
        ApprovalGate::new(Arc::new(repo.clone()), Arc::new(clock.clone()))
    }

    fn request(session: &SessionId, plan: ExecutionPlan) -> ApprovalRequest {
        ApprovalRequest {
            session: session.clone(),
            run: Some(RunId::from_raw("run-1")),
            call: Some(ToolCallId::from_raw("call-1")),
            plan,
            reason: "任意 shell / Python 代码（规则 arbitrary-code）".into(),
            changes: None,
            evidence: None,
            scopes: vec![ApprovalScope::Once],
        }
    }

    #[test]
    fn a_request_binds_the_plan_hash_and_gets_a_short_id() {
        let repo = MemApprovalRepo::new();
        let clock = TestClock::fixed();
        let gate = gate(&repo, &clock);
        let session = SessionId::from_raw("sess-1");
        let plan = sample_plan("shell", &session);

        let record = block_on(gate.request(request(&session, plan.clone()))).unwrap();
        assert_eq!(record.plan_hash, plan.plan_hash());
        assert_eq!(record.short_id.as_str().len(), 4);
        assert_eq!(record.valid_until, Some(clock.now() + DEFAULT_VALIDITY));
        assert!(record.decision.is_none());
    }

    #[test]
    fn two_pending_requests_never_share_a_short_id() {
        let repo = MemApprovalRepo::new();
        let clock = TestClock::fixed();
        let gate = gate(&repo, &clock);
        let session = SessionId::from_raw("sess-1");
        let one =
            block_on(gate.request(request(&session, sample_plan("shell", &session)))).unwrap();
        let two =
            block_on(gate.request(request(&session, sample_plan("python", &session)))).unwrap();
        assert_ne!(one.short_id, two.short_id);
        assert_eq!(
            block_on(gate.find_pending(&one.short_id))
                .unwrap()
                .map(|r| r.approval),
            Some(one.approval)
        );
    }

    #[test]
    fn an_unanswered_request_stays_pending() {
        let repo = MemApprovalRepo::new();
        let clock = TestClock::fixed();
        let gate = gate(&repo, &clock);
        let session = SessionId::from_raw("sess-1");
        let plan = sample_plan("shell", &session);
        let record = block_on(gate.request(request(&session, plan.clone()))).unwrap();

        let outcome = block_on(gate.settle(&record.approval, &plan, ConsumeIntent::First)).unwrap();
        assert!(
            matches!(outcome, ApprovalOutcome::Pending(_)),
            "{outcome:?}"
        );
    }

    #[test]
    fn approving_twice_only_yields_one_usable_grant() {
        let repo = MemApprovalRepo::new();
        let clock = TestClock::fixed();
        let gate = gate(&repo, &clock);
        let session = SessionId::from_raw("sess-1");
        let plan = sample_plan("shell", &session);
        let record = block_on(gate.request(request(&session, plan.clone()))).unwrap();

        // 操作者连点两次：第二次得到"已决定"，不产生第二条决定。
        let first =
            block_on(gate.decide(&record.approval, true, ApprovalScope::Once, None)).unwrap();
        assert!(!first.already_decided);
        let second =
            block_on(gate.decide(&record.approval, false, ApprovalScope::Once, None)).unwrap();
        assert!(second.already_decided);
        assert!(second.decision.approved, "第二次答复不能翻盘");

        let outcome = block_on(gate.settle(&record.approval, &plan, ConsumeIntent::First)).unwrap();
        assert!(
            matches!(outcome, ApprovalOutcome::Approved { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_denial_comes_back_as_a_denial_not_an_error() {
        let repo = MemApprovalRepo::new();
        let clock = TestClock::fixed();
        let gate = gate(&repo, &clock);
        let session = SessionId::from_raw("sess-1");
        let plan = sample_plan("shell", &session);
        let record = block_on(gate.request(request(&session, plan.clone()))).unwrap();
        block_on(gate.decide(&record.approval, false, ApprovalScope::Once, None)).unwrap();

        let outcome = block_on(gate.settle(&record.approval, &plan, ConsumeIntent::First)).unwrap();
        assert!(
            matches!(outcome, ApprovalOutcome::Denied { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_decision_bound_to_another_plan_is_stale_not_approval() {
        let repo = MemApprovalRepo::new();
        let clock = TestClock::fixed();
        let gate = gate(&repo, &clock);
        let session = SessionId::from_raw("sess-1");
        let plan = sample_plan("shell", &session);
        let record = block_on(gate.request(request(&session, plan.clone()))).unwrap();
        block_on(gate.decide(&record.approval, true, ApprovalScope::Once, None)).unwrap();

        let mut other = plan.clone();
        other.args = serde_json::json!({"command": "rm -rf /"});
        let outcome =
            block_on(gate.settle(&record.approval, &other, ConsumeIntent::First)).unwrap();
        assert!(
            matches!(outcome, ApprovalOutcome::Stale { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn resume_after_decision_consumes_a_once_grant_exactly_once() {
        let repo = MemApprovalRepo::new();
        let clock = TestClock::fixed();
        let gate = gate(&repo, &clock);
        let session = SessionId::from_raw("sess-1");
        let plan = sample_plan("shell", &session);
        let record = block_on(gate.request(request(&session, plan.clone()))).unwrap();

        // 一条绑定这份计划的一次性授权，挂在这条审批上。
        let grant = GrantId::from_raw("g-1");
        repo.add_grant(Grant {
            id: grant.clone(),
            approval: record.approval.clone(),
            scope: GrantScope::Once {
                plan_hash: plan.plan_hash(),
                call: None,
            },
            granted_at: clock.now(),
            valid_until: None,
            consumed: false,
            reason: "操作者批准".into(),
        });
        block_on(repo.decide(
            &record.approval,
            ApprovalDecisionRecord {
                approved: true,
                scope: ApprovalScope::Once,
                by: None,
                decided_at: clock.now(),
                grant: Some(grant),
                consumed: false,
            },
        ))
        .unwrap();

        let first =
            block_on(gate.resume_after_decision(&record.approval, &plan, ConsumeIntent::First))
                .unwrap();
        assert!(
            matches!(first, ApprovalOutcome::Approved { .. }),
            "{first:?}"
        );
        let second =
            block_on(gate.resume_after_decision(&record.approval, &plan, ConsumeIntent::First))
                .unwrap();
        assert!(
            matches!(second, ApprovalOutcome::Stale { .. }),
            "一次性授权用过就不再覆盖：{second:?}"
        );
    }

    #[test]
    fn an_expired_approval_cannot_be_consumed() {
        let repo = MemApprovalRepo::new();
        let clock = TestClock::fixed();
        let gate = gate(&repo, &clock);
        let session = SessionId::from_raw("sess-1");
        let plan = sample_plan("shell", &session);
        let record = block_on(gate.request(request(&session, plan.clone()))).unwrap();
        block_on(gate.decide(&record.approval, true, ApprovalScope::Once, None)).unwrap();

        clock.advance(DEFAULT_VALIDITY + Duration::minutes(1));
        assert!(!is_valid_at(&record, clock.now()));
        let outcome = block_on(gate.settle(&record.approval, &plan, ConsumeIntent::First)).unwrap();
        assert!(
            matches!(outcome, ApprovalOutcome::Stale { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn an_approval_the_database_never_had_is_missing_not_approved() {
        let repo = MemApprovalRepo::new();
        let clock = TestClock::fixed();
        let gate = gate(&repo, &clock);
        let session = SessionId::from_raw("sess-1");
        let outcome = block_on(gate.settle(
            &ApprovalId::from_raw("ap-ghost"),
            &sample_plan("shell", &session),
            ConsumeIntent::First,
        ))
        .unwrap();
        assert!(
            matches!(outcome, ApprovalOutcome::Missing { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_wider_scope_always_still_offers_this_call_only() {
        assert_eq!(
            normalize_scopes(vec![ApprovalScope::Run]),
            vec![ApprovalScope::Once, ApprovalScope::Run]
        );
    }

    /// 规则**在 repo 里**（`ApprovalRepo::consume` 的契约）：这里断言的是它那个
    /// `GrantMismatch` 被映射成同一个 outcome，而不是 gate 自己又判了一遍。
    #[test]
    fn a_spent_one_shot_approval_is_refused_unless_the_call_is_known_not_to_have_run() {
        let repo = MemApprovalRepo::new();
        let clock = TestClock::fixed();
        let gate = gate(&repo, &clock);
        let session = SessionId::from_raw("sess-1");
        let plan = sample_plan("shell", &session);
        let record = block_on(gate.request(request(&session, plan.clone()))).unwrap();
        block_on(gate.decide(&record.approval, true, ApprovalScope::Once, None)).unwrap();

        // 第一次用：换得出凭据。
        let first = block_on(gate.settle(&record.approval, &plan, ConsumeIntent::First)).unwrap();
        assert!(
            matches!(first, ApprovalOutcome::Approved { .. }),
            "{first:?}"
        );

        // 同一条审批被重复派发：拒绝，重新审核。
        let repeated =
            block_on(gate.settle(&record.approval, &plan, ConsumeIntent::First)).unwrap();
        assert!(
            matches!(repeated, ApprovalOutcome::Stale { .. }),
            "{repeated:?}"
        );

        // 但一次"确定没跑过"的续跑仍然可以用它——审批不因重启再问一次（§7.4）。
        let recovered = block_on(gate.resume_after_decision(
            &record.approval,
            &plan,
            ConsumeIntent::KnownNotToHaveRun,
        ))
        .unwrap();
        assert!(
            matches!(recovered, ApprovalOutcome::Approved { .. }),
            "{recovered:?}"
        );
    }
}
