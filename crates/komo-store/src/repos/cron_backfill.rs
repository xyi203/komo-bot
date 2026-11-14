//! 回填：存量定时任务（`cron_jobs`）→ 目标（`matters`）上的定时（`docs/redesign.md` P8、§3）。
//!
//! Gateway 启动时跑一次（[`backfill_cron_into_goals`]），**可重入**：跑第二遍什么都不改。
//! 整个回填在一个事务里（`with_write_retry`），要么全做要么全不做。
//!
//! 规则：
//!
//! - **没有链到任何目标的定时任务** → 新建一个目标，**目标 id 直接用任务 id**，所以它名下的
//!   `cron_firings` 不用改键。目标是「领域」（没有终点；已完成的一次性任务记成已不再管），标题
//!   用任务名，说明取提示词（命令任务取命令），定时配置与运行状态照搬。
//! - **已经链到某个目标的定时任务**（目标的 `triggers` 里挂着 `cron`）→ 配置并入那个目标，
//!   它的 `cron_firings` 改挂到目标 id（主键是 `job_id@scheduled_at` 的哈希，所以是新建一行、
//!   删旧一行）。一个目标链了多个任务时，**第一个并入，其余各自成为独立的新目标**；「第一个」
//!   = 先按目标 id 升序、再按这个目标 `triggers` 里挂上的先后，第一个存在、且那个目标还没有定时
//!   的任务（所以结果与启动次数、读取顺序无关）。
//! - 并完之后所有目标上挂着的 `cron` 触发器一律摘掉（定时现在是目标自己的配置，不再是引用），
//!   摘了的目标升一版。
//! - `cron_jobs` 表**不读之外的任何操作**：不删行、不改行。「这个任务处理过没有」记在目标上
//!   （`matters.migrated_from`），所以之后目标的定时被清掉也不会再造出一个目标。
//!
//! 字段按原样搬（JSON 文本不解码再编码），读不出来的字段原样带过去，由读的一侧
//! （`TursoCronRepo`）按老办法跳过并 warn。

use std::collections::{BTreeMap, BTreeSet};

use komo_kernel::cron::JobStatus;
use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::CronJobId;
use komo_kernel::types::matter::MatterTrigger;
use serde_json::{Value, json};

use crate::db::{BoxFuture, Db, from_ts, map_toasty};
use crate::models::{CronFiringRow, CronJobRow, MatterRow};
use crate::repos::cron::firing_id;

/// 一个旧定时任务落到了哪里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// 新建了一个目标（id = 任务 id）。
    NewGoal,
    /// 并入了这个已有目标。
    MergedInto(String),
}

/// 一次回填做了什么。第二遍跑应当 [`BackfillReport::is_noop`]。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// 这一遍新处理的任务：`(任务 id, 去向)`，按任务 id 升序。
    pub placed: Vec<(String, Placement)>,
    /// 改挂了目标的触发记录条数。
    pub firings_rekeyed: usize,
    /// 摘掉了 `cron` 触发器的目标 id。
    pub triggers_stripped: Vec<String>,
}

impl BackfillReport {
    pub fn is_noop(&self) -> bool {
        self.placed.is_empty() && self.firings_rekeyed == 0 && self.triggers_stripped.is_empty()
    }
}

