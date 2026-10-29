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
    ConsolidateGroupStatus, ConsolidateOutcome, MemoryAddRequest, MemoryAddResponse,
    MemoryClassifyRequest, MemoryConsolidateRequest, MemoryConsolidateResponse, MemoryDetail,
    MemoryIndexStatus, MemoryListQuery, MemoryListResponse, MemoryRevisionRequest,
    MemorySweepRequest, MemorySweepResponse, RebuildIndexRequest, RebuildIndexResponse,
};
use komo_kernel::types::ids::{MemoryId, SessionId};
use komo_kernel::types::memory::{MemoryScope, MemoryState, scope_allowed};
use komo_runtime::memory::{BelongsChange, MemoryError};

use super::Api;
use super::error::{ApiFailure, ApiResult};
use super::idempotency::body_hash;
use crate::service::durable::hooks;

/// 不带查询词时一次最多列多少条。
const LIST_LIMIT: usize = 200;

/// `GET /v1/memories`：按作用域、状态和查询条件列出。
pub async fn list(
    State(api): State<Api>,
    Query(query): Query<MemoryListQuery>,
) -> ApiResult<Json<MemoryListResponse>> {
    let memories = &api.state.memories;
    let text = query.query.clone().unwrap_or_default();
    // 带会话 = Agent 在 Run 里自己查：只给这个会话放行的作用域（§9.4「Agent 主动查」）。
    let allowed = match &query.session {
        Some(session) => Some(session_scopes(&api, session).await?),
        None => None,
    };
    if let (Some(allowed), Some(scope)) = (&allowed, &query.scope)
        && !scope_allowed(allowed, scope)
    {
        return Err(ApiFailure::invalid(format!(
            "这个会话的 Agent 没有 {scope} 作用域的记忆权限"
        )));
    }

    if text.trim().is_empty() {
        // 库存清单。这条路不碰 embedding，所以没配向量模型也列得出来。
        let limit = query.limit.map(|n| n as usize).unwrap_or(LIST_LIMIT);
        // 已归档的默认不列（`docs/goals.md` §6）；明确问 `state = archived` 或带
        // `include_archived` 才有。要先筛再截，否则归档的占着名额、列出来的比 limit 少。
        let hide_archived = !query.include_archived && query.state.is_none();
        let mut items = memories
            .catalog()
            .list(
                query.scope.clone(),
                query.state,
                if hide_archived { usize::MAX } else { limit },
            )
            .await
            .map_err(ApiFailure::from)?;
        if hide_archived {
            items.retain(|item| item.state != MemoryState::Archived);
            items.truncate(limit);
        }
        if let Some(allowed) = &allowed {
            items.retain(|item| scope_allowed(allowed, &item.scope));
        }
        return Ok(Json(MemoryListResponse {
            memories: items,
            degraded: false,
            degraded_reason: None,
        }));
    }

    let mode = query.mode.unwrap_or(memories.retrieval().mode);
    let mut recall = memories.query(text, Some(mode));
    // 明确写了作用域就只查它；否则会话放行的那些（空 = 不限，但不含任何 `agent:*`，
    // `scope_allowed` 的口径）。
    recall.scopes = match (&query.scope, allowed) {
        (Some(scope), _) => vec![scope.clone()],
        (None, Some(allowed)) => allowed,
        (None, None) => Vec::new(),
    };
    if let Some(top_k) = query.limit {
        recall.top_k = top_k;
    }
    // 明确指定状态 = 明确 search：`contested` 暂停自动召回，但用户要查得到（§9.6）。
    if let Some(state) = query.state {
        recall.include_states = vec![state];
    } else if query.include_archived {
        // 默认的检索只给 active；归档的是另一个状态，要明说才带上。
        recall.include_states = vec![MemoryState::Active, MemoryState::Archived];
    }

    let result = memories.search(recall).await.map_err(memory_failure)?;
    Ok(Json(MemoryListResponse {
        memories: result.items,
        degraded: result.degraded,
        degraded_reason: result.degraded_reason,
    }))
}

/// 这个会话的 Profile 放行哪些记忆作用域：与自动召回同一处判断（`recall_scopes`）。
/// `memory = off` 的会话不能查。
async fn session_scopes(api: &Api, session: &SessionId) -> ApiResult<Vec<MemoryScope>> {
    let core = api.state.core()?;
    let record = core.session(session.as_str()).await?;
    let snapshot = api.state.snapshot();
    let resolved = hooks::resolve(&api.state, &*core.storage, record.current_branch, &snapshot)
        .await
        .map_err(|error| ApiFailure::invalid(error.to_string()))?;
    hooks::recall_scopes(&resolved).ok_or_else(|| {
        ApiFailure::invalid(format!(
            "这个会话的 Agent（{}）的记忆是关着的（memory = off），不能查记忆",
            resolved.agent_id
        ))
    })
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
/// 路由层已经判过。这不是对 Agent 的硬边界——bash 子进程读得到发现文件里的令牌（§7
/// 没有 Policy）；Agent 不替用户确认靠的是系统提示与 §9.6 的约定。
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

/// `POST /v1/memories`：手动记一条（§9.3「手动记一条」）。
///
/// 带 `session` + `run` 的是 Agent 在 Run 里替用户记的，记成未确认；都不带的是操作者
/// 自己记的，记成已确认（[`komo_runtime::memory::MemoryManager::add`]）。**后台任务会话不能替用户记**：那里
/// 的「用户输入」是主 Agent 写的任务正文，不是用户原话。记下之后像 restore 一样提交一次
/// 索引补齐。
pub async fn add(
    State(api): State<Api>,
    Json(request): Json<MemoryAddRequest>,
) -> ApiResult<Json<MemoryAddResponse>> {
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<MemoryAddResponse>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    if let Some(session) = &request.session
        && let Ok(record) = api.state.durable.session(session.as_str()).await
        && record.kind == komo_kernel::durable::SessionKind::Task
    {
        return Err(ApiFailure::invalid(format!(
            "会话 {session} 是后台任务：任务正文不是用户原话，不能替用户记；把要记的交回主会话"
        )));
    }
    // 归属指向的是操作者本人的事项：给别人用的 Agent（memory = private / off）的会话不能挂
    // （`docs/goals.md` §2，同事项接口的闸）。
    if request.belongs_to.is_some()
        && let Some(session) = &request.session
    {
        super::matters::gate(&api, session).await?;
    }
    let (memory, created) = api
        .state
        .memories
        .add(&request)
        .await
        .map_err(memory_failure)?;
    if api.state.memories.space().is_some()
        && let Err(error) = api.state.memories.schedule_index()
    {
        tracing::warn!(%error, memory = %memory.id, "手动记下之后补向量没提交上，先只靠关键词命中");
    }
    let response = MemoryAddResponse { memory, created };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &response);
    Ok(Json(response))
}

