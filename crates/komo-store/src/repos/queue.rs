//! `RunQueue`：调度器、恢复扫描、Cron 与手动 resume 共用的领取入口（§8.7）。
//!
//! **这是允许写 raw SQL 的三个模块之一**，而且原因很具体：toasty 的类型化 `UPDATE`
//! 能表达条件（含非索引列、`AND`、`IS NULL`），但**拿不到受影响行数**——查询目标的
//! `.exec()` 恒为 `Ok(())`，命中 0 行与 1 行不可区分（spike 实测）。而领取的全部意义
//! 就是"`rows affected == 1` 是接管成功，`== 0` 是别人先到"。
//!
//! **竞争失败的第一手信号是错误而不是 0 行**：并发竞争同一行时，败者拿到
//! serialization failure，只有在胜负已分之后再跑才会拿到 `0`。所以 `claim` 也包在
//! `with_write_retry` 里——它是少数几个"可以安全重跑的写"，因为守卫写在 `WHERE` 子句
//! 里：重跑一次要么还是没人领（赢），要么已经有人领了（`0`，判为输）。

use async_trait::async_trait;
use komo_kernel::traits::{LedgerError, RunQueue, StoreError};
use komo_kernel::types::ids::{ExecutorId, RunId};
use komo_kernel::types::status::{Claimed, RunStatus};
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, column_i64, column_string, map_toasty, to_ts};

/// §8.7：领取一个 Run。`rows affected == 1` 是接管成功。
const CLAIM_SQL: &str = r#"
UPDATE runs
   SET status           = 'running',
       claimed_by       = ?1,
       claim_generation = claim_generation + 1,
       claimed_at       = ?2
 WHERE id               = ?3
   AND claimed_by IS NULL
   AND status IN ('queued', 'waiting_retry')
"#;

/// §8.7：候选由一条普通查询给出，领取一个一条。
const DUE_SQL: &str = r#"
SELECT id FROM runs
 WHERE claimed_by IS NULL
   AND status IN ('queued', 'waiting_retry')
   AND next_retry_at <= ?1
 ORDER BY next_retry_at
 LIMIT ?2
"#;

/// §8.7：代次围栏——这个执行者写账本的每一条状态提交都带它。
const FENCE_SQL: &str = r#"
UPDATE runs
   SET status = ?1,
       updated_at = ?2
 WHERE id = ?3 AND claim_generation = ?4 AND claimed_by = ?5
"#;

/// §8.7：启动回收——旧实例的 running → interrupted，并交还领取权。
const RECLAIM_SQL: &str = r#"
UPDATE runs
   SET status = 'interrupted', claimed_by = NULL
 WHERE status = 'running' AND claimed_by <> ?1
"#;

/// 交还名额。
///
/// 只清 `claimed_by`，**不动 `status`，除非它还停在 `running`**：让出执行的那一刻
/// `Ledger::suspend` 已经把状态写成 `waiting_*` 了，再写一次就会盖掉它；而一个
/// `running` 却没人领的行谁也捡不起来（`claim` 要 `status IN ('queued',
/// 'waiting_retry')`），所以那一种要回到 `queued`。
const RELEASE_SQL: &str = r#"
UPDATE runs
   SET claimed_by = NULL,
       status = CASE WHEN status = 'running' THEN 'queued' ELSE status END
 WHERE id = ?1 AND claim_generation = ?2
"#;

/// 一次领取扫描最多看多少个候选。
const DUE_BATCH: i64 = 32;

/// Turso 上的 [`RunQueue`]。
#[derive(Debug, Clone)]
pub struct TursoRunQueue {
    db: Db,
}

impl TursoRunQueue {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// 到期且没人领的 Run，按 `next_retry_at` 排序。
    pub async fn due(&self, now: OffsetDateTime, limit: i64) -> Result<Vec<RunId>, StoreError> {
        let cutoff = to_ts(now);
        self.db
            .read(move |ex| {
                Box::pin(async move {
                    let rows = toasty::sql::query(DUE_SQL)
                        .bind(cutoff)
                        .bind(limit)
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    Ok(rows
                        .iter()
                        .filter_map(|row| column_string(row, 0))
                        .map(RunId::from_raw)
                        .collect::<Vec<_>>())
                }) as BoxFuture<'_, Result<Vec<RunId>, StoreError>>
            })
            .await
    }

