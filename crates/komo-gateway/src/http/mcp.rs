//! `GET /v1/mcp`（`docs/mcp.md` §2.6）、`POST /v1/mcp/reconnect`（§2.5）与 `POST /v1/mcp/disconnect`，
//! 以及设置页的两个持久开关：`PUT /v1/mcp/{name}`（整台）、`PUT /v1/mcp/{name}/tools/{tool}`（逐个工具）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use komo_kernel::protocol::config::McpTransport;
use komo_kernel::protocol::http::{
    EnabledRequest, ErrorCode, McpReconnectRequest, McpReconnectResponse, McpServersResponse,
};
use komo_tools::mcp_auth::{self, WebAuthorizationSession};

use crate::auth::Credential;
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

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

// ------------------------------------------------------------------ 网页里的 MCP 登录（OAuth）
//
// 命令行的 `komo auth mcp login` 在本机起随机端口收回调；Gateway 在远程机器上、操作者只用网页时那条路
// 走不通。网页的做法：`POST /v1/mcp/{name}/oauth/start` 在 Gateway 里备好授权会话（动态注册 + PKCE），
// 返回授权地址；浏览器去授权；授权服务器跳回 `<public_url>/v1/mcp/{name}/oauth/callback`，Gateway 换 token、
// 落进与命令行相同的凭证文件（`<数据目录>/mcp/<name>/credentials.json`），再后台重连这台服务器。
// 两个路由都在登录之后；会话只在内存里（10 分钟、用一次），按授权地址里的 `state` 对号。

/// 一次等回调的授权会话最多挂多久。
const LOGIN_TTL: Duration = Duration::minutes(10);
/// 同时挂着的上限：满了先丢最早的。
const MAX_PENDING: usize = 16;

struct PendingLogin {
    server: String,
    /// 发起它的那条登录状态（网页会话 / 设备令牌的 id；本机令牌是 `None`）。回调必须来自同一条：
    /// 否则别人可以把他自己的授权回调发给你点，把你的 MCP 绑到他的账号上。
    owner: Option<String>,
    session: Arc<WebAuthorizationSession>,
    expires: OffsetDateTime,
}

/// 等回调的 MCP 登录（`state` → 会话）。
#[derive(Default)]
pub struct McpLogins {
    pending: Mutex<HashMap<String, PendingLogin>>,
}

impl std::fmt::Debug for McpLogins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpLogins").finish_non_exhaustive()
    }
}

impl McpLogins {
    fn insert(&self, state: String, login: PendingLogin, now: OffsetDateTime) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, login| login.expires > now);
        while pending.len() >= MAX_PENDING {
            let oldest = pending
                .iter()
                .min_by_key(|(_, login)| login.expires)
                .map(|(state, _)| state.clone());
            match oldest {
                Some(state) => pending.remove(&state),
                None => break,
            };
        }
        pending.insert(state, login);
    }

    /// 取走（用一次作废）。过期或服务器名对不上都当没有。
    fn take(
        &self,
        state: &str,
        server: &str,
        owner: Option<&str>,
        now: OffsetDateTime,
    ) -> Option<Arc<WebAuthorizationSession>> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let login = pending.remove(state)?;
        (login.server == server && login.owner.as_deref() == owner && login.expires > now)
            .then_some(login.session)
    }
}

/// `POST /v1/mcp/{name}/oauth/start` 的响应：浏览器要去的授权地址。
#[derive(Debug, Serialize)]
pub struct McpOauthStartResponse {
    pub authorization_url: String,
}

