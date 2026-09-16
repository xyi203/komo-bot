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
use komo_kernel::types::status::{RunEnd, Wait};
use komo_kernel::types::turn::{AcceptInput, AssistantRound, ToolCallRequest};
use time::macros::datetime;

use super::*;

const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

// ---------------------------------------------------------------- 替身

/// 记账用的索引：恢复做了什么，事后数得出来。
#[derive(Debug, Default)]
struct MemIndex {
    runs: Mutex<Vec<UnfinishedRun>>,
    state: Mutex<IndexState>,
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
        })
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
}

#[derive(Debug)]
struct FakeLiveness(bool);

impl ExecutorLiveness for FakeLiveness {
    fn stopped(&self, _previous: Option<&ExecutorId>) -> bool {
        self.0
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
        self.ledger
            .accept_input(AcceptInput {
                session: self.session.clone(),
                request_key: RequestKey::new("api:1"),
                text: "帮我看看".into(),
                source: PlanSource::Interactive {
                    session: self.session.clone(),
                },
                peer: None,
                model: komo_kernel::test_support::sample_model(),
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

    fn run_row(&self, run: &RunId, status: RunStatus) -> UnfinishedRun {
        UnfinishedRun {
            run: run.clone(),
            session: self.session.clone(),
            status,
            claimed_by: None,
            retry: None,
            result_delivered: false,
        }
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
        let index = MemIndex::with(rows);
        let scan = RecoveryScan::new(
            ledger,
            Arc::clone(&index) as Arc<dyn RecoveryIndex>,
            outputs,
            Arc::clone(&self.approvals) as Arc<dyn ApprovalRepo>,
            Arc::new(self.clock.clone()),
            ExecutorId::from_raw("exec-now"),
            Arc::new(FakeLiveness(previous_stopped)),
        );
        (scan, index)
    }
}

// ---------------------------------------------------------------- 逐行

/// 第 1 行：输入已在 JSONL 持久保存，Run 尚未开始 → 补齐索引后自动入队。
#[tokio::test]
async fn row_1_input_persisted_but_the_run_never_started() {
    let world = World::new();
    let run = world.accept().await;
    let (scan, index) = world.scan(vec![world.run_row(&run, RunStatus::Ingesting)]);

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
    let (scan, index) = world.scan(vec![world.run_row(&run, RunStatus::Ingesting)]);

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

    let (scan, index) = world.scan(vec![world.run_row(&run, RunStatus::Running)]);
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

    let (scan, index) = world.scan(vec![world.run_row(&run, RunStatus::Interrupted)]);
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
            },
        )
        .await
        .unwrap();
    world.ledger.finish_call(&attempt, published).await.unwrap();

    let (scan, index) = world.scan(vec![world.run_row(&run, RunStatus::Running)]);
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

    let (scan, index) = world.scan(vec![world.run_row(&run, RunStatus::Interrupted)]);
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

    let (scan, _index) = world.scan(vec![world.run_row(&run, RunStatus::Interrupted)]);
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
            Wait::Approval {
                approval: approval.clone(),
                call: None,
                attempt: None,
            },
        )
        .await
        .unwrap();
    world
        .approvals
        .create(approval_record(&approval, &world.session, &run, None))
        .await
        .unwrap();

    let (scan, index) = world.scan(vec![world.run_row(&run, RunStatus::WaitingApproval)]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::KeepWaitingApproval {
            approval: approval.clone()
        }
    );
    assert!(index.requeued().is_empty());
    assert_eq!(report.summary(), "1 个等待审批或重试");

    // (b) 答了，而且还有效。
    let world = World::new();
    let run = world.accept().await;
    world
        .ledger
        .suspend(
            &run,
            Wait::Approval {
                approval: approval.clone(),
                call: None,
                attempt: None,
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

    let (scan, index) = world.scan(vec![world.run_row(&run, RunStatus::WaitingApproval)]);
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
            Wait::Approval {
                approval: approval.clone(),
                call: None,
                attempt: None,
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

    let (scan, index) = world.scan(vec![world.run_row(&run, RunStatus::WaitingApproval)]);
    let report = scan.scan().await.unwrap();
    assert!(
        matches!(
            report.outcomes[0].action,
            RecoveryAction::NeedsAttention { .. }
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
            Wait::Approval {
                approval: ApprovalId::from_raw("ap-gone"),
                call: None,
                attempt: None,
            },
        )
        .await
        .unwrap();

    let (scan, index) = world.scan(vec![world.run_row(&run, RunStatus::WaitingApproval)]);
    let report = scan.scan().await.unwrap();
    let RecoveryAction::NeedsAttention { reason } = &report.outcomes[0].action else {
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
            Wait::Retry {
                attempts: 2,
                next_retry_at: at,
                reason: "连接被拒".into(),
            },
        )
        .await
        .unwrap();

    let mut row = world.run_row(&run, RunStatus::WaitingRetry);
    row.retry = Some(RetryObservation {
        attempts: 2,
        next_retry_at: at,
        exhausted: false,
    });
    let (scan, index) = world.scan(vec![row]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::WaitUntilRetry { at, attempts: 2 }
    );
    assert!(index.requeued().is_empty());
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

    let mut row = world.run_row(&run, RunStatus::Completed);
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
    for (status, end) in [
        (
            RunStatus::Completed,
            RunEnd::Completed {
                final_message: None,
                rounds: 1,
            },
        ),
        (
            RunStatus::Failed,
            RunEnd::Failed {
                reason: "模型不可用".into(),
            },
        ),
        (RunStatus::Cancelled, RunEnd::Cancelled { by: None }),
    ] {
        let world = World::new();
        let run = world.accept().await;
        world.ledger.complete(&run, end).await.unwrap();

        let mut row = world.run_row(&run, status);
        row.result_delivered = true;
        let (scan, index) = world.scan(vec![row]);
        let report = scan.scan().await.unwrap();

        assert_eq!(
            report.outcomes[0].action,
            RecoveryAction::KeepTerminal { status },
            "{status:?}"
        );
        assert!(index.requeued().is_empty(), "{status:?}");
    }
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

    let mut row = world.run_row(&run, RunStatus::Cancelled);
    row.result_delivered = true;
    let (scan, index) = world.scan(vec![row]);
    let report = scan.scan().await.unwrap();
    assert_eq!(
        report.outcomes[0].action,
        RecoveryAction::KeepTerminal {
            status: RunStatus::Cancelled
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
        vec![world.run_row(&run, RunStatus::Running)],
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
            },
        )
        .await
        .unwrap();
    published.output.0.hash = komo_kernel::types::digest::ContentHash::of_str("别的内容");
    world.ledger.finish_call(&attempt, published).await.unwrap();

    let (scan, _index) = world.scan(vec![world.run_row(&run, RunStatus::Running)]);
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

    let mut row = world.run_row(&run, RunStatus::Interrupted);
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
            RecoveryAction::NeedsAttention { .. }
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
    let (scan, _index) = world.scan(vec![world.run_row(&run, RunStatus::Ingesting)]);
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
