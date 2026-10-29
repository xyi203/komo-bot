//! `/v1/matters`：目标与领域（`docs/goals.md` §4）。
//!
//! 事项属于操作者本人，不按渠道或会话归属（§2）；带会话来的调用先过一道闸：这个会话的
//! Agent 若是 `memory = private` 或 `off`（给别人用的 Agent），读写一律拒——别人的话不进
//! 操作者的清单，操作者的清单也不成为别人的注入通道。后台任务会话能推进、不能改承诺，
//! 那一条在 [`komo_runtime::matter`]。
//!
//! 每个响应都带 Gateway 本地时区的今天：超期是读出来的（`Matter::is_overdue`），读的人不拿
//! 自己的时区再算一遍。

use axum::Json;
use axum::extract::{Path, Query, State};
use komo_kernel::durable::SessionKind;
use komo_kernel::protocol::http::{
    ClosureConvention, ErrorCode, MatterActionRequest, MatterAddRequest, MatterClosure,
    MatterDetail, MatterListQuery, MatterListResponse, MatterShowQuery,
};
use komo_kernel::types::agent::MemoryMode;
use komo_kernel::types::ids::{MatterId, SessionId};
use komo_kernel::types::matter::{MatterError, MatterVerb};
use komo_runtime::matter::{Caller, MatterFailure};
use time::{Date, Month, OffsetDateTime};

use super::Api;
use super::error::{ApiFailure, ApiResult};
use super::idempotency::body_hash;
use crate::service::durable::hooks;

/// `GET /v1/matters`：默认只列未结案的，`all=true` 带上结案的。
pub async fn list(
    State(api): State<Api>,
    Query(query): Query<MatterListQuery>,
) -> ApiResult<Json<MatterListResponse>> {
    if let Some(session) = &query.session {
        gate(&api, session).await?;
    }
    let matters = api
        .state
        .matters
        .list(query.kind, query.all)
        .await
        .map_err(failure)?;
    Ok(Json(MatterListResponse {
        matters,
        today: today(&api),
    }))
}

/// `GET /v1/matters/{id}`
pub async fn show(
    State(api): State<Api>,
    Path(id): Path<String>,
    Query(query): Query<MatterShowQuery>,
) -> ApiResult<Json<MatterDetail>> {
    if let Some(session) = &query.session {
        gate(&api, session).await?;
    }
    let matter = api
        .state
        .matters
        .get(&MatterId::from_raw(id))
        .await
        .map_err(failure)?;
    Ok(Json(MatterDetail {
        matter,
        today: today(&api),
        closure: None,
    }))
}

/// `POST /v1/matters`：新建，`active`。
pub async fn add(
    State(api): State<Api>,
    Json(request): Json<MatterAddRequest>,
) -> ApiResult<Json<MatterDetail>> {
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<MatterDetail>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let caller = caller(&api, &request.session, &request.run).await?;
    let matter = api
        .state
        .matters
        .add(&request, &caller)
        .await
        .map_err(failure)?;
    let detail = MatterDetail {
        matter,
        today: today(&api),
        closure: None,
    };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &detail);
    Ok(Json(detail))
}

/// `POST /v1/matters/{id}/{action}`：action 是 §4 那张表的动词。
pub async fn act(
    State(api): State<Api>,
    Path((id, action)): Path<(String, String)>,
    Json(request): Json<MatterActionRequest>,
) -> ApiResult<Json<MatterDetail>> {
    let verb = action.parse::<MatterVerb>().map_err(ApiFailure::invalid)?;
    let id = MatterId::from_raw(id);
    // 键管的是"同一次操作重试"：同一个键换了事项或动词，就不是同一次。
    let hash = body_hash(&(&id, verb, &request));
    if let Some(previous) = api
        .idempotency
        .lookup::<MatterDetail>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let caller = caller(&api, &request.session, &request.run).await?;
    let matter = api
        .state
        .matters
        .apply(&id, verb, &request, &caller)
        .await
        .map_err(failure)?;
    let closure = settle_memories(&api, &matter, verb).await?;
    let detail = MatterDetail {
        matter,
        today: today(&api),
        closure,
    };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &detail);
    Ok(Json(detail))
}

