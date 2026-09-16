//! `/v1/memories` 与 `/v1/memory-index`（§13.1、§9）。
//!
//! 「confirm / forget 都返回**整条**记忆而不是一个 `{ok:true}`：它们带着预期 revision
//! 来，回去的那条才说得清现在是第几版。」

use axum::Json;
use axum::extract::{Path, Query, State};
use komo_kernel::protocol::http::{
    IndexState, MemoryDetail, MemoryIndexStatus, MemoryListQuery, MemoryListResponse,
    MemoryRevisionRequest, RebuildIndexRequest, RebuildIndexResponse,
};
use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::memory::{RecallQuery, RetrievalMode};

use super::Api;
use super::error::{ApiFailure, ApiResult};
use super::idempotency::body_hash;

/// `GET /v1/memories`：按作用域、状态和查询条件列出。
pub async fn list(
    State(api): State<Api>,
    Query(query): Query<MemoryListQuery>,
) -> ApiResult<Json<MemoryListResponse>> {
    let snapshot = api.state.snapshot();
    let mode = query.mode.unwrap_or(snapshot.memory.retrieval.mode);
    let recall = RecallQuery {
        text: query.query.clone().unwrap_or_default(),
        mode,
        scopes: query.scope.clone().into_iter().collect(),
        candidate_limit: snapshot.memory.retrieval.candidate_limit,
        top_k: query.limit.unwrap_or(snapshot.memory.retrieval.top_k),
        max_tokens: snapshot.memory.retrieval.max_tokens,
        now: api.state.clock.now(),
    };
    let result = api.state.memory.recall(&recall).await?;

    // 「向量服务不可用时 hybrid 明示降级，**vector-only 明确报不可用**」（§9.4）。
    if result.degraded && mode == RetrievalMode::Vector {
        return Err(ApiFailure::new(
            komo_kernel::protocol::http::ErrorCode::VectorUnavailable,
            result
                .degraded_reason
                .clone()
                .unwrap_or_else(|| "向量后端不可用".to_string()),
        ));
    }

    let mut memories = result.items;
    if let Some(state) = query.state {
        memories.retain(|item| item.state == state);
    }
    Ok(Json(MemoryListResponse {
        memories,
        degraded: result.degraded,
        degraded_reason: result.degraded_reason,
    }))
}

/// `GET /v1/memories/{id}`
pub async fn show(State(api): State<Api>, Path(id): Path<String>) -> ApiResult<Json<MemoryDetail>> {
    let memory = MemoryId::from_raw(id);
    api.state
        .memory
        .get(&memory)
        .await?
        .map(|item| Json(MemoryDetail { memory: item }))
        .ok_or_else(|| ApiFailure::not_found(format!("记忆 {memory}")))
}

/// `POST /v1/memories/{id}/confirm`：操作者确认指定 revision。
pub async fn confirm(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<MemoryRevisionRequest>,
) -> ApiResult<Json<MemoryDetail>> {
    let memory = MemoryId::from_raw(id);
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<MemoryDetail>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let item = api
        .state
        .memory
        .confirm(&memory, request.expected_revision, api.state.clock.now())
        .await?;
    let detail = MemoryDetail { memory: item };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &detail);
    Ok(Json(detail))
}

/// `POST /v1/memories/{id}/forget`：停用指定 revision 并失效索引。
pub async fn forget(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<MemoryRevisionRequest>,
) -> ApiResult<Json<MemoryDetail>> {
    let memory = MemoryId::from_raw(id);
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<MemoryDetail>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let item = api
        .state
        .memory
        .forget(&memory, request.expected_revision, api.state.clock.now())
        .await?;
    let detail = MemoryDetail { memory: item };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &detail);
    Ok(Json(detail))
}

/// `GET /v1/memory-index`：当前空间、进度、覆盖率和错误。
pub async fn index(State(api): State<Api>) -> ApiResult<Json<MemoryIndexStatus>> {
    Ok(Json(api.state.memory.index_status().await?))
}

/// `POST /v1/memory-index/rebuild`：幂等提交重建任务。
///
// TODO(decide: 重建的**执行**是 MemoryManager 的事（§9.5 的空间指纹与索引代次），而
// `komo-runtime` 的 `memory` 模块在 W4 时只有一行模块注释。所以这里只答"当前代次是
// 什么、这次没有新提交"，并在没有配置向量模型时明确拒绝——**不假装已经开始重建**。
// 见报告"需要编排者做"。)
pub async fn rebuild(
    State(api): State<Api>,
    Json(request): Json<RebuildIndexRequest>,
) -> ApiResult<Json<RebuildIndexResponse>> {
    let status = api.state.memory.index_status().await?;
    if status.state == IndexState::Unconfigured {
        return Err(ApiFailure::config_invalid(
            "没有配置向量模型（[memory.embedding]），没有可重建的索引",
            vec![komo_kernel::protocol::config::KeyPath::new(
                "memory.embedding",
            )],
        ));
    }
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<RebuildIndexResponse>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let response = RebuildIndexResponse {
        generation: status.generation.clone().unwrap_or_default(),
        accepted: false,
    };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &response);
    Ok(Json(response))
}
