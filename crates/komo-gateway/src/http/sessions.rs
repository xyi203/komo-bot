//! `/v1/sessions`（§13.1）。

use axum::Json;
use axum::extract::{Path, State};
use komo_kernel::fold::fold;
use komo_kernel::protocol::http::{
    BoundaryRequest, BoundaryResponse, CreateSessionRequest, PendingItem, ResumeRequest,
    ResumeResponse, SessionDetail, SessionListResponse, SessionSummary, SubmitRunRequest,
    SubmitRunResponse,
};
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::model::{Effort, ModelConfig};
use komo_kernel::types::status::RunStatus;

use super::error::{ApiFailure, ApiResult};
use super::idempotency::body_hash;
use super::{Api, created_at, events_of};

/// `GET /v1/sessions`
pub async fn list(State(api): State<Api>) -> ApiResult<Json<SessionListResponse>> {
    let records = komo_store::repos::session::list(&api.state.db).await?;
    let mut sessions = Vec::with_capacity(records.len());
    for record in records {
        sessions.push(summary_of(&api, &record.session).await?);
    }
    sessions.sort_by_key(|session| session.created_at);
    Ok(Json(SessionListResponse { sessions }))
}

/// `POST /v1/sessions`
pub async fn create(
    State(api): State<Api>,
    Json(request): Json<CreateSessionRequest>,
) -> ApiResult<Json<SessionSummary>> {
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<SessionSummary>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }

    let session = SessionId::new_at(api.state.clock.now());
    api.state.ledgers.open(&session, "agent").await?;
    if let Some(workdir) = &request.workdir {
        api.state.remember_workdir(&session, workdir);
    }
    let summary = summary_of(&api, &session).await?;
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &summary);
    Ok(Json(summary))
}

/// `GET /v1/sessions/{id}`
pub async fn detail(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionDetail>> {
    let session = SessionId::from_raw(id);
    let summary = summary_of(&api, &session).await?;
    let runs = komo_store::repos::runs::list_for_session(&api.state.db, &session).await?;
    let unfinished = runs
        .iter()
        .filter(|run| !run.status.is_terminal())
        .map(super::runs::summary_of)
        .collect();
    let pending_approvals = api.state.approval_repo.list_pending(Some(&session)).await?;
    Ok(Json(SessionDetail {
        summary,
        unfinished,
        pending_approvals,
    }))
}

/// `POST /v1/sessions/{id}/runs`：提交新输入。
pub async fn submit(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<SubmitRunRequest>,
) -> ApiResult<Json<SubmitRunResponse>> {
    let session = SessionId::from_raw(id);
    let model = model_override(&api, request.model.as_deref(), request.effort.as_ref())?;
    // 幂等由账本自己答（§8.5）：同一请求键返回原 Run，内容不同则 409。
    let response = api
        .state
        .submit(
            &session,
            request.request_key.clone(),
            request.text.clone(),
            None,
            model,
        )
        .await?;
    Ok(Json(response))
}

/// `POST /v1/sessions/{id}/resume`：检查恢复位置，恢复可继续的运行或返回待处理状态。
///
/// **CLI 不承担恢复调度**（§8.8）：Gateway 启动时已经扫过一遍，这里只是"现在到哪了"，
/// 顺手把还能继续的放回队列。
pub async fn resume(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<ResumeRequest>,
) -> ApiResult<Json<ResumeResponse>> {
    let session = SessionId::from_raw(id);
    let _ = request; // 模型覆盖属于下一次提交，不改已经固定在 Run 上的那一份快照。

    let runs = komo_store::repos::runs::list_for_session(&api.state.db, &session).await?;
    let mut resumed = Vec::new();
    let mut pending = Vec::new();
    for run in runs.iter().filter(|run| !run.status.is_terminal()) {
        match run.status {
            RunStatus::WaitingApproval => {
                for record in api.state.approval_repo.list_pending(Some(&session)).await? {
                    if record.run.as_ref() == Some(&run.run) {
                        pending.push(PendingItem::Approval(Box::new(record)));
                    }
                }
            }
            RunStatus::NeedsAttention => pending.push(PendingItem::NeedsAttention {
                run: run.run.clone(),
                reason: run
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "需要你判断".to_string()),
            }),
            RunStatus::WaitingRetry => resumed.push(super::runs::summary_of(run)),
            _ => {
                api.state.wake_run(&run.run).await?;
                resumed.push(super::runs::summary_of(run));
            }
        }
    }
    Ok(Json(ResumeResponse {
        session,
        resumed,
        pending,
    }))
}

/// `POST /v1/sessions/{id}/boundary`：`/new`，**不切 Session**。
pub async fn boundary(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(_request): Json<BoundaryRequest>,
) -> ApiResult<Json<BoundaryResponse>> {
    let session = SessionId::from_raw(id);
    let seq = api.state.routed_boundary(&session).await?;
    Ok(Json(BoundaryResponse { session, seq }))
}

/// 一个 Session 的概览。
pub async fn summary_of(api: &Api, session: &SessionId) -> Result<SessionSummary, ApiFailure> {
    let record = komo_store::repos::session::get(&api.state.db, session)
        .await?
        .ok_or_else(|| ApiFailure::not_found(format!("会话 {session}")))?;
    let runs = komo_store::repos::runs::list_for_session(&api.state.db, session).await?;
    let current = runs.iter().rev().find(|run| !run.status.is_terminal());
    let now = api.state.clock.now();
    let created = created_at(session.as_str(), now);
    Ok(SessionSummary {
        session: session.clone(),
        title: record.title.clone(),
        workdir: record
            .workdir
            .clone()
            .or_else(|| api.state.workdir_of(session)),
        current_run: current.map(|run| run.run.clone()),
        current_status: current.map(|run| run.status),
        applied_seq: record.applied_seq,
        created_at: created,
        // 最后一条事件的时刻；没有事件就是创建时刻。
        updated_at: last_event_at(api, session).await.unwrap_or(created),
    })
}

async fn last_event_at(api: &Api, session: &SessionId) -> Option<time::OffsetDateTime> {
    let events = events_of(api, session).await.ok()?;
    fold(&events).last_at
}

/// 本次 Run 的模型覆盖。
///
/// **不支持的 effort 在请求前拒绝**（§13.3）——不是发出去让服务端 400，更不是悄悄
/// 删掉这个参数再试一次。
fn model_override(
    api: &Api,
    model: Option<&str>,
    effort: Option<&Effort>,
) -> Result<Option<ModelConfig>, ApiFailure> {
    if model.is_none() && effort.is_none() {
        return Ok(None);
    }
    let mut config = api.state.snapshot().model.clone();
    if let Some(name) = model {
        config.model = name.trim().to_string();
    }
    if let Some(effort) = effort {
        config.effort = Some(effort.clone());
    }
    if let Err(problem) = api.state.caps.check(&config) {
        return Err(ApiFailure::invalid(problem.to_string()));
    }
    Ok(Some(config))
}
