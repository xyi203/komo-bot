//! `/v1/cron`（§13.1、§10）。
//!
//! 「模型需要管理 Cron 时通过 shell 调这些命令；CLI 连接 Gateway，不直接访问数据库，
//! 也不增加专用 cron tool。」——所以这组接口是 `komo cron ...` 的后端，也是唯一的写入
//! 口。
//!
//! 两条 §10 的语句在这个文件里各只有一处实现：
//!
//! - **「Job、模块或权限发生变化时重新匹配授权」**：改**定义**走
//!   [`CronRepo::put`]（版本 +1，`GrantScope::CronJob` 绑的那个版本随之失效），只改
//!   `status`（`pause` / `resume`）走 [`CronRepo::advance`]（版本一个字不动）。把两者
//!   混成一条路的代价是具体的：操作者每暂停一次就要把这个 Job 的授权重批一遍。
//! - **「可指定该 Job 的主模型与 effort；覆盖按完整模型配置解析」**：
//!   [`resolve_model`] 要么整份用请求给的 `model_config`，要么把 `model` 当作目录 alias
//!   解析——两种都产出一份**完整**配置，而且都不碰记忆与向量模型（那两个
//!   角色在快照里，这里一个字都不读）。
//!
//! 时区与 effort **在请求前拒绝并指出支持值**：两者都是打字打出来的，而它们出错的
//! 时刻本来会推迟到凌晨三点——那时发现它的人已经走了。

use axum::Json;
use axum::extract::{Path, State};
use komo_kernel::cron::{CronJob, JobStatus, TimeZone, Trigger, parse_schedule};
use komo_kernel::protocol::http::{
    CreateCronRequest, CronDeleteResponse, CronJobStatus, CronListResponse, ManualCronRunRequest,
    ManualCronRunResponse, UpdateCronRequest,
};
use komo_kernel::types::ids::CronJobId;
use komo_kernel::types::model::{Effort, ModelConfig};
use komo_runtime::config::EffortCapabilities;

use super::Api;
use super::error::{ApiFailure, ApiResult};
use super::idempotency::body_hash;