/// `POST /v1/memories/{id}/classify`：操作者给指定 revision 定使用方式与条件（§9.3「存量
/// 分类」）。与 confirm 同属操作者动作，边界同样靠系统提示与约定，不是硬边界（§9.6）。
/// 改了条件就改了索引输入：像 restore 一样提交一次索引补齐。
pub async fn classify(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<MemoryClassifyRequest>,
) -> ApiResult<Json<MemoryDetail>> {
    let memory = MemoryId::from_raw(id);
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<MemoryDetail>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let belongs = match (&request.belongs_to, request.clear_belongs_to) {
        (Some(_), true) => {
            return Err(ApiFailure::invalid(
                "belongs_to 与 clear_belongs_to 只能给一个：`--for` 要么是事项 ID，要么是 none",
            ));
        }
        (Some(matter), false) => BelongsChange::Set(matter.clone()),
        (None, true) => BelongsChange::Clear,
        (None, false) => BelongsChange::Keep,
    };
    if belongs != BelongsChange::Keep
        && let Some(session) = &request.session
    {
        super::matters::gate(&api, session).await?;
    }
    let item = api
        .state
        .memories
        .classify(
            &memory,
            request.expected_revision,
            request.application,
            request.applies_when.as_deref(),
            belongs,
        )
        .await
        .map_err(memory_failure)?;
    if api.state.memories.space().is_some()
        && let Err(error) = api.state.memories.schedule_index()
    {
        tracing::warn!(%error, memory = %memory, "分类之后补向量没提交上，先只靠关键词命中");
    }
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

/// `POST /v1/memories/{id}/restore`：撤回一次遗忘或争议，回到 active。
///
/// 向量在 forget 时删了：恢复之后顺手提交一次索引补齐，它只补缺的那些。没配向量就
/// 不提交——关键词索引已经由 `put` 重写，恢复本身不依赖它。
pub async fn restore(
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
        .restore(&memory, request.expected_revision)
        .await
        .map_err(memory_failure)?;
    if api.state.memories.space().is_some()
        && let Err(error) = api.state.memories.schedule_index()
    {
        tracing::warn!(%error, memory = %memory, "恢复之后补向量没提交上，先只靠关键词命中");
    }
    let detail = MemoryDetail { memory: item };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &detail);
    Ok(Json(detail))
}

/// `POST /v1/memory-sweep`：按规则遗忘；`apply = false` 只预演。
///
/// 不走幂等键：预演本来就无副作用，应用一次之后再来一次，第二次自然什么都找不到。
pub async fn sweep(
    State(api): State<Api>,
    Json(request): Json<MemorySweepRequest>,
) -> ApiResult<Json<MemorySweepResponse>> {
    Ok(Json(
        api.state
            .memories
            .sweep(&request)
            .await
            .map_err(memory_failure)?,
    ))
}

/// `POST /v1/memory-consolidate`：离线整理（§9.8）；`apply = false` 只预演。
///
/// 不走幂等键：预演无副作用，执行每组一个事务，重来一次只会看到上次盖过游标的组被跳过。
/// 执行写过东西之后发一次索引补齐信号（§9.5）。
pub async fn consolidate(
    State(api): State<Api>,
    Json(request): Json<MemoryConsolidateRequest>,
) -> ApiResult<Json<MemoryConsolidateResponse>> {
    let response = api
        .state
        .memories
        .consolidate(&request)
        .await
        .map_err(memory_failure)?;
    let wrote = response.applied
        && response.groups.iter().any(|group| {
            group.status == ConsolidateGroupStatus::Judged
                && group
                    .actions
                    .iter()
                    .any(|entry| entry.outcome != ConsolidateOutcome::Skipped)
        });
    if wrote
        && api.state.memories.space().is_some()
        && let Err(error) = api.state.memories.schedule_index()
    {
        tracing::warn!(%error, "离线整理之后补向量没提交上，先只靠关键词命中");
    }
    Ok(Json(response))
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
        MemoryError::Invalid(message) => ApiFailure::invalid(message),
        MemoryError::Repo(error) => ApiFailure::from(error),
        MemoryError::Store(error) => ApiFailure::from(error),
        other => ApiFailure::from(komo_kernel::traits::GatewayError::Internal(
            other.to_string(),
        )),
    }
}
