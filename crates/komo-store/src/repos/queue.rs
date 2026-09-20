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

use std::collections::BTreeMap;

use async_trait::async_trait;
use komo_kernel::traits::{LedgerError, RunQueue, StoreError};
use komo_kernel::types::ids::{ExecutorId, RunId};
use komo_kernel::types::status::{Claimed, RunState, WaitReason};
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, column_i64, column_string, map_toasty, to_ts};
use crate::models::RunRow;
use crate::repos::runs::{nearest_unfinished_predecessor, state_of, state_str};

/// §8.7：领取一个 Run。`rows affected == 1` 是接管成功。
///
/// **同一 Session 里更早的那个没结束就不领**（§8.4）：`waiting`（等审批 / 等时钟 / 等人
/// 判断）与 `accepted` / `running` 都是没结束。少了这一条，后一个 Run 会越到前一个头上，
/// 而前一个停在的半轮里**那次调用还没有结果**——回放窗口就会带着一个没有输出的
/// `function_call` 发给模型，provider 直接 400（`No tool output found for tool call …`）。
/// 序号用 id 比：UUIDv7 的字典序就是创建序，而且是全序，不会出现两个 Run 互相挡住的死结。
///
/// 候选形状见 [`DUE_SQL`]——**同一份谓词**，包括 `wake_at`：退避没到期的 Run 「此刻不可
/// 领取」（§8.7），`komo resume` 也一样，由 §8.4 的决策表把它答成 `WaitUntilRetry`。
///
/// **会话必须存在且还在服务**（§8.9 最后一段）：`state IN ('active', 'closing')`。
/// 只有这两种服务——`deleted` / `purged` 的会话不再领，行不在更不领。少了这一条，
/// "目录被手工 `rm -rf`、Run 还留在队列里"会被照常领走并按空上下文执行；reconcile 负责
/// 把这种情况说清楚，这条守卫负责让它不发生。领取守卫只看会话，**不看内容**：目录缺失由
/// reconcile 观察出来并停成 `blocked`，不在这里猜。
const CLAIM_SQL: &str = r#"
UPDATE runs
   SET state            = 'running',
       wait_kind        = NULL,
       wait_ref         = NULL,
       wake_at          = 0,
       claimed_by       = ?1,
       claim_generation = claim_generation + 1,
       claimed_at       = ?2,
       lease_until      = ?3
 WHERE id               = ?4
   AND claimed_by IS NULL
   AND (
        state = 'queued'
     OR (state = 'waiting' AND wait_kind = 'retry' AND wake_at <= ?5)
   )
   AND EXISTS (
       SELECT 1 FROM sessions AS s
        WHERE s.id     = runs.session_id
          AND s.state IN ('active', 'closing')
   )
   AND NOT EXISTS (
       SELECT 1 FROM runs AS earlier
        WHERE earlier.session_id = runs.session_id
          AND (
               earlier.input_seq < runs.input_seq
            OR (earlier.input_seq = runs.input_seq AND earlier.id < runs.id)
          )
          AND earlier.state NOT IN ('completed', 'failed', 'cancelled', 'abandoned')
   )
"#;

/// §8.7：候选由一条普通查询给出，领取一个一条。同一 Session 的次序约束与 [`CLAIM_SQL`]
/// 同源，两处都要有：候选筛选会把不该领的排掉，领取那一句才是最终守卫。
///
/// 候选只有两种形状：`queued`，以及**等时钟**的 `waiting + retry`（`wake_at` 到了）。
/// 等人（`wait_kind IN ('approval','intervention')`）与等依赖（`dependency`）都不在
/// 候选里——它们在等人来处置，不是调度器能解决的。
///
/// **可领取的谓词只有一处定义**：这一条与 [`CLAIM_SQL`] 的 `WHERE` 逐字同源（候选是
/// 筛掉不该领的，领取那一句才是最终守卫，而两句判断的是同一件事）。两者是否真的一致由
/// `due_and_claim_agree_on_what_is_claimable` 逐形状断言——**别只改一处**。
///
/// **会话守卫也两处都要有**（§8.9 最后一段）：`state IN ('active','closing')`。
const DUE_SQL: &str = r#"
SELECT r.id FROM runs AS r
 WHERE r.claimed_by IS NULL
   AND (
        r.state = 'queued'
     OR (r.state = 'waiting' AND r.wait_kind = 'retry' AND r.wake_at <= ?1)
   )
   AND EXISTS (
       SELECT 1 FROM sessions AS s
        WHERE s.id     = r.session_id
          AND s.state IN ('active', 'closing')
   )
   AND NOT EXISTS (
       SELECT 1 FROM runs AS earlier
        WHERE earlier.session_id = r.session_id
          AND (
               earlier.input_seq < r.input_seq
            OR (earlier.input_seq = r.input_seq AND earlier.id < r.id)
          )
          AND earlier.state NOT IN ('completed', 'failed', 'cancelled', 'abandoned')
   )
 ORDER BY r.wake_at
 LIMIT ?2
"#;

/// §8.7：代次围栏——这个执行者写账本的每一条状态提交都带它。`wait` 为空时顺手把三个
/// 等待列清掉：一个不是 `waiting` 的行不该留着上一次的等待痕迹。
const FENCE_SQL: &str = r#"
UPDATE runs
   SET state      = ?1,
       wait_kind  = NULL,
       wait_ref   = NULL,
       wake_at    = 0,
       updated_at = ?2
 WHERE id = ?3 AND claim_generation = ?4 AND claimed_by = ?5
"#;

/// 停在等待上：`state = 'waiting'` 与等待三列**一起**写（§8.10 的两维状态）。
const FENCE_WAIT_SQL: &str = r#"
UPDATE runs
   SET state      = ?1,
       wait_kind  = ?2,
       wait_ref   = ?3,
       wake_at    = ?4,
       updated_at = ?5
 WHERE id = ?6 AND claim_generation = ?7 AND claimed_by = ?8
"#;

/// 交还名额（§8.7）：**只清 `claimed_by`，一个状态字都不动**。
///
/// 让出执行的那一刻 [`TursoRunQueue::commit_status`] 已经把状态写成 `waiting` 与等待三列
/// 了；store 不替它猜"该回 `queued` 还是该等人"。如果交还时它还停在 `running`，那一行就是
/// "`running` 且没人领"的 orphan——**它不可领取**（候选只有 `queued` 与 `waiting+retry`），
/// 由 reconcile 按 §8.4 落成 `queued` 或 `waiting + intervention`。
const RELEASE_SQL: &str = r#"
UPDATE runs
   SET claimed_by = NULL
 WHERE id = ?1 AND claim_generation = ?2
"#;

/// 启动回收：旧实例遗留的 `running`，**只交还领取权**（§8.9）。
///
/// 交还之后那一行是"`running` 且没人领"的 orphan，reconcile 立刻按决策表处置它。store
/// 看不见"那条调用停在 `planned` 还是 `started` 没结果"，所以它不判 `queued` 还是等人。
const RECLAIM_UNOWNED_SQL: &str = r#"
UPDATE runs
   SET claimed_by = NULL
 WHERE state = 'running' AND claimed_by IS NOT NULL AND claimed_by <> ?1
