//! 内存里的 [`ApprovalRepo`]。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::traits::*;
use crate::types::ids::*;

use crate::policy::{Grant, GrantScope};
use crate::protocol::http::{ApprovalDecisionRecord, ApprovalDecisionResponse, ApprovalRecord};
use crate::types::plan::{ConsumeIntent, ConsumedApproval, ExecutionPlan};

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
        intent: ConsumeIntent,
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
                // 一次性授权已经用掉了：只有恢复流程核对过才准重用（§7.4）。先把这一条
                // 单独答，免得"用过了"和"根本不匹配"混成同一句话——操作者要分得清。
                let used_up = matches!(grant.scope, GrantScope::Once { .. }) && grant.consumed;
                if used_up && intent != ConsumeIntent::KnownNotToHaveRun {
                    return Err(RepoError::GrantMismatch(
                        "这条一次性授权已经用过了；已完成的调用不能再执行一次".into(),
                    ));
                }
                // 核对过"没发生过"就只差 consumed 这一条；计划、范围、版本、有效期照查。
                let covered = if used_up {
                    let mut fresh = grant.clone();
                    fresh.consumed = false;
                    fresh.covers(plan, now)
                } else {
                    grant.covers(plan, now)
                };
                if !covered {
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
                // 没有挂授权的审批就是"本次调用"，用过一次就没了。
                if decision.consumed && intent != ConsumeIntent::KnownNotToHaveRun {
                    return Err(RepoError::GrantMismatch(
                        "这条审批已经消费过了；已完成的调用不能再执行一次".into(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::http::ApprovalRecord;
    use crate::test_support::{block_on, sample_plan};
    use crate::types::ids::ShortId;

    #[test]
    fn deciding_twice_returns_the_first_decision() {
        block_on(async {
            let repo = MemApprovalRepo::new();
            let approval = ApprovalId::from_raw("ap-1");
            let plan = sample_plan("shell", &SessionId::from_raw("sess-1"));
            let record = ApprovalRecord {
                approval: approval.clone(),
                short_id: ShortId::from_index(1),
                session: SessionId::from_raw("sess-1"),
                run: None,
                call: None,
                plan_hash: plan.plan_hash(),
                plan: plan.clone(),
                reason: "任意 shell".into(),
                changes: None,
                evidence: None,
                scopes: vec![],
                requested_at: time::macros::datetime!(2026-09-15 08:00:00 UTC),
                valid_until: None,
                decision: None,
            };
            repo.create(record).await.unwrap();

            let decision = ApprovalDecisionRecord {
                approved: true,
                scope: crate::types::chat::ApprovalScope::Once,
                by: None,
                decided_at: time::macros::datetime!(2026-09-15 08:01:00 UTC),
                grant: None,
                consumed: false,
            };
            let first = repo.decide(&approval, decision.clone()).await.unwrap();
            assert!(!first.already_decided);

            let mut rejection = decision.clone();
            rejection.approved = false;
            let second = repo.decide(&approval, rejection).await.unwrap();
            assert!(second.already_decided);
            assert!(second.decision.approved, "返回的是原决定");

            // 消费时核对计划哈希。
            let other = sample_plan("write", &SessionId::from_raw("sess-1"));
            assert!(
                repo.consume(
                    &approval,
                    &other,
                    ConsumeIntent::First,
                    time::macros::datetime!(2026-09-15 08:02:00 UTC)
                )
                .await
                .is_err()
            );
            let consumed = repo
                .consume(
                    &approval,
                    &plan,
                    ConsumeIntent::First,
                    time::macros::datetime!(2026-09-15 08:02:00 UTC),
                )
                .await
                .unwrap();
            assert_eq!(consumed.plan_hash(), &plan.plan_hash());
        });
    }

    #[test]
    fn a_scoped_grant_covers_several_plans_and_survives_being_used() {
        block_on(async {
            use crate::policy::{Grant, GrantScope, Matcher};
            use crate::types::ids::GrantId;
            use crate::types::plan::{Operation, PlanVersions};

            let now = time::macros::datetime!(2026-09-15 08:00:00 UTC);
            let session = SessionId::from_raw("sess-1");
            let run = RunId::from_raw("run-1");
            let repo = MemApprovalRepo::new();

            let call = |command: &str| {
                let mut plan = sample_plan("shell", &session);
                plan.run = Some(run.clone());
                plan.operation = Operation::ShellCommand {
                    command: command.into(),
                };
                plan
            };

            let grant = Grant {
                id: GrantId::from_raw("g-1"),
                approval: ApprovalId::from_raw("ap-1"),
                scope: GrantScope::Run {
                    run: run.clone(),
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
            };
            repo.add_grant(grant);

            let first = call("cargo test --workspace");
            let approval = ApprovalRecord {
                approval: ApprovalId::from_raw("ap-1"),
                short_id: ShortId::from_index(2),
                session: session.clone(),
                run: Some(run.clone()),
                call: None,
                plan_hash: first.plan_hash(),
                plan: first.clone(),
                reason: "任意 shell".into(),
                changes: None,
                evidence: None,
                scopes: vec![crate::types::chat::ApprovalScope::Run],
                requested_at: now,
                valid_until: None,
                decision: None,
            };
            repo.create(approval).await.unwrap();
            repo.decide(
                &ApprovalId::from_raw("ap-1"),
                ApprovalDecisionRecord {
                    approved: true,
                    scope: crate::types::chat::ApprovalScope::Run,
                    by: None,
                    decided_at: now,
                    grant: Some(GrantId::from_raw("g-1")),
                    consumed: false,
                },
            )
            .await
            .unwrap();

            // 第一份计划过得去，而且记下了是凭哪条授权。
            let used = repo
                .consume(
                    &ApprovalId::from_raw("ap-1"),
                    &first,
                    ConsumeIntent::First,
                    now,
                )
                .await
                .unwrap();
            assert_eq!(used.plan_hash(), &first.plan_hash());

            // **另一份**计划——哈希不同——同一条范围授权照样覆盖得到。
            let second = call("cargo test -p komo-kernel");
            assert_ne!(second.plan_hash(), first.plan_hash());
            assert!(
                repo.consume(
                    &ApprovalId::from_raw("ap-1"),
                    &second,
                    ConsumeIntent::First,
                    now
                )
                .await
                .is_ok(),
                "范围授权不因一次使用而作废"
            );

            // 范围之外的命令还是不行。
            let outside = call("rm -rf /");
            assert!(
                repo.consume(
                    &ApprovalId::from_raw("ap-1"),
                    &outside,
                    ConsumeIntent::First,
                    now
                )
                .await
                .is_err()
            );
        });
    }

    #[test]
    fn a_once_grant_is_used_up_by_the_call_it_was_given_for() {
        block_on(async {
            let now = time::macros::datetime!(2026-09-15 08:00:00 UTC);
            let session = SessionId::from_raw("sess-1");
            let repo = MemApprovalRepo::new();
            let plan = sample_plan("shell", &session);

            repo.create(ApprovalRecord {
                approval: ApprovalId::from_raw("ap-2"),
                short_id: ShortId::from_index(3),
                session: session.clone(),
                run: None,
                call: None,
                plan_hash: plan.plan_hash(),
                plan: plan.clone(),
                reason: "任意 shell".into(),
                changes: None,
                evidence: None,
                scopes: vec![],
                requested_at: now,
                valid_until: None,
                decision: None,
            })
            .await
            .unwrap();
            repo.decide(
                &ApprovalId::from_raw("ap-2"),
                ApprovalDecisionRecord {
                    approved: true,
                    scope: crate::types::chat::ApprovalScope::Once,
                    by: None,
                    decided_at: now,
                    grant: None,
                    consumed: false,
                },
            )
            .await
            .unwrap();

            let used = repo
                .consume(
                    &ApprovalId::from_raw("ap-2"),
                    &plan,
                    ConsumeIntent::First,
                    now,
                )
                .await
                .unwrap();
            assert!(used.into_proof().approval_id().is_some());
            assert!(
                repo.get(&ApprovalId::from_raw("ap-2"))
                    .await
                    .unwrap()
                    .unwrap()
                    .decision
                    .unwrap()
                    .consumed,
                "一次性授权用过了就记下来"
            );

            // 再来一次是**可分辨的失败**，不是第二次副作用（§7.4「已完成调用不能再次执行」）。
            let again = repo
                .consume(
                    &ApprovalId::from_raw("ap-2"),
                    &plan,
                    ConsumeIntent::First,
                    now,
                )
                .await;
            assert!(
                matches!(&again, Err(RepoError::GrantMismatch(m)) if m.contains("已经消费")),
                "{again:?}"
            );

            // 只有恢复流程核对过"原动作没发生"，才准重用同一条授权继续
            // （§7.4「已经消费授权本身不是重试依据」的另一半）。
            assert!(
                repo.consume(
                    &ApprovalId::from_raw("ap-2"),
                    &plan,
                    ConsumeIntent::KnownNotToHaveRun,
                    now,
                )
                .await
                .is_ok()
            );
        });
    }

    #[test]
    fn a_used_up_once_grant_is_only_reusable_for_a_call_known_not_to_have_run() {
        block_on(async {
            use crate::policy::{Grant, GrantScope};
            use crate::types::ids::GrantId;

            let now = time::macros::datetime!(2026-09-15 08:00:00 UTC);
            let session = SessionId::from_raw("sess-1");
            let repo = MemApprovalRepo::new();
            let plan = sample_plan("shell", &session);

            repo.add_grant(Grant {
                id: GrantId::from_raw("g-1"),
                approval: ApprovalId::from_raw("ap-3"),
                scope: GrantScope::Once {
                    plan_hash: plan.plan_hash(),
                    call: None,
                },
                granted_at: now,
                valid_until: None,
                consumed: false,
                reason: "操作者批了这一次".into(),
            });
            repo.create(ApprovalRecord {
                approval: ApprovalId::from_raw("ap-3"),
                short_id: ShortId::from_index(4),
                session: session.clone(),
                run: None,
                call: None,
                plan_hash: plan.plan_hash(),
                plan: plan.clone(),
                reason: "任意 shell".into(),
                changes: None,
                evidence: None,
                scopes: vec![],
                requested_at: now,
                valid_until: None,
                decision: None,
            })
            .await
            .unwrap();
            repo.decide(
                &ApprovalId::from_raw("ap-3"),
                ApprovalDecisionRecord {
                    approved: true,
                    scope: crate::types::chat::ApprovalScope::Once,
                    by: None,
                    decided_at: now,
                    grant: Some(GrantId::from_raw("g-1")),
                    consumed: false,
                },
            )
            .await
            .unwrap();

            // 第一次：过。
            repo.consume(
                &ApprovalId::from_raw("ap-3"),
                &plan,
                ConsumeIntent::First,
                now,
            )
            .await
            .unwrap();

            // 第二次当作首次：拒。重启之后不能凭"批准记录还在"就再跑一遍。
            let again = repo
                .consume(
                    &ApprovalId::from_raw("ap-3"),
                    &plan,
                    ConsumeIntent::First,
                    now,
                )
                .await;
            assert!(
                matches!(&again, Err(RepoError::GrantMismatch(m)) if m.contains("已经用过")),
                "{again:?}"
            );

            // 核对过了：准。
            assert!(
                repo.consume(
                    &ApprovalId::from_raw("ap-3"),
                    &plan,
                    ConsumeIntent::KnownNotToHaveRun,
                    now,
                )
                .await
                .is_ok(),
                "确定原动作未发生，可在原授权范围内继续"
            );

            // 但核对过也不能把它变成万能钥匙：别的计划照样不行。
            let other = sample_plan("write", &session);
            assert!(
                repo.consume(
                    &ApprovalId::from_raw("ap-3"),
                    &other,
                    ConsumeIntent::KnownNotToHaveRun,
                    now,
                )
                .await
                .is_err()
            );
        });
    }
}