/// 启动时的回填入口（可重入）。
pub async fn backfill_cron_into_goals(db: &Db) -> Result<BackfillReport, StoreError> {
    db.with_write_retry(|ex| {
        Box::pin(async move {
            let mut jobs = CronJobRow::all().exec(&mut *ex).await.map_err(map_toasty)?;
            jobs.sort_by(|a, b| a.id.cmp(&b.id));
            let mut matters = MatterRow::all().exec(&mut *ex).await.map_err(map_toasty)?;
            matters.sort_by(|a, b| a.id.cmp(&b.id));

            let mut report = BackfillReport::default();
            let job_by_id: BTreeMap<&str, &CronJobRow> =
                jobs.iter().map(|job| (job.id.as_str(), job)).collect();

            // 已经处理过的任务：有目标的 id 就是它，或有目标记着回填自它。
            let mut done: BTreeSet<String> = BTreeSet::new();
            for matter in &matters {
                if job_by_id.contains_key(matter.id.as_str()) {
                    done.insert(matter.id.clone());
                }
                if let Some(from) = &matter.migrated_from {
                    done.insert(from.clone());
                }
            }

            // 目标 → 它的 triggers（一次解出来；读不出来的不动它）。
            let mut triggers: BTreeMap<String, Vec<MatterTrigger>> = BTreeMap::new();
            for matter in &matters {
                match serde_json::from_str::<Vec<MatterTrigger>>(&matter.triggers) {
                    Ok(list) => {
                        triggers.insert(matter.id.clone(), list);
                    }
                    Err(error) => tracing::warn!(
                        goal = %matter.id, %error, "目标的触发器读不出来，回填跳过它的 cron 链接"
                    ),
                }
            }

            // 1. 并入：目标 id 升序 → 它的触发器先后 → 第一个存在、还没处理、目标还没有定时的任务。
            let mut has_schedule: BTreeSet<String> = matters
                .iter()
                .filter(|m| m.schedule.is_some())
                .map(|m| m.id.clone())
                .collect();
            let mut merges: Vec<(String, String)> = Vec::new(); // (job, goal)
            for matter in &matters {
                if has_schedule.contains(&matter.id) || matter.migrated_from.is_some() {
                    continue;
                }
                let Some(list) = triggers.get(&matter.id) else {
                    continue;
                };
                for trigger in list {
                    let MatterTrigger::Cron { job } = trigger else {
                        continue;
                    };
                    let job = job.as_str();
                    if done.contains(job) || !job_by_id.contains_key(job) {
                        continue;
                    }
                    merges.push((job.to_string(), matter.id.clone()));
                    done.insert(job.to_string());
                    has_schedule.insert(matter.id.clone());
                    break;
                }
            }
            let mut rows: BTreeMap<String, MatterRow> =
                matters.into_iter().map(|m| (m.id.clone(), m)).collect();
            for (job_id, goal_id) in &merges {
                let job = job_by_id[job_id.as_str()];
                let goal = rows.get_mut(goal_id).expect("目标在表里");
                goal.update()
                    .schedule(Some(schedule_json(job)))
                    .schedule_status(job.status.clone())
                    .next_run_at(job.next_run_at)
                    .last_error(job.last_error.clone())
                    .migrated_from(Some(job.id.clone()))
                    .exec(&mut *ex)
                    .await
                    .map_err(map_toasty)?;
                report
                    .placed
                    .push((job_id.clone(), Placement::MergedInto(goal_id.clone())));
                report.firings_rekeyed += rekey_firings(&mut *ex, job_id, goal_id).await?;
            }

            // 2. 其余没处理过的任务各成一个独立的目标。
            for job in &jobs {
                if done.contains(&job.id) {
                    continue;
                }
                create_goal(&mut *ex, job).await?;
                report.placed.push((job.id.clone(), Placement::NewGoal));
            }

            // 3. 摘掉所有目标上的 cron 触发器（回填并入的、独立的、悬空的都摘）。
            for (goal_id, list) in &triggers {
                if !list
                    .iter()
                    .any(|trigger| matches!(trigger, MatterTrigger::Cron { .. }))
                {
                    continue;
                }
                let kept: Vec<&MatterTrigger> = list
                    .iter()
                    .filter(|trigger| !matches!(trigger, MatterTrigger::Cron { .. }))
                    .collect();
                let goal = rows.get_mut(goal_id).expect("目标在表里");
                let revision = goal.revision + 1;
                goal.update()
                    .revision(revision)
                    .triggers(serde_json::to_string(&kept).expect("触发器可序列化"))
                    .exec(&mut *ex)
                    .await
                    .map_err(map_toasty)?;
                report.triggers_stripped.push(goal_id.clone());
            }
            report.placed.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(report)
        }) as BoxFuture<'_, Result<BackfillReport, StoreError>>
    })
    .await
}

/// 旧任务的调度与执行配置，拼成 `matters.schedule` 的 JSON（字段名同 `ScheduleDef`）。
fn schedule_json(job: &CronJobRow) -> String {
    fn raw(text: &str) -> Value {
        serde_json::from_str(text).unwrap_or(Value::Null)
    }
    let mut def = json!({
        "version": job.version.max(1),
        "trigger": raw(&job.trigger),
        "prompt": job.prompt,
        "overlap": job.overlap,
        "skills": serde_json::from_str::<Value>(&job.skills).unwrap_or_else(|_| json!([])),
        "notify": job.notify,
    });
    let map = def.as_object_mut().expect("是对象");
    if let Some(command) = &job.command {
        map.insert("command".into(), json!(command));
    }
    if let Some(workdir) = &job.workdir {
        map.insert("workdir".into(), json!(workdir));
    }
    if let Some(model) = &job.model {
        map.insert("model".into(), raw(model));
    }
    if let Some(effort) = &job.effort {
        map.insert("effort".into(), json!(effort));
    }
    if job.max_rounds > 0 {
        map.insert("max_rounds".into(), json!(job.max_rounds));
    }
    def.to_string()
}

