//! `CronRepo`：调度器 + Gateway API 共用（§10），**读写目标（`matters`）上的定时列**。
//!
//! 定时并进了目标（`docs/redesign.md` P8）：一个 [`CronJob`] 就是一个带定时的目标，`id` 是目标
//! id、`name` 是目标标题。定时的内容在 `matters.schedule`（JSON，[`ScheduleDef`]），启停、下一次
//! 时间、上次错误是三个独立的列；`next_run_at` 带索引（`matters_next_run`），[`CronRepo::due`]
//! 走它。
//!
//! 两条语义在这里落地：
//!
//! - **运行状态与定时定义都不升目标的 `revision`**，也不碰 `updated_at`：版本号只管内容修改，
//!   调度器推进槽位（[`CronRepo::advance`]）或网页改定时都不能让正在编辑目标的人撞上版本冲突；
//!   反过来，`MatterRepo::put` 也从不写这几列（见那边的注释），两边互不覆盖。定时自己的版本
//!   （`ScheduleDef::version`，进触发快照 `job_version`）仍是「定义改过几次」。
//! - **`claim_firing` 的唯一键是 `job_id + scheduled_at`**（`job_id` 现在存目标 id），已经存在
//!   就返回 `false`——同一计划时间不重复创建运行。它没有写成"先查再插"的两步，而是把这条唯一性
//!   放进**主键**：重复插入就是同一行，并发重复插入就是同一行上的写写冲突，`with_write_retry`
//!   干净重跑一次后看见行已在，答 `false`。

use async_trait::async_trait;
use komo_kernel::cron::{
    CronFiring, CronJob, FiringStatus, JobStatus, NotifyPolicy, OverlapPolicy, Trigger,
};
use komo_kernel::traits::{CronRepo, RepoError, StoreError};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{CronJobId, RunId, SessionId};
use komo_kernel::types::matter::MatterState;
use komo_kernel::types::model::{Effort, ModelConfig};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::db::{
    BoxFuture, Db, decode, encode, from_ts, from_ts_opt, map_toasty, to_ts, to_ts_opt,
};
use crate::models::{CronFiringRow, MatterRow};

/// Turso 上的 [`CronRepo`]。
#[derive(Debug, Clone)]
pub struct TursoCronRepo {
    db: Db,
}

impl TursoCronRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

/// `matters.schedule` 里存的定时定义：[`CronJob`] 去掉 id / 名字（目标自己的）与运行状态
/// （独立的列）。字段与旧 `cron_jobs` 一一对应，一个都没丢。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleDef {
    /// 定义版本：每次 `put` +1（§10）。
    pub version: u64,
    pub trigger: Trigger,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default)]
    pub overlap: OverlapPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    #[serde(default)]
    pub notify: NotifyPolicy,
}

impl ScheduleDef {
    fn of(job: &CronJob) -> ScheduleDef {
        ScheduleDef {
            version: job.version.max(1),
            trigger: job.trigger.clone(),
            prompt: job.prompt.clone(),
            command: job.command.clone(),
            workdir: job.workdir.as_ref().map(|p| p.display().to_string()),
            overlap: job.overlap,
            model: job.model.clone(),
            effort: job.effort.clone(),
            skills: job.skills.clone(),
            max_rounds: job.max_rounds,
            notify: job.notify,
        }
    }
}

/// 唯一键 `job_id + scheduled_at` 的确定性行 ID。
pub fn firing_id(job: &CronJobId, scheduled_at: OffsetDateTime) -> String {
    ContentHash::of_str(&format!("{job}@{}", to_ts(scheduled_at)))
        .as_str()
        .to_string()
}

/// 目标此刻还在追踪：进行中 / 等待中。搁置与结案的目标，定时不触发（`docs/redesign.md`
/// §2.2「状态映射」：搁置 / 完成 / 放弃 → 不再追踪）。
fn tracked(state: &str) -> bool {
    matches!(
        state.parse::<MatterState>(),
        Ok(MatterState::Active | MatterState::Waiting)
    )
}

