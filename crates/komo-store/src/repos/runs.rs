//! `runs` 的行读写：store 内部的具体类型，只被 Coordinator 与队列用（§13.5）。
//!
//! 领取与代次围栏在 [`super::queue`]（那里要受影响行数，所以走 raw SQL）；这里是别的
//! 一切——建行、记事件引用、记终态、按请求键去重。

use komo_kernel::traits::StoreError;
use komo_kernel::types::delegate::DelegateSpec;
use komo_kernel::types::ids::{
    ApprovalId, EventId, ExecutorId, InterventionId, RequestKey, RunId, Seq, SessionId,
};
use komo_kernel::types::memory::MemoryWork;
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::status::{RetryCause, RunState, WaitReason};
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
    /// 承载输入那条事件的 `seq`（§8.3）：**同 Session 内次序的权威**。`Seq(0)` = 未知。
    pub input_seq: Seq,
    pub final_event: Option<EventId>,
    /// 派它的那条 Run；`None` = 顶层 Run（§8.4 的委派子 Run）。
    pub parent: Option<RunId>,
    /// 受理时那份委派计划；读不出来（旧行 / 损坏行）时是 `None`，不猜（[`delegate_of`]）。
    pub delegate: Option<DelegateSpec>,
    /// 调度状态（§8.4）。
    pub state: RunState,
    /// `state == Waiting` 时在等什么（§8.4）。
    pub wait: Option<WaitReason>,
    pub source: PlanSource,
    pub peer: Option<String>,
    pub claimed_by: Option<String>,
    pub claim_generation: u64,
    pub retry_attempts: u32,
    /// `wait_kind = 'retry'` 的到点时刻（`wake_at` 列的哨兵 `0` 读成 `None`）。
    pub wake_at: Option<OffsetDateTime>,
    /// 心跳租约的到期时刻（`lease_until` 列的哨兵 `0` 读成 `None` = 没租约）。
    pub lease_until: Option<OffsetDateTime>,
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
            input_seq: Seq(row.input_seq.max(0) as u64),
            final_event: row.final_event.clone().map(EventId::from_raw),
            parent: row.parent_run_id.clone().map(RunId::from_raw),
            delegate: delegate_of(row),
            state: state_of(row)?,
            wait: wait_of(row)?,
            source: decode(&row.source, "runs.source")?,
            peer: row.peer.clone(),
            claimed_by: row.claimed_by.clone(),
            claim_generation: row.claim_generation.max(0) as u64,
            retry_attempts: row.retry_attempts.max(0) as u32,
            wake_at: crate::db::from_ts_opt(row.wake_at),
            lease_until: crate::db::from_ts_opt(row.lease_until),
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
    /// 这条 Run 是一次委派的子 Run 时，父侧那一份计划（§8.4）。`None` = 顶层 Run。
    pub delegate: Option<DelegateSpec>,
    pub peer: Option<String>,
    pub model: ModelConfig,
    pub effort: Option<String>,
    pub at: OffsetDateTime,
}

/// 一行的调度状态。**认不出就是损坏**，不默认成 `queued`——那会让一条不该跑的 Run
/// 被领走（`RunState::parse` 的同一句话）。
pub fn state_of(row: &RunRow) -> Result<RunState, StoreError> {
    RunState::parse(&row.state).ok_or_else(|| {
        StoreError::Corrupt(format!(
            "runs.state 认不出的值 {:?}（Run {}）：只认 accepted / queued / running / \
             waiting / completed / failed / cancelled / abandoned",
            row.state, row.id
        ))
    })
}

