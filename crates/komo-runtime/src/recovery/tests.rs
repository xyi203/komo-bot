//! §8.4 的决策表，**每一行一个场景**——事件由 [`MemLedger`] 真的写出来，观察由
//! [`RecoveryScan::observe`] 真的采集，决定由 kernel 的纯函数给出，动作落在一个记账用的
//! [`RecoveryIndex`] 替身上。
//!
//! 「仅检查"恢复后状态变成 running"不算通过」（§14），所以每条都同时断言**决定**和
//! **照它做了什么**。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::protocol::http::{ApprovalDecisionRecord, ApprovalRecord};
use komo_kernel::test_support::{MemApprovalRepo, MemLedger, MemOutputStore, TestClock};
use komo_kernel::traits::{ApprovalRepo, Ledger, OutputWriter, ToolOutputStore};
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::ids::{ApprovalId, RequestKey, ShortId, ToolCallId};
use komo_kernel::types::plan::{ExecutionPlan, PlanSource};
use komo_kernel::types::refs::{
    AttemptRef, ContentRef, OutputRef, ToolResultBody, ToolResultStatus,
};
use komo_kernel::types::status::{RetryCause, RunEnd, RunState, SessionState, WaitReason};
use komo_kernel::types::turn::{AcceptInput, AssistantRound, ToolCallRequest};
use time::macros::datetime;

use super::*;

const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

#[test]
fn corrupt_runs_in_one_session_form_one_operator_notification_group() {
    let session = SessionId::from_raw("session-broken");
    let reason = "会话读不出来".to_string();
    let report = RecoveryReport {
        reclaimed: 0,
        outcomes: ["run-1", "run-2", "run-3"]
            .into_iter()
            .map(|run| RecoveryOutcome {
                run: RunId::from_raw(run),
                session: session.clone(),
                action: RecoveryAction::HaltCorrupt {
                    reason: reason.clone(),
                },
                applied: Applied::NeedsOperator,
            })
            .collect(),
    };

    let groups = report.corrupt_groups();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].session, session);
    assert_eq!(groups[0].reason, reason);
    assert_eq!(groups[0].runs.len(), 3);
}

// ---------------------------------------------------------------- 替身

/// 记账用的索引：恢复做了什么，事后数得出来。
#[derive(Debug, Default)]
struct MemIndex {
    runs: Mutex<Vec<UnfinishedRun>>,
    state: Mutex<IndexState>,
    /// `sessions.state`（§8.10）。默认是一处世外桃源：`active` 且内容在。
    session: Mutex<Option<SessionState>>,
    /// 内容在不在（§8.9）。默认在；要验"内容缺失而状态没说不服务"就把它设成 `false`。
    content: Mutex<bool>,
}

#[derive(Debug, Default)]
struct IndexState {
    reclaimed: u64,
    backfilled: Vec<RunId>,
    requeued: Vec<RunId>,
    attention: Vec<(RunId, String)>,
}

impl MemIndex {
    fn with(runs: Vec<UnfinishedRun>) -> Arc<Self> {
        Arc::new(MemIndex {
            runs: Mutex::new(runs),
            state: Mutex::new(IndexState {
                reclaimed: 1,
                ..Default::default()
            }),
            // §8.9 的默认观察：会话在服务范围里、内容也在。`None` = 连会话行都不在了。
            session: Mutex::new(Some(SessionState::Active)),
            content: Mutex::new(true),
        })
    }

    /// `sessions.state` 这一列（§8.10）。`None` = 那一行都没了。
    fn set_session_state(&self, state: Option<SessionState>) {
        *self.session.lock().unwrap() = state;
    }

    /// 内容在不在（§8.9 的第二个问题）：`false` = 目录/日志读不出来。
    fn set_session_content(&self, available: bool) {
        *self.content.lock().unwrap() = available;
    }

    fn requeued(&self) -> Vec<RunId> {
        self.state.lock().unwrap().requeued.clone()
    }

    fn backfilled(&self) -> Vec<RunId> {
        self.state.lock().unwrap().backfilled.clone()
    }

    fn attention(&self) -> Vec<(RunId, String)> {
        self.state.lock().unwrap().attention.clone()
    }
}

#[async_trait]
impl RecoveryIndex for MemIndex {
    async fn unfinished_runs(&self) -> Result<Vec<UnfinishedRun>, StoreError> {
        Ok(self.runs.lock().unwrap().clone())
    }

    async fn reclaim_running(&self, _executor: &ExecutorId) -> Result<u64, StoreError> {
        Ok(self.state.lock().unwrap().reclaimed)
    }

    async fn backfill(&self, run: &RunId) -> Result<(), StoreError> {
        self.state.lock().unwrap().backfilled.push(run.clone());
        Ok(())
    }

    async fn requeue(&self, run: &RunId) -> Result<(), StoreError> {
        self.state.lock().unwrap().requeued.push(run.clone());
        Ok(())
    }

    async fn mark_needs_attention(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.state
            .lock()
            .unwrap()
            .attention
            .push((run.clone(), reason.to_string()));
        Ok(())
    }

    async fn session_state(
        &self,
        _session: &SessionId,
    ) -> Result<Option<SessionState>, StoreError> {
        Ok(*self.session.lock().unwrap())
    }

    async fn session_content(&self, _session: &SessionId) -> Result<bool, StoreError> {
        Ok(*self.content.lock().unwrap())
    }
}

#[derive(Debug)]
struct FakeLiveness(bool);

impl ExecutorLiveness for FakeLiveness {
    fn stopped(&self, _previous: Option<&ExecutorId>) -> bool {
        self.0
    }
}

/// 一个会话读不出来的账本：除了那一个会话，其余一律照常。
///
/// 中间损坏、半行、已提交范围缺失——在这一层都长一个样：`read` 报
/// [`LedgerError::Corrupt`]。
struct PoisonedLedger {
    inner: Arc<MemLedger>,
    corrupt: SessionId,
    /// 补记结果这一步也失败（例如冷进程里 attempt → session 的路由丢了）。
    finish_fails: bool,
}

impl PoisonedLedger {
    fn corrupting(inner: Arc<MemLedger>, corrupt: SessionId) -> Arc<Self> {
        Arc::new(PoisonedLedger {
            inner,
            corrupt,
            finish_fails: false,
        })
    }

    /// 会话读得出来，但补记结果会失败。
    fn unroutable(inner: Arc<MemLedger>) -> Arc<Self> {
        Arc::new(PoisonedLedger {
            inner,
            corrupt: SessionId::from_raw("没有这个会话"),
            finish_fails: true,
        })
    }
}

