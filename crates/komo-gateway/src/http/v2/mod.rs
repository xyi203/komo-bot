//! HTTP 路由（会话、run、树、事件、intervention、导出）：路径与请求 / 响应体是冻结的协议
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
//! | `interventions` | 待处理清单、详情、答复、reconcile | W4a |
//! | `export` | `GET /v1/sessions/{id}/export` | W4c |
//! | `monitors` | `GET /v1/monitors`（只读清单） | S4 |
//! | `tasks` | `GET /v1/tasks`（任务清单，只读投影） | T1 |
//! | `changes` | 每轮改了哪些文件与逐文件对比（只活在进程里） | — |

pub mod changes;
pub mod events;
pub mod export;
pub mod interventions;
pub mod monitors;
pub mod runs;
pub mod sessions;
pub mod tasks;
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
        .merge(changes::routes())
        .merge(events::routes())
        .merge(interventions::routes())
        .merge(export::routes())
        .merge(monitors::routes())
        .merge(tasks::routes())
}
