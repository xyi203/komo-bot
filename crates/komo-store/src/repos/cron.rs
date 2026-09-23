//! `CronRepo`：调度器 + CLI + Gateway API 三方共用（§10）。
//!
//! 一条语义在这里落地：**`claim_firing` 的唯一键是 `job_id + scheduled_at`**，已经存在
//! 就返回 `false`——同一计划时间不重复创建运行。它没有写成"先查再插"的两步，而是把这
//! 条唯一性放进**主键**：重复插入就是同一行，并发重复插入就是同一行上的写写冲突，
//! `with_write_retry` 干净重跑一次后看见行已在，答 `false`。

use async_trait::async_trait;
use komo_kernel::cron::{CronFiring, CronJob, FiringStatus, JobStatus, NotifyPolicy};
use komo_kernel::traits::{CronRepo, RepoError, StoreError};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{CronJobId, RunId, SessionId};
use time::OffsetDateTime;

use crate::db::{
    BoxFuture, Db, decode, encode, from_ts, from_ts_opt, map_toasty, to_ts, to_ts_opt,
};
use crate::models::{CronFiringRow, CronJobRow, RunRow};

/// Turso 上的 [`CronRepo`]。
#[derive(Debug, Clone)]
pub struct TursoCronRepo {
    db: Db,
}

