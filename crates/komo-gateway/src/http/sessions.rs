//! `/v1/sessions`（§13.1、§8.10）。

use axum::Json;
use axum::extract::{Path, State};
use komo_kernel::fold::fold;
use komo_kernel::protocol::http::{
    BoundaryRequest, BoundaryResponse, CreateSessionRequest, DeleteSessionRequest,
    InterventionListQuery, PurgeSessionRequest, PurgeSessionResponse, ResumeRequest,
    ResumeResponse, SessionDetail, SessionListResponse, SessionSummary, SubmitRunRequest,
    SubmitRunResponse,
};
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::model::{Effort, ModelConfig};
use komo_kernel::types::status::{RunState, WaitReason};

use super::error::{ApiFailure, ApiResult};
use super::idempotency::body_hash;
use super::{Api, created_at, events_of};

/// `GET /v1/sessions`
///
/// 默认只列 `active` 与 `closing`（`closing` 要列出来并标注"正在关闭"，它还在服务）；
/// `?all=1` 把 `deleted` 也带上。**`purged` 两种都不列**——那一行只是"这个会话曾经存在"
/// 的记录，只在显式查看单个会话时可见（§8.10）。
///
/// query 在这里**自己认** `all`，不走 `Query<SessionListQuery>`：那个类型是 kernel 的
/// `bool`，而 `serde_urlencoded` 只认 `true` / `false`——§13.1 写的是 `?all=1`，照类型解
/// 会得到 400。`1` / `true` / `yes` 与光秃秃的 `?all` 都算真，别的都算假。
pub async fn list(
    State(api): State<Api>,
    uri: axum::http::Uri,
) -> ApiResult<Json<SessionListResponse>> {
    let all = flag(uri.query().unwrap_or_default(), "all");
    let records = komo_store::repos::session::list(&api.state.db, all).await?;
    let mut sessions = Vec::with_capacity(records.len());
    for record in records {
        sessions.push(summary_of(&api, &record.session).await?);
    }
    Ok(Json(SessionListResponse { sessions }))
}

/// query 里那个开关开着吗。`?all`、`?all=1`、`?all=true`、`?all=yes` 都算开。
fn flag(query: &str, name: &str) -> bool {
    query.split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        key == name
            && matches!(
                value.to_ascii_lowercase().as_str(),
                "" | "1" | "true" | "yes"
            )
    })
}

/// `POST /v1/sessions`
pub async fn create(
    State(api): State<Api>,
    Json(request): Json<CreateSessionRequest>,
) -> ApiResult<Json<SessionSummary>> {
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<SessionSummary>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }

    let session = SessionId::new_at(api.state.clock.now());
    api.state.ledgers.open(&session, "agent").await?;
    if let Some(workdir) = &request.workdir {
        api.state.remember_workdir(&session, workdir);
    }
    let summary = summary_of(&api, &session).await?;
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &summary);
    Ok(Json(summary))
}

