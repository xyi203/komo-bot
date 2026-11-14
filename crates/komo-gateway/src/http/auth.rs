//! 登录相关的路由（`docs/redesign.md` §2.9）：
//!
//! | 路由 | 登录 | 作用 |
//! |---|---|---|
//! | `GET /auth/login[?next=/路径]` | 不要 | 跳去 OIDC 提供方（授权码 + PKCE） |
//! | `GET /auth/callback` | 不要 | 提供方回调：校验、核对名单、种会话 cookie、跳回网页 |
//! | `POST /auth/logout` | 不要 | 吊销当前会话、清 cookie |
//! | `GET /auth/me` | 不要 | 当前登录状态（永远 200），网页据此决定要不要跳登录 |
//! | `GET /auth/device/authorize` | 不要 | 桌面端 / CLI 发起设备登录（浏览器里打开），完成后带一次性码跳回 |
//! | `POST /auth/device/token` | 不要（码即凭据） | 一次性码 + PKCE verifier 换长期设备令牌 |
//! | `GET /v1/auth/sessions` | 要 | 登录状态清单（网页会话与设备令牌） |
//! | `DELETE /v1/auth/sessions/{id}` | 要 | 吊销一条 |
//!
//! 浏览器直接打开的几条（login / callback / device/authorize）出错时回一张简单的 HTML 页，不回
//! JSON：人看得懂比机器读得懂要紧。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use komo_kernel::protocol::http::{
    AuthMeResponse, AuthRevokeResponse, AuthSessionsResponse, DeviceTokenRequest,
    DeviceTokenResponse, ErrorCode,
};
use serde::Deserialize;

use crate::auth::{
    AuthError, Credential, DeviceRequest, LOGIN_COOKIE, Purpose, redirect_allowed,
    redirect_with_code, safe_next,
};
use crate::http::Api;
use crate::http::error::{ApiFailure, ApiResult};
use crate::web::{cookie_value, same_origin};

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    next: Option<String>,
}

/// `GET /auth/login`
pub async fn login(State(api): State<Api>, Query(query): Query<LoginQuery>) -> Response {
    let purpose = Purpose::Web {
        next: safe_next(query.next.as_deref()),
    };
    start(&api, purpose).await
}