/// 服务器名要能原样放进 URL 路径（回调地址里带它）。
fn path_safe(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `POST /v1/mcp/{name}/oauth/start`：只对 `oauth = true` 的 HTTP 服务器；回调地址要有公开地址
/// （`[auth.oidc] public_url`），没有就 400——没有公开地址的本机用法继续用 `komo auth mcp login`。
pub async fn oauth_start(
    State(api): State<Api>,
    Extension(credential): Extension<Credential>,
    Path(name): Path<String>,
) -> ApiResult<Json<McpOauthStartResponse>> {
    let snapshot = api.state.snapshot();
    let Some(config) = snapshot.mcp.get(&name) else {
        return Err(ApiFailure::not_found(format!("MCP 服务器 `{name}`")));
    };
    let McpTransport::Http { url, oauth: true, .. } = &config.transport else {
        return Err(ApiFailure::invalid(format!(
            "[mcp.servers.{name}] 不是 oauth = true 的 HTTP 服务器"
        )));
    };
    let Some(oidc) = &snapshot.start_only.oidc else {
        return Err(ApiFailure::invalid(
            "没有公开地址（[auth.oidc] public_url）：网页登录的回调回不到这台 Gateway；本机请用 komo auth mcp login",
        ));
    };
    if !path_safe(&name) {
        return Err(ApiFailure::invalid(format!(
            "服务器名 `{name}` 含有不能放进回调地址的字符（只认字母、数字、-、_、.）"
        )));
    }
    let redirect_uri = format!(
        "{}/v1/mcp/{name}/oauth/callback",
        oidc.public_url.trim_end_matches('/')
    );
    let path = mcp_auth::credentials_path(&snapshot.start_only.data_dir, &name);
    let session = mcp_auth::begin_session(url, &path, &redirect_uri)
        .await
        .map_err(|error| ApiFailure::new(ErrorCode::Internal, format!("发起授权失败：{error}")))?;
    let authorization_url = session.auth_url.clone();
    let Some(state) = mcp_auth::authorization_state(&authorization_url) else {
        return Err(ApiFailure::new(
            ErrorCode::Internal,
            "授权地址里没有 state，无法对回调",
        ));
    };
    let now = api.state.clock.now();
    api.mcp_logins.insert(
        state,
        PendingLogin {
            server: name,
            owner: credential.session_id().map(str::to_owned),
            session: Arc::new(session),
            expires: now + LOGIN_TTL,
        },
        now,
    );
    Ok(Json(McpOauthStartResponse { authorization_url }))
}

#[derive(Debug, Deserialize)]
pub struct OauthCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    iss: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// `GET /v1/mcp/{name}/oauth/callback`：授权服务器跳回来的地方（浏览器顶层导航，带登录 cookie）。回一张
/// 简单的 HTML 页，成功后后台重连这台服务器。
pub async fn oauth_callback(
    State(api): State<Api>,
    Extension(credential): Extension<Credential>,
    Path(name): Path<String>,
    Query(query): Query<OauthCallbackQuery>,
) -> Response {
    if let Some(error) = &query.error {
        let detail = query.error_description.clone().unwrap_or_default();
        return result_page(StatusCode::BAD_REQUEST, "授权没有完成", &format!("授权服务器返回：{error} {detail}"));
    }
    let (Some(code), Some(state)) = (&query.code, &query.state) else {
        return result_page(StatusCode::BAD_REQUEST, "授权没有完成", "回调缺少 code 或 state");
    };
    let Some(session) = api.mcp_logins.take(state, &name, credential.session_id(), api.state.clock.now()) else {
        return result_page(
            StatusCode::BAD_REQUEST,
            "授权已失效",
            "这次授权过期了、已经用过，或不是在这台 Gateway 上、用这个登录状态发起的；回到 Komo 重新点「登录」",
        );
    };
    // 用会话自己记的回调地址拼出完整的回跳地址（授权服务器带回来的参数照原样放进去）。
    let Ok(mut callback) = reqwest::Url::parse(&session.redirect_uri) else {
        return result_page(StatusCode::INTERNAL_SERVER_ERROR, "授权没有完成", "回调地址不合法");
    };
    {
        let mut pairs = callback.query_pairs_mut();
        pairs.append_pair("code", code);
        pairs.append_pair("state", state);
        if let Some(iss) = &query.iss {
            pairs.append_pair("iss", iss);
        }
    }
    if let Err(error) = mcp_auth::complete_session(&session, callback.as_str()).await {
        return result_page(StatusCode::BAD_GATEWAY, "授权没有完成", &error.to_string());
    }
    // 凭证已落盘：后台重连，工具从下一条 Run 起可用。
    let state = Arc::clone(&api.state);
    tokio::spawn(async move {
        if let Err(error) = state.reconnect_mcp(&[name.clone()]).await {
            tracing::warn!(%error, server = %name, "MCP 授权完成但重连没成功");
        }
    });
    result_page(StatusCode::OK, "授权完成", "可以关掉这个页面，回到 Komo。")
}

/// 只有标题与一句话的 HTML 页（内容转义）。
fn result_page(status: StatusCode, title: &str, detail: &str) -> Response {
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;")
    };
    let body = format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{title}</title><body style=\"font-family:system-ui,sans-serif;max-width:32rem;margin:15vh auto;padding:0 1rem;line-height:1.6\"><h1 style=\"font-size:1.25rem\">{title}</h1><p>{detail}</p></body></html>",
        title = escape(title),
        detail = escape(detail),
    );
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}
