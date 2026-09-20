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
mod orphan;
mod processes;

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::events::Event;
use komo_kernel::recovery::{
    ApprovalObservation, LogTail, OutputCheck, PendingCall, RecoveryAction, RecoveryInput,
    SessionObservation, decide,
};
use komo_kernel::traits::{
    ApprovalRepo, Clock, Ledger, LedgerError, RepoError, StoreError, ToolOutputStore,
};
use komo_kernel::types::ids::{AttemptId, ExecutorId, RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::refs::{AttemptRef, PublishedOutput};
use komo_kernel::types::status::{RunState, SessionState, WaitReason};
use time::OffsetDateTime;

pub use observe::{
    StartedCall, events_of, log_tail, output_ref_of, started_call, waiting_on_approval,
};
pub use orphan::SessionDirOrphanOutputs;
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

    /// 「启动回收：旧实例遗留的 running，**只交还领取权、不判状态**」（§8.7）。返回
    /// 影响行数。交还之后那一行是 `running` 且没有主人——一个可查询的孤儿，由 §8.4
    /// 判成 `queued` 或 `waiting + intervention`。
    async fn reclaim_running(&self, executor: &ExecutorId) -> Result<u64, StoreError>;

    /// 用 JSONL 已有的事件补齐 state.db 的索引与派生执行状态。**不重放动作。**
    async fn backfill(&self, run: &RunId) -> Result<(), StoreError>;

    /// 放回队列，等调度器领。
    async fn requeue(&self, run: &RunId) -> Result<(), StoreError>;

    /// 需要操作者判断：停成 `waiting + intervention`（§8.4、§9.7 的清单）。理由是给
    /// 操作者看的那一句（`runs.last_error`），因为 `WaitReason::Intervention` 只有句柄。
    async fn mark_needs_attention(&self, run: &RunId, reason: &str) -> Result<(), StoreError>;

    /// `sessions.state`（§8.10）。**§8.9 的第一个问题就是它**：「这条未完成的 Run，
    /// 它所属的会话还在服务范围里吗」——一条 Run 自己的状态说得再清楚，也答不出这件事，
    /// 而按空上下文继续一轮比停下来更糟。
    ///
    /// 行不在 = `None`：那是一处"数据库与内容对不上"的损坏（会话墓碑都没了），调用方
    /// 把它当**不服务**处置（停成 `waiting + intervention`），而不是让整轮扫描失败。
    async fn session_state(&self, session: &SessionId) -> Result<Option<SessionState>, StoreError>;

    /// 内容在不在（§8.9 的第二个问题）：目录在、且 `events.jsonl` 打得开。
    ///
    /// **它必须是独立于 `read` 的一问**，不能拿"读回来没报错"顶替：`Ledger::read` 对
    /// "还没有日志的新会话"返回空列表（`GET /v1/sessions/{id}/events` 依赖那个行为），
    /// 于是内容缺失会永远判不出来——一条被搬走的会话会被当成"内容在"，然后**按空上下文
    /// 继续一轮**，而 §8.9 说那比停下来更糟。
    ///
    /// 只观察、不创建（§8.9：观察不改写）；权威实现是 store 的
    /// `repos::reconcile::content_available`。
    async fn session_content(&self, session: &SessionId) -> Result<bool, StoreError>;
}

/// 按一次尝试的身份找**孤儿 `output.json`**：工具跑完、`output.json` 已经完整落盘，
/// 而 `tool.result` 还没写就崩了的那种（§14 故障注入表「output.json 已完成但 JSONL
/// 结果事件尚未写入 → 校验身份、计划及完成状态后补记结果；**不能只凭文件存在判断**」）。
///
/// 它没有并进 [`ToolOutputStore`]，因为那个 trait 在 kernel，而这一条能力目前只有恢复
/// 扫描要用；接上之后这里换成直接调 `ToolOutputStore` 即可（见交付报告的请求）。
/// **没有接的部署行为与今天一致**：[`NoOrphanLookup`] 一律答"找不到"，于是
/// `started` 而无结果仍旧走 §8.4 第 7 行的工具核对。
#[async_trait]
pub trait OrphanOutputs: Send + Sync {
    /// 没有 → `Ok(None)`；有但读不出来或身份对不上 → [`StoreError::Corrupt`]。
    async fn find(&self, attempt: &AttemptRef) -> Result<Option<PublishedOutput>, StoreError>;
}

/// 没有这个能力时的明确降级：一律答"找不到"。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOrphanLookup;

