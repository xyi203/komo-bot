//! `GET /v1/mcp`（`docs/mcp.md` §2.6）、`POST /v1/mcp/reconnect`（§2.5）与 `POST /v1/mcp/disconnect`，
//! 以及设置页的两个持久开关：`PUT /v1/mcp/{name}`（整台）、`PUT /v1/mcp/{name}/tools/{tool}`（逐个工具）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use komo_kernel::protocol::config::McpTransport;
use komo_kernel::protocol::http::{
    EnabledRequest, ErrorCode, McpReconnectRequest, McpReconnectResponse, McpServersResponse,
};
use komo_tools::mcp_auth::{self, WebAuthorizationSession};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

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
// 会话只在内存里（10 分钟、用一次），按授权地址里的 `state` 对号。
//
// 发起要登录；回调是公开路由，在处理函数里自己认人：桌面端用设备令牌发起、再打开系统浏览器，回调落在
// 浏览器里，带的是网页会话（或者还没登录），与发起时不是同一条凭据。所以回调只要求「是这台 Gateway 的
// 登录用户」——任一条凭据都行，没登录先跳 `/auth/login?next=` 登录再回来（state 这时还没取走）。
// 这一步仍挡得住授权地址泄露：拿到地址的人能在授权服务器上用他的账号授权，但他的浏览器过不了这里的
// 登录，换不成 token。凭证是整台 Gateway 共用的，绑「发起的那条凭据」挡不住任何登录用户自己完成回调，
// 反而把跨端交接挡死了。

/// 一次等回调的授权会话最多挂多久。
const LOGIN_TTL: SignedDuration = SignedDuration::from_mins(10);
/// 同时挂着的上限：满了先丢最早的。
const MAX_PENDING: usize = 16;

struct PendingLogin {
    server: String,
    session: Arc<WebAuthorizationSession>,
    expires: Timestamp,
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
    fn insert(&self, state: String, login: PendingLogin, now: Timestamp) {
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
        now: Timestamp,
    ) -> Option<Arc<WebAuthorizationSession>> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let login = pending.remove(state)?;
        (login.server == server && login.expires > now).then_some(login.session)
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
    Path(name): Path<String>,
) -> ApiResult<Json<McpOauthStartResponse>> {
    let snapshot = api.state.snapshot();
    let Some(config) = snapshot.mcp.get(&name) else {
        return Err(ApiFailure::not_found(format!("MCP 服务器 `{name}`")));
    };
    let McpTransport::Http {
        url, oauth: true, ..
    } = &config.transport
    else {
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

/// `GET /v1/mcp/{name}/oauth/callback`：授权服务器跳回来的地方（浏览器顶层导航）。公开路由，自己认人
/// （见本节开头）：没登录跳去登录、登录完回到这里；认出来了再按 `state` 取会话。回一张简单的 HTML 页，
/// 成功后后台重连这台服务器。
pub async fn oauth_callback(
    State(api): State<Api>,
    Path(name): Path<String>,
    Query(query): Query<OauthCallbackQuery>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
) -> Response {
    match api.auth.authenticate(&headers, &method).await {
        Ok(Some(_)) => {}
        // 登录完回到同一个回调地址；state 还在等着，没被取走。
        Ok(None) => return login_first(&uri),
        Err(failure) => return failure.into_response(),
    }
    if let Some(error) = &query.error {
        let detail = query.error_description.clone().unwrap_or_default();
        return result_page(
            StatusCode::BAD_REQUEST,
            "授权没有完成",
            &format!("授权服务器返回：{error} {detail}"),
        );
    }
    let (Some(code), Some(state)) = (&query.code, &query.state) else {
        return result_page(
            StatusCode::BAD_REQUEST,
            "授权没有完成",
            "回调缺少 code 或 state",
        );
    };
    let Some(session) = api.mcp_logins.take(state, &name, api.state.clock.now()) else {
        return result_page(
            StatusCode::BAD_REQUEST,
            "授权已失效",
            "这次授权过期了、已经用过，或不是在这台 Gateway 上发起的；回到 Komo 重新点「登录」",
        );
    };
    // 用会话自己记的回调地址拼出完整的回跳地址（授权服务器带回来的参数照原样放进去）。
    let Ok(mut callback) = reqwest::Url::parse(&session.redirect_uri) else {
        return result_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "授权没有完成",
            "回调地址不合法",
        );
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

/// 没登录的回调：303 去 `/auth/login?next=<这个回调的路径与参数>`，登录完浏览器带着 cookie 回来。
fn login_first(uri: &Uri) -> Response {
    let next = uri
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or_else(|| uri.path());
    let location = reqwest::Url::parse_with_params("http://komo/auth/login", [("next", next)])
        .map(|url| format!("/auth/login?{}", url.query().unwrap_or_default()));
    let Ok(Ok(value)) = location.map(|location| HeaderValue::from_str(&location)) else {
        return result_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "授权没有完成",
            "登录跳转地址不合法",
        );
    };
    let mut response = StatusCode::SEE_OTHER.into_response();
    response.headers_mut().insert(header::LOCATION, value);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}