#[async_trait]
impl Ledger for PoisonedLedger {
    async fn accept_input(
        &self,
        input: AcceptInput,
    ) -> Result<komo_kernel::types::turn::Accepted, LedgerError> {
        self.inner.accept_input(input).await
    }
    async fn record_round(
        &self,
        run: &RunId,
        round: AssistantRound,
    ) -> Result<Vec<ToolCallId>, LedgerError> {
        self.inner.record_round(run, round).await
    }
    async fn start_run(
        &self,
        run: &RunId,
        executor: &ExecutorId,
        generation: u64,
    ) -> Result<(), LedgerError> {
        self.inner.start_run(run, executor, generation).await
    }
    async fn plan_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
    ) -> Result<komo_kernel::types::ids::EventId, LedgerError> {
        self.inner.plan_call(call, plan).await
    }
    async fn start_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
        grant: Option<komo_kernel::types::turn::GrantUse>,
    ) -> Result<komo_kernel::types::ids::AttemptId, LedgerError> {
        self.inner.start_call(call, plan, grant).await
    }
    async fn finish_call(
        &self,
        attempt: &komo_kernel::types::ids::AttemptId,
        published: komo_kernel::types::refs::PublishedOutput,
    ) -> Result<(), LedgerError> {
        if self.finish_fails {
            return Err(LedgerError::NotFound {
                what: format!("尝试 {attempt} 属于哪个会话"),
            });
        }
        self.inner.finish_call(attempt, published).await
    }

    async fn fail_call(
        &self,
        call: &komo_kernel::types::ids::ToolCallId,
        attempt: &komo_kernel::types::ids::AttemptId,
        published: komo_kernel::types::refs::PublishedOutput,
    ) -> Result<(), LedgerError> {
        self.inner.fail_call(call, attempt, published).await
    }
    async fn suspend(&self, run: &RunId, wait: WaitReason) -> Result<(), LedgerError> {
        self.inner.suspend(run, wait).await
    }
    async fn complete(&self, run: &RunId, end: RunEnd) -> Result<(), LedgerError> {
        self.inner.complete(run, end).await
    }
    async fn read(
        &self,
        session: &SessionId,
        from: komo_kernel::types::ids::Seq,
        limit: u32,
    ) -> Result<komo_kernel::types::turn::EventBatch, LedgerError> {
        if session == &self.corrupt {
            return Err(LedgerError::Corrupt(format!(
                "{session} 的 JSONL 中间缺了一段，seq 不连续"
            )));
        }
        self.inner.read(session, from, limit).await
    }
    async fn boundary(
        &self,
        session: &SessionId,
    ) -> Result<komo_kernel::types::ids::Seq, LedgerError> {
        self.inner.boundary(session).await
    }
    async fn append_audit(
        &self,
        session: &SessionId,
        event_id: &komo_kernel::types::ids::EventId,
        payload: komo_kernel::events::EventPayload,
        at: OffsetDateTime,
    ) -> Result<komo_kernel::types::ids::Seq, LedgerError> {
        self.inner
            .append_audit(session, event_id, payload, at)
            .await
    }
    async fn run_end(&self, run: &RunId) -> Result<Option<RunEnd>, LedgerError> {
        self.inner.run_end(run).await
    }
}

/// 按 attempt 交出一份已经落盘的孤儿 `output.json`。
#[derive(Debug, Default)]
struct FoundOrphans {
    found: Mutex<Vec<(AttemptRef, PublishedOutput)>>,
}

impl FoundOrphans {
    fn with(attempt: AttemptRef, published: PublishedOutput) -> Arc<Self> {
        Arc::new(FoundOrphans {
            found: Mutex::new(vec![(attempt, published)]),
        })
    }
}

#[async_trait]
impl OrphanOutputs for FoundOrphans {
    async fn find(&self, attempt: &AttemptRef) -> Result<Option<PublishedOutput>, StoreError> {
        Ok(self
            .found
            .lock()
            .unwrap()
            .iter()
            .find(|(reference, _)| reference == attempt)
            .map(|(_, published)| published.clone()))
    }
}

/// 输出正文**丢了**（不是哈希不符）的存储。
#[derive(Debug)]
struct LostOutputs;

#[async_trait]
impl ToolOutputStore for LostOutputs {
    async fn begin(&self, _attempt: &AttemptRef) -> Result<Box<dyn OutputWriter>, StoreError> {
        unimplemented!("恢复扫描只读")
    }
    async fn publish(
        &self,
        _writer: Box<dyn OutputWriter>,
        _result: ToolResultBody,
    ) -> Result<komo_kernel::types::refs::PublishedOutput, StoreError> {
        unimplemented!("恢复扫描只读")
    }
    async fn open(
        &self,
        output: &OutputRef,
    ) -> Result<komo_kernel::types::refs::VerifiedOutput, StoreError> {
        Err(StoreError::NotFound {
            what: output.0.path.clone(),
        })
    }
}

// ---------------------------------------------------------------- 夹具

struct World {
    ledger: Arc<MemLedger>,
    outputs: Arc<MemOutputStore>,
    approvals: Arc<MemApprovalRepo>,
    clock: TestClock,
    session: SessionId,
}

impl World {
    fn new() -> Self {
        let clock = TestClock::at(NOW);
        World {
            ledger: Arc::new(MemLedger::new(clock.clone())),
            outputs: Arc::new(MemOutputStore::new()),
            approvals: Arc::new(MemApprovalRepo::new()),
            clock,
            session: SessionId::from_raw("01a0a414-7800-7bbd-8fa1-632b66973666"),
        }
    }

    /// 输入落盘：`run.accepted` + `run.queued`。
    async fn accept(&self) -> RunId {
        self.accept_in(&self.session, "api:1").await
    }

    /// 同上，但落在指定会话上。
    async fn accept_in(&self, session: &SessionId, key: &str) -> RunId {
        self.ledger
            .accept_input(AcceptInput {
                session: session.clone(),
                request_key: RequestKey::new(key),
                text: "帮我看看".into(),
                source: PlanSource::Interactive {
                    session: session.clone(),
                },
                peer: None,
                model: komo_kernel::test_support::sample_model(),
                workdir: None,
                // 这条输入不是委派：它的 Run 没有父。
                delegate: None,
                snapshot: None,
                skip_memory: false,
                at: NOW,
            })
            .await
            .unwrap()
            .run
    }

