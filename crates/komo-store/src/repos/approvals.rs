//! `ApprovalRepo`：executor 等待、聊天 / TUI 答复、outbox 补写三方共用（§7.4、§11.3）。
//!
//! 两条语义在这里落地，它们都不是"存一行"那么简单：
//!
//! - **重复回答幂等**：已决定的返回原决定并把 `already_decided` 置位，不报错（§11.3）。
//! - **`ConsumeIntent` 把「第一次跑」和「恢复核对过」分开**（§7.4）：一条已经用掉的
//!   一次性授权，对 [`ConsumeIntent::First`] 是一个可分辨的失败，而不是悄悄放行第二次
//!   副作用；只有 [`ConsumeIntent::KnownNotToHaveRun`] 才豁免 `consumed` 这**一条**，
//!   计划、范围、版本、有效期照查。
//! - **消费的参数是整份计划，不是它的哈希**（§7.2）。三种范围核对的不是同一件事：
//!   `Once` 按计划哈希逐字比，且消费后标 `consumed`；`Run` / `CronJob` 按
//!   [`Grant::covers`] 判定，**不因一次使用而消耗**，但返回的 [`ConsumedApproval`] 仍
//!   记下用的是哪一条授权，于是 `GrantUse` 进账本，事后答得出"这次是凭哪条授权跑的"。

use async_trait::async_trait;
use komo_kernel::policy::{Grant, GrantScope};
use komo_kernel::protocol::http::{
    ApprovalDecisionRecord, ApprovalDecisionResponse, ApprovalRecord,
};
use komo_kernel::traits::{ApprovalRepo, RepoError, StoreError};
use komo_kernel::types::ids::{ApprovalId, CronJobId, RunId, SessionId, ShortId};
use komo_kernel::types::plan::{ConsumeIntent, ConsumedApproval, ExecutionPlan};
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, decode, encode, map_toasty, to_ts, to_ts_opt};
use crate::models::{ApprovalRequestRow, PolicyGrantRow};

/// Turso 上的 [`ApprovalRepo`]。
#[derive(Debug, Clone)]
pub struct TursoApprovalRepo {
    db: Db,
}

impl TursoApprovalRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// 写入一条范围授权。审批决定与它在**同一个控制事务**里提交（§7.4）。
    pub async fn put_grant(&self, grant: Grant) -> Result<Grant, RepoError> {
        self.db
            .with_write_retry(move |ex| {
                let grant = grant.clone();
                Box::pin(async move { put_grant_in(ex, &grant).await.map(|_| grant) })
                    as BoxFuture<'_, Result<Grant, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 读一条授权。
    pub async fn grant(
        &self,
        id: &komo_kernel::types::ids::GrantId,
    ) -> Result<Option<Grant>, RepoError> {
        let id = id.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let Some(row) = PolicyGrantRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(None);
                    };
                    Ok(Some(grant_from_row(&row)?))
                }) as BoxFuture<'_, Result<Option<Grant>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }
}

