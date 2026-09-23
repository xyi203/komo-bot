//! Cron 到期检查（§10）。
//!
//! 「Cron Scheduler 只负责产生 Run，复用 AgentLoop、Policy、工具和存储」——所以这个
//! 文件到 `accept_input` 为止就结束了：到期 → 重叠判定 → `claim_firing` 占位 → 把触发
//! 输入交给账本 → 推进下一个槽位。**执行是普通队列的事**，这里不碰。
//!
//! 三条默认行为各是下面的一段：
//!
//! - 唯一键 `job_id + scheduled_at_utc` 防止同一计划时间重复创建运行——由
//!   [`CronRepo::claim_firing`] 答，`false` 就是别人（或上一次启动）已经占过了。
//! - 上一次仍未结束时按 [`OverlapPolicy`] 跳过并**记录原因**。
//! - 停机期间错过的触发**不集中补跑**：推进槽位时从 `now` 往后找，中间那些槽位就此
//!   过去——而"停机前已经持久创建、尚未完成的 Cron Run"是恢复扫描的事，不在这里。

use std::sync::Arc;

use komo_kernel::cron::{CronFiring, CronJob, FiringStatus, JobStatus, OverlapPolicy, Trigger};
use komo_kernel::traits::{Clock, CronRepo, Ledger, LedgerError, RepoError, ZoneResolver};
use komo_kernel::types::ids::{CronJobId, RequestKey, RunId, SessionId};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::turn::AcceptInput;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::Waker;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CronError {
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

/// 一次真的投出去了的触发。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    pub job: CronJobId,
    pub job_version: u64,
    pub run: RunId,
    pub session: SessionId,
    pub scheduled_at: OffsetDateTime,
    /// 账本说这是同一请求键的原 Run（重发）——重启后补跑时会看到。
    pub deduplicated: bool,
}

/// 一次没有投出去的触发，以及原因。**原因要记下来**（§10）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub job: CronJobId,
    pub scheduled_at: Option<OffsetDateTime>,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// 上一次触发还没结束（含等待审批、重试或结果核对）。
    Overlap,
    /// 这个计划时间已经有一条触发记录了。
    AlreadyFired,
    /// 触发器解析不了 / 算不出下一槽。**不致命**：跳过它，别的 Job 照跑。
    Schedule(String),
    /// 这一槽错过得太久了——超过这个 Job 自己的间隔（§10：停机期间错过的触发不集中
    /// 补跑）。
    TooLate,
}

impl SkipReason {
    /// 写进触发记录的那句话。**跳过要留痕**（§10），所以它不是一句"跳过了"，而是
    /// 说得出为什么。
    pub fn note(&self) -> String {
        match self {
            SkipReason::Overlap => "上一次触发还没结束（含等待审批、重试或结果核对）".into(),
            SkipReason::AlreadyFired => "这个计划时间已经有一条触发记录了".into(),
            SkipReason::Schedule(reason) => format!("算不出槽位：{reason}"),
            SkipReason::TooLate => "错过得超过这个 Job 自己的间隔，不补跑".into(),
        }
    }
}

/// 一次扫描的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CronTick {
    pub fired: Vec<Fired>,
    pub skipped: Vec<Skipped>,
}

pub struct CronScheduler {
    jobs: Arc<dyn CronRepo>,
    ledger: Arc<dyn Ledger>,
    clock: Arc<dyn Clock>,
    zones: Arc<dyn ZoneResolver>,
    /// 没有 Job 级覆盖时用的模型配置快照（§10：覆盖按完整模型配置解析）。
    model: ModelConfig,
    /// 投进队列之后叫醒调度器（§10 最后一段）。
    waker: Option<Waker>,
}

impl std::fmt::Debug for CronScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CronScheduler")
            .field("model", &self.model.model)
            .finish()
    }
}

impl CronScheduler {
    pub fn new(
        jobs: Arc<dyn CronRepo>,
        ledger: Arc<dyn Ledger>,
        clock: Arc<dyn Clock>,
        zones: Arc<dyn ZoneResolver>,
        model: ModelConfig,
    ) -> Self {
        CronScheduler {
            jobs,
            ledger,
            clock,
            zones,
            model,
            waker: None,
        }
    }

