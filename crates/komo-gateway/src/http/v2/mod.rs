//! Durable 模式的 HTTP 路由（`docs/storage-durable-d5.md` §1、§4）。
//!
//! 协议冻结：路径与请求 / 响应体和 Legacy 一致，只是 handler 换成读写新存储。没改的路由
//! （memories、config、models、mcp、healthz）由 `http::router` 直接复用旧 handler，不在这里。
//!
//! 每个子模块一组路由、一个 `routes()`；W0b 只留空桩，各包往自己的文件里填：
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

use axum::Router;

use super::Api;

/// Durable 模式独有的那部分路由（认证 layer 由 `http::router` 统一加）。
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
