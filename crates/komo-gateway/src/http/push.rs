//! 浏览器推送的三个接口（`docs/redesign.md` §1.8；形状是 `packages/protocol/src/push.ts` 的手写类型）。
//! 都在登录之后（`shared_routes`）。
//!
//! | 路由 | 请求 | 响应 |
//! |---|---|---|
//! | `GET /v1/push/key` | — | `{ "public_key" }`（VAPID 公钥，未压缩 P-256 点的 base64url） |
//! | `POST /v1/push/subscriptions` | `{ endpoint, keys: { p256dh, auth }, user_agent? }` | `{ "id" }`（同一 endpoint = 更新） |
//! | `DELETE /v1/push/subscriptions` | `{ endpoint }` | 204 |

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use komo_kernel::protocol::http::ErrorCode;
use serde::{Deserialize, Serialize};

use super::Api;
use super::error::{ApiFailure, ApiResult};
use crate::push::PushError;

#[derive(Debug, Serialize)]
pub struct PushKeyResponse {
    pub public_key: String,
}

#[derive(Debug, Deserialize)]
pub struct PushKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Deserialize)]
pub struct PushSubscribeRequest {
    pub endpoint: String,
    pub keys: PushKeys,
    #[serde(default)]
    pub user_agent: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PushSubscribeResponse {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct PushUnsubscribeRequest {
    pub endpoint: String,
}

fn failure(error: PushError) -> ApiFailure {
    match error {
        PushError::Invalid(message) => ApiFailure::invalid(message),
        other => ApiFailure::new(ErrorCode::Internal, other.to_string()),
    }
}

/// `GET /v1/push/key`
pub async fn key(State(api): State<Api>) -> ApiResult<Json<PushKeyResponse>> {
    let public_key = api.state.push.public_key().await.map_err(failure)?;
    Ok(Json(PushKeyResponse { public_key }))
}

/// `POST /v1/push/subscriptions`
pub async fn subscribe(
    State(api): State<Api>,
    Json(request): Json<PushSubscribeRequest>,
) -> ApiResult<Json<PushSubscribeResponse>> {
    let id = api
        .state
        .push
        .subscribe(
            &request.endpoint,
            &request.keys.p256dh,
            &request.keys.auth,
            request.user_agent.as_deref(),
        )
        .await
        .map_err(failure)?;
    Ok(Json(PushSubscribeResponse { id }))
}

/// `DELETE /v1/push/subscriptions`
pub async fn unsubscribe(
    State(api): State<Api>,
    Json(request): Json<PushUnsubscribeRequest>,
) -> ApiResult<StatusCode> {
    api.state
        .push
        .unsubscribe(&request.endpoint)
        .await
        .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
