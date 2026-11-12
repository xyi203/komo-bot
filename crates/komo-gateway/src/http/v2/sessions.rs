//! Durable 模式的 `/v1/sessions`（`docs/storage-durable-d5.md` §4 W2a）：create / list / detail /
//! submit / boundary / resume，加 `/v1/home-session`。
//!
//! delete / purge（W4a，`service::durable::lifecycle`）；`/v1/reconcile` 在 `interventions`；事件流是 W2b 的。
//!
//! 状态码映射在 `http::error`（`SessionError`）：`NotFound` 404、`Invalid` 400、`NotOpen` /
//! `Busy` / `Conflict` 409（`NotOpen` 说清是哪种状态）、请求键冲突 409、`Internal` 500。

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{StreamExt, TryStreamExt};
use komo_kernel::durable::legacy_ids;
use komo_kernel::protocol::http::{
    BoundaryRequest, BoundaryResponse, CreateSessionRequest, DeleteSessionRequest, PurgeBlocked,
    PurgeSessionRequest, PurgeSessionResponse, ResumeRequest, ResumeResponse, SessionDetail,
    SessionLifecycleResponse, SessionListResponse, SessionSearchQuery, SessionSearchResponse,
    SessionSummary, SubmitRunRequest, SubmitRunResponse,
};
use komo_kernel::types::ids::{RunId, Seq, SessionId};
use komo_kernel::types::model::Effort;
use komo_kernel::types::status::{RunState, SessionState};
use komo_runtime::durable::harness::WhenBusy;

use crate::http::Api;
use crate::http::error::{ApiFailure, ApiResult};
use crate::http::idempotency::body_hash;
use crate::service::durable::DurableCore;
use crate::service::durable::hooks;
use crate::service::durable::sessions::lifecycle;
use crate::service::durable::sessions::{NewSession, SessionError, SessionPageCursor};
use crate::service::durable::submit::shell;
use crate::service::durable::submit::{Accepted, InputRequest, ModelOverride};

pub fn routes() -> Router<Api> {
    Router::new()
        .route("/v1/home-session", get(home))
        .route("/v1/sessions", get(list).post(create))
        .route("/v1/session-search", get(search))
        .route("/v1/sessions/{id}", get(detail))
        .route("/v1/sessions/{id}/runs", post(submit))
        .route("/v1/sessions/{id}/resume", post(resume))
        .route("/v1/sessions/{id}/boundary", post(boundary))
        .route("/v1/sessions/{id}/delete", post(delete))
        .route("/v1/sessions/{id}/purge", post(purge))
}

/// query 里那个开关开着吗。`?all`、`?all=1`、`?all=true`、`?all=yes` 都算开（
/// §13.1 写的是 `?all=1`，而 `serde_urlencoded` 的 bool 只认 `true` / `false`）。
pub(super) fn flag(query: &str, name: &str) -> bool {
    query.split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        key == name
            && matches!(
                value.to_ascii_lowercase().as_str(),
                "" | "1" | "true" | "yes"
            )
    })
}

/// 会话列表并发读概览的上限。
const SUMMARY_READS: usize = 2;

/// 会话列表一页的上限：再大就按它给。
pub(super) const MAX_PAGE: u32 = 500;

/// `GET /v1/sessions` 的分页参数（`all` 走 [`flag`]，这里只认这两个，其余 serde 忽略）。
#[derive(serde::Deserialize)]
pub(crate) struct ListPaging {
    pub(crate) limit: Option<u32>,
    pub(crate) cursor: Option<String>,
}