#[async_trait]
impl OrphanOutputs for NoOrphanLookup {
    async fn find(&self, _attempt: &AttemptRef) -> Result<Option<PublishedOutput>, StoreError> {
        Ok(None)
    }
}

/// 一次找到的孤儿输出。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Orphan {
    call: ToolCallId,
    attempt: AttemptId,
    published: PublishedOutput,
}

/// 一次观察，以及它顺手捞到的那份孤儿输出。
#[derive(Debug, Clone)]
struct Observed {
    input: RecoveryInput,
    orphan: Option<Orphan>,
}

/// 一个未终态 Run 在 state.db 里的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfinishedRun {
    pub run: RunId,
    pub session: SessionId,
    /// 能不能跑（§8.4）。
    pub state: RunState,
    /// 为什么不能跑（`state == Waiting` 时有值）。**它与状态是两个维度**：少了它，
    /// "排队二十分钟"就只是一句状态，答不出在等谁、等到什么时候。
    pub wait: Option<WaitReason>,
    /// 还握着领取权的执行实例。
    pub claimed_by: Option<ExecutorId>,
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
    /// 保持原状：还在等审批 / 等干预 / 等退避 / 等前一条 Run / 已经是终态 / 等同一请求键
    /// 重传。**放它出来不是恢复扫描的事**——等退避的看时钟、等依赖的看前一条的终态
    /// （§8.9）。
    LeftAsIs,
    /// 停成 `waiting + intervention`，等人来答（§7.5 的清单）。
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

/// 同一个损坏会话可能挂着多个未完成 Run。状态仍逐 Run 落库，通知则按这一组发送一次。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorruptGroup {
    pub session: SessionId,
    pub reason: String,
    pub runs: Vec<RunId>,
}

impl RecoveryReport {
    pub fn requeued(&self) -> usize {
        self.count(Applied::Requeued)
    }

