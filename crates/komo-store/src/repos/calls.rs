//! `tool_calls` 与 `tool_attempts`：store 内部的具体类型，只被 Coordinator 用（§13.5）。
//!
//! 一个 ToolCall 是**逻辑动作**；`tool_attempts` 记它实际执行过的各次尝试。一次重试
//! 沿用同一个 ToolCall ID，新增一条 attempt（§8.6）——所以这两张表不能合成一张。

use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{AttemptId, EventId, ExecutorId, RunId, SessionId, ToolCallId};
use komo_kernel::types::plan::{ExecutionPlan, PlanHash};
use komo_kernel::types::refs::{OutputRef, ToolResultStatus};
use komo_kernel::types::status::{AttemptState, ToolCallState};
use komo_kernel::types::turn::ToolCallRequest;
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, encode, map_toasty, to_ts};
use crate::models::{ToolAttemptRow, ToolCallRow};

fn call_state_str(state: ToolCallState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("ToolCallState 序列化成一个字符串")
}

fn attempt_state_str(state: AttemptState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("AttemptState 序列化成一个字符串")
}

/// 建立一轮里的一个 `planned` 之前的调用行。
///
/// 这一步在 `record_round` 里发生：assistant 事件已经落盘，调用的**参数**有了，但计划
/// 还没有——计划要到执行前由 `prepare` 产生（§8.4 第 6 行），所以 `plan_hash` 此刻是
/// `NULL`，状态仍是 `planned` 的前一格。
pub async fn record_request_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    run: &RunId,
    round: u32,
    args_event: &EventId,
    request: &ToolCallRequest,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    if ToolCallRow::filter_by_id(request.call_id.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .is_some()
    {
        return Ok(());
    }
    toasty::create!(ToolCallRow {
        id: request.call_id.as_str(),
        session_id: session.as_str(),
        run_id: run.as_str(),
        tool: request.name.clone(),
        provider_call_id: request.provider_call_id.clone(),
        round: i64::from(round),
        state: call_state_str(ToolCallState::Planned),
        args_event: Some(args_event.to_string()),
        plan_event: None as Option<String>,
        result_event: None as Option<String>,
        plan_hash: None as Option<String>,
        recovery: "{}".to_string(),
        idempotency_key: None as Option<String>,
        attempts: 0_i64,
        output_ref: None as Option<String>,
        preview: None as Option<String>,
        created_at: to_ts(now),
        updated_at: to_ts(now),
    })
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    Ok(())
}