/// 一行停在什么上（§8.4）。`wait_kind` 为空 = 不在等待。
///
/// **等待三列要自洽**：`wait_kind` 认不出、或者指到了具体对象却没写下 `wait_ref`，都是
/// 损坏——把一条"在等什么说不清"的 Run 当正常读回去，正是这次改造要消掉的那种形状。
pub fn wait_of(row: &RunRow) -> Result<Option<WaitReason>, StoreError> {
    let Some(kind) = row.wait_kind.as_deref() else {
        return Ok(None);
    };
    let reference = |what: &str| -> Result<String, StoreError> {
        row.wait_ref.clone().ok_or_else(|| {
            StoreError::Corrupt(format!(
                "runs.wait_kind = {what} 却没有 runs.wait_ref（Run {}）",
                row.id
            ))
        })
    };
    Ok(Some(match kind {
        "approval" => WaitReason::Approval {
            approval: ApprovalId::from_raw(reference("approval")?),
        },
        "intervention" => WaitReason::Intervention {
            intervention: InterventionId::from_raw(reference("intervention")?),
        },
        "dependency" => WaitReason::Dependency {
            run: RunId::from_raw(reference("dependency")?),
        },
        "retry" => WaitReason::Retry {
            attempts: row.retry_attempts.max(0) as u32,
            not_before: crate::db::from_ts(row.wake_at),
            // 哪一种失败必须回写得出来：`WaitReason::Retry.cause` 是必填字段，
            // 编一个默认值就是"替它猜"。见 [`wait_ref_str`]。
            cause: row
                .wait_ref
                .as_deref()
                .and_then(RetryCause::parse)
                .ok_or_else(|| {
                    StoreError::Corrupt(format!(
                        "runs.wait_kind = retry 却没写下退避原因（runs.wait_ref = {:?}，Run {}）",
                        row.wait_ref, row.id
                    ))
                })?,
        },
        other => {
            return Err(StoreError::Corrupt(format!(
                "runs.wait_kind 认不出的值 {other:?}（Run {}）：只认 approval / retry / \
                 intervention / dependency",
                row.id
            )));
        }
    }))
}

/// 写进 `runs.state` 的那一段。
pub fn state_str(state: RunState) -> String {
    state.as_str().to_string()
}

/// 一行身上那份委派计划（§8.4）。**读不出来不算损坏**：当作"没有契约"放行，只告警。
///
/// 与 [`state_of`] / [`wait_of`] 的严厉相反，这里的理由是这两列**不参与调度判定**：
/// 能不能领、谁挡着谁看的是 `state` / `wait_*` / `parent_run_id`，而 `delegate` 那一列
/// 只影响"父侧复验时用哪份契约"。为一份读不出来的契约把整条 Run 判成损坏，代价是这条
/// Run 再也恢复不了（`unfinished` 扫到它就报错，恢复流程整批停摆），换来的只是把一次
/// "按自由文本处理"升级成"停摆"——**代价大于收益**。
///
/// 两种来源：旧行（这一列还是 NULL，这次改造之前受理的）与损坏行（写坏了的 JSON）。
/// 前者根本不告警——它不是异常，是历史。
pub fn delegate_of(row: &RunRow) -> Option<DelegateSpec> {
    let raw = row.delegate.as_deref()?;
    match serde_json::from_str::<DelegateSpec>(raw) {
        Ok(spec) => Some(spec),
        Err(error) => {
            tracing::warn!(
                run = %row.id,
                %error,
                "runs.delegate 解析不出来，当作没有契约"
            );
            None
        }
    }
}

/// 写进 `runs.wait_ref` 的那一段。
///
/// kernel 的 [`WaitReason::reference`] 对 `retry` 给 `None`（它认为退避的进度就是次数 +
/// 到点时刻），但 [`WaitReason::Retry`] 的 `cause` 是必填字段：不回写它，读回来那条
/// `WaitReason::Retry` 就少了"是哪一种失败"，而 `wait_of` 拒绝替它挑一个。所以 `retry`
/// 把 `cause` 写进 `wait_ref`——`wait_of` 用 [`RetryCause::parse`] 认它。
pub fn wait_ref_str(wait: &WaitReason) -> Option<String> {
    match wait {
        WaitReason::Retry { cause, .. } => Some(cause.as_str().to_string()),
        other => other.reference(),
    }
}