/// 结案归档 / 重开恢复（`memory.md` §9.6「结案归档」）：事项写完之后，按事项**现在**的状态
/// 把它名下的记忆理一遍。看状态而不看这次动词，所以对已在目标状态上的重复请求（上一次
/// 事项写完、归档没跑完）也会补上；归档本身幂等。结案与归档不在同一个 aux.db 事务里（事项与
/// 记忆是两个仓储），差别见 `komo_runtime::memory::belongs` 的模块注释。
async fn settle_memories(
    api: &Api,
    matter: &komo_kernel::types::matter::Matter,
    verb: MatterVerb,
) -> ApiResult<Option<MatterClosure>> {
    let memories = &api.state.memories;
    if matter.state.is_closed() {
        let outcome = memories
            .archive_for(&matter.id)
            .await
            .map_err(|error| ApiFailure::new(ErrorCode::Internal, error.to_string()))?;
        return Ok(Some(closure_of(outcome)));
    }
    if verb == MatterVerb::Reopen {
        let outcome = memories
            .restore_for(&matter.id)
            .await
            .map_err(|error| ApiFailure::new(ErrorCode::Internal, error.to_string()))?;
        if outcome.restored > 0 && memories.space().is_some() {
            // 改了 revision，旧版本的向量失效：补齐一次。
            if let Err(error) = memories.schedule_index() {
                tracing::warn!(%error, "重开之后补向量没提交上，先只靠关键词命中");
            }
        }
        return Ok(Some(closure_of(outcome)));
    }
    Ok(None)
}

fn closure_of(outcome: komo_runtime::memory::BelongsOutcome) -> MatterClosure {
    MatterClosure {
        archived: outcome.archived,
        restored: outcome.restored,
        conventions: outcome
            .conventions
            .into_iter()
            .map(|item| ClosureConvention {
                id: item.id,
                revision: item.revision,
                content: item.content,
                applies_when: item.applies_when,
            })
            .collect(),
    }
}

/// 带会话的写：先过闸，再看它是不是后台任务会话。
async fn caller(
    api: &Api,
    session: &Option<SessionId>,
    run: &Option<komo_kernel::types::ids::RunId>,
) -> ApiResult<Caller> {
    let background_task = match session {
        Some(session) => gate(api, session).await?,
        None => false,
    };
    Caller::new(session.clone(), run.clone(), background_task).map_err(failure)
}

/// 给别人用的 Agent（`memory = private / off`）的会话拒掉（§2）；放行时答这个会话是不是
/// 后台任务。Profile 的判断与召回同一处（`hooks::resolve`）。
pub(super) async fn gate(api: &Api, session: &SessionId) -> ApiResult<bool> {
    let core = api.state.core()?;
    let record = core.session(session.as_str()).await?;
    let snapshot = api.state.snapshot();
    let resolved = hooks::resolve(&api.state, &*core.storage, record.current_branch, &snapshot)
        .await
        .map_err(|error| ApiFailure::invalid(error.to_string()))?;
    let mode = match resolved.profile.memory {
        MemoryMode::Shared => return Ok(record.kind == SessionKind::Task),
        MemoryMode::Private => "private",
        MemoryMode::Off => "off",
    };
    Err(ApiFailure::new(
        ErrorCode::Denied,
        format!(
            "这个会话的 Agent（{}）是 memory = {mode}：目标与领域属于操作者本人，给别人用的 Agent \
             看不到也改不了",
            resolved.agent_id
        ),
    ))
}

fn failure(error: MatterFailure) -> ApiFailure {
    match error {
        // 起点不对：请求读得懂，是它与此刻的状态对不上。
        MatterFailure::Rule(error @ MatterError::NotAllowed { .. }) => {
            ApiFailure::new(ErrorCode::Conflict, error.to_string())
        }
        MatterFailure::Rule(error) => ApiFailure::invalid(error.to_string()),
        MatterFailure::Invalid(message) => ApiFailure::invalid(message),
        MatterFailure::Repo(error) => error.into(),
        MatterFailure::Ledger(message) => ApiFailure::new(ErrorCode::Internal, message),
    }
}

/// Gateway 本地时区的今天（§3「超期」）。时区走系统的（同 `komo cron` 的时刻显示）；
/// 换算不出来退回 UTC 的日期。
fn today(api: &Api) -> Date {
    local_date(api.state.clock.now(), &jiff::tz::TimeZone::system())
}

fn local_date(now: OffsetDateTime, zone: &jiff::tz::TimeZone) -> Date {
    let civil = jiff::Timestamp::from_second(now.unix_timestamp())
        .map(|ts| ts.to_zoned(zone.clone()).date())
        .ok();
    civil
        .and_then(|day| {
            let month = Month::try_from(u8::try_from(day.month()).ok()?).ok()?;
            Date::from_calendar_date(i32::from(day.year()), month, u8::try_from(day.day()).ok()?)
                .ok()
        })
        .unwrap_or_else(|| now.date())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{date, datetime};

    /// UTC 的 10-31 16:30 在上海已经是 11-01：超期按 Gateway 本地的日期读。
    #[test]
    fn today_is_the_local_calendar_date() {
        let shanghai = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap();
        assert_eq!(
            local_date(datetime!(2026-10-31 16:30 UTC), &shanghai),
            date!(2026 - 11 - 01)
        );
        assert_eq!(
            local_date(datetime!(2026-10-31 16:30 UTC), &jiff::tz::TimeZone::UTC),
            date!(2026 - 10 - 31)
        );
    }
}
