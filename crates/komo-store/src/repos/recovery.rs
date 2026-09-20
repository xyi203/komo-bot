//! 启动扫描要在索引上做的那几件事（§8.4、§8.7）。
//!
//! runtime 的 `RecoveryIndex` trait 是这五个函数的形状——但 **store 不能依赖 runtime**
//! （依赖只向下），所以这里给的是具体函数，trait 的 impl 由接线处在 runtime 侧用一个
//! newtype 包住 [`RecoveryStore`] 来做。五个签名逐字对齐那张 trait，包在外面的那层是
//! 一行一个。
//!
//! 贯穿全模块的一条规则：**回放只补索引与派生执行状态**——不调用工具、不发送外部请求、
//! 不消费授权，也不覆盖数据库里已经记下的取消或权限撤销（§8.5）。所以 [`requeue`] 与
//! [`mark_needs_attention`] 都先看一眼终态就退。

use std::path::{Path, PathBuf};

use komo_kernel::events::EventPayload;
use komo_kernel::traits::StoreError;
use komo_kernel::types::chat::Outbound;
use komo_kernel::types::ids::{ExecutorId, InterventionId, RunId, SessionId};
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::status::{RunState, WaitReason};
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, to_ts};
use crate::models::DeliveryRow;
use crate::payloads::PayloadStore;
use crate::repos::{calls, runs, session};
use crate::session_log::{SessionPaths, scan_records};

/// 一个未终态 Run 在 state.db 里的样子。
///
/// 字段与 runtime 的 `UnfinishedRun` 一一对应，包在外面的 newtype 逐字段搬过去就行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfinishedRun {
    pub run: RunId,
    pub session: SessionId,
    pub state: RunState,
    pub wait: Option<WaitReason>,
    /// 还握着领取权的执行实例。
    pub claimed_by: Option<ExecutorId>,
    /// `waiting` 时在等什么（§8.4）。**预算不在这里**：`retry_attempts` 是行上的计数器，
    /// "用完了没有"由拿着预算的那一层判。
    /// 最终结果已经送达客户端了吗（§8.4 第 10 行）。
    ///
    /// 「送达」按投递义务判：`deliveries` 里没有一行 `RunFinished` 还卡在 pending /
    /// deferred 就算送达。TUI / HTTP 来源的 Run 从来不产生投递行——结果由客户端**补读**
    /// （§8.4：「补发或补读」），不是补发。
    pub result_delivered: bool,
}

/// 恢复扫描的索引面。
#[derive(Debug, Clone)]
pub struct RecoveryStore {
    db: Db,
    sessions_root: PathBuf,
}

impl RecoveryStore {
    pub fn new(db: Db, sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            db,
            sessions_root: sessions_root.into(),
        }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn sessions_root(&self) -> &Path {
        &self.sessions_root
    }

    /// 启动扫描要逐个判断的那些 Run（§8.4）。
    ///
    /// 两批，不是一批：
    ///
    /// - **未终态的**——那张表前九行说的就是它们；
    /// - **已终态但结果还没送到客户端的**——第 10 行「已保存最终结果，但客户端没有收到
    ///   → 补发或补读原结果，**不重新执行任务**」。不带上它们，`RecoveryAction::
    ///   RedeliverResult` 就是一段永远走不到的代码。
    ///
    /// 结构里不需要多一个字段区分：`status` 本身已经说了是哪一批。「没送到」= 有一行
    /// `RunFinished` 投递还不是 `sent`；没有投递行的 Run（TUI / HTTP 来源）不在其中，
    /// 它们的结果由客户端补读。补发之后那行变成 `sent`，下一轮它自己退出这个集合。
    pub async fn unfinished_runs(&self) -> Result<Vec<UnfinishedRun>, StoreError> {
        let undelivered = undelivered_runs(&self.db).await?;
        let mut rows = runs::unfinished(&self.db).await?;
        rows.extend(runs::terminal_undelivered(&self.db, &undelivered).await?);
        rows.sort_by(|a, b| a.run.as_str().cmp(b.run.as_str()));

        Ok(rows
            .into_iter()
            .map(|record| UnfinishedRun {
                result_delivered: !undelivered.contains(record.run.as_str()),
                claimed_by: record.claimed_by.clone().map(ExecutorId::from_raw),
                run: record.run,
                session: record.session,
                state: record.state,
                wait: record.wait,
            })
            .collect())
    }

