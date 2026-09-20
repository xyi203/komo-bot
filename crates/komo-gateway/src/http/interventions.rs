//! `/v1/interventions`（§7.5、§13.1）。
//!
//! 四个界面——`komo intervention list | show | answer`、TUI 的待处理清单、聊天里的
//! `/pending` 与 `/answer`、HTTP——打的是**同一批函数**（§13.1 最后一段）。这一层只做
//! 翻译：请求体是 kernel 的协议类型，动作在 [`GatewayState`] 上。
//!
//! 「清单是派生视图，不是第四张表」（§7.5 第 1 条）：`GET /v1/interventions` 是 `runs` 与
//! `approval_requests` 的并集查询，`kind` 当场判出。所以这里没有"登记一条 Intervention"的
//! 路径——它只读，答复走 `/answer`。

use axum::Json;
use axum::extract::{Path, Query, State};
use komo_kernel::protocol::http::{
    InterventionAnswerRequest, InterventionAnswerResponse, InterventionBatchAnswerRequest,
    InterventionBatchAnswerResponse, InterventionDetail, InterventionListQuery,
    InterventionListResponse,
};
use komo_kernel::traits::GatewayError;

use super::Api;
use super::error::{ApiFailure, ApiResult};
use super::idempotency::body_hash;

/// `GET /v1/interventions`
///
/// 三类一起列：审批（句柄是短 ID）、`verify`（结果不明）、`blocked`（前提没了）。每一条
/// 自带 `verdicts`——界面不必自己推"这一条能答什么"，推错一个就会给出一个按下去没反应的
/// 答案（§11.3 的 TUI 菜单是同一条理由）。
///
/// **`waiting + dependency` 与 `waiting + retry` 不在这里**：它们在等前一条 Run / 等时钟，
/// 不是在等人（§7.5 第 2 条）。要看那两类去 `GET /v1/runs/{id}` 的 `wait` 那一格。
pub async fn list(
    State(api): State<Api>,
    Query(query): Query<InterventionListQuery>,
) -> ApiResult<Json<InterventionListResponse>> {
    let interventions = api.state.interventions(&query).await?;
    Ok(Json(InterventionListResponse { interventions }))
}

/// `GET /v1/interventions/{handle}`
///
/// 审批那一份详情就是 §7.2 要展示的全部内容（计划、改动、原因、范围）。句柄此刻不在清单里
/// ——已经答过、或者从来不存在——就是 404：已答过的**原结论**由 `POST .../answer` 答
/// （"已答复的返回原结论，不报错"，§11.3）。
pub async fn show(
    State(api): State<Api>,
    Path(handle): Path<String>,
) -> ApiResult<Json<InterventionDetail>> {
    api.state
        .intervention(&handle)
        .await?
        .map(Json)
        .ok_or_else(|| ApiFailure::not_found(format!("待处理的 Intervention {handle}")))
}

/// `POST /v1/interventions/{handle}/answer`
///
/// 答复：`approve` / `reject`（范围按 §7.2）、`satisfied` / `not_performed` / `abandon` /
/// `resolve`。**结论按种类分派**：给 `blocked` 答 `satisfied` 是 422，不是"尽力而为"。
pub async fn answer(
    State(api): State<Api>,
    Path(handle): Path<String>,
    Json(request): Json<InterventionAnswerRequest>,
) -> ApiResult<Json<InterventionAnswerResponse>> {
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<InterventionAnswerResponse>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let response = api
        .state
        .answer_intervention(&handle, request.verdict, request.scope, None)
        .await
        .map_err(answer_failure)?;
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &response);
    Ok(Json(response))
}

/// `POST /v1/interventions/answers`：**一次答一批**（§11.3 的 `/approve all`）。
///
/// **只收审批**，而且范围固定为本次调用：一条命令替一批互不相干的计划选一个范围，是在替
/// 操作者猜一件他没看过的事（§7.2）。名单由发起方列出——协议里没有"全部"这个词。
pub async fn answer_batch(
    State(api): State<Api>,
    Json(request): Json<InterventionBatchAnswerRequest>,
) -> ApiResult<Json<InterventionBatchAnswerResponse>> {
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<InterventionBatchAnswerResponse>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let response = api
        .state
        .answer_approvals(&request.handles, request.approved, None)
        .await
        .map_err(answer_failure)?;
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &response);
    Ok(Json(response))
}

/// 答复的失败码。
///
/// `answer_intervention` 只在一处报 [`GatewayError::InvalidRequest`]：**结论与这条的种类
/// 不符**（§7.5 第 3 条）。那一条要的是 422（"请求读得懂，但它和此刻这条对不上"），所以
/// 在这里换一下。别的失败按原样映射。
fn answer_failure(error: GatewayError) -> ApiFailure {
    match error {
        GatewayError::InvalidRequest(message) => ApiFailure::verdict_mismatch(message),
        other => other.into(),
    }
}