    /// 一轮 assistant 回复，带 `calls` 个调用。
    async fn round(&self, run: &RunId, calls: &[&str]) -> Vec<ToolCallId> {
        self.ledger
            .record_round(
                run,
                AssistantRound {
                    round: 1,
                    text: None,
                    text_ref: None,
                    tool_calls: calls
                        .iter()
                        .map(|id| ToolCallRequest {
                            call_id: ToolCallId::from_raw(*id),
                            provider_call_id: format!("p-{id}"),
                            name: "shell".into(),
                            arguments: serde_json::json!({ "command": "echo hi" }),
                            arguments_ref: None,
                        })
                        .collect(),
                    provider_blocks: None,
                    usage: Default::default(),
                },
            )
            .await
            .unwrap()
    }

    fn plan(&self, call: &ToolCallId) -> ExecutionPlan {
        let mut plan = komo_kernel::test_support::sample_plan("shell", &self.session);
        plan.tool_call = Some(call.clone());
        plan
    }

    fn run_row(&self, run: &RunId, state: RunState) -> UnfinishedRun {
        self.run_row_in(run, &self.session, state)
    }

    fn run_row_in(&self, run: &RunId, session: &SessionId, state: RunState) -> UnfinishedRun {
        UnfinishedRun {
            run: run.clone(),
            session: session.clone(),
            state,
            wait: None,
            claimed_by: None,
            result_delivered: false,
        }
    }

    /// 停在某个外部条件上的 Run（§8.4：状态只有一个 `waiting`，理由在 `WaitReason`）。
    fn waiting_row(&self, run: &RunId, wait: WaitReason) -> UnfinishedRun {
        let mut row = self.run_row(run, RunState::Waiting);
        row.wait = Some(wait);
        row
    }

    fn scan(&self, rows: Vec<UnfinishedRun>) -> (RecoveryScan, Arc<MemIndex>) {
        self.scan_with(
            Arc::clone(&self.ledger) as Arc<dyn Ledger>,
            Arc::clone(&self.outputs) as Arc<dyn ToolOutputStore>,
            rows,
            true,
        )
    }

    fn scan_with(
        &self,
        ledger: Arc<dyn Ledger>,
        outputs: Arc<dyn ToolOutputStore>,
        rows: Vec<UnfinishedRun>,
        previous_stopped: bool,
    ) -> (RecoveryScan, Arc<MemIndex>) {
        self.scan_full(
            ledger,
            outputs,
            Arc::new(NoOrphanLookup),
            rows,
            previous_stopped,
        )
    }

    fn scan_full(
        &self,
        ledger: Arc<dyn Ledger>,
        outputs: Arc<dyn ToolOutputStore>,
        orphans: Arc<dyn OrphanOutputs>,
        rows: Vec<UnfinishedRun>,
        previous_stopped: bool,
    ) -> (RecoveryScan, Arc<MemIndex>) {
        let index = MemIndex::with(rows);
        let scan = RecoveryScan::new(
            ledger,
            Arc::clone(&index) as Arc<dyn RecoveryIndex>,
            outputs,
            Arc::clone(&self.approvals) as Arc<dyn ApprovalRepo>,
            Arc::new(self.clock.clone()),
            ExecutorId::from_raw("exec-now"),
            Arc::new(FakeLiveness(previous_stopped)),
        )
        .with_orphan_outputs(orphans);
        (scan, index)
    }

    /// 为一次尝试真的发布一份 `output.json`。
    async fn publish(
        &self,
        run: &RunId,
        call: &ToolCallId,
        attempt: &komo_kernel::types::ids::AttemptId,
    ) -> PublishedOutput {
        let reference = AttemptRef {
            session: self.session.clone(),
            run: run.clone(),
            call: call.clone(),
            attempt: attempt.clone(),
        };
        let writer = self.outputs.begin(&reference).await.unwrap();
        self.outputs
            .publish(
                writer,
                ToolResultBody {
                    status: ToolResultStatus::Completed,
                    result: serde_json::json!({ "stdout": "hi" }),
                    error: None,
                    exit_code: Some(0),
                    artifacts: vec![],
                    preview: None,
                },
            )
            .await
            .unwrap()
    }
}

// ---------------------------------------------------------------- 逐行

/// 第 1 行：输入已在 JSONL 持久保存，Run 尚未开始 → 补齐索引后自动入队。
#[tokio::test]
async fn row_1_input_persisted_but_the_run_never_started() {
    let world = World::new();
    let run = world.accept().await;
    let (scan, index) = world.scan(vec![world.run_row(&run, RunState::Accepted)]);

    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::BackfillIndexAndQueue
    );
    assert_eq!(report.outcomes[0].applied, Applied::Requeued);
    assert_eq!(index.backfilled(), vec![run.clone()]);
    assert_eq!(index.requeued(), vec![run]);
    assert_eq!(report.summary(), "1 个任务已接续");
}

/// 第 2 行：只预留了 ID，正文没落盘 → 等同请求键重传，**不能补造用户指令**。
#[tokio::test]
async fn row_2_only_the_id_was_reserved() {
    let world = World::new();
    let run = RunId::from_raw("run-ghost");
    let (scan, index) = world.scan(vec![world.run_row(&run, RunState::Accepted)]);

    let report = scan.scan().await.unwrap();
    assert_eq!(report.outcomes[0].action, RecoveryAction::AwaitResend);
    assert_eq!(report.outcomes[0].applied, Applied::LeftAsIs);
    assert!(index.requeued().is_empty(), "不入队，等重传");
    assert!(index.backfilled().is_empty());
}

/// 第 3 行：正在请求 LLM，完整回复尚未保存 → 丢弃未完成输出，重新请求。
#[tokio::test]
async fn row_3_the_model_reply_never_arrived_in_full() {
    let world = World::new();
    let run = world.accept().await;
    world
        .ledger
        .start_run(&run, &ExecutorId::from_raw("exec-old"), 1)
        .await
        .unwrap();

    let (scan, index) = world.scan(vec![world.run_row(&run, RunState::Running)]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::DiscardPartialAndRerequestModel
    );
    assert_eq!(index.requeued(), vec![run]);
}

/// 第 4 行：assistant 回复已保存、没有未完成调用 → 沿用原计划接着跑。
#[tokio::test]
async fn row_4_the_round_is_persisted_and_nothing_is_left_unfinished() {
    let world = World::new();
    let run = world.accept().await;
    world.round(&run, &[]).await;

    let (scan, index) = world.scan(vec![world.run_row(&run, RunState::Running)]);
    let report = scan.scan().await.unwrap();
    assert_eq!(report.outcomes[0].action, RecoveryAction::ResumeFromPlan);
    assert_eq!(index.requeued(), vec![run]);
}

