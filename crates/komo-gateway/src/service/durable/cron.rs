//! Durable 模式的 cron（`docs/storage-durable-d5.md` §4 W3b、决定 4 / 9）。
//!
//! 调度器（`komo_runtime::scheduler::CronScheduler`）只管「到期 → 重叠判定 → 占位 → 投出」；
//! 这里是 Durable 的**投出**：[`DurableCronTarget`] 实现 `CronTarget`。
//!
//! # 跨库幂等
//!
//! 触发记录（`cron_firings`）在 `aux.db`，会话在 `durable.db`——两个库没有同一个事务。靠预分配 ID：
//!
//! 1. 调度器先 `claim_firing`，**Session ID 随占位一起写进触发记录**；
//! 2. 目标一次 engine 提交建会话（`kind = cron`，`komo.agent` 带模型覆盖与 `maxRounds`）并受理第一条
//!    输入（`request_id = cron:{job}:{slot}`）——没有"会话建了、输入没进"的窗口；
//! 3. 回写触发记录的 Run。
//!
//! 崩在任何两步之间，重启后 [`recover`] 按触发记录里那个 Session ID 再投一次：会话已经存在就**跳过
//! 建会话**，只取回它的 Run。
//!
//! # 命令型 Job（决定 9）
//!
//! 不经模型、不建 harness run：先提交建会话，再 [`shell::run_command`] 直跑，最后提交一条 `shell.command`
//! write 落账（RunId = 那条 write submission 的 ID）。不做崩溃重放：崩在命令跑到一半，会话在而账里没有
//! submission，[`recover`] 把那一槽记成 `error`，不会把命令再跑一遍。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::cron::{CronFiring, CronJob, FiringStatus};
use komo_kernel::durable::storage::SubmissionQuery;
use komo_kernel::durable::{
    Content, ScanOrder, SessionKind, SessionRecord, SubmissionId, SubmissionStatus, legacy_ids,
};
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::traits::{CronRepo, Shutdown};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::CronJobId;
use komo_kernel::types::model::ModelConfig;
use komo_runtime::durable::DurableError;
use komo_runtime::durable::harness::inbox::Admitted;
use komo_runtime::durable::harness::{InputEntry, SubmissionDraft, WhenBusy, WriteDraft};
use komo_runtime::scheduler::{CronError, CronTarget, FireSpec, FiredRef, firing_key, model_for};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use super::DurableCore;
use super::sessions::{NewSession, create_in};
use super::shell::{self, SHELL_ENTRY};
use super::submit::Landing;
use crate::service::state::GatewayState;

/// cron 触发放置成的 entry 的 kind（`docs/storage-durable-d5.md` §2）。
pub const CRON_INPUT: &str = "cron.input";

/// 一分钟一扫（五字段表达式的精度就是分钟）。
const CRON_TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// Durable 的触发目标。
pub struct DurableCronTarget {
    state: Arc<GatewayState>,
}

impl DurableCronTarget {
    pub fn new(state: Arc<GatewayState>) -> Self {
        DurableCronTarget { state }
    }
}

fn target_error(error: impl std::fmt::Display) -> CronError {
    CronError::Target(error.to_string())
}

/// 这个 Job 的完整模型配置在当前 `model_catalog` 里叫什么（`komo.agent.model` 只认 alias）。
///
/// 按 provider / base_url / model / 凭证来源比，不比 effort 与窗口（那两样是 alias 之外的覆盖 / 探测值）。
pub(crate) fn alias_of(config: &ConfigSnapshot, model: &ModelConfig) -> Option<String> {
    config
        .model_catalog
        .completions()
        .find_map(|(alias, entry)| {
            let candidate = entry.completion()?;
            (candidate.provider == model.provider
                && candidate.base_url == model.base_url
                && candidate.model == model.model
                && candidate.api_key_env == model.api_key_env
                && candidate.auth == model.auth)
                .then(|| alias.to_string())
        })
}

/// 触发建出来的会话：`kind = cron`，`komo.agent` 带这个 Job 的覆盖。
fn new_session(config: &ConfigSnapshot, spec: &FireSpec) -> Result<NewSession, CronError> {
    let job = &spec.job;
    let mut new = NewSession::new(config.agent.default_agent.clone())
        .kind(SessionKind::Cron)
        .title(job.name.clone())
        .source(format!("cron:{}", job.id));
    if let Some(dir) = &job.workdir {
        new = new.workdir(dir.display().to_string());
    }
    if let Some(model) = &job.model {
        let alias = alias_of(config, model).ok_or_else(|| {
            CronError::Target(format!(
                "这个 Job 的模型覆盖（{}）不在当前 model_catalog 里，Durable 下只认目录里的 alias",
                model.model
            ))
        })?;
        new.doc.model = Some(alias);
    }
    new.doc.effort = spec.model.effort.as_ref().map(|effort| effort.to_string());
    new.doc.max_rounds = job.max_rounds;
    Ok(new)
}