    /// 「启动回收：旧实例的 running -> interrupted，并交还领取权」（§8.7）。返回影响行数。
    ///
    /// 同一个事务里还要**收拾那几个 Run 遗留的尝试**：上一个执行实例没回来，它开着的
    /// `tool_attempts` 就停在 `started`，而 `started` 在 §8.6 里的意思是"正在跑"。不标
    /// 成 `interrupted` 的话，核对流程看到的是一个永远在跑的尝试。
    ///
    /// **`tool_calls` 的状态一个字不动**：`started` 而无结果的调用仍要走核对流程——把它
    /// 改成 failed 就等于宣布副作用没发生，而那正是不知道的事。
    pub async fn reclaim_running(&self, executor: &ExecutorId) -> Result<u64, StoreError> {
        let executor = executor.clone();
        let now = OffsetDateTime::now_utc();
        self.db
            .with_write_retry(move |ex| {
                let executor = executor.clone();
                Box::pin(async move {
                    let reclaimed = super::queue::reclaim_unowned_in(ex, &executor).await?;
                    // **按执行实例收拾，不按被回收的那几行**：正常停机时
                    // `RunQueue::release` 已经把 Run 从 `running` 放回 `queued`，于是它
                    // 根本不在回收集合里——可它的尝试还停在 `started`。一个 db 文件只有
                    // 一个进程开着，所以"不属于本次启动身份的 started 尝试"就是上一世
                    // 留下的，一个不漏。
                    let closed = calls::interrupt_open_attempts_in(ex, &executor, now).await?;
                    if closed > 0 {
                        tracing::info!(closed, "收拾了上一代遗留的、没有收尾的尝试");
                    }
                    Ok(reclaimed)
                }) as BoxFuture<'_, Result<u64, StoreError>>
            })
            .await
    }