/// 第 5 行：JSONL 已有结果引用且输出校验通过 → 补结果索引，**复用原输出，不重放动作**。
#[tokio::test]
async fn row_5_the_result_is_on_disk_and_verified_while_the_database_lags() {
    let world = World::new();
    let run = world.accept().await;
    let calls = world.round(&run, &["call-1"]).await;
    let plan = world.plan(&calls[0]);
    world.ledger.plan_call(&calls[0], &plan).await.unwrap();
    let attempt = world
        .ledger
        .start_call(&calls[0], &plan, None)
        .await
        .unwrap();

    let writer = world
        .outputs
        .begin(&AttemptRef {
            session: world.session.clone(),
            run: run.clone(),
            call: calls[0].clone(),
            attempt: attempt.clone(),
        })
        .await
        .unwrap();
    let published = world
        .outputs
        .publish(
            writer,
            ToolResultBody {
                status: ToolResultStatus::Completed,
                result: serde_json::json!({ "stdout": "hi" }),
                error: None,
                exit_code: Some(0),
                artifacts: vec![],
                preview: None,
            },
        )
        .await
        .unwrap();
    world.ledger.finish_call(&attempt, published).await.unwrap();

    let (scan, index) = world.scan(vec![world.run_row(&run, RunState::Running)]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::BackfillResultAndContinue {
            call: calls[0].clone()
        }
    );
    assert_eq!(index.requeued(), vec![run]);
}

/// 第 6 行：调用 planned，**确定尚未执行** → 自动执行原计划。
#[tokio::test]
async fn row_6_the_call_is_planned_and_certainly_never_ran() {
    let world = World::new();
    let run = world.accept().await;
    let calls = world.round(&run, &["call-1"]).await;
    let plan = world.plan(&calls[0]);
    world.ledger.plan_call(&calls[0], &plan).await.unwrap();

    let (scan, index) = world.scan(vec![world.run_row(&run, RunState::Running)]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::ExecutePlannedCall {
            call: calls[0].clone()
        }
    );
    assert_eq!(index.requeued(), vec![run]);
    assert!(index.attention().is_empty());
}

/// 第 7 行：调用 started 而没有结果 → 先核对外部效果。
#[tokio::test]
async fn row_7_the_call_started_and_left_no_result() {
    let world = World::new();
    let run = world.accept().await;
    let calls = world.round(&run, &["call-1"]).await;
    let plan = world.plan(&calls[0]);
    world.ledger.plan_call(&calls[0], &plan).await.unwrap();
    world
        .ledger
        .start_call(&calls[0], &plan, None)
        .await
        .unwrap();

    let (scan, _index) = world.scan(vec![world.run_row(&run, RunState::Running)]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::VerifyEffect {
            call: calls[0].clone()
        },
        "started 本身不证明副作用已发生，也不证明没发生"
    );
}

/// 第 8 行：等待审批 → 保留原请求；已答复且仍有效的自动接续，**不因重启再问一次**。
#[tokio::test]
async fn row_8_waiting_on_an_approval() {
    let approval = ApprovalId::from_raw("ap-1");

    // (a) 还没人答。
    let world = World::new();
    let run = world.accept().await;
    world
        .ledger
        .suspend(
            &run,
            WaitReason::Approval {
                approval: approval.clone(),
            },
        )
        .await
        .unwrap();
    world
        .approvals
        .create(approval_record(&approval, &world.session, &run, None))
        .await
        .unwrap();

    let (scan, index) = world.scan(vec![world.waiting_row(
        &run,
        WaitReason::Approval {
            approval: approval.clone(),
        },
    )]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::KeepWaiting {
            reason: WaitReason::Approval {
                approval: approval.clone()
            }
        }
    );
    assert!(index.requeued().is_empty());
    assert_eq!(report.summary(), "1 个在等待（审批 / 干预 / 退避）");

    // (b) 答了，而且还有效。
    let world = World::new();
    let run = world.accept().await;
    world
        .ledger
        .suspend(
            &run,
            WaitReason::Approval {
                approval: approval.clone(),
            },
        )
        .await
        .unwrap();
    world
        .approvals
        .create(approval_record(
            &approval,
            &world.session,
            &run,
            Some(ApprovalDecisionRecord {
                approved: true,
                scope: ApprovalScope::Once,
                by: None,
                decided_at: NOW,
                grant: None,
                consumed: false,
            }),
        ))
        .await
        .unwrap();

    let (scan, index) = world.scan(vec![world.waiting_row(
        &run,
        WaitReason::Approval {
            approval: approval.clone(),
        },
    )]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::ResumeWithDecision {
            approval: approval.clone(),
            approved: true
        },
        "审批无需用户因重启再答一次"
    );
    assert_eq!(index.requeued(), vec![run]);

    // (c) 答了，但已经过期 → 重新审核。
    let world = World::new();
    let run = world.accept().await;
    world
        .ledger
        .suspend(
            &run,
            WaitReason::Approval {
                approval: approval.clone(),
            },
        )
        .await
        .unwrap();
    let mut expired = approval_record(
        &approval,
        &world.session,
        &run,
        Some(ApprovalDecisionRecord {
            approved: true,
            scope: ApprovalScope::Once,
            by: None,
            decided_at: NOW - time::Duration::hours(2),
            grant: None,
            consumed: false,
        }),
    );
    expired.valid_until = Some(NOW - time::Duration::hours(1));
    world.approvals.create(expired).await.unwrap();

    let (scan, index) = world.scan(vec![world.waiting_row(
        &run,
        WaitReason::Approval {
            approval: approval.clone(),
        },
    )]);
    let report = scan.scan().await.unwrap();
    assert!(
        matches!(
            report.outcomes[0].action,
            RecoveryAction::WaitingForOperator { .. }
        ),
        "{:?}",
        report.outcomes[0]
    );
    assert_eq!(index.attention().len(), 1);
}

/// 表外但同段规定：数据库里没有这条审批 → 请操作者重答，审计副本不能创建授权。
#[tokio::test]
async fn an_approval_the_database_lost_asks_the_operator_again() {
    let world = World::new();
    let run = world.accept().await;
    world
        .ledger
        .suspend(
            &run,
            WaitReason::Approval {
                approval: ApprovalId::from_raw("ap-gone"),
            },
        )
        .await
        .unwrap();

    let (scan, index) = world.scan(vec![world.waiting_row(
        &run,
        WaitReason::Approval {
            approval: ApprovalId::from_raw("ap-gone"),
        },
    )]);
    let report = scan.scan().await.unwrap();
    let RecoveryAction::WaitingForOperator { reason } = &report.outcomes[0].action else {
        panic!("{:?}", report.outcomes[0])
    };
    assert!(reason.contains("审计副本"), "{reason}");
    assert_eq!(index.attention()[0].1, *reason);
}

