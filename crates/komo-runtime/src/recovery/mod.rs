//! Recovery：启动扫描与调用核对，决策表在 kernel（§8.4、§8.6、§8.7）。
//!
//! 「Gateway 自动接续能够确定恢复位置的任务。用户只处理审批、时效或结果不明等确实需要
//! 判断的情况。」这个模块做的是那句话里"确定恢复位置"的那一半：**采集三样观察，交给
//! kernel 的纯函数决定，然后执行那个决定**。
//!
//! 一条贯穿全表的规则写在这里的每一个分支上：**回放只补索引与派生执行状态**——不调用
//! 工具、不发送外部请求、不消费授权，也不覆盖数据库里已经记下的取消或权限撤销
//! （§8.5）。"继续执行"在这里一律等于"把 Run 放回队列"，真正接着跑是 AgentLoop 领走
//! 它以后的事，而它读的是同一份日志。

mod observe;
mod processes;

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::events::Event;
use komo_kernel::recovery::{
    ApprovalObservation, LogTail, OutputCheck, PendingCall, RecoveryAction, RecoveryInput,
    RetryObservation, decide,
};
use komo_kernel::traits::{
    ApprovalRepo, Clock, Ledger, LedgerError, RepoError, StoreError, ToolOutputStore,
};
use komo_kernel::types::ids::{ExecutorId, RunId, Seq, SessionId};
use komo_kernel::types::status::RunStatus;
use time::OffsetDateTime;

pub use observe::{events_of, log_tail, output_ref_of, waiting_approval};
pub use processes::{
    ChildProcess, ChildRegistry, ExecutorLiveness, Liveness, LockHolderLiveness, ProcessProbe,
    SysProcessProbe,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecoveryError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Repo(#[from] RepoError),
}

/// state.db 里那些**未终态 Run** 的样子，以及恢复要在索引上做的几件事。
///
/// 它不在 kernel 的 trait 表里，因为 kernel 那张表只列了"账本、队列、审批、Cron……"这
/// 几个跨层接缝；"未完成 Run 的索引"是 store 的派生状态，恢复扫描是它唯一的读者。
/// **生产实现在 store**（§8.7 的四条 SQL 就在那里），这里只要一个能替身的形状。
#[async_trait]
pub trait RecoveryIndex: Send + Sync {
    /// 所有未终态 Run（§8.4 要逐个判断的就是它们）。
    async fn unfinished_runs(&self) -> Result<Vec<UnfinishedRun>, StoreError>;

    /// 「启动回收：旧实例的 running -> interrupted，并交还领取权」（§8.7）。返回影响行数。
    async fn reclaim_running(&self, executor: &ExecutorId) -> Result<u64, StoreError>;

    /// 用 JSONL 已有的事件补齐 state.db 的索引与派生执行状态。**不重放动作。**
    async fn backfill(&self, run: &RunId) -> Result<(), StoreError>;

    /// 放回队列，等调度器领。
    async fn requeue(&self, run: &RunId) -> Result<(), StoreError>;

    /// 需要操作者判断：结果不明、引用损坏、授权失效。
    async fn mark_needs_attention(&self, run: &RunId, reason: &str) -> Result<(), StoreError>;
}

/// 一个未终态 Run 在 state.db 里的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfinishedRun {
    pub run: RunId,
    pub session: SessionId,
    pub status: RunStatus,
    /// 还握着领取权的执行实例。
    pub claimed_by: Option<ExecutorId>,
    /// 等待重试时已经用掉的次数与下次时间。**重启不重置预算**（§8.5）。
    pub retry: Option<RetryObservation>,
    /// 最终结果已经送达客户端了吗。
    pub result_delivered: bool,
}

/// 一个 Run 的恢复结论，以及照它做了什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryOutcome {
    pub run: RunId,
    pub session: SessionId,
    pub action: RecoveryAction,
    pub applied: Applied,
}

/// 照决定做了什么。**每一项都只碰索引和队列**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// 补齐索引后放回队列，由 AgentLoop 接着跑。
    Requeued,
    /// 保持原状：还在等审批 / 等退避 / 已经是终态 / 等同一请求键重传。
    LeftAsIs,
    /// 标成 needs_attention，等人。
    NeedsOperator,
    /// 结果是好的，只是没送到——调用方补发（§8.4 第 10 行）。
    Redeliver,
}

/// 一次启动扫描的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// 上一代遗留的 running 被交还了几条。
    pub reclaimed: u64,
    pub outcomes: Vec<RecoveryOutcome>,
}

impl RecoveryReport {
    pub fn requeued(&self) -> usize {
        self.count(Applied::Requeued)
    }