/// 计划落盘：记下计划事件、计划哈希、恢复方式与外部幂等键。
pub async fn record_plan_in(
    ex: &mut dyn Executor,
    call: &ToolCallId,
    plan: &ExecutionPlan,
    plan_event: &EventId,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let mut row = require(ex, call).await?;
    let idempotency_key = match &plan.recovery {
        komo_kernel::types::plan::RecoveryMode::IdempotencyKey { key, .. } => Some(key.clone()),
        _ => None,
    };
    row.update()
        .plan_event(Some(plan_event.to_string()))
        .plan_hash(Some(plan.plan_hash().to_string()))
        .recovery(encode(&plan.recovery)?)
        .idempotency_key(idempotency_key)
        .state(call_state_str(ToolCallState::Planned))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// `tool.started` + 一条新的执行尝试，同一事务（§8.5）。
#[allow(clippy::too_many_arguments)]
pub async fn record_started_in(
    ex: &mut dyn Executor,
    call: &ToolCallId,
    attempt: &AttemptId,
    plan_hash: &PlanHash,
    started_event: &EventId,
    executor: Option<&ExecutorId>,
    process: Option<String>,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    // 幂等：这条尝试已经记过了就什么都不做。恢复的 `backfill` 会把同一段 JSONL 重放
    // 进索引，累加 `attempts` 会让"跑了几次"这个数字随重启膨胀。
    if ToolAttemptRow::filter_by_id(attempt.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .is_some()
    {
        return Ok(());
    }
    let mut row = require(ex, call).await?;
    let ordinal = row.attempts + 1;
    let (session, run) = (row.session_id.clone(), row.run_id.clone());

    row.update()
        .state(call_state_str(ToolCallState::Started))
        .plan_hash(Some(plan_hash.to_string()))
        .attempts(ordinal)
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)?;

    toasty::create!(ToolAttemptRow {
        id: attempt.as_str(),
        call_id: call.as_str(),
        run_id: run,
        session_id: session,
        ordinal,
        executor: executor.map(|e| e.to_string()),
        process,
        state: attempt_state_str(AttemptState::Started),
        started_at: to_ts(now),
        ended_at: 0_i64,
        started_event: Some(started_event.to_string()),
        result_event: None as Option<String>,
    })
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    Ok(())
}

/// 结果引用、调用状态与尝试状态。
///
/// `uncertain` 是 **ToolCall 的状态**，不是 Run 的：副作用可能已经发生而完整输出没有
/// 落盘，这段窗口必须保留（§8.6）。所以一条 `uncertain` 的结果把调用留在
/// `ToolCallState::Uncertain`，而它的 attempt 仍是 `started`——那次尝试确实没有收尾。
pub async fn record_result_in(
    ex: &mut dyn Executor,
    attempt: &AttemptId,
    status: ToolResultStatus,
    output: &OutputRef,
    preview: Option<String>,
    result_event: &EventId,
    now: OffsetDateTime,
) -> Result<ToolCallId, StoreError> {
    let mut attempt_row = ToolAttemptRow::filter_by_id(attempt.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .ok_or_else(|| StoreError::NotFound {
            what: format!("attempt {attempt}"),
        })?;
    let call = ToolCallId::from_raw(attempt_row.call_id.clone());

    let (call_state, attempt_state) = match status {
        ToolResultStatus::Completed => (ToolCallState::Completed, AttemptState::Completed),
        ToolResultStatus::Failed => (ToolCallState::Failed, AttemptState::Failed),
        ToolResultStatus::Uncertain => (ToolCallState::Uncertain, AttemptState::Started),
    };

    attempt_row
        .update()
        .state(attempt_state_str(attempt_state))
        .ended_at(to_ts(now))
        .result_event(Some(result_event.to_string()))
        .exec(ex)
        .await
        .map_err(map_toasty)?;

    let mut call_row = require(ex, &call).await?;
    call_row
        .update()
        .state(call_state_str(call_state))
        .output_ref(Some(encode(output)?))
        .preview(preview)
        .result_event(Some(result_event.to_string()))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)?;
    Ok(call)
}

/// 把**不属于本次启动身份**的、没有收尾的尝试标成 `interrupted`（§8.7）。返回改了几行。
///
/// **调用状态一个字不动**：`started` 而无结果的调用仍要走 §8.6 的核对流程，把它改成
/// failed 就等于宣布副作用没发生，而那正是不知道的事。这里说的只有"那个执行实例没回
/// 来"——`AttemptState::Interrupted` 的全部含义。
///
/// 按**执行实例**作用域，不按"被回收的那几个 Run"：正常停机时 `RunQueue::release` 已经
///把 Run 从 `running` 放回 `queued`，于是它根本不在回收集合里，可它的尝试还停在
/// `started`。一个 db 文件只有一个进程开着（§8.2），所以"不是本次启动身份的 started
/// 尝试"就是上一世留下的，一个不漏。`executor` 为空的同理——不知道是谁的，就一定不是
/// 这次的。
pub async fn interrupt_open_attempts_in(
    ex: &mut dyn Executor,
    executor: &ExecutorId,
    now: OffsetDateTime,
) -> Result<u64, StoreError> {
    let started = attempt_state_str(AttemptState::Started);
    let rows = ToolAttemptRow::filter(ToolAttemptRow::fields().state().eq(started.as_str()))
        .exec(ex)
        .await
        .map_err(map_toasty)?;
    let mut touched = 0;
    for mut row in rows {
        if row.executor.as_deref() == Some(executor.as_str()) {
            continue;
        }
        row.update()
            .state(attempt_state_str(AttemptState::Interrupted))
            .ended_at(to_ts(now))
            .exec(ex)
            .await
            .map_err(map_toasty)?;
        touched += 1;
    }
    Ok(touched)
}

/// 一个 Run 的全部调用，按创建顺序。
pub async fn list_for_run(db: &Db, run: &RunId) -> Result<Vec<ToolCallRow>, StoreError> {
    let id = run.to_string();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let mut rows = ToolCallRow::filter(ToolCallRow::fields().run_id().eq(id.as_str()))
                .exec(ex)
                .await
                .map_err(map_toasty)?;
            rows.sort_by_key(|row| (row.round, row.id.clone()));
            Ok(rows)
        }) as BoxFuture<'_, Result<Vec<ToolCallRow>, StoreError>>
    })
    .await
}

/// 一个调用的全部尝试，按序号。
pub async fn attempts_of(db: &Db, call: &ToolCallId) -> Result<Vec<ToolAttemptRow>, StoreError> {
    let id = call.to_string();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let mut rows =
                ToolAttemptRow::filter(ToolAttemptRow::fields().call_id().eq(id.as_str()))
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
            rows.sort_by_key(|r| r.ordinal);
            Ok(rows)
        }) as BoxFuture<'_, Result<Vec<ToolAttemptRow>, StoreError>>
    })
    .await
}

async fn require(ex: &mut dyn Executor, call: &ToolCallId) -> Result<ToolCallRow, StoreError> {
    ToolCallRow::filter_by_id(call.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .ok_or_else(|| StoreError::NotFound {
            what: format!("tool call {call}"),
        })
}
