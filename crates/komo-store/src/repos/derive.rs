//! 可重建表的派生写入：一条 JSONL 事件在 `tool_calls` / `tool_attempts` / `runs` 上留下
//! 什么（§8.2、§8.9）。
//!
//! **只有这一处。** 实时提交（[`crate::coordinator::Coordinator`] 每段 §8.5 的第三步）与
//! 启动补齐（[`crate::repos::recovery::RecoveryStore::backfill`]）调的是同一个函数，所以
//! "派生行是 JSONL 的投影"是一句测试钉得住的话：两条路折出来的行逐列相同，只差 JSONL 里
//! 本来就没有的那几格（执行实例、时刻）。以前两边各写一份，backfill 漏了 `runs.rounds`。
//!
//! 全部幂等：同一段事件重放十遍与一遍相同。Run 的状态不在这里——那是决策表的结论（§8.4）。

use komo_kernel::events::EventPayload;
use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{ExecutorId, SessionId};
use komo_kernel::types::plan::ExecutionPlan;
use time::OffsetDateTime;
use toasty::Executor;

use super::{calls, runs};
use crate::session_log::AppendedEvent;

/// JSONL 那一行之外、派生写入还要的东西。
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts<'a> {
    /// `tool.planned` 外置到 `payloads/` 的计划。内联的直接从事件里取；外置的由调用方
    /// 先读回来——数据库事务里不等文件（§8.5）。
    pub plan: Option<&'a ExecutionPlan>,
    /// 写进 `tool_attempts.executor` 的本次启动身份。回放不知道它，留空（§8.7）。
    pub executor: Option<&'a ExecutorId>,
}

/// 把一条事件的派生状态写进当前事务。不属于任何 Run 的事件、以及在这些表上没有派生
/// 状态的事件，什么都不写。
pub async fn apply_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    record: &AppendedEvent,
    facts: Facts<'_>,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let Some(run) = record.event.run.as_ref() else {
        return Ok(());
    };
    let event_id = &record.event.event_id;
    match &record.event.payload {
        EventPayload::RunAccepted(_) => {
            // **只记引用，不动状态**：入不入队是决策表的结论，不是"日志里有
            // run.accepted"的推论——推成 queued 会在补发结果那条路上悄悄复活一个已取消的
            // Run。实时受理那一刻由 `runs::mark_queued_in` 连状态一起写。
            runs::set_input_event_in(ex, run, event_id, record.event.seq, now).await
        }
        EventPayload::MessageAssistant(body) => {
            for request in &body.tool_calls {
                calls::record_request_in(ex, session, run, body.round, event_id, request, now)
                    .await?;
            }
            runs::bump_rounds_in(ex, run, body.round, now).await
        }
        EventPayload::ToolPlanned(body) => match body.plan.as_deref().or(facts.plan) {
            Some(plan) => calls::record_plan_in(ex, &body.call_id, plan, event_id, now).await,
            // 外置的计划没读回来：留着 planned 之前的样子，不编一份。
            None => Ok(()),
        },
        EventPayload::ToolStarted(body) => {
            calls::record_started_in(
                ex,
                &body.call_id,
                &body.attempt_id,
                &body.plan_hash,
                event_id,
                facts.executor,
                None,
                now,
            )
            .await
        }
        EventPayload::ToolResult(body) => {
            // 没有 `tool.started` 的结果（一次没执行过的调用的结论）要把那条尝试补上：
            // `record_result_in` 按尝试行找调用，行不在就记不上。尝试已经在（执行过的那
            // 一种）时这一步什么都不做。
            calls::record_unstarted_in(ex, &body.call_id, &body.attempt_id, now).await?;
            calls::record_result_in(
                ex,
                &body.attempt_id,
                body.status,
                &body.output_ref,
                body.preview.clone(),
                event_id,
                now,
            )
            .await
            .map(|_| ())
        }
        _ => Ok(()),
    }
}