fn not_a_goal(id: &str) -> StoreError {
    StoreError::NotFound {
        what: format!("目标 {id}"),
    }
}

#[async_trait]
impl CronRepo for TursoCronRepo {
    async fn list(&self) -> Result<Vec<CronJob>, RepoError> {
        self.db
            .read(move |ex| {
                Box::pin(async move {
                    let mut rows = MatterRow::all().exec(ex).await.map_err(map_toasty)?;
                    rows.sort_by(|a, b| a.id.cmp(&b.id));
                    let mut out = Vec::with_capacity(rows.len());
                    for row in &rows {
                        // 一个**存储**的定时若读不出来，跳过并 warn，不要让一行旧 / 新版本写下的
                        // 数据把整张清单带走（§10）。
                        match job_from_row(row) {
                            Ok(Some(job)) => out.push(job),
                            Ok(None) => {}
                            Err(error) => {
                                tracing::warn!(goal = %row.id, %error, "跳过一个读不出来的定时")
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
                    let Some(row) = MatterRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(None);
                    };
                    job_from_row(&row)
                }) as BoxFuture<'_, Result<Option<CronJob>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn put(&self, job: CronJob) -> Result<CronJob, RepoError> {
        self.db
            .with_write_retry(move |ex| {
                let job = job.clone();
                Box::pin(async move {
                    let Some(mut row) = MatterRow::filter_by_id(job.id.as_str())
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Err(not_a_goal(job.id.as_str()));
                    };
                    let mut job = job;
                    // **定时版本递增**——定义改了就是新的一版（§10）。原来没有定时的从 1 起。
                    job.version = match row
                        .schedule
                        .as_deref()
                        .and_then(|raw| decode::<ScheduleDef>(raw, "matters.schedule").ok())
                    {
                        Some(existing) => existing.version + 1,
                        None => job.version.max(1),
                    };
                    job.name = row.title.clone();
                    row.update()
                        .schedule(Some(encode(&ScheduleDef::of(&job))?))
                        .schedule_status(job.status.as_str())
                        .next_run_at(to_ts_opt(job.next_run_at))
                        .last_error(job.last_error.clone())
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
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
        self.db
            .with_write_retry(move |ex| {
                let (id, last_error) = (id.clone(), last_error.clone());
                Box::pin(async move {
                    let Some(mut row) = MatterRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        // 推进一个不存在的定时是错误，不是无声无息。
                        return Err(not_a_goal(&id));
                    };
                    if row.schedule.is_none() {
                        return Err(StoreError::NotFound {
                            what: format!("目标 {id} 的定时"),
                        });
                    }
                    // **只写这三列**：目标的 `revision` / `updated_at` / 定时版本一个字都不动——推进
                    // 槽位不是内容修改，也不是定义变更（§10）。
                    row.update()
                        .next_run_at(to_ts_opt(next_run_at))
                        .schedule_status(status.as_str())
                        .last_error(last_error)
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
                    // 「移除后续调度，已有执行历史保留」（§13.1）——只清定时那几列，目标和
                    // `cron_firings` 都留着。
                    let Some(mut row) = MatterRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(false);
                    };
                    if row.schedule.is_none() {
                        return Ok(false);
                    }
                    row.update()
                        .schedule(None::<String>)
                        .schedule_status(JobStatus::Active.as_str())
                        .next_run_at(0)
                        .last_error(None::<String>)
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    Ok(true)
                }) as BoxFuture<'_, Result<bool, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn due(&self, now: OffsetDateTime) -> Result<Vec<CronJob>, RepoError> {
        self.db
            .read(move |ex| {
                Box::pin(async move {
                    // 走 `matters_next_run` 索引：只取槽位已到的行，再按目标状态与启停筛。
                    let mut rows = MatterRow::filter(
                        MatterRow::fields()
                            .next_run_at()
                            .gt(0)
                            .and(MatterRow::fields().next_run_at().le(to_ts(now))),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    rows.sort_by(|a, b| a.id.cmp(&b.id));
                    let mut out = Vec::new();
                    for row in &rows {
                        if !tracked(&row.state) {
                            continue;
                        }
                        match job_from_row(row) {
                            Ok(Some(job)) if job.is_due(now) => out.push(job),
                            Ok(_) => {}
                            Err(error) => {
                                tracing::warn!(goal = %row.id, %error, "跳过一个读不出来的定时")
                            }
                        }
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<CronJob>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
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

fn job_from_row(row: &MatterRow) -> Result<Option<CronJob>, StoreError> {
    let Some(raw) = &row.schedule else {
        return Ok(None);
    };
    let def: ScheduleDef = decode(raw, "matters.schedule")?;
    Ok(Some(CronJob {
        id: CronJobId::from_raw(row.id.clone()),
        name: row.title.clone(),
        version: def.version,
        trigger: def.trigger,
        // 没写提示词也没写命令：到点做的事取目标的说明（活的，说明改了下一次就跟着改）。
        prompt: if def.prompt.is_empty() && def.command.is_none() {
            row.description.clone().unwrap_or_default()
        } else {
            def.prompt
        },
        command: def.command,
        workdir: def.workdir.map(std::path::PathBuf::from),
        // 一个读不出来的启停读成 `paused`：宁可不响，也不要在不该响的时候响。
        status: JobStatus::parse(&row.schedule_status).unwrap_or(JobStatus::Paused),
        overlap: def.overlap,
        model: def.model,
        effort: def.effort,
        skills: def.skills,
        notify: def.notify,
        max_rounds: def.max_rounds,
        next_run_at: from_ts_opt(row.next_run_at),
        last_error: row.last_error.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repos::matter::TursoMatterRepo;
    use komo_kernel::cron::{JobStatus, OverlapPolicy, TimeZone, Trigger};
    use komo_kernel::traits::MatterRepo;
    use komo_kernel::types::ids::MatterId;
    use komo_kernel::types::matter::{MatterChange, MatterKind, NewMatter, transition};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("aux.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    /// 定时挂在目标上：先要有这个目标。
    async fn goal(db: &Db, id: &str) {
        let matter = NewMatter {
            kind: MatterKind::Area,
            title: "晨间摘要".into(),
            next: None,
            due: None,
            standard: None,
        }
        .build(MatterId::from_raw(id), NOW)
        .unwrap();
        TursoMatterRepo::new(db.clone())
            .put(matter, None)
            .await
            .unwrap();
    }

    fn job(id: &str) -> CronJob {
        CronJob {
            id: CronJobId::from_raw(id),
            name: "忽略：名字是目标的标题".into(),
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

    async fn repo_with(ids: &[&str]) -> (TursoCronRepo, Db, tempfile::TempDir) {
        let (db, dir) = temp().await;
        for id in ids {
            goal(&db, id).await;
        }
        (TursoCronRepo::new(db.clone()), db, dir)
    }

    #[tokio::test]
    async fn a_schedule_round_trips_through_its_goal() {
        let (repo, _db, _dir) = repo_with(&["g-1"]).await;
        let stored = repo.put(job("g-1")).await.unwrap();
        let read = repo
            .get(&CronJobId::from_raw("g-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, stored);
        assert_eq!(read.name, "晨间摘要", "名字是目标的标题");
        assert_eq!(read.skills, vec!["memos".to_string()]);
        assert_eq!(read.max_rounds, Some(12));
        assert_eq!(read.next_run_at, job("g-1").next_run_at);
        // 没有定时的目标不在清单里，也 get 不到。
        goal(&_db, "g-2").await;
        assert_eq!(repo.list().await.unwrap().len(), 1);
        assert!(
            repo.get(&CronJobId::from_raw("g-2"))
                .await
                .unwrap()
                .is_none()
        );
    }

    /// 定时挂在目标上：没有这个目标就不能设。
    #[tokio::test]
    async fn a_schedule_needs_an_existing_goal() {
        let (repo, _db, _dir) = repo_with(&[]).await;
        assert!(matches!(
            repo.put(job("nope")).await,
            Err(RepoError::NotFound { .. })
        ));
    }

    /// **定时版本递增**——定义改了就是新的一版（§10）。
    #[tokio::test]
    async fn every_put_bumps_the_schedule_version() {
        let (repo, _db, _dir) = repo_with(&["g-1"]).await;
        assert_eq!(repo.put(job("g-1")).await.unwrap().version, 1);
        assert_eq!(repo.put(job("g-1")).await.unwrap().version, 2);
        assert_eq!(repo.put(job("g-1")).await.unwrap().version, 3);
    }

    /// 调度器推进槽位：定时版本不动，**目标的 revision 与 updated_at 也不动**。
    #[tokio::test]
    async fn advancing_a_slot_changes_neither_the_schedule_version_nor_the_goals_revision() {
        let (repo, db, _dir) = repo_with(&["g-1"]).await;
        let matters = TursoMatterRepo::new(db.clone());
        let id = CronJobId::from_raw("g-1");
        let stored = repo.put(job("g-1")).await.unwrap();
        let before = matters
            .get(&MatterId::from_raw("g-1"))
            .await
            .unwrap()
            .unwrap();

        let next = NOW + time::Duration::days(1);
        repo.advance(&id, Some(next), JobStatus::Active, None)
            .await
            .unwrap();
        let after = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(after.version, stored.version, "推进槽位不是定义变更");
        assert_eq!(after.next_run_at, Some(next));

        // 一次性触发跑完：状态变 done，槽位清空。
        repo.advance(&id, None, JobStatus::Done, None)
            .await
            .unwrap();
        let done = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(done.status, JobStatus::Done);
        assert!(done.next_run_at.is_none());

        // 触发层面的问题记在 last_error 上。
        repo.advance(&id, None, JobStatus::Active, Some("时区解析不了".into()))
            .await
            .unwrap();
        let broken = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(broken.last_error.as_deref(), Some("时区解析不了"));
        assert_eq!(broken.version, stored.version);

        let now_matter = matters
            .get(&MatterId::from_raw("g-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(now_matter, before, "目标本身一个字都没动（revision 1）");
        assert_eq!(now_matter.revision, 1);

        // 改定义才递增。
        assert_eq!(repo.put(broken).await.unwrap().version, stored.version + 1);
    }

    /// 网页改目标（升版）带着读到的旧状态写回，不会把调度器刚推进的槽位盖回去；反过来
    /// 调度器推进也不会让网页的版本对不上。
    #[tokio::test]
    async fn editing_the_goal_and_advancing_its_schedule_do_not_overwrite_each_other() {
        let (repo, db, _dir) = repo_with(&["g-1"]).await;
        let matters = TursoMatterRepo::new(db.clone());
        let id = CronJobId::from_raw("g-1");
        repo.put(job("g-1")).await.unwrap();
        // 网页先读到目标（revision 1）。
        let read = matters
            .get(&MatterId::from_raw("g-1"))
            .await
            .unwrap()
            .unwrap();

        // 这时调度器推进了槽位。
        let next = NOW + time::Duration::days(1);
        repo.advance(
            &id,
            Some(next),
            JobStatus::Active,
            Some("上次的问题".into()),
        )
        .await
        .unwrap();

        // 网页带着 revision 1 改标题：不冲突。
        let edited = transition(
            &read,
            &MatterChange::Edit {
                title: Some("换个标题".into()),
                due: None,
                standard: None,
                description: Some(Some("新的说明".into())),
            },
            NOW,
        )
        .unwrap()
        .unwrap();
        matters.put(edited, Some(1)).await.unwrap();

        let job = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(job.next_run_at, Some(next), "槽位没被盖回去");
        assert_eq!(job.last_error.as_deref(), Some("上次的问题"));
        assert_eq!(job.name, "换个标题", "名字跟着标题走");
        let matter = matters
            .get(&MatterId::from_raw("g-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(matter.description.as_deref(), Some("新的说明"));
        assert_eq!(matter.revision, 2);
    }

    /// 推进一个不存在 / 没有定时的目标是错误，不是无声无息。
    #[tokio::test]
    async fn advancing_a_schedule_that_is_not_there_is_an_error() {
        let (repo, _db, _dir) = repo_with(&["g-1"]).await;
        for id in ["g-9", "g-1"] {
            assert!(matches!(
                repo.advance(&CronJobId::from_raw(id), None, JobStatus::Active, None)
                    .await,
                Err(RepoError::NotFound { .. })
            ));
        }
    }

    /// 验收：`claim_firing` 的唯一键是 `job + scheduled_at`，重复就是 `false`。
    #[tokio::test]
    async fn the_same_scheduled_time_is_only_claimed_once() {
        let (repo, _db, _dir) = repo_with(&["g-1"]).await;
        repo.put(job("g-1")).await.unwrap();
        let id = CronJobId::from_raw("g-1");

        assert!(repo.claim_firing(firing(&id, NOW)).await.unwrap());
        assert!(!repo.claim_firing(firing(&id, NOW)).await.unwrap());
        assert!(
            repo.claim_firing(firing(&id, NOW + time::Duration::days(1)))
                .await
                .unwrap()
        );
        assert_eq!(repo.firings(&id, 10).await.unwrap().len(), 2);
    }

    /// 定时配置与触发状态是真的列，**重启之后还在**。
    #[tokio::test]
    async fn notify_and_firing_status_survive_a_reopen() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("aux.db");
        let id = CronJobId::from_raw("g-1");

        {
            let db = Db::connect(&path).await.expect("打开库");
            goal(&db, "g-1").await;
            let repo = TursoCronRepo::new(db);
            let mut j = job("g-1");
            j.notify = NotifyPolicy::OnError;
            j.workdir = Some(std::path::PathBuf::from("/tmp"));
            repo.put(j).await.unwrap();
            repo.claim_firing(firing(&id, NOW)).await.unwrap();
            assert!(
                repo.update_firing(CronFiring {
                    session: Some(SessionId::from_raw("sess-1")),
                    run: Some(RunId::from_raw("run-1")),
                    status: FiringStatus::Waiting,
                    error: Some("停在核对上".into()),
                    ..firing(&id, NOW)
                })
                .await
                .unwrap()
            );
        }

        let db = Db::connect(&path).await.expect("重新打开");
        let repo = TursoCronRepo::new(db);
        let read = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(read.notify, NotifyPolicy::OnError);
        assert_eq!(read.workdir, Some(std::path::PathBuf::from("/tmp")));

        let firings = repo.firings(&id, 10).await.unwrap();
        assert_eq!(firings.len(), 1);
        assert_eq!(firings[0].status, FiringStatus::Waiting);
        assert_eq!(firings[0].error.as_deref(), Some("停在核对上"));
        assert_eq!(firings[0].run, Some(RunId::from_raw("run-1")));

        // **重启不重复创建同次触发**，也不覆盖已经记下的那一行。
        assert!(!repo.claim_firing(firing(&id, NOW)).await.unwrap());
        let after = repo.firings(&id, 10).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].status, FiringStatus::Waiting);
    }

    /// 一条不存在的触发记录：`update_firing` 答 `false`，不是错误（手动 run 没有触发记录）。
    #[tokio::test]
    async fn updating_a_firing_that_is_not_there_says_so_without_failing() {
        let (repo, _db, _dir) = repo_with(&[]).await;
        let id = CronJobId::from_raw("g-1");
        assert!(!repo.update_firing(firing(&id, NOW)).await.unwrap());
    }

    /// 并发抢同一个计划时间也只成一次。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_claims_of_one_slot_produce_exactly_one_firing() {
        let (repo, _db, _dir) = repo_with(&["g-1"]).await;
        repo.put(job("g-1")).await.unwrap();
        let id = CronJobId::from_raw("g-1");

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

    /// 调度器读到期的：只有目标还在追踪、定时启用、槽位到了的才是 due。
    #[tokio::test]
    async fn only_tracked_goals_with_an_active_slot_that_has_come_are_due() {
        let (repo, db, _dir) = repo_with(&["g-1", "g-2", "g-3", "g-4", "g-5"]).await;
        repo.put(job("g-1")).await.unwrap();

        let mut paused = job("g-2");
        paused.status = JobStatus::Paused;
        repo.put(paused).await.unwrap();

        let mut later = job("g-3");
        later.next_run_at = Some(NOW + time::Duration::hours(1));
        repo.put(later).await.unwrap();

        // 搁置的目标：不再追踪，定时不触发。
        repo.put(job("g-4")).await.unwrap();
        let matters = TursoMatterRepo::new(db.clone());
        let parked_goal = {
            // g-4 是领域：用结案状态代表「不再追踪」。
            let read = matters
                .get(&MatterId::from_raw("g-4"))
                .await
                .unwrap()
                .unwrap();
            transition(&read, &MatterChange::Retire, NOW)
                .unwrap()
                .unwrap()
        };
        matters.put(parked_goal, Some(1)).await.unwrap();

        // 没有槽位（一次性已完成）。
        let mut finished = job("g-5");
        finished.next_run_at = None;
        finished.status = JobStatus::Done;
        repo.put(finished).await.unwrap();

        let due = repo.due(NOW).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id.as_str(), "g-1");
    }

    /// 「移除后续调度，已有执行历史保留」：清掉定时，目标与触发记录都在。
    #[tokio::test]
    async fn removing_a_schedule_keeps_the_goal_and_its_firings() {
        let (repo, db, _dir) = repo_with(&["g-1"]).await;
        repo.put(job("g-1")).await.unwrap();
        let id = CronJobId::from_raw("g-1");
        repo.claim_firing(firing(&id, NOW)).await.unwrap();

        assert!(repo.remove(&id).await.unwrap());
        assert!(!repo.remove(&id).await.unwrap());
        assert!(repo.get(&id).await.unwrap().is_none());
        assert!(repo.due(NOW).await.unwrap().is_empty());
        assert_eq!(repo.firings(&id, 10).await.unwrap().len(), 1, "历史保留");
        assert!(
            TursoMatterRepo::new(db)
                .get(&MatterId::from_raw("g-1"))
                .await
                .unwrap()
                .is_some()
        );
    }

    /// 一行读不出来的定时只是被跳过，不该把整张清单带走（§10）。
    #[tokio::test]
    async fn a_schedule_that_no_longer_parses_is_skipped_not_fatal() {
        let (repo, db, _dir) = repo_with(&["g-1", "g-2"]).await;
        repo.put(job("g-1")).await.unwrap();
        repo.put(job("g-2")).await.unwrap();

        db.with_write_retry(|ex| {
            Box::pin(async move {
                let mut row = MatterRow::filter_by_id("g-2")
                    .get(&mut *ex)
                    .await
                    .map_err(map_toasty)?;
                row.update()
                    .schedule(Some(
                        r#"{"version":1,"trigger":{"kind":"from_the_future"}}"#.to_string(),
                    ))
                    .exec(&mut *ex)
                    .await
                    .map_err(map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();

        let jobs = repo.list().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id.as_str(), "g-1");
    }
}
