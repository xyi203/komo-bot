//! 除 `GET /healthz` 外统一认证（§13.1）。
//!
//! 令牌在发现文件里（与数据目录同权限），客户端按 `Authorization: Bearer <token>` 带
//! 上。失败一律是 [`ErrorCode::Unauthorized`] 的 [`ErrorBody`]——不区分"没带"与"带错
//! 了"，那个区别只对猜令牌的人有用。

use axum::extract::Request;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::Response;
use komo_kernel::protocol::http::ErrorCode;

use crate::http::error::ApiFailure;

/// 这一层的类型。`from_fn_with_state` 的返回类型写出来很长，起个名字。
pub type BearerLayer = axum::middleware::FromFnLayer<CheckFn, String, CheckArgs>;
type CheckFn = fn(
    axum::extract::State<String>,
    Request,
    Next,
) -> std::pin::Pin<Box<dyn Future<Output = Result<Response, ApiFailure>> + Send>>;
type CheckArgs = (axum::extract::State<String>, Request);

/// 挂在受保护路由上的那一层。
pub fn bearer(token: String) -> BearerLayer {
    from_fn_with_state(token, check as CheckFn)
}

fn check(
    axum::extract::State(expected): axum::extract::State<String>,
    request: Request,
    next: Next,
) -> std::pin::Pin<Box<dyn Future<Output = Result<Response, ApiFailure>> + Send>> {
    Box::pin(async move {
        // 空令牌 = 这台 Gateway 没有设令牌（只可能是测试或显式关掉），放行。
        if expected.is_empty() || presented(&request).as_deref() == Some(expected.as_str()) {
            return Ok(next.run(request).await);
        }
        Err(ApiFailure::new(
            ErrorCode::Unauthorized,
            "需要 Authorization: Bearer <token>（令牌在数据目录的 runtime/gateway.json 里）",
        ))
    })
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