"#;

/// 租约过期：只交还**自己**持有的那一批（§8.7）。
///
/// **租约过期只是信号，不是抢走一条活着的长调用的许可**——那会真的产生重复副作用。
/// 调用方还要过存活判定（确认持有者真的不在，或自己内存里不再持有它）。
const RECLAIM_EXPIRED_LEASE_SQL: &str = r#"
UPDATE runs
   SET claimed_by = NULL
 WHERE state = 'running' AND claimed_by = ?1 AND lease_until < ?2
"#;

/// **逐条**的租约回收：一个 Run 的租约过期、而它确实还挂在**这个**执行者名下，就交还。
///
/// 整批那条（[`RECLAIM_EXPIRED_LEASE_SQL`]）做不到"只收我确实不再持有的那些"——它要么
/// 全收、要么不收。回收之前还要过存活判定（"这条还在我内存里跑着吗"），所以调用方需要
/// 一个**逐条**的口子自己筛：筛掉还在手里的，剩下的逐条来。
const RECLAIM_LEASE_SQL: &str = r#"
UPDATE runs
   SET claimed_by = NULL
 WHERE id = ?1 AND claimed_by = ?2 AND state = 'running' AND lease_until < ?3
"#;

/// `running` 而没有主人的行——reconcile 的输入之一。
const UNOWNED_RUNNING_SQL: &str = r#"
SELECT id FROM runs
 WHERE state = 'running' AND claimed_by IS NULL
 ORDER BY id
"#;

/// 租约过期但还被别人（或自己）持有的行——回收前的**候选**，不是结论。
const EXPIRED_LEASE_RUNNING_SQL: &str = r#"
SELECT id FROM runs
 WHERE state = 'running' AND claimed_by IS NOT NULL AND lease_until < ?1
 ORDER BY id
"#;

/// 心跳续租。带代次围栏：旧代次续不动新代次的租约。
const RENEW_LEASE_SQL: &str = r#"
UPDATE runs
   SET lease_until = ?1
 WHERE id = ?2 AND claim_generation = ?3 AND claimed_by = ?4 AND state = 'running'
"#;

/// 一次领取扫描最多看多少个候选。
const DUE_BATCH: i64 = 32;

/// 没显式配置时用的租约窗口。
///
/// **它不是 TTL 的权威**：窗口是接线方的配置，用 [`TursoRunQueue::with_lease`] 传进来。
/// 这里给一个非零默认值只为一件事——领取时 `lease_until` 真的写下一个值，而不是 0
/// （0 = 没租约 = 立刻被当成过期候选）。
pub const DEFAULT_LEASE_WINDOW: time::Duration = time::Duration::minutes(2);

/// Turso 上的 [`RunQueue`]。
#[derive(Debug, Clone)]
pub struct TursoRunQueue {
    db: Db,
    /// 领取时写下的租约窗口：`lease_until = claimed_at + lease`。续租由 [`RunQueue::renew`]
    /// 按同一个窗口推进。
    lease: time::Duration,
}

impl TursoRunQueue {
    pub fn new(db: Db) -> Self {
        Self::with_lease(db, DEFAULT_LEASE_WINDOW)
    }

    /// 用配置里的租约窗口建一个队列（§8.7）。窗口越长，"handler 死了"被发现得越晚；
    /// 越短，长调用需要续租得越勤。
    pub fn with_lease(db: Db, lease: time::Duration) -> Self {
        Self { db, lease }
    }