/// `GET /v1/sessions/{id}`
///
/// `unfinished` 逐条带 `state` 与 `wait`（§8.4 的两维），`pending` 是这个会话上**待处理的
/// Intervention**（§7.5 的**三类**）——不是只列审批。少了后面这一条，"卡住但清单为空"
/// 会从详情这个入口重新长出来。
pub async fn detail(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionDetail>> {
    let session = SessionId::from_raw(id);
    let summary = summary_of(&api, &session).await?;
    let unfinished = unfinished_of(&api, &session).await?;
    let pending = api
        .state
        .interventions(&InterventionListQuery {
            session: Some(session.clone()),
            ..Default::default()
        })
        .await?;
    Ok(Json(SessionDetail {
        summary,
        unfinished,
        pending,
    }))
}

/// `POST /v1/sessions/{id}/runs`：提交新输入。
///
/// **已逻辑删除的会话拒绝新输入**（§8.10）：`closing` / `deleted` / `purged` 一律 409，
/// 并且说清楚是**哪一种**——操作者下一步该做的事三种状态各不相同。
pub async fn submit(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<SubmitRunRequest>,
) -> ApiResult<Json<SubmitRunResponse>> {
    let session = SessionId::from_raw(id);
    if let Some(state) = api.state.input_refusal(&session).await? {
        return Err(ApiFailure::session_not_open(state));
    }
    let model = model_override(&api, request.model.as_deref(), request.effort.as_ref())?;
    // 幂等由账本自己答（§8.5）：同一请求键返回原 Run，内容不同则 409。
    let response = api
        .state
        .submit(
            &session,
            request.request_key.clone(),
            request.text.clone(),
            None,
            model,
        )
        .await?;
    Ok(Json(response))
}

/// `POST /v1/sessions/{id}/resume`：检查恢复位置，恢复可继续的运行或返回待处理状态。
///
/// **CLI 不承担恢复调度**（§8.8）：启动与每拍的兜底都对账过了，这里只是"现在到哪了"，
/// 顺手把**现在就能跑**的放回队列。所以：
///
/// - `accepted` / `queued` / 没主人的 `running` → 叫醒调度器，列进 `resumed`；
/// - 停在人身上的（`approval` / `intervention`）→ **不叫醒**，它们在 `pending` 里，
///   等操作者答复（§7.5）；
/// - 等时钟（`retry`）与等前一条 Run（`dependency`）→ 也**不叫醒**：它们不是在等人，
///   但也没有"现在就继续"这回事——到点由对账放回队列（§8.9），强行 requeue 前者会绕过
///   退避、后者会越过前面那条 Run。
pub async fn resume(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<ResumeRequest>,
) -> ApiResult<Json<ResumeResponse>> {
    let session = SessionId::from_raw(id);
    let _ = request; // 模型覆盖属于下一次提交，不改已经固定在 Run 上的那一份快照。

    let mut resumed = Vec::new();
    for run in komo_store::repos::runs::list_for_session(&api.state.db, &session).await? {
        if run.state.is_terminal() {
            continue;
        }
        let waiting_on_condition = matches!(
            run.wait,
            Some(WaitReason::Approval { .. })
                | Some(WaitReason::Intervention { .. })
                | Some(WaitReason::Retry { .. })
                | Some(WaitReason::Dependency { .. })
        );
        if waiting_on_condition {
            continue;
        }
        api.state.wake_run(&run.run).await?;
        resumed.push(super::runs::summary_of(&run));
    }
    let pending = api
        .state
        .interventions(&InterventionListQuery {
            session: Some(session.clone()),
            ..Default::default()
        })
        .await?;
    Ok(Json(ResumeResponse {
        session,
        resumed,
        pending,
    }))
}

/// `POST /v1/sessions/{id}/boundary`：`/new`，**不切 Session**。
pub async fn boundary(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(_request): Json<BoundaryRequest>,
) -> ApiResult<Json<BoundaryResponse>> {
    let session = SessionId::from_raw(id);
    let seq = api.state.routed_boundary(&session).await?;
    Ok(Json(BoundaryResponse { session, seq }))
}

/// `POST /v1/sessions/{id}/delete`（§8.10）。
///
/// 逻辑删除：进 `closing`。`{"now":true}` 立刻把未完成的 Run 各写一条取消再进 `deleted`。
/// **不碰内容**——内容只有 `purge` 才回收，而且必须有人明确下令。
pub async fn delete(
    State(api): State<Api>,
    Path(id): Path<String>,
    body: Option<Json<DeleteSessionRequest>>,
) -> ApiResult<Json<komo_kernel::protocol::http::SessionLifecycleResponse>> {
    let session = SessionId::from_raw(id);
    let request = body.map(|Json(request)| request).unwrap_or_default();
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<komo_kernel::protocol::http::SessionLifecycleResponse>(
            request.request_key.as_ref(),
            &hash,
        )?
    {
        return Ok(Json(previous));
    }
    let response = api.state.delete_session(&session, request.now).await?;
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &response);
    Ok(Json(response))
}

