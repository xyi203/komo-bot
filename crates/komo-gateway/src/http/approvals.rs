//! `/v1/approvals`：**审批的审计读**（§7.4）。
//!
//! §7.5 把"需要人判断"的三类合成了一个入口（[`crate::http::interventions`]），审批的
//! **答复**只走那一条路。这个端点留下的是另一半——§7.4 那句「这一步是谁放行的」：
//! `komo run inspect` 拿它回答一份计划当时是谁、什么时候、按什么范围批的。
//!
//! 所以它**不是待处理清单**：默认只列待处理（那是审批自己的状态，不是"该答什么"），
//! `include_decided` 才把已经决定过的那些一并列出来。要看"现在有什么在等人"就去
//! `GET /v1/interventions`。

use axum::Json;
use axum::extract::{Query, State};
use komo_kernel::protocol::http::{ApprovalListQuery, ApprovalListResponse, ApprovalRecord};

use super::Api;
use super::error::ApiResult;

/// `GET /v1/approvals`
///
/// `?run=` 限定到一条 Run，`?include_decided=1` 把已决定的也列出来（按 `run` 找回来——
/// 已决定的不在待处理集合里，只能按 Run 的事件把它们的 ID 找回来再逐条读）。
pub async fn list(
    State(api): State<Api>,
    Query(query): Query<ApprovalListQuery>,
) -> ApiResult<Json<ApprovalListResponse>> {
    let mut approvals = api
        .state
        .approval_repo
        .list_pending(query.session.as_ref())
        .await?;
    if let Some(run) = &query.run {
        approvals.retain(|record| record.run.as_ref() == Some(run));
    }
    if query.include_decided
        && let Some(run) = &query.run
    {
        approvals.extend(decided_of(&api, run).await?);
    }
    Ok(Json(ApprovalListResponse { approvals }))
}

/// 这个 Run 上已经决定过的审批。
async fn decided_of(
    api: &Api,
    run: &komo_kernel::types::ids::RunId,
) -> Result<Vec<ApprovalRecord>, super::error::ApiFailure> {
    // 已决定的不在待处理集合里，只能按 Run 的事件把它们的 ID 找回来再逐条读。
    let record = komo_store::repos::runs::get(&api.state.db, run).await?;
    let Some(record) = record else {
        return Ok(Vec::new());
    };
    let events = super::events_of(api, &record.session).await?;
    let mut out = Vec::new();
    for event in events
        .iter()
        .filter(|event| event.run.as_ref() == Some(run))
    {
        let komo_kernel::events::EventPayload::ApprovalRequested(body) = &event.payload else {
            continue;
        };
        if let Some(found) = api.state.approval_repo.get(&body.approval).await?
            && found.decision.is_some()
        {
            out.push(found);
        }
    }
    Ok(out)
}
