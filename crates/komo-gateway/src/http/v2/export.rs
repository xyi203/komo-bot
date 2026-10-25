//! Durable 模式的 `export` 路由。W4c 填；W0b 只留空桩。

use axum::Router;

use crate::http::Api;

pub fn routes() -> Router<Api> {
    Router::new()
}
