//! `runs.memory_work` 这条队列（§9.3）。
//!
//! 「runs 中记录 pending / processing / done / error 与处理游标。**进程崩溃后可重新领取**，
//! 重复处理相同来源不重复新增。」——所以队列只有三个动作：领一批、处理完记状态与游标、
//! 启动时把卡在 `processing` 的放回去。
//!
//! 它是一个 trait 而不是直接调 store，只为了一件事：[`super::MemoryManager`] 的测试要能
//! 在没有数据库的情况下断言"失败不推进游标"。

use async_trait::async_trait;
use komo_kernel::traits::{Clock, StoreError};
use komo_kernel::types::ids::{RunId, Seq, SessionId};
use komo_kernel::types::memory::MemoryWork;
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::status::RunStatus;
use komo_store::Db;
use std::sync::Arc;

/// 领到的一个待处理 Run。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryWorkItem {
    pub run: RunId,
    pub session: SessionId,
    /// 终态。取消的不整理成经验（§9.3）。
    pub status: RunStatus,
    /// 来源。Cron 的 Run 不整理用户偏好（§9.3）。
    pub source: PlanSource,
    /// 已经处理到哪条 seq。
    pub cursor: Seq,
}

#[async_trait]
pub trait MemoryWorkLog: Send + Sync {
    /// 领一批。领到就翻成 `processing`——两个消费者不会领到同一个。
    async fn claim(&self, limit: usize) -> Result<Vec<MemoryWorkItem>, StoreError>;

    /// 处理完：记状态与游标。**游标只进不退**，`Pending` + 原游标 = 下次重试。
    async fn finish(&self, run: &RunId, work: MemoryWork, cursor: Seq) -> Result<(), StoreError>;

    /// 启动时把卡在 `processing` 的放回 `pending`。返回放回几行。
    async fn requeue_stuck(&self) -> Result<u64, StoreError>;
}

/// state.db 上的那一个。
#[derive(Clone)]
pub struct DbMemoryWork {
    db: Db,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for DbMemoryWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbMemoryWork").finish_non_exhaustive()
    }
}

impl DbMemoryWork {
    pub fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        DbMemoryWork { db, clock }
    }
}

#[async_trait]
impl MemoryWorkLog for DbMemoryWork {
    async fn claim(&self, limit: usize) -> Result<Vec<MemoryWorkItem>, StoreError> {
        let claimed = komo_store::repos::runs::claim_memory_work(&self.db, limit).await?;
        Ok(claimed
            .into_iter()
            .map(|record| MemoryWorkItem {
                run: record.run,
                session: record.session,
                status: record.status,
                source: record.source,
                cursor: record.memory_cursor,
            })
            .collect())
    }

    async fn finish(&self, run: &RunId, work: MemoryWork, cursor: Seq) -> Result<(), StoreError> {
        komo_store::repos::runs::finish_memory_work(&self.db, run, work, cursor, self.clock.now())
            .await
    }

    async fn requeue_stuck(&self) -> Result<u64, StoreError> {
        komo_store::repos::runs::requeue_stuck_memory_work(&self.db).await
    }
}
