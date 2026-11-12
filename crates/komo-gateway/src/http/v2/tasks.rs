//! `GET /v1/tasks`：任务清单（`docs/tasks.md` §3.3）。只读投影，把会话翻成面向人的任务行；
//! 哪些会话算任务、状态怎么读出来见 `service::durable::sessions::task_list`。
//!
//! 参数同会话列表：`?all` 带上已归档的；`limit` / `cursor` 分页，不带 `limit` 就是全部。
//! `?goal=<目标 id>` 只要带着这个目标的键的任务（分页规则不变）。

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use komo_kernel::protocol::http::TaskListResponse;
use komo_kernel::types::ids::MatterId;
use komo_kernel::types::matter::MatterKind;

use super::sessions::{MAX_PAGE, flag};
use crate::http::Api;
use crate::http::error::ApiResult;
use crate::service::durable::sessions::{SessionError, SessionPageCursor};

pub fn routes() -> Router<Api> {
    Router::new().route("/v1/tasks", get(list))
}

/// 分页（同会话列表）加目标过滤；`all` 走 [`flag`]。
#[derive(serde::Deserialize)]
pub(crate) struct TaskPaging {
    pub(crate) limit: Option<u32>,
    pub(crate) cursor: Option<String>,
    pub(crate) goal: Option<MatterId>,
}

pub(crate) async fn list(
    State(api): State<Api>,
    uri: axum::http::Uri,
    Query(paging): Query<TaskPaging>,
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
    if paging
        .goal
        .as_ref()
        .is_some_and(|goal| goal.as_str().is_empty())
    {
        return Err(SessionError::Invalid("goal 不能是空的".into()).into());
    }
    let page = core
        .task_page_for_goal(all, paging.goal.as_ref(), limit, after.as_ref())
        .await?;
    let mut tasks = page.tasks;
    // 行里的目标只有 id；标题从目标清单补（含已结案的）。目标已不存在的保持只有 id。
    if tasks.iter().any(|task| !task.goals.is_empty()) {
        let titles: std::collections::HashMap<String, String> = api
            .state
            .matters
            .list(Some(MatterKind::Goal), true)
            .await
            .map_err(|error| SessionError::Internal(format!("读不了目标清单：{error}")))?
            .into_iter()
            .map(|matter| (matter.id.as_str().to_owned(), matter.title))
            .collect();
        for goal in tasks.iter_mut().flat_map(|task| task.goals.iter_mut()) {
            goal.title = titles.get(goal.id.as_str()).cloned();
        }
    }
    Ok(Json(TaskListResponse {
        tasks,
        next_cursor: page.next.map(|next| next.encode()),
    }))
}
