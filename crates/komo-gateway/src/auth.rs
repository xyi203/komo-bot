//! 除 `GET /healthz` 与配对换 cookie 外统一认证（§13.1，`docs/clients.md` D6）。
//!
//! 两种凭据：
//!
//! - `Authorization: Bearer <token>`：令牌在发现文件里（与数据目录同权限），CLI / TUI / 渠道插件用；
//! - 会话 cookie：浏览器经配对码换来（[`crate::web`]）。用 cookie 的请求，**会改东西的**（非
//!   GET / HEAD）还要核对 `Origin` 与 `Host` 同源，否则 403——`SameSite=Strict` 之外再加一道。
//!
//! 失败一律是 [`ErrorCode::Unauthorized`] 的 [`ErrorBody`]——不区分"没带"与"带错了"，那个区别
//! 只对猜令牌的人有用。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use komo_kernel::protocol::http::ErrorCode;

use crate::http::error::ApiFailure;
use crate::web::{WebAuth, same_origin, session_cookie};

/// 这个请求是用哪种凭据过的鉴权（放进请求扩展里，给只认 Bearer 的接口看）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// 空令牌的 Gateway（测试或显式关掉鉴权），算 Bearer 一档。
    Bearer,
    Cookie,
}

/// 鉴权层要的状态。
#[derive(Debug, Clone)]
pub struct AuthState {
    pub token: String,
    pub web: Arc<WebAuth>,
}

/// 挂在受保护路由上的那一层（`axum::middleware::from_fn_with_state(AuthState, check)`）。
pub async fn check(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiFailure> {
    // 空令牌 = 这台 Gateway 没有设令牌（只可能是测试或显式关掉），放行。
    if state.token.is_empty() || presented(&request).as_deref() == Some(state.token.as_str()) {
        request.extensions_mut().insert(Credential::Bearer);
        return Ok(next.run(request).await);
    }
    if let Some(cookie) = session_cookie(request.headers())
        && state.web.session_valid(&cookie)
    {
        if !matches!(*request.method(), Method::GET | Method::HEAD)
            && !same_origin(request.headers())
        {
            return Err(ApiFailure::new(
                ErrorCode::Denied,
                "用 cookie 发起的修改请求要求 Origin 与 Host 同源",
            ));
        }
        request.extensions_mut().insert(Credential::Cookie);
        return Ok(next.run(request).await);
    }
    Err(ApiFailure::new(
        ErrorCode::Unauthorized,
        "需要 Authorization: Bearer <token>（令牌在数据目录的 runtime/gateway.json 里），或先运行 komo web 配对浏览器",
    ))
}

/// 请求里带的那个令牌。
fn presented(request: &Request) -> Option<String> {
    let value = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    Some(token.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with(header: Option<&str>) -> Request {
        let mut builder = axum::http::Request::builder().uri("/v1/sessions");
        if let Some(header) = header {
            builder = builder.header(axum::http::header::AUTHORIZATION, header);
        }
        builder.body(axum::body::Body::empty()).expect("请求")
    }

    #[test]
    fn a_bearer_token_is_read_case_insensitively() {
        assert_eq!(
            presented(&request_with(Some("Bearer abc"))).as_deref(),
            Some("abc")
        );
        assert_eq!(
            presented(&request_with(Some("bearer abc"))).as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn another_scheme_is_not_a_token() {
        assert_eq!(presented(&request_with(Some("Basic abc"))), None);
        assert_eq!(presented(&request_with(None)), None);
    }
}