async fn create_goal(ex: &mut dyn toasty::Executor, job: &CronJobRow) -> Result<(), StoreError> {
    let finished = JobStatus::parse(&job.status) == Some(JobStatus::Done);
    let description = match (&job.command, job.prompt.trim()) {
        (Some(command), _) => Some(command.clone()),
        (None, "") => None,
        (None, prompt) => Some(prompt.to_string()),
    };
    toasty::create!(MatterRow {
        id: job.id.clone(),
        revision: 1,
        // 定时任务没有终点：复用「领域」。
        kind: "area",
        title: job.name.clone(),
        // 跑完了的一次性任务 = 不再管（领域的结案状态）。
        state: if finished { "retired" } else { "active" },
        next: None::<String>,
        waiting_for: None::<String>,
        due: None::<String>,
        standard: None::<String>,
        evidence: "[]",
        created_at: job.created_at,
        updated_at: job.updated_at,
        closed_at: if finished { job.updated_at } else { 0 },
        triggers: "[]",
        description: description,
        schedule: Some(schedule_json(job)),
        schedule_status: job.status.clone(),
        next_run_at: job.next_run_at,
        last_error: job.last_error.clone(),
        migrated_from: Some(job.id.clone()),
        chat_session: None::<String>,
    })
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    Ok(())
}

