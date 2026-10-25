//! Durable 模式的会话树路由（`docs/storage-durable-d5.md` §7）：tree / navigate / fork /
//! context-edit / label / name / compact。
//!
//! 服务层在 `service::durable::{tree, copy}`；这里只做协议翻译：
//!
//! - `/tree` = `pi_tree(各 conversation 自己的 entry, current_branch, session.leaf)`——传**原始的**
//!   `session.leaf`（`None` 跟末端，`Some(BEFORE_FIRST)` 在第一条之前），不传解出来的 `TreeData.leaf`。
//! - 每个改树的操作返回操作之后的 `leaf_id`：重读一遍树、用同一个 `pi_tree` 算，所以和下一次
//!   `/tree` 的 `leaf_id` 一字不差（一条 durable entry 展开成几条 pi entry 时，叶子是最后一条展开）。
//! - 任一分支忙（有 run、收件箱非空、压缩中）→ 409；会话不收输入 → 409 + 状态。

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use komo_kernel::durable::pi_tree::{ConversationEntries, PiTree, pi_tree};
use komo_kernel::protocol::http::{
    EditSessionContextRequest, ForkSessionRequest, ForkSessionResponse, LabelSessionRequest,
    NameSessionRequest, NavigateSessionRequest, NavigateSessionResponse, SessionTreeResponse,
};
use komo_kernel::traits::LlmClient;
use komo_kernel::types::ids::SessionId;

use crate::http::Api;
use crate::http::error::ApiResult;
use crate::service::durable::copy::ForkRequest;
use crate::service::durable::tree::{NavigateRequest, Summarizer, parse_entry_id};

pub fn routes() -> Router<Api> {
    Router::new()
        .route("/v1/sessions/{id}/tree", get(tree))
        .route("/v1/sessions/{id}/navigate", post(navigate))
        .route("/v1/sessions/{id}/fork", post(fork))
        .route("/v1/sessions/{id}/context-edit", post(edit))
        .route("/v1/sessions/{id}/label", post(label))
        .route("/v1/sessions/{id}/name", post(name))
        .route("/v1/sessions/{id}/compact", post(compact))
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

/// 改树的操作之后的回执：重算 `leaf_id`，`editor_text` 是选中输入时的原文。
async fn receipt(
    api: &Api,
    session: &str,
    editor_text: Option<String>,
) -> ApiResult<Json<NavigateSessionResponse>> {
    Ok(Json(NavigateSessionResponse {
        leaf_id: projected(api, session).await?.leaf_id,
        editor_text,
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

/// `POST /v1/sessions/{id}/navigate`：只改位置（不追加 entry）；要摘要或标签才在目标处落分支。
async fn navigate(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<NavigateSessionRequest>,
) -> ApiResult<Json<NavigateSessionResponse>> {
    let core = api.state.core()?;
    let mut navigate = NavigateRequest::to(parse_entry_id(&request.target_id)?);
    if request.summarize {
        navigate.summarize = Some(Summarizer::new(
            api.state.llm.clone() as std::sync::Arc<dyn LlmClient>,
            api.state.snapshot().model.clone(),
        ));
    }
    navigate.instructions = request.instructions;
    navigate.label = request.label;
    let outcome = core.navigate(&id, navigate).await?;
    receipt(&api, &id, outcome.editor_text).await
}

/// `POST /v1/sessions/{id}/label`
async fn label(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<LabelSessionRequest>,
) -> ApiResult<Json<NavigateSessionResponse>> {
    let core = api.state.core()?;
    let target = parse_entry_id(&request.target_id)?;
    core.label(&id, target, request.label).await?;
    receipt(&api, &id, None).await
}

/// `POST /v1/sessions/{id}/name`
async fn name(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<NameSessionRequest>,
) -> ApiResult<Json<NavigateSessionResponse>> {
    let core = api.state.core()?;
    core.name(&id, &request.name).await?;
    receipt(&api, &id, None).await
}

/// `POST /v1/sessions/{id}/context-edit`：只改模型上下文，原始历史不动。
async fn edit(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<EditSessionContextRequest>,
) -> ApiResult<Json<NavigateSessionResponse>> {
    let core = api.state.core()?;
    let target = parse_entry_id(&request.target_id)?;
    core.context_edit(&id, target, request.replacement).await?;
    receipt(&api, &id, None).await
}

#[derive(serde::Deserialize)]
struct CompactRequest {
    instructions: Option<String>,
}

/// `POST /v1/sessions/{id}/compact`：`Harness::compact` 在当前分支上跑，等摘要落下。
async fn compact(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<CompactRequest>,
) -> ApiResult<Json<NavigateSessionResponse>> {
    let core = api.state.core()?;
    core.compact(&id, request.instructions).await?;
    receipt(&api, &id, None).await
}

/// `POST /v1/sessions/{id}/fork`：`/fork` 与 `/clone`，都**复制**成新会话。
async fn fork(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<ForkSessionRequest>,
) -> ApiResult<Json<ForkSessionResponse>> {
    let core = api.state.core()?;
    let target = request
        .target_id
        .as_deref()
        .map(parse_entry_id)
        .transpose()?;
    let record = core
        .fork_session(
            &id,
            ForkRequest {
                target,
                clone: request.clone,
            },
        )
        .await?;
    Ok(Json(ForkSessionResponse {
        session: SessionId::from_raw(record.id),
    }))
}