/// 把一条 Run 写进等待：状态、等待三列、重试计数、原因，与**交还领取权**同一个提交。
///
/// 交还领取权是这一条的一部分（§7.4「停在一条审批上，已释放执行名额」）：不清
/// `claimed_by`，§8.7 的候选查询里那个 `claimed_by IS NULL` 永远筛不到这一行——审批答复
/// 了、退避到期了，它也再没有人领得走，一个"等一会儿"就变成了永久停摆。
async fn write_wait_in(
    ex: &mut dyn Executor,
    row: &mut RunRow,
    wait: &WaitReason,
    reason: Option<String>,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let retry_attempts = match wait {
        WaitReason::Retry { attempts, .. } => i64::from(*attempts),
        _ => row.retry_attempts,
    };
    row.update()
        .state(state_str(wait.state()))
        .wait_kind(Some(wait.kind().to_string()))
        .wait_ref(wait_ref_str(wait))
        .wake_at(wait.wake_at().map(to_ts).unwrap_or(0))
        .retry_attempts(retry_attempts)
        .claimed_by(None as Option<String>)
        .last_error(reason)
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

fn memory_work_str(work: MemoryWork) -> String {
    serde_json::to_value(work)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("MemoryWork 序列化成一个字符串")
}

/// 用请求键预留一个 Run，状态 `accepted`，**只存输入哈希与来源**（§8.5 第一段箭头）。
///
/// `accepted` 不是"可以跑了"：输入正文可能还没写完，所以它**不可领取**。`mark_queued_in`
/// 才是那一步。
pub async fn reserve_in(ex: &mut dyn Executor, new: &NewRun) -> Result<RunRow, StoreError> {
    toasty::create!(RunRow {
        id: new.run.as_str(),
        session_id: new.session.as_str(),
        request_key: new.request_key.as_str(),
        input_hash: new.input_hash.clone(),
        input_event: None as Option<String>,
        // 受理那一步（[`mark_queued_in`]）才拿得到输入事件的 seq（§8.3）。
        input_seq: 0_i64,
        final_event: None as Option<String>,
        // 委派的两列在**预留**那一刻就写下来：受理与领取都要看它们（`mark_queued_in` 判
        // "这是子 Run，直接进队列"，§8.7 的领取语句判"拦住我的那条是不是我的父"），
        // 而这两步之间没有任何写者会再补——留到受理那一步写就是在开一个窗口。
        parent_run_id: new.delegate.as_ref().map(|spec| spec.parent.to_string()),
        delegate: new.delegate.as_ref().map(|spec| encode(spec)).transpose()?,
        // 退役列：不再读，写入给空值（§8.2 只允许加列）。
        status: String::new(),
        state: state_str(RunState::Accepted),
        wait_kind: None as Option<String>,
        wait_ref: None as Option<String>,
        wake_at: 0_i64,
        lease_until: 0_i64,
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
/// `cancelled` 还是停在等待上，是数据库自己的事——「回放只补索引与派生执行状态，
/// 不能覆盖 state.db 已记录的用户取消或权限撤销」（§8.5）。
pub async fn set_input_event_in(
    ex: &mut dyn Executor,
    run: &RunId,
    input_event: &EventId,
    seq: Seq,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let mut row = require(ex, run).await?;
    let seq = i64::try_from(seq.0).unwrap_or(i64::MAX);
    // 引用与 seq 一起补齐：老行（这次改造之前受理的）`input_seq` 是 0，而回放手里正好有
    // 那条事件的 seq——不补，它的次序会一直退化到按 id 比（§8.3）。
    if row.input_event.as_deref() == Some(input_event.as_str()) && row.input_seq == seq {
        return Ok(());
    }
    row.update()
        .input_event(Some(input_event.to_string()))
        .input_seq(seq)
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 同 Session 内的次序键：`(input_seq, id)`。**seq 是权威**（§8.3），id 只在 seq 相同时
/// 做次级比较（`input_seq = 0` 的老行）。
pub fn order_key(row: &RunRow) -> (i64, String) {
    (row.input_seq, row.id.clone())
}

/// 比 `key` 早的那些行里，**紧挨着的那一条还非终态的 Run**。
///
/// 这是"谁挡着谁"的**唯一一处实现**：受理判定、依赖放行都用它，§8.7 的领取语句用的是同一
/// 条谓词（`earlier.input_seq < … OR (… AND earlier.id < …)` 且非终态）。选"紧挨着"而不是
/// "最早那条"，是因为 `queued` 的定义是"现在就能跑、只缺 worker"：等最早那条结束还不够，
/// 它后面可能还压着在跑的。
///
/// `exclude` 是"不要把自己算进去"的那个 id：受理那一刻，被受理那行的 `input_seq` 列还没写
/// 进去（还是 0），拿它自己列的键比会把自己当成更早的一条。
pub fn nearest_unfinished_predecessor(
    rows: &[RunRow],
    session: &str,
    key: (i64, &str),
    exclude: Option<&str>,
) -> Result<Option<String>, StoreError> {
    let mut nearest: Option<(i64, String)> = None;
    for row in rows {
        if row.session_id != session || exclude == Some(row.id.as_str()) {
            continue;
        }
        let candidate = order_key(row);
        if (candidate.0, candidate.1.as_str()) >= key {
            continue;
        }
        if state_of(row)?.is_terminal() {
            continue;
        }
        if nearest.as_ref().is_none_or(|current| candidate > *current) {
            nearest = Some(candidate);
        }
    }
    Ok(nearest.map(|(_, id)| id))
}

/// 受理：记下承载输入的事件与它的 `seq`，并把状态定成 **`queued` 或
/// `waiting + dependency`**。
///
/// **次序在受理那一刻就定下来**（§8.4）：同一 Session 里更早的那条还没结束，这条新的就是
/// `waiting + dependency`，句柄指向那条更早的 Run。`queued` 只是"现在就能跑、只缺 worker"
/// ——先写 `queued`、再由调用方补一条 suspend 的两个写者之间，调度器（`claim` 随时可调）
/// 能把它领走，那就**越过了前面那条未完成的 Run**：回放窗口会带着一个没有输出的
/// `function_call` 发给模型，provider 直接 400。
///
/// **先后按 `(input_seq, id)` 比**，判据与 §8.7 领取语句里那句 `NOT EXISTS`、以及
/// [`crate::repos::queue::release_satisfied_dependencies`] 的放行判据同源。次序的权威是
/// 输入事件的 seq（§8.3），id 只在 seq 相同时做次级比较（`input_seq = 0` 的老行）。
///
/// **被委派的子 Run 是这条规则唯一的例外**（§8.4 的 `dependency`）：它一律落 `queued`。
/// 它以输入序排在父后面，而父此刻正在跑、随后要停下来等它——照搬那条规则就是父子互等
/// 死锁（父等子、子等父，两条都在库里不动）。`queued` 在这里不等于"能领走"：§8.7 的领取
/// 语句对它的豁免**只对"拦住它的恰好是它的父、且父正等着它"这一种形状**放行，父还在跑的
/// 时候它照样不是候选（见 [`crate::repos::queue::CLAIM_SQL`] 的 `NOT (...)`）。
pub async fn mark_queued_in(
    ex: &mut dyn Executor,
    run: &RunId,
    input_event: &EventId,
    seq: Seq,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let mut row = require(ex, run).await?;
    let (session_id, self_id) = (row.session_id.clone(), row.id.clone());
    let self_key = (i64::try_from(seq.0).unwrap_or(i64::MAX), self_id.clone());

    // 子 Run 不必问"谁挡着我"：挡着它的只可能是父，而父被 §8.7 的豁免放行了（见上面）。
    // 顺带省掉一次会话级的行扫描——子 Run 的受理每次都会走这条路。
    let predecessor = if row.parent_run_id.is_some() {
        None
    } else {
        // 紧挨着的那一条：最大的、比它早的非终态 Run（判据见
        // [`nearest_unfinished_predecessor`]）。只取这个会话的行（`runs_session` 索引），
        // 别为了受理一条输入扫全表。
        let rows = RunRow::filter(RunRow::fields().session_id().eq(session_id.as_str()))
            .exec(ex)
            .await
            .map_err(map_toasty)?;
        nearest_unfinished_predecessor(
            &rows,
            &session_id,
            (self_key.0, self_key.1.as_str()),
            Some(&self_id),
        )?
    };

    match predecessor {
        Some(earlier) => {
            row.update()
                .input_event(Some(input_event.to_string()))
                .input_seq(self_key.0)
                .exec(ex)
                .await
                .map_err(map_toasty)?;
            // 复用等待三列的唯一映射：`wait_ref` 就是那条更早的 Run ID。
            write_wait_in(
                ex,
                &mut row,
                &WaitReason::Dependency {
                    run: RunId::from_raw(earlier),
                },
                None,
                now,
            )
            .await
        }
        None => row
            .update()
            .input_event(Some(input_event.to_string()))
            .input_seq(self_key.0)
            .state(state_str(RunState::Queued))
            .wait_kind(None as Option<String>)
            .wait_ref(None as Option<String>)
            .wake_at(0_i64)
            .updated_at(to_ts(now))
            .exec(ex)
            .await
            .map_err(map_toasty),
    }
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
        .state(state_str(RunState::Running))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)?;
    Ok(None)
}

/// 让出执行名额时的状态提交：`waiting` + **在等什么**（§8.4）。
///
/// 状态与等待三列必须一起写：只有一个 `waiting` 而说不出在等谁，reconcile 与清单都
/// 处置不了它。
pub async fn mark_waiting_in(
    ex: &mut dyn Executor,
    run: &RunId,
    wait: &WaitReason,
    reason: Option<String>,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let mut row = require(ex, run).await?;
    write_wait_in(ex, &mut row, wait, reason, now).await
}

/// 终态，连同最终事件引用与 Memory 待处理标记（§8.5 的"结束任务"）。
pub async fn mark_final_in(
    ex: &mut dyn Executor,
    run: &RunId,
    state: RunState,
    final_event: &EventId,
    rounds: u32,
    reason: Option<String>,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    debug_assert!(state.is_terminal(), "终态只能是那四个");
    let mut row = require(ex, run).await?;
    row.update()
        .state(state_str(state))
        .wait_kind(None as Option<String>)
        .wait_ref(None as Option<String>)
        .wake_at(0_i64)
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

/// 终态，**但没有会话事件落在它身上**。
///
/// 走这条路的是「这个会话已经读不出来」（日志丢了 / 中间损坏）时的一次取消。取消是
/// **调度事实**（§8.2 把会话内容与调度分开），它不产生会话内容，所以那条 `run.cancelled`
/// 写不进去的时候不能整个失败——否则这条 Run 永远停在等人判断上，§8.4 的「操作者处理后
/// queued / cancelled」一条也走不通。`final_event` 留空：**没有**那一条事件，不假装有。
pub async fn mark_final_without_event_in(
    ex: &mut dyn Executor,
    run: &RunId,
    state: RunState,
    rounds: u32,
    reason: Option<String>,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    debug_assert!(state.is_terminal(), "终态只能是那四个");
    let mut row = require(ex, run).await?;
    row.update()
        .state(state_str(state))
        .wait_kind(None as Option<String>)
        .wait_ref(None as Option<String>)
        .wake_at(0_i64)
        .rounds(i64::from(rounds))
        .last_error(reason)
        .memory_work(memory_work_str(MemoryWork::Pending))
        .ended_at(to_ts(now))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 同上，自己开事务，并顺手清掉会话的当前 Run——**与 `Ledger::complete` 的收尾一致**
/// （终态之后 `sessions.current_run` 不该还指着它）。
pub async fn stop_without_event(
    db: &Db,
    session: &SessionId,
    run: &RunId,
    state: RunState,
    reason: Option<String>,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let (session, run) = (session.clone(), run.clone());
    db.with_write_retry(move |ex| {
        let (session, run, reason) = (session.clone(), run.clone(), reason.clone());
        Box::pin(async move {
            let rounds = get_in(ex, &run)
                .await?
                .map(|row| row.rounds.max(0) as u32)
                .unwrap_or(0);
            mark_final_without_event_in(ex, &run, state, rounds, reason, now).await?;
            if state.is_terminal() {
                crate::repos::session::set_current_run_in(ex, &session, None, now).await?;
            }
            Ok(())
        }) as BoxFuture<'_, Result<(), StoreError>>
    })
    .await
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
                if record.state.is_unfinished() {
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
/// `undelivered` 是"有结果投递卡在半路"的那一组 Run id，由调用方查 `deliveries` 得出
/// ——这里不自己去查，因为同一轮扫描的两批行要用**同一份**送达观察。
pub async fn terminal_undelivered(
    db: &Db,
    undelivered: &std::collections::BTreeSet<String>,
) -> Result<Vec<RunRecord>, StoreError> {
    let undelivered = undelivered.clone();
    db.read(move |ex| {
        let undelivered = undelivered.clone();
        Box::pin(async move {
            let rows = RunRow::all().exec(ex).await.map_err(map_toasty)?;
            let mut out = Vec::new();
            for row in &rows {
                if !undelivered.contains(&row.id) {
                    continue;
                }
                let record = RunRecord::try_from_row(row)?;
                if record.state.is_terminal() {
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
                if !state_of(&row)?.is_terminal() {
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