/// 第 9 行：等待退避 → 沿用已保存的次数与时间，**重启不重置预算**。
#[tokio::test]
async fn row_9_waiting_for_a_backoff_to_expire() {
    let world = World::new();
    let run = world.accept().await;
    let at = NOW + time::Duration::minutes(5);
    world
        .ledger
        .suspend(
            &run,
            WaitReason::Retry {
                attempts: 2,
                not_before: at,
                cause: RetryCause::Transport,
            },
        )
        .await
        .unwrap();

    // 次数与到点时刻现在就在 `WaitReason::Retry` 里——**没有一个平行的 `retry` 观察**，
    // 所以"重启不重置预算"这件事只有一个来源可读。
    let row = world.waiting_row(
        &run,
        WaitReason::Retry {
            attempts: 2,
            not_before: at,
            cause: RetryCause::Transport,
        },
    );
    let (scan, index) = world.scan(vec![row]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::WaitUntilRetry { at, attempts: 2 }
    );
    assert!(index.requeued().is_empty());
    assert_eq!(
        report.outcomes[0].applied,
        Applied::LeftAsIs,
        "到点之前原样不动"
    );
}

/// 第 10 行：结果已保存但客户端没收到 → 补发或补读，**不重新执行任务**。
#[tokio::test]
async fn row_10_the_result_is_final_but_the_client_never_saw_it() {
    let world = World::new();
    let run = world.accept().await;
    world.round(&run, &[]).await;
    world
        .ledger
        .complete(
            &run,
            RunEnd::Completed {
                final_message: Some("看完了".into()),
                rounds: 1,
            },
        )
        .await
        .unwrap();

    let mut row = world.run_row(&run, RunState::Completed);
    row.result_delivered = false;
    let (scan, index) = world.scan(vec![row]);
    let report = scan.scan().await.unwrap();

    assert_eq!(report.outcomes[0].action, RecoveryAction::RedeliverResult);
    assert_eq!(report.outcomes[0].applied, Applied::Redeliver);
    assert!(index.requeued().is_empty(), "不重新执行");
    assert_eq!(report.to_redeliver().len(), 1);
}

/// 第 11 行：终态就保持终态，不因重启自动开启新一轮。
#[tokio::test]
async fn row_11_a_terminal_run_stays_terminal() {
    for (state, end) in [
        (
            RunState::Completed,
            RunEnd::Completed {
                final_message: None,
                rounds: 1,
            },
        ),
        (
            RunState::Failed,
            RunEnd::Failed {
                reason: "模型不可用".into(),
            },
        ),
        (RunState::Cancelled, RunEnd::Cancelled { by: None }),
    ] {
        let world = World::new();
        let run = world.accept().await;
        world.ledger.complete(&run, end).await.unwrap();

        let mut row = world.run_row(&run, state);
        row.result_delivered = true;
        let (scan, index) = world.scan(vec![row]);
        let report = scan.scan().await.unwrap();

        assert_eq!(
            report.outcomes[0].action,
            RecoveryAction::KeepTerminal { state },
            "{state:?}"
        );
        assert!(index.requeued().is_empty(), "{state:?}");
    }
}

/// 第 11 行的另一半：操作者放弃（`abandoned`）也是终态，重启不给它开第二轮（§7.5）。
#[tokio::test]
async fn an_abandoned_run_stays_terminal_too() {
    let world = World::new();
    let run = world.accept().await;
    // 放弃走网关那条 `Ledger::complete(RunEnd::Abandoned { .. })`——与取消分开记，
    // 事后统计"多少人放弃了什么"才有意义（§8.4）。
    world
        .ledger
        .complete(
            &run,
            RunEnd::Abandoned {
                by: None,
                reason: Some("查过了，不追究".into()),
            },
        )
        .await
        .unwrap();

    let mut row = world.run_row(&run, RunState::Abandoned);
    row.result_delivered = true;
    let (scan, index) = world.scan(vec![row]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::KeepTerminal {
            state: RunState::Abandoned
        }
    );
    assert_eq!(report.outcomes[0].applied, Applied::LeftAsIs);
    assert!(index.requeued().is_empty(), "不因重启自动开启新一轮");
}

/// 等前一条 Run 的那一条：**原样不动**。放它出来是 reconcile 的事（前一条进终态时由
/// `wait_kind = 'dependency'` 那条 UPDATE 机械放行），恢复扫描不替它判（§8.4、§8.9）。
#[tokio::test]
async fn a_dependency_wait_is_left_alone_for_the_reconciler() {
    let world = World::new();
    let run = world.accept().await;
    let earlier = RunId::from_raw("run-earlier");
    let row = world.waiting_row(
        &run,
        WaitReason::Dependency {
            run: earlier.clone(),
        },
    );

    let (scan, index) = world.scan(vec![row]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::KeepWaiting {
            reason: WaitReason::Dependency { run: earlier }
        },
        "状态是 waiting，理由也说得出来在等谁"
    );
    assert_eq!(report.outcomes[0].applied, Applied::LeftAsIs);
    assert!(index.requeued().is_empty());
    assert!(index.attention().is_empty(), "等前一条 Run 不是在等人");
}

/// 验收口径的反面：状态说 `waiting` 却说不清在等什么——**这是损坏，不是"没关系"**。
/// 恢复扫描不替它编一个理由，把它停到人那里（§8.4）。
#[tokio::test]
async fn a_waiting_run_that_cannot_say_what_it_waits_for_goes_to_a_human() {
    let world = World::new();
    let run = world.accept().await;
    let (scan, index) = world.scan(vec![world.run_row(&run, RunState::Waiting)]);

    let report = scan.scan().await.unwrap();
    let RecoveryAction::WaitingForOperator { reason } = &report.outcomes[0].action else {
        panic!("{:?}", report.outcomes[0])
    };
    assert!(reason.contains("却没有记录在等什么"), "{reason}");
    assert!(index.requeued().is_empty(), "不许凭空造一个理由再放它去跑");
}

