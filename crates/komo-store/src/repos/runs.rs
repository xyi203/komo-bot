//! `runs` 的行读写：store 内部的具体类型，只被 Coordinator 与队列用（§13.5）。
//!
//! 领取与代次围栏在 [`super::queue`]（那里要受影响行数，所以走 raw SQL）；这里是别的
//! 一切——建行、记事件引用、记终态、按请求键去重。

use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{EventId, ExecutorId, RequestKey, RunId, Seq, SessionId};
use komo_kernel::types::memory::MemoryWork;
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::status::RunStatus;
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, decode, encode, map_toasty, to_ts};
use crate::models::RunRow;

/// 一个 Run，读出来的样子。
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub run: RunId,
    pub session: SessionId,
    pub request_key: RequestKey,
    pub input_hash: String,
    pub input_event: Option<EventId>,
    pub final_event: Option<EventId>,
    pub status: RunStatus,
    pub source: PlanSource,
    pub peer: Option<String>,
    pub claimed_by: Option<String>,
    pub claim_generation: u64,
    pub retry_attempts: u32,
    pub next_retry_at: Option<OffsetDateTime>,
    pub rounds: u32,
    pub model: ModelConfig,
    pub memory_work: MemoryWork,
    /// 记忆处理游标：这个 Run 的证据已经处理到哪条 seq（§9.3）。
    pub memory_cursor: Seq,
    pub last_error: Option<String>,
}

impl RunRecord {
    fn try_from_row(row: &RunRow) -> Result<RunRecord, StoreError> {
        Ok(RunRecord {
            run: RunId::from_raw(row.id.clone()),
            session: SessionId::from_raw(row.session_id.clone()),
            request_key: RequestKey::new(row.request_key.clone()),
            input_hash: row.input_hash.clone(),
            input_event: row.input_event.clone().map(EventId::from_raw),
            final_event: row.final_event.clone().map(EventId::from_raw),
            status: decode(&format!("\"{}\"", row.status), "runs.status")?,
            source: decode(&row.source, "runs.source")?,
            peer: row.peer.clone(),
            claimed_by: row.claimed_by.clone(),
            claim_generation: row.claim_generation.max(0) as u64,
            retry_attempts: row.retry_attempts.max(0) as u32,
            next_retry_at: crate::db::from_ts_opt(row.next_retry_at),
            rounds: row.rounds.max(0) as u32,
            model: decode(&row.model_snapshot, "runs.model_snapshot")?,
            memory_work: decode(&format!("\"{}\"", row.memory_work), "runs.memory_work")?,
            memory_cursor: Seq(row.memory_cursor.max(0) as u64),
            last_error: row.last_error.clone(),
        })
    }
}

/// 一个新 Run 的初始内容。
#[derive(Debug, Clone)]
pub struct NewRun {
    pub run: RunId,
    pub session: SessionId,
    pub request_key: RequestKey,
    pub input_hash: String,
    pub source: PlanSource,
    pub peer: Option<String>,
    pub model: ModelConfig,
    pub effort: Option<String>,
    pub at: OffsetDateTime,
}

/// 一行的状态枚举。列里存的是 `snake_case` 字符串。
pub fn status_of(row: &RunRow) -> Result<RunStatus, StoreError> {
    decode(&format!("\"{}\"", row.status), "runs.status")
}

/// 把状态枚举写成列里的那个字符串。
pub fn status_str(status: RunStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("RunStatus 序列化成一个字符串")
}

fn memory_work_str(work: MemoryWork) -> String {
    serde_json::to_value(work)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("MemoryWork 序列化成一个字符串")
}

