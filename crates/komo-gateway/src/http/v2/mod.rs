//! HTTP 路由（会话、run、树、事件、cron、intervention、导出）：路径与请求 / 响应体是冻结的协议
//! （`komo_kernel::protocol::http`），handler 读写 `durable.db`（`docs/storage-durable-d5.md` §1、§4）。
//! memories、config、models、mcp、healthz 在上一层（`http/*.rs`）。
//!
//! 每个子模块一组路由、一个 `routes()`：
//!
//! | 文件 | 路由 | 包 |
//! |---|---|---|
//! | `sessions` | 会话 create / list / detail / submit / boundary / resume / delete / purge | W2a |
//! | `tree` | tree / navigate / fork / context-edit / label / name / compact | W2a |
//! | `runs` | run detail / cancel | W2a |
//! | `events` | 事件 JSON 分页与 SSE | W2b |
//! | `cron` | cron 增删改与 run-now | W3b |
//! | `interventions` | 待处理清单、详情、答复、reconcile | W4a |
//! | `export` | `GET /v1/sessions/{id}/export` | W4c |

pub mod cron;
pub mod events;
pub mod export;
pub mod interventions;
pub mod runs;
pub mod sessions;
pub mod tree;

#[cfg(test)]
mod tree_tests;

use axum::Router;

use super::Api;

/// 这一组路由（认证 layer 由 `http::router` 统一加）。
pub fn routes() -> Router<Api> {
    Router::new()
        .merge(sessions::routes())
        .merge(tree::routes())
        .merge(runs::routes())
        .merge(events::routes())
        .merge(cron::routes())
        .merge(interventions::routes())
        .merge(export::routes())
}