/// `GET /v1/cron`
///
/// 响应带上每个 Job 的**下一次**与**最近一次触发**：问"那个 Job 昨天怎么样了"的人
/// 要的是这两样，而它们分别在 `cron_jobs` 和 `cron_firings` 两张表上。
pub async fn list(State(api): State<Api>) -> ApiResult<Json<CronListResponse>> {
    let jobs = api.state.cron.list().await?;
    let mut status = Vec::with_capacity(jobs.len());
    for job in &jobs {
        // 一条也读不出来不该让整张清单报错：这是个运行面，不是权威。
        let last = api
            .state
            .cron
            .firings(&job.id, 1)
            .await
            .unwrap_or_default()
            .into_iter()
            .next();
        status.push(CronJobStatus {
            job: job.id.clone(),
            next_run_at: job.next_run_at,
            last,
        });
    }
    Ok(Json(CronListResponse { jobs, status }))
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
    let trigger = trigger_of(&request.schedule, &zone, now)?;
    let workdir = resolve_workdir(request.workdir.as_deref())?;
    let model = resolve_model(
        &api,
        request.model_config.clone(),
        request.model.as_deref(),
        request.effort.as_ref(),
    )?;

    let mut job = CronJob {
        id: CronJobId::new_at(now),
        name: request.name.trim().to_string(),
        version: 1,
        trigger,
        prompt: request.prompt.clone(),
        workdir,
        status: JobStatus::Active,
        overlap: request.overlap,
        model,
        effort: request.effort.clone(),
        skills: request.skills.clone(),
        max_rounds: request.max_rounds,
        notify: request.notify.unwrap_or_default(),
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
    let hash = body_hash(&request);
    if let Some(previous) = api
        .idempotency
        .lookup::<CronJob>(request.request_key.as_ref(), &hash)?
    {
        return Ok(Json(previous));
    }
    let mut job = api
        .state
        .cron
        .get(&id)
        .await?
        .ok_or_else(|| ApiFailure::not_found(format!("定时任务 {id}")))?;

    let now = api.state.clock.now();
    if let Some(name) = &request.name {
        job.name = name.trim().to_string();
    }
    if let Some(prompt) = &request.prompt {
        job.prompt = prompt.clone();
    }
    if let Some(overlap) = request.overlap {
        job.overlap = overlap;
    }
    if let Some(max_rounds) = request.max_rounds {
        job.max_rounds = Some(max_rounds);
    }
    if let Some(notify) = request.notify {
        job.notify = notify;
    }
    if let Some(skills) = &request.skills {
        job.skills = skills.clone();
    }
    if request.workdir.is_some() {
        job.workdir = resolve_workdir(request.workdir.as_deref())?;
    }
    if request.effort.is_some() {
        job.effort = request.effort.clone();
    }
    if request.model_config.is_some() || request.model.is_some() {
        job.model = resolve_model(
            &api,
            request.model_config.clone(),
            request.model.as_deref(),
            job.effort.as_ref(),
        )?;
    } else if let Some(model) = &mut job.model {
        // effort 改了而模型没改：那份完整配置上的 effort 也要跟着改，否则它会带着
        // 旧的那一档跑。
        model.effort = job.effort.clone();
        let model = model.clone();
        check_effort(&api, &model)?;
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
        job.trigger = trigger_of(&schedule, &zone, now)?;
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
    // 只改 `status` 走 `advance`：暂停一个 Job 不是改它的定义，版本一个字都不动。
    let stored = if request.changes_definition() {
        api.state.cron.put(job).await?
    } else {
        api.state
            .cron
            .advance(&id, job.next_run_at, job.status, job.last_error.clone())
            .await?;
        job
    };
    api.idempotency
        .remember(request.request_key.as_ref(), &hash, &stored);
    Ok(Json(stored))
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

    // 一次性触发跑完就是 `done`，`enable` / `run` 都不再受理它（§10：行留着当可查询的
    // 记录，不是一个还能按的按钮）。
    let job = api
        .state
        .cron
        .get(&id)
        .await?
        .ok_or_else(|| ApiFailure::not_found(format!("定时任务 {id}")))?;
    if job.status == JobStatus::Done {
        return Err(ApiFailure::invalid(format!(
            "{} 是一次性任务，已经完成了；要再跑一次请新建一个",
            job.name
        )));
    }

    let fired = api
        .state
        .cron_scheduler()
        .run_now(&id)
        .await
        .map_err(|error| ApiFailure::invalid(error.to_string()))?
        .ok_or_else(|| ApiFailure::not_found(format!("定时任务 {id}")))?;
    api.state.watch_cron_run(&fired, &job, false);
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

/// 这个 Job 的**完整**模型配置覆盖（§10）。
///
/// 三条路，产出都是一份完整配置或者 `None`：
///
/// - 给了 `model_config`：整份用它。
/// - 只给了模型名：按 `model.<alias>` 解析完整配置。
/// - 都没给：`None`，触发时用当时的主模型快照。
///
/// 无论哪条，**记忆整理与向量模型一个字都不动**——它们是另外两个角色，在快照里各自
/// 解析（§13.3），这个函数从头到尾没读过它们。
fn resolve_model(
    api: &Api,
    full: Option<ModelConfig>,
    name: Option<&str>,
    effort: Option<&Effort>,
) -> Result<Option<ModelConfig>, ApiFailure> {
    let mut model = match (full, name) {
        (Some(full), _) => full,
        (None, Some(name)) if !name.trim().is_empty() => {
            super::config::completion_model(api, name)?
        }
        _ => {
            // 没有模型覆盖，但可能单给了 effort：那一档也得立得住，否则它会在凌晨三点
            // 变成一个 400。
            if let Some(effort) = effort {
                let mut probe = api.state.snapshot().model.clone();
                probe.effort = Some(effort.clone());
                check_effort(api, &probe)?;
            }
            return Ok(None);
        }
    };
    if model.model.trim().is_empty() {
        return Err(ApiFailure::invalid("模型覆盖里没有写 model"));
    }
    model.effort = effort.cloned().or(model.effort);
    check_effort(api, &model)?;
    Ok(Some(model))
}

/// effort **在请求前**校验，并指出支持值（§13.3：「不支持：返回具体错误，指出模型、
/// 配置位置及支持值；不静默忽略或强行映射为另一档」）。
fn check_effort(api: &Api, model: &ModelConfig) -> Result<(), ApiFailure> {
    let _ = api;
    EffortCapabilities::builtin()
        .check(model)
        .map_err(|problem| ApiFailure::invalid(format!("effort 立不住：{problem}")))
}

/// 工作目录**在创建时就核实**（§10 的 `workdir`）。
///
/// 解析晚了会在凌晨三点变成一串权限拒绝，读起来像策略问题而不是打错字。
fn resolve_workdir(dir: Option<&str>) -> Result<Option<std::path::PathBuf>, ApiFailure> {
    let Some(dir) = dir else { return Ok(None) };
    if dir.trim().is_empty() {
        return Ok(None);
    }
    let path = std::path::PathBuf::from(dir.trim());
    let resolved = path.canonicalize().map_err(|error| {
        ApiFailure::invalid(format!("工作目录 {} 用不了：{error}", path.display()))
    })?;
    Ok(Some(resolved))
}

fn trigger_of(
    schedule: &str,
    zone: &TimeZone,
    now: time::OffsetDateTime,
) -> Result<Trigger, ApiFailure> {
    let zones = komo_runtime::scheduler::JiffZoneResolver::new();
    parse_schedule(schedule, zone, now, &zones).map_err(|error| {
        // 时区不认识时把"该写什么"一起说出来：IANA 名字不是一个人能猜的形状。
        if matches!(
            error,
            komo_kernel::cron::ScheduleError::Zone(komo_kernel::cron::ZoneError::UnknownZone(_))
        ) {
            return ApiFailure::invalid(format!(
                "{error}；要写 IANA 时区名，例如 Asia/Shanghai、Europe/Berlin、UTC"
            ));
        }
        ApiFailure::invalid(error.to_string())
    })
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