/// `GET /v1/sessions`：默认只列 `active` 与 `closing`；`?all=1` 带上 `deleted`；`purged` 不列。
///
/// 分页只做加法：**不带 `limit` 就是全部**（与分页之前逐字节相同，老客户端不被截断，`cursor` 被忽略）；
/// 带 `limit` 按页给，还有下一页时回 `next_cursor`。概览（live / inbox / 末条 entry）只读当页的。
pub(crate) async fn list(
    State(api): State<Api>,
    uri: axum::http::Uri,
    Query(paging): Query<ListPaging>,
) -> ApiResult<Json<SessionListResponse>> {
    let core = api.state.core()?;
    let all = flag(uri.query().unwrap_or_default(), "all");
    let (records, next_cursor) = match paging.limit {
        None => (core.list_sessions(all).await?, None),
        Some(0) => return Err(SessionError::Invalid("limit 至少是 1".into()).into()),
        Some(limit) => {
            let after = paging
                .cursor
                .as_deref()
                .map(SessionPageCursor::decode)
                .transpose()?;
            let page = core
                .list_sessions_page(all, limit.min(MAX_PAGE) as usize, after.as_ref())
                .await?;
            (page.records, page.next.map(|next| next.encode()))
        }
    };
    // 每条概览是几次点查（live / inbox / 末条 entry）：有界并发、保持次序。上限取读连接池
    // （`TursoStorage` 4 条）的一半，与 feed 补读同一个口径，给提交线留出空连接。
    let sessions = futures_util::stream::iter(records)
        .map(|record| {
            let core = core.clone();
            async move { core.session_summary(&record).await }
        })
        .buffered(SUMMARY_READS)
        .try_collect()
        .await?;
    Ok(Json(SessionListResponse {
        sessions,
        next_cursor,
    }))
}

/// `GET /v1/session-search?q=…&limit=…`：跨会话找回（`komo session search`，roadmap-muse S3）。
/// 只读；可见性与次序见 `service::durable::search`。空关键词 400。
pub(crate) async fn search(
    State(api): State<Api>,
    Query(query): Query<SessionSearchQuery>,
) -> ApiResult<Json<SessionSearchResponse>> {
    let core = api.state.core()?;
    let config = api.state.snapshot();
    let hits = core
        .search_sessions(
            &query.q,
            query.limit.map(|limit| limit as usize),
            &config.agent,
        )
        .await?;
    Ok(Json(SessionSearchResponse { hits }))
}

/// `POST /v1/sessions`：建会话（sessions 行 + 根 conversation + `komo.agent`，一次提交），落在默认
/// Agent 名下。请求键和正文哈希随会话持久化。
async fn create(
    State(api): State<Api>,
    Json(request): Json<CreateSessionRequest>,
) -> ApiResult<Json<SessionSummary>> {
    let hash = body_hash(&request);
    let core = api.state.core()?;
    let mut spec = NewSession::new(api.state.snapshot().agent.default_agent.clone());
    if let Some(title) = &request.title {
        spec = spec.title(title);
    }
    if let Some(workdir) = &request.workdir {
        spec = spec.workdir(workdir);
    }
    let key = request
        .request_key
        .as_ref()
        .map(|key| key.as_str().to_owned());
    let (record, _) = core
        .create_session_idempotent(spec, key, hash.to_string())
        .await?;
    Ok(Json(core.session_summary(&record).await?))
}

/// `GET /v1/home-session`：默认 Agent 的主会话（没有就建）。
async fn home(State(api): State<Api>) -> ApiResult<Json<SessionSummary>> {
    let core = api.state.core()?;
    let agent = api.state.snapshot().agent.default_agent.clone();
    let (record, _) = core.main_session(&agent).await?;
    Ok(Json(core.session_summary(&record).await?))
}

/// `GET /v1/sessions/{id}`
///
/// `unfinished` 是没结束的 run（逐条带 `state` 与 `wait`）。`pending`（待处理的 Intervention）
/// W4a 接上：新存储里它是 faulted 的任务，现在先空着。
async fn detail(State(api): State<Api>, Path(id): Path<String>) -> ApiResult<Json<SessionDetail>> {
    let core = api.state.core()?;
    let record = core.session(&id).await?;
    let summary = core.session_summary(&record).await?;
    let unfinished = core.unfinished_runs(&id).await?;
    Ok(Json(SessionDetail {
        summary,
        unfinished,
        pending: core.pending_of(&id).await?,
    }))
}