#[async_trait]
impl ApprovalRepo for TursoApprovalRepo {
    async fn create(&self, request: ApprovalRecord) -> Result<ApprovalRecord, RepoError> {
        self.db
            .with_write_retry(move |ex| {
                let request = request.clone();
                Box::pin(async move {
                    if ApprovalRequestRow::filter_by_id(request.approval.as_str())
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                        .is_some()
                    {
                        // 同一条审批重复创建是幂等的：requested 的那一刻已经定下了。
                        return Ok(request);
                    }
                    toasty::create!(ApprovalRequestRow {
                        id: request.approval.as_str(),
                        short_id: request.short_id.as_str(),
                        session_id: request.session.as_str(),
                        run_id: request.run.as_ref().map(|r| r.to_string()),
                        call_id: request.call.as_ref().map(|c| c.to_string()),
                        plan_hash: request.plan_hash.to_string(),
                        plan: encode(&request.plan)?,
                        reason: request.reason.clone(),
                        changes: request.changes.clone(),
                        evidence: request.evidence.clone(),
                        scopes: encode(&request.scopes)?,
                        requested_at: to_ts(request.requested_at),
                        valid_until: to_ts_opt(request.valid_until),
                        decided: request.decision.is_some(),
                        decision: match &request.decision {
                            Some(decision) => Some(encode(decision)?),
                            None => None,
                        },
                    })
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    Ok(request)
                }) as BoxFuture<'_, Result<ApprovalRecord, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn get(&self, id: &ApprovalId) -> Result<Option<ApprovalRecord>, RepoError> {
        let id = id.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let Some(row) = ApprovalRequestRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(None);
                    };
                    Ok(Some(record_from_row(&row)?))
                }) as BoxFuture<'_, Result<Option<ApprovalRecord>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn find_by_short_id(&self, short: &ShortId) -> Result<Option<ApprovalRecord>, RepoError> {
        let short = short.to_string();
        self.db
            .read(move |ex| {
                let short = short.clone();
                Box::pin(async move {
                    // 短 ID 在**待处理集合内**唯一，所以这个查找只在待处理集合里做（§11.3）。
                    let rows = ApprovalRequestRow::filter(
                        ApprovalRequestRow::fields()
                            .decided()
                            .eq(false)
                            .and(ApprovalRequestRow::fields().short_id().eq(short.as_str())),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    match rows.first() {
                        Some(row) => Ok(Some(record_from_row(row)?)),
                        None => Ok(None),
                    }
                }) as BoxFuture<'_, Result<Option<ApprovalRecord>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn find_latest_by_short_id(
        &self,
        short: &ShortId,
    ) -> Result<Option<ApprovalRecord>, RepoError> {
        let short = short.to_string();
        self.db
            .read(move |ex| {
                let short = short.clone();
                Box::pin(async move {
                    let rows = ApprovalRequestRow::filter(
                        ApprovalRequestRow::fields().short_id().eq(short.as_str()),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;

                    // **待处理的优先**：短 ID 的重用窗口是"下一条审批产生之前"，所以待
                    // 处理那条才是用户正看着的那张卡（§11.3）。
                    if let Some(row) = rows.iter().find(|row| !row.decided) {
                        return Ok(Some(record_from_row(row)?));
                    }
                    // 没有待处理的就给**最近一条已决定的**，让 Dispatcher 能回「已决定：
                    // 原决定」而不是「没有这条」。
                    let latest = rows.iter().max_by_key(|row| {
                        row.decision
                            .as_deref()
                            .and_then(|raw| {
                                serde_json::from_str::<ApprovalDecisionRecord>(raw).ok()
                            })
                            .map(|decision| decision.decided_at)
                    });
                    match latest {
                        Some(row) => Ok(Some(record_from_row(row)?)),
                        None => Ok(None),
                    }
                }) as BoxFuture<'_, Result<Option<ApprovalRecord>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn list_pending(
        &self,
        session: Option<&SessionId>,
    ) -> Result<Vec<ApprovalRecord>, RepoError> {
        let session = session.map(|s| s.to_string());
        self.db
            .read(move |ex| {
                let session = session.clone();
                Box::pin(async move {
                    let mut rows = ApprovalRequestRow::filter(
                        ApprovalRequestRow::fields().decided().eq(false),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    if let Some(session) = &session {
                        rows.retain(|row| &row.session_id == session);
                    }
                    rows.sort_by(|a, b| a.requested_at.cmp(&b.requested_at).then(a.id.cmp(&b.id)));
                    let mut out = Vec::with_capacity(rows.len());
                    for row in &rows {
                        out.push(record_from_row(row)?);
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<ApprovalRecord>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn decide(
        &self,
        id: &ApprovalId,
        decision: ApprovalDecisionRecord,
    ) -> Result<ApprovalDecisionResponse, RepoError> {
        let id = id.clone();
        self.db
            .with_write_retry(move |ex| {
                let (id, decision) = (id.clone(), decision.clone());
                Box::pin(async move {
                    let mut row = ApprovalRequestRow::filter_by_id(id.as_str())
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                        .ok_or_else(|| StoreError::NotFound {
                            what: format!("approval {id}"),
                        })?;

                    // 重复回答幂等：已决定的返回原决定，不报错（§11.3）。
                    if let Some(raw) = &row.decision {
                        let existing: ApprovalDecisionRecord =
                            decode(raw, "approval_requests.decision")?;
                        return Ok(ApprovalDecisionResponse {
                            approval: id,
                            short_id: ShortId::parse(&row.short_id)
                                .unwrap_or_else(|| ShortId::from_index(0)),
                            decision: existing,
                            already_decided: true,
                        });
                    }

                    let short_id = row.short_id.clone();
                    row.update()
                        .decided(true)
                        .decision(Some(encode(&decision)?))
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;

                    Ok(ApprovalDecisionResponse {
                        approval: id,
                        short_id: ShortId::parse(&short_id)
                            .unwrap_or_else(|| ShortId::from_index(0)),
                        decision,
                        already_decided: false,
                    })
                }) as BoxFuture<'_, Result<ApprovalDecisionResponse, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn consume(
        &self,
        id: &ApprovalId,
        plan: &ExecutionPlan,
        intent: ConsumeIntent,
        now: OffsetDateTime,
    ) -> Result<ConsumedApproval, RepoError> {
        let id = id.clone();
        let plan = plan.clone();
        self.db
            .with_write_retry(move |ex| {
                let (id, plan) = (id.clone(), plan.clone());
                Box::pin(async move { consume_in(ex, &id, &plan, intent, now).await })
                    as BoxFuture<'_, Result<ConsumedApproval, StoreError>>
            })
            .await
            // `StoreError::GrantMismatch` 逐个对上 `RepoError::GrantMismatch`（kernel 的
            // `From`），所以这里不再有字符串前缀那一道还原。
            .map_err(RepoError::from)
    }

    async fn grants_for_run(
        &self,
        run: &RunId,
        now: OffsetDateTime,
    ) -> Result<Vec<Grant>, RepoError> {
        let run = run.to_string();
        self.db
            .read(move |ex| {
                let run = run.clone();
                Box::pin(async move {
                    let rows =
                        PolicyGrantRow::filter(PolicyGrantRow::fields().run_id().eq(run.as_str()))
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                    let mut out = Vec::new();
                    for row in &rows {
                        let grant = grant_from_row(row)?;
                        if grant.is_valid_at(now) {
                            out.push(grant);
                        }
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<Grant>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn grants_for_job(
        &self,
        job: &CronJobId,
        job_version: u64,
        now: OffsetDateTime,
    ) -> Result<Vec<Grant>, RepoError> {
        let job = job.to_string();
        let version = i64::try_from(job_version).unwrap_or(i64::MAX);
        self.db
            .read(move |ex| {
                let job = job.clone();
                Box::pin(async move {
                    // Job 版本变了就不该再返回旧的（§10）。
                    let rows = PolicyGrantRow::filter(
                        PolicyGrantRow::fields()
                            .job_id()
                            .eq(job.as_str())
                            .and(PolicyGrantRow::fields().job_version().eq(version)),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    let mut out = Vec::new();
                    for row in &rows {
                        let grant = grant_from_row(row)?;
                        if grant.is_valid_at(now) {
                            out.push(grant);
                        }
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<Grant>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }
}

/// 「这条授权用不了」在事务里就说清楚。
///
/// `with_write_retry` 的错误通道只有 [`StoreError`]，而它现在有
/// [`StoreError::GrantMismatch`]——kernel 那边有 `impl From<StoreError> for RepoError`
/// 把它逐个搬到 [`RepoError::GrantMismatch`]，所以不需要再用字符串前缀把类别夹带出去。
fn mismatch(message: impl std::fmt::Display) -> StoreError {
    StoreError::GrantMismatch(message.to_string())
}

async fn consume_in(
    ex: &mut dyn Executor,
    id: &ApprovalId,
    plan: &ExecutionPlan,
    intent: ConsumeIntent,
    now: OffsetDateTime,
) -> Result<ConsumedApproval, StoreError> {
    let mut row = ApprovalRequestRow::filter_by_id(id.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .ok_or_else(|| StoreError::NotFound {
            what: format!("approval {id}"),
        })?;

    let raw = row
        .decision
        .clone()
        .ok_or_else(|| mismatch("这条审批还没有决定"))?;
    let mut decision: ApprovalDecisionRecord = decode(&raw, "approval_requests.decision")?;
    if !decision.approved {
        return Err(mismatch("这条审批是拒绝"));
    }
    if crate::db::from_ts_opt(row.valid_until).is_some_and(|until| until <= now) {
        return Err(mismatch("这条审批已过期"));
    }

    let grant_id = match decision.grant.clone() {
        Some(grant_id) => {
            let mut grant_row = PolicyGrantRow::filter_by_id(grant_id.as_str())
                .first()
                .exec(ex)
                .await
                .map_err(map_toasty)?
                .ok_or_else(|| mismatch(format!("找不到授权 {grant_id}")))?;
            let grant = grant_from_row(&grant_row)?;
            // 一次性授权已经用掉了：只有恢复流程核对过才准重用（§7.4）。先单独答这一
            // 条，免得"用过了"和"根本不匹配"混成同一句话——操作者要分得清。
            let used_up = matches!(grant.scope, GrantScope::Once { .. }) && grant.consumed;
            if used_up && intent != ConsumeIntent::KnownNotToHaveRun {
                return Err(mismatch(
                    "这条一次性授权已经用过了；已完成的调用不能再执行一次",
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
                return Err(mismatch("这条授权覆盖不到这份计划"));
            }
            // 只有一次性授权会被用掉；范围授权本来就是给这个范围里的多次调用用的。
            if matches!(grant.scope, GrantScope::Once { .. }) {
                grant_row
                    .update()
                    .consumed(true)
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
            }
            Some(grant_id)
        }
        None => {
            // 没有挂授权的审批就是"本次调用"，按哈希逐字比。
            if row.plan_hash != plan.plan_hash().to_string() {
                return Err(mismatch("计划哈希与审批绑定的不一致"));
            }
            // 用过一次就没了——除非恢复流程核对过原动作没发生（§7.4）。
            if decision.consumed && intent != ConsumeIntent::KnownNotToHaveRun {
                return Err(mismatch("这条审批已经消费过了；已完成的调用不能再执行一次"));
            }
            None
        }
    };

    // 一次性的消费记在审批上；范围授权的审批不因一次使用而作废。
    let one_shot = match &grant_id {
        None => true,
        Some(grant_id) => {
            let row = PolicyGrantRow::filter_by_id(grant_id.as_str())
                .first()
                .exec(ex)
                .await
                .map_err(map_toasty)?;
            row.map(|row| row.scope_kind == "once").unwrap_or(false)
        }
    };
    if one_shot && !decision.consumed {
        decision.consumed = true;
        let encoded = encode(&decision)?;
        row.update()
            .decision(Some(encoded))
            .exec(ex)
            .await
            .map_err(map_toasty)?;
    }

    Ok(ConsumedApproval::new(
        id.clone(),
        grant_id,
        plan.plan_hash(),
    ))
}

async fn put_grant_in(ex: &mut dyn Executor, grant: &Grant) -> Result<(), StoreError> {
    let (scope_kind, run_id, job_id, job_version) = match &grant.scope {
        GrantScope::Once { .. } => ("once", None, None, 0),
        GrantScope::Run { run, .. } => ("run", Some(run.to_string()), None, 0),
        GrantScope::CronJob {
            job, job_version, ..
        } => (
            "cron_job",
            None,
            Some(job.to_string()),
            i64::try_from(*job_version).unwrap_or(i64::MAX),
        ),
    };
    if let Some(mut row) = PolicyGrantRow::filter_by_id(grant.id.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
    {
        row.update()
            .consumed(grant.consumed)
            .valid_until(to_ts_opt(grant.valid_until))
            .scope(encode(&grant.scope)?)
            .exec(ex)
            .await
            .map_err(map_toasty)?;
        return Ok(());
    }
    toasty::create!(PolicyGrantRow {
        id: grant.id.as_str(),
        approval_id: grant.approval.as_str(),
        scope_kind,
        run_id,
        job_id,
        job_version,
        scope: encode(&grant.scope)?,
        granted_at: to_ts(grant.granted_at),
        valid_until: to_ts_opt(grant.valid_until),
        consumed: grant.consumed,
        reason: grant.reason.clone(),
    })
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    Ok(())
}

fn record_from_row(row: &ApprovalRequestRow) -> Result<ApprovalRecord, StoreError> {
    Ok(ApprovalRecord {
        approval: ApprovalId::from_raw(row.id.clone()),
        short_id: ShortId::parse(&row.short_id).unwrap_or_else(|| ShortId::from_index(0)),
        session: SessionId::from_raw(row.session_id.clone()),
        run: row.run_id.clone().map(RunId::from_raw),
        call: row
            .call_id
            .clone()
            .map(komo_kernel::types::ids::ToolCallId::from_raw),
        plan_hash: komo_kernel::types::plan::PlanHash::from_raw(row.plan_hash.clone()),
        plan: decode(&row.plan, "approval_requests.plan")?,
        reason: row.reason.clone(),
        changes: row.changes.clone(),
        evidence: row.evidence.clone(),
        scopes: decode(&row.scopes, "approval_requests.scopes")?,
        requested_at: crate::db::from_ts(row.requested_at),
        valid_until: crate::db::from_ts_opt(row.valid_until),
        decision: match &row.decision {
            Some(raw) => Some(decode(raw, "approval_requests.decision")?),
            None => None,
        },
    })
}

fn grant_from_row(row: &PolicyGrantRow) -> Result<Grant, StoreError> {
    Ok(Grant {
        id: komo_kernel::types::ids::GrantId::from_raw(row.id.clone()),
        approval: ApprovalId::from_raw(row.approval_id.clone()),
        scope: decode(&row.scope, "policy_grants.scope")?,
        granted_at: crate::db::from_ts(row.granted_at),
        valid_until: crate::db::from_ts_opt(row.valid_until),
        consumed: row.consumed,
        reason: row.reason.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::policy::{Matcher, OperationMatch};
    use komo_kernel::types::chat::ApprovalScope;
    use komo_kernel::types::ids::{GrantId, OperationId, ToolCallId};
    use komo_kernel::types::plan::{Operation, PlanSource, RecoveryMode};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    fn shell_plan(command: &str) -> ExecutionPlan {
        ExecutionPlan {
            operation_id: OperationId::from_raw("op-1"),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            tool: "shell".into(),
            operation: Operation::ShellCommand {
                command: command.into(),
            },
            run: Some(RunId::from_raw("run-1")),
            tool_call: Some(ToolCallId::from_raw("call-1")),
            args: serde_json::json!({}),
            cwd: None,
            targets: vec![],
            versions: Default::default(),
            resources: vec![],
            recovery: RecoveryMode::NoSafeRecovery,
        }
    }

    fn request(id: &str, plan: &ExecutionPlan, index: u32) -> ApprovalRecord {
        ApprovalRecord {
            approval: ApprovalId::from_raw(id),
            short_id: ShortId::from_index(index),
            session: SessionId::from_raw("sess-1"),
            run: Some(RunId::from_raw("run-1")),
            call: Some(ToolCallId::from_raw("call-1")),
            plan_hash: plan.plan_hash(),
            plan: plan.clone(),
            reason: "要人看一眼".into(),
            changes: None,
            evidence: None,
            scopes: vec![ApprovalScope::Once],
            requested_at: NOW,
            valid_until: None,
            decision: None,
        }
    }

    fn decision(approved: bool, grant: Option<GrantId>) -> ApprovalDecisionRecord {
        ApprovalDecisionRecord {
            approved,
            scope: ApprovalScope::Once,
            by: Some(komo_kernel::types::chat::PeerId::new("operator")),
            decided_at: NOW,
            grant,
            consumed: false,
        }
    }

    fn grant(id: &str, scope: GrantScope) -> Grant {
        Grant {
            id: GrantId::from_raw(id),
            approval: ApprovalId::from_raw("ap-1"),
            scope,
            granted_at: NOW,
            valid_until: None,
            consumed: false,
            reason: "操作者批准".into(),
        }
    }

    /// 验收 ⑪（前半）：重复回答**幂等**——已决定的返回原决定并置位
    /// `already_decided`，不报错（§11.3）。
    #[tokio::test]
    async fn answering_twice_returns_the_first_answer() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("ls");
        repo.create(request("ap-1", &plan, 1)).await.unwrap();

        let first = repo
            .decide(&ApprovalId::from_raw("ap-1"), decision(true, None))
            .await
            .unwrap();
        assert!(!first.already_decided);
        assert!(first.decision.approved);

        // 同一个人连点两次：第二次得到的是"已决定"。
        let second = repo
            .decide(&ApprovalId::from_raw("ap-1"), decision(false, None))
            .await
            .unwrap();
        assert!(second.already_decided);
        assert!(
            second.decision.approved,
            "第二次的'拒绝'没有改写第一次的'批准'"
        );
        assert_eq!(second.short_id, first.short_id);
    }

    /// 短 ID 只在**待处理集合内**查（§11.3）。
    #[tokio::test]
    async fn a_short_id_only_resolves_while_the_approval_is_pending() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("ls");
        repo.create(request("ap-1", &plan, 7)).await.unwrap();
        let short = ShortId::from_index(7);

        assert!(repo.find_by_short_id(&short).await.unwrap().is_some());
        repo.decide(&ApprovalId::from_raw("ap-1"), decision(true, None))
            .await
            .unwrap();
        assert!(
            repo.find_by_short_id(&short).await.unwrap().is_none(),
            "决定之后这个短 ID 可以被下一条审批重用"
        );

        // 重用：同一个短 ID 又出现在待处理集合里，指向新的那一条。
        repo.create(request("ap-2", &plan, 7)).await.unwrap();
        let found = repo.find_by_short_id(&short).await.unwrap().unwrap();
        assert_eq!(found.approval.as_str(), "ap-2");
    }

    /// 验收 BUG(1)：`/approve <short_id>` 第二次到达时还认得出那条。
    ///
    /// `find_by_short_id` 只看待处理集合（§11.3），所以决定之后它就答不出来了——而
    /// Dispatcher 这时候要回的是「已决定：原决定」，不是「没有这条」。
    #[tokio::test]
    async fn a_short_id_still_resolves_after_the_decision() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("ls");
        repo.create(request("ap-1", &plan, 7)).await.unwrap();
        let short = ShortId::from_index(7);

        let pending = repo.find_latest_by_short_id(&short).await.unwrap().unwrap();
        assert_eq!(pending.approval.as_str(), "ap-1");
        assert!(pending.decision.is_none());

        repo.decide(&ApprovalId::from_raw("ap-1"), decision(true, None))
            .await
            .unwrap();

        assert!(
            repo.find_by_short_id(&short).await.unwrap().is_none(),
            "待处理集合里已经没有它了"
        );
        let decided = repo.find_latest_by_short_id(&short).await.unwrap().unwrap();
        assert_eq!(decided.approval.as_str(), "ap-1");
        assert!(decided.decision.expect("带着原决定").approved);
    }

    /// **待处理的优先**：短 ID 的重用窗口是"下一条审批产生之前"，用户正看着的是新那张卡。
    #[tokio::test]
    async fn a_reused_short_id_resolves_to_the_pending_one_first() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("ls");
        let short = ShortId::from_index(7);

        repo.create(request("ap-old", &plan, 7)).await.unwrap();
        repo.decide(&ApprovalId::from_raw("ap-old"), decision(false, None))
            .await
            .unwrap();
        // 短 ID 被下一条审批重用了。
        repo.create(request("ap-new", &plan, 7)).await.unwrap();

        let found = repo.find_latest_by_short_id(&short).await.unwrap().unwrap();
        assert_eq!(found.approval.as_str(), "ap-new");
        assert!(found.decision.is_none());
    }

    /// 都决定过了就给**最近那一条**（按 `decided_at`）。
    #[tokio::test]
    async fn among_decided_records_the_latest_decision_wins() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("ls");
        let short = ShortId::from_index(7);

        repo.create(request("ap-old", &plan, 7)).await.unwrap();
        let mut early = decision(false, None);
        early.decided_at = NOW - time::Duration::hours(2);
        repo.decide(&ApprovalId::from_raw("ap-old"), early)
            .await
            .unwrap();

        repo.create(request("ap-new", &plan, 7)).await.unwrap();
        let mut late = decision(true, None);
        late.decided_at = NOW;
        repo.decide(&ApprovalId::from_raw("ap-new"), late)
            .await
            .unwrap();

        let found = repo.find_latest_by_short_id(&short).await.unwrap().unwrap();
        assert_eq!(found.approval.as_str(), "ap-new");
        assert!(found.decision.expect("带着决定").approved);
    }

    #[tokio::test]
    async fn an_unknown_short_id_resolves_to_nothing() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        assert!(
            repo.find_latest_by_short_id(&ShortId::from_index(31))
                .await
                .unwrap()
                .is_none()
        );
    }

    /// 验收 ⑪（后半，Once）：计划哈希逐字相同才算，消费之后标 `consumed`。
    #[tokio::test]
    async fn a_once_approval_covers_exactly_the_plan_it_was_given_for() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("ls");
        repo.create(request("ap-1", &plan, 1)).await.unwrap();
        repo.decide(&ApprovalId::from_raw("ap-1"), decision(true, None))
            .await
            .unwrap();

        // 换一份计划就不是它了。
        let other = shell_plan("rm -rf /");
        let error = repo
            .consume(
                &ApprovalId::from_raw("ap-1"),
                &other,
                ConsumeIntent::First,
                NOW,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RepoError::GrantMismatch(_)), "{error}");

        let consumed = repo
            .consume(
                &ApprovalId::from_raw("ap-1"),
                &plan,
                ConsumeIntent::First,
                NOW,
            )
            .await
            .unwrap();
        assert_eq!(consumed.plan_hash(), &plan.plan_hash());

        let record = repo
            .get(&ApprovalId::from_raw("ap-1"))
            .await
            .unwrap()
            .unwrap();
        assert!(record.decision.unwrap().consumed, "一次性的消费记在审批上");
    }

    /// 拒绝的审批消费不出凭据；过期的也不行。
    #[tokio::test]
    async fn a_refused_or_expired_approval_is_not_a_credential() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("ls");

        repo.create(request("ap-deny", &plan, 1)).await.unwrap();
        repo.decide(&ApprovalId::from_raw("ap-deny"), decision(false, None))
            .await
            .unwrap();
        assert!(matches!(
            repo.consume(
                &ApprovalId::from_raw("ap-deny"),
                &plan,
                ConsumeIntent::First,
                NOW
            )
            .await,
            Err(RepoError::GrantMismatch(_))
        ));

        let mut expiring = request("ap-old", &plan, 2);
        expiring.valid_until = Some(NOW - time::Duration::hours(1));
        repo.create(expiring).await.unwrap();
        repo.decide(&ApprovalId::from_raw("ap-old"), decision(true, None))
            .await
            .unwrap();
        assert!(matches!(
            repo.consume(
                &ApprovalId::from_raw("ap-old"),
                &plan,
                ConsumeIntent::First,
                NOW
            )
            .await,
            Err(RepoError::GrantMismatch(_))
        ));

        // 还没决定的同样不行。
        repo.create(request("ap-open", &plan, 3)).await.unwrap();
        assert!(matches!(
            repo.consume(
                &ApprovalId::from_raw("ap-open"),
                &plan,
                ConsumeIntent::First,
                NOW
            )
            .await,
            Err(RepoError::GrantMismatch(_))
        ));
    }

    /// 验收 ⑪（Run 范围）：按 `Grant::covers` 判定，**不因一次使用而消耗**。
    #[tokio::test]
    async fn a_run_scoped_grant_serves_many_calls_and_stops_at_its_run() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("cargo test");
        repo.create(request("ap-1", &plan, 1)).await.unwrap();
        repo.put_grant(grant(
            "g-run",
            GrantScope::Run {
                run: RunId::from_raw("run-1"),
                matcher: Matcher::operations([OperationMatch::ShellCommand]),
                versions: Default::default(),
            },
        ))
        .await
        .unwrap();
        repo.decide(
            &ApprovalId::from_raw("ap-1"),
            decision(true, Some(GrantId::from_raw("g-run"))),
        )
        .await
        .unwrap();

        // 同一族计划，多次消费都成——范围授权本来就是给多次调用用的。
        for command in ["cargo test", "cargo test --workspace", "cargo build"] {
            let consumed = repo
                .consume(
                    &ApprovalId::from_raw("ap-1"),
                    &shell_plan(command),
                    ConsumeIntent::First,
                    NOW,
                )
                .await
                .unwrap();
            assert_eq!(
                consumed.into_proof().grant_id().map(|g| g.to_string()),
                Some("g-run".to_string()),
                "账本要答得出这次是凭哪条授权跑的"
            );
        }

        // 换一个 Run 就不覆盖了。
        let mut elsewhere = shell_plan("cargo test");
        elsewhere.run = Some(RunId::from_raw("run-2"));
        assert!(matches!(
            repo.consume(
                &ApprovalId::from_raw("ap-1"),
                &elsewhere,
                ConsumeIntent::First,
                NOW
            )
            .await,
            Err(RepoError::GrantMismatch(_))
        ));

        assert_eq!(
            repo.grants_for_run(&RunId::from_raw("run-1"), NOW)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            repo.grants_for_run(&RunId::from_raw("run-2"), NOW)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// 验收 ⑪（CronJob 范围）：绑定 Job 版本——**Job 改了，旧授权失效**（§10）。
    #[tokio::test]
    async fn a_cron_grant_dies_with_the_job_version() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);

        let mut cron_plan = shell_plan("cargo test");
        cron_plan.source = PlanSource::Cron {
            job: CronJobId::from_raw("job-1"),
            job_version: 3,
        };
        repo.create(request("ap-1", &cron_plan, 1)).await.unwrap();
        repo.put_grant(grant(
            "g-job",
            GrantScope::CronJob {
                job: CronJobId::from_raw("job-1"),
                job_version: 3,
                matcher: Matcher::operations([OperationMatch::ShellCommand]),
                versions: Default::default(),
            },
        ))
        .await
        .unwrap();
        repo.decide(
            &ApprovalId::from_raw("ap-1"),
            decision(true, Some(GrantId::from_raw("g-job"))),
        )
        .await
        .unwrap();

        repo.consume(
            &ApprovalId::from_raw("ap-1"),
            &cron_plan,
            ConsumeIntent::First,
            NOW,
        )
        .await
        .expect("同一个 Job 版本覆盖得到");

        let mut next_version = cron_plan.clone();
        next_version.source = PlanSource::Cron {
            job: CronJobId::from_raw("job-1"),
            job_version: 4,
        };
        assert!(matches!(
            repo.consume(
                &ApprovalId::from_raw("ap-1"),
                &next_version,
                ConsumeIntent::First,
                NOW
            )
            .await,
            Err(RepoError::GrantMismatch(_))
        ));

        assert_eq!(
            repo.grants_for_job(&CronJobId::from_raw("job-1"), 3, NOW)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            repo.grants_for_job(&CronJobId::from_raw("job-1"), 4, NOW)
                .await
                .unwrap()
                .is_empty(),
            "Job 版本变了就不该再返回旧的"
        );
    }

    /// 验收：`ConsumeIntent` 把「第一次跑」和「恢复核对过」分开（§7.4）。
    #[tokio::test]
    async fn a_used_up_once_grant_is_refused_to_a_first_run_and_reusable_only_after_verification() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("rm -rf build");
        repo.create(request("ap-1", &plan, 1)).await.unwrap();
        repo.put_grant(grant(
            "g-once",
            GrantScope::Once {
                plan_hash: plan.plan_hash(),
                call: None,
            },
        ))
        .await
        .unwrap();
        repo.decide(
            &ApprovalId::from_raw("ap-1"),
            decision(true, Some(GrantId::from_raw("g-once"))),
        )
        .await
        .unwrap();

        repo.consume(
            &ApprovalId::from_raw("ap-1"),
            &plan,
            ConsumeIntent::First,
            NOW,
        )
        .await
        .expect("第一次用得了");

        // 第二次"第一次跑"：这个动作已经发生过了，要一个**可分辨的**失败。
        let error = repo
            .consume(
                &ApprovalId::from_raw("ap-1"),
                &plan,
                ConsumeIntent::First,
                NOW,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, RepoError::GrantMismatch(message) if message.contains("已经用过了")),
            "「用过了」要和「根本不匹配」分得开：{error}"
        );

        // 恢复核对过"没发生过"才准重用。
        repo.consume(
            &ApprovalId::from_raw("ap-1"),
            &plan,
            ConsumeIntent::KnownNotToHaveRun,
            NOW,
        )
        .await
        .expect("核对过就准重用");

        // 但它只豁免 consumed 这**一条**：计划变了照样不行。
        let error = repo
            .consume(
                &ApprovalId::from_raw("ap-1"),
                &shell_plan("rm -rf /"),
                ConsumeIntent::KnownNotToHaveRun,
                NOW,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, RepoError::GrantMismatch(message) if message.contains("覆盖不到")),
            "{error}"
        );
    }

    /// 没有挂授权的审批（"本次调用"）同样只能用一次。
    #[tokio::test]
    async fn a_bare_approval_is_also_spent_after_one_use() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("ls");
        repo.create(request("ap-1", &plan, 1)).await.unwrap();
        repo.decide(&ApprovalId::from_raw("ap-1"), decision(true, None))
            .await
            .unwrap();

        repo.consume(
            &ApprovalId::from_raw("ap-1"),
            &plan,
            ConsumeIntent::First,
            NOW,
        )
        .await
        .unwrap();
        assert!(matches!(
            repo.consume(
                &ApprovalId::from_raw("ap-1"),
                &plan,
                ConsumeIntent::First,
                NOW
            )
            .await,
            Err(RepoError::GrantMismatch(_))
        ));
        repo.consume(
            &ApprovalId::from_raw("ap-1"),
            &plan,
            ConsumeIntent::KnownNotToHaveRun,
            NOW,
        )
        .await
        .expect("核对过就准重用");
    }

    /// 范围授权**不因一次使用而消耗**，所以 `intent` 对它没有影响。
    #[tokio::test]
    async fn a_scoped_grant_is_not_spent_so_the_intent_changes_nothing() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("cargo test");
        repo.create(request("ap-1", &plan, 1)).await.unwrap();
        repo.put_grant(grant(
            "g-run",
            GrantScope::Run {
                run: RunId::from_raw("run-1"),
                matcher: Matcher::operations([OperationMatch::ShellCommand]),
                versions: Default::default(),
            },
        ))
        .await
        .unwrap();
        repo.decide(
            &ApprovalId::from_raw("ap-1"),
            decision(true, Some(GrantId::from_raw("g-run"))),
        )
        .await
        .unwrap();

        for intent in [ConsumeIntent::First, ConsumeIntent::KnownNotToHaveRun] {
            repo.consume(&ApprovalId::from_raw("ap-1"), &plan, intent, NOW)
                .await
                .unwrap_or_else(|e| panic!("{intent:?} 该过：{e}"));
        }
    }

    /// 一次性授权用掉之后**不是重试依据**（§7.4）。
    #[tokio::test]
    async fn a_consumed_once_grant_cannot_be_used_again() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("ls");
        repo.create(request("ap-1", &plan, 1)).await.unwrap();
        repo.put_grant(grant(
            "g-once",
            GrantScope::Once {
                plan_hash: plan.plan_hash(),
                call: None,
            },
        ))
        .await
        .unwrap();
        repo.decide(
            &ApprovalId::from_raw("ap-1"),
            decision(true, Some(GrantId::from_raw("g-once"))),
        )
        .await
        .unwrap();

        repo.consume(
            &ApprovalId::from_raw("ap-1"),
            &plan,
            ConsumeIntent::First,
            NOW,
        )
        .await
        .expect("第一次用得了");
        assert!(
            matches!(
                repo.consume(
                    &ApprovalId::from_raw("ap-1"),
                    &plan,
                    ConsumeIntent::First,
                    NOW
                )
                .await,
                Err(RepoError::GrantMismatch(_))
            ),
            "用过就不能再用"
        );
        // 只有恢复核对过才准重用。
        repo.consume(
            &ApprovalId::from_raw("ap-1"),
            &plan,
            ConsumeIntent::KnownNotToHaveRun,
            NOW,
        )
        .await
        .expect("核对过就准重用");
    }

    #[tokio::test]
    async fn pending_lists_only_undecided_and_can_be_scoped_to_a_session() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("ls");
        repo.create(request("ap-1", &plan, 1)).await.unwrap();
        let mut other_session = request("ap-2", &plan, 2);
        other_session.session = SessionId::from_raw("sess-2");
        repo.create(other_session).await.unwrap();

        assert_eq!(repo.list_pending(None).await.unwrap().len(), 2);
        assert_eq!(
            repo.list_pending(Some(&SessionId::from_raw("sess-1")))
                .await
                .unwrap()
                .len(),
            1
        );

        repo.decide(&ApprovalId::from_raw("ap-1"), decision(true, None))
            .await
            .unwrap();
        assert_eq!(repo.list_pending(None).await.unwrap().len(), 1);
    }

    /// 创建是幂等的——同一条审批重复投递不会变成两条。
    #[tokio::test]
    async fn creating_the_same_approval_twice_is_idempotent() {
        let (db, _dir) = temp().await;
        let repo = TursoApprovalRepo::new(db);
        let plan = shell_plan("ls");
        repo.create(request("ap-1", &plan, 1)).await.unwrap();
        repo.create(request("ap-1", &plan, 1)).await.unwrap();
        assert_eq!(repo.list_pending(None).await.unwrap().len(), 1);
    }
}
