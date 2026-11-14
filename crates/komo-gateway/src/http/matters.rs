//! `/v1/matters`：目标与领域（`docs/goals.md` §4）。
//!
//! 事项属于操作者本人，不按渠道或会话归属（§2）；带会话来的调用先过一道闸：这个会话的
//! Agent 若是 `memory = private` 或 `off`（给别人用的 Agent），读写一律拒——别人的话不进
//! 操作者的清单，操作者的清单也不成为别人的注入通道。后台任务会话能推进、不能改承诺，
//! 那一条在 [`komo_memory::matter`]。
//!
//! 每个响应都带 Gateway 本地时区的今天：超期是读出来的（`Matter::is_overdue`），读的人不拿
//! 自己的时区再算一遍。

use axum::Json;
use axum::extract::{Path, Query, State};
use komo_kernel::durable::SessionKind;
use komo_kernel::protocol::http::{
    ActivityWaiting, ClosureConvention, ErrorCode, MATTER_ACTIVITY_LIMIT, MATTER_MEMORY_LIMIT,
    ManualCronRunResponse, MatterActionRequest, MatterActivity, MatterAddRequest, MatterChat,
    MatterChatResponse, MatterClosure, MatterDetail, MatterListQuery, MatterListResponse,
    MatterOverview, MatterRun, MatterRunsQuery, MatterRunsResponse, MatterShowQuery,
};
use komo_kernel::types::agent::MemoryMode;
use komo_kernel::types::ids::{CronJobId, MatterId, SessionId};
use komo_kernel::types::matter::{Matter, MatterError, MatterTrigger, MatterVerb};
use komo_kernel::types::memory::MemoryState;
use komo_memory::matter::{Caller, MatterFailure};
use time::{Date, Month, OffsetDateTime};

