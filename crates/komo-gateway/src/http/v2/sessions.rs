//! Durable 模式的 `/v1/sessions`（`docs/storage-durable-d5.md` §4 W2a）：create / list / detail /
//! submit / boundary / resume，加 `/v1/home-session`。
//!
//! delete / purge 与 `/v1/reconcile` 是 W4a 的（生命周期）；事件流是 W2b 的。
//!
//! 状态码映射在 `http::error`（`SessionError`）：`NotFound` 404、`Invalid` 400、`NotOpen` /
//! `Busy` / `Conflict` 409（`NotOpen` 说清是哪种状态）、请求键冲突 409、`Internal` 500。

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use komo_kernel::durable::legacy_ids;
use komo_kernel::protocol::http::{
    BoundaryRequest, BoundaryResponse, CreateSessionRequest, ResumeRequest, ResumeResponse,
    SessionDetail, SessionListResponse, SessionSummary, SubmitRunRequest, SubmitRunResponse,
};
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::model::Effort;
use komo_runtime::durable::harness::WhenBusy;

use crate::http::Api;
use crate::http::error::{ApiFailure, ApiResult};
use crate::http::idempotency::body_hash;
use crate::service::durable::DurableCore;
use crate::service::durable::hooks;
use crate::service::durable::sessions::NewSession;
use crate::service::durable::shell;
use crate::service::durable::submit::{Accepted, InputRequest, ModelOverride};

pub fn routes() -> Router<Api> {
    Router::new()
        .route("/v1/home-session", get(home))
        .route("/v1/sessions", get(list).post(create))
        .route("/v1/sessions/{id}", get(detail))
        .route("/v1/sessions/{id}/runs", post(submit))
        .route("/v1/sessions/{id}/resume", post(resume))
        .route("/v1/sessions/{id}/boundary", post(boundary))
}

/// query 里那个开关开着吗。`?all`、`?all=1`、`?all=true`、`?all=yes` 都算开（同 Legacy：
/// §13.1 写的是 `?all=1`，而 `serde_urlencoded` 的 bool 只认 `true` / `false`）。
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

/// `GET /v1/sessions`：默认只列 `active` 与 `closing`；`?all=1` 带上 `deleted`；`purged` 不列。
async fn list(
    State(api): State<Api>,
    uri: axum::http::Uri,
) -> ApiResult<Json<SessionListResponse>> {
    let core = api.state.core()?;
    let all = flag(uri.query().unwrap_or_default(), "all");
    let records = core.list_sessions(all).await?;
    let mut sessions = Vec::with_capacity(records.len());
    for record in &records {
        sessions.push(core.session_summary(record).await?);
    }
    Ok(Json(SessionListResponse { sessions }))
}

/// `POST /v1/sessions`：建会话（sessions 行 + 根 conversation + `komo.agent`，一次提交），落在默认
/// Agent 名下。请求键幂等沿用进程内的那份（同 Legacy）。
async fn create(
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
    let core = api.state.core()?;
    let mut spec = NewSession::new(api.state.snapshot().agent.default_agent.clone());
    if let Some(title) = &request.title {
        spec = spec.title(title);
    }
    if let Some(workdir) = &request.workdir {
        spec = spec.workdir(workdir);
    }
    if let Some(key) = &request.request_key {
        spec = spec.request_key(key.as_str());
    }
    let record = core.create_session(spec).await?;
    let summary = core.session_summary(&record).await?;
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &summary);
    Ok(Json(summary))
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
        // W4a：faulted 任务 → `Blocked`。
        pending: Vec::new(),
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
        let accepted = shell::submit_shell(
            &api.state,
            &id,
            Some(request.request_key.as_str().to_owned()),
            &request.text,
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
    let accepted = core.submit_input(&id, input).await?;
    receipt(core, accepted).await
}

/// `POST /v1/sessions/{id}/resume`：现在到哪了。D5 里 run 的续跑由引擎自己做（启动与每拍的恢复），
/// 这里只返回还没结束的 run（并叫醒调度），待处理的 Intervention W4a 接上。
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
    Ok(Json(ResumeResponse {
        session: SessionId::from_raw(id),
        resumed,
        // W4a：faulted 任务 → `Blocked`。
        pending: Vec::new(),
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