/// 用请求键预留一个 Run，状态 `ingesting`，**只存输入哈希与来源**（§8.5 第一段箭头）。
pub async fn reserve_in(ex: &mut dyn Executor, new: &NewRun) -> Result<RunRow, StoreError> {
    toasty::create!(RunRow {
        id: new.run.as_str(),
        session_id: new.session.as_str(),
        request_key: new.request_key.as_str(),
        input_hash: new.input_hash.clone(),
        input_event: None as Option<String>,
        final_event: None as Option<String>,
        status: status_str(RunStatus::Ingesting),
        source: encode(&new.source)?,
        peer: new.peer.clone(),
        claimed_by: None as Option<String>,
        claim_generation: 0_i64,
        claimed_at: 0_i64,
        next_retry_at: 0_i64,
        retry_attempts: 0_i64,
        rounds: 0_i64,
        max_rounds: 0_i64,
        valid_until: 0_i64,
        model_snapshot: encode(&new.model)?,
        effort: new.effort.clone(),
        grants: "[]".to_string(),
        memory_work: memory_work_str(MemoryWork::Pending),
        memory_cursor: 0_i64,
        last_error: None as Option<String>,
        created_at: to_ts(new.at),
        updated_at: to_ts(new.at),
        ended_at: 0_i64,
    })
    .exec(ex)
    .await
    .map_err(map_toasty)
}

/// 按请求键找已有的 Run——同一键重发返回原 Run（§8.5）。
pub async fn find_by_request_key_in(
    ex: &mut dyn Executor,
    key: &RequestKey,
) -> Result<Option<RunRow>, StoreError> {
    RunRow::filter(RunRow::fields().request_key().eq(key.as_str()))
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)
}

pub async fn get_in(ex: &mut dyn Executor, run: &RunId) -> Result<Option<RunRow>, StoreError> {
    RunRow::filter_by_id(run.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 只记下承载输入的事件，**不动状态**。
///
/// 回放补索引用它：`run.accepted` 在日志里说明"输入已经完整落盘"，但这一行现在是
/// `cancelled` 还是 `waiting_approval`，是数据库自己的事——「回放只补索引与派生执行状态，
/// 不能覆盖 state.db 已记录的用户取消或权限撤销」（§8.5）。
pub async fn set_input_event_in(
    ex: &mut dyn Executor,
    run: &RunId,
    input_event: &EventId,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let mut row = require(ex, run).await?;
    if row.input_event.as_deref() == Some(input_event.as_str()) {
        return Ok(());
    }
    row.update()
        .input_event(Some(input_event.to_string()))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 记下承载输入的事件，并把状态推成 `queued`——**这一步之后才能向客户端确认已接收**。
pub async fn mark_queued_in(
    ex: &mut dyn Executor,
    run: &RunId,
    input_event: &EventId,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let mut row = require(ex, run).await?;
    row.update()
        .input_event(Some(input_event.to_string()))
        .status(status_str(RunStatus::Queued))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 某个执行实例开跑：确认领取代次仍是自己的，然后把状态坐实成 `running`。
///
/// 返回 `Ok(None)` = 坐实了；`Ok(Some(current))` = **自己已经是旧代次**，当前是
/// `current`。§8.7：这时候要停止这个任务的一切写入，不重试、不降级。
///
/// 领取（[`super::queue::TursoRunQueue::claim`] / `claim_run`）只改行——它是一条条件
/// UPDATE，`rows affected` 是胜负的唯一信号，所以它不写事件。事件由 handler 在真正开跑
/// 时通过 `Ledger::start_run` 写，于是 JSONL 上的 `run.started` 记的是"谁开始跑了"，而
/// 不是"谁抢到了名额"——中间可能隔着一次失败的接管。
///
/// 这里是**读—核对—写**，整段在一个 `BEGIN CONCURRENT` 事务里：并发的代次递增会让这
/// 个事务提交失败并干净重跑，重跑时读到的就是新代次。
pub async fn start_in(
    ex: &mut dyn Executor,
    run: &RunId,
    executor: &ExecutorId,
    generation: u64,
    now: OffsetDateTime,
) -> Result<Option<u64>, StoreError> {
    let mut row = require(ex, run).await?;
    let current = row.claim_generation.max(0) as u64;
    if current != generation || row.claimed_by.as_deref() != Some(executor.as_str()) {
        return Ok(Some(current));
    }
    row.update()
        .status(status_str(RunStatus::Running))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)?;
    Ok(None)
}

/// 让出执行名额时的状态提交（`waiting_approval` / `waiting_retry` / `needs_attention`）。
///
/// **一并把 `claimed_by` 清掉**：让出执行名额就是交还领取权（§7.4「停在一条审批上，已
/// 释放执行名额」）。不清的话 §8.7 的候选查询里那个 `claimed_by IS NULL` 永远筛不到这
/// 一行——等审批答复了、退避到期了，它也再没有人领得走，一个"等一会儿"就变成了永久
/// 停摆。
pub async fn mark_waiting_in(
    ex: &mut dyn Executor,
    run: &RunId,
    status: RunStatus,
    attempts: u32,
    next_retry_at: Option<OffsetDateTime>,
    reason: Option<String>,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let mut row = require(ex, run).await?;
    row.update()
        .status(status_str(status))
        .claimed_by(None as Option<String>)
        .retry_attempts(i64::from(attempts))
        .next_retry_at(crate::db::to_ts_opt(next_retry_at))
        .last_error(reason)
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 终态，连同最终事件引用与 Memory 待处理标记（§8.5 的"结束任务"）。
pub async fn mark_final_in(
    ex: &mut dyn Executor,
    run: &RunId,
    status: RunStatus,
    final_event: &EventId,
    rounds: u32,
    reason: Option<String>,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let mut row = require(ex, run).await?;
    row.update()
        .status(status_str(status))
        .final_event(Some(final_event.to_string()))
        .rounds(i64::from(rounds))
        .last_error(reason)
        // 「run.completed 在 JSONL 持久保存后，state.db 提交终态与 memory_work =
        // pending」（§9.3）。取消与失败也标：后台自己判断有没有可提取的证据。
        .memory_work(memory_work_str(MemoryWork::Pending))
        .ended_at(to_ts(now))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 记这一轮之后的轮次计数。
pub async fn bump_rounds_in(
    ex: &mut dyn Executor,
    run: &RunId,
    rounds: u32,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let mut row = require(ex, run).await?;
    row.update()
        .rounds(i64::from(rounds))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 读一个 Run。
pub async fn get(db: &Db, run: &RunId) -> Result<Option<RunRecord>, StoreError> {
    let id = run.clone();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let Some(row) = get_in(ex, &id).await? else {
                return Ok(None);
            };
            Ok(Some(RunRecord::try_from_row(&row)?))
        }) as BoxFuture<'_, Result<Option<RunRecord>, StoreError>>
    })
    .await
}

/// 一个 Session 的全部 Run，按 id（= 创建时间）排序。
pub async fn list_for_session(db: &Db, session: &SessionId) -> Result<Vec<RunRecord>, StoreError> {
    let id = session.to_string();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let rows = RunRow::filter(RunRow::fields().session_id().eq(id.as_str()))
                .exec(ex)
                .await
                .map_err(map_toasty)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                out.push(RunRecord::try_from_row(row)?);
            }
            out.sort_by(|a, b| a.run.as_str().cmp(b.run.as_str()));
            Ok(out)
        }) as BoxFuture<'_, Result<Vec<RunRecord>, StoreError>>
    })
    .await
}

