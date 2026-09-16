//! `/v1/approvals`（§13.1、§11.3）。
//!
//! 四个界面——飞书卡片按钮、Telegram / WeChat 命令、TUI 弹窗、CLI 子命令——都打到
//! `POST /v1/approvals/{id}/decision`（§13.5）。渠道之间的差别只在渲染，不在决策。

use axum::Json;
use axum::extract::{Path, Query, State};
use komo_kernel::protocol::http::{
    ApprovalDecisionRequest, ApprovalDecisionResponse, ApprovalListQuery, ApprovalListResponse,
    ApprovalRecord,
};
use komo_kernel::types::ids::ApprovalId;

use super::Api;
use super::error::{ApiFailure, ApiResult};
use super::idempotency::body_hash;

/// `GET /v1/approvals`
pub async fn list(
    State(api): State<Api>,
    Query(query): Query<ApprovalListQuery>,
) -> ApiResult<Json<ApprovalListResponse>> {
    let mut approvals = api
        .state
        .approval_repo
        .list_pending(query.session.as_ref())
        .await?;
    if let Some(run) = &query.run {
        approvals.retain(|record| record.run.as_ref() == Some(run));
    }
    // `include_decided` 是审计面（`komo run inspect` 要答"这一步是谁放行的"）。
    if query.include_decided
        && let Some(run) = &query.run
    {
        approvals.extend(decided_of(&api, run).await?);
    }
    Ok(Json(ApprovalListResponse { approvals }))
}

/// `GET /v1/approvals/{id}`
pub async fn show(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<ApprovalRecord>> {
    let approval = ApprovalId::from_raw(id);
    api.state
        .approval_repo
        .get(&approval)
        .await?
        .map(Json)
        .ok_or_else(|| ApiFailure::not_found(format!("审批 {approval}")))
}

/// `POST /v1/approvals/{id}/decision`
///
/// **已决定的返回原决定，不报错**（§11.3）——同一人连点两次，第二次得到的是"已决定"。
pub async fn decide(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<ApprovalDecisionRequest>,
) -> ApiResult<Json<ApprovalDecisionResponse>> {
    let approval = ApprovalId::from_raw(id);
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<ApprovalDecisionResponse>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let response = api
        .state
        .decide_approval(&approval, request.approved, request.scope, None)
        .await?;
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &response);
    Ok(Json(response))
}

/// 这个 Run 上已经决定过的审批。
async fn decided_of(
    api: &Api,
    run: &komo_kernel::types::ids::RunId,
) -> Result<Vec<ApprovalRecord>, ApiFailure> {
    // 已决定的不在待处理集合里，只能按 Run 的事件把它们的 ID 找回来再逐条读。
    let record = komo_store::repos::runs::get(&api.state.db, run).await?;
    let Some(record) = record else {
        return Ok(Vec::new());
    };
    let events = super::events_of(api, &record.session).await?;
    let mut out = Vec::new();
    for event in events
        .iter()
        .filter(|event| event.run.as_ref() == Some(run))
    {
        let komo_kernel::events::EventPayload::ApprovalRequested(body) = &event.payload else {
            continue;
        };
        if let Some(found) = api.state.approval_repo.get(&body.approval).await?
            && found.decision.is_some()
        {
            out.push(found);
        }
    }
    Ok(out)
}