    /// 这个执行者对这个 Run 的状态提交，带代次围栏。
    ///
    /// `rows affected == 0` 意味着自己已经是旧代次：报
    /// [`LedgerError::StaleGeneration`]，**停止这个任务的一切写入，不重试、不降级**
    /// （§8.7）。
    pub async fn commit_status(
        &self,
        claimed: &Claimed,
        executor: &ExecutorId,
        status: RunStatus,
        now: OffsetDateTime,
    ) -> Result<(), LedgerError> {
        let run = claimed.run.to_string();
        let generation = i64::try_from(claimed.generation).unwrap_or(i64::MAX);
        let executor = executor.to_string();
        let status = serde_json::to_value(status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .expect("RunStatus 序列化成一个字符串");
        let at = to_ts(now);

        let affected = self
            .db
            .with_write_retry(move |ex| {
                let (run, executor, status) = (run.clone(), executor.clone(), status.clone());
                Box::pin(async move {
                    toasty::sql::statement(FENCE_SQL)
                        .bind(status)
                        .bind(at)
                        .bind(run)
                        .bind(generation)
                        .bind(executor)
                        .exec(ex)
                        .await
                        .map_err(map_toasty)
                }) as BoxFuture<'_, Result<u64, StoreError>>
            })
            .await
            .map_err(crate::db::store_to_ledger)?;

        if affected == 0 {
            let current = self
                .generation_of(&claimed.run)
                .await
                .map_err(crate::db::store_to_ledger)?
                .unwrap_or(0);
            return Err(LedgerError::StaleGeneration {
                held: claimed.generation,
                current,
            });
        }
        Ok(())
    }

    /// 这个 Run 当前的领取代次。
    pub async fn generation_of(&self, run: &RunId) -> Result<Option<u64>, StoreError> {
        let id = run.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let rows =
                        toasty::sql::query("SELECT claim_generation FROM runs WHERE id = ?1")
                            .bind(id)
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                    Ok(rows
                        .first()
                        .and_then(|row| column_i64(row, 0))
                        .map(|n| n.max(0) as u64))
                }) as BoxFuture<'_, Result<Option<u64>, StoreError>>
            })
            .await
    }

    async fn try_claim(
        &self,
        run: &RunId,
        executor: &ExecutorId,
        now: OffsetDateTime,
    ) -> Result<Option<Claimed>, StoreError> {
        let id = run.to_string();
        let who = executor.to_string();
        let at = to_ts(now);

        self.db
            .with_write_retry(move |ex| {
                let (id, who) = (id.clone(), who.clone());
                Box::pin(async move {
                    let affected = toasty::sql::statement(CLAIM_SQL)
                        .bind(who)
                        .bind(at)
                        .bind(id.clone())
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    if affected == 0 {
                        return Ok(None);
                    }
                    // 代次要在**同一个事务里**读回来：出了这个事务，另一个执行者的
                    // release + claim 就能让这个数字变成别人的。
                    let rows =
                        toasty::sql::query("SELECT claim_generation FROM runs WHERE id = ?1")
                            .bind(id.clone())
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                    let generation = rows
                        .first()
                        .and_then(|row| column_i64(row, 0))
                        .map(|n| n.max(0) as u64)
                        .ok_or_else(|| {
                            StoreError::Other(format!("刚领到的 run {id} 读不回代次"))
                        })?;
                    Ok(Some(Claimed {
                        run: RunId::from_raw(id),
                        generation,
                    }))
                }) as BoxFuture<'_, Result<Option<Claimed>, StoreError>>
            })
            .await
    }
}

#[async_trait]
impl RunQueue for TursoRunQueue {
    async fn claim(&self, executor: &ExecutorId) -> Result<Option<Claimed>, StoreError> {
        // 时钟只在这里读一次：候选筛选与 `claimed_at` 用同一个瞬时，否则一个 Run 可能
        // 在筛选之后、领取之前"变得还没到期"。
        let now = OffsetDateTime::now_utc();
        for run in self.due(now, DUE_BATCH).await? {
            if let Some(claimed) = self.try_claim(&run, executor, now).await? {
                return Ok(Some(claimed));
            }
        }
        Ok(None)
    }

    async fn claim_run(
        &self,
        run: &RunId,
        executor: &ExecutorId,
    ) -> Result<Option<Claimed>, StoreError> {
        self.try_claim(run, executor, OffsetDateTime::now_utc())
            .await
    }

