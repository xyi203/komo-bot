//! 每轮改了哪些文件（`service::changes`）。记录只活在 Gateway 进程里：没记过的 Run 是 404。
//!
//! - `GET /v1/sessions/{id}/changes`：这个会话里还记得的、改了文件的各条 Run；
//! - `GET /v1/runs/{id}/changes`：一条 Run 的改动，还没收尾就等它（至多 20 秒）；
//! - `GET /v1/runs/{id}/changes/{index}`：其中第 `index` 个文件的 unified diff。

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use komo_kernel::protocol::http::{FileDiff, RunChanges, SessionChanges};
use komo_kernel::types::ids::{RunId, SessionId};

use crate::http::Api;
use crate::http::error::{ApiFailure, ApiResult};

pub fn routes() -> Router<Api> {
    Router::new()
        .route("/v1/sessions/{id}/changes", get(session))
        .route("/v1/runs/{id}/changes", get(run))
        .route("/v1/runs/{id}/changes/{index}", get(diff))
}

async fn session(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionChanges>> {
    let runs = api.state.changes.session(&SessionId::from_raw(id)).await;
    Ok(Json(SessionChanges { runs }))
}

async fn run(State(api): State<Api>, Path(id): Path<String>) -> ApiResult<Json<RunChanges>> {
    let run = RunId::from_raw(id);
    match api.state.changes.run(&run).await {
        Some(changes) => Ok(Json(changes)),
        None => Err(ApiFailure::not_found(format!("Run {run} 的改动记录"))),
    }
}

async fn diff(
    State(api): State<Api>,
    Path((id, index)): Path<(String, usize)>,
) -> ApiResult<Json<FileDiff>> {
    let run = RunId::from_raw(id);
    match api.state.changes.diff(&run, index).await {
        Some(diff) => Ok(Json(diff)),
        None => Err(ApiFailure::not_found(format!(
            "Run {run} 第 {index} 个文件的对比"
        ))),
    }
}