    pub fn with_waker(mut self, waker: Waker) -> Self {
        self.waker = Some(waker);
        self
    }

    /// 到期的都过一遍。
    pub async fn tick(&self) -> Result<CronTick, CronError> {
        let now = self.clock.now();
        let mut tick = CronTick::default();

        for job in self.jobs.due(now).await? {
            match self.fire(job, now).await? {
                Outcome::Fired(fired) => tick.fired.push(fired),
                Outcome::Skipped(skipped) => tick.skipped.push(skipped),
            }
        }

        if !tick.fired.is_empty()
            && let Some(waker) = &self.waker
        {
            waker.wake();
        }
        Ok(tick)
    }

    /// 手动触发（`komo cron run JOB_ID`）。
    ///
    /// §10：「手动 run 使用独立请求幂等键，**不冒充定时触发**」——所以它既不写
    /// `cron_firings`，也不推进槽位，键里带一个此刻的时间戳。
    pub async fn run_now(&self, job: &CronJobId) -> Result<Option<Fired>, CronError> {
        let Some(job) = self.jobs.get(job).await? else {
            return Ok(None);
        };
        let now = self.clock.now();
        let session = SessionId::new_at(now);
        let key = RequestKey::new(format!(
            "cron-manual:{}:{}",
            job.id,
            now.format(&Rfc3339).unwrap_or_else(|_| now.to_string())
        ));
        let accepted = self.accept(&job, session.clone(), key, now).await?;
        if let Some(waker) = &self.waker {
            waker.wake();
        }
        Ok(Some(Fired {
            job: job.id.clone(),
            job_version: job.version,
            run: accepted.0,
            session,
            scheduled_at: now,
            deduplicated: accepted.1,
        }))
    }

    async fn fire(&self, mut job: CronJob, now: OffsetDateTime) -> Result<Outcome, CronError> {
        let Some(slot) = job.next_run_at else {
            return Ok(Outcome::Skipped(Skipped {
                job: job.id.clone(),
                scheduled_at: None,
                reason: SkipReason::Schedule("没有下一个槽位".into()),
            }));
        };

        // 「停机期间错过的触发不集中补跑」（§10）。**超过这个 Job 自己的间隔**就不补跑：
        // 每分钟的 Job 迟到两分钟已经没有意义，每月一次的 Job 迟到一天照跑，一个全局的
        // 宽限期对两端一定有一端是错的。
        if job.slot_missed_by_too_much(slot, now, self.zones.as_ref()) {
            tracing::info!(job = %job.id, scheduled_at = %slot, "这一槽错过太久了，不补跑");
            return self.skip(job, slot, now, SkipReason::TooLate).await;
        }

        // 「同一 Job 上一次仍未结束（含等待审批、重试或结果核对）时，跳过本次并记录
        // 原因」。
        if job.overlap == OverlapPolicy::Skip && self.jobs.has_unfinished_firing(&job.id).await? {
            tracing::info!(job = %job.id, scheduled_at = %slot, "上一次触发还没结束，跳过本次");
            return self.skip(job, slot, now, SkipReason::Overlap).await;
        }

        // 唯一键 `job_id + scheduled_at_utc`：已经存在就**不重复创建运行**。
        if !self
            .jobs
            .claim_firing(CronFiring::claimed(&job, slot))
            .await?
        {
            tracing::info!(job = %job.id, scheduled_at = %slot, "这个计划时间已经有触发记录了");
            // 这一条**不重写那行触发记录**：那一槽归别人（或上一次启动）了，把它盖成
            // "skipped" 会把一条真的跑过的历史抹掉。
            self.advance(&mut job, now).await?;
            return Ok(Outcome::Skipped(Skipped {
                job: job.id.clone(),
                scheduled_at: Some(slot),
                reason: SkipReason::AlreadyFired,
            }));
        }

        let session = SessionId::new_at(now);
        let key = RequestKey::new(format!(
            "cron:{}:{}",
            job.id,
            slot.format(&Rfc3339).unwrap_or_else(|_| slot.to_string())
        ));
        let (run, deduplicated) = self.accept(&job, session.clone(), key, now).await?;

        // 触发记录补上它产生的 Session / Run——「据此可以补完尚未写入的触发输入」。
        let recorded = CronFiring {
            session: Some(session.clone()),
            run: Some(run.clone()),
            status: FiringStatus::Queued,
            ..CronFiring::claimed(&job, slot)
        };
        // 补字段走 `update_firing`：`claim_firing` 的 `false` 同时是"别人抢走了"的
        // 意思，用它兼做更新就分不清两件相反的事。
        let _ = self.jobs.update_firing(recorded).await?;

        let job_version = job.version;
        self.advance(&mut job, now).await?;

        tracing::info!(job = %job.id, run = %run, session = %session, scheduled_at = %slot, "定时任务已入队");
        Ok(Outcome::Fired(Fired {
            job: job.id.clone(),
            job_version,
            run,
            session,
            scheduled_at: slot,
            deduplicated,
        }))
    }