/// 未完成的 Run——启动扫描要的就是这一批（§8.7）。
pub async fn unfinished(db: &Db) -> Result<Vec<RunRecord>, StoreError> {
    db.read(move |ex| {
        Box::pin(async move {
            let rows = RunRow::all().exec(ex).await.map_err(map_toasty)?;
            let mut out = Vec::new();
            for row in &rows {
                let record = RunRecord::try_from_row(row)?;
                if record.status.is_unfinished() {
                    out.push(record);
                }
            }
            out.sort_by(|a, b| a.run.as_str().cmp(b.run.as_str()));
            Ok(out)
        }) as BoxFuture<'_, Result<Vec<RunRecord>, StoreError>>
    })
    .await
}

/// 已经终态、但结果还没送到客户端的 Run（§8.4 第 10 行）。
///
/// `delivered` 是"已经送过了"的那一组 Run id，由调用方查 `deliveries` 得出——这里不自
/// 己去查，因为同一轮扫描的两批行要用**同一份**送达观察。
pub async fn terminal_undelivered(
    db: &Db,
    delivered: &std::collections::BTreeSet<String>,
) -> Result<Vec<RunRecord>, StoreError> {
    let delivered = delivered.clone();
    db.read(move |ex| {
        let delivered = delivered.clone();
        Box::pin(async move {
            let rows = RunRow::all().exec(ex).await.map_err(map_toasty)?;
            let mut out = Vec::new();
            for row in &rows {
                if delivered.contains(&row.id) {
                    continue;
                }
                let record = RunRecord::try_from_row(row)?;
                if record.status.is_terminal() {
                    out.push(record);
                }
            }
            Ok(out)
        }) as BoxFuture<'_, Result<Vec<RunRecord>, StoreError>>
    })
    .await
}