/// 把 `job` 名下的触发记录改挂到 `goal`：主键是 `job_id@scheduled_at` 的哈希，所以新建一行再删旧行。
async fn rekey_firings(
    ex: &mut dyn toasty::Executor,
    job: &str,
    goal: &str,
) -> Result<usize, StoreError> {
    if job == goal {
        return Ok(0);
    }
    let rows = CronFiringRow::filter(CronFiringRow::fields().job_id().eq(job))
        .exec(&mut *ex)
        .await
        .map_err(map_toasty)?;
    let goal_id = CronJobId::from_raw(goal);
    let mut moved = 0;
    for row in rows {
        let id = firing_id(&goal_id, from_ts(row.scheduled_at));
        let exists = CronFiringRow::filter_by_id(&id)
            .first()
            .exec(&mut *ex)
            .await
            .map_err(map_toasty)?
            .is_some();
        if !exists {
            toasty::create!(CronFiringRow {
                id: id,
                job_id: goal,
                job_version: row.job_version,
                scheduled_at: row.scheduled_at,
                prompt: row.prompt.clone(),
                session_id: row.session_id.clone(),
                run_id: row.run_id.clone(),
                status: row.status.clone(),
                error: row.error.clone(),
                created_at: row.created_at,
            })
            .exec(&mut *ex)
            .await
            .map_err(map_toasty)?;
        }
        row.delete().exec(&mut *ex).await.map_err(map_toasty)?;
        moved += 1;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TursoCronRepo;
    use crate::repos::matter::TursoMatterRepo;
    use komo_kernel::cron::{FiringStatus, JobStatus};
    use komo_kernel::traits::{CronRepo, MatterRepo};
    use komo_kernel::types::ids::MatterId;
    use komo_kernel::types::matter::{
        MatterChange, MatterKind, MatterState, NewMatter, transition,
    };
    use time::OffsetDateTime;
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-10-29 08:00:00 UTC);

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("aux.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    async fn insert_job(db: &Db, id: &str, name: &str, status: &str, next_run_at: i64) {
        let (id, name, status) = (id.to_string(), name.to_string(), status.to_string());
        db.with_write_retry(move |ex| {
            let (id, name, status) = (id.clone(), name.clone(), status.clone());
            Box::pin(async move {
                toasty::create!(CronJobRow {
                    id: id,
                    name: name,
                    version: 3,
                    trigger: r#"{"kind":"cron","expr":"0 9 * * *","tz":"Asia/Shanghai"}"#,
                    prompt: "整理今天的动态",
                    command: None::<String>,
                    workdir: None::<String>,
                    status: status,
                    overlap: "skip",
                    model: None::<String>,
                    effort: None::<String>,
                    skills: r#"["memos"]"#,
                    max_rounds: 12,
                    notify: "on_error",
                    next_run_at: next_run_at,
                    last_error: None::<String>,
                    created_at: 100,
                    updated_at: 200,
                })
                .exec(ex)
                .await
                .map_err(map_toasty)?;
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
    }

    async fn insert_firing(db: &Db, job: &str, at: i64) {
        let (job, id) = (
            job.to_string(),
            firing_id(&CronJobId::from_raw(job), from_ts(at)),
        );
        db.with_write_retry(move |ex| {
            let (job, id) = (job.clone(), id.clone());
            Box::pin(async move {
                toasty::create!(CronFiringRow {
                    id: id,
                    job_id: job,
                    job_version: 3,
                    scheduled_at: at,
                    prompt: "整理今天的动态",
                    session_id: Some("sess-1".to_string()),
                    run_id: None::<String>,
                    status: "ok",
                    error: None::<String>,
                    created_at: at,
                })
                .exec(ex)
                .await
                .map_err(map_toasty)?;
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
    }

    /// 一个挂着 `cron` 触发器的目标。
    async fn goal_linking(db: &Db, id: &str, jobs: &[&str]) {
        let repo = TursoMatterRepo::new(db.clone());
        let mut matter = NewMatter {
            kind: MatterKind::Goal,
            title: format!("目标 {id}"),
            next: None,
            due: None,
            standard: None,
        }
        .build(MatterId::from_raw(id), T0)
        .unwrap();
        repo.put(matter.clone(), None).await.unwrap();
        for job in jobs {
            matter = transition(
                &matter,
                &MatterChange::Link(MatterTrigger::cron(CronJobId::from_raw(*job))),
                T0,
            )
            .unwrap()
            .unwrap();
            repo.put(matter.clone(), Some(matter.revision - 1))
                .await
                .unwrap();
        }
    }

    async fn counts(db: &Db) -> (usize, usize, usize) {
        db.read(|ex| {
            Box::pin(async move {
                let jobs = CronJobRow::all().exec(&mut *ex).await.map_err(map_toasty)?;
                let matters = MatterRow::all().exec(&mut *ex).await.map_err(map_toasty)?;
                let firings = CronFiringRow::all()
                    .exec(&mut *ex)
                    .await
                    .map_err(map_toasty)?;
                Ok((jobs.len(), matters.len(), firings.len()))
            }) as BoxFuture<'_, Result<(usize, usize, usize), StoreError>>
        })
        .await
        .unwrap()
    }

    /// 没有链到任何目标的任务 → 一个新目标，id 就是任务 id，所以 firing 不用改键。
    #[tokio::test]
    async fn an_unlinked_job_becomes_a_goal_with_its_own_id() {
        let (db, _dir) = temp().await;
        insert_job(&db, "job-1", "晨间摘要", "active", 5_000).await;
        insert_firing(&db, "job-1", 1_000).await;

        let report = backfill_cron_into_goals(&db).await.unwrap();
        assert_eq!(
            report.placed,
            vec![("job-1".to_string(), Placement::NewGoal)]
        );
        assert_eq!(report.firings_rekeyed, 0);

        let matters = TursoMatterRepo::new(db.clone());
        let goal = matters
            .get(&MatterId::from_raw("job-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(goal.kind, MatterKind::Area, "没有终点：复用领域");
        assert_eq!(goal.title, "晨间摘要");
        assert_eq!(goal.state, MatterState::Active);
        assert_eq!(goal.description.as_deref(), Some("整理今天的动态"));

        let cron = TursoCronRepo::new(db.clone());
        let job = cron
            .get(&CronJobId::from_raw("job-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.name, "晨间摘要");
        assert_eq!(job.version, 3);
        assert_eq!(job.skills, vec!["memos".to_string()]);
        assert_eq!(job.max_rounds, Some(12));
        assert_eq!(job.notify, komo_kernel::cron::NotifyPolicy::OnError);
        assert_eq!(job.status, JobStatus::Active);
        assert_eq!(job.next_run_at, Some(from_ts(5_000)));
        // firing 的键没动：还是同一个 id、同一个 job_id。
        assert_eq!(
            cron.firings(&CronJobId::from_raw("job-1"), 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// 暂停的任务保持暂停；跑完的一次性任务成为「不再管」的领域。
    #[tokio::test]
    async fn paused_and_finished_jobs_keep_their_state() {
        let (db, _dir) = temp().await;
        insert_job(&db, "job-1", "暂停的", "paused", 5_000).await;
        insert_job(&db, "job-2", "跑完的", "done", 0).await;
        backfill_cron_into_goals(&db).await.unwrap();

        let cron = TursoCronRepo::new(db.clone());
        let paused = cron
            .get(&CronJobId::from_raw("job-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(paused.status, JobStatus::Paused);
        let matters = TursoMatterRepo::new(db.clone());
        let finished = matters
            .get(&MatterId::from_raw("job-2"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(finished.state, MatterState::Retired);
        assert!(finished.closed_at.is_some());
        assert_eq!(
            cron.get(&CronJobId::from_raw("job-2"))
                .await
                .unwrap()
                .unwrap()
                .status,
            JobStatus::Done
        );
    }

    /// 已经链到目标的任务并入那个目标，firing 改挂目标 id，目标上的 cron 触发器摘掉。
    #[tokio::test]
    async fn a_linked_job_merges_into_its_goal_and_its_firings_follow() {
        let (db, _dir) = temp().await;
        insert_job(&db, "job-1", "晨间摘要", "active", 5_000).await;
        insert_firing(&db, "job-1", 1_000).await;
        insert_firing(&db, "job-1", 2_000).await;
        goal_linking(&db, "goal-1", &["job-1"]).await;

        let report = backfill_cron_into_goals(&db).await.unwrap();
        assert_eq!(
            report.placed,
            vec![(
                "job-1".to_string(),
                Placement::MergedInto("goal-1".to_string())
            )]
        );
        assert_eq!(report.firings_rekeyed, 2);
        assert_eq!(report.triggers_stripped, vec!["goal-1".to_string()]);

        let cron = TursoCronRepo::new(db.clone());
        assert!(
            cron.get(&CronJobId::from_raw("job-1"))
                .await
                .unwrap()
                .is_none(),
            "没有再造一个以任务 id 为 id 的目标"
        );
        let job = cron
            .get(&CronJobId::from_raw("goal-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.name, "目标 goal-1", "标题是目标自己的");
        assert_eq!(job.prompt, "整理今天的动态");
        assert_eq!(
            cron.firings(&CronJobId::from_raw("goal-1"), 10)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            cron.firings(&CronJobId::from_raw("job-1"), 10)
                .await
                .unwrap()
                .is_empty()
        );
        // 改挂之后同一个计划时间仍然只能占一次（主键是新 id 的哈希）。
        let repeat = komo_kernel::cron::CronFiring {
            job: CronJobId::from_raw("goal-1"),
            job_version: 3,
            scheduled_at: from_ts(1_000),
            prompt: String::new(),
            session: None,
            run: None,
            status: FiringStatus::Queued,
            error: None,
        };
        assert!(!cron.claim_firing(repeat).await.unwrap());

        let matters = TursoMatterRepo::new(db.clone());
        let goal = matters
            .get(&MatterId::from_raw("goal-1"))
            .await
            .unwrap()
            .unwrap();
        assert!(goal.triggers.is_empty());
        assert_eq!(goal.revision, 3, "挂上 +1，摘掉 +1");
    }

    /// 一个目标链了多个任务：先挂的并入，其余各成独立目标（firing 不改键）。
    #[tokio::test]
    async fn only_the_first_of_several_linked_jobs_merges() {
        let (db, _dir) = temp().await;
        insert_job(&db, "job-a", "甲", "active", 5_000).await;
        insert_job(&db, "job-b", "乙", "active", 6_000).await;
        insert_firing(&db, "job-b", 1_000).await;
        // 触发器顺序是 b、a：b 先挂，所以 b 并入，不是按任务 id。
        goal_linking(&db, "goal-1", &["job-b", "job-a"]).await;

        let report = backfill_cron_into_goals(&db).await.unwrap();
        assert_eq!(
            report.placed,
            vec![
                ("job-a".to_string(), Placement::NewGoal),
                (
                    "job-b".to_string(),
                    Placement::MergedInto("goal-1".to_string())
                )
            ]
        );
        let cron = TursoCronRepo::new(db.clone());
        let merged = cron
            .get(&CronJobId::from_raw("goal-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.next_run_at, Some(from_ts(6_000)));
        let standalone = cron
            .get(&CronJobId::from_raw("job-a"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(standalone.name, "甲");
        assert_eq!(
            cron.firings(&CronJobId::from_raw("goal-1"), 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// 跑两遍结果一样；目标的定时被清掉之后再跑也不会把任务再造一次。
    #[tokio::test]
    async fn running_twice_changes_nothing_the_second_time() {
        let (db, _dir) = temp().await;
        insert_job(&db, "job-1", "晨间摘要", "active", 5_000).await;
        insert_job(&db, "job-2", "巡检", "active", 6_000).await;
        insert_job(&db, "job-3", "第三个", "paused", 7_000).await;
        insert_firing(&db, "job-2", 1_000).await;
        goal_linking(&db, "goal-1", &["job-2", "job-3"]).await;

        let first = backfill_cron_into_goals(&db).await.unwrap();
        assert!(!first.is_noop());
        let after_first = counts(&db).await;
        assert_eq!(
            after_first,
            (3, 3, 1),
            "三个任务三个目标（goal-1 / job-1 / job-3），firing 一条"
        );

        let second = backfill_cron_into_goals(&db).await.unwrap();
        assert!(second.is_noop(), "{second:?}");
        assert_eq!(counts(&db).await, after_first);

        // 把并入的那个定时清掉：任务已处理过，不会再长出新目标。
        let cron = TursoCronRepo::new(db.clone());
        assert!(cron.remove(&CronJobId::from_raw("goal-1")).await.unwrap());
        let third = backfill_cron_into_goals(&db).await.unwrap();
        assert!(third.is_noop(), "{third:?}");
        assert_eq!(counts(&db).await, after_first);
    }

    /// 回填演练：对**真实 aux.db 的副本**跑回填函数两遍，打印前后的数量与每个任务对应的目标 id。
    ///
    /// 手动跑，`KOMO_BACKFILL_AUX_DB` 指向副本里的 aux.db（只读复制 `~/.komo`，绝不对原件跑）：
    /// `KOMO_BACKFILL_AUX_DB=/path/to/copy/aux.db cargo test -p komo-store rehearsal -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "回填演练：要 KOMO_BACKFILL_AUX_DB 指向 aux.db 的副本"]
    async fn rehearsal_on_a_copy_of_the_real_aux_db() {
        let path = std::env::var("KOMO_BACKFILL_AUX_DB")
            .expect("把 KOMO_BACKFILL_AUX_DB 设成 aux.db 副本的路径");
        // 连库会按当前 schema 给副本补列、建索引：这正是升级时 Gateway 启动做的第一件事。
        let db = Db::connect(&path).await.expect("打开副本");
        let (jobs, matters, firings) = counts(&db).await;
        println!("回填前：定时任务 {jobs}，目标（含领域）{matters}，触发记录 {firings}");

        let first = backfill_cron_into_goals(&db).await.expect("第一遍");
        let (jobs2, matters2, firings2) = counts(&db).await;
        println!("第一遍后：定时任务 {jobs2}，目标 {matters2}，触发记录 {firings2}");
        println!(
            "第一遍：处理 {} 个任务，改挂触发记录 {} 条，摘掉 cron 触发器的目标 {} 个",
            first.placed.len(),
            first.firings_rekeyed,
            first.triggers_stripped.len()
        );
        let cron = TursoCronRepo::new(db.clone());
        for (job, placement) in &first.placed {
            let goal = match placement {
                Placement::NewGoal => job.clone(),
                Placement::MergedInto(goal) => goal.clone(),
            };
            let read = cron
                .get(&CronJobId::from_raw(goal.clone()))
                .await
                .expect("读定时");
            let how = match placement {
                Placement::NewGoal => "新建目标（id = 任务 id）".to_string(),
                Placement::MergedInto(goal) => format!("并入已有目标 {goal}"),
            };
            match read {
                Some(read) => println!(
                    "  任务 {job} → 目标 {goal} · {how} · 「{}」 · {} · {}",
                    read.name,
                    read.status.as_str(),
                    read.next_run_at
                        .map(|at| at.to_string())
                        .unwrap_or_else(|| "无下一次".into())
                ),
                None => println!("  任务 {job} → 目标 {goal} · {how} · 读不出定时（见 warn）"),
            }
        }

        let second = backfill_cron_into_goals(&db).await.expect("第二遍");
        let after_second = counts(&db).await;
        println!("第二遍：{second:?}；数量 {after_second:?}");
        assert!(second.is_noop(), "第二遍应当什么都不改：{second:?}");
        assert_eq!(after_second, (jobs2, matters2, firings2));
        assert_eq!(jobs2, jobs, "cron_jobs 一行没动");
        assert_eq!(firings2, firings, "触发记录一条没多没少，只是改了键");
    }
}
