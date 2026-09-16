//! 内存里的 [`ApprovalRepo`]。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::traits::*;
use crate::types::ids::*;

use crate::policy::{Grant, GrantScope};
use crate::protocol::http::{ApprovalDecisionRecord, ApprovalDecisionResponse, ApprovalRecord};
use crate::types::plan::{ConsumedApproval, ExecutionPlan};

/// 内存里的 [`ApprovalRepo`]。幂等与计划哈希核对都是真的。
#[derive(Debug, Clone, Default)]
pub struct MemApprovalRepo {
    state: Arc<Mutex<ApprovalState>>,
}

#[derive(Debug, Default)]
struct ApprovalState {
    approvals: BTreeMap<ApprovalId, ApprovalRecord>,
    grants: Vec<Grant>,
}

impl MemApprovalRepo {
    pub fn new() -> Self {
        Self::default()
    }

    /// 直接塞一条授权进去，省得每个测试都走一遍审批流程。
    pub fn add_grant(&self, grant: Grant) {
        self.state.lock().expect("审批").grants.push(grant);
    }
}

#[async_trait]
impl ApprovalRepo for MemApprovalRepo {
    async fn create(&self, request: ApprovalRecord) -> Result<ApprovalRecord, RepoError> {
        let mut state = self.state.lock().expect("审批");
        state
            .approvals
            .insert(request.approval.clone(), request.clone());
        Ok(request)
    }

    async fn get(&self, id: &ApprovalId) -> Result<Option<ApprovalRecord>, RepoError> {
        Ok(self.state.lock().expect("审批").approvals.get(id).cloned())
    }

    async fn find_by_short_id(&self, short: &ShortId) -> Result<Option<ApprovalRecord>, RepoError> {
        Ok(self
            .state
            .lock()
            .expect("审批")
            .approvals
            .values()
            .find(|a| &a.short_id == short && a.decision.is_none())
            .cloned())
    }

    async fn list_pending(
        &self,
        session: Option<&SessionId>,
    ) -> Result<Vec<ApprovalRecord>, RepoError> {
        Ok(self
            .state
            .lock()
            .expect("审批")
            .approvals
            .values()
            .filter(|a| a.decision.is_none())
            .filter(|a| session.is_none_or(|s| &a.session == s))
            .cloned()
            .collect())
    }

    async fn decide(
        &self,
        id: &ApprovalId,
        decision: ApprovalDecisionRecord,
    ) -> Result<ApprovalDecisionResponse, RepoError> {
        let mut state = self.state.lock().expect("审批");
        let record = state
            .approvals
            .get_mut(id)
            .ok_or_else(|| RepoError::NotFound {
                what: format!("approval {id}"),
            })?;
        // 重复回答幂等：已决定的返回原决定。
        if let Some(existing) = record.decision.clone() {
            return Ok(ApprovalDecisionResponse {
                approval: id.clone(),
                short_id: record.short_id.clone(),
                decision: existing,
                already_decided: true,
            });
        }
        record.decision = Some(decision.clone());
        Ok(ApprovalDecisionResponse {
            approval: id.clone(),
            short_id: record.short_id.clone(),
            decision,
            already_decided: false,
        })
    }

    async fn consume(
        &self,
        id: &ApprovalId,
        plan: &ExecutionPlan,
        now: OffsetDateTime,
    ) -> Result<ConsumedApproval, RepoError> {
        let mut state = self.state.lock().expect("审批");
        let record = state
            .approvals
            .get(id)
            .cloned()
            .ok_or_else(|| RepoError::NotFound {
                what: format!("approval {id}"),
            })?;
        let decision = record
            .decision
            .clone()
            .ok_or_else(|| RepoError::GrantMismatch("这条审批还没有决定".into()))?;
        if !decision.approved {
            return Err(RepoError::GrantMismatch("这条审批是拒绝".into()));
        }
        if record.valid_until.is_some_and(|until| until <= now) {
            return Err(RepoError::GrantMismatch("这条审批已过期".into()));
        }

        // 范围授权按 Grant::covers 判定；没有挂授权的审批就是"本次调用"，按哈希。
        let grant_id = match &decision.grant {
            Some(grant_id) => {
                let grant = state
                    .grants
                    .iter()
                    .find(|g| &g.id == grant_id)
                    .cloned()
                    .ok_or_else(|| RepoError::GrantMismatch(format!("找不到授权 {grant_id}")))?;
                if !grant.covers(plan, now) {
                    return Err(RepoError::GrantMismatch("这条授权覆盖不到这份计划".into()));
                }
                // 只有一次性授权会被用掉；范围授权本来就是给这个范围里的多次调用用的。
                if matches!(grant.scope, GrantScope::Once { .. })
                    && let Some(slot) = state.grants.iter_mut().find(|g| &g.id == grant_id)
                {
                    slot.consumed = true;
                }
                Some(grant_id.clone())
            }
            None => {
                if record.plan_hash != plan.plan_hash() {
                    return Err(RepoError::GrantMismatch(
                        "计划哈希与审批绑定的不一致".into(),
                    ));
                }
                None
            }
        };

        // 一次性的消费记在审批上；范围授权的审批不因一次使用而作废。
        let one_shot = grant_id.is_none()
            || state.grants.iter().any(|g| {
                Some(&g.id) == grant_id.as_ref() && matches!(g.scope, GrantScope::Once { .. })
            });
        if one_shot
            && let Some(slot) = state.approvals.get_mut(id)
            && let Some(decision) = slot.decision.as_mut()
        {
            decision.consumed = true;
        }

        Ok(ConsumedApproval::new(
            id.clone(),
            grant_id,
            plan.plan_hash(),
        ))
    }

    async fn grants_for_run(
        &self,
        run: &RunId,
        now: OffsetDateTime,
    ) -> Result<Vec<Grant>, RepoError> {
        use crate::policy::GrantScope;
        Ok(self
            .state
            .lock()
            .expect("审批")
            .grants
            .iter()
            .filter(|g| g.is_valid_at(now))
            .filter(|g| match &g.scope {
                GrantScope::Run { run: r, .. } => r == run,
                GrantScope::Once { .. } => true,
                GrantScope::CronJob { .. } => false,
            })
            .cloned()
            .collect())
    }

    async fn grants_for_job(
        &self,
        job: &CronJobId,
        job_version: u64,
        now: OffsetDateTime,
    ) -> Result<Vec<Grant>, RepoError> {
        use crate::policy::GrantScope;
        Ok(self
            .state
            .lock()
            .expect("审批")
            .grants
            .iter()
            .filter(|g| g.is_valid_at(now))
            .filter(|g| {
                matches!(&g.scope, GrantScope::CronJob { job: j, job_version: v, .. }
                    if j == job && *v == job_version)
            })
            .cloned()
            .collect())
    }
}
