//! 目标上的定时（`docs/redesign.md` P8）：设置 / 修改 / 去掉 / 立即触发，以及目标详情里带的定时。
//!
//! 原先独立的 `/v1/cron*` 并进了目标的接口：`POST /v1/matters`（创建时带 `schedule`）与
//! `POST /v1/matters/{id}/{schedule|unschedule|run}`（[`super::matters`] 分派到这里）。「模型需要
//! 管理定时时调 `komo` 工具的 `goal.schedule` 等动作，CLI 连接 Gateway，不直接访问数据库」——
//! 这里是它们共用的唯一写入口。
//!
//! 两条 §10 的语句在这个文件里各只有一处实现：
//!
//! - **改定义与改状态分开**：改**定义**走 [`CronRepo::put`]（定时版本 +1），只暂停 / 恢复
//!   （`enabled`）走 [`CronRepo::advance`]（版本一个字不动）——暂停一个定时不是改它的定义。
//!   两者都不动目标的 `revision`。
//! - **「可指定该定时的主模型与 effort；覆盖按完整模型配置解析」**：
//!   [`resolve_model`] 要么整份用请求给的 `model_config`，要么把 `model` 当作目录 alias
//!   解析——两种都产出一份**完整**配置，而且都不碰记忆与向量模型（那两个角色在快照里，这里一个字都不读）。
//!
//! 时区与 effort **在请求前拒绝并指出支持值**：两者都是打字打出来的，而它们出错的
//! 时刻本来会推迟到凌晨三点——那时发现它的人已经走了。

use komo_kernel::cron::{CronJob, JobStatus, TimeZone, Trigger, parse_schedule};
use komo_kernel::protocol::http::{ManualCronRunResponse, MatterSchedule, ScheduleSpec};
use komo_kernel::types::ids::{CronJobId, MatterId, RequestKey};
use komo_kernel::types::matter::Matter;
use komo_kernel::types::model::{Effort, ModelConfig};
use komo_llm::EffortCapabilities;

use super::Api;
use super::error::{ApiFailure, ApiResult};

/// 没给时区时用的：与 CLI 一直以来的默认一致。
const DEFAULT_TIMEZONE: &str = "UTC";

/// 目标上的定时（没有就是 `None`），带最近一次触发。读不出最近一次触发不让整次读取失败：这是
/// 个运行面，不是权威。
pub(super) async fn schedule_of(api: &Api, id: &MatterId) -> ApiResult<Option<MatterSchedule>> {
    let id = CronJobId::from_raw(id.as_str());
    let Some(job) = api.state.cron.get(&id).await? else {
        return Ok(None);
    };
    let last = api
        .state
        .cron
        .firings(&id, 1)
        .await
        .unwrap_or_default()
        .into_iter()
        .next();
    Ok(Some(MatterSchedule { job, last }))
}

/// 清单里带定时的目标的定时。
pub(super) async fn schedules_of(api: &Api, matters: &[Matter]) -> ApiResult<Vec<MatterSchedule>> {
    let ids: std::collections::BTreeSet<&str> = matters.iter().map(|m| m.id.as_str()).collect();
    let mut out = Vec::new();
    for job in api.state.cron.list().await? {
        if !ids.contains(job.id.as_str()) {
            continue;
        }
        let last = api
            .state
            .cron
            .firings(&job.id, 1)
            .await
            .unwrap_or_default()
            .into_iter()
            .next();
        out.push(MatterSchedule { job, last });
    }
    Ok(out)
}

