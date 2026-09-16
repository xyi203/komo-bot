//! 内存里的 [`CronRepo`]。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::traits::*;
use crate::types::ids::*;

use crate::cron::{CronFiring, CronJob};

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