    /// 跳过这一槽，**并留痕**（§10：「跳过本次并记录原因」）。
    ///
    /// 留痕的地方是 `cron_firings` 自己：一条 `skipped` 的触发记录。记在 `last_error`
    /// 上是不行的——那一列是"触发 / 配置层面的问题"，下一次成功触发就会把它清掉，而
    /// "昨天那一槽被跳过了"是一条历史，不是一个当前状态。
    async fn skip(
        &self,
        mut job: CronJob,
        slot: OffsetDateTime,
        now: OffsetDateTime,
        reason: SkipReason,
    ) -> Result<Outcome, CronError> {
        let note = reason.note();
        let firing = CronFiring::skipped(&job, slot, note);
        if !self.jobs.claim_firing(firing.clone()).await? {
            // 这一槽已经有记录了（重叠判定与 claim 之间还有别人）——那条记录是权威。
            let _ = self.jobs.update_firing(firing).await?;
        }
        self.advance(&mut job, now).await?;
        Ok(Outcome::Skipped(Skipped {
            job: job.id.clone(),
            scheduled_at: Some(slot),
            reason,
        }))
    }

    /// 更新本次触发的状态（§10 流程的最后一步）。
    ///
    /// `waiting` 既不是跑成也不是失败：那是任务在**问**。调用方按 `NotifyPolicy` 决定
    /// 投不投，这里只负责让它在 `komo cron list` 上看得见。
    pub async fn settle(
        &self,
        job: &CronJobId,
        scheduled_at: OffsetDateTime,
        status: FiringStatus,
        error: Option<String>,
    ) -> Result<bool, CronError> {
        let Some(stored) = self.jobs.get(job).await? else {
            return Ok(false);
        };
        let existing = self
            .jobs
            .firings(job, u32::MAX)
            .await?
            .into_iter()
            .find(|firing| firing.scheduled_at == scheduled_at);
        let Some(firing) = existing else {
            // 手动 run 没有触发记录，这是正常的（§10：不冒充定时触发）。
            let _ = stored;
            return Ok(false);
        };
        Ok(self
            .jobs
            .update_firing(CronFiring {
                status,
                error,
                ..firing
            })
            .await?)
    }

    async fn accept(
        &self,
        job: &CronJob,
        session: SessionId,
        request_key: RequestKey,
        now: OffsetDateTime,
    ) -> Result<(RunId, bool), CronError> {
        // §10：Job 可指定自己的主模型与 effort，**按完整模型配置解析**，不影响记忆整理
        // 或向量模型。所以这里要么整份用 Job 的，要么整份用当前主模型的。
        let mut model = job.model.clone().unwrap_or_else(|| self.model.clone());
        if let Some(effort) = &job.effort {
            model.effort = Some(effort.clone());
        }

        let accepted = self
            .ledger
            .accept_input(AcceptInput {
                session,
                request_key,
                text: job.prompt.clone(),
                // 来源仍是 Cron（§8.8）：恢复后也不会因为由本机 Gateway 发起就升权。
                source: PlanSource::Cron {
                    job: job.id.clone(),
                    job_version: job.version,
                },
                peer: None,
                model,
                // Job 的工作目录落到这个 Session 上——创建 Job 时就核实过它存在
                // （§10），这里只是把它带过去。
                workdir: job.workdir.clone(),
                // 定时 Job 不是子代理：没有父 Run，也没有结果契约。
                delegate: None,
                // Cron 是**没有归属的入口**（§四）：它的身份按当前配置的默认 Agent 在装配
                // 那一步兜底（`service::segment`）。这里不编一份——`CronScheduler` 手里只有
                // 队列与模型，没有工具目录、没有工作目录解析、也没有配置快照。
                snapshot: None,
                at: now,
            })
            .await?;
        Ok((accepted.run, accepted.deduplicated))
    }

