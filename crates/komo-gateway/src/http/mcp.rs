//! `GET /v1/mcp`（`docs/mcp.md` §2.6）、`POST /v1/mcp/reconnect`（§2.5）与 `POST /v1/mcp/disconnect`，
//! 以及设置页的两个持久开关：`PUT /v1/mcp/{name}`（整台）、`PUT /v1/mcp/{name}/tools/{tool}`（逐个工具）。

use axum::Json;
use axum::extract::{Path, State};
use komo_kernel::protocol::http::{
    EnabledRequest, McpReconnectRequest, McpReconnectResponse, McpServersResponse,
};

use super::Api;
use super::config::write_config;
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

/// 整台开 / 关（持久，写 `[mcp.servers.<name>] enabled`，热重载生效）：关 = 断开、撤工具、重启后仍关着；
/// 开 = 后台连，工具连上后从下一条 Run 起可用。交回所有服务器此刻的样子。
pub async fn set_server(
    State(api): State<Api>,
    Path(name): Path<String>,
    Json(request): Json<EnabledRequest>,
) -> ApiResult<Json<McpServersResponse>> {
    write_config(&api, None, |config, _| {
        Ok((
            Some(komo_config::set_mcp_server_enabled(
                config,
                &name,
                request.enabled,
            )?),
            None,
        ))
    })
    .await?;
    Ok(Json(McpServersResponse {
        servers: api.state.mcp.views(),
    }))
}

/// 逐个工具开 / 关（持久，写 `disabled_tools`，不重连服务器）。关一个工具要求服务器列出过它（`GET /v1/mcp`
/// 看得到）；开不查，好让操作者清掉服务器已经没有的名字。
pub async fn set_tool(
    State(api): State<Api>,
    Path((name, tool)): Path<(String, String)>,
    Json(request): Json<EnabledRequest>,
) -> ApiResult<Json<McpServersResponse>> {
    if !request.enabled {
        let views = api.state.mcp.views();
        let server = views
            .iter()
            .find(|server| server.name == name)
            .ok_or_else(|| ApiFailure::not_found(format!("MCP 服务器 `{name}`")))?;
        if !server.tools.iter().any(|listed| listed.name == tool) {
            return Err(ApiFailure::invalid(format!(
                "MCP 服务器 `{name}` 没有列出过工具 `{tool}`（还没连上，或名字不对）"
            )));
        }
    }
    write_config(&api, None, |config, _| {
        Ok((
            Some(komo_config::set_mcp_tool_enabled(
                config,
                &name,
                &tool,
                request.enabled,
            )?),
            None,
        ))
    })
    .await?;
    Ok(Json(McpServersResponse {
        servers: api.state.mcp.views(),
    }))
}