    /// 到期且没人领的 Run，按 `wake_at` 排序（`queued` 的 `wake_at` 是 0，排在最前）。
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
    ///
    /// `wait` 是 `Some` 时 `state` 必须是 `Waiting`，三个等待列与状态**同一个提交**写下去
    /// （§8.10 的两维状态）；`None` 时那三列被清空。等待三列分开写会让 reconcile 看到
    /// "`waiting` 但不知道在等谁"这种没人能处置的形状。
    pub async fn commit_status(
        &self,
        claimed: &Claimed,
        executor: &ExecutorId,
        state: RunState,
        wait: Option<&WaitReason>,
        now: OffsetDateTime,
    ) -> Result<(), LedgerError> {
        debug_assert!(
            wait.is_none() || state == RunState::Waiting,
            "只有 waiting 才带等待三列"
        );
        let run = claimed.run.to_string();
        let generation = i64::try_from(claimed.generation).unwrap_or(i64::MAX);
        let executor = executor.to_string();
        let at = to_ts(now);
        let state = state.as_str();
        // 摊成可 move 的普通值：闭包可能被重跑，而 `wait` 是借用。
        let (kind, reference, wake) = match wait {
            Some(wait) => (
                Some(wait.kind()),
                wait.reference(),
                wait.wake_at().map(to_ts).unwrap_or(0),
            ),
            None => (None, None, 0),
        };

        let affected = self
            .db
            .with_write_retry(move |ex| {
                let run = run.clone();
                let executor = executor.clone();
                let reference = reference.clone();
                Box::pin(async move {
                    let statement = match kind {
                        Some(kind) => {
                            let statement = toasty::sql::statement(FENCE_WAIT_SQL)
                                .bind(state)
                                .bind(kind);
                            let statement = match reference {
                                Some(reference) => statement.bind(reference),
                                None => statement.bind_typed(
                                    toasty::stmt::Value::Null,
                                    toasty::schema::db::Type::Text,
                                ),
                            };
                            statement
                                .bind(wake)
                                .bind(at)
                                .bind(run)
                                .bind(generation)
                                .bind(executor)
                        }
                        None => toasty::sql::statement(FENCE_SQL)
                            .bind(state)
                            .bind(at)
                            .bind(run)
                            .bind(generation)
                            .bind(executor),
                    };
                    statement.exec(ex).await.map_err(map_toasty)
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

    /// 心跳续租的那条 UPDATE（trait 方法 [`RunQueue::renew`] 走它）。
    ///
    /// `until` 由调用方按租约窗口算（store 不定义窗口）。`false` = 已经不是自己持有的
    /// `running` 了——**此时要停下一切写入**，和代次围栏被拒是同一件事。
    async fn renew_lease(
        &self,
        claimed: &Claimed,
        executor: &ExecutorId,
        until: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let run = claimed.run.to_string();
        let generation = i64::try_from(claimed.generation).unwrap_or(i64::MAX);
        let executor = executor.to_string();
        let until = to_ts(until);
        let affected = self
            .db
            .with_write_retry(move |ex| {
                let (run, executor) = (run.clone(), executor.clone());
                Box::pin(async move {
                    toasty::sql::statement(RENEW_LEASE_SQL)
                        .bind(until)
                        .bind(run)
                        .bind(generation)
                        .bind(executor)
                        .exec(ex)
                        .await
                        .map_err(map_toasty)
                }) as BoxFuture<'_, Result<u64, StoreError>>
            })
            .await?;
        Ok(affected == 1)
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
        let lease_until = to_ts(now + self.lease);

        self.db
            .with_write_retry(move |ex| {
                let (id, who) = (id.clone(), who.clone());
                Box::pin(async move {
                    let affected = toasty::sql::statement(CLAIM_SQL)
                        .bind(who)
                        .bind(at)
                        .bind(lease_until)
                        .bind(id.clone())
                        .bind(at)
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

    /// **续租**（§8.7）：handler 每 `TTL/3` 调一次，租约只用来发现"没人管了"。
    ///
    /// `false` = 这个 Run 的领取权已经不是自己的了：调用方应当停止这个任务的一切写入
    /// （与状态提交拿到 [`LedgerError::StaleGeneration`] 同一个信号，所以这里返回布尔）。
    async fn renew(
        &self,
        claimed: &Claimed,
        executor: &ExecutorId,
        until: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        self.renew_lease(claimed, executor, until).await
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

/// 启动回收：把**别的**执行实例遗留的 `running` 交还领取权（§8.9）。
///
/// **只清 `claimed_by`**：状态留成 `running` 且没人领——那是一个可查询的 orphan
/// （[`unowned_running`]），reconcile 按 §8.4 落成 `queued` 或 `waiting + intervention`。
/// store 判不了"旧执行者停在哪一步"，也不该判。
///
/// 返回回收了几行。
pub async fn reclaim_unowned(db: &Db, executor: &ExecutorId) -> Result<u64, StoreError> {
    let who = executor.to_string();
    db.with_write_retry(move |ex| {
        let who = who.clone();
        Box::pin(async move { reclaim_unowned_in(ex, &ExecutorId::from_raw(who)).await })
            as BoxFuture<'_, Result<u64, StoreError>>
    })
    .await
}

/// 同上，但在调用方已经打开的事务里跑——恢复要把它和"收拾遗留的尝试"放在一起提交。
pub async fn reclaim_unowned_in(
    ex: &mut dyn Executor,
    executor: &ExecutorId,
) -> Result<u64, StoreError> {
    toasty::sql::statement(RECLAIM_UNOWNED_SQL)
        .bind(executor.to_string())
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 租约过期：交还**自己**持有的那一批（§8.7）。
///
/// **这不等于"可以抢走"**：抢一条还活着的长调用会真的产生重复副作用。调用方必须先过存活
/// 判定（`claimed_by <> self`，或自己内存里不再持有它），再调它。
pub async fn reclaim_expired_lease(
    db: &Db,
    executor: &ExecutorId,
    now: OffsetDateTime,
) -> Result<u64, StoreError> {
    let who = executor.to_string();
    let cutoff = to_ts(now);
    db.with_write_retry(move |ex| {
        let who = who.clone();
        Box::pin(async move {
            toasty::sql::statement(RECLAIM_EXPIRED_LEASE_SQL)
                .bind(who)
                .bind(cutoff)
                .exec(ex)
                .await
                .map_err(map_toasty)
        }) as BoxFuture<'_, Result<u64, StoreError>>
    })
    .await
}

/// `running` 而没有主人的 Run——reconcile 的输入之一（§8.9）。
///
/// 这些行**领不走**（候选只有 `queued` 与 `waiting+retry`），所以不存在"调度器抢在
/// reconcile 前面把它领走"的竞态：reconcile 的一次扫描会把它们各落成 `queued` 或
/// `waiting + intervention`。
pub async fn unowned_running(db: &Db) -> Result<Vec<RunId>, StoreError> {
    db.read(move |ex| {
        Box::pin(async move {
            let rows = toasty::sql::query(UNOWNED_RUNNING_SQL)
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

/// 租约过期但还被某个执行者持有的 Run——**回收前的候选，不是结论**（§8.7）。
///
/// 交给存活判定那一层：持有者是自己且内存里还握着它，就续租而不是回收。
pub async fn expired_lease_running(db: &Db, now: OffsetDateTime) -> Result<Vec<RunId>, StoreError> {
    let cutoff = to_ts(now);
    db.read(move |ex| {
        Box::pin(async move {
            let rows = toasty::sql::query(EXPIRED_LEASE_RUNNING_SQL)
                .bind(cutoff)
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

/// **逐条**回收一条过期租约：`true` = 这次交还了它。
///
/// 判据全在 `WHERE` 里（CAS）：这一条必须还挂在**这个**执行者名下、还在 `running`、而且
/// 租约真的过期了。三条缺一就是 `false`——别人持有的那一条是 [`reclaim_unowned`] 的地盘，
/// 没到期的不是"没人管"。
///
/// **只清 `claimed_by`，状态仍是 `running`**：交还领取权不判状态（§8.9），这一行随即成为
/// reconcile 的孤儿输入。调用方用它自己做存活判定：还在内存里跑着的那些压根不该来调，
/// 剩下的逐条来——整批那条做不到这件事。
pub async fn reclaim_lease(
    db: &Db,
    run: &RunId,
    executor: &ExecutorId,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    let (run, executor) = (run.to_string(), executor.to_string());
    let cutoff = to_ts(now);
    db.with_write_retry(move |ex| {
        let (run, executor) = (run.clone(), executor.clone());
        Box::pin(async move {
            let affected = toasty::sql::statement(RECLAIM_LEASE_SQL)
                .bind(run)
                .bind(executor)
                .bind(cutoff)
                .exec(ex)
                .await
                .map_err(map_toasty)?;
            Ok(affected == 1)
        }) as BoxFuture<'_, Result<bool, StoreError>>
    })
    .await
}

/// 放行等到了的依赖（§8.4）：返回**回到 `queued`** 了几条。reconcile 每一拍顺手调它。
///
/// 每一条 `waiting + dependency` 只看两件事：它等的那条进终态了吗，以及**还有没有更早的
/// 非终态 Run**（同一份判据，见 [`nearest_unfinished_predecessor`]）。三种落点：
///
/// 1. 等的那条已终态、且没有更早的非终态 Run → `queued`（现在就能跑、只缺 worker）；
/// 2. 等的那条已终态、但**还有**更早的非终态 Run → **改指**到紧挨着的那条，仍是
///    `waiting + dependency`。**不能挪成 `queued`**：领取守卫查的是"任何更早的非终态
///    Run"，A 还没完的话这条根本领不走，库里就会留下"显示排队中却一直不动"的行——正是
///    这次改造要消灭的那种现象；
/// 3. 等的那条还没终态 → 不动。
///
/// `wait_ref` 指向的行不在了（整行被删）按已终态算：那条 Run 不会再有下文，等它等于永远
/// 等下去，而下一拍的重判会把它挪到该去的地方。
///
/// 整段在一个事务里读—判—写：每一条的落点都从**同一批行**派生，不会出现"判完了、写之前
/// 世界变了"的窗口。
pub async fn release_satisfied_dependencies(
    db: &Db,
    now: OffsetDateTime,
) -> Result<u64, StoreError> {
    db.with_write_retry(move |ex| {
        Box::pin(async move {
            // 只有"在等前一条 Run"的那些才需要重判。通常一条都没有——那一句就结束了，
            // 不必为了一次空转扫全表。
            let mut candidates = RunRow::filter(
                RunRow::fields()
                    .state()
                    .eq(RunState::Waiting.as_str())
                    .and(RunRow::fields().wait_kind().eq("dependency")),
            )
            .exec(ex)
            .await
            .map_err(map_toasty)?;
            if candidates.is_empty() {
                return Ok(0);
            }

            // 判"还有没有更早的非终态 Run"要看**同会话的全体**，但只读有依赖等待的那几个
            // 会话（`runs_session` 索引）。
            let mut sessions: BTreeMap<String, Vec<RunRow>> = BTreeMap::new();
            for candidate in &candidates {
                if sessions.contains_key(&candidate.session_id) {
                    continue;
                }
                let rows = RunRow::filter(
                    RunRow::fields()
                        .session_id()
                        .eq(candidate.session_id.as_str()),
                )
                .exec(ex)
                .await
                .map_err(map_toasty)?;
                sessions.insert(candidate.session_id.clone(), rows);
            }

            let mut released = 0u64;
            for row in candidates.iter_mut() {
                let Some(session_rows) = sessions.get(&row.session_id) else {
                    continue;
                };
                let Some(wait_ref) = row.wait_ref.as_deref() else {
                    continue;
                };
                // 等的那条进终态了吗。**行不在也算终态**：它不会再有下文，等它等于永远
                // 等下去，而下面那次重判会把它挪到该去的地方。
                let referenced_terminal = match session_rows.iter().find(|r| r.id == wait_ref) {
                    Some(referenced) => state_of(referenced)?.is_terminal(),
                    None => true,
                };
                if !referenced_terminal {
                    continue;
                }
                match nearest_unfinished_predecessor(
                    session_rows,
                    &row.session_id,
                    (row.input_seq, row.id.as_str()),
                    Some(&row.id),
                )? {
                    // 还有更早的非终态 Run：**改指**，仍是 `waiting + dependency`。
                    Some(closer) => {
                        row.update()
                            .wait_ref(Some(closer))
                            .updated_at(to_ts(now))
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                    }
                    // 没有人再挡着它了：回队列。
                    None => {
                        row.update()
                            .state(state_str(RunState::Queued))
                            .wait_kind(None as Option<String>)
                            .wait_ref(None as Option<String>)
                            .wake_at(0_i64)
                            .updated_at(to_ts(now))
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                        released += 1;
                    }
                }
            }
            Ok(released)
        }) as BoxFuture<'_, Result<u64, StoreError>>
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbOptions;
    use crate::models::RunRow;
    use komo_kernel::types::ids::{ApprovalId, SessionId};
    use komo_kernel::types::status::{RetryCause, SessionState};

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

    /// 只建 `runs` 那一行，**不建 `sessions` 行**——"孤儿 Run"（会话行不在）的测试要它。
    async fn orphan_run(db: &Db, session: &str, id: &str) -> RunId {
        let id = id.to_string();
        let session = session.to_string();
        let for_tx = id.clone();
        db.with_write_retry(move |ex| {
            let session = session.clone();
            let id = for_tx.clone();
            Box::pin(async move { insert_run(ex, &session, &id, 0, RunState::Queued, None).await })
                as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .expect("建 run 行");
        RunId::from_raw(id)
    }

    /// 一个排队中的 Run，会话行也建好（`active`）——§8.9 的领取守卫要求会话存在。
    async fn queued_run(db: &Db, session: &str, id: &str) -> RunId {
        let id = id.to_string();
        let session = session.to_string();
        let for_tx = id.clone();
        db.with_write_retry(move |ex| {
            let id = for_tx.clone();
            let session = session.clone();
            Box::pin(async move {
                crate::repos::session::ensure_in(
                    ex,
                    &SessionId::from_raw(session.clone()),
                    "api",
                    &format!("sessions/{session}/events.jsonl"),
                    OffsetDateTime::now_utc(),
                )
                .await?;
                insert_run(ex, &session, &id, 0, RunState::Queued, None).await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .expect("建 run 行");
        RunId::from_raw(id)
    }

    /// 只建 `runs` 那一行，状态、等待与 `input_seq` 由调用方给。
    /// 一个排队中的 Run，带显式 `input_seq`（次序测试用）。
    async fn queued_run_with_seq(db: &Db, session: &str, id: &str, seq: i64) -> RunId {
        let id = id.to_string();
        let session = session.to_string();
        let for_tx = id.clone();
        db.with_write_retry(move |ex| {
            let id = for_tx.clone();
            let session = session.clone();
            Box::pin(async move {
                crate::repos::session::ensure_in(
                    ex,
                    &SessionId::from_raw(session.clone()),
                    "api",
                    &format!("sessions/{session}/events.jsonl"),
                    OffsetDateTime::now_utc(),
                )
                .await?;
                insert_run(ex, &session, &id, seq, RunState::Queued, None).await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .expect("建 run 行");
        RunId::from_raw(id)
    }

    async fn insert_run(
        ex: &mut dyn Executor,
        session: &str,
        id: &str,
        input_seq: i64,
        state: RunState,
        wait: Option<&WaitReason>,
    ) -> Result<(), StoreError> {
        let (kind, reference, wake_at) = match wait {
            Some(wait) => (
                Some(wait.kind().to_string()),
                crate::repos::runs::wait_ref_str(wait),
                wait.wake_at().map(crate::db::to_ts).unwrap_or(0),
            ),
            None => (None, None, 0),
        };
        toasty::create!(RunRow {
            id,
            session_id: session,
            request_key: "k",
            input_hash: "h",
            input_event: None as Option<String>,
            // 次序权威是输入事件的 seq（§8.3）；测试里要摆顺序就显式给。
            input_seq,
            final_event: None as Option<String>,
            // 退役列：不再读，写入给空值（§8.2 只允许加列）。
            status: String::new(),
            state: state.as_str(),
            wait_kind: kind,
            wait_ref: reference,
            wake_at,
            lease_until: 0_i64,
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
    }

    /// 把会话推进到某个生命周期状态（CAS 一步）。测试里都从 `active` 出发。
    async fn session_state(db: &Db, session: &str, from: SessionState, to: SessionState) {
        let session = session.to_string();
        db.with_write_retry(move |ex| {
            let session = session.clone();
            Box::pin(async move {
                crate::repos::session::set_state_in(
                    ex,
                    &SessionId::from_raw(session),
                    from,
                    to,
                    OffsetDateTime::now_utc(),
                )
                .await
                .map(|_| ())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .expect("推进会话状态");
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
            let run =
                queued_run(&db, &format!("sess-{round:04}"), &format!("run-{round:04}")).await;
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
            queued_run(&db, &format!("sess-{index:04}"), &format!("run-{index:04}")).await;
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

    /// §8.4：**同一 Session 里更早的那个没结束，后一个不越过它。**
    ///
    /// 少了这条约束，后一个 Run 会踩在前一个停住的半轮上：那次调用还没有结果，回放窗口
    /// 只能带着一个没有输出的 `function_call` 发给模型，provider 直接 400
    /// （`No tool output found for tool call …`）。停在哪一种等待上都算——这里用用户那条
    /// 路径上的"等审批"。
    #[tokio::test]
    async fn a_later_run_in_the_same_session_waits_for_the_unfinished_one() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let executor = ExecutorId::from_raw("exec-1");
        let first = queued_run(&db, "sess-1", "run-0001").await;
        let second = queued_run(&db, "sess-1", "run-0002").await;
        let now = OffsetDateTime::now_utc();

        let claimed = queue.claim(&executor).await.unwrap().expect("先来先领");
        assert_eq!(claimed.run, first, "先来的先领");

        queue
            .commit_status(
                &claimed,
                &executor,
                RunState::Waiting,
                Some(&approval()),
                now,
            )
            .await
            .unwrap();
        assert!(
            queue.claim(&executor).await.unwrap().is_none(),
            "前一个还停在等审批，后一个不许领"
        );

        // 前一个收尾之后才轮到后一个。
        queue
            .commit_status(&claimed, &executor, RunState::Completed, None, now)
            .await
            .unwrap();
        let next = queue.claim(&executor).await.unwrap().expect("轮到后一个了");
        assert_eq!(next.run, second);
    }

    /// 同 Session 的次序**按输入事件的 seq**（§8.3），不按 Run ID 的字典序。
    ///
    /// 靶子刻意造得相反：`run-zzz` 先受理（seq 1），`run-aaa` 后受理（seq 2）。按 id 比会
    /// 排反，按 seq 比才对——而 UUIDv7 在同一纳秒内的低位本来就是随机的，拿 id 当先后是
    /// 一个会真出错的假设。
    #[tokio::test]
    async fn session_order_follows_the_input_seq_not_the_id_order() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let executor = ExecutorId::from_raw("exec-1");
        let earlier = queued_run_with_seq(&db, "sess-seq", "run-zzz", 1).await;
        let later = queued_run_with_seq(&db, "sess-seq", "run-aaa", 2).await;
        assert!(
            later.as_str() < earlier.as_str(),
            "这条靶子的 id 字典序与受理顺序相反"
        );

        let now = OffsetDateTime::now_utc();
        assert_eq!(
            queue.due(now, 10).await.unwrap(),
            vec![earlier.clone()],
            "候选只有 seq 小的那条"
        );
        assert!(
            queue.claim_run(&later, &executor).await.unwrap().is_none(),
            "seq 大的那条被挡着"
        );

        // 先受理的那条收尾 → 后受理的才能领。
        let claimed = queue.claim_run(&earlier, &executor).await.unwrap().unwrap();
        queue
            .commit_status(&claimed, &executor, RunState::Completed, None, now)
            .await
            .unwrap();
        assert_eq!(
            queue
                .claim_run(&later, &executor)
                .await
                .unwrap()
                .unwrap()
                .run,
            later
        );
    }

    /// 验收 ⑥：代次围栏——旧代次的状态提交 `rows affected == 0`，报
    /// [`LedgerError::StaleGeneration`]。
    #[tokio::test]
    async fn a_stale_generation_cannot_commit_state() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let run = queued_run(&db, "sess-fenced", "run-fenced").await;

        let first = ExecutorId::from_raw("exec-1");
        let claimed = queue
            .claim_run(&run, &first)
            .await
            .unwrap()
            .expect("第一次领得到");
        assert_eq!(claimed.generation, 1);

        // 当前代次写得进去。**让出执行就是 `waiting` + 在等什么**（state 与等待三列
        // 同一个提交写下去，§8.4）。
        queue
            .commit_status(
                &claimed,
                &first,
                RunState::Waiting,
                // 到点的退避：交还之后它**此刻就可领取**（§8.7 的"此刻不可领取"看
                // `wake_at`），这样这一步测的是围栏而不是时钟。
                Some(&retry_wait(
                    OffsetDateTime::now_utc() - time::Duration::seconds(1),
                )),
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
                RunState::Completed,
                None,
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
        let run = queued_run(&db, "sess-release", "run-release").await;

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

    /// 验收：启动回收只把**别的**实例遗留的 `running` 交还领取权——状态一个字不改。
    ///
    /// 交还之后那一行是"`running` 且没人领"的孤儿：它**领不走**（候选只有 `queued` 与
    /// 到点的 `waiting + retry`），所以不存在"调度器抢在 reconcile 前面把它领走"的竞态，
    /// 由 §8.9 的同一趟扫描按决策表落成 `queued` 或 `waiting + intervention`。
    #[tokio::test]
    async fn startup_reclaims_runs_left_running_by_another_instance() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let mine = queued_run(&db, "sess-mine", "run-mine").await;
        let theirs = queued_run(&db, "sess-theirs", "run-theirs").await;

        let old = ExecutorId::from_raw("exec-old");
        let now = ExecutorId::from_raw("exec-now");
        queue.claim_run(&theirs, &old).await.unwrap().unwrap();
        queue.claim_run(&mine, &now).await.unwrap().unwrap();

        let reclaimed = reclaim_unowned(&db, &now).await.unwrap();
        assert_eq!(reclaimed, 1, "只回收别人的那一个");

        let record = crate::repos::runs::get(&db, &theirs)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, RunState::Running, "状态留给 reconcile 判");
        assert!(record.claimed_by.is_none(), "领取权交还了");
        assert_eq!(
            unowned_running(&db).await.unwrap(),
            vec![theirs.clone()],
            "它正是 reconcile 要看的那个孤儿"
        );

        let still = crate::repos::runs::get(&db, &mine).await.unwrap().unwrap();
        assert_eq!(still.state, RunState::Running, "自己的那个不动");
        assert_eq!(still.claimed_by.as_deref(), Some("exec-now"));
    }

    /// 租约过期只是**信号**：候选看得见，回收只动自己持有的那一批，而且不改状态（§8.7）。
    ///
    /// **绝不因为租约过期就把一条活着的长调用抢走**——那会真的产生重复副作用。所以
    /// "过期"与"可以回收"是两件事：续过租的立刻退出候选。
    #[tokio::test]
    async fn an_expired_lease_hands_back_only_this_instances_runs() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let mine = queued_run(&db, "sess-lease-mine", "run-lease-mine").await;
        let theirs = queued_run(&db, "sess-lease-theirs", "run-lease-theirs").await;
        let me = ExecutorId::from_raw("exec-me");
        queue.claim_run(&mine, &me).await.unwrap().unwrap();
        queue
            .claim_run(&theirs, &ExecutorId::from_raw("exec-other"))
            .await
            .unwrap()
            .unwrap();

        // 没续过租（`lease_until = 0`，视为立即过期）的两条都是候选。
        let later = OffsetDateTime::now_utc() + time::Duration::minutes(5);
        assert_eq!(
            expired_lease_running(&db, later).await.unwrap(),
            vec![mine.clone(), theirs.clone()]
        );

        // 续租之后就不再是候选。
        let claimed = Claimed {
            run: mine.clone(),
            generation: 1,
        };
        assert!(
            queue
                .renew(&claimed, &me, later + time::Duration::minutes(10))
                .await
                .unwrap()
        );
        assert_eq!(
            expired_lease_running(&db, later).await.unwrap(),
            vec![theirs.clone()]
        );
        assert_eq!(
            reclaim_expired_lease(&db, &me, later).await.unwrap(),
            0,
            "刚续过租的那条不许被回收"
        );

        // 租约真的过期了：**只动自己持有的那一条**。
        assert_eq!(
            reclaim_expired_lease(&db, &me, later + time::Duration::minutes(20))
                .await
                .unwrap(),
            1
        );
        let mine_row = crate::repos::runs::get(&db, &mine).await.unwrap().unwrap();
        assert!(mine_row.claimed_by.is_none());
        assert_eq!(
            mine_row.state,
            RunState::Running,
            "状态不改，留给 reconcile"
        );
        assert_eq!(unowned_running(&db).await.unwrap(), vec![mine.clone()]);
        let theirs_row = crate::repos::runs::get(&db, &theirs)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(theirs_row.claimed_by.as_deref(), Some("exec-other"));
    }

    /// 验收 B1：**让出执行名额就是交还领取权**。
    ///
    /// 挂起时不清 `claimed_by` 的话，§8.7 那两条语句里的 `claimed_by IS NULL` 永远筛不
    /// 到这一行——退避到期了、审批答复了，它也再没有人领得走。
    #[tokio::test]
    async fn a_run_waiting_for_its_backoff_can_be_claimed_again_when_it_is_due() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let run = queued_run(&db, "sess-suspend", "run-suspend").await;

        let claimed = queue
            .claim_run(&run, &ExecutorId::from_raw("exec-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.generation, 1);

        let due_at = OffsetDateTime::now_utc() - time::Duration::seconds(1);
        suspend(&db, &run, &retry_wait(due_at)).await;

        let record = crate::repos::runs::get(&db, &run).await.unwrap().unwrap();
        assert_eq!(record.state, RunState::Waiting);
        assert!(record.claimed_by.is_none(), "让出名额就要交还领取权");

        // 到期的候选查询看得到它（候选是 `queued` 与"到点的 `waiting + retry`"两种）。
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
    /// 它**不进**候选查询——候选只有 `queued` 与"到点的 `waiting + retry`"，等人回答的
    /// 那一行要等决定到达后被重新入队（§13.5）。所以这里断言的是"重新入队之后领得走"，
    /// 而不是"现在就领得走"：领取权有没有交还，正是这两者的分界。
    #[tokio::test]
    async fn a_run_waiting_for_an_approval_also_hands_back_its_claim() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let run = queued_run(&db, "sess-approval", "run-approval").await;
        queue
            .claim_run(&run, &ExecutorId::from_raw("exec-1"))
            .await
            .unwrap()
            .unwrap();

        suspend(&db, &run, &approval()).await;
        let record = crate::repos::runs::get(&db, &run).await.unwrap().unwrap();
        assert_eq!(record.state, RunState::Waiting);
        assert_eq!(record.wait, Some(approval()));
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

    /// 等一条审批（句柄是短 ID，这里只用它的 id）。
    fn approval() -> WaitReason {
        WaitReason::Approval {
            approval: ApprovalId::from_raw("ap-1"),
        }
    }

    /// 等到某个时刻的一次退避。
    fn retry_wait(at: OffsetDateTime) -> WaitReason {
        WaitReason::Retry {
            attempts: 1,
            not_before: at,
            cause: RetryCause::Transport,
        }
    }

    /// 让一行停下来等。
    async fn suspend(db: &Db, run: &RunId, wait: &WaitReason) {
        let (run, wait) = (run.clone(), wait.clone());
        db.with_write_retry(move |ex| {
            let run = run.clone();
            let wait = wait.clone();
            Box::pin(async move {
                crate::repos::runs::mark_waiting_in(
                    ex,
                    &run,
                    &wait,
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
        let run = queued_run(&db, "sess-backoff", "run-backoff").await;

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
                    &WaitReason::Retry {
                        attempts: 1,
                        not_before: later,
                        cause: RetryCause::Transport,
                    },
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

    /// 领取时就写下租约：`lease_until = claimed_at + 窗口`（§8.7）。
    ///
    /// 不写的话 `lease_until = 0`——"没租约"在回收那一侧读起来是**立刻过期**，一条正常
    /// 跑着的长调用会被当成没人管。
    #[tokio::test]
    async fn claiming_writes_a_lease_so_a_live_run_is_not_mistaken_for_abandoned() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::with_lease(db.clone(), time::Duration::minutes(2));
        let run = queued_run(&db, "sess-lease", "run-lease").await;
        let before = OffsetDateTime::now_utc();

        queue
            .claim_run(&run, &ExecutorId::from_raw("exec-1"))
            .await
            .unwrap()
            .unwrap();

        let row = crate::repos::runs::get(&db, &run).await.unwrap().unwrap();
        let lease = row.lease_until.expect("领取就写了租约");
        assert!(
            lease >= before + time::Duration::minutes(2),
            "租约是领取时刻 + 窗口：{lease}"
        );
        assert!(
            expired_lease_running(&db, before).await.unwrap().is_empty(),
            "刚领到的行不该是过期候选"
        );
    }

    /// 「哪些 Run 该被领」**只有一处定义**：`due` 的候选与 `claim_run` 的守卫逐个形状
    /// 一致（§8.7）。这一条是那句"别写两份谓词"的护栏。
    #[tokio::test]
    async fn due_and_claim_agree_on_what_is_claimable() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let now = OffsetDateTime::now_utc();
        let due_at = now - time::Duration::seconds(1);
        let later = now + time::Duration::hours(1);

        // 该领的：`queued`，以及到点的退避。
        let queued = queued_run(&db, "sess-a", "run-queued").await;
        let retry_due = orphan_run(&db, "sess-b", "run-retry-due").await;
        db.with_write_retry({
            let session = "sess-b";
            move |ex| {
                let session = session.to_string();
                Box::pin(async move {
                    crate::repos::session::ensure_in(
                        ex,
                        &SessionId::from_raw(session),
                        "api",
                        "p",
                        OffsetDateTime::now_utc(),
                    )
                    .await?;
                    toasty::sql::statement(
                        "UPDATE runs SET state = 'waiting', wait_kind = 'retry', \
                         retry_attempts = 1, wake_at = ?1 WHERE id = 'run-retry-due'",
                    )
                    .bind(crate::db::to_ts(due_at))
                    .exec(ex)
                    .await
                    .map(|_| ())
                    .map_err(map_toasty)
                }) as BoxFuture<'_, Result<(), StoreError>>
            }
        })
        .await
        .unwrap();

        // 不该领的：退避没到点、等人（审批 / 待判断）、等依赖、已终态。
        for (id, kind, at) in [
            ("run-retry-later", "retry", later),
            ("run-approval", "approval", now),
            ("run-intervention", "intervention", now),
            ("run-dependency", "dependency", now),
        ] {
            let run = orphan_run(&db, "sess-c", id).await;
            let _ = run;
            let id = id.to_string();
            let kind = kind.to_string();
            let at = crate::db::to_ts(at);
            db.with_write_retry(move |ex| {
                let (id, kind) = (id.clone(), kind.clone());
                Box::pin(async move {
                    toasty::sql::statement(
                        "UPDATE runs SET state = 'waiting', wait_kind = ?1, wait_ref = 'x', \
                         wake_at = ?2 WHERE id = ?3",
                    )
                    .bind(kind)
                    .bind(at)
                    .bind(id)
                    .exec(ex)
                    .await
                    .map(|_| ())
                    .map_err(map_toasty)
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .unwrap();
        }
        let done = queued_run(&db, "sess-d", "run-done").await;
        db.with_write_retry(|ex| {
            Box::pin(async move {
                toasty::sql::statement("UPDATE runs SET state = 'completed' WHERE id = 'run-done'")
                    .exec(ex)
                    .await
                    .map(|_| ())
                    .map_err(map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();

        let candidates = queue.due(now, 100).await.unwrap();
        let mut claimable = Vec::new();
        for run in [
            &queued,
            &retry_due,
            &done,
            &RunId::from_raw("run-retry-later"),
            &RunId::from_raw("run-approval"),
            &RunId::from_raw("run-intervention"),
            &RunId::from_raw("run-dependency"),
        ] {
            if queue
                .claim_run(run, &ExecutorId::from_raw(format!("exec-{run}")))
                .await
                .unwrap()
                .is_some()
            {
                claimable.push(run.clone());
            }
        }
        claimable.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let mut expected = vec![queued.clone(), retry_due.clone()];
        expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let mut candidates_sorted: Vec<RunId> = candidates
            .iter()
            .filter(|run| **run == queued || **run == retry_due)
            .cloned()
            .collect();
        candidates_sorted.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        assert_eq!(
            candidates_sorted, expected,
            "候选就是那两条（queued 与到点的退避）"
        );
        assert_eq!(
            claimable, expected,
            "**领取守卫与候选同一份谓词**：能领的正好是候选"
        );
    }

    /// **逐条**的租约回收：只在"过期的 + 自己持有的 + 还在 running"三条同时成立时交还。
    ///
    /// 整批那条做不到"只收我确实不再持有的那些"，而回收前必须过存活判定——所以 gateway
    /// 需要一个能逐条筛的口子。
    #[tokio::test]
    async fn a_single_expired_lease_is_reclaimed_only_for_its_own_holder() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::with_lease(db.clone(), time::Duration::minutes(2));
        let me = ExecutorId::from_raw("exec-me");
        let other = ExecutorId::from_raw("exec-other");
        let mine = queued_run(&db, "sess-mine", "run-mine").await;
        let theirs = queued_run(&db, "sess-theirs", "run-theirs").await;
        queue.claim_run(&mine, &me).await.unwrap().unwrap();
        queue.claim_run(&theirs, &other).await.unwrap().unwrap();

        let inside = OffsetDateTime::now_utc();
        assert!(
            !reclaim_lease(&db, &mine, &me, inside).await.unwrap(),
            "还在租约里：不是'没人管'"
        );
        assert!(
            !reclaim_lease(&db, &theirs, &other, inside).await.unwrap(),
            "同理"
        );

        let expired = inside + time::Duration::minutes(3);
        assert!(
            !reclaim_lease(&db, &theirs, &me, expired).await.unwrap(),
            "别人持有的那一条是 reclaim_unowned 的地盘"
        );
        assert!(
            reclaim_lease(&db, &mine, &me, expired).await.unwrap(),
            "过期的、自己持有的、还在 running → 交还"
        );
        let row = crate::repos::runs::get(&db, &mine).await.unwrap().unwrap();
        assert!(row.claimed_by.is_none(), "领取权交还了");
        assert_eq!(row.state, RunState::Running, "**交还领取权不判状态**");
        assert!(
            !reclaim_lease(&db, &mine, &me, expired).await.unwrap(),
            "再收一次已经是无主状态，什么都不改"
        );
    }

    /// 把一条 Run 改成"在等某条 Run"。
    async fn wait_on_run(db: &Db, run: &RunId, on: &RunId) {
        let (run, on) = (run.clone(), on.clone());
        db.with_write_retry(move |ex| {
            let (run, on) = (run.clone(), on.clone());
            Box::pin(async move {
                crate::repos::runs::mark_waiting_in(
                    ex,
                    &run,
                    &WaitReason::Dependency { run: on },
                    None,
                    OffsetDateTime::now_utc(),
                )
                .await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
    }

    /// 把一条 Run 收成终态。
    async fn mark_terminal(db: &Db, run: &RunId) {
        let run = run.to_string();
        db.with_write_retry(move |ex| {
            let run = run.clone();
            Box::pin(async move {
                toasty::sql::statement(
                    "UPDATE runs SET state = 'completed', wait_kind = NULL, wait_ref = NULL, \
                     wake_at = 0 WHERE id = ?1",
                )
                .bind(run)
                .exec(ex)
                .await
                .map(|_| ())
                .map_err(map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
    }

    /// **`queued` ⇒ 可领**这条不变量在 A/B/C/D 那个形状下也成立。
    ///
    /// A(未终态) < B(早已终态) < C(未终态)，D 等 C。C 终态时 D **不能**变成 `queued`：
    /// 领取守卫查的是"任何更早的非终态 Run"，A 还在，所以那样会留下"显示排队中却一直不
    /// 动"的行。正确的落点是**改指到 A**，等 A 也终态了才真的进队列。
    #[tokio::test]
    async fn a_satisfied_dependency_repoints_instead_of_queueing_behind_an_earlier_run() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let executor = ExecutorId::from_raw("exec-1");
        let now = OffsetDateTime::now_utc();
        let a = queued_run_with_seq(&db, "sess-chain", "run-a", 1).await;
        let b = queued_run_with_seq(&db, "sess-chain", "run-b", 2).await;
        let c = queued_run_with_seq(&db, "sess-chain", "run-c", 3).await;
        let d = queued_run_with_seq(&db, "sess-chain", "run-d", 4).await;
        mark_terminal(&db, &b).await; // B 早就不在跑了（比如被取消）
        wait_on_run(&db, &d, &c).await; // D 在等 C

        // C 终态：还有 A 挡着 → **改指**，不是放行。
        mark_terminal(&db, &c).await;
        assert_eq!(
            release_satisfied_dependencies(&db, now).await.unwrap(),
            0,
            "还有更早的非终态 Run，一条都不该放进队列"
        );
        let row = crate::repos::runs::get(&db, &d).await.unwrap().unwrap();
        assert_eq!(row.state, RunState::Waiting, "不许出现'排队中却领不走'");
        assert_eq!(
            row.wait,
            Some(WaitReason::Dependency { run: a.clone() }),
            "改指到紧挨着的那条更早非终态 Run"
        );

        // A 也终态：这一次才真的进队列，而且 `due` 领得到。
        mark_terminal(&db, &a).await;
        assert_eq!(release_satisfied_dependencies(&db, now).await.unwrap(), 1);
        let row = crate::repos::runs::get(&db, &d).await.unwrap().unwrap();
        assert_eq!(row.state, RunState::Queued);
        assert!(row.wait.is_none());
        assert!(queue.due(now, 10).await.unwrap().contains(&d));
        assert!(
            queue.claim_run(&d, &executor).await.unwrap().is_some(),
            "`queued` 的定义就是'现在就能跑'"
        );
    }

    /// 依赖等到了就放行：前置 Run 进终态 → 后面那条回 `queued`（§8.4）。
    #[tokio::test]
    async fn a_dependency_wait_moves_on_once_the_earlier_run_ends() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let earlier = queued_run(&db, "sess-1", "run-0001").await;
        let later = orphan_run(&db, "sess-1", "run-0002").await;
        db.with_write_retry({
            let earlier = earlier.to_string();
            move |ex| {
                let earlier = earlier.clone();
                Box::pin(async move {
                    toasty::sql::statement(
                        "UPDATE runs SET state = 'waiting', wait_kind = 'dependency', \
                         wait_ref = ?1 WHERE id = 'run-0002'",
                    )
                    .bind(earlier)
                    .exec(ex)
                    .await
                    .map(|_| ())
                    .map_err(map_toasty)
                }) as BoxFuture<'_, Result<(), StoreError>>
            }
        })
        .await
        .unwrap();

        assert_eq!(
            release_satisfied_dependencies(&db, OffsetDateTime::now_utc())
                .await
                .unwrap(),
            0,
            "前置还在排队，不放行"
        );
        assert!(
            queue
                .claim_run(&later, &ExecutorId::from_raw("exec-1"))
                .await
                .unwrap()
                .is_none(),
            "它在等前一条 Run"
        );

        db.with_write_retry(move |ex| {
            Box::pin(async move {
                toasty::sql::statement("UPDATE runs SET state = 'completed' WHERE id = 'run-0001'")
                    .exec(ex)
                    .await
                    .map(|_| ())
                    .map_err(map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();

        assert_eq!(
            release_satisfied_dependencies(&db, OffsetDateTime::now_utc())
                .await
                .unwrap(),
            1
        );
        let row = crate::repos::runs::get(&db, &later).await.unwrap().unwrap();
        assert_eq!(row.state, RunState::Queued);
        assert!(row.wait.is_none());
        assert!(
            queue
                .claim_run(&later, &ExecutorId::from_raw("exec-2"))
                .await
                .unwrap()
                .is_some(),
            "放行之后领得走"
        );
        // 幂等：再跑一拍没有可放行的。
        assert_eq!(
            release_satisfied_dependencies(&db, OffsetDateTime::now_utc())
                .await
                .unwrap(),
            0
        );
    }

    /// 验收 ⑩ 的第一半：**会话不在服务范围里，Run 一个都领不走**（§8.9 最后一段）。
    ///
    /// 四种形状一起测：`deleted` / `purged` / 会话行根本不在（"目录被手工删掉、Run 还在
    /// 队列里"）都不领；`closing` 仍然领得到——它只是不再收新活，手上的活要跑完。候选
    /// 查询与领取语句各自都要有这条守卫，所以 `due` 与 `claim_run` 两个入口都断言。
    #[tokio::test]
    async fn a_run_whose_session_is_not_served_is_never_claimed() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());

        let orphan = orphan_run(&db, "sess-gone", "run-orphan").await;
        let deleted = queued_run(&db, "sess-deleted", "run-deleted").await;
        session_state(
            &db,
            "sess-deleted",
            SessionState::Active,
            SessionState::Closing,
        )
        .await;
        session_state(
            &db,
            "sess-deleted",
            SessionState::Closing,
            SessionState::Deleted,
        )
        .await;
        let purged = queued_run(&db, "sess-purged", "run-purged").await;
        session_state(
            &db,
            "sess-purged",
            SessionState::Active,
            SessionState::Closing,
        )
        .await;
        session_state(
            &db,
            "sess-purged",
            SessionState::Closing,
            SessionState::Deleted,
        )
        .await;
        session_state(
            &db,
            "sess-purged",
            SessionState::Deleted,
            SessionState::Purged,
        )
        .await;
        let closing = queued_run(&db, "sess-closing", "run-closing").await;
        session_state(
            &db,
            "sess-closing",
            SessionState::Active,
            SessionState::Closing,
        )
        .await;

        let due = queue.due(OffsetDateTime::now_utc(), 10).await.unwrap();
        for run in [&orphan, &deleted, &purged] {
            assert!(!due.contains(run), "{run} 的会话不服务，不该是候选");
        }
        assert!(due.contains(&closing), "closing 还在服务范围里");

        let executor = ExecutorId::from_raw("exec-1");
        for run in [&orphan, &deleted, &purged] {
            assert!(
                queue.claim_run(run, &executor).await.unwrap().is_none(),
                "{run} 不该被领走"
            );
        }
        assert_eq!(
            queue
                .claim_run(&closing, &executor)
                .await
                .unwrap()
                .expect("closing 领得到")
                .run,
            closing
        );
    }

    /// 会话回到 `active` 才重新领得走：守卫读的是**当下的**状态，不是领取那一刻的快照。
    #[tokio::test]
    async fn a_run_waits_while_its_session_is_closing_and_moves_again_once_active() {
        let (db, _dir) = temp().await;
        let queue = TursoRunQueue::new(db.clone());
        let run = queued_run(&db, "sess-back", "run-back").await;
        session_state(
            &db,
            "sess-back",
            SessionState::Active,
            SessionState::Closing,
        )
        .await;
        session_state(
            &db,
            "sess-back",
            SessionState::Closing,
            SessionState::Deleted,
        )
        .await;

        let executor = ExecutorId::from_raw("exec-1");
        assert!(queue.claim_run(&run, &executor).await.unwrap().is_none());

        session_state(
            &db,
            "sess-back",
            SessionState::Deleted,
            SessionState::Active,
        )
        .await;
        assert_eq!(
            queue
                .claim_run(&run, &executor)
                .await
                .unwrap()
                .expect("回到 active 之后领得走")
                .run,
            run
        );
    }
}