    /// 推进到下一个槽位并落库。
    ///
    /// 走 [`CronRepo::advance`] 而不是 `put`：推进槽位不是定义变更，**版本一个字都不能
    /// 动**——`put` 会递增版本，而绑定这个 Job 的授权是按版本绑的（§7.2 / §10），每触发
    /// 一次就作废一次授权是说不通的。
    ///
    /// 一次性触发在这里**完成**（§10：`@at` 是一次性，claim 时完成）；算不出下一槽的
    /// Job 把原因写进 `last_error` 并留在清单里——安静地再也不响是最糟的结果。
    async fn advance(&self, job: &mut CronJob, now: OffsetDateTime) -> Result<(), CronError> {
        if matches!(job.trigger, Trigger::At { .. }) {
            job.status = JobStatus::Done;
            job.next_run_at = None;
        } else if let Err(error) = job.advance(now, self.zones.as_ref()) {
            // `CronJob::advance` 已经把 next_run_at 清空、把原因写进 last_error 了。
            tracing::warn!(job = %job.id, error = %error, "算不出下一个槽位");
        }
        self.jobs
            .advance(&job.id, job.next_run_at, job.status, job.last_error.clone())
            .await?;
        Ok(())
    }
}

enum Outcome {
    Fired(Fired),
    Skipped(Skipped),
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::cron::{TimeZone, next_occurrence, parse_schedule};
    use komo_kernel::test_support::{MemCronRepo, MemLedger, TestClock};
    use komo_kernel::types::ids::CronJobId;
    use time::macros::datetime;

    use crate::scheduler::JiffZoneResolver;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    fn model() -> ModelConfig {
        komo_kernel::test_support::sample_model()
    }

    fn job(id: &str, next_run_at: Option<OffsetDateTime>) -> CronJob {
        CronJob {
            id: CronJobId::from_raw(id),
            name: "morning-summary".into(),
            version: 1,
            trigger: Trigger::Cron {
                expr: "0 9 * * *".into(),
                tz: TimeZone::new("Asia/Shanghai"),
            },
            prompt: "整理今天的技术动态".into(),
            workdir: None,
            status: JobStatus::Active,
            overlap: OverlapPolicy::Skip,
            model: None,
            effort: None,
            skills: vec![],
            max_rounds: None,
            notify: Default::default(),
            next_run_at,
            last_error: None,
        }
    }

    struct Harness {
        jobs: Arc<MemCronRepo>,
        ledger: Arc<MemLedger>,
        clock: TestClock,
        scheduler: CronScheduler,
    }

    fn harness() -> Harness {
        let jobs = Arc::new(MemCronRepo::new());
        let clock = TestClock::at(NOW);
        let ledger = Arc::new(MemLedger::new(clock.clone()));
        let scheduler = CronScheduler::new(
            Arc::clone(&jobs) as Arc<dyn CronRepo>,
            Arc::clone(&ledger) as Arc<dyn Ledger>,
            Arc::new(clock.clone()),
            Arc::new(JiffZoneResolver::new()),
            model(),
        );
        Harness {
            jobs,
            ledger,
            clock,
            scheduler,
        }
    }