/// §8.9 的第一问：这个会话还在服务范围里吗。不在 → **不许领**，停成
/// `waiting + intervention`，理由说清是哪一种（会话已删 / 内容缺失）。
#[tokio::test]
async fn a_session_outside_the_serving_range_stops_the_run_for_the_operator() {
    for (observed, expected) in [
        (Some(SessionState::Deleted), "逻辑删除"),
        (Some(SessionState::Purged), "回收"),
        // 连会话行都不在了：数据库与内容对不上，同样不许按空上下文继续。
        (None, "回收"),
    ] {
        let world = World::new();
        let run = world.accept().await;
        let (scan, index) = world.scan(vec![world.run_row(&run, RunState::Accepted)]);
        index.set_session_state(observed);

        let report = scan.scan().await.unwrap();
        let RecoveryAction::WaitingForOperator { reason } = &report.outcomes[0].action else {
            panic!("{:?}", report.outcomes[0])
        };
        assert!(reason.contains(expected), "{observed:?}：{reason}");
        assert_eq!(report.outcomes[0].applied, Applied::NeedsOperator);
        assert_eq!(index.attention().len(), 1, "{observed:?}");
        assert!(
            index.requeued().is_empty(),
            "一个不服务的会话不许被领走（{observed:?}）"
        );
    }
}

/// §8.9 的第二个问题单独成一条：**内容读不出来，而会话状态没说"不服务"**。
///
/// 这一条钉的是"内容在不在"为什么得是**独立于 `read` 的一问**：`Ledger::read` 对缺失的
/// 内容返回空列表（新会话本来就该是空的，`GET /v1/sessions/{id}/events` 依赖它），所以
/// 拿"读回来没报错"顶替的话，内容缺失永远判不出来——这条 Run 会被当成"内容在"，然后
/// 按空上下文继续一轮。
#[tokio::test]
async fn missing_content_stops_the_run_even_while_the_session_says_active() {
    let world = World::new();
    let run = world.accept().await;
    let (scan, index) = world.scan(vec![world.waiting_row(
        &run,
        WaitReason::Retry {
            attempts: 1,
            not_before: NOW + time::Duration::seconds(30),
            cause: RetryCause::Transport,
        },
    )]);
    index.set_session_state(Some(SessionState::Active));
    index.set_session_content(false);

    let report = scan.scan().await.unwrap();
    let RecoveryAction::WaitingForOperator { reason } = &report.outcomes[0].action else {
        panic!("{:?}", report.outcomes[0]);
    };
    assert!(reason.contains("内容"), "{reason}");
    assert_eq!(report.outcomes[0].applied, Applied::NeedsOperator);
    assert_eq!(index.attention().len(), 1);
    assert!(
        index.requeued().is_empty(),
        "内容缺失不许按空上下文继续（§8.9）"
    );
}

/// 表外：用户取消过的 Run，哪怕日志里还留着没跑完的调用，也不复活。
#[tokio::test]
async fn a_cancelled_run_is_not_revived_even_with_work_left_in_the_log() {
    let world = World::new();
    let run = world.accept().await;
    let calls = world.round(&run, &["call-1"]).await;
    let plan = world.plan(&calls[0]);
    world.ledger.plan_call(&calls[0], &plan).await.unwrap();
    world
        .ledger
        .complete(&run, RunEnd::Cancelled { by: None })
        .await
        .unwrap();

    let mut row = world.run_row(&run, RunState::Cancelled);
    row.result_delivered = true;
    let (scan, index) = world.scan(vec![row]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::KeepTerminal {
            state: RunState::Cancelled
        }
    );
    assert!(index.requeued().is_empty());
}

/// 表外：引用存在但正文**缺失** → 停止受影响任务，不重跑来补造旧结果。
#[tokio::test]
async fn a_missing_output_body_stops_the_task_instead_of_rerunning_it() {
    let world = World::new();
    let run = world.accept().await;
    let calls = world.round(&run, &["call-1"]).await;
    let plan = world.plan(&calls[0]);
    world.ledger.plan_call(&calls[0], &plan).await.unwrap();
    let attempt = world
        .ledger
        .start_call(&calls[0], &plan, None)
        .await
        .unwrap();
    world
        .ledger
        .finish_call(
            &attempt,
            published_pointing_at("tool-output/gone/output.json"),
        )
        .await
        .unwrap();

    let (scan, index) = world.scan_with(
        Arc::clone(&world.ledger) as Arc<dyn Ledger>,
        Arc::new(LostOutputs),
        vec![world.run_row(&run, RunState::Running)],
        true,
    );
    let report = scan.scan().await.unwrap();
    assert!(
        matches!(
            report.outcomes[0].action,
            RecoveryAction::HaltCorrupt { .. }
        ),
        "{:?}",
        report.outcomes[0]
    );
    assert_eq!(report.outcomes[0].applied, Applied::NeedsOperator);
    assert!(index.requeued().is_empty(), "**不重跑**");
}

/// 表外：引用存在、正文在，但哈希不符 → 同样停下来。
#[tokio::test]
async fn an_altered_output_body_stops_the_task_too() {
    let world = World::new();
    let run = world.accept().await;
    let calls = world.round(&run, &["call-1"]).await;
    let plan = world.plan(&calls[0]);
    world.ledger.plan_call(&calls[0], &plan).await.unwrap();
    let attempt = world
        .ledger
        .start_call(&calls[0], &plan, None)
        .await
        .unwrap();
    // 真的发布过一份，但事件里记的哈希对不上（正文被改过）。
    let writer = world
        .outputs
        .begin(&AttemptRef {
            session: world.session.clone(),
            run: run.clone(),
            call: calls[0].clone(),
            attempt: attempt.clone(),
        })
        .await
        .unwrap();
    let mut published = world
        .outputs
        .publish(
            writer,
            ToolResultBody {
                status: ToolResultStatus::Completed,
                result: serde_json::json!({}),
                error: None,
                exit_code: Some(0),
                artifacts: vec![],
                preview: None,
            },
        )
        .await
        .unwrap();
    published.output.0.hash = komo_kernel::types::digest::ContentHash::of_str("别的内容");
    world.ledger.finish_call(&attempt, published).await.unwrap();

    let (scan, _index) = world.scan(vec![world.run_row(&run, RunState::Running)]);
    let report = scan.scan().await.unwrap();
    assert!(
        matches!(
            report.outcomes[0].action,
            RecoveryAction::HaltCorrupt { .. }
        ),
        "{:?}",
        report.outcomes[0]
    );
}