/// 发起 OIDC：记下临时状态，种绑定浏览器的 cookie，跳去提供方。
async fn start(api: &Api, purpose: Purpose) -> Response {
    match api.auth.begin(purpose).await {
        Ok((url, state)) => {
            let mut response = redirect(&url);
            append_cookie(&mut response, &api.auth.login_cookie(&state));
            response
        }
        Err(error) => error_page(&error),
    }
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// `GET /auth/callback`
pub async fn callback(
    State(api): State<Api>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    // 提供方自己拒绝（用户点了取消、账号被停 …）：错误码是它的，原样告诉人，页面里转义。
    if let Some(error) = query.error.as_deref() {
        let mut response = page(
            StatusCode::FORBIDDEN,
            "登录没有完成",
            &format!("身份提供方返回：{error}"),
        );
        append_cookie(&mut response, &api.auth.clear_login_cookie());
        return response;
    }
    let (Some(code), Some(state)) = (query.code.as_deref(), query.state.as_deref()) else {
        return page(StatusCode::BAD_REQUEST, "登录没有完成", "回调缺少 code 或 state");
    };
    let bound = cookie_value(&headers, LOGIN_COOKIE);
    let finished = api.auth.finish(state, bound.as_deref(), code).await;
    let mut response = match finished {
        Ok((claims, Purpose::Web { next })) => {
            let user_agent = headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            match api
                .auth
                .issue_web(&claims.subject, claims.email.as_deref(), user_agent)
                .await
            {
                Ok(token) => {
                    let mut response = redirect(&next);
                    append_cookie(&mut response, &api.auth.session_cookie(&token));
                    response
                }
                Err(error) => error_page(&error),
            }
        }
        Ok((claims, Purpose::Device(request))) => {
            device_redirect(&api, request, &claims.subject, claims.email.as_deref())
        }
        Err(error) => error_page(&error),
    };
    // 临时 cookie 用完即清，不管成败。
    append_cookie(&mut response, &api.auth.clear_login_cookie());
    response
}

/// 发码并跳回客户端。
fn device_redirect(api: &Api, request: DeviceRequest, subject: &str, email: Option<&str>) -> Response {
    let (uri, state) = (request.redirect_uri.clone(), request.state.clone());
    let code = api.auth.issue_code(request, subject, email);
    match redirect_with_code(&uri, &code, state.as_deref()) {
        Some(target) => redirect(&target),
        None => page(StatusCode::BAD_REQUEST, "登录没有完成", "回跳地址不合法"),
    }
}

/// `POST /auth/logout`
pub async fn logout(State(api): State<Api>, headers: HeaderMap) -> ApiResult<Response> {
    // 有 cookie 就是浏览器在退：核对 Origin，免得别的站点替你退出（成本低，顺手挡）。
    if cookie_value(&headers, crate::auth::SESSION_COOKIE).is_some()
        && !same_origin(&headers, api.auth.public_origin().as_deref())
    {
        return Err(ApiFailure::new(
            ErrorCode::Denied,
            "退出登录要求 Origin 与 Host（或公开地址）同源",
        ));
    }
    api.auth.logout(&headers).await.map_err(api_failure)?;
    let mut response = Json(serde_json::json!({})).into_response();
    append_cookie(&mut response, &api.auth.clear_session_cookie());
    no_store(&mut response);
    Ok(response)
}

/// `GET /auth/me`
pub async fn me(State(api): State<Api>, headers: HeaderMap) -> ApiResult<Response> {
    let authed = api
        .auth
        .authenticate(&headers, &axum::http::Method::GET)
        .await?;
    let body = match authed {
        Some(authed) => AuthMeResponse {
            authenticated: true,
            oidc_configured: api.auth.oidc_configured(),
            credential: Some(authed.credential.kind()),
            session_id: authed.credential.session_id().map(str::to_string),
            subject: authed.subject,
            email: authed.email,
        },
        None => AuthMeResponse {
            authenticated: false,
            oidc_configured: api.auth.oidc_configured(),
            credential: None,
            subject: None,
            email: None,
            session_id: None,
        },
    };
    let mut response = Json(body).into_response();
    no_store(&mut response);
    Ok(response)
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    redirect_uri: Option<String>,
    state: Option<String>,
    name: Option<String>,
}

/// `GET /auth/device/authorize`：客户端在系统浏览器里打开它。
///
/// 这个浏览器已经登录过（有效会话 cookie）就直接发码；否则走一遍 OIDC，回调里再发码。
pub async fn device_authorize(
    State(api): State<Api>,
    headers: HeaderMap,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    // 回跳地址必须先过白名单，**才**有任何一种形式的跳转——不能把码送去没校验过的地址。
    let Some(redirect_uri) = query.redirect_uri.filter(|uri| redirect_allowed(uri)) else {
        return page(
            StatusCode::BAD_REQUEST,
            "不能发起设备登录",
            "redirect_uri 不在白名单里（只认 komo://auth/callback 与本机回环地址 http://127.0.0.1:端口/…）",
        );
    };
    if query
        .code_challenge_method
        .as_deref()
        .is_some_and(|method| method != "S256")
    {
        return page(StatusCode::BAD_REQUEST, "不能发起设备登录", "只支持 code_challenge_method=S256");
    }
    let Some(challenge) = query
        .code_challenge
        .filter(|c| c.len() == 43 && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
    else {
        return page(
            StatusCode::BAD_REQUEST,
            "不能发起设备登录",
            "code_challenge 缺失或不是 S256（43 个 base64url 字符）",
        );
    };
    let state = query.state.filter(|s| !s.is_empty() && s.len() <= 512);
    let name = query
        .name
        .map(|name| name.trim().chars().take(60).collect::<String>())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "设备".to_string());
    let request = DeviceRequest {
        challenge,
        redirect_uri,
        state,
        name,
    };
    match api.auth.web_identity(&headers).await {
        Ok(Some((subject, email))) => device_redirect(&api, request, &subject, email.as_deref()),
        Ok(None) => start(&api, Purpose::Device(request)).await,
        Err(error) => error_page(&error),
    }
}

/// `POST /auth/device/token`
pub async fn device_token(
    State(api): State<Api>,
    Json(request): Json<DeviceTokenRequest>,
) -> ApiResult<Response> {
    let (token, record) = api
        .auth
        .redeem_code(&request.code, &request.code_verifier, &request.redirect_uri)
        .await
        .map_err(|error| match error {
            // 码错了 / 过期 / 对不上都是 401：凭据（授权码）不成立。
            AuthError::BadRequest(message) => ApiFailure::new(ErrorCode::Unauthorized, message),
            other => api_failure(other),
        })?;
    let mut response = Json(DeviceTokenResponse {
        token,
        token_type: "Bearer".into(),
        session_id: record.id,
        subject: record.subject,
        email: record.email,
    })
    .into_response();
    no_store(&mut response);
    Ok(response)
}

/// `GET /v1/auth/sessions`
pub async fn sessions(
    State(api): State<Api>,
    axum::Extension(credential): axum::Extension<Credential>,
) -> ApiResult<Json<AuthSessionsResponse>> {
    let sessions = api
        .auth
        .list(credential.session_id())
        .await
        .map_err(api_failure)?;
    Ok(Json(AuthSessionsResponse { sessions }))
}

/// `DELETE /v1/auth/sessions/{id}`
pub async fn revoke(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<AuthRevokeResponse>> {
    if api.auth.revoke(&id).await.map_err(api_failure)? {
        Ok(Json(AuthRevokeResponse { revoked: true }))
    } else {
        Err(ApiFailure::not_found(format!("登录状态 {id}")))
    }
}

// ---------------------------------------------------------------- 小件

fn api_failure(error: AuthError) -> ApiFailure {
    match error {
        AuthError::NotConfigured => ApiFailure::new(ErrorCode::NotFound, error.to_string()),
        AuthError::BadRequest(message) => ApiFailure::new(ErrorCode::InvalidRequest, message),
        AuthError::BadState => ApiFailure::new(ErrorCode::Unauthorized, error.to_string()),
        AuthError::Denied => ApiFailure::new(ErrorCode::Denied, error.to_string()),
        AuthError::Provider(message) | AuthError::Internal(message) => {
            ApiFailure::new(ErrorCode::Internal, message)
        }
    }
}

/// 303 跳转（`GET` 导航最稳妥的一种），带 `no-store`。
fn redirect(location: &str) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    match HeaderValue::from_str(location) {
        Ok(value) => {
            response.headers_mut().insert(header::LOCATION, value);
        }
        Err(_) => {
            return page(StatusCode::INTERNAL_SERVER_ERROR, "登录没有完成", "跳转地址不合法");
        }
    }
    no_store(&mut response);
    response
}

fn append_cookie(response: &mut Response, cookie: &str) {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

fn no_store(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

/// 登录失败的页面。状态码与说法按失败的种类分。
fn error_page(error: &AuthError) -> Response {
    let (status, title) = match error {
        AuthError::NotConfigured => (StatusCode::NOT_FOUND, "没有配置登录"),
        AuthError::BadRequest(_) | AuthError::BadState => (StatusCode::BAD_REQUEST, "登录没有完成"),
        AuthError::Denied => (StatusCode::FORBIDDEN, "不允许登录"),
        AuthError::Provider(_) => (StatusCode::BAD_GATEWAY, "身份提供方不可用"),
        AuthError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "登录没有完成"),
    };
    page(status, title, &error.to_string())
}

/// 一张只有标题与一句话的 HTML 页（内容转义；不引外部资源）。
fn page(status: StatusCode, title: &str, detail: &str) -> Response {
    let body = format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{title}</title><body style=\"font-family:system-ui,sans-serif;max-width:32rem;margin:15vh auto;padding:0 1rem;line-height:1.6\"><h1 style=\"font-size:1.25rem\">{title}</h1><p>{detail}</p><p><a href=\"/auth/login\">重新登录</a></p></body></html>",
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
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    response
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_text_is_escaped() {
        assert_eq!(escape("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }
}
