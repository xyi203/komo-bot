//! `/v1/cron`（§13.1、§10）。
//!
//! 「模型需要管理 Cron 时通过 shell 调这些命令；CLI 连接 Gateway，不直接访问数据库，
//! 也不增加专用 cron tool。」——所以这组接口是 `komo cron ...` 的后端，也是唯一的写入
//! 口。

use axum::Json;
use axum::extract::{Path, State};
use komo_kernel::cron::{CronJob, JobStatus, TimeZone, Trigger, parse_schedule};
use komo_kernel::protocol::http::{
    CreateCronRequest, CronDeleteResponse, CronListResponse, ManualCronRunRequest,
    ManualCronRunResponse, UpdateCronRequest,
};
use komo_kernel::types::ids::CronJobId;

use super::Api;
use super::error::{ApiFailure, ApiResult};
use super::idempotency::body_hash;

/// `GET /v1/cron`
pub async fn list(State(api): State<Api>) -> ApiResult<Json<CronListResponse>> {
    Ok(Json(CronListResponse {
        jobs: api.state.cron.list().await?,
    }))
}

/// `POST /v1/cron`
pub async fn create(
    State(api): State<Api>,
    Json(request): Json<CreateCronRequest>,
) -> ApiResult<Json<CronJob>> {
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<CronJob>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }

    let now = api.state.clock.now();
    let zone = TimeZone::new(request.timezone.trim());
    let trigger = trigger_of(&api, &request.schedule, &zone, now)?;
    // **工作目录在创建时就核实**（§10 的 `workdir`）：解析晚了会在凌晨三点变成一串
    // 权限拒绝，读起来像策略问题而不是打错字。
    let workdir = match &request.workdir {
        None => None,
        Some(dir) => {
            let path = std::path::PathBuf::from(dir);
            let resolved = path.canonicalize().map_err(|error| {
                ApiFailure::invalid(format!("工作目录 {} 用不了：{error}", path.display()))
            })?;
            Some(resolved)
        }
    };

    let mut job = CronJob {
        id: CronJobId::new_at(now),
        name: request.name.trim().to_string(),
        version: 1,
        trigger,
        prompt: request.prompt.clone(),
        workdir,
        status: JobStatus::Active,
        overlap: request.overlap,
        model: request.model.as_ref().map(|name| {
            let mut model = api.state.snapshot().model.clone();
            model.model = name.trim().to_string();
            model
        }),
        effort: request.effort.clone(),
        skills: request.skills.clone(),
        max_rounds: request.max_rounds,
        next_run_at: None,
        last_error: None,
    };
    let zones = komo_runtime::scheduler::JiffZoneResolver::new();
    if let Err(error) = job.advance(now, &zones) {
        return Err(ApiFailure::invalid(error.to_string()));
    }

    let stored = api.state.cron.put(job).await?;
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &stored);
    Ok(Json(stored))
}

/// `PATCH /v1/cron/{id}`：给出的字段才改。`pause` / `resume` 就是改 `status`。
pub async fn update(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<UpdateCronRequest>,
) -> ApiResult<Json<CronJob>> {
    let id = CronJobId::from_raw(id);
    let mut job = api
        .state
        .cron
        .get(&id)
        .await?
        .ok_or_else(|| ApiFailure::not_found(format!("定时任务 {id}")))?;

    let now = api.state.clock.now();
    if let Some(name) = request.name {
        job.name = name;
    }
    if let Some(prompt) = request.prompt {
        job.prompt = prompt;
    }
    if let Some(overlap) = request.overlap {
        job.overlap = overlap;
    }
    if let Some(max_rounds) = request.max_rounds {
        job.max_rounds = Some(max_rounds);
    }
    let zone = match &request.timezone {
        Some(name) => TimeZone::new(name.trim()),
        None => zone_of(&job.trigger),
    };
    if request.schedule.is_some() || request.timezone.is_some() {
        let schedule = request
            .schedule
            .clone()
            .unwrap_or_else(|| schedule_text(&job.trigger));
        job.trigger = trigger_of(&api, &schedule, &zone, now)?;
    }
    if let Some(status) = request.status {
        job.status = status;
    }
    let zones = komo_runtime::scheduler::JiffZoneResolver::new();
    if job.status == JobStatus::Active
        && let Err(error) = job.advance(now, &zones)
    {
        return Err(ApiFailure::invalid(error.to_string()));
    }
    // **改定义走 `put`**：版本递增，绑定这个 Job 的授权随之失效（§7.2 / §10）。
    Ok(Json(api.state.cron.put(job).await?))
}

/// `DELETE /v1/cron/{id}`：移除后续调度，已有执行历史保留。
pub async fn remove(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<CronDeleteResponse>> {
    let id = CronJobId::from_raw(id);
    let firings = api.state.cron.firings(&id, u32::MAX).await?.len() as u32;
    let removed = api.state.cron.remove(&id).await?;
    Ok(Json(CronDeleteResponse {
        job: id,
        removed,
        firings_kept: firings,
    }))
}

/// `POST /v1/cron/{id}/run`：手动触发。
///
/// **使用独立请求幂等键，不冒充定时触发**（§10）——键由 `CronScheduler::run_now` 自己
/// 铸，它既不写 `cron_firings` 也不推进槽位。
pub async fn run_now(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<ManualCronRunRequest>,
) -> ApiResult<Json<ManualCronRunResponse>> {
    let id = CronJobId::from_raw(id);
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<ManualCronRunResponse>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }

    let fired = api
        .state
        .cron_scheduler()
        .run_now(&id)
        .await
        .map_err(|error| ApiFailure::invalid(error.to_string()))?
        .ok_or_else(|| ApiFailure::not_found(format!("定时任务 {id}")))?;
    api.state.waker().wake();

    let response = ManualCronRunResponse {
        job: fired.job,
        session: fired.session,
        run: fired.run,
        request_key: komo_kernel::types::ids::RequestKey::new(format!(
            "cron-manual:{id}:{}",
            fired.scheduled_at
        )),
    };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &response);
    Ok(Json(response))
}

fn trigger_of(
    api: &Api,
    schedule: &str,
    zone: &TimeZone,
    now: time::OffsetDateTime,
) -> Result<Trigger, ApiFailure> {
    let zones = komo_runtime::scheduler::JiffZoneResolver::new();
    let _ = api;
    parse_schedule(schedule, zone, now, &zones)
        .map_err(|error| ApiFailure::invalid(error.to_string()))
}

fn zone_of(trigger: &Trigger) -> TimeZone {
    match trigger {
        Trigger::Cron { tz, .. } => tz.clone(),
        Trigger::At { .. } => TimeZone::utc(),
    }
}

fn schedule_text(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Cron { expr, .. } => expr.clone(),
        Trigger::At { at } => format!(
            "@at {}",
            at.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default()
        ),
    }
}