use super::Api;
use super::error::{ApiFailure, ApiResult};
use super::idempotency::body_hash;
use super::schedule;
use crate::service::durable::chats::WEB_SOURCE;
use crate::service::durable::hooks;
use crate::service::durable::sessions::{NewSession, time_of};

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
    let schedules = schedule::schedules_of(&api, &matters).await?;
    Ok(Json(MatterListResponse {
        matters,
        schedules,
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
    let overview = overview_of(&api, &matter).await?;
    let detail = MatterDetail {
        overview: Some(overview),
        ..detail_of(&api, matter, None).await?
    };
    Ok(Json(detail))
}

/// 目标页要的其余东西，一次取齐（`docs/redesign.md` §1.4）：绑定的支线、监听、活动、为它记的记忆。
/// 定时本身在 [`MatterDetail::schedule`]（含下一次时间、最近错误与最近一次触发）。
async fn overview_of(api: &Api, matter: &Matter) -> ApiResult<MatterOverview> {
    let core = api.state.core()?;

    // 绑定的支线：归档 / 删除了的读成没绑定（`DurableCore::bound_chat`）。
    let chat = core
        .bound_chat(matter.id.as_str())
        .await?
        .map(|record| chat_of(matter, &record));

    // 监听：触发器里的 monitor 对上登记（含已结束的）；登记里找不到的不列。
    let wanted: Vec<(&SessionId, &str)> = matter
        .triggers
        .iter()
        .filter_map(|trigger| match trigger {
            MatterTrigger::Monitor { session, monitor } => Some((session, monitor.as_str())),
            MatterTrigger::Cron { .. } => None,
        })
        .collect();
    let monitors = if wanted.is_empty() {
        Vec::new()
    } else {
        super::v2::monitors::summaries(core, true)
            .await?
            .into_iter()
            .filter(|summary| {
                summary.session.as_ref().is_some_and(|session| {
                    wanted.iter().any(|(wanted_session, wanted_monitor)| {
                        *wanted_session == session
                            && summary.handle.trim_start_matches('#') == *wanted_monitor
                    })
                })
            })
            .collect()
    };

    // 活动：带着这个目标的键的会话（定时触发的运行、挂了键的任务），最新的在前；在等你的标出来（P16）。
    let asks = core.pending_asks().await?;
    let page = core
        .task_page_for_goal(false, Some(&matter.id), Some(MATTER_ACTIVITY_LIMIT), None)
        .await?;
    let activity = page
        .tasks
        .into_iter()
        .map(|mut task| {
            for goal in &mut task.goals {
                if goal.id == matter.id {
                    goal.title = Some(matter.title.clone());
                }
            }
            let waiting = asks.get(task.session.as_str()).cloned();
            MatterActivity { task, waiting }
        })
        .collect();

    // 为它记的记忆：归属它、还没归档 / 遗忘 / 被取代的，新的在前。
    let mut memories = api
        .state
        .memories
        .catalog()
        .belonging_to(&matter.id)
        .await?;
    memories.retain(|item| {
        matches!(
            item.state,
            MemoryState::Candidate | MemoryState::Active | MemoryState::Contested
        )
    });
    memories.reverse();
    memories.truncate(MATTER_MEMORY_LIMIT);

    Ok(MatterOverview {
        chat,
        monitors,
        activity,
        memories,
    })
}

fn chat_of(matter: &Matter, record: &komo_kernel::durable::SessionRecord) -> MatterChat {
    MatterChat {
        session: SessionId::from_raw(record.id.clone()),
        title: record
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| matter.title.clone()),
    }
}

/// `GET /v1/matters/{id}/runs`：定时触发的运行历史（`cron_firings`，键是目标 id），新的在前，翻页用
/// `before`。每条带计划时间、实际时间、结果状态、错误，以及点进去看过程的会话与 Run；此刻在等你的标出来。
pub async fn runs(
    State(api): State<Api>,
    Path(id): Path<String>,
    Query(query): Query<MatterRunsQuery>,
) -> ApiResult<Json<MatterRunsResponse>> {
    let id = MatterId::from_raw(id);
    api.state.matters.get(&id).await.map_err(failure)?;
    let limit = query.limit.unwrap_or(RUNS_PAGE).clamp(1, RUNS_PAGE_MAX);
    let mut firings = api
        .state
        .cron
        .firings_page(&CronJobId::from_raw(id.as_str()), query.before, limit + 1)
        .await?;
    let next_before = if firings.len() > limit as usize {
        firings.truncate(limit as usize);
        firings.last().map(|firing| firing.scheduled_at)
    } else {
        None
    };
    let core = api.state.core()?;
    let asks = core.pending_asks().await?;
    let mut runs = Vec::with_capacity(firings.len());
    for firing in firings {
        // 实际触发的时刻 = 建出来的会话的创建时刻；读不出来不让整页失败。
        let fired_at = match &firing.session {
            Some(session) => core
                .storage
                .session(session.as_str())
                .await
                .ok()
                .flatten()
                .map(|record| time_of(record.created_at)),
            None => None,
        };
        let waiting: Option<ActivityWaiting> = firing
            .session
            .as_ref()
            .and_then(|session| asks.get(session.as_str()).cloned());
        runs.push(MatterRun {
            scheduled_at: firing.scheduled_at,
            fired_at,
            status: firing.status,
            error: firing.error,
            job_version: firing.job_version,
            session: firing.session,
            run: firing.run,
            waiting,
        });
    }
    Ok(Json(MatterRunsResponse { runs, next_before }))
}

/// 运行历史一页默认 / 最多几条。
const RUNS_PAGE: u32 = 20;
const RUNS_PAGE_MAX: u32 = 100;

/// `POST /v1/matters/{id}/chat`：给目标新开一段支线对话并绑定（P22）。标题取目标的标题。已经绑着一段
/// **活着的**支线就原样返回它（`created = false`），不再开第二段；不再追踪（结案）的目标不给绑。
/// 只有网页调：模型侧的 `komo` 工具没有这个动作。
///
/// 先建会话、再记绑定：两步不在同一个库里，第二步失败时留下一段没绑定的普通支线（用户在对话列表里看得到、
/// 能自己归档），不会出现「绑了一段不存在的会话」。
pub async fn bind_chat(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<MatterChatResponse>> {
    let id = MatterId::from_raw(id);
    let matter = api.state.matters.get(&id).await.map_err(failure)?;
    let core = api.state.core()?;
    if let Some(record) = core.bound_chat(id.as_str()).await? {
        return Ok(Json(MatterChatResponse {
            chat: Some(chat_of(&matter, &record)),
            created: false,
        }));
    }
    if matter.state.is_closed() {
        return Err(ApiFailure::new(
            ErrorCode::Conflict,
            format!(
                "{} {id} 已经是 {}，不再追踪：先 reopen 再绑定对话",
                matter.kind.label(),
                matter.state
            ),
        ));
    }
    let spec = NewSession::new(api.state.snapshot().agent.default_agent.clone())
        .source(WEB_SOURCE)
        .title(matter.title.clone());
    let record = core.create_session(spec).await?;
    api.state
        .matters
        .set_chat(&id, Some(record.id.clone()))
        .await
        .map_err(failure)?;
    Ok(Json(MatterChatResponse {
        chat: Some(chat_of(&matter, &record)),
        created: true,
    }))
}

/// `DELETE /v1/matters/{id}/chat`：解除绑定。那段支线留着，变回普通支线；之后这个目标的结果与提问回到主对话。
/// 本来就没绑定也成功（幂等）。
pub async fn unbind_chat(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<MatterChatResponse>> {
    let id = MatterId::from_raw(id);
    api.state.matters.get(&id).await.map_err(failure)?;
    api.state
        .matters
        .set_chat(&id, None)
        .await
        .map_err(failure)?;
    Ok(Json(MatterChatResponse {
        chat: None,
        created: false,
    }))
}

/// 一条事项的详情：目标本身加它的定时（`fired` 只有 `run` 动词带）。
async fn detail_of(
    api: &Api,
    matter: Matter,
    fired: Option<ManualCronRunResponse>,
) -> ApiResult<MatterDetail> {
    let schedule = schedule::schedule_of(api, &matter.id).await?;
    Ok(MatterDetail {
        matter,
        schedule,
        fired,
        overview: None,
        today: today(api),
        still_linked: Vec::new(),
        closure: None,
    })
}

/// `POST /v1/matters`：新建，`active`；带 `schedule` 就同时给它一个定时。
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
    // 定时先校验（时区、表达式、effort、工作目录……），过了再建目标：不留一个建了一半的目标。
    let job = match &request.schedule {
        Some(spec) => {
            let title = request.title.trim();
            Some(schedule::build_job(
                &api,
                &MatterId::from_raw("pending"),
                title,
                request.description.as_deref(),
                None,
                spec,
            )?)
        }
        None => None,
    };
    let matter = api
        .state
        .matters
        .add(&request, &caller)
        .await
        .map_err(failure)?;
    if let Some(mut job) = job {
        job.id = CronJobId::from_raw(matter.id.as_str());
        job.name = matter.title.clone();
        api.state.cron.put(job).await?;
    }
    let detail = detail_of(&api, matter, None).await?;
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
    if verb.is_schedule() {
        let detail = act_on_schedule(&api, &id, verb, &request, &caller).await?;
        api.idempotency
            .remember(request.request_key.as_ref(), &hash, &detail);
        return Ok(Json(detail));
    }
    let matter = api
        .state
        .matters
        .apply(&id, verb, &request, &caller)
        .await
        .map_err(failure)?;
    // 结案时提醒还挂着的触发器：不自动删，触发器可能另有用途（§8）。
    let still_linked = if matches!(
        verb,
        MatterVerb::Done | MatterVerb::Drop | MatterVerb::Retire
    ) {
        matter.triggers.clone()
    } else {
        Vec::new()
    };
    let closure = settle_memories(&api, &matter, verb).await?;
    let detail = MatterDetail {
        still_linked,
        closure,
        ..detail_of(&api, matter, None).await?
    };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &detail);
    Ok(Json(detail))
}

/// `schedule` / `unschedule` / `run`：定时是目标上的另一块，不经状态机、不升目标的版本。
///
/// 设置与去掉改的是用户要它什么时候做什么，是承诺类操作（Run 里要有用户原话，任务会话与定时会话被拒，
/// 同 `goal.add`）；`run` 是推进，不要原话。定时只给还在追踪（未结案）的目标设、触发；去掉随时可以。
async fn act_on_schedule(
    api: &Api,
    id: &MatterId,
    verb: MatterVerb,
    request: &MatterActionRequest,
    caller: &Caller,
) -> ApiResult<MatterDetail> {
    let matter = api.state.matters.get(id).await.map_err(failure)?;
    if let Some(kind) = request.kind
        && kind != matter.kind
    {
        return Err(ApiFailure::invalid(format!(
            "{id} 是{}，不是{}：用 `komo {}`",
            matter.kind.label(),
            kind.label(),
            matter.kind
        )));
    }
    if let Some(expected) = request.expected_revision
        && expected != matter.revision
    {
        return Err(komo_kernel::traits::RepoError::VersionConflict {
            expected,
            actual: matter.revision,
        }
        .into());
    }
    // 只有 schedule 认 `schedule` 字段；别的动词带了别的字段报错，同其余动词。
    if verb != MatterVerb::Schedule && request.schedule.is_some() {
        return Err(ApiFailure::invalid(format!("{verb} 不认 `schedule` 这个字段")));
    }
    let other_fields = request.title.is_some()
        || request.next.is_some()
        || request.waiting_for.is_some()
        || request.due.is_some()
        || request.clear_due
        || request.standard.is_some()
        || request.description.is_some()
        || request.trigger.is_some();
    if other_fields {
        return Err(ApiFailure::invalid(format!(
            "{verb} 只认定时的字段，不改目标的内容：改内容用 edit"
        )));
    }
    if verb.is_commitment() {
        api.state
            .matters
            .commitment_evidence(caller, verb.as_str())
            .await
            .map_err(failure)?;
    }
    if verb != MatterVerb::Unschedule && matter.state.is_closed() {
        return Err(ApiFailure::new(
            ErrorCode::Conflict,
            format!(
                "{} {id} 已经是 {}，不再追踪：定时不给结案的{}设置或触发（先 reopen）",
                matter.kind.label(),
                matter.state,
                matter.kind.label()
            ),
        ));
    }

    let job_id = CronJobId::from_raw(id.as_str());
    match verb {
        MatterVerb::Schedule => {
            let spec = request.schedule.as_ref().ok_or_else(|| {
                ApiFailure::invalid("schedule 要给定时的字段（schedule、timezone、prompt …）")
            })?;
            let existing = api.state.cron.get(&job_id).await?;
            let definition_changed = existing.is_none() || schedule::changes_definition(spec);
            let job = schedule::build_job(
                api,
                id,
                &matter.title,
                matter.description.as_deref(),
                existing,
                spec,
            )?;
            schedule::store_job(api, job, definition_changed).await?;
            detail_of(api, matter, None).await
        }
        MatterVerb::Unschedule => {
            if !api.state.cron.remove(&job_id).await? {
                return Err(ApiFailure::invalid(format!("{} {id} 没有定时", matter.kind.label())));
            }
            detail_of(api, matter, None).await
        }
        MatterVerb::Run => {
            let fired = schedule::run_now(api, id).await?;
            detail_of(api, matter, Some(fired)).await
        }
        _ => unreachable!("is_schedule 只有这三个"),
    }
}

/// 结案归档 / 重开恢复（`memory.md` §9.6「结案归档」）：事项写完之后，按事项**现在**的状态
/// 把它名下的记忆理一遍。看状态而不看这次动词，所以对已在目标状态上的重复请求（上一次
/// 事项写完、归档没跑完）也会补上；归档本身幂等。结案与归档不在同一个 aux.db 事务里（事项与
/// 记忆是两个仓储），差别见 `komo_memory::belongs` 的模块注释。
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

fn closure_of(outcome: komo_memory::BelongsOutcome) -> MatterClosure {
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
pub(crate) async fn gate(api: &Api, session: &SessionId) -> ApiResult<bool> {
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