    #[tokio::test]
    async fn a_due_job_becomes_one_queued_run() {
        let h = harness();
        h.jobs
            .put(job("job-1", Some(NOW - time::Duration::minutes(1))))
            .await
            .unwrap();

        let tick = h.scheduler.tick().await.unwrap();
        assert_eq!(tick.fired.len(), 1, "{tick:?}");
        assert!(tick.skipped.is_empty());

        // 账本里真的有一条输入，来源是 Cron。
        let events = h.ledger.events();
        assert!(!events.is_empty());
        assert_eq!(tick.fired[0].job.as_str(), "job-1");
    }

    /// 唯一键 `job_id + scheduled_at_utc`：同一计划时间不重复创建运行。
    #[tokio::test]
    async fn the_same_scheduled_time_is_only_ever_claimed_once() {
        let h = harness();
        let slot = NOW - time::Duration::minutes(1);
        h.jobs.put(job("job-1", Some(slot))).await.unwrap();

        let first = h.scheduler.tick().await.unwrap();
        assert_eq!(first.fired.len(), 1);

        // 把槽位按回原处（模拟"推进没落库就崩了"），再扫一次。
        let mut stored = h
            .jobs
            .get(&CronJobId::from_raw("job-1"))
            .await
            .unwrap()
            .unwrap();
        stored.next_run_at = Some(slot);
        h.jobs.put(stored).await.unwrap();

        let second = h.scheduler.tick().await.unwrap();
        assert!(second.fired.is_empty(), "{second:?}");
        assert_eq!(second.skipped[0].reason, SkipReason::AlreadyFired);
    }

    /// 上一次还没结束 → 跳过本次并记录原因。
    #[tokio::test]
    async fn an_unfinished_previous_firing_skips_this_one() {
        let h = harness();
        h.jobs
            .put(job("job-1", Some(NOW - time::Duration::minutes(1))))
            .await
            .unwrap();
        h.jobs.mark_unfinished(CronJobId::from_raw("job-1"));

        let tick = h.scheduler.tick().await.unwrap();
        assert!(tick.fired.is_empty());
        assert_eq!(tick.skipped[0].reason, SkipReason::Overlap);

        // **记录原因**（§10）：跳过留在触发记录上，不是只留在日志里。`last_error` 不
        // 行——那一列是当前状态，下一次成功触发就会把它清掉，而"昨天那一槽被跳过了"
        // 是一条历史。
        let firings = h
            .jobs
            .firings(&CronJobId::from_raw("job-1"), 10)
            .await
            .unwrap();
        assert_eq!(firings.len(), 1, "{firings:?}");
        assert_eq!(firings[0].status, komo_kernel::cron::FiringStatus::Skipped);
        assert!(
            firings[0].error.as_deref().unwrap().contains("还没结束"),
            "{firings:?}"
        );

        // 槽位照样往前推——跳过的是这一次，不是这个 Job。
        let stored = h
            .jobs
            .get(&CronJobId::from_raw("job-1"))
            .await
            .unwrap()
            .unwrap();
        assert!(stored.next_run_at.unwrap() > NOW);
    }

    /// 投出去的那一条触发记录带着它的 Session / Run，状态是 `queued`——「据此可以补完
    /// 尚未写入的触发输入」（§8.5）。
    #[tokio::test]
    async fn a_fired_firing_carries_its_session_and_run() {
        let h = harness();
        let slot = NOW - time::Duration::minutes(1);
        h.jobs.put(job("job-1", Some(slot))).await.unwrap();

        let tick = h.scheduler.tick().await.unwrap();
        let fired = &tick.fired[0];

        let firings = h
            .jobs
            .firings(&CronJobId::from_raw("job-1"), 10)
            .await
            .unwrap();
        assert_eq!(firings.len(), 1);
        assert_eq!(firings[0].session.as_ref(), Some(&fired.session));
        assert_eq!(firings[0].run.as_ref(), Some(&fired.run));
        assert_eq!(firings[0].status, komo_kernel::cron::FiringStatus::Queued);
        assert_eq!(firings[0].job_version, 1, "记的是触发时那一版");

        // 「更新本次触发状态」（§10）：跑完了记 ok。
        assert!(
            h.scheduler
                .settle(
                    &CronJobId::from_raw("job-1"),
                    slot,
                    komo_kernel::cron::FiringStatus::Ok,
                    None
                )
                .await
                .unwrap()
        );
        let after = h
            .jobs
            .firings(&CronJobId::from_raw("job-1"), 10)
            .await
            .unwrap();
        assert_eq!(after[0].status, komo_kernel::cron::FiringStatus::Ok);
        assert_eq!(after[0].run.as_ref(), Some(&fired.run), "回写状态不丢 Run");
    }

