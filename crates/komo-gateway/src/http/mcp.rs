//! `GET /v1/mcp`（`docs/mcp.md` §2.6）、`POST /v1/mcp/reconnect`（§2.5）与 `POST /v1/mcp/disconnect`。

use axum::Json;
use axum::extract::State;
use komo_kernel::protocol::http::{McpReconnectRequest, McpReconnectResponse, McpServersResponse};

use super::Api;
use super::error::{ApiFailure, ApiResult};

/// 每台服务器此刻的样子：不等握手、不重连，给设置页的服务器表。
pub async fn list(State(api): State<Api>) -> Json<McpServersResponse> {
    Json(McpServersResponse {
        servers: api.state.mcp.views(),
    })
}

/// 点名的服务器断开重连（空 = 全部），**等握手**：人手动敲的命令要当场看到连没连上。点了配置里
/// 没有的名字 400，一台也不动。
pub async fn reconnect(
    State(api): State<Api>,
    Json(request): Json<McpReconnectRequest>,
) -> ApiResult<Json<McpReconnectResponse>> {
    let servers = api
        .state
        .reconnect_mcp(&request.servers)
        .await
        .map_err(ApiFailure::invalid)?;
    Ok(Json(McpReconnectResponse { servers }))
}

/// 点名的服务器断开（空 = 全部），停到人重连为止；名字不在配置里 400，一台也不动。
pub async fn disconnect(
    State(api): State<Api>,
    Json(request): Json<McpReconnectRequest>,
) -> ApiResult<Json<McpReconnectResponse>> {
    let servers = api
        .state
        .disconnect_mcp(&request.servers)
        .await
        .map_err(ApiFailure::invalid)?;
    Ok(Json(McpReconnectResponse { servers }))
}