    async fn release(&self, claimed: &Claimed) -> Result<(), StoreError> {
        let id = claimed.run.to_string();
        let generation = i64::try_from(claimed.generation).unwrap_or(i64::MAX);
        self.db
            .with_write_retry(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    // 代次不对就什么都不做——受影响行数是 0，这不是错误。
                    toasty::sql::statement(RELEASE_SQL)
                        .bind(id)
                        .bind(generation)
                        .exec(ex)
                        .await
                        .map_err(map_toasty)
                        .map(|_| ())
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
    }
}

/// 启动回收：把**别的**执行实例遗留的 `running` 标成 `interrupted` 并交还领取权。
///
/// 返回回收了几行。`interrupted` 是未完成状态，不是取消——Gateway 关闭或系统重启属于
/// 中断（§8.4）；恢复要先核对外部效果，再决定是否继续。
pub async fn reclaim_abandoned_runs(db: &Db, executor: &ExecutorId) -> Result<u64, StoreError> {
    let who = executor.to_string();
    db.with_write_retry(move |ex| {
        let who = who.clone();
        Box::pin(async move { reclaim_abandoned_runs_in(ex, &ExecutorId::from_raw(who)).await })
            as BoxFuture<'_, Result<u64, StoreError>>
    })
    .await
}

/// 同上，但在调用方已经打开的事务里跑——恢复要把它和"收拾遗留的尝试"放在一起提交。
pub async fn reclaim_abandoned_runs_in(
    ex: &mut dyn Executor,
    executor: &ExecutorId,
) -> Result<u64, StoreError> {
    toasty::sql::statement(RECLAIM_SQL)
        .bind(executor.to_string())
        .exec(ex)
        .await
        .map_err(map_toasty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbOptions;
    use crate::models::RunRow;

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect_with(
            dir.path().join("state.db"),
            DbOptions {
                pool_size: 8,
                ..Default::default()
            },
        )
        .await
        .expect("打开库");
        (db, dir)
    }

    async fn queued_run(db: &Db, id: &str) -> RunId {
        let id = id.to_string();
        let for_tx = id.clone();
        db.with_write_retry(move |ex| {
            let id = for_tx.clone();
            Box::pin(async move {
                toasty::create!(RunRow {
                    id,
                    session_id: "sess",
                    request_key: "k",
                    input_hash: "h",
                    input_event: None as Option<String>,
                    final_event: None as Option<String>,
                    status: "queued",
                    source: r#"{"kind":"interactive","session":"sess"}"#,
                    peer: None as Option<String>,
                    claimed_by: None as Option<String>,
                    claim_generation: 0_i64,
                    claimed_at: 0_i64,
                    next_retry_at: 0_i64,
                    retry_attempts: 0_i64,
                    rounds: 0_i64,
                    max_rounds: 0_i64,
                    valid_until: 0_i64,
                    model_snapshot: r#"{"provider":"test","base_url":"memory://test","model":"m","api_key_env":"K","timeout_secs":30}"#,
                    effort: None as Option<String>,
                    grants: "[]",
                    memory_work: "pending",
                    memory_cursor: 0_i64,
                    last_error: None as Option<String>,
                    created_at: 0_i64,
                    updated_at: 0_i64,
                    ended_at: 0_i64,
                })
                .exec(ex)
                .await
                .map_err(map_toasty)?;
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .expect("建 run 行");
        RunId::from_raw(id)
    }

    /// 验收 ⑤：并发 `claim_run` 同一个 Run，**恰好一个成功**。
    ///
    /// §8.7 的实测形状：竞争失败的第一手信号是错误而不是 0 行，`with_write_retry` 干净
    /// 重跑之后才收敛到 0。所以这条测试同时锁住"没有 0 个赢家"和"没有多个赢家"。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn exactly_one_executor_claims_a_given_run() {
        const ROUNDS: usize = 100;
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());

        let mut zero = 0usize;
        let mut multi = 0usize;
        let mut exactly_one = 0usize;

        for round in 0..ROUNDS {
            let run = queued_run(&db, &format!("run-{round:04}")).await;
            let competitors = 2 + (round % 3);

            let mut handles = Vec::new();
            for k in 0..competitors {
                let queue = queue.clone();
                let run = run.clone();
                handles.push(tokio::spawn(async move {
                    queue
                        .claim_run(&run, &ExecutorId::from_raw(format!("exec-{k}")))
                        .await
                }));
            }

            let mut winners = Vec::new();
            for handle in handles {
                match handle.await.expect("领取任务没有 panic") {
                    Ok(Some(claimed)) => winners.push(claimed),
                    Ok(None) => {}
                    Err(error) => panic!("领取不该报错：{error}"),
                }
            }
            match winners.len() {
                0 => zero += 1,
                1 => {
                    assert_eq!(winners[0].generation, 1, "赢家的代次从 0 递增到 1");
                    exactly_one += 1;
                }
                _ => multi += 1,
            }
        }

        assert_eq!(
            (exactly_one, zero, multi),
            (ROUNDS, 0, 0),
            "恰好一个赢家：没有 0 赢家，也没有多赢家"
        );
    }

    /// `claim` 拿的是"下一个可执行的"，同一个 Run 不会被两个执行者拿到。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn claiming_the_next_run_hands_each_one_to_exactly_one_executor() {
        const RUNS: usize = 20;
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        for index in 0..RUNS {
            queued_run(&db, &format!("run-{index:04}")).await;
        }

        let mut handles = Vec::new();
        for k in 0..4 {
            let queue = queue.clone();
            handles.push(tokio::spawn(async move {
                let executor = ExecutorId::from_raw(format!("exec-{k}"));
                let mut mine = Vec::new();
                while let Some(claimed) = queue.claim(&executor).await.expect("领取不报错") {
                    mine.push(claimed.run);
                }
                mine
            }));
        }

        let mut all = Vec::new();
        for handle in handles {
            all.extend(handle.await.expect("领取任务没有 panic"));
        }
        all.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let unique: std::collections::BTreeSet<_> = all.iter().map(|r| r.to_string()).collect();
        assert_eq!(all.len(), unique.len(), "同一个 Run 不能被领两次：{all:?}");
        assert_eq!(unique.len(), RUNS, "每个 Run 都被领走了");
    }

    /// 验收 ⑥：代次围栏——旧代次的状态提交 `rows affected == 0`，报
    /// [`LedgerError::StaleGeneration`]。
    #[tokio::test]
    async fn a_stale_generation_cannot_commit_state() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let run = queued_run(&db, "run-fenced").await;

        let first = ExecutorId::from_raw("exec-1");
        let claimed = queue
            .claim_run(&run, &first)
            .await
            .unwrap()
            .expect("第一次领得到");
        assert_eq!(claimed.generation, 1);

        // 当前代次写得进去。
        queue
            .commit_status(
                &claimed,
                &first,
                RunStatus::WaitingRetry,
                OffsetDateTime::now_utc(),
            )
            .await
            .expect("当前代次可以提交");

        // 交还名额之后换一个执行者接管，代次递增。
        queue.release(&claimed).await.unwrap();
        let second = ExecutorId::from_raw("exec-2");
        let taken = queue
            .claim_run(&run, &second)
            .await
            .unwrap()
            .expect("交还后别人领得到");
        assert_eq!(taken.generation, 2);

        // 旧执行者再写就是旧代次：**停止这个任务的一切写入，不重试、不降级**（§8.7）。
        let error = queue
            .commit_status(
                &claimed,
                &first,
                RunStatus::Completed,
                OffsetDateTime::now_utc(),
            )
            .await
            .expect_err("旧代次不能提交");
        assert_eq!(
            error,
            LedgerError::StaleGeneration {
                held: 1,
                current: 2
            },
            "要说清楚握着哪一代、现在是哪一代"
        );
    }

    /// 交还名额时代次不对就什么都不做。
    #[tokio::test]
    async fn releasing_with_a_stale_generation_does_nothing() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let run = queued_run(&db, "run-release").await;

        let executor = ExecutorId::from_raw("exec-1");
        let claimed = queue.claim_run(&run, &executor).await.unwrap().unwrap();

        let stale = Claimed {
            run: run.clone(),
            generation: claimed.generation - 1,
        };
        queue.release(&stale).await.unwrap();

        // 还在这个执行者手里——旧代次没能把它放回队列。
        assert!(
            queue
                .claim_run(&run, &ExecutorId::from_raw("exec-2"))
                .await
                .unwrap()
                .is_none(),
            "旧代次的 release 不该让别人领走"
        );
    }

