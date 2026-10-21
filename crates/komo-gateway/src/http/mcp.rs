//! `POST /v1/mcp/reconnect`（`docs/mcp.md` §2.5）。

use axum::Json;
use axum::extract::State;
use komo_kernel::protocol::http::{McpReconnectRequest, McpReconnectResponse};

use super::Api;
use super::error::{ApiFailure, ApiResult};

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