async fn require(ex: &mut dyn Executor, run: &RunId) -> Result<RunRow, StoreError> {
    get_in(ex, run).await?.ok_or_else(|| StoreError::NotFound {
        what: format!("run {run}"),
    })
}

// ---------------------------------------------------------------- Memory 处理游标（§9.3）

/// 领取一批待处理的 Run：**终态 + `memory_work = pending`**，领到就翻成 `processing`。
///
/// 领取是"读—核对—写"，整段在 `with_write_retry` 的事务里：两个消费者同时扫到同一行
/// 时，后提交的那个会冲突重跑，重跑时读到的已经是 `processing`，于是只有一个领到。
/// 「进程崩溃后可重新领取」（§9.3）由 [`requeue_stuck_memory_work`] 在启动时完成——
/// 一个 `processing` 但没人在处理的行，和一个从未处理过的行是同一件事。
pub async fn claim_memory_work(db: &Db, limit: usize) -> Result<Vec<RunRecord>, StoreError> {
    let limit = limit.max(1);
    db.with_write_retry(move |ex| {
        Box::pin(async move {
            let rows = RunRow::all().exec(ex).await.map_err(map_toasty)?;
            let mut candidates: Vec<RunRow> = Vec::new();
            for row in rows {
                let status = status_of(&row)?;
                if !status.is_terminal() {
                    continue;
                }
                if row.memory_work != memory_work_str(MemoryWork::Pending) {
                    continue;
                }
                candidates.push(row);
            }
            candidates.sort_by(|a, b| a.id.cmp(&b.id));
            candidates.truncate(limit);

            let mut claimed = Vec::with_capacity(candidates.len());
            for mut row in candidates {
                let record = RunRecord::try_from_row(&row)?;
                row.update()
                    .memory_work(memory_work_str(MemoryWork::Processing))
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                claimed.push(RunRecord {
                    memory_work: MemoryWork::Processing,
                    ..record
                });
            }
            Ok(claimed)
        }) as BoxFuture<'_, Result<Vec<RunRecord>, StoreError>>
    })
    .await
}

/// 处理完：记状态与游标。**失败时调用方传 `Pending` 且不推进游标**——「失败不推进、
/// 下次重试」（§9.3），而"重试"这件事在这张表上就是把标记放回 pending。
pub async fn finish_memory_work(
    db: &Db,
    run: &RunId,
    work: MemoryWork,
    cursor: Seq,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let run = run.clone();
    db.with_write_retry(move |ex| {
        let run = run.clone();
        Box::pin(async move {
            let mut row = require(ex, &run).await?;
            // 游标只进不退：重复处理相同来源不重复新增（§9.3）。
            let cursor = row.memory_cursor.max(cursor.0 as i64);
            row.update()
                .memory_work(memory_work_str(work))
                .memory_cursor(cursor)
                .updated_at(to_ts(now))
                .exec(ex)
                .await
                .map_err(map_toasty)?;
            Ok(())
        }) as BoxFuture<'_, Result<(), StoreError>>
    })
    .await
}

/// 启动时把卡在 `processing` 的行放回 `pending`（§9.3「进程崩溃后可重新领取」）。
///
/// 返回放回了几行。`processing` 是**进程内**的状态：没有哪个进程还握着上一次的那一批，
/// 所以启动时它一定是"上次没跑完"，而不是"别人正在跑"——Gateway 只有一个实例（§8.7）。
pub async fn requeue_stuck_memory_work(db: &Db) -> Result<u64, StoreError> {
    db.with_write_retry(move |ex| {
        Box::pin(async move {
            let rows = RunRow::all().exec(ex).await.map_err(map_toasty)?;
            let processing = memory_work_str(MemoryWork::Processing);
            let mut requeued: u64 = 0;
            for mut row in rows {
                if row.memory_work != processing {
                    continue;
                }
                row.update()
                    .memory_work(memory_work_str(MemoryWork::Pending))
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                requeued += 1;
            }
            Ok(requeued)
        }) as BoxFuture<'_, Result<u64, StoreError>>
    })
    .await
}