    pub fn waiting(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o.action,
                    RecoveryAction::KeepWaitingApproval { .. }
                        | RecoveryAction::WaitUntilRetry { .. }
                )
            })
            .count()
    }

    pub fn needs_operator(&self) -> usize {
        self.count(Applied::NeedsOperator)
    }

    pub fn to_redeliver(&self) -> Vec<&RecoveryOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.applied == Applied::Redeliver)
            .collect()
    }

    fn count(&self, applied: Applied) -> usize {
        self.outcomes
            .iter()
            .filter(|o| o.applied == applied)
            .count()
    }

    /// 「打开 komo 时可以看到"2 个任务已接续，1 个等待审批"」（§8.8）。
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.requeued() > 0 {
            parts.push(format!("{} 个任务已接续", self.requeued()));
        }
        if self.waiting() > 0 {
            parts.push(format!("{} 个等待审批或重试", self.waiting()));
        }
        if self.needs_operator() > 0 {
            parts.push(format!("{} 个需要你处理", self.needs_operator()));
        }
        if parts.is_empty() {
            "没有未完成的任务".to_string()
        } else {
            parts.join("，")
        }
    }
}

/// 启动扫描。
pub struct RecoveryScan {
    ledger: Arc<dyn Ledger>,
    index: Arc<dyn RecoveryIndex>,
    outputs: Arc<dyn ToolOutputStore>,
    approvals: Arc<dyn ApprovalRepo>,
    clock: Arc<dyn Clock>,
    executor: ExecutorId,
    liveness: Arc<dyn ExecutorLiveness>,
}

impl std::fmt::Debug for RecoveryScan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryScan")
            .field("executor", &self.executor)
            .finish()
    }
}

impl RecoveryScan {
    pub fn new(
        ledger: Arc<dyn Ledger>,
        index: Arc<dyn RecoveryIndex>,
        outputs: Arc<dyn ToolOutputStore>,
        approvals: Arc<dyn ApprovalRepo>,
        clock: Arc<dyn Clock>,
        executor: ExecutorId,
        liveness: Arc<dyn ExecutorLiveness>,
    ) -> Self {
        RecoveryScan {
            ledger,
            index,
            outputs,
            approvals,
            clock,
            executor,
            liveness,
        }
    }

    /// 跑一遍（§8.7 的启动顺序里"将旧执行实例的 running 标为 interrupted"之后那几步）。
    pub async fn scan(&self) -> Result<RecoveryReport, RecoveryError> {
        let reclaimed = self.index.reclaim_running(&self.executor).await?;
        if reclaimed > 0 {
            tracing::info!(reclaimed, "上一代遗留的 running 已交还领取权");
        }

        let mut report = RecoveryReport {
            reclaimed,
            ..Default::default()
        };
        for run in self.index.unfinished_runs().await? {
            let observed = self.observe(&run).await?;
            let action = decide(&observed);
            let applied = self.apply(&run, &action).await?;
            tracing::info!(
                run = %run.run,
                session = %run.session,
                status = ?run.status,
                action = ?action,
                ?applied,
                "恢复决定"
            );
            report.outcomes.push(RecoveryOutcome {
                run: run.run.clone(),
                session: run.session.clone(),
                action,
                applied,
            });
        }
        Ok(report)
    }

    /// 单个 Run 的三样观察。公开出来，因为 `komo resume` 要的是同一份判断。
    pub async fn observe(&self, run: &UnfinishedRun) -> Result<RecoveryInput, RecoveryError> {
        let events = self.read_session(&run.session).await?;
        let log_tail = log_tail(&events, &run.run);
        let output_check = self.check_output(&events, run, &log_tail).await;
        let approval = self.observe_approval(&events, run).await?;

        Ok(RecoveryInput {
            db_status: run.status,
            log_tail,
            output_check,
            approval,
            retry: run.retry.clone(),
            result_delivered: run.result_delivered,
            // 「无法确认旧执行已结束时，阻止该任务重复启动并显示原因」（§8.7）。
            previous_executor_stopped: self.liveness.stopped(run.claimed_by.as_ref()),
        })
    }

    async fn read_session(&self, session: &SessionId) -> Result<Vec<Event>, RecoveryError> {
        // 短事务分页（§13.5 `Ledger::read` 的注释：`limit` 不是可选的）。
        let mut all = Vec::new();
        let mut from = Seq(0);
        loop {
            let batch = self.ledger.read(session, from, 0).await?;
            all.extend(batch.events);
            match batch.next {
                Some(next) => from = next,
                None => break,
            }
        }
        Ok(all)
    }