/// 触发放置成的输入 entry：`cron.input`，`data {job, slot}`（手动 run 没有 slot，带 `manual`）。
fn input_data(spec: &FireSpec, hash: &str) -> Value {
    let slot = spec
        .slot
        .map(|slot| slot.format(&Rfc3339).unwrap_or_else(|_| slot.to_string()));
    let mut data = json!({ "job": spec.job.id.as_str(), "slot": slot, "inputHash": hash });
    if spec.slot.is_none() {
        data["manual"] = json!(true);
    }
    data
}

impl DurableCronTarget {
    fn core(&self) -> Result<&Arc<DurableCore>, CronError> {
        self.state.core().map_err(target_error)
    }

    /// 会话已经建过（重启后补投）：取回它的 Run。命令型 Job 崩在跑命令那一步时没有 submission——
    /// 不重跑，报错。
    async fn existing(
        &self,
        core: &DurableCore,
        record: &SessionRecord,
    ) -> Result<FiredRef, CronError> {
        let query = SubmissionQuery {
            conversation: Some(record.root_conversation),
            status: None,
            order: Some(ScanOrder::Ascending),
        };
        let page = core
            .storage
            .scan_submissions(&query, 1, None)
            .await
            .map_err(target_error)?;
        match page.items.first() {
            Some(first) => Ok(FiredRef {
                run: legacy_ids::run_id(first.id),
                deduplicated: true,
            }),
            None => Err(CronError::Target(
                "会话已经建了但没有受理任何输入（命令型 Job 在命令跑到一半时进程没了；不重放）"
                    .into(),
            )),
        }
    }

    async fn fire_prompt(
        &self,
        core: &Arc<DurableCore>,
        new: NewSession,
        spec: &FireSpec,
    ) -> Result<FiredRef, CronError> {
        let text = spec.job.prompt.clone();
        let hash = ContentHash::of_str(&text).to_string();
        let draft = SubmissionDraft::Input {
            content: Content::Text(text),
            entry: Some(InputEntry::new(CRON_INPUT, Some(input_data(spec, &hash)))),
            when_busy: WhenBusy::FollowUp,
            request_id: Some(spec.request_key.to_string()),
            internal: false,
        };
        let id = spec.session.to_string();
        let harness = core.harness.clone();
        core.engine.resume();
        let submission = core
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    let record = create_in(tx, &new, &id).await?;
                    match harness
                        .admit_in_tx(tx, record.root_conversation, &draft)
                        .await?
                    {
                        Admitted::Id(submission) => Ok(submission),
                        Admitted::Busy => Err(DurableError::rejected("新会话受理时忙了")),
                    }
                })
            })
            .await
            .map_err(target_error)?;
        Ok(FiredRef {
            run: legacy_ids::run_id(submission),
            deduplicated: false,
        })
    }

    async fn fire_command(
        &self,
        core: &Arc<DurableCore>,
        new: NewSession,
        spec: &FireSpec,
        command: &str,
    ) -> Result<FiredRef, CronError> {
        let id = spec.session.to_string();
        let id_in = id.clone();
        let record = core
            .engine
            .commit(move |tx| Box::pin(async move { create_in(tx, &new, &id_in).await }))
            .await
            .map_err(target_error)?;

        let call = format!("shell-{}", uuid::Uuid::now_v7());
        let ran = shell::run_command(&self.state, record.current_branch, command, &call)
            .await
            .map_err(target_error)?;
        let text = format!("!{command}");
        let hash = ContentHash::of_str(&text).to_string();
        let now_ms = (core.clock.now().unix_timestamp_nanos() / 1_000_000) as i64;
        let mut data = ran.data.clone();
        data["inputHash"] = json!(hash);
        data["job"] = json!(spec.job.id.as_str());
        if let Some(slot) = spec.slot {
            data["slot"] = json!(slot.format(&Rfc3339).unwrap_or_else(|_| slot.to_string()));
        }
        let entry = WriteDraft {
            kind: SHELL_ENTRY.to_owned(),
            model: Some(vec![komo_kernel::durable::Message::user(
                shell::model_text(command, &ran.view),
                now_ms,
            )]),
            data: Some(data),
            head: None,
            edits: None,
        };
        let accepted = core
            .land(
                &id,
                SubmissionDraft::Write {
                    entry,
                    request_id: Some(spec.request_key.to_string()),
                },
                Landing {
                    hash,
                    text,
                    reject_when_busy: false,
                    model: None,
                    interject: false,
                },
            )
            .await
            .map_err(target_error)?;
        Ok(FiredRef {
            run: accepted.run,
            deduplicated: accepted.deduplicated,
        })
    }
}

#[async_trait]
impl CronTarget for DurableCronTarget {
    async fn fire(&self, spec: FireSpec) -> Result<FiredRef, CronError> {
        let core = self.core()?;
        let id = spec.session.to_string();
        // 会话已经存在 = 上一次已经建过（重启幂等）：只取回 Run，不再建。
        if let Some(record) = core.storage.session(&id).await.map_err(target_error)? {
            return self.existing(core, &record).await;
        }
        let new = new_session(&self.state.snapshot(), &spec)?;
        match spec.job.command.clone() {
            Some(command) => self.fire_command(core, new, &spec, &command).await,
            None => self.fire_prompt(core, new, &spec).await,
        }
    }