/// §8.7：无法确认旧执行已结束时，阻止该任务重复启动并显示原因。
#[tokio::test]
async fn a_surviving_previous_executor_blocks_a_second_start() {
    let world = World::new();
    let run = world.accept().await;
    let calls = world.round(&run, &["call-1"]).await;
    let plan = world.plan(&calls[0]);
    world.ledger.plan_call(&calls[0], &plan).await.unwrap();

    let mut row = world.run_row(&run, RunState::Running);
    row.claimed_by = Some(ExecutorId::from_raw("exec-old"));
    let (scan, index) = world.scan_with(
        Arc::clone(&world.ledger) as Arc<dyn Ledger>,
        Arc::clone(&world.outputs) as Arc<dyn ToolOutputStore>,
        vec![row],
        false,
    );
    let report = scan.scan().await.unwrap();
    assert!(
        matches!(
            report.outcomes[0].action,
            RecoveryAction::WaitingForOperator { .. }
        ),
        "{:?}",
        report.outcomes[0]
    );
    assert!(index.requeued().is_empty());
}

/// 启动扫描先把上一代的 running 交还，再逐个判断（§8.7 的顺序）。
#[tokio::test]
async fn the_scan_reclaims_before_it_decides() {
    let world = World::new();
    let run = world.accept().await;
    let (scan, _index) = world.scan(vec![world.run_row(&run, RunState::Accepted)]);
    let report = scan.scan().await.unwrap();
    assert_eq!(report.reclaimed, 1);
    assert_eq!(report.outcomes.len(), 1);
}

#[tokio::test]
async fn an_empty_database_reports_nothing_to_do() {
    let world = World::new();
    let (scan, _index) = world.scan(vec![]);
    let report = scan.scan().await.unwrap();
    assert!(report.outcomes.is_empty());
    assert_eq!(report.summary(), "没有未完成的任务");
}

fn approval_record(
    approval: &ApprovalId,
    session: &SessionId,
    run: &RunId,
    decision: Option<ApprovalDecisionRecord>,
) -> ApprovalRecord {
    let plan = komo_kernel::test_support::sample_plan("shell", session);
    ApprovalRecord {
        approval: approval.clone(),
        short_id: ShortId::from_index(1),
        session: session.clone(),
        run: Some(run.clone()),
        call: None,
        plan_hash: plan.plan_hash(),
        plan,
        reason: "危险操作".into(),
        changes: None,
        evidence: None,
        scopes: vec![ApprovalScope::Once],
        requested_at: NOW,
        valid_until: None,
        decision,
    }
}

fn published_pointing_at(path: &str) -> komo_kernel::types::refs::PublishedOutput {
    komo_kernel::types::refs::PublishedOutput {
        output: OutputRef(ContentRef {
            path: path.into(),
            size: 2,
            hash: komo_kernel::types::digest::ContentHash::of_str("{}"),
            pointer: None,
        }),
        status: ToolResultStatus::Completed,
        elapsed_ms: 1,
        preview: None,
        stdout: None,
        stderr: None,
    }
}

// ---------------------------------------------------------------- 一个坏会话只停它自己

/// §8.4「JSONL 已提交范围缺失或中间损坏 → 停止受影响会话，报告损坏」——**受影响的是
/// 它自己**。一个读不出来的会话不能让别的 Run 少判一个。
#[tokio::test]
async fn a_corrupt_session_stops_itself_and_the_others_are_still_judged() {
    let world = World::new();
    let broken_session = SessionId::from_raw("01a0a414-7800-7bbd-8fa1-000000000001");
    let healthy_session = SessionId::from_raw("01a0a414-7800-7bbd-8fa1-000000000002");

    let broken = world.accept_in(&broken_session, "api:broken").await;
    let healthy = world.accept_in(&healthy_session, "api:healthy").await;

    let ledger = PoisonedLedger::corrupting(Arc::clone(&world.ledger), broken_session.clone());
    let (scan, index) = world.scan_full(
        ledger,
        Arc::clone(&world.outputs) as Arc<dyn ToolOutputStore>,
        Arc::new(NoOrphanLookup),
        vec![
            world.run_row_in(&broken, &broken_session, RunState::Running),
            world.run_row_in(&healthy, &healthy_session, RunState::Accepted),
        ],
        true,
    );

    let report = scan.scan().await.unwrap();
    assert_eq!(report.outcomes.len(), 2, "两个都判了：{report:?}");

    // 坏的：停下来，原因看得见。
    let stopped = &report.outcomes[0];
    assert_eq!(stopped.run, broken);
    assert!(
        matches!(stopped.action, RecoveryAction::HaltCorrupt { .. }),
        "{stopped:?}"
    );
    assert_eq!(stopped.applied, Applied::NeedsOperator);
    let (run, reason) = index.attention()[0].clone();
    assert_eq!(run, broken);
    assert!(reason.contains("读不出来"), "原因要说得出是什么：{reason}");
    assert_eq!(report.corrupt().len(), 1);
    assert!(
        report.summary().contains("因损坏已停止"),
        "{}",
        report.summary()
    );

    // 好的：照常判、照常入队。
    let ok = &report.outcomes[1];
    assert_eq!(ok.run, healthy);
    assert_eq!(ok.action, RecoveryAction::BackfillIndexAndQueue);
    assert_eq!(index.requeued(), vec![healthy], "其他 Session 正常运行");
}

/// 坏会话排在最后也一样——它不能把已经判完的那些带走，也不能让扫描本身失败。
#[tokio::test]
async fn a_corrupt_session_never_fails_the_whole_scan() {
    let world = World::new();
    let broken_session = SessionId::from_raw("01a0a414-7800-7bbd-8fa1-000000000003");
    let healthy = world.accept().await;
    let broken = world.accept_in(&broken_session, "api:broken").await;

    let ledger = PoisonedLedger::corrupting(Arc::clone(&world.ledger), broken_session.clone());
    let (scan, index) = world.scan_full(
        ledger,
        Arc::clone(&world.outputs) as Arc<dyn ToolOutputStore>,
        Arc::new(NoOrphanLookup),
        vec![
            world.run_row(&healthy, RunState::Accepted),
            world.run_row_in(&broken, &broken_session, RunState::Running),
        ],
        true,
    );

    let report = scan.scan().await.expect("一个坏会话不该让整轮扫描失败");
    assert_eq!(report.requeued(), 1);
    assert_eq!(report.corrupt().len(), 1);
    assert_eq!(index.requeued(), vec![healthy]);
}

// ---------------------------------------------------------------- 孤儿输出