    /// 验收：启动回收把**别的**实例遗留的 running 标成 interrupted 并交还领取权。
    #[tokio::test]
    async fn startup_reclaims_runs_left_running_by_another_instance() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let mine = queued_run(&db, "run-mine").await;
        let theirs = queued_run(&db, "run-theirs").await;

        let old = ExecutorId::from_raw("exec-old");
        let now = ExecutorId::from_raw("exec-now");
        queue.claim_run(&theirs, &old).await.unwrap().unwrap();
        queue.claim_run(&mine, &now).await.unwrap().unwrap();

        let reclaimed = reclaim_abandoned_runs(&db, &now).await.unwrap();
        assert_eq!(reclaimed, 1, "只回收别人的那一个");

        let record = crate::repos::runs::get(&db, &theirs)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.status, RunStatus::Interrupted);
        assert!(record.claimed_by.is_none(), "领取权交还了");

        let still = crate::repos::runs::get(&db, &mine).await.unwrap().unwrap();
        assert_eq!(still.status, RunStatus::Running, "自己的那个不动");
    }

    /// 验收 B1：**让出执行名额就是交还领取权**。
    ///
    /// 挂起时不清 `claimed_by` 的话，§8.7 那两条语句里的 `claimed_by IS NULL` 永远筛不
    /// 到这一行——退避到期了、审批答复了，它也再没有人领得走。
    #[tokio::test]
    async fn a_run_waiting_for_its_backoff_can_be_claimed_again_when_it_is_due() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let run = queued_run(&db, "run-suspend").await;

        let claimed = queue
            .claim_run(&run, &ExecutorId::from_raw("exec-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.generation, 1);

        suspend(&db, &run, RunStatus::WaitingRetry).await;

        let record = crate::repos::runs::get(&db, &run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::WaitingRetry);
        assert!(record.claimed_by.is_none(), "让出名额就要交还领取权");

        // 到期的候选查询看得到它（`DUE_SQL` 收 `queued` 与 `waiting_retry` 两种）。
        assert!(
            queue
                .due(OffsetDateTime::now_utc(), 10)
                .await
                .unwrap()
                .contains(&run)
        );
        // 而且真的领得走，代次递增。
        let again = queue
            .claim(&ExecutorId::from_raw("exec-2"))
            .await
            .unwrap()
            .expect("领得到");
        assert_eq!(again.run, run);
        assert_eq!(again.generation, 2, "换人接管，代次要涨");
    }

    /// 等审批的那一行同样交还领取权。
    ///
    /// 它**不进**候选查询——`DUE_SQL` 只收 `queued` 与 `waiting_retry`，等人回答的那一
    /// 行要等决定到达后被重新入队（§13.5）。所以这里断言的是"重新入队之后领得走"，
    /// 而不是"现在就领得走"：领取权有没有交还，正是这两者的分界。
    #[tokio::test]
    async fn a_run_waiting_for_an_approval_also_hands_back_its_claim() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let run = queued_run(&db, "run-approval").await;
        queue
            .claim_run(&run, &ExecutorId::from_raw("exec-1"))
            .await
            .unwrap()
            .unwrap();

        suspend(&db, &run, RunStatus::WaitingApproval).await;
        let record = crate::repos::runs::get(&db, &run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::WaitingApproval);
        assert!(record.claimed_by.is_none());
        assert!(
            queue
                .due(OffsetDateTime::now_utc(), 10)
                .await
                .unwrap()
                .is_empty(),
            "等人回答的行不该被调度器捡走"
        );

        // 决定到了 → 重新入队 → 领得走。
        crate::repos::recovery::RecoveryStore::new(db.clone(), std::path::PathBuf::from("."))
            .requeue(&run)
            .await
            .unwrap();
        let again = queue
            .claim(&ExecutorId::from_raw("exec-2"))
            .await
            .unwrap()
            .expect("领得到");
        assert_eq!(again.run, run);
        assert_eq!(again.generation, 2);
    }

    /// 让一行停下来等。
    async fn suspend(db: &Db, run: &RunId, status: RunStatus) {
        let due_at = OffsetDateTime::now_utc() - time::Duration::seconds(1);
        let run = run.clone();
        db.with_write_retry(move |ex| {
            let run = run.clone();
            Box::pin(async move {
                crate::repos::runs::mark_waiting_in(
                    ex,
                    &run,
                    status,
                    1,
                    Some(due_at),
                    Some("等一会儿".into()),
                    OffsetDateTime::now_utc(),
                )
                .await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
    }

    /// `due` 只给没人领、到期了的。
    #[tokio::test]
    async fn a_run_waiting_for_its_backoff_is_not_due_yet() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let run = queued_run(&db, "run-backoff").await;

        let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let claimed = queue
            .claim_run(&run, &ExecutorId::from_raw("exec-1"))
            .await
            .unwrap()
            .unwrap();
        crate::repos::runs::get(&db, &run).await.unwrap().unwrap();
        db.with_write_retry(move |ex| {
            Box::pin(async move {
                crate::repos::runs::mark_waiting_in(
                    ex,
                    &RunId::from_raw("run-backoff"),
                    RunStatus::WaitingRetry,
                    1,
                    Some(later),
                    Some("慢一点".into()),
                    OffsetDateTime::now_utc(),
                )
                .await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
        queue.release(&claimed).await.unwrap();

        assert!(
            queue
                .due(OffsetDateTime::now_utc(), 10)
                .await
                .unwrap()
                .is_empty(),
            "退避没到期就不该出现在候选里"
        );
        assert!(
            queue
                .due(later + time::Duration::seconds(1), 10)
                .await
                .unwrap()
                .contains(&run),
            "到期了就该出现"
        );
    }
}
