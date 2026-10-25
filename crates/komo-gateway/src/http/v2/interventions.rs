//! Durable 模式的 `/v1/interventions` 与 `/v1/reconcile`（`docs/storage-durable-d5.md` §4 W4a）。
//!
//! 协议冻结：清单里只有 faulted 任务，映射成 `blocked`，可答只有 `abandon`（决定 13）。别的结论是 422，
//! 已经答过的原样返回（`already_answered`）。`/v1/reconcile` 只查 blob 引用并推进 `closing`
//! （`service::durable::lifecycle`）。

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use komo_kernel::protocol::http::{
    InterventionAnswerRequest, InterventionAnswerResponse, InterventionDetail,
    InterventionListQuery, InterventionListResponse, ReconcileResponse,
};
use komo_kernel::traits::GatewayError;

use crate::http::Api;
use crate::http::error::{ApiFailure, ApiResult};
use crate::http::idempotency::body_hash;
use crate::service::durable::lifecycle;

pub fn routes() -> Router<Api> {
    Router::new()
        .route("/v1/interventions", get(list))
        .route("/v1/interventions/{handle}", get(show))
        .route("/v1/interventions/{handle}/answer", post(answer))
        .route("/v1/reconcile", post(reconcile))
}

async fn list(
    State(api): State<Api>,
    Query(query): Query<InterventionListQuery>,
) -> ApiResult<Json<InterventionListResponse>> {
    let core = api.state.core()?;
    Ok(Json(InterventionListResponse {
        interventions: core.interventions(&query).await?,
    }))
}

async fn show(
    State(api): State<Api>,
    Path(handle): Path<String>,
) -> ApiResult<Json<InterventionDetail>> {
    let core = api.state.core()?;
    core.intervention(&handle)
        .await?
        .map(Json)
        .ok_or_else(|| ApiFailure::not_found(format!("待处理的 Intervention {handle}")))
}

async fn answer(
    State(api): State<Api>,
    Path(handle): Path<String>,
    Json(request): Json<InterventionAnswerRequest>,
) -> ApiResult<Json<InterventionAnswerResponse>> {
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<InterventionAnswerResponse>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let core = api.state.core()?;
    let response = core
        .answer_intervention(&handle, request.verdict)
        .await
        .map_err(|error| match error {
            // 结论与这条对不上：请求读得懂，但和此刻这条对不上 → 422。
            GatewayError::InvalidRequest(message) => ApiFailure::verdict_mismatch(message),
            other => other.into(),
        })?;
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &response);
    Ok(Json(response))
}

/// `POST /v1/reconcile`：只查 blob 引用、推进 `closing → deleted`、收掉墓碑已落的目录。幂等。
async fn reconcile(State(api): State<Api>) -> ApiResult<Json<ReconcileResponse>> {
    let report = lifecycle::reconcile(&api.state).await?;
    Ok(Json(report.response(api.state.clock.now())))
}