    /// Job 的 `workdir` 跟着这一次触发交给账本（§10）。
    ///
    /// 它**落到哪儿**是 store 的事（`sessions.workdir` 那一列，见
    /// `komo_store::coordinator`）；这里断言的是它真的被带上了——账本拿不到它，
    /// 装配执行段时就只能回到 `workspaces/`，而 Job 的目录就此无声地失效。
    #[tokio::test]
    async fn a_job_workdir_travels_with_its_firing() {
        let h = harness();
        let mut with_dir = job("job-1", Some(NOW - time::Duration::minutes(1)));
        with_dir.workdir = Some(std::path::PathBuf::from("/tmp/komo-cron-ws"));
        h.jobs.put(with_dir).await.unwrap();

        h.scheduler.tick().await.unwrap();
        let inputs = h.ledger.accepted();
        assert_eq!(inputs.len(), 1, "{inputs:?}");
        assert_eq!(
            inputs[0].workdir.as_deref(),
            Some(std::path::Path::new("/tmp/komo-cron-ws"))
        );
    }

    #[tokio::test]
    async fn an_allow_policy_fires_even_when_the_last_one_is_still_running() {
        let h = harness();
        let mut job = job("job-1", Some(NOW - time::Duration::minutes(1)));
        job.overlap = OverlapPolicy::Allow;
        h.jobs.put(job).await.unwrap();
        h.jobs.mark_unfinished(CronJobId::from_raw("job-1"));

        let tick = h.scheduler.tick().await.unwrap();
        assert_eq!(tick.fired.len(), 1, "{tick:?}");
    }

    /// 停机期间错过的触发**不集中补跑**：推进时从此刻往后找，而**错过太久的那一槽
    /// 根本不跑**——超过这个 Job 自己的间隔就算太久（§10）。
    #[tokio::test]
    async fn a_slot_missed_by_days_is_not_made_up() {
        let h = harness();
        // 三天前的槽位，机器一直没开。这个 Job 每天一次，迟到三天就不是"迟到"了。
        let stale = NOW - time::Duration::days(3);
        h.jobs.put(job("job-1", Some(stale))).await.unwrap();

        let tick = h.scheduler.tick().await.unwrap();
        assert!(tick.fired.is_empty(), "不补跑：{tick:?}");
        assert_eq!(tick.skipped[0].reason, SkipReason::TooLate);

        // **跳过留痕**：一条 `skipped` 的触发记录，说得出为什么。
        let firings = h
            .jobs
            .firings(&CronJobId::from_raw("job-1"), 10)
            .await
            .unwrap();
        assert_eq!(firings.len(), 1, "{firings:?}");
        assert_eq!(firings[0].status, komo_kernel::cron::FiringStatus::Skipped);
        assert_eq!(firings[0].scheduled_at, stale);
        assert!(
            firings[0].error.as_deref().unwrap().contains("间隔"),
            "{firings:?}"
        );

        // 槽位照样往前推——跳过的是这一槽，不是这个 Job。
        let stored = h
            .jobs
            .get(&CronJobId::from_raw("job-1"))
            .await
            .unwrap()
            .unwrap();
        let next = stored.next_run_at.unwrap();
        assert!(next > NOW, "下一个槽位在未来：{next}");
        assert_eq!(
            next,
            next_occurrence(&stored.trigger, NOW, &JiffZoneResolver::new())
                .unwrap()
                .unwrap()
        );
    }

    /// 迟到但没超过自己的间隔：照跑一次，只跑一次。
    #[tokio::test]
    async fn a_slot_missed_by_less_than_the_interval_still_runs_once() {
        let h = harness();
        h.jobs
            .put(job("job-1", Some(NOW - time::Duration::hours(2))))
            .await
            .unwrap();

        let tick = h.scheduler.tick().await.unwrap();
        assert_eq!(tick.fired.len(), 1, "{tick:?}");

        // 再扫一次：槽位已经推到未来了，不会有第二次。
        let again = h.scheduler.tick().await.unwrap();
        assert!(again.fired.is_empty(), "{again:?}");
    }