    /// 还停着等人或等钟的那些——**它们每一个都说得出在等什么**（§8.4 的验收口径）。
    pub fn waiting(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o.action,
                    RecoveryAction::KeepWaiting { .. } | RecoveryAction::WaitUntilRetry { .. }
                )
            })
            .count()
    }

    pub fn needs_operator(&self) -> usize {
        self.count(Applied::NeedsOperator)
    }

    /// 因为损坏被停下来的那些 Run，以及原因。
    ///
    /// 「停止受影响会话，报告损坏」（§8.4）——**报告**这一半就是它：一个读不出来的会话
    /// 不会让别的 Run 少判一个，但它自己得在这份报告里看得见。
    pub fn corrupt(&self) -> Vec<(&RunId, &str)> {
        self.outcomes
            .iter()
            .filter_map(|outcome| match &outcome.action {
                RecoveryAction::HaltCorrupt { reason } => Some((&outcome.run, reason.as_str())),
                _ => None,
            })
            .collect()
    }

    /// 给操作者看的损坏通知按「会话 + 原因」聚合，避免一个坏会话有几个未完成 Run 就刷
    /// 几条看起来完全一样的消息。恢复动作本身仍保留逐 Run 结果。
    pub fn corrupt_groups(&self) -> Vec<CorruptGroup> {
        let mut groups: Vec<CorruptGroup> = Vec::new();
        for outcome in &self.outcomes {
            let RecoveryAction::HaltCorrupt { reason } = &outcome.action else {
                continue;
            };
            if let Some(group) = groups
                .iter_mut()
                .find(|group| group.session == outcome.session && group.reason == *reason)
            {
                group.runs.push(outcome.run.clone());
            } else {
                groups.push(CorruptGroup {
                    session: outcome.session.clone(),
                    reason: reason.clone(),
                    runs: vec![outcome.run.clone()],
                });
            }
        }
        groups
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
            parts.push(format!("{} 个在等待（审批 / 干预 / 退避）", self.waiting()));
        }
        if self.needs_operator() > 0 {
            parts.push(format!("{} 个需要你处理", self.needs_operator()));
        }
        let corrupt = self.corrupt().len();
        if corrupt > 0 {
            parts.push(format!("{corrupt} 个因损坏已停止"));
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
    orphans: Arc<dyn OrphanOutputs>,
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
            // 默认不查孤儿输出：行为与接这条能力之前一致。
            orphans: Arc::new(NoOrphanLookup),
        }
    }

    /// 接上孤儿 `output.json` 的查找（§14 故障注入表那一行）。不接就一律答"找不到"。
    pub fn with_orphan_outputs(mut self, orphans: Arc<dyn OrphanOutputs>) -> Self {
        self.orphans = orphans;
        self
    }

    /// 跑一遍（§8.7 的启动顺序里"回收没有主人的 running"之后那几步）。
    ///
    /// **一个坏会话只停它自己**（§8.4：「停止受影响会话，报告损坏」——受影响的是它，
    /// 不是这一轮扫描）。所以观察失败在循环里当场接住：把那个 Run 停成
    /// `waiting + intervention` 并记下原因，然后继续判下一个。会一路冒上去的只有索引层
    /// 的失败——那是数据库级的问题，不是某一个会话的。
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
            let (action, orphan) = match self.observe_full(&run).await {
                Ok(observed) => (decide(&observed.input), observed.orphan),
                Err(error) => {
                    // 读不出来的会话：停止**它**，报告损坏，不重跑来掩盖（§8.5）。
                    let reason = format!("会话 {} 读不出来：{error}", run.session);
                    tracing::error!(
                        run = %run.run,
                        session = %run.session,
                        %error,
                        "会话读不出来，停止这个 Run，继续判下一个"
                    );
                    (RecoveryAction::HaltCorrupt { reason }, None)
                }
            };

            let applied = self.apply(&run, &action, orphan.as_ref()).await?;
            tracing::info!(
                run = %run.run,
                session = %run.session,
                state = ?run.state,
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
        Ok(self.observe_full(run).await?.input)
    }

    async fn observe_full(&self, run: &UnfinishedRun) -> Result<Observed, RecoveryError> {
        // 会话这一维先取（§8.9 的第一个问题）。数据库级失败是整轮扫描的事，不在这里
        // 吞掉——一个 Run 的会话行读不出来，说明这一层已经不可信了。
        let state = self.index.session_state(&run.session).await?;
        // 墓碑都没了也是"不服务"：内容不该在，这条 Run 不许领（§8.10）。
        let state = state.unwrap_or(SessionState::Purged);

        // 「内容在不在」是**另一个维度，另一次询问**（§8.9 的第二个问题）。它不能从
        // `read` 的结果推：`Ledger::read` 对"还没有日志的新会话"返回空列表（那是
        // `GET /v1/sessions/{id}/events` 要的行为），所以"读回来没报错"不等于"内容在"。
        // 拿它顶替的话，一个被搬走的会话会被判成"内容在"，然后按空上下文继续一轮。
        let content_available = self.index.session_content(&run.session).await?;

        // 事件另取一份。这里的失败分两种：
        // - **JSONL 中间损坏**：内容在，只是坏了 → 这是损坏，交给 `scan` 的损坏处置
        //   （§8.3：停止受影响会话，报告损坏），**不走"内容缺失"**。
        // - **读不出来**（IO 失败）：`read` 只负责取事件，取不到就是空列表——内容在不在
        //   已经由上面那一问回答了，这里不重复判一次。
        let events = match self.read_session(&run.session).await {
            Ok(events) => events,
            Err(error) => match &error {
                RecoveryError::Ledger(LedgerError::Corrupt(_)) => return Err(error),
                _ => {
                    tracing::warn!(
                        run = %run.run,
                        session = %run.session,
                        %error,
                        "会话事件读不出来，这一轮按空日志判；内容在不在已单独问过"
                    );
                    Vec::new()
                }
            },
        };

        let log_tail = log_tail(&events, &run.run);
        let (output_check, orphan) = self.check_output(&events, run, &log_tail).await;
        let approval = self.observe_approval(&events, run).await?;

        Ok(Observed {
            input: RecoveryInput {
                state: run.state,
                wait: run.wait.clone(),
                log_tail,
                output_check,
                approval,
                result_delivered: run.result_delivered,
                // 「无法确认旧执行已结束时，阻止该任务重复启动并显示原因」（§8.7）。
                previous_executor_stopped: self.liveness.stopped(run.claimed_by.as_ref()),
                session: SessionObservation {
                    state,
                    content_available,
                },
            },
            orphan,
        })
    }

    /// 取这个会话的全部事件。**只负责取事件**：读不到就是空列表，不在这里判"内容在不在"
    /// ——那是 [`RecoveryIndex::session_content`] 的活（§8.9 的两个问题是两次询问）。
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

    /// 输出引用的校验，以及**孤儿 `output.json` 的打捞**。
    ///
    /// 两种情形，同一个问题「那份输出到底成不成立」：
    ///
    /// - `tool.result` 已经写了（第 5 行）：按事件里的引用核对。「若结果引用存在但对应
    ///   输出缺失或哈希不符，停止受影响任务」（§8.5）。
    /// - `tool.started` 写了而结果没写（第 7 行）：去存储里按这次尝试的身份找
    ///   `output.json`。找到、身份对得上、哈希也读得回来——那这次动作**确实发生过**，
    ///   而且结果是完整的；这是 §8.6「先判断是否发生」能拿到的最硬的证据。
    ///   找不到就照旧走工具核对，**不能只凭文件存在判断**。
    async fn check_output(
        &self,
        events: &[Event],
        run: &UnfinishedRun,
        log_tail: &LogTail,
    ) -> (OutputCheck, Option<Orphan>) {
        let LogTail::RoundPersisted { pending } = log_tail else {
            return (OutputCheck::NotApplicable, None);
        };
        match pending {
            PendingCall::ResultPersisted { call } => {
                (self.check_result(events, run, call).await, None)
            }
            PendingCall::Started { call } => self.check_orphan(events, run, call).await,
            PendingCall::Planned { .. } | PendingCall::None => (OutputCheck::NotApplicable, None),
        }
    }

    async fn check_result(
        &self,
        events: &[Event],
        run: &UnfinishedRun,
        call: &ToolCallId,
    ) -> OutputCheck {
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

    async fn check_orphan(
        &self,
        events: &[Event],
        run: &UnfinishedRun,
        call: &ToolCallId,
    ) -> (OutputCheck, Option<Orphan>) {
        let Some(started) = observe::started_call(events, &run.run, call) else {
            return (OutputCheck::NotApplicable, None);
        };
        let attempt = AttemptRef {
            session: run.session.clone(),
            run: run.run.clone(),
            call: call.clone(),
            attempt: started.attempt.clone(),
        };

        let published = match self.orphans.find(&attempt).await {
            Ok(None) => return (OutputCheck::NotApplicable, None),
            Ok(Some(published)) => published,
            Err(StoreError::Corrupt(reason)) => {
                tracing::error!(run = %run.run, call = %call, %reason, "孤儿输出损坏");
                return (OutputCheck::HashMismatch, None);
            }
            Err(error) => {
                tracing::warn!(run = %run.run, call = %call, %error, "孤儿输出查不了");
                return (OutputCheck::NotApplicable, None);
            }
        };

        // 身份：这份 output.json 必须**正是这次尝试**的那一份。「不能只凭文件存在判断」
        // ——路径里编着 run / call / attempt，对不上就是另一次尝试的东西。
        let expected = format!(
            "tool-output/{}/{}/{}/output.json",
            run.run, call, started.attempt
        );
        if published.output.0.path != expected {
            tracing::error!(
                run = %run.run,
                found = %published.output.0.path,
                %expected,
                "孤儿输出的身份对不上"
            );
            return (OutputCheck::HashMismatch, None);
        }

        // 哈希：`open` 读回来并逐字核对（对不上它返回 Corrupt，且**不返回内容**）。
        match self.outputs.open(&published.output).await {
            Ok(_) => {
                tracing::info!(
                    run = %run.run,
                    call = %call,
                    attempt = %started.attempt,
                    plan_hash = %started.plan_hash,
                    output = %published.output.0.path,
                    "找到一份完整的孤儿输出，这次动作确实发生过"
                );
                (
                    OutputCheck::Verified,
                    Some(Orphan {
                        call: call.clone(),
                        attempt: started.attempt,
                        published,
                    }),
                )
            }
            Err(StoreError::NotFound { .. }) => (OutputCheck::Missing, None),
            Err(StoreError::Corrupt(reason)) => {
                tracing::error!(run = %run.run, %reason, "孤儿输出哈希不符");
                (OutputCheck::HashMismatch, None)
            }
            Err(error) => {
                tracing::warn!(run = %run.run, %error, "孤儿输出核对不了");
                (OutputCheck::Missing, None)
            }
        }
    }

    /// 审批的观察。**权威是 state.db**——JSONL 里的审计副本不能自行创建授权（§7.4）。
    ///
    /// 只有**真的停在审批上**才去问数据库：停在退避、干预或前一条 Run 上的 Run 问不出
    /// 审批来，硬问只会把"找不到"当成损坏。
    async fn observe_approval(
        &self,
        events: &[Event],
        run: &UnfinishedRun,
    ) -> Result<ApprovalObservation, RecoveryError> {
        if !matches!(run.wait, Some(WaitReason::Approval { .. })) {
            return Ok(ApprovalObservation::None);
        }
        let Some(id) = waiting_on_approval(events, &run.run) else {
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
    ///
    /// 唯一"写"到账本上的是补记一条已经完整落盘的结果（`VerifyEffect` 那一支），而那是
    /// **复用原输出**，不是重放动作。
    async fn apply(
        &self,
        run: &UnfinishedRun,
        action: &RecoveryAction,
        orphan: Option<&Orphan>,
    ) -> Result<Applied, RecoveryError> {
        match action {
            // 能确定恢复位置的：补索引 → 放回队列。接着跑是 AgentLoop 的事，它读的是
            // 同一份日志，所以"从哪儿接着跑"不需要在这里再说一遍。
            RecoveryAction::BackfillIndexAndQueue
            | RecoveryAction::DiscardPartialAndRerequestModel
            | RecoveryAction::ResumeFromPlan
            | RecoveryAction::ExecutePlannedCall { .. }
            | RecoveryAction::BackfillResultAndContinue { .. }
            | RecoveryAction::ResumeWithDecision { .. } => {
                self.index.backfill(&run.run).await?;
                self.index.requeue(&run.run).await?;
                Ok(Applied::Requeued)
            }
            // 「先核对外部效果，按 §8.6 决定是否安全继续」。核对在这里只有一种做法：
            // 看那次尝试的 `output.json` 在不在、对不对。**在**，就说明动作发生过而且
            // 结果是完整的——于是把这条结果补记进账本（复用原输出，工具不重做），后面
            // 照常接着跑；**不在**，才交给 AgentLoop 去调工具自己的 `verify`。
            RecoveryAction::VerifyEffect { call } => {
                if let Some(orphan) = orphan.filter(|found| &found.call == call) {
                    match self
                        .ledger
                        .finish_call(&orphan.attempt, orphan.published.clone())
                        .await
                    {
                        Ok(()) => tracing::info!(
                            run = %run.run,
                            call = %call,
                            attempt = %orphan.attempt,
                            output = %orphan.published.output.0.path,
                            "补记了那次尝试的结果，复用原输出"
                        ),
                        // 补记不上就退回第 7 行：交给 AgentLoop 去调工具自己的 `verify`。
                        // **不让它把这一轮扫描带走**——别的 Run 还等着判（B5 的教训）。
                        Err(error) => tracing::error!(
                            run = %run.run,
                            call = %call,
                            attempt = %orphan.attempt,
                            %error,
                            "找到了完整的输出却补记不进账本，退回工具核对"
                        ),
                    }
                }
                self.index.backfill(&run.run).await?;
                self.index.requeue(&run.run).await?;
                Ok(Applied::Requeued)
            }
            // 等人、等钟、等重传、已经是终态：原样留着，**一个字都不写**。
            //
            // - `KeepWaiting`：它就是从这个 Run 自己的等待列上读出来的，再写一遍是同值
            //   覆盖；而 `Ledger::suspend` 还会往 JSONL 追一条新的 `run.waiting`——
            //   回放只补索引，不制造新的事实（§8.5、§8.9）。
            // - `WaitUntilRetry`：**恢复扫描不动等退避的 Run**。次数与到点时刻都已经
            //   持久化（§8.5：重启不重置预算），到期之后由调度器的领取查询
            //   （`wait_kind = 'retry' AND wake_at <= now`，§8.7）自己领回去。在这里
            //   requeue 只会让它提前跑，正好绕过那次退避。
            // - `KeepTerminal`：已经是那四个之一了。补写一条终态事件是**重复的终态**，
            //   不是"补齐"——真要补的是索引（第 10 行那条路径的 `RedeliverResult`）。
            RecoveryAction::AwaitResend
            | RecoveryAction::KeepWaiting { .. }
            | RecoveryAction::WaitUntilRetry { .. }
            | RecoveryAction::KeepTerminal { state: _ } => Ok(Applied::LeftAsIs),
            // 结果是好的，只是没送到。**不重新执行任务。**
            RecoveryAction::RedeliverResult => {
                self.index.backfill(&run.run).await?;
                Ok(Applied::Redeliver)
            }
            // 需要操作者判断：停成 `waiting + intervention`（§8.4、§7.5 的清单）。理由
            // 落在 `last_error` 上，因为 `WaitReason::Intervention` 只有句柄。
            RecoveryAction::WaitingForOperator { reason } => {
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
