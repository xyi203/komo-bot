//! `runs` 的行读写：store 内部的具体类型，只被 Coordinator 与队列用（§13.5）。
//!
//! 领取与代次围栏在 [`super::queue`]（那里要受影响行数，所以走 raw SQL）；这里是别的
//! 一切——建行、记事件引用、记终态、按请求键去重。

use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{EventId, RequestKey, RunId, SessionId};
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

/// 让出执行名额时的状态提交（`waiting_approval` / `waiting_retry` / `needs_attention`）。
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

async fn require(ex: &mut dyn Executor, run: &RunId) -> Result<RunRow, StoreError> {
    get_in(ex, run).await?.ok_or_else(|| StoreError::NotFound {
        what: format!("run {run}"),
    })
}