/// `POST /v1/sessions/{id}/purge`（§8.10 第 3 条）。
///
/// 先跑引用检查：不过就 **409 + `PurgeBlocked`**（列出要先处置什么，不假装成功）；过了就
/// **先提交 `purged` 墓碑、再删内容**，最后对账把"标了 `purged` 而内容还在"的会话收尾。
/// 幂等：再跑一次得到同一个结果，`removed_bytes` 是 0。
pub async fn purge(
    State(api): State<Api>,
    Path(id): Path<String>,
    body: Option<Json<PurgeSessionRequest>>,
) -> ApiResult<Json<PurgeSessionResponse>> {
    let session = SessionId::from_raw(id);
    let request = body.map(|Json(request)| request).unwrap_or_default();
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<PurgeSessionResponse>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    match api.state.purge_session(&session).await? {
        Ok(response) => {
            api.idempotency
                .remember(request.request_key.as_ref(), &hash, &response);
            Ok(Json(response))
        }
        Err(blockers) => Err(ApiFailure::purge_blocked(
            komo_kernel::protocol::http::PurgeBlocked {
                session: session.clone(),
                blockers,
            },
        )),
    }
}

/// 一个 Session 的概览。
///
/// 两格是这次改造的验收口径落在界面上的样子：`current_state` 答"能不能跑"，
/// `current_wait` 答"为什么不能跑"（§8.4）。少了后者，"排队二十分钟"就只是一句状态。
pub async fn summary_of(api: &Api, session: &SessionId) -> Result<SessionSummary, ApiFailure> {
    let record = api
        .state
        .session_record(session)
        .await
        .map_err(ApiFailure::from)?;
    let runs = komo_store::repos::runs::list_for_session(&api.state.db, session).await?;
    // **子代理不算这条会话的"当前 Run"**（§4）：父 Run 才是操作者发起的那个，而它此刻正
    // 在等它的子 Run。取最后一条非终态的**顶层** Run——否则界面上显示的是子代理，操作者
    // 看到"会话卡住了"，却看不到自己交给 komo 的那件事。
    let current = runs
        .iter()
        .rev()
        .find(|run| run.state.is_unfinished() && run.parent.is_none());
    let now = api.state.clock.now();
    let created = created_at(session.as_str(), now);
    Ok(SessionSummary {
        session: session.clone(),
        title: record.title.clone(),
        state: record.state,
        workdir: record
            .workdir
            .clone()
            .or_else(|| api.state.workdir_of(session)),
        current_run: current.map(|run| run.run.clone()),
        current_state: current.map(|run| run.state),
        // 只在 `waiting` 时有值：`queued` 的 Run "一定说得出有活干"，它没有"在等什么"。
        current_wait: current.and_then(|run| {
            (run.state == RunState::Waiting)
                .then(|| run.wait.clone())
                .flatten()
        }),
        applied_seq: record.applied_seq,
        created_at: created,
        // 最后一条事件的时刻；没有事件就是创建时刻。
        updated_at: last_event_at(api, session).await.unwrap_or(created),
    })
}

/// 这个会话没进终态的 Run。
async fn unfinished_of(
    api: &Api,
    session: &SessionId,
) -> Result<Vec<komo_kernel::protocol::http::RunSummary>, ApiFailure> {
    Ok(
        komo_store::repos::runs::list_for_session(&api.state.db, session)
            .await?
            .iter()
            .filter(|run| run.state.is_unfinished())
            .map(super::runs::summary_of)
            .collect(),
    )
}

async fn last_event_at(api: &Api, session: &SessionId) -> Option<time::OffsetDateTime> {
    let events = events_of(api, session).await.ok()?;
    fold(&events).last_at
}

/// 本次 Run 的模型覆盖。
///
/// **不支持的 effort 在请求前拒绝**（§13.3）——不是发出去让服务端 400，更不是悄悄
/// 删掉这个参数再试一次。
fn model_override(
    api: &Api,
    model: Option<&str>,
    effort: Option<&Effort>,
) -> Result<Option<ModelConfig>, ApiFailure> {
    if model.is_none() && effort.is_none() {
        return Ok(None);
    }
    let snapshot = api.state.snapshot();
    let mut config = match model {
        Some(alias) => super::config::completion_model(api, alias)?,
        None => snapshot.model.clone(),
    };
    if let Some(effort) = effort {
        config.effort = Some(effort.clone());
    }
    if let Err(problem) = api.state.caps.check(&config) {
        return Err(ApiFailure::invalid(problem.to_string()));
    }
    Ok(Some(config))
}
