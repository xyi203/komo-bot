//! Durable 模式的 `/v1/runs`（`docs/storage-durable-d5.md` §4 W2a）。
//!
//! `RunId` = run 首个输入 submission 的十进制 ID；详情由 submission + entry + `komo.live` 拼
//! （`service::durable::runs`），旧 UUID 形状的 RunId 是 404。cancel：排队中的单条 =
//! `Harness::withdraw`，已经在跑的 = `abort_run`（决定 8：只停当前一轮，不撤回收件箱）。

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use komo_kernel::protocol::http::{CancelRunRequest, CancelRunResponse, RunDetail};
use komo_kernel::types::ids::RunId;

use crate::http::Api;
use crate::http::error::ApiResult;

pub fn routes() -> Router<Api> {
    Router::new()
        .route("/v1/runs/{id}", get(detail))
        .route("/v1/runs/{id}/cancel", post(cancel))
}

/// `GET /v1/runs/{id}`：执行过程、工具结果与产物。
async fn detail(State(api): State<Api>, Path(id): Path<String>) -> ApiResult<Json<RunDetail>> {
    let core = api.state.core()?;
    Ok(Json(core.run_detail(&RunId::from_raw(id)).await?))
}

/// `POST /v1/runs/{id}/cancel`。已终态的原样返回状态（重复取消是幂等的）。
async fn cancel(
    State(api): State<Api>,
    Path(id): Path<String>,
    body: Option<Json<CancelRunRequest>>,
) -> ApiResult<Json<CancelRunResponse>> {
    let _ = body;
    let run = RunId::from_raw(id);
    let core = api.state.core()?;
    let state = core.cancel_run_id(&run).await?;
    Ok(Json(CancelRunResponse { run, state }))
}