    /// 一次性触发**永不因迟到而作废**：人指定了一个时刻要它跑一次。
    #[tokio::test]
    async fn a_one_shot_still_runs_after_a_long_outage() {
        let h = harness();
        let missed = NOW - time::Duration::days(10);
        let mut one_shot = job("job-1", Some(missed));
        one_shot.trigger = Trigger::At { at: missed };
        h.jobs.put(one_shot).await.unwrap();

        let tick = h.scheduler.tick().await.unwrap();
        assert_eq!(tick.fired.len(), 1, "{tick:?}");
    }

    /// `@at` 是一次性：claim 之后这个 Job 就完成了，行留着当可查询的记录。
    #[tokio::test]
    async fn a_one_shot_completes_at_claim_time() {
        let h = harness();
        let mut one_shot = job("job-1", Some(NOW - time::Duration::minutes(1)));
        one_shot.trigger = Trigger::At {
            at: NOW - time::Duration::minutes(1),
        };
        h.jobs.put(one_shot).await.unwrap();

        let tick = h.scheduler.tick().await.unwrap();
        assert_eq!(tick.fired.len(), 1);

        let stored = h
            .jobs
            .get(&CronJobId::from_raw("job-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, JobStatus::Done);
        assert_eq!(stored.next_run_at, None, "没有下一槽就不会再被找到");
    }

    /// 一个区名解析不了的 Job：跳过它，把原因写进 `last_error`，别的 Job 照跑。
    #[tokio::test]
    async fn a_job_with_a_broken_trigger_does_not_take_the_sweep_down() {
        let h = harness();
        let mut broken = job("job-broken", Some(NOW - time::Duration::minutes(1)));
        broken.trigger = Trigger::Cron {
            expr: "0 9 * * *".into(),
            tz: TimeZone::new("Mars/Olympus_Mons"),
        };
        h.jobs.put(broken).await.unwrap();
        h.jobs
            .put(job("job-ok", Some(NOW - time::Duration::minutes(1))))
            .await
            .unwrap();

        let tick = h.scheduler.tick().await.unwrap();
        assert_eq!(tick.fired.len(), 2, "两个都投出去了：{tick:?}");

        let stored = h
            .jobs
            .get(&CronJobId::from_raw("job-broken"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.next_run_at, None);
        assert!(stored.last_error.is_some(), "问题要在清单里看得见");
    }

    /// **时区明确保存，到期时刻按那个区算**（§10），真 tzdb。
    ///
    /// 两个区各一条：一个没有夏令时（上海恒 +08），一个有（柏林夏令时 +02、冬令时
    /// +01）。同一个"每天 09:00"在两个区里是**三个不同的 UTC 时刻**——把区名当装饰、
    /// 拿 `Utc::now()` 直接算，这三条断言里至少两条会错。
    #[tokio::test]
    async fn a_jobs_due_instant_is_computed_in_its_own_zone() {
        let zones = JiffZoneResolver::new();

        // 上海：09:00 恒等于 UTC 01:00。
        let shanghai =
            parse_schedule("0 9 * * *", &TimeZone::new("Asia/Shanghai"), NOW, &zones).unwrap();
        let next = next_occurrence(&shanghai, datetime!(2026-09-15 08:00:00 UTC), &zones)
            .unwrap()
            .unwrap();
        assert_eq!(
            next.to_offset(time::UtcOffset::UTC),
            datetime!(2026-09-16 01:00:00 UTC)
        );

        // 柏林：夏令时里 09:00 = UTC 07:00。
        let berlin =
            parse_schedule("0 9 * * *", &TimeZone::new("Europe/Berlin"), NOW, &zones).unwrap();
        let summer = next_occurrence(&berlin, datetime!(2026-09-15 08:00:00 UTC), &zones)
            .unwrap()
            .unwrap();
        assert_eq!(
            summer.to_offset(time::UtcOffset::UTC),
            datetime!(2026-09-16 07:00:00 UTC)
        );

        // 同一个 Job，冬令时里 09:00 = UTC 08:00。**同一个表达式、同一个区、不同的
        // UTC 时刻**——这正是"时区只存名字、到期现算"要的。
        let winter = next_occurrence(&berlin, datetime!(2026-11-15 12:00:00 UTC), &zones)
            .unwrap()
            .unwrap();
        assert_eq!(
            winter.to_offset(time::UtcOffset::UTC),
            datetime!(2026-11-16 08:00:00 UTC)
        );
    }

    /// 到期判定走的也是那个区：一个上海的 09:00 Job 在 UTC 01:00 到期，不是 09:00。
    #[tokio::test]
    async fn a_shanghai_job_fires_at_its_own_nine_oclock() {
        let h = harness();
        let mut shanghai = job("job-1", None);
        shanghai
            .advance(NOW, &JiffZoneResolver::new())
            .expect("算得出槽位");
        let slot = shanghai.next_run_at.unwrap();
        assert_eq!(
            slot.to_offset(time::UtcOffset::UTC),
            datetime!(2026-09-16 01:00:00 UTC),
            "上海的 9 点是 UTC 的 1 点"
        );
        h.jobs.put(shanghai).await.unwrap();

        // UTC 09:00 的前一刻还没到期。
        h.clock.set(datetime!(2026-09-16 00:59:00 UTC));
        assert!(h.scheduler.tick().await.unwrap().fired.is_empty());

        h.clock.set(datetime!(2026-09-16 01:00:30 UTC));
        assert_eq!(h.scheduler.tick().await.unwrap().fired.len(), 1);
    }

    /// §10：手动 run 用独立的幂等键，不冒充定时触发。
    #[tokio::test]
    async fn a_manual_run_does_not_pretend_to_be_a_scheduled_firing() {
        let h = harness();
        let slot = NOW + time::Duration::hours(1);
        h.jobs.put(job("job-1", Some(slot))).await.unwrap();

        let fired = h
            .scheduler
            .run_now(&CronJobId::from_raw("job-1"))
            .await
            .unwrap()
            .unwrap();
        assert!(!fired.deduplicated);

        // 没占用那个计划时间：到点了照样触发。
        let firings = h
            .jobs
            .firings(&CronJobId::from_raw("job-1"), 10)
            .await
            .unwrap();
        assert!(firings.is_empty(), "手动 run 不写触发记录：{firings:?}");

        h.clock.set(slot + time::Duration::minutes(1));
        let tick = h.scheduler.tick().await.unwrap();
        assert_eq!(tick.fired.len(), 1);
    }

    #[tokio::test]
    async fn a_job_that_is_not_due_yet_is_left_alone() {
        let h = harness();
        h.jobs
            .put(job("job-1", Some(NOW + time::Duration::hours(1))))
            .await
            .unwrap();
        assert_eq!(h.scheduler.tick().await.unwrap(), CronTick::default());
    }

    /// Job 的模型覆盖按**完整模型配置**解析，不去拼一个模型名和另一个的端点。
    #[tokio::test]
    async fn a_job_model_override_is_a_whole_config() {
        let h = harness();
        let mut with_override = job("job-1", Some(NOW - time::Duration::minutes(1)));
        with_override.model = Some(ModelConfig {
            provider: "openai_responses".into(),
            base_url: "https://other.example.com/v1".into(),
            model: "job-model".into(),
            api_key_env: "OTHER_KEY".into(),
            auth: None,
            effort: None,
            efforts: None,
            timeout_secs: 60,
        });
        with_override.effort = Some(komo_kernel::types::model::Effort::new("high"));
        h.jobs.put(with_override).await.unwrap();

        h.scheduler.tick().await.unwrap();

        // 账本里记下的是这个 Run 固定住的那份配置的模型身份——Job 的那个，不是主模型的。
        let line = h.ledger.to_jsonl();
        assert!(line.contains("job-model"), "{line}");
        assert!(!line.contains("scripted"), "主模型不该混进来：{line}");
    }
}