/// §14 故障注入表：「output.json 已完成但 JSONL 结果事件尚未写入 → 校验身份、计划及
/// 完成状态后补记结果；不能只凭文件存在判断」。
#[tokio::test]
async fn an_orphan_output_is_verified_and_its_result_is_backfilled() {
    let world = World::new();
    let run = world.accept().await;
    let calls = world.round(&run, &["call-1"]).await;
    let plan = world.plan(&calls[0]);
    world.ledger.plan_call(&calls[0], &plan).await.unwrap();
    let attempt = world
        .ledger
        .start_call(&calls[0], &plan, None)
        .await
        .unwrap();
    // 工具跑完了，output.json 完整落盘——**但 tool.result 没写就崩了**。
    let published = world.publish(&run, &calls[0], &attempt).await;
    assert!(tool_results(&world).is_empty(), "结果事件尚未写入");

    let orphans = FoundOrphans::with(
        AttemptRef {
            session: world.session.clone(),
            run: run.clone(),
            call: calls[0].clone(),
            attempt: attempt.clone(),
        },
        published.clone(),
    );
    let (scan, index) = world.scan_full(
        Arc::clone(&world.ledger) as Arc<dyn Ledger>,
        Arc::clone(&world.outputs) as Arc<dyn ToolOutputStore>,
        orphans,
        vec![world.run_row(&run, RunState::Running)],
        true,
    );

    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::VerifyEffect {
            call: calls[0].clone()
        },
        "核对，而不是当它没发生过"
    );
    assert_eq!(report.outcomes[0].applied, Applied::Requeued);

    // 账本里补上了**那次尝试**的结果，用的是**原来那份输出**。
    let results = tool_results(&world);
    assert_eq!(results.len(), 1, "补记了一条结果");
    assert_eq!(results[0].attempt_id, attempt, "补记的是原来那次尝试");
    assert_eq!(results[0].call_id, calls[0]);
    assert_eq!(
        results[0].output_ref.0.path, published.output.0.path,
        "复用已经完整落盘的那份输出"
    );
    // 工具没重跑：还是只有一条 tool.started。
    assert_eq!(tool_started_count(&world), 1, "不重跑");
    assert_eq!(index.requeued(), vec![run]);
}

/// 「不能只凭文件存在判断」：身份对不上的那份输出是损坏，不是证据。
#[tokio::test]
async fn an_output_from_another_attempt_is_corruption_not_evidence() {
    let world = World::new();
    let run = world.accept().await;
    let calls = world.round(&run, &["call-1"]).await;
    let plan = world.plan(&calls[0]);
    world.ledger.plan_call(&calls[0], &plan).await.unwrap();
    let attempt = world
        .ledger
        .start_call(&calls[0], &plan, None)
        .await
        .unwrap();

    // 查到的那份 output.json 属于**另一次尝试**。
    let stranger = komo_kernel::types::ids::AttemptId::from_raw("attempt-from-another-life");
    let published = world.publish(&run, &calls[0], &stranger).await;
    let orphans = FoundOrphans::with(
        AttemptRef {
            session: world.session.clone(),
            run: run.clone(),
            call: calls[0].clone(),
            attempt: attempt.clone(),
        },
        published,
    );

    let (scan, index) = world.scan_full(
        Arc::clone(&world.ledger) as Arc<dyn Ledger>,
        Arc::clone(&world.outputs) as Arc<dyn ToolOutputStore>,
        orphans,
        vec![world.run_row(&run, RunState::Running)],
        true,
    );

    let report = scan.scan().await.unwrap();
    assert!(
        matches!(
            report.outcomes[0].action,
            RecoveryAction::HaltCorrupt { .. }
        ),
        "{:?}",
        report.outcomes[0]
    );
    assert!(tool_results(&world).is_empty(), "不补记来路不明的结果");
    assert!(index.requeued().is_empty());
}

/// 没有孤儿输出时照旧：交给 AgentLoop 去调工具自己的 `verify`（§8.4 第 7 行）。
#[tokio::test]
async fn without_an_orphan_output_the_call_still_goes_to_the_tools_own_verify() {
    let world = World::new();
    let run = world.accept().await;
    let calls = world.round(&run, &["call-1"]).await;
    let plan = world.plan(&calls[0]);
    world.ledger.plan_call(&calls[0], &plan).await.unwrap();
    world
        .ledger
        .start_call(&calls[0], &plan, None)
        .await
        .unwrap();

    let (scan, index) = world.scan(vec![world.run_row(&run, RunState::Running)]);
    let report = scan.scan().await.unwrap();

    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::VerifyEffect {
            call: calls[0].clone()
        }
    );
    assert!(tool_results(&world).is_empty(), "没有证据就不补记");
    assert_eq!(index.requeued(), vec![run]);
}

fn tool_results(world: &World) -> Vec<komo_kernel::events::ToolResult> {
    world
        .ledger
        .events()
        .into_iter()
        .filter_map(|event| match event.payload {
            komo_kernel::events::EventPayload::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}

fn tool_started_count(world: &World) -> usize {
    world
        .ledger
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                komo_kernel::events::EventPayload::ToolStarted(_)
            )
        })
        .count()
}

/// 找到了完整的输出、却补记不进账本（冷进程里 attempt → session 的路由丢了）：**退回
/// 工具核对**，而且这一轮扫描照常走完——别的 Run 还等着判。
#[tokio::test]
async fn a_backfill_that_cannot_be_written_falls_back_to_the_tools_verify() {
    let world = World::new();
    let run = world.accept().await;
    let calls = world.round(&run, &["call-1"]).await;
    let plan = world.plan(&calls[0]);
    world.ledger.plan_call(&calls[0], &plan).await.unwrap();
    let attempt = world
        .ledger
        .start_call(&calls[0], &plan, None)
        .await
        .unwrap();
    let published = world.publish(&run, &calls[0], &attempt).await;

    let orphans = FoundOrphans::with(
        AttemptRef {
            session: world.session.clone(),
            run: run.clone(),
            call: calls[0].clone(),
            attempt: attempt.clone(),
        },
        published,
    );
    let (scan, index) = world.scan_full(
        PoisonedLedger::unroutable(Arc::clone(&world.ledger)),
        Arc::clone(&world.outputs) as Arc<dyn ToolOutputStore>,
        orphans,
        vec![world.run_row(&run, RunState::Running)],
        true,
    );

    let report = scan.scan().await.expect("补记失败不该让整轮扫描失败");
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::VerifyEffect {
            call: calls[0].clone()
        }
    );
    assert_eq!(report.outcomes[0].applied, Applied::Requeued, "照常接着跑");
    assert!(tool_results(&world).is_empty(), "没补上就是没补上，不假装");
    assert_eq!(index.requeued(), vec![run]);
}
