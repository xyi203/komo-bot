//! `/v1/memories` 与 `/v1/memory-index`（§13.1、§9）。
//!
//! 「confirm / forget 都返回**整条**记忆而不是一个 `{ok:true}`：它们带着预期 revision
//! 来，回去的那条才说得清现在是第几版。」
//!
//! `GET /v1/memories` 有**两种问法**，答法不同，这不是一个可以合并的分支：
//!
//! - 带 `query` = 检索。走 [`MemoryManager::search`]，降级如实报，`vector` 模式不通就是
//!   `VectorUnavailable` 而不是空集（§9.4）。
//! - 不带 `query` = 列库存。走目录，按作用域 / 状态筛。一个空查询在检索里应当返回空
//!   （§9.4「没有足够相关内容时返回空」），而 `komo memory list` 要的显然不是空。

use axum::Json;
use axum::extract::{Path, Query, State};
use komo_kernel::protocol::http::{
    MemoryDetail, MemoryIndexStatus, MemoryListQuery, MemoryListResponse, MemoryRevisionRequest,
    RebuildIndexRequest, RebuildIndexResponse,
};
use komo_kernel::types::ids::MemoryId;
use komo_runtime::memory::MemoryError;

use super::Api;
use super::error::{ApiFailure, ApiResult};
use super::idempotency::body_hash;

/// 不带查询词时一次最多列多少条。
const LIST_LIMIT: usize = 200;

/// `GET /v1/memories`：按作用域、状态和查询条件列出。
pub async fn list(
    State(api): State<Api>,
    Query(query): Query<MemoryListQuery>,
) -> ApiResult<Json<MemoryListResponse>> {
    let memories = &api.state.memories;
    let text = query.query.clone().unwrap_or_default();

    if text.trim().is_empty() {
        // 库存清单。这条路不碰 embedding，所以没配向量模型也列得出来。
        let items = memories
            .catalog()
            .list(
                query.scope.clone(),
                query.state,
                query.limit.map(|n| n as usize).unwrap_or(LIST_LIMIT),
            )
            .await
            .map_err(ApiFailure::from)?;
        return Ok(Json(MemoryListResponse {
            memories: items,
            degraded: false,
            degraded_reason: None,
        }));
    }

    let mode = query.mode.unwrap_or(memories.retrieval().mode);
    let mut recall = memories.query(text, Some(mode));
    recall.scopes = query.scope.clone().into_iter().collect();
    if let Some(top_k) = query.limit {
        recall.top_k = top_k;
    }
    // 明确指定状态 = 明确 search：`contested` 暂停自动召回，但用户要查得到（§9.6）。
    if let Some(state) = query.state {
        recall.include_states = vec![state];
    }

    let result = memories.search(recall).await.map_err(memory_failure)?;
    Ok(Json(MemoryListResponse {
        memories: result.items,
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
///
/// **这条路是"受信任的操作者交互"那一侧**（§9.6）：`require_loopback` 与 bearer 在
/// 路由层已经判过，Agent 通过 shell 调管理命令走不到这里——它没有令牌。
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
        .memories
        .confirm(&memory, request.expected_revision)
        .await
        .map_err(memory_failure)?;
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
        .memories
        .forget(&memory, request.expected_revision)
        .await
        .map_err(memory_failure)?;
    let detail = MemoryDetail { memory: item };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &detail);
    Ok(Json(detail))
}

/// `GET /v1/memory-index`：当前空间、进度、覆盖率和错误。
///
/// 配置换了向量模型时 `errors` 里会多一条"指纹对不上，需要一次重建"——**热重载不偷偷
/// 重建**（§9.5、§13.3），它只说出来。
pub async fn index(State(api): State<Api>) -> ApiResult<Json<MemoryIndexStatus>> {
    Ok(Json(
        api.state
            .memories
            .index_status()
            .await
            .map_err(memory_failure)?,
    ))
}

/// `POST /v1/memory-index/rebuild`：幂等提交重建任务。
///
/// 真的起一个后台 [`IndexBuilder`]，进度由 `GET /v1/memory-index` 查。同一个代次已经在
/// 跑时返回 `accepted = false` 并给回**同一个**代次号（§13.1 的幂等提交）。
pub async fn rebuild(
    State(api): State<Api>,
    Json(request): Json<RebuildIndexRequest>,
) -> ApiResult<Json<RebuildIndexResponse>> {
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<RebuildIndexResponse>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let (generation, accepted) = api
        .state
        .memories
        .request_rebuild()
        .map_err(memory_failure)?;
    let response = RebuildIndexResponse {
        generation,
        accepted,
    };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &response);
    Ok(Json(response))
}

/// 记忆这一层的失败怎么变成 HTTP。
///
/// **`VectorUnconfigured` 是配置错误，`VectorUnavailable` 是后端错误**——两者分开报，
/// 因为处理方式不同：前者去改 config.toml，后者去看端点。两者都不是"没有相关记忆"
/// （§9.4）。
fn memory_failure(error: MemoryError) -> ApiFailure {
    match error {
        MemoryError::Disabled => ApiFailure::config_invalid(
            "`[memory] enabled = false`：这台 Gateway 没有开启自动记忆",
            vec![komo_kernel::protocol::config::KeyPath::new(
                "memory.enabled",
            )],
        ),
        MemoryError::VectorUnconfigured => ApiFailure::config_invalid(
            "检索模式要向量，但没有配置 `memory.embedding` alias：要么补上，要么把 \
             memory.retrieval.mode 明确设成 \"keyword\"",
            vec![komo_kernel::protocol::config::KeyPath::new(
                "memory.embedding",
            )],
        ),
        MemoryError::VectorUnavailable(reason) => ApiFailure::new(
            komo_kernel::protocol::http::ErrorCode::VectorUnavailable,
            reason,
        ),
        MemoryError::Repo(error) => ApiFailure::from(error),
        MemoryError::Store(error) => ApiFailure::from(error),
        other => ApiFailure::from(komo_kernel::traits::GatewayError::Internal(
            other.to_string(),
        )),
    }
}