    /// 用 JSONL 已有的事件补齐 state.db 的索引与派生执行状态。**不重放动作。**
    ///
    /// 「JSONL 已有结果引用且完整输出校验通过，state.db 可能落后 → 先补齐结果索引和状态，
    /// 复用原输出，不重放动作」（§8.4）。这里做的正好是"补齐"那一半：
    ///
    /// - 每一行进 `session_log_index`（按 `event_id` 幂等）；
    /// - `message.assistant` / `tool.planned` / `tool.started` / `tool.result` 依次
    ///   还原 `tool_calls` 与 `tool_attempts` 的派生状态；
    /// - `applied_seq` 推到这个 Session 已校验的连续前缀末端。
    ///
    /// **Run 的状态不在这里改**：改不改、改成什么，是 kernel 那张决策表的结论，由
    /// [`RecoveryStore::requeue`] / [`RecoveryStore::mark_needs_attention`] 执行。
    pub async fn backfill(&self, run: &RunId) -> Result<(), StoreError> {
        let Some(record) = runs::get(&self.db, run).await? else {
            return Err(StoreError::NotFound {
                what: format!("run {run}"),
            });
        };
        let session = record.session.clone();
        let paths = SessionPaths::new(&self.sessions_root, &session);

        // 扫描是只读的：中间损坏 / 未知版本照样是 Corrupt，**不隔离、不截断**。
        let records = scan_records(&paths, &session)
            .await
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
        let payloads = PayloadStore::new(paths);

        // 外置的计划要在事务外读回来——数据库事务里不等文件（§8.5）。
        let mut plans: Vec<(String, ExecutionPlan)> = Vec::new();
        for record in &records {
            if let EventPayload::ToolPlanned(body) = &record.event.payload {
                let plan = match (&body.plan, &body.plan_ref) {
                    (Some(plan), _) => (**plan).clone(),
                    (None, Some(reference)) => payloads.open_json(reference).await?,
                    (None, None) => continue,
                };
                plans.push((record.event.event_id.to_string(), plan));
            }
        }

        let last_seq = records.last().map(|r| r.seq()).unwrap_or_default();
        let bytes = records
            .last()
            .map(|r| r.byte_offset + r.byte_len)
            .unwrap_or(0);
        let now = OffsetDateTime::now_utc();
        let run_id = run.clone();

        self.db
            .with_write_retry(move |ex| {
                let (records, plans, session, run_id) = (
                    records.clone(),
                    plans.clone(),
                    session.clone(),
                    run_id.clone(),
                );
                Box::pin(async move {
                    for record in &records {
                        session::index_event_in(ex, record).await?;
                        // 只还原**这个 Run** 的调用状态；同一 Session 的别的 Run 由它们
                        // 自己那一轮 backfill 管。
                        if record.event.run.as_ref() != Some(&run_id) {
                            continue;
                        }
                        apply_event(ex, &session, &run_id, record, &plans, now).await?;
                    }
                    session::advance_applied_in(ex, &session, last_seq, bytes, now).await?;
                    Ok(())
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
    }

    /// 放回队列，等调度器领。
    ///
    /// **不覆盖已经记下的终态**（§8.5）：一个已取消的 Run 不会因为回放又活过来。重试
    /// 重试计数原样留着——「沿用已保存的次数，到期再尝试」（§8.4）。
    pub async fn requeue(&self, run: &RunId) -> Result<(), StoreError> {
        let run = run.clone();
        self.db
            .with_write_retry(move |ex| {
                let run = run.clone();
                Box::pin(async move {
                    let Some(mut row) = runs::get_in(ex, &run).await? else {
                        return Err(StoreError::NotFound {
                            what: format!("run {run}"),
                        });
                    };
                    if runs::state_of(&row)?.is_terminal() {
                        tracing::debug!(run = %run, "已经是终态，回放不复活它");
                        return Ok(());
                    }
                    // 回 `queued` 就是"现在就能跑"（§8.4）：等待三列一起清掉。重试预算
                    // 留在 `retry_attempts` 上——**重启不重置**（§8.5）。
                    row.update()
                        .state(runs::state_str(RunState::Queued))
                        .wait_kind(None as Option<String>)
                        .wait_ref(None as Option<String>)
                        .wake_at(0_i64)
                        .claimed_by(None as Option<String>)
                        .updated_at(to_ts(OffsetDateTime::now_utc()))
                        .exec(ex)
                        .await
                        .map_err(crate::db::map_toasty)
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
    }

    /// 需要操作者判断：结果不明、引用损坏、授权失效。
    pub async fn mark_needs_attention(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        let run = run.clone();
        let reason = reason.to_string();
        self.db
            .with_write_retry(move |ex| {
                let (run, reason) = (run.clone(), reason.clone());
                Box::pin(async move {
                    let Some(mut row) = runs::get_in(ex, &run).await? else {
                        return Err(StoreError::NotFound {
                            what: format!("run {run}"),
                        });
                    };
                    if runs::state_of(&row)?.is_terminal() {
                        tracing::debug!(run = %run, "已经是终态，不再停在等人判断上");
                        return Ok(());
                    }
                    // `waiting + intervention`，句柄就是这个 Run（§7.5：一个 Run 上最多停着
                    // 一条要人判断的 Intervention，所以 Run ID 本身就是它的句柄）。
                    row.update()
                        .state(runs::state_str(RunState::Waiting))
                        .wait_kind(Some("intervention".to_string()))
                        .wait_ref(Some(InterventionId::for_run(&run).to_string()))
                        .wake_at(0_i64)
                        .claimed_by(None as Option<String>)
                        .last_error(Some(reason))
                        .updated_at(to_ts(OffsetDateTime::now_utc()))
                        .exec(ex)
                        .await
                        .map_err(crate::db::map_toasty)
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
    }
}

/// 一条事件在索引上的回放。**只写派生状态**。
async fn apply_event(
    ex: &mut dyn Executor,
    session: &SessionId,
    run: &RunId,
    record: &crate::session_log::AppendedEvent,
    plans: &[(String, ExecutionPlan)],
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let event_id = &record.event.event_id;
    match &record.event.payload {
        EventPayload::RunAccepted(_) => {
            // **只记引用，不动状态**：入不入队是决策表的结论（`RecoveryAction::Requeue`），
            // 不是"日志里有 run.accepted"这件事的推论。推成 queued 的话，一个已取消的
            // Run 会在补发结果那条路径上（第 10 行也走 backfill）被悄悄复活。
            runs::set_input_event_in(ex, run, event_id, record.event.seq, now).await?;
        }
        EventPayload::MessageAssistant(body) => {
            for request in &body.tool_calls {
                calls::record_request_in(ex, session, run, body.round, event_id, request, now)
                    .await?;
            }
        }
        EventPayload::ToolPlanned(body) => {
            if let Some((_, plan)) = plans.iter().find(|(id, _)| id == event_id.as_str()) {
                calls::record_plan_in(ex, &body.call_id, plan, event_id, now).await?;
            }
        }
        EventPayload::ToolStarted(body) => {
            calls::record_started_in(
                ex,
                &body.call_id,
                &body.attempt_id,
                &body.plan_hash,
                event_id,
                None,
                None,
                now,
            )
            .await?;
        }
        EventPayload::ToolResult(body) => {
            calls::record_result_in(
                ex,
                &body.attempt_id,
                body.status,
                &body.output_ref,
                body.preview.clone(),
                event_id,
                now,
            )
            .await?;
        }
        // Run 级别的状态由决策表定，回放不动它；其余事件在索引上没有派生状态。
        _ => {}
    }
    Ok(())
}

/// 已经送达过结果的 Run。
///
/// 「已保存最终结果，但客户端没有收到 → 补发或补读原结果，不重新执行任务」（§8.4）。
/// 判据就是 `deliveries` 里有没有一条 `sent` 的行提到它——补发按 `DeliveryId` 幂等，
/// 所以"送过了"是一个查得出来的事实，不是一个猜测。
/// 有一行 `RunFinished` 投递还没到 `sent` 的那些 Run。
async fn undelivered_runs(db: &Db) -> Result<std::collections::BTreeSet<String>, StoreError> {
    db.read(move |ex| {
        Box::pin(async move {
            let rows = DeliveryRow::filter(DeliveryRow::fields().state().ne("sent"))
                .exec(ex)
                .await
                .map_err(crate::db::map_toasty)?;
            let mut out = std::collections::BTreeSet::new();
            for row in &rows {
                // 渠道写下的正文；解不出来就当它没提到任何 Run，不要让一行坏数据
                // 把整个启动扫描带走。
                let Ok(outbound) = serde_json::from_str::<Outbound>(&row.outbound) else {
                    continue;
                };
                if let Outbound::RunFinished { run, .. } = outbound {
                    out.insert(run.to_string());
                }
            }
            Ok(out)
        }) as BoxFuture<'_, Result<std::collections::BTreeSet<String>, StoreError>>
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::test_support::{TestClock, sample_model, sample_plan};
    use komo_kernel::traits::{Clock, Ledger, RunQueue};
    use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, DeliveryTarget};
    use komo_kernel::types::ids::{DeliveryId, RequestKey, Seq, ToolCallId};
    use komo_kernel::types::plan::PlanSource;
    use komo_kernel::types::refs::{ContentRef, OutputRef, PublishedOutput, ToolResultStatus};
    use komo_kernel::types::status::{RetryCause, RunEnd, RunState, WaitReason};
    use komo_kernel::types::turn::{AcceptInput, AssistantRound, ToolCallRequest};
    use std::sync::Arc;

    use crate::coordinator::Coordinator;
    use crate::db::DbOptions;

    struct Fixture {
        _dir: tempfile::TempDir,
        db: Db,
        session: SessionId,
        store: RecoveryStore,
        coordinator: Coordinator,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect_with(dir.path().join("state.db"), DbOptions::default())
            .await
            .expect("打开库");
        let session = SessionId::from_raw("00000000-0000-7000-8000-000000000001");
        let sessions_root = dir.path().join("sessions");
        let coordinator = Coordinator::open(
            db.clone(),
            &sessions_root,
            session.clone(),
            "api",
            Arc::new(TestClock::fixed()),
        )
        .await
        .expect("打开 Coordinator");
        Fixture {
            _dir: dir,
            store: RecoveryStore::new(db.clone(), sessions_root),
            db,
            session,
            coordinator,
        }
    }

    fn input(key: &str, session: &SessionId) -> AcceptInput {
        AcceptInput {
            session: session.clone(),
            request_key: RequestKey::new(key),
            text: "读一下 a.txt".into(),
            source: PlanSource::Interactive {
                session: session.clone(),
            },
            peer: None,
            model: sample_model(),
            workdir: None,
            at: TestClock::fixed().now(),
        }
    }

    /// 一个 Run，上面有一次**开着没收尾**的尝试。
    async fn a_run_with_an_open_attempt(f: &Fixture) -> (RunId, ToolCallId) {
        let accepted = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();
        let ids = f
            .coordinator
            .record_round(
                &accepted.run,
                AssistantRound {
                    round: 1,
                    text: None,
                    text_ref: None,
                    tool_calls: vec![ToolCallRequest {
                        call_id: ToolCallId::from_raw("call-1"),
                        provider_call_id: "pc-1".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({}),
                        arguments_ref: None,
                    }],
                    provider_blocks: None,
                    usage: Default::default(),
                },
            )
            .await
            .unwrap();
        let plan = sample_plan("shell", &f.session);
        f.coordinator.plan_call(&ids[0], &plan).await.unwrap();
        // 开跑了，但没有结果——上一个实例就停在这里。
        f.coordinator
            .start_call(&ids[0], &plan, None)
            .await
            .unwrap();
        (accepted.run, ids[0].clone())
    }

    async fn a_run_with_one_finished_call(f: &Fixture) -> RunId {
        let accepted = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();
        let ids = f
            .coordinator
            .record_round(
                &accepted.run,
                AssistantRound {
                    round: 1,
                    text: Some("好的".into()),
                    text_ref: None,
                    tool_calls: vec![ToolCallRequest {
                        call_id: ToolCallId::from_raw("call-1"),
                        provider_call_id: "pc-1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "a.txt"}),
                        arguments_ref: None,
                    }],
                    provider_blocks: None,
                    usage: Default::default(),
                },
            )
            .await
            .unwrap();
        let plan = sample_plan("read", &f.session);
        f.coordinator.plan_call(&ids[0], &plan).await.unwrap();
        let attempt = f
            .coordinator
            .start_call(&ids[0], &plan, None)
            .await
            .unwrap();
        f.coordinator
            .finish_call(
                &attempt,
                PublishedOutput {
                    output: OutputRef(ContentRef {
                        path: "tool-output/r/c/a/output.json".into(),
                        size: 2,
                        hash: komo_kernel::types::digest::ContentHash::of_str("{}"),
                        pointer: None,
                    }),
                    status: ToolResultStatus::Completed,
                    elapsed_ms: 1,
                    preview: Some("ok".into()),
                    stdout: None,
                    stderr: None,
                },
            )
            .await
            .unwrap();
        accepted.run
    }

    #[tokio::test]
    async fn the_scan_set_is_the_in_flight_runs_plus_the_undelivered_terminal_ones() {
        let f = fixture().await;
        let open = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();
        let closed = f
            .coordinator
            .accept_input(input("api:2", &f.session))
            .await
            .unwrap();
        f.coordinator
            .complete(
                &closed.run,
                RunEnd::Completed {
                    final_message: None,
                    rounds: 1,
                },
            )
            .await
            .unwrap();
        // 没有投递义务的终态 Run（TUI / HTTP 来源）不进集合：结果由客户端补读。
        assert_eq!(f.store.unfinished_runs().await.unwrap().len(), 1);
        // 给它一行卡在 pending 的结果投递——这才是「客户端没有收到」。
        let deliveries = crate::repos::deliveries::TursoDeliveryRepo::new(f.db.clone());
        deliveries
            .record(
                &DeliveryId::from_raw("d-1"),
                &DeliveryTarget::to_peer(ChannelPeer::new(ChannelPlatform::Telegram, "42")),
                &komo_kernel::types::chat::Outbound::RunFinished {
                    session: f.session.clone(),
                    run: closed.run.clone(),
                    summary: "跑完了".into(),
                },
                TestClock::fixed().now(),
            )
            .await
            .unwrap();

        let unfinished = f.store.unfinished_runs().await.unwrap();
        let in_flight = unfinished
            .iter()
            .find(|row| row.run == open.run)
            .expect("未终态的那个在");
        assert_eq!(in_flight.session, f.session);
        assert_eq!(in_flight.state, RunState::Queued);
        assert!(in_flight.wait.is_none());
        assert!(in_flight.claimed_by.is_none());
        assert!(
            in_flight.result_delivered,
            "没有卡住的投递 = 没有欠着的结果"
        );

        // §8.4 第 10 行：已终态、结果还没送到，也要进扫描集合，否则
        // `RecoveryAction::RedeliverResult` 是一段永远走不到的代码。
        let undelivered = unfinished
            .iter()
            .find(|row| row.run == closed.run)
            .expect("已终态但没送到的那个也在");
        assert_eq!(undelivered.state, RunState::Completed);
        assert!(!undelivered.result_delivered);
        assert_eq!(unfinished.len(), 2);
    }

    /// 验收 B6：送达过的终态 Run **退出**扫描集合——补发一次之后它自己就不来了。
    #[tokio::test]
    async fn a_terminal_run_whose_result_reached_the_client_leaves_the_scan_set() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();
        f.coordinator
            .complete(
                &accepted.run,
                RunEnd::Completed {
                    final_message: Some("跑完了".into()),
                    rounds: 1,
                },
            )
            .await
            .unwrap();

        let deliveries = crate::repos::deliveries::TursoDeliveryRepo::new(f.db.clone());
        let id = DeliveryId::from_raw("d-1");
        deliveries
            .record(
                &id,
                &DeliveryTarget::to_peer(ChannelPeer::new(ChannelPlatform::Telegram, "42")),
                &komo_kernel::types::chat::Outbound::RunFinished {
                    session: f.session.clone(),
                    run: accepted.run.clone(),
                    summary: "跑完了".into(),
                },
                TestClock::fixed().now(),
            )
            .await
            .unwrap();
        assert_eq!(f.store.unfinished_runs().await.unwrap().len(), 1);
        deliveries
            .settle(
                &id,
                komo_kernel::types::chat::DeliveryState::Sent,
                None,
                TestClock::fixed().now(),
            )
            .await
            .unwrap();

        assert!(
            f.store.unfinished_runs().await.unwrap().is_empty(),
            "送到了就不该再被扫描出来"
        );
    }

    /// 等退避的 Run 把**在等什么**整份带出来——**重启不重置预算**（§8.5）。
    #[tokio::test]
    async fn a_run_waiting_for_a_backoff_carries_its_wait() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();
        let later = TestClock::fixed().now() + time::Duration::minutes(5);
        f.coordinator
            .suspend(
                &accepted.run,
                WaitReason::Retry {
                    attempts: 2,
                    not_before: later,
                    cause: RetryCause::Transport,
                },
            )
            .await
            .unwrap();

        let unfinished = f.store.unfinished_runs().await.unwrap();
        assert_eq!(unfinished[0].state, RunState::Waiting);
        assert_eq!(
            unfinished[0].wait,
            Some(WaitReason::Retry {
                attempts: 2,
                not_before: later,
                cause: RetryCause::Transport,
            }),
            "重启之后仍答得出在等什么、等到什么时候"
        );
    }

    /// 送达过结果的 Run 认得出来（§8.4 第 10 行）。
    #[tokio::test]
    async fn a_run_whose_result_was_delivered_is_flagged() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();

        let deliveries = crate::repos::deliveries::TursoDeliveryRepo::new(f.db.clone());
        let id = DeliveryId::from_raw("d-1");
        deliveries
            .record(
                &id,
                &DeliveryTarget::to_peer(ChannelPeer::new(ChannelPlatform::Telegram, "42")),
                &komo_kernel::types::chat::Outbound::RunFinished {
                    session: f.session.clone(),
                    run: accepted.run.clone(),
                    summary: "跑完了".into(),
                },
                TestClock::fixed().now(),
            )
            .await
            .unwrap();

        assert!(
            !f.store.unfinished_runs().await.unwrap()[0].result_delivered,
            "还在 pending 不算送到"
        );
        deliveries
            .settle(
                &id,
                komo_kernel::types::chat::DeliveryState::Sent,
                None,
                TestClock::fixed().now(),
            )
            .await
            .unwrap();
        assert!(f.store.unfinished_runs().await.unwrap()[0].result_delivered);
    }

    #[tokio::test]
    async fn reclaim_hands_back_what_another_instance_left_running() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();
        let queue = crate::repos::queue::TursoRunQueue::new(f.db.clone());
        queue
            .claim_run(&accepted.run, &ExecutorId::from_raw("exec-old"))
            .await
            .unwrap()
            .unwrap();

        let reclaimed = f
            .store
            .reclaim_running(&ExecutorId::from_raw("exec-now"))
            .await
            .unwrap();
        assert_eq!(reclaimed, 1);
        let unfinished = f.store.unfinished_runs().await.unwrap();
        // **只交还领取权**：状态留着 `running`，那一行是 reconcile 的输入（§8.9）——
        // store 判不了旧执行者停在哪一步，所以它不判。
        assert_eq!(unfinished[0].state, RunState::Running);
        assert!(unfinished[0].claimed_by.is_none(), "领取权交还了");
    }

    /// 验收 B2：回收领取权的**同一个事务**里，把那几个 Run 遗留的尝试标成 `interrupted`。
    ///
    /// 不标的话，`started` 而无结果的尝试在 §8.6 里读起来是"正在跑"——核对流程看到的是
    /// 一个永远跑不完的尝试。**`tool_calls` 的状态一个字不动**：它仍要走核对。
    #[tokio::test]
    async fn reclaim_also_closes_the_attempts_the_dead_instance_left_open() {
        let f = fixture().await;
        let (run, call) = a_run_with_an_open_attempt(&f).await;

        let queue = crate::repos::queue::TursoRunQueue::new(f.db.clone());
        queue
            .claim_run(&run, &ExecutorId::from_raw("exec-old"))
            .await
            .unwrap()
            .unwrap();

        let before = calls::attempts_of(&f.db, &call).await.unwrap();
        assert_eq!(before[0].state, "started");

        assert_eq!(
            f.store
                .reclaim_running(&ExecutorId::from_raw("exec-now"))
                .await
                .unwrap(),
            1
        );

        let after = calls::attempts_of(&f.db, &call).await.unwrap();
        assert_eq!(after[0].state, "interrupted", "那次尝试没有收尾");
        assert!(after[0].ended_at > 0, "记下它停在什么时候");
        assert_eq!(
            calls::list_for_run(&f.db, &run).await.unwrap()[0].state,
            "started",
            "调用状态不动——它仍要走 §8.6 的核对"
        );
    }

    /// 正常停机把 Run 放回了 `queued`，可它的尝试还停在 `started`——照样要收拾。
    ///
    /// 这是按"被回收的那几行"去收拾会漏掉的那一种：`RunQueue::release` 已经让它离开
    /// `running`，回收集合里一个都没有，而那次尝试确实没有收尾。
    #[tokio::test]
    async fn an_attempt_left_open_by_a_gracefully_stopped_instance_is_still_closed() {
        let f = fixture().await;
        let (run, call) = a_run_with_an_open_attempt(&f).await;

        let queue = crate::repos::queue::TursoRunQueue::new(f.db.clone());
        let old = ExecutorId::from_raw("exec-old");
        let claimed = queue.claim_run(&run, &old).await.unwrap().unwrap();
        // 交还名额：只清 `claimed_by`，状态不动（§8.7）——要回 `queued` 得由 reconcile
        // 按 §8.4 判，store 不替它猜。
        queue.release(&claimed).await.unwrap();
        assert_eq!(
            runs::get(&f.db, &run).await.unwrap().unwrap().state,
            RunState::Running
        );

        let reclaimed = f
            .store
            .reclaim_running(&ExecutorId::from_raw("exec-now"))
            .await
            .unwrap();
        assert_eq!(reclaimed, 0, "没有 running 行要回收");
        assert_eq!(
            calls::attempts_of(&f.db, &call).await.unwrap()[0].state,
            "interrupted",
            "可那次尝试还是没有收尾"
        );
    }

    /// 自己这一代开着的尝试不能被自己回收——那是**正在跑**的。
    #[tokio::test]
    async fn reclaim_leaves_this_instances_own_runs_alone() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();
        let queue = crate::repos::queue::TursoRunQueue::new(f.db.clone());
        let mine = ExecutorId::from_raw("exec-now");
        queue
            .claim_run(&accepted.run, &mine)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(f.store.reclaim_running(&mine).await.unwrap(), 0);
        let record = runs::get(&f.db, &accepted.run).await.unwrap().unwrap();
        assert_eq!(record.state, RunState::Running);
        assert_eq!(record.claimed_by.as_deref(), Some("exec-now"));
    }

    /// `backfill` 用 JSONL 补索引与派生状态，**不重放动作**，而且可以重复跑。
    #[tokio::test]
    async fn backfill_rebuilds_the_index_from_the_log_and_is_idempotent() {
        let f = fixture().await;
        let run = a_run_with_one_finished_call(&f).await;

        // 模拟"state.db 落后于 JSONL"：把索引与派生状态清掉。
        f.db.with_write_retry(|ex| {
            Box::pin(async move {
                for row in crate::models::SessionLogIndexRow::all()
                    .exec(&mut *ex)
                    .await
                    .map_err(crate::db::map_toasty)?
                {
                    row.delete()
                        .exec(&mut *ex)
                        .await
                        .map_err(crate::db::map_toasty)?;
                }
                for row in crate::models::ToolAttemptRow::all()
                    .exec(&mut *ex)
                    .await
                    .map_err(crate::db::map_toasty)?
                {
                    row.delete()
                        .exec(&mut *ex)
                        .await
                        .map_err(crate::db::map_toasty)?;
                }
                for row in crate::models::ToolCallRow::all()
                    .exec(&mut *ex)
                    .await
                    .map_err(crate::db::map_toasty)?
                {
                    row.delete()
                        .exec(&mut *ex)
                        .await
                        .map_err(crate::db::map_toasty)?;
                }
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
        assert!(
            crate::repos::calls::list_for_run(&f.db, &run)
                .await
                .unwrap()
                .is_empty()
        );

        f.store.backfill(&run).await.unwrap();

        let rebuilt = crate::repos::calls::list_for_run(&f.db, &run)
            .await
            .unwrap();
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].state, "completed", "结果引用照 JSONL 补回来");
        assert_eq!(rebuilt[0].attempts, 1);
        assert!(rebuilt[0].plan_hash.is_some());
        assert!(rebuilt[0].output_ref.is_some());
        assert_eq!(
            crate::repos::session::digests(&f.db, &f.session)
                .await
                .unwrap()
                .len(),
            6,
            "每一行都回到 session_log_index：accepted / queued / assistant / planned / started / result"
        );
        assert_eq!(
            crate::repos::session::get(&f.db, &f.session)
                .await
                .unwrap()
                .unwrap()
                .applied_seq,
            Seq(6)
        );

        // 再跑一遍：尝试次数不该膨胀。
        f.store.backfill(&run).await.unwrap();
        let again = crate::repos::calls::list_for_run(&f.db, &run)
            .await
            .unwrap();
        assert_eq!(again[0].attempts, 1, "回放不能把'跑了几次'算重");
        assert_eq!(
            crate::repos::calls::attempts_of(&f.db, &ToolCallId::from_raw("call-1"))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn backfill_refuses_a_run_it_has_never_heard_of() {
        let f = fixture().await;
        assert!(matches!(
            f.store.backfill(&RunId::from_raw("run-nope")).await,
            Err(StoreError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn requeue_puts_a_run_back_and_hands_back_the_claim() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();
        let queue = crate::repos::queue::TursoRunQueue::new(f.db.clone());
        queue
            .claim_run(&accepted.run, &ExecutorId::from_raw("exec-1"))
            .await
            .unwrap()
            .unwrap();

        f.store.requeue(&accepted.run).await.unwrap();
        let record = runs::get(&f.db, &accepted.run).await.unwrap().unwrap();
        assert_eq!(record.state, RunState::Queued);
        assert!(record.wait.is_none(), "回队列就是现在能跑，没有在等什么");
        assert!(record.claimed_by.is_none());
        // 交还之后别人领得到。
        assert!(
            queue
                .claim_run(&accepted.run, &ExecutorId::from_raw("exec-2"))
                .await
                .unwrap()
                .is_some()
        );
    }

    /// 回放把老行的 `input_seq` 补上（§8.3）：`0` = 未知，不补它那条 Run 的次序会一直
    /// 退化到按 id 比——而 UUIDv7 的字典序不是可靠的创建序。
    #[tokio::test]
    async fn backfill_fills_in_the_input_seq_of_a_run_accepted_before_this_change() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();
        // 造一条"这次改造之前受理的"行：引用与 seq 都缺。
        let run_id = accepted.run.to_string();
        f.db.with_write_retry(move |ex| {
            let run_id = run_id.clone();
            Box::pin(async move {
                toasty::sql::statement(
                    "UPDATE runs SET input_event = NULL, input_seq = 0 WHERE id = ?1",
                )
                .bind(run_id)
                .exec(ex)
                .await
                .map(|_| ())
                .map_err(crate::db::map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
        let before = runs::get(&f.db, &accepted.run).await.unwrap().unwrap();
        assert_eq!(before.input_seq, Seq(0), "先把这一列打回未知");
        assert!(before.input_event.is_none());

        f.store.backfill(&accepted.run).await.unwrap();

        let record = runs::get(&f.db, &accepted.run).await.unwrap().unwrap();
        assert!(record.input_event.is_some(), "引用补上了");
        assert_eq!(
            record.input_seq,
            Seq(1),
            "`run.accepted` 是这个会话的第一条事件"
        );
    }

    /// **回放不覆盖数据库已经记下的取消**（§8.5）。
    #[tokio::test]
    async fn a_cancelled_run_is_not_brought_back_to_life() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();
        f.coordinator
            .complete(&accepted.run, RunEnd::Cancelled { by: None })
            .await
            .unwrap();

        f.store.requeue(&accepted.run).await.unwrap();
        f.store
            .mark_needs_attention(&accepted.run, "结果不明")
            .await
            .unwrap();
        // 第 10 行「补发原结果」走的也是 backfill——它同样不能把这一行推回队列。
        f.store.backfill(&accepted.run).await.unwrap();

        let record = runs::get(&f.db, &accepted.run).await.unwrap().unwrap();
        assert_eq!(record.state, RunState::Cancelled, "终态不动");
        assert!(record.last_error.is_none());
        assert!(record.input_event.is_some(), "引用还是补上了");
    }

    #[tokio::test]
    async fn needs_attention_records_the_reason_and_frees_the_claim() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", &f.session))
            .await
            .unwrap();
        let queue = crate::repos::queue::TursoRunQueue::new(f.db.clone());
        queue
            .claim_run(&accepted.run, &ExecutorId::from_raw("exec-1"))
            .await
            .unwrap()
            .unwrap();

        f.store
            .mark_needs_attention(&accepted.run, "输出引用哈希不符")
            .await
            .unwrap();
        let record = runs::get(&f.db, &accepted.run).await.unwrap().unwrap();
        assert_eq!(record.state, RunState::Waiting);
        assert_eq!(
            record.wait,
            Some(WaitReason::Intervention {
                intervention: InterventionId::for_run(&accepted.run),
            })
        );
        assert_eq!(record.last_error.as_deref(), Some("输出引用哈希不符"));
        assert!(record.claimed_by.is_none());
    }
}