/// 本次提交的模型 / effort 覆盖，按**当前**配置解出 provider 与模型名（`model.change` 展示用）。
///
/// **不支持的 effort 在请求前拒绝**（§13.3）。只给 effort 时按这个会话此刻会用的模型校验
/// （当前分支的 sticky 模型，没有就是 Agent 的默认），不是全局主模型。
async fn model_override(
    api: &Api,
    core: &DurableCore,
    session: &str,
    model: Option<&str>,
    effort: Option<&Effort>,
) -> Result<Option<ModelOverride>, ApiFailure> {
    if model.is_none() && effort.is_none() {
        return Ok(None);
    }
    let mut config = match model {
        Some(alias) => crate::http::config::completion_model(api, alias)?,
        None => {
            let record = core.session(session).await?;
            let snapshot = api.state.snapshot();
            hooks::resolve(&api.state, &*core.storage, record.current_branch, &snapshot)
                .await
                .map_err(|error| ApiFailure::invalid(error.to_string()))?
                .model
        }
    };
    if let Some(effort) = effort {
        config.effort = Some(effort.clone());
    }
    if let Err(problem) = api.state.caps.check(&config) {
        return Err(ApiFailure::invalid(problem.to_string()));
    }
    Ok(Some(ModelOverride {
        alias: model.map(str::to_owned),
        provider: Some(config.provider.clone()),
        model_id: Some(config.model.clone()),
        effort: effort.map(|effort| effort.as_str().to_owned()),
    }))
}

/// 提交回执：旧协议的形状（`seq` = 受理那一帧 `RunAccepted` 的 `Seq`）。
async fn receipt(core: &DurableCore, accepted: Accepted) -> ApiResult<Json<SubmitRunResponse>> {
    let state = core.submission_state(accepted.submission).await?;
    Ok(Json(SubmitRunResponse {
        run: accepted.run,
        session: SessionId::from_raw(accepted.session),
        seq: legacy_ids::seq(accepted.submission, 0),
        state,
        deduplicated: accepted.deduplicated,
        detached: false,
    }))
}

/// `POST /v1/sessions/{id}/runs`：提交新输入。
///
/// 已逻辑删除的会话拒绝新输入（409，说清是 `closing` / `deleted` / `purged` 哪一种）。`shell` =
/// TUI 的 `!`：不经模型直跑（决定 9）。`interject` = TUI 的 Enter：忙时 `WhenBusy::Steer`，
/// 没轮上就照常排队，回执与普通提交一样。同一请求键重发返回原 run，内容不同 409。
async fn submit(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<SubmitRunRequest>,
) -> ApiResult<Json<SubmitRunResponse>> {
    let core = api.state.core()?;
    if let Some(state) = core.input_refusal(&id).await? {
        return Err(ApiFailure::session_not_open(state));
    }
    if request.shell {
        let receipt_of = shell::submit_shell(
            &api.state,
            &id,
            Some(request.request_key.as_str().to_owned()),
            &request.text,
        )
        .await?;
        return match receipt_of {
            shell::ShellReceipt::Landed(accepted) => receipt(core, accepted).await,
            // 转后台：还没有 submission，`run` 是这条命令的临时身份。
            shell::ShellReceipt::Detached { call, deduplicated } => Ok(Json(SubmitRunResponse {
                run: RunId::from_raw(call),
                session: SessionId::from_raw(id),
                seq: Seq::ZERO,
                state: RunState::Running,
                deduplicated,
                detached: true,
            })),
        };
    }
    // 主会话不起模型轮次：消息落成主线上的展示消息，由分发任务判断去向（`docs/tasks.md` §4.11）。
    // 回话只落主线（HTTP 来的没有渠道对端）；`model` / `interject` 对没有轮次的主会话没有意义。
    if core.session(&id).await?.kind == komo_kernel::durable::SessionKind::Main {
        let accepted = core
            .dispatch_message(
                &id,
                crate::service::durable::dispatch::MainMessage {
                    text: request.text,
                    request_id: request.request_key.as_str().to_owned(),
                    peer: None,
                    principal: None,
                },
            )
            .await?;
        return receipt(core, accepted).await;
    }
    let model = model_override(
        &api,
        core,
        &id,
        request.model.as_deref(),
        request.effort.as_ref(),
    )
    .await?;
    let mut input = InputRequest::new(request.text).request_id(request.request_key.as_str());
    if request.interject {
        // 插话：忙时并进当前 run，并举旗让前台正跑着的长 `bash` 提前转后台（W2c）。
        input = input.when_busy(WhenBusy::Steer).interject(true);
    }
    if let Some(model) = model {
        input = input.model(model);
    }
    // W3a：先挂看客再提交（标题、聊天来源的内部输入回复都靠它）。
    crate::service::durable::background::watch::ensure(&api.state, &id);
    let accepted = core.submit_input(&id, input).await?;
    receipt(core, accepted).await
}