/// 照 `spec` 造出（`existing` 为 `None`）或改出（给出的字段才改）这个目标的定时，并算好下一次。
/// **只校验、不落库**：创建目标时先过这一关，再建目标。
pub(super) fn build_job(
    api: &Api,
    matter_id: &MatterId,
    title: &str,
    description: Option<&str>,
    existing: Option<CronJob>,
    spec: &ScheduleSpec,
) -> ApiResult<CronJob> {
    let now = api.state.clock.now();
    let creating = existing.is_none();
    let mut job = match existing {
        Some(job) => job,
        None => {
            if spec.schedule.as_deref().is_none_or(|s| s.trim().is_empty()) {
                return Err(ApiFailure::invalid(
                    "还没有定时：要给 schedule（五字段 cron 表达式或 `@at YYYY-MM-DD HH:MM`）",
                ));
            }
            CronJob {
                id: CronJobId::from_raw(matter_id.as_str()),
                name: title.to_string(),
                version: 1,
                // 下面一定会被 `schedule` 换掉；占位而已。
                trigger: Trigger::At { at: now },
                prompt: String::new(),
                command: None,
                workdir: None,
                status: JobStatus::Active,
                overlap: Default::default(),
                model: None,
                effort: None,
                skills: Vec::new(),
                max_rounds: None,
                notify: Default::default(),
                next_run_at: None,
                last_error: None,
            }
        }
    };
    job.name = title.to_string();

    // `prompt` 与 `command` 二选一（§10）：都给就拒；换了一种就把另一种清掉。
    let command = spec
        .command
        .as_deref()
        .map(str::trim)
        .filter(|command| !command.is_empty());
    let prompt = spec
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty());
    if command.is_some() && prompt.is_some() {
        return Err(ApiFailure::invalid(
            "prompt 与 command 二选一，不能同时给（§10）",
        ));
    }
    if let Some(command) = command {
        job.command = Some(command.to_string());
        job.prompt = String::new();
    } else if let Some(prompt) = prompt {
        job.command = None;
        job.prompt = prompt.to_string();
    }
    if creating
        && job.command.is_none()
        && job.prompt.is_empty()
        && description.is_none_or(|text| text.trim().is_empty())
    {
        return Err(ApiFailure::invalid(
            "到点做什么没有说：给 prompt 或 command，或者先给目标写说明（description）",
        ));
    }
    if let Some(overlap) = spec.overlap {
        job.overlap = overlap;
    }
    if let Some(max_rounds) = spec.max_rounds {
        job.max_rounds = Some(max_rounds);
    }
    if let Some(notify) = spec.notify {
        job.notify = notify;
    }
    if let Some(skills) = &spec.skills {
        job.skills = skills.clone();
    }
    if spec.workdir.is_some() {
        job.workdir = resolve_workdir(spec.workdir.as_deref())?;
    }
    if spec.effort.is_some() {
        job.effort = spec.effort.clone();
    }
    if spec.model_config.is_some() || spec.model.is_some() {
        job.model = resolve_model(
            api,
            spec.model_config.clone(),
            spec.model.as_deref(),
            job.effort.as_ref(),
        )?;
    } else if let Some(model) = &mut job.model {
        // effort 改了而模型没改：那份完整配置上的 effort 也要跟着改，否则它会带着旧的那一档跑。
        model.effort = job.effort.clone();
        let model = model.clone();
        check_effort(api, &model)?;
    } else if let Some(effort) = &job.effort {
        // 没有模型覆盖，但单给了 effort：那一档也得立得住，否则它会在凌晨三点变成一个 400。
        let mut probe = api.state.snapshot().model.clone();
        probe.effort = Some(effort.clone());
        check_effort(api, &probe)?;
    }

    let rescheduled = spec.schedule.is_some() || spec.timezone.is_some();
    if rescheduled {
        let zone = match (&spec.timezone, creating) {
            (Some(name), _) => TimeZone::new(name.trim()),
            (None, true) => TimeZone::new(DEFAULT_TIMEZONE),
            (None, false) => zone_of(&job.trigger),
        };
        let schedule = spec
            .schedule
            .clone()
            .unwrap_or_else(|| schedule_text(&job.trigger));
        job.trigger = trigger_of(&schedule, &zone, now)?;
        // 给了新的调度，一次性触发跑完留下的 `done` 不再成立。
        if job.status == JobStatus::Done {
            job.status = JobStatus::Active;
        }
    }
    match spec.enabled {
        Some(false) => job.status = JobStatus::Paused,
        Some(true) if job.status != JobStatus::Done || rescheduled => {
            job.status = JobStatus::Active;
        }
        Some(true) => {
            return Err(ApiFailure::invalid(
                "这是一次性定时，已经跑完了：要再来一次请重新给 schedule",
            ));
        }
        None => {}
    }

    let zones = komo_runtime::scheduler::JiffZoneResolver::new();
    if job.status == JobStatus::Active
        && let Err(error) = job.advance(now, &zones)
    {
        return Err(ApiFailure::invalid(error.to_string()));
    }
    Ok(job)
}

/// 落库一个造好 / 改好的定时。只改了启停（`enabled`）不算改定义：走 `advance`，版本不动。
pub(super) async fn store_job(
    api: &Api,
    job: CronJob,
    definition_changed: bool,
) -> ApiResult<CronJob> {
    if definition_changed {
        return Ok(api.state.cron.put(job).await?);
    }
    api.state
        .cron
        .advance(&job.id, job.next_run_at, job.status, job.last_error.clone())
        .await?;
    Ok(job)
}

/// 这次 `schedule` 动词改的是**定义**吗：除了 `enabled` 之外还给了别的字段。
pub(super) fn changes_definition(spec: &ScheduleSpec) -> bool {
    ScheduleSpec {
        enabled: None,
        ..spec.clone()
    } != ScheduleSpec::default()
}

/// 手动触发：**使用独立请求幂等键，不冒充定时触发**（§10）——键由 `CronScheduler::run_now`
/// 自己铸，它既不写 `cron_firings` 也不推进槽位。
pub(super) async fn run_now(api: &Api, id: &MatterId) -> ApiResult<ManualCronRunResponse> {
    let job_id = CronJobId::from_raw(id.as_str());
    // 一次性定时跑完就是 `done`，`run` 不再受理它（§10：行留着当可查询的记录，不是一个还能按的按钮）。
    let job = api
        .state
        .cron
        .get(&job_id)
        .await?
        .ok_or_else(|| ApiFailure::invalid(format!("目标 {id} 没有定时")))?;
    if job.status == JobStatus::Done {
        return Err(ApiFailure::invalid(format!(
            "{} 的定时是一次性的，已经跑完了；要再跑一次请重新给 schedule",
            job.name
        )));
    }
    let fired = api
        .state
        .cron_scheduler()
        .run_now(&job_id)
        .await
        .map_err(|error| ApiFailure::invalid(error.to_string()))?
        .ok_or_else(|| ApiFailure::invalid(format!("目标 {id} 没有定时")))?;
    api.state.watch_cron_run(&fired, &job, false);
    Ok(ManualCronRunResponse {
        job: fired.job,
        session: fired.session,
        run: fired.run,
        request_key: RequestKey::new(format!("cron-manual:{job_id}:{}", fired.scheduled_at)),
    })
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
    // `komo.agent.model` 只认目录里的 alias，整份配置对不上任何 alias 就当场拒绝，
    // 不留到凌晨触发时才失败。
    if crate::service::durable::background::cron::alias_of(&api.state.snapshot(), &model).is_none()
    {
        return Err(ApiFailure::invalid(format!(
            "模型覆盖（{}）不在 model_catalog 里：只能用目录里的模型 alias",
            model.model
        )));
    }
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