    /// 重叠判定：Durable 没有 `runs` 表，按触发记录里的 Run 对应的 submission 有没有结算来判。
    /// `skipped` / `ok` / `error` 的记录都不算；`queued` / `waiting` 的看 submission——结算了就不算
    /// （回写状态的看客可能还差一步）；没有 Run 的 `queued` 记录（占位之后还没投出）算没结束。
    async fn has_unfinished(
        &self,
        jobs: &dyn CronRepo,
        job: &CronJobId,
    ) -> Result<bool, CronError> {
        let core = self.core()?;
        for firing in jobs.firings(job, 50).await? {
            if !matches!(firing.status, FiringStatus::Queued | FiringStatus::Waiting) {
                continue;
            }
            let Some(run) = &firing.run else {
                return Ok(true);
            };
            let Some(id) = legacy_ids::submission_of(run) else {
                continue;
            };
            if !settled(core, id).await {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

async fn settled(core: &DurableCore, id: SubmissionId) -> bool {
    matches!(
        core.storage.submission(id).await,
        Ok(Some(record)) if matches!(
            record.status(),
            SubmissionStatus::Done | SubmissionStatus::Unanswered
        )
    )
}

impl GatewayState {
    /// 按**当前**快照造 Durable 的触发目标。
    pub fn durable_cron_target(self: &Arc<Self>) -> Arc<dyn CronTarget> {
        Arc::new(DurableCronTarget::new(Arc::clone(self)))
    }
}

/// 启动补做：触发记录还在 `queued` / `waiting` 的——有 Run 的重挂看客；有 Session 没有 Run 的按
/// 预分配的 Session ID 再投一次；连 Session 都没有的记成 `error`。
pub async fn recover(state: &Arc<GatewayState>) {
    let jobs = match state.cron.list().await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::warn!(%error, "读不出 cron 清单，重启后的触发补做没做成");
            return;
        }
    };
    for job in jobs {
        let firings = match state.cron.firings(&job.id, 20).await {
            Ok(firings) => firings,
            Err(error) => {
                tracing::warn!(%error, job = %job.id, "读不出触发记录");
                continue;
            }
        };
        for firing in firings {
            if matches!(firing.status, FiringStatus::Queued | FiringStatus::Waiting) {
                recover_one(state, &job, firing).await;
            }
        }
    }
}

async fn recover_one(state: &Arc<GatewayState>, job: &CronJob, firing: CronFiring) {
    let (session, run) = match (firing.session.clone(), firing.run.clone()) {
        (Some(session), Some(run)) => (session, run),
        (Some(session), None) => {
            let spec = FireSpec {
                job: job.clone(),
                session: session.clone(),
                request_key: firing_key(&job.id, firing.scheduled_at),
                model: model_for(job, &state.snapshot().model),
                slot: Some(firing.scheduled_at),
                at: state.clock.now(),
            };
            match state.durable_cron_target().fire(spec).await {
                Ok(fired) => {
                    let _ = state
                        .cron
                        .update_firing(CronFiring {
                            run: Some(fired.run.clone()),
                            ..firing.clone()
                        })
                        .await;
                    tracing::info!(job = %job.id, run = %fired.run, "补投了崩在中途的触发");
                    (session, fired.run)
                }
                Err(error) => {
                    let _ = state
                        .cron
                        .update_firing(CronFiring {
                            status: FiringStatus::Error,
                            error: Some(error.to_string()),
                            ..firing
                        })
                        .await;
                    return;
                }
            }
        }
        (None, _) => {
            let _ = state
                .cron
                .update_firing(CronFiring {
                    status: FiringStatus::Error,
                    error: Some("占位之后进程没了，没有记下会话；这一槽不补跑".into()),
                    ..firing
                })
                .await;
            return;
        }
    };
    let fired = komo_runtime::scheduler::Fired {
        job: job.id.clone(),
        job_version: firing.job_version,
        run,
        session,
        scheduled_at: firing.scheduled_at,
        deduplicated: true,
    };
    state.watch_cron_run(&fired, job, true);
}

/// 起 cron：先补做上一代没做完的，再一分钟一扫。
pub fn spawn(state: &Arc<GatewayState>, shutdown: &Shutdown) {
    let state = Arc::clone(state);
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        recover(&state).await;
        loop {
            tokio::time::sleep(CRON_TICK).await;
            if shutdown.is_cancelled() {
                return;
            }
            match state.cron_scheduler().tick().await {
                Ok(tick) => {
                    if !tick.fired.is_empty() {
                        tracing::info!(fired = tick.fired.len(), "定时任务已入队");
                    }
                    state.watch_fired(&tick.fired).await;
                }
                Err(error) => tracing::warn!(%error, "Cron 这一轮扫描失败"),
            }
        }
    });
}
