//! Durable 模式的 `/v1/cron`（`docs/storage-durable-d5.md` §4 W3b）。
//!
//! 路由与请求 / 响应体见 [`crate::http::cron`]：Job 定义与触发记录在 `aux.db`，handler 只碰
//! `CronRepo` 与 `CronScheduler`（触发目标是 `DurableCronTarget`，`GatewayState::cron_scheduler`）。
//! 一条规矩在 [`crate::http::cron`] 的 `resolve_model` 里：Job 的模型覆盖必须能
//! 对上 `model_catalog` 里的 alias（`komo.agent.model` 只认 alias），对不上当场 400。

use axum::Router;
use axum::routing::{get, patch, post};

use crate::http::Api;
use crate::http::cron::{create, list, remove, run_now, update};

pub fn routes() -> Router<Api> {
    Router::new()
        .route("/v1/cron", get(list).post(create))
        .route("/v1/cron/{id}", patch(update).delete(remove))
        .route("/v1/cron/{id}/run", post(run_now))
}
