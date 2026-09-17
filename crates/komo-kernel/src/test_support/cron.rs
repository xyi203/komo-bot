//! 内存里的 [`CronRepo`]。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::traits::*;
use crate::types::ids::*;

use crate::cron::{CronFiring, CronJob, JobStatus};

/// 内存里的 [`CronRepo`]。`claim_firing` 的唯一键是真的。
#[derive(Debug, Clone, Default)]
pub struct MemCronRepo {
    state: Arc<Mutex<CronState>>,
}

#[derive(Debug, Default)]
struct CronState {
    jobs: BTreeMap<CronJobId, CronJob>,
    firings: Vec<CronFiring>,
    unfinished: Vec<CronJobId>,
}

impl MemCronRepo {
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记某个 Job "上一次还没结束"，用来测重叠策略。
    pub fn mark_unfinished(&self, job: CronJobId) {
        self.state.lock().expect("cron").unfinished.push(job);
    }
}

#[async_trait]
impl CronRepo for MemCronRepo {
    async fn list(&self) -> Result<Vec<CronJob>, RepoError> {
        Ok(self
            .state
            .lock()
            .expect("cron")
            .jobs
            .values()
            .cloned()
            .collect())
    }

    async fn get(&self, id: &CronJobId) -> Result<Option<CronJob>, RepoError> {
        Ok(self.state.lock().expect("cron").jobs.get(id).cloned())
    }

    async fn put(&self, mut job: CronJob) -> Result<CronJob, RepoError> {
        let mut state = self.state.lock().expect("cron");
        if let Some(existing) = state.jobs.get(&job.id) {
            job.version = existing.version + 1;
        }
        state.jobs.insert(job.id.clone(), job.clone());
        Ok(job)
    }

    async fn advance(
        &self,
        id: &CronJobId,
        next_run_at: Option<OffsetDateTime>,
        status: JobStatus,
        last_error: Option<String>,
    ) -> Result<(), RepoError> {
        let mut state = self.state.lock().expect("cron");
        let job = state.jobs.get_mut(id).ok_or_else(|| RepoError::NotFound {
            what: format!("cron job {id}"),
        })?;
        // 版本一个字都不动：推进槽位不是定义变更。
        job.next_run_at = next_run_at;
        job.status = status;
        job.last_error = last_error;
        Ok(())
    }

    async fn remove(&self, id: &CronJobId) -> Result<bool, RepoError> {
        Ok(self.state.lock().expect("cron").jobs.remove(id).is_some())
    }

    async fn due(&self, now: OffsetDateTime) -> Result<Vec<CronJob>, RepoError> {
        Ok(self
            .state
            .lock()
            .expect("cron")
            .jobs
            .values()
            .filter(|j| j.is_due(now))
            .cloned()
            .collect())
    }

    async fn claim_firing(&self, firing: CronFiring) -> Result<bool, RepoError> {
        let mut state = self.state.lock().expect("cron");
        let exists = state
            .firings
            .iter()
            .any(|f| f.job == firing.job && f.scheduled_at == firing.scheduled_at);
        if exists {
            return Ok(false);
        }
        state.firings.push(firing);
        Ok(true)
    }

    async fn update_firing(&self, firing: CronFiring) -> Result<bool, RepoError> {
        let mut state = self.state.lock().expect("cron");
        let Some(row) = state
            .firings
            .iter_mut()
            .find(|f| f.job == firing.job && f.scheduled_at == firing.scheduled_at)
        else {
            return Ok(false);
        };
        *row = firing;
        Ok(true)
    }

    async fn has_unfinished_firing(&self, id: &CronJobId) -> Result<bool, RepoError> {
        Ok(self.state.lock().expect("cron").unfinished.contains(id))
    }

    async fn firings(&self, id: &CronJobId, limit: u32) -> Result<Vec<CronFiring>, RepoError> {
        Ok(self
            .state
            .lock()
            .expect("cron")
            .firings
            .iter()
            .filter(|f| &f.job == id)
            .take(limit as usize)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::block_on;

    #[test]
    fn the_same_scheduled_time_is_only_claimed_once() {
        block_on(async {
            let repo = MemCronRepo::new();
            let firing = CronFiring {
                job: CronJobId::from_raw("job-1"),
                job_version: 1,
                scheduled_at: time::macros::datetime!(2026-09-16 01:00:00 UTC),
                prompt: "整理今天的动态".into(),
                session: None,
                run: None,
                status: Default::default(),
                error: None,
            };
            assert!(repo.claim_firing(firing.clone()).await.unwrap());
            assert!(!repo.claim_firing(firing).await.unwrap());
        });
    }

    #[test]
    fn advancing_a_slot_does_not_invalidate_the_jobs_grants() {
        block_on(async {
            use crate::cron::{
                CronJob, FixedOffsetZone, JobStatus, OverlapPolicy, TimeZone, parse_schedule,
            };

            let now = time::macros::datetime!(2026-09-15 08:00:00 UTC);
            let zones = FixedOffsetZone::utc();
            let repo = MemCronRepo::new();
            let job = CronJob {
                id: CronJobId::from_raw("job-1"),
                name: "morning-summary".into(),
                version: 1,
                trigger: parse_schedule("0 9 * * *", &TimeZone::utc(), now, &zones).unwrap(),
                prompt: "整理今天的动态".into(),
                workdir: None,
                status: JobStatus::Active,
                overlap: OverlapPolicy::Skip,
                model: None,
                effort: None,
                skills: vec![],
                max_rounds: None,
                notify: Default::default(),
                next_run_at: Some(now),
                last_error: None,
            };
            let stored = repo.put(job.clone()).await.unwrap();
            assert_eq!(stored.version, 1, "第一次写就是第一版");

            // 推进槽位：版本一个字不动，否则 GrantScope::CronJob 绑的那个版本每天作废一次。
            let next = time::macros::datetime!(2026-09-16 09:00:00 UTC);
            repo.advance(&stored.id, Some(next), JobStatus::Active, None)
                .await
                .unwrap();
            let after = repo.get(&stored.id).await.unwrap().unwrap();
            assert_eq!(after.version, 1, "推进槽位不是定义变更");
            assert_eq!(after.next_run_at, Some(next));

            // 一次性触发跑完了：状态变 done，版本还是不动。
            repo.advance(&stored.id, None, JobStatus::Done, None)
                .await
                .unwrap();
            let done = repo.get(&stored.id).await.unwrap().unwrap();
            assert_eq!(done.version, 1);
            assert_eq!(done.status, JobStatus::Done);
            assert!(done.next_run_at.is_none());

            // 改定义才递增——旧授权就该在这一刻失效。
            let bumped = repo.put(after).await.unwrap();
            assert_eq!(bumped.version, 2);

            assert!(
                repo.advance(&CronJobId::from_raw("job-9"), None, JobStatus::Active, None)
                    .await
                    .is_err(),
                "推进一个不存在的 Job 是错误，不是无声无息"
            );
        });
    }
}
