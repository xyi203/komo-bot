//! `GET /v1/tasks`：任务清单（`docs/tasks.md` §3.3）。只读投影，把会话翻成面向人的任务行；
//! 哪些会话算任务、状态怎么读出来见 `service::durable::sessions::task_list`。
//!
//! 参数同会话列表：`?all` 带上已归档的；`limit` / `cursor` 分页，不带 `limit` 就是全部。

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use komo_kernel::protocol::http::TaskListResponse;

use super::sessions::{ListPaging, MAX_PAGE, flag};
use crate::http::Api;
use crate::http::error::ApiResult;
use crate::service::durable::sessions::{SessionError, SessionPageCursor};

pub fn routes() -> Router<Api> {
    Router::new().route("/v1/tasks", get(list))
}

pub(crate) async fn list(
    State(api): State<Api>,
    uri: axum::http::Uri,
    Query(paging): Query<ListPaging>,
) -> ApiResult<Json<TaskListResponse>> {
    let core = api.state.core()?;
    let all = flag(uri.query().unwrap_or_default(), "all");
    let limit = match paging.limit {
        Some(0) => return Err(SessionError::Invalid("limit 至少是 1".into()).into()),
        limit => limit.map(|limit| limit.min(MAX_PAGE) as usize),
    };
    let after = paging
        .cursor
        .as_deref()
        .filter(|_| limit.is_some())
        .map(SessionPageCursor::decode)
        .transpose()?;
    let page = core.task_page(all, limit, after.as_ref()).await?;
    Ok(Json(TaskListResponse {
        tasks: page.tasks,
        next_cursor: page.next.map(|next| next.encode()),
    }))
}