impl TursoCronRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// 给一条触发记录补上它产生的 Session / Run。
    pub async fn attach_run(
        &self,
        job: &CronJobId,
        scheduled_at: OffsetDateTime,
        session: &SessionId,
        run: &RunId,
    ) -> Result<(), RepoError> {
        let id = firing_id(job, scheduled_at);
        let (session, run) = (session.to_string(), run.to_string());
        self.db
            .with_write_retry(move |ex| {
                let (id, session, run) = (id.clone(), session.clone(), run.clone());
                Box::pin(async move {
                    let Some(mut row) = CronFiringRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Err(StoreError::NotFound {
                            what: format!("cron firing {id}"),
                        });
                    };
                    row.update()
                        .session_id(Some(session))
                        .run_id(Some(run))
                        .exec(ex)
                        .await
                        .map_err(map_toasty)
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }
}

/// 唯一键 `job_id + scheduled_at` 的确定性行 ID。
pub fn firing_id(job: &CronJobId, scheduled_at: OffsetDateTime) -> String {
    ContentHash::of_str(&format!("{job}@{}", to_ts(scheduled_at)))
        .as_str()
        .to_string()
}

#[async_trait]
impl CronRepo for TursoCronRepo {
    async fn list(&self) -> Result<Vec<CronJob>, RepoError> {
        self.db
            .read(move |ex| {
                Box::pin(async move {
                    let mut rows = CronJobRow::all().exec(ex).await.map_err(map_toasty)?;
                    rows.sort_by(|a, b| a.id.cmp(&b.id));
                    let mut out = Vec::with_capacity(rows.len());
                    for row in &rows {
                        // 一个**存储**的 Job 若触发器解析不出来，跳过并 warn，不要让一行
                        // 旧 / 新版本写下的数据把整张清单带走（§10）。
                        match job_from_row(row) {
                            Ok(job) => out.push(job),
                            Err(error) => {
                                tracing::warn!(job = %row.id, %error, "跳过一个读不出来的 cron job")
                            }
                        }
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<CronJob>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn get(&self, id: &CronJobId) -> Result<Option<CronJob>, RepoError> {
        let id = id.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let Some(row) = CronJobRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(None);
                    };
                    Ok(Some(job_from_row(&row)?))
                }) as BoxFuture<'_, Result<Option<CronJob>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn put(&self, job: CronJob) -> Result<CronJob, RepoError> {
        let now = OffsetDateTime::now_utc();
        self.db
            .with_write_retry(move |ex| {
                let job = job.clone();
                Box::pin(async move {
                    let existing = CronJobRow::filter_by_id(job.id.as_str())
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;

                    let mut job = job;
                    match existing {
                        Some(mut row) => {
                            // **版本递增**——Job 改了，绑定它的授权失效（§7.2）。
                            job.version = (row.version.max(0) as u64) + 1;
                            row.update()
                                .name(job.name.clone())
                                .version(i64::try_from(job.version).unwrap_or(i64::MAX))
                                .trigger(encode(&job.trigger)?)
                                .prompt(job.prompt.clone())
                                .command(job.command.clone())
                                .workdir(job.workdir.as_ref().map(|p| p.display().to_string()))
                                .status(enum_str(&job.status))
                                .overlap(enum_str(&job.overlap))
                                .model(match &job.model {
                                    Some(model) => Some(encode(model)?),
                                    None => None,
                                })
                                .effort(job.effort.as_ref().map(|e| e.to_string()))
                                .skills(encode(&job.skills)?)
                                .max_rounds(i64::from(job.max_rounds.unwrap_or(0)))
                                .notify(job.notify.as_str())
                                .next_run_at(to_ts_opt(job.next_run_at))
                                .last_error(job.last_error.clone())
                                .updated_at(to_ts(now))
                                .exec(ex)
                                .await
                                .map_err(map_toasty)?;
                        }
                        None => {
                            toasty::create!(CronJobRow {
                                id: job.id.as_str(),
                                name: job.name.clone(),
                                version: i64::try_from(job.version.max(1)).unwrap_or(i64::MAX),
                                trigger: encode(&job.trigger)?,
                                prompt: job.prompt.clone(),
                                command: job.command.clone(),
                                workdir: job.workdir.as_ref().map(|p| p.display().to_string()),
                                status: enum_str(&job.status),
                                overlap: enum_str(&job.overlap),
                                model: match &job.model {
                                    Some(model) => Some(encode(model)?),
                                    None => None,
                                },
                                effort: job.effort.as_ref().map(|e| e.to_string()),
                                skills: encode(&job.skills)?,
                                max_rounds: i64::from(job.max_rounds.unwrap_or(0)),
                                notify: job.notify.as_str(),
                                next_run_at: to_ts_opt(job.next_run_at),
                                last_error: job.last_error.clone(),
                                created_at: to_ts(now),
                                updated_at: to_ts(now),
                            })
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                            job.version = job.version.max(1);
                        }
                    }
                    Ok(job)
                }) as BoxFuture<'_, Result<CronJob, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn advance(
        &self,
        id: &CronJobId,
        next_run_at: Option<OffsetDateTime>,
        status: JobStatus,
        last_error: Option<String>,
    ) -> Result<(), RepoError> {
        let id = id.to_string();
        let now = OffsetDateTime::now_utc();
        self.db
            .with_write_retry(move |ex| {
                let (id, last_error) = (id.clone(), last_error.clone());
                Box::pin(async move {
                    let Some(mut row) = CronJobRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        // 推进一个不存在的 Job 是错误，不是无声无息。
                        return Err(StoreError::NotFound {
                            what: format!("cron job {id}"),
                        });
                    };
                    // **`version` 一个字都不动**：推进槽位不是定义变更。走 `put` 的话
                    // 每次触发都会让版本 +1，而 `GrantScope::CronJob` 绑的正是版本
                    // （§7.2、§10），于是一个每天跑的 Job 的授权活不过第一次触发。
                    row.update()
                        .next_run_at(to_ts_opt(next_run_at))
                        .status(enum_str(&status))
                        .last_error(last_error)
                        .updated_at(to_ts(now))
                        .exec(ex)
                        .await
                        .map_err(map_toasty)
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn remove(&self, id: &CronJobId) -> Result<bool, RepoError> {
        let id = id.to_string();
        self.db
            .with_write_retry(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    // 「移除后续调度，已有执行历史保留」（§13.1）——只删 job 行。
                    let Some(row) = CronJobRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(false);
                    };
                    row.delete().exec(ex).await.map_err(map_toasty)?;
                    Ok(true)
                }) as BoxFuture<'_, Result<bool, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn due(&self, now: OffsetDateTime) -> Result<Vec<CronJob>, RepoError> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .filter(|job| job.is_due(now))
            .collect())
    }

    async fn claim_firing(&self, firing: CronFiring) -> Result<bool, RepoError> {
        let now = OffsetDateTime::now_utc();
        self.db
            .with_write_retry(move |ex| {
                let firing = firing.clone();
                Box::pin(async move {
                    let id = firing_id(&firing.job, firing.scheduled_at);
                    if CronFiringRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                        .is_some()
                    {
                        return Ok(false);
                    }
                    toasty::create!(CronFiringRow {
                        id,
                        job_id: firing.job.as_str(),
                        job_version: i64::try_from(firing.job_version).unwrap_or(i64::MAX),
                        scheduled_at: to_ts(firing.scheduled_at),
                        prompt: firing.prompt.clone(),
                        session_id: firing.session.as_ref().map(|s| s.to_string()),
                        run_id: firing.run.as_ref().map(|r| r.to_string()),
                        status: firing.status.as_str(),
                        error: firing.error.clone(),
                        created_at: to_ts(now),
                    })
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    Ok(true)
                }) as BoxFuture<'_, Result<bool, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn update_firing(&self, firing: CronFiring) -> Result<bool, RepoError> {
        self.db
            .with_write_retry(move |ex| {
                let firing = firing.clone();
                Box::pin(async move {
                    let id = firing_id(&firing.job, firing.scheduled_at);
                    let Some(mut row) = CronFiringRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        // 手动 run 没有触发记录：说"没有这一行"，不是报错。
                        return Ok(false);
                    };
                    row.update()
                        .session_id(firing.session.as_ref().map(|s| s.to_string()))
                        .run_id(firing.run.as_ref().map(|r| r.to_string()))
                        .status(firing.status.as_str())
                        .error(firing.error.clone())
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    Ok(true)
                }) as BoxFuture<'_, Result<bool, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn has_unfinished_firing(&self, id: &CronJobId) -> Result<bool, RepoError> {
        let id = id.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let rows =
                        CronFiringRow::filter(CronFiringRow::fields().job_id().eq(id.as_str()))
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                    for row in rows {
                        let Some(run_id) = row.run_id else {
                            // 触发记录已在、Run 还没建出来——这也算"上一次还没结束"。
                            return Ok(true);
                        };
                        let Some(run) = RunRow::filter_by_id(&run_id)
                            .first()
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?
                        else {
                            return Ok(true);
                        };
                        // 「含等待审批、重试或结果核对」都算没结束（§10）——判据就是
                        // §8.4 的"非终态"，与领取语句、reconcile 同一份。
                        if crate::repos::runs::state_of(&run)?.is_unfinished() {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }) as BoxFuture<'_, Result<bool, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn firings(&self, id: &CronJobId, limit: u32) -> Result<Vec<CronFiring>, RepoError> {
        let id = id.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let mut rows =
                        CronFiringRow::filter(CronFiringRow::fields().job_id().eq(id.as_str()))
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                    // 新的在前：问"上一次怎么样"的人要的是最近那次。
                    rows.sort_by_key(|row| std::cmp::Reverse(row.scheduled_at));
                    rows.truncate(limit as usize);
                    Ok(rows
                        .into_iter()
                        .map(|row| CronFiring {
                            job: CronJobId::from_raw(row.job_id),
                            job_version: row.job_version.max(0) as u64,
                            scheduled_at: from_ts(row.scheduled_at),
                            prompt: row.prompt,
                            session: row.session_id.map(SessionId::from_raw),
                            run: row.run_id.map(RunId::from_raw),
                            status: firing_status(&row.status),
                            error: row.error,
                        })
                        .collect::<Vec<_>>())
                }) as BoxFuture<'_, Result<Vec<CronFiring>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }
}

/// 一个读不出来的触发状态读成 `queued`——**不要让一行数据把整张历史带走**（§10 那条
/// 「跳过一个读不出来的 cron job」是同一条理由）。
fn firing_status(raw: &str) -> FiringStatus {
    match raw {
        "ok" => FiringStatus::Ok,
        "error" => FiringStatus::Error,
        "waiting" => FiringStatus::Waiting,
        "skipped" => FiringStatus::Skipped,
        _ => FiringStatus::Queued,
    }
}

fn enum_str<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("这个枚举序列化成一个字符串")
}

fn job_from_row(row: &CronJobRow) -> Result<CronJob, StoreError> {
    Ok(CronJob {
        id: CronJobId::from_raw(row.id.clone()),
        name: row.name.clone(),
        version: row.version.max(0) as u64,
        trigger: decode(&row.trigger, "cron_jobs.trigger")?,
        prompt: row.prompt.clone(),
        command: row.command.clone(),
        workdir: row.workdir.clone().map(std::path::PathBuf::from),
        status: decode(&format!("\"{}\"", row.status), "cron_jobs.status")?,
        overlap: decode(&format!("\"{}\"", row.overlap), "cron_jobs.overlap")?,
        model: match &row.model {
            Some(raw) => Some(decode(raw, "cron_jobs.model")?),
            None => None,
        },
        effort: row
            .effort
            .as_ref()
            .map(komo_kernel::types::model::Effort::new),
        skills: decode(&row.skills, "cron_jobs.skills")?,
        // 一个写不出来的 notify 读成默认的 `always`，不是整行读不出来：投多一条胜过
        // 让一个 Job 从清单里消失。
        notify: NotifyPolicy::parse(&row.notify).unwrap_or_default(),
        max_rounds: (row.max_rounds > 0).then_some(row.max_rounds as u32),
        next_run_at: from_ts_opt(row.next_run_at),
        last_error: row.last_error.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::cron::{JobStatus, OverlapPolicy, TimeZone, Trigger};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    fn job(id: &str) -> CronJob {
        CronJob {
            id: CronJobId::from_raw(id),
            name: "morning-summary".into(),
            version: 1,
            trigger: Trigger::Cron {
                expr: "0 9 * * *".into(),
                tz: TimeZone::new("Asia/Shanghai"),
            },
            prompt: "整理今天的动态".into(),
            command: None,
            workdir: None,
            status: JobStatus::Active,
            overlap: OverlapPolicy::Skip,
            model: None,
            effort: None,
            skills: vec!["memos".into()],
            max_rounds: Some(12),
            notify: Default::default(),
            next_run_at: Some(NOW - time::Duration::minutes(1)),
            last_error: None,
        }
    }

    fn firing(job: &CronJobId, at: OffsetDateTime) -> CronFiring {
        CronFiring {
            job: job.clone(),
            job_version: 1,
            scheduled_at: at,
            prompt: "整理今天的动态".into(),
            session: None,
            run: None,
            status: Default::default(),
            error: None,
        }
    }

    #[tokio::test]
    async fn a_job_round_trips_through_the_row() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db);
        let stored = repo.put(job("job-1")).await.unwrap();
        let read = repo
            .get(&CronJobId::from_raw("job-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, stored);
        assert_eq!(read.skills, vec!["memos".to_string()]);
        assert_eq!(read.max_rounds, Some(12));
        assert_eq!(read.next_run_at, job("job-1").next_run_at);
    }

    /// **版本递增**——Job 改了，绑定它的授权失效（§7.2）。
    #[tokio::test]
    async fn every_put_bumps_the_version() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db);
        assert_eq!(repo.put(job("job-1")).await.unwrap().version, 1);
        assert_eq!(repo.put(job("job-1")).await.unwrap().version, 2);
        assert_eq!(repo.put(job("job-1")).await.unwrap().version, 3);
    }

    /// 验收 ⑤：`advance` **不碰 version**，`put` 才递增。
    ///
    /// 走 `put` 推进槽位的话，每次触发都会让版本 +1，而 `GrantScope::CronJob` 绑的正是
    /// 版本（§7.2、§10）——于是一个每天跑的 Job 的授权活不过第一次触发。
    #[tokio::test]
    async fn advancing_a_slot_does_not_invalidate_the_jobs_grants() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db);
        let stored = repo.put(job("job-1")).await.unwrap();
        assert_eq!(stored.version, 1);

        let next = NOW + time::Duration::days(1);
        repo.advance(&stored.id, Some(next), JobStatus::Active, None)
            .await
            .unwrap();
        let after = repo.get(&stored.id).await.unwrap().unwrap();
        assert_eq!(after.version, 1, "推进槽位不是定义变更");
        assert_eq!(after.next_run_at, Some(next));
        assert_eq!(after.status, JobStatus::Active);

        // 一次性触发跑完了：状态变 done，槽位清空，版本还是不动。
        repo.advance(&stored.id, None, JobStatus::Done, None)
            .await
            .unwrap();
        let done = repo.get(&stored.id).await.unwrap().unwrap();
        assert_eq!(done.version, 1);
        assert_eq!(done.status, JobStatus::Done);
        assert!(done.next_run_at.is_none());

        // 触发层面的问题记在 last_error 上——一个区名解析不了的 Job 要在清单里看得见。
        repo.advance(
            &stored.id,
            None,
            JobStatus::Active,
            Some("时区解析不了".into()),
        )
        .await
        .unwrap();
        let broken = repo.get(&stored.id).await.unwrap().unwrap();
        assert_eq!(broken.last_error.as_deref(), Some("时区解析不了"));
        assert_eq!(broken.version, 1);

        // 改定义才递增——旧授权就该在这一刻失效。
        assert_eq!(repo.put(broken).await.unwrap().version, 2);
    }

    /// 推进一个不存在的 Job 是错误，不是无声无息。
    #[tokio::test]
    async fn advancing_a_job_that_is_not_there_is_an_error() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db);
        assert!(matches!(
            repo.advance(&CronJobId::from_raw("job-9"), None, JobStatus::Active, None)
                .await,
            Err(RepoError::NotFound { .. })
        ));
    }

    /// 验收：`claim_firing` 的唯一键是 `job + scheduled_at`，重复就是 `false`。
    #[tokio::test]
    async fn the_same_scheduled_time_is_only_claimed_once() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db);
        repo.put(job("job-1")).await.unwrap();
        let id = CronJobId::from_raw("job-1");

        assert!(repo.claim_firing(firing(&id, NOW)).await.unwrap());
        assert!(!repo.claim_firing(firing(&id, NOW)).await.unwrap());
        // 另一个计划时间是另一次触发。
        assert!(
            repo.claim_firing(firing(&id, NOW + time::Duration::days(1)))
                .await
                .unwrap()
        );
        assert_eq!(repo.firings(&id, 10).await.unwrap().len(), 2);
    }

    /// `notify` 与触发状态是真的列，不是内存里的东西——**重启之后还在**。
    #[tokio::test]
    async fn notify_and_firing_status_survive_a_reopen() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("state.db");
        let id = CronJobId::from_raw("job-1");

        {
            let db = Db::connect(&path).await.expect("打开库");
            let repo = TursoCronRepo::new(db);
            let mut j = job("job-1");
            j.notify = NotifyPolicy::OnError;
            j.workdir = Some(std::path::PathBuf::from("/tmp"));
            repo.put(j).await.unwrap();
            repo.claim_firing(firing(&id, NOW)).await.unwrap();
            assert!(
                repo.update_firing(CronFiring {
                    session: Some(SessionId::from_raw("sess-1")),
                    run: Some(RunId::from_raw("run-1")),
                    status: FiringStatus::Waiting,
                    error: Some("停在审批上".into()),
                    ..firing(&id, NOW)
                })
                .await
                .unwrap()
            );
        }

        // 同一个数据目录再打开一次 = 重启。
        let db = Db::connect(&path).await.expect("重新打开");
        let repo = TursoCronRepo::new(db);
        let read = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(read.notify, NotifyPolicy::OnError);
        assert_eq!(read.workdir, Some(std::path::PathBuf::from("/tmp")));

        let firings = repo.firings(&id, 10).await.unwrap();
        assert_eq!(firings.len(), 1);
        assert_eq!(firings[0].status, FiringStatus::Waiting);
        assert_eq!(firings[0].error.as_deref(), Some("停在审批上"));
        assert_eq!(firings[0].run, Some(RunId::from_raw("run-1")));

        // **重启不重复创建同次触发**（§14 阶段 7 第一句）：同一个 `scheduled_at`
        // 第二次 claim 仍然是 `false`，而且**不覆盖**已经记下的那一行。
        assert!(!repo.claim_firing(firing(&id, NOW)).await.unwrap());
        let after = repo.firings(&id, 10).await.unwrap();
        assert_eq!(after.len(), 1, "还是一条");
        assert_eq!(after[0].status, FiringStatus::Waiting, "没有被盖回 queued");
        assert_eq!(after[0].run, Some(RunId::from_raw("run-1")));
    }

    /// 一条不存在的触发记录：`update_firing` 答 `false`，不是错误。
    ///
    /// 手动 run 本来就没有触发记录（§10：不冒充定时触发），所以"没有这一行"是常态。
    #[tokio::test]
    async fn updating_a_firing_that_is_not_there_says_so_without_failing() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db);
        let id = CronJobId::from_raw("job-1");
        assert!(!repo.update_firing(firing(&id, NOW)).await.unwrap());
    }

    /// 并发抢同一个计划时间也只成一次。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_claims_of_one_slot_produce_exactly_one_firing() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db);
        repo.put(job("job-1")).await.unwrap();
        let id = CronJobId::from_raw("job-1");

        let mut handles = Vec::new();
        for _ in 0..4 {
            let repo = repo.clone();
            let firing = firing(&id, NOW);
            handles.push(tokio::spawn(async move { repo.claim_firing(firing).await }));
        }
        let mut wins = 0;
        for handle in handles {
            if handle.await.unwrap().unwrap() {
                wins += 1;
            }
        }
        assert_eq!(wins, 1, "同一计划时间不重复创建运行");
        assert_eq!(repo.firings(&id, 10).await.unwrap().len(), 1);
    }

    /// 「含等待审批、重试或结果核对」都算没结束（§10）。
    #[tokio::test]
    async fn a_firing_without_a_finished_run_counts_as_unfinished() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db.clone());
        repo.put(job("job-1")).await.unwrap();
        let id = CronJobId::from_raw("job-1");

        assert!(!repo.has_unfinished_firing(&id).await.unwrap());
        repo.claim_firing(firing(&id, NOW)).await.unwrap();
        assert!(
            repo.has_unfinished_firing(&id).await.unwrap(),
            "触发记录已在、Run 还没建出来，也算没结束"
        );
    }

    /// 只有 active 且槽位到期的才是 due。
    #[tokio::test]
    async fn only_active_jobs_whose_slot_has_come_are_due() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db);
        repo.put(job("job-1")).await.unwrap();

        let mut paused = job("job-2");
        paused.status = JobStatus::Paused;
        repo.put(paused).await.unwrap();

        let mut later = job("job-3");
        later.next_run_at = Some(NOW + time::Duration::hours(1));
        repo.put(later).await.unwrap();

        let due = repo.due(NOW).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id.as_str(), "job-1");
    }

    /// 「移除后续调度，已有执行历史保留」。
    #[tokio::test]
    async fn removing_a_job_keeps_its_firings() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db);
        repo.put(job("job-1")).await.unwrap();
        let id = CronJobId::from_raw("job-1");
        repo.claim_firing(firing(&id, NOW)).await.unwrap();

        assert!(repo.remove(&id).await.unwrap());
        assert!(!repo.remove(&id).await.unwrap());
        assert!(repo.get(&id).await.unwrap().is_none());
        assert_eq!(repo.firings(&id, 10).await.unwrap().len(), 1, "历史保留");
    }

    /// 触发记录事后能指回它产生的 Session / Run。
    #[tokio::test]
    async fn a_firing_can_be_linked_to_the_run_it_produced() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db);
        repo.put(job("job-1")).await.unwrap();
        let id = CronJobId::from_raw("job-1");
        repo.claim_firing(firing(&id, NOW)).await.unwrap();

        repo.attach_run(
            &id,
            NOW,
            &SessionId::from_raw("sess-1"),
            &RunId::from_raw("run-1"),
        )
        .await
        .unwrap();

        let firings = repo.firings(&id, 10).await.unwrap();
        assert_eq!(firings[0].session.as_ref().unwrap().as_str(), "sess-1");
        assert_eq!(firings[0].run.as_ref().unwrap().as_str(), "run-1");
    }

    /// 一行读不出来的 Job 只是被跳过，不该把整张清单带走（§10）。
    #[tokio::test]
    async fn a_job_whose_trigger_no_longer_parses_is_skipped_not_fatal() {
        let (db, _dir) = temp().await;
        let repo = TursoCronRepo::new(db.clone());
        repo.put(job("job-1")).await.unwrap();
        repo.put(job("job-2")).await.unwrap();

        db.with_write_retry(|ex| {
            Box::pin(async move {
                let mut row = CronJobRow::filter_by_id("job-2")
                    .get(&mut *ex)
                    .await
                    .map_err(map_toasty)?;
                row.update()
                    .trigger(r#"{"kind":"from_the_future"}"#)
                    .exec(&mut *ex)
                    .await
                    .map_err(map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();

        let jobs = repo.list().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id.as_str(), "job-1");
    }
}