    /// 「若结果引用存在但对应输出缺失或哈希不符，停止受影响任务」（§8.5）。
    async fn check_output(
        &self,
        events: &[Event],
        run: &UnfinishedRun,
        log_tail: &LogTail,
    ) -> OutputCheck {
        let LogTail::RoundPersisted {
            pending: PendingCall::ResultPersisted { call },
        } = log_tail
        else {
            return OutputCheck::NotApplicable;
        };
        let Some(reference) = output_ref_of(events, &run.run, call) else {
            return OutputCheck::NotApplicable;
        };
        match self.outputs.open(&reference).await {
            Ok(_) => OutputCheck::Verified,
            Err(StoreError::NotFound { .. }) => OutputCheck::Missing,
            Err(StoreError::Corrupt(_)) => OutputCheck::HashMismatch,
            // 读不出来 ≠ 确认没问题。停下来报告，胜过重跑一遍去"补造"旧结果。
            Err(error) => {
                tracing::warn!(run = %run.run, %error, "输出引用核对不了");
                OutputCheck::Missing
            }
        }
    }

    /// 审批的观察。**权威是 state.db**——JSONL 里的审计副本不能自行创建授权（§7.4）。
    async fn observe_approval(
        &self,
        events: &[Event],
        run: &UnfinishedRun,
    ) -> Result<ApprovalObservation, RecoveryError> {
        if run.status != RunStatus::WaitingApproval {
            return Ok(ApprovalObservation::None);
        }
        let Some(id) = waiting_approval(events, &run.run) else {
            return Ok(ApprovalObservation::None);
        };
        let Some(record) = self.approvals.get(&id).await? else {
            return Ok(ApprovalObservation::MissingFromDatabase);
        };
        Ok(match &record.decision {
            None => ApprovalObservation::Pending { approval: id },
            Some(decision) => ApprovalObservation::Decided {
                approval: id,
                approved: decision.approved,
                still_valid: still_valid(&record, decision, self.clock.now()),
            },
        })
    }

    /// 执行一个决定。**这里没有任何一条路径会调用工具、发请求或消费授权。**
    async fn apply(
        &self,
        run: &UnfinishedRun,
        action: &RecoveryAction,
    ) -> Result<Applied, RecoveryError> {
        match action {
            // 能确定恢复位置的：补索引 → 放回队列。接着跑是 AgentLoop 的事，它读的是
            // 同一份日志，所以"从哪儿接着跑"不需要在这里再说一遍。
            RecoveryAction::BackfillIndexAndQueue
            | RecoveryAction::DiscardPartialAndRerequestModel
            | RecoveryAction::ResumeFromPlan
            | RecoveryAction::ExecutePlannedCall { .. }
            | RecoveryAction::VerifyEffect { .. }
            | RecoveryAction::BackfillResultAndContinue { .. }
            | RecoveryAction::ResumeWithDecision { .. } => {
                self.index.backfill(&run.run).await?;
                self.index.requeue(&run.run).await?;
                Ok(Applied::Requeued)
            }
            // 等人、等钟、等重传、已经是终态：原样留着。
            RecoveryAction::AwaitResend
            | RecoveryAction::KeepWaitingApproval { .. }
            | RecoveryAction::WaitUntilRetry { .. }
            | RecoveryAction::KeepTerminal { .. } => Ok(Applied::LeftAsIs),
            // 结果是好的，只是没送到。**不重新执行任务。**
            RecoveryAction::RedeliverResult => {
                self.index.backfill(&run.run).await?;
                Ok(Applied::Redeliver)
            }
            RecoveryAction::NeedsAttention { reason } => {
                self.index.mark_needs_attention(&run.run, reason).await?;
                Ok(Applied::NeedsOperator)
            }
            // 「停止受影响任务并报告损坏。不重跑来掩盖数据损坏。」
            RecoveryAction::HaltCorrupt { reason } => {
                tracing::error!(run = %run.run, reason = %reason, "引用损坏，停止这个任务");
                self.index.mark_needs_attention(&run.run, reason).await?;
                Ok(Applied::NeedsOperator)
            }
        }
    }
}

/// 这条决定现在还成立吗（§8.4 第 8 行的 `still_valid`）。
///
/// 这里判得了的只有两条：**没过期**，以及**只批一次的授权还没被用掉**。范围、计划哈希
/// 与 Job 版本的核对在消费那一刻由 [`ApprovalRepo::consume`] 做——那是权威，这里不重
/// 复一份可能和它不一致的判断。
fn still_valid(
    record: &komo_kernel::protocol::http::ApprovalRecord,
    decision: &komo_kernel::protocol::http::ApprovalDecisionRecord,
    now: OffsetDateTime,
) -> bool {
    use komo_kernel::types::chat::ApprovalScope;

    if record.valid_until.is_some_and(|until| until <= now) {
        return false;
    }
    if decision.scope == ApprovalScope::Once && decision.consumed {
        return false;
    }
    true
}

#[cfg(test)]
mod tests;