/// `POST /v1/sessions/{id}/resume`：现在到哪了。D5 里 run 的续跑由引擎自己做（启动与每拍的恢复），
/// 这里只返回还没结束的 run（并叫醒调度）与待处理的 Intervention（faulted 任务）。
async fn resume(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<ResumeRequest>,
) -> ApiResult<Json<ResumeResponse>> {
    // 模型覆盖属于下一次提交，不改已经在跑的那一轮。
    let _ = request;
    let core = api.state.core()?;
    let resumed = core.unfinished_runs(&id).await?;
    core.engine.resume();
    let pending = core.pending_of(&id).await?;
    Ok(Json(ResumeResponse {
        session: SessionId::from_raw(id),
        resumed,
        pending,
    }))
}

/// `POST /v1/sessions/{id}/boundary`：`/new`，**不切 Session**（`Harness::reset`：当前分支追加
/// `komo.reset`，忙时排在收件箱里，在下一个 boundary 落）。
async fn boundary(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(_request): Json<BoundaryRequest>,
) -> ApiResult<Json<BoundaryResponse>> {
    let core = api.state.core()?;
    let submission = core.boundary(&id, None).await?;
    Ok(Json(BoundaryResponse {
        session: SessionId::from_raw(id),
        seq: legacy_ids::seq(submission, 0),
    }))
}

/// `POST /v1/sessions/{id}/delete`（§8.10）：`closing` → 撤回排队输入（含内部输入）→（`now`）abort →
/// `deleted`。已经 `deleted` / `purged` 的再删是幂等的。
async fn delete(
    State(api): State<Api>,
    Path(id): Path<String>,
    body: Option<Json<DeleteSessionRequest>>,
) -> ApiResult<Json<SessionLifecycleResponse>> {
    let request = body.map(|Json(request)| request).unwrap_or_default();
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<SessionLifecycleResponse>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let core = api.state.core()?;
    let outcome = core.delete_session(&id, request.now).await?;
    let response = SessionLifecycleResponse {
        session: SessionId::from_raw(id),
        state: crate::service::durable::sessions::state_of(outcome.lifecycle),
        changed_at: crate::service::durable::sessions::time_of(outcome.changed_at),
        cancelled: outcome.cancelled,
    };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &response);
    Ok(Json(response))
}

/// `POST /v1/sessions/{id}/purge`（§8.10 第 3 条）：要求已 `deleted`；有没做完的工作（或记忆证据、
/// 未送出的投递、cron 触发）就 409 并列出；否则一次提交 `PurgeConversations` + 墓碑，之后删目录。幂等。
async fn purge(
    State(api): State<Api>,
    Path(id): Path<String>,
    body: Option<Json<PurgeSessionRequest>>,
) -> ApiResult<Json<PurgeSessionResponse>> {
    let request = body.map(|Json(request)| request).unwrap_or_default();
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<PurgeSessionResponse>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let session = SessionId::from_raw(id.clone());
    match lifecycle::purge(&api.state, &id).await? {
        Ok(purged) => {
            let response = PurgeSessionResponse {
                session,
                state: SessionState::Purged,
                removed_bytes: purged.removed_bytes,
            };
            api.idempotency
                .remember(request.request_key.as_ref(), &hash, &response);
            Ok(Json(response))
        }
        Err(blockers) => Err(ApiFailure::purge_blocked(PurgeBlocked {
            session,
            blockers,
        })),
    }
}
