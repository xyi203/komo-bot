//! Durable 模式的会话树路由（`docs/storage-durable-d5.md` §7）：tree / fork / name。
//! navigate / context-edit / label / compact 已在 2026-11-12 随会话树的用户入口一起删除
//! （`docs/tasks.md` §5，D5）。
//!
//! 服务层在 `service::durable::sessions::{tree, copy}`；这里只做协议翻译：
//!
//! - `/tree` = `pi_tree(各 conversation 自己的 entry, current_branch, session.leaf)`——传**原始的**
//!   `session.leaf`（`None` 跟末端，`Some(BEFORE_FIRST)` 在第一条之前），不传解出来的 `TreeData.leaf`。
//! - `name` 返回操作之后的 `leaf_id`：重读一遍树、用同一个 `pi_tree` 算，所以和下一次
//!   `/tree` 的 `leaf_id` 一字不差。
//! - `fork` 在任一分支忙（有 run、收件箱非空、压缩中）时 409；会话不收输入 → 409 + 状态。

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use komo_kernel::durable::pi_tree::{ConversationEntries, PiTree, pi_tree};
use komo_kernel::protocol::http::{
    ForkSessionResponse, NameSessionRequest, NavigateSessionResponse, SessionTreeResponse,
};
use komo_kernel::types::ids::SessionId;

use crate::http::Api;
use crate::http::error::ApiResult;

pub fn routes() -> Router<Api> {
    Router::new()
        .route("/v1/sessions/{id}/tree", get(tree))
        .route("/v1/sessions/{id}/fork", post(fork))
        .route("/v1/sessions/{id}/name", post(name))
}

/// 一个 Session 现在的 pi 树。
async fn projected(api: &Api, session: &str) -> ApiResult<PiTree> {
    let core = api.state.core()?;
    let data = core.read_tree(session).await?;
    let conversations: Vec<ConversationEntries<'_>> = data
        .branches
        .iter()
        .map(|branch| ConversationEntries {
            record: &branch.conversation,
            entries: &branch.entries,
        })
        .collect();
    Ok(pi_tree(
        &conversations,
        data.session.current_branch,
        data.session.leaf,
    ))
}

/// 改名之后的回执：重算 `leaf_id`。
async fn receipt(api: &Api, session: &str) -> ApiResult<Json<NavigateSessionResponse>> {
    Ok(Json(NavigateSessionResponse {
        leaf_id: projected(api, session).await?.leaf_id,
        editor_text: None,
        cancelled: false,
    }))
}

/// `GET /v1/sessions/{id}/tree`
async fn tree(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionTreeResponse>> {
    let pi = projected(&api, &id).await?;
    Ok(Json(SessionTreeResponse {
        entries: pi.entries,
        leaf_id: pi.leaf_id,
    }))
}

/// `POST /v1/sessions/{id}/name`
async fn name(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<NameSessionRequest>,
) -> ApiResult<Json<NavigateSessionResponse>> {
    let core = api.state.core()?;
    core.name(&id, &request.name).await?;
    receipt(&api, &id).await
}

/// `POST /v1/sessions/{id}/fork`：把当前会话**复制**成新会话（网页会话菜单的"分叉"）。
async fn fork(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<ForkSessionResponse>> {
    let core = api.state.core()?;
    let record = core.fork_session(&id).await?;
    Ok(Json(ForkSessionResponse {
        session: SessionId::from_raw(record.id),
    }))
}
