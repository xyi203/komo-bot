//! `Harness`：submission 的入口（受理、撤回、等待、reset）与 Engine 的装配
//! （pi-durable `harness/submissions.ts`）。
//!
//! 用法（测试里就是这样）：
//!
//! ```ignore
//! let harness = Harness::new(HarnessDeps::new(hooks, stream));
//! let registry = Registry::new();
//! harness.install(&registry);                       // komo.generation + komo.tool
//! let engine = Engine::open(storage, registry, clock, EngineOptions {
//!     hooks: harness.hooks(), ..Default::default()  // faulted / orphaned 的清理
//! }).await?;
//! let id = harness.submit(&engine, conversation, SubmissionDraft::input("hi")).await?;
//! let settled = harness.wait(&engine, id).await?;
//! ```
//!
//! 一个 `Harness` 可以跨多次"重启"用：它只持有依赖，状态都在存储里。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::durable::records::TaskOutcome;
use komo_kernel::durable::{
    ConversationId, Message, Storage, SubmissionId, SubmissionRecord, SubmissionStatus, TaskId,
    TaskRecord,
};
use tokio::sync::Notify;

use super::compaction::{self, CompactionTask};
use super::generation::{self, GenerationTask};
use super::hooks::HarnessDeps;
use super::inbox::{self, Admitted, QueueModes, SubmissionDraft, WriteDraft};
use super::live::{self, LiveState, live_address};
use super::orchestration::monitor;
use super::outcome::unanswered;
use super::tool::ToolTask;
use super::{COMPACTION_KIND, GENERATION_KIND, TOOL_KIND};
use crate::durable::engine::Engine;
use crate::durable::error::DurableError;
use crate::durable::kind::{Registry, TaskHooks};
use crate::durable::tx::Tx;

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    /// `whenBusy: reject` 的输入遇到忙的 conversation：什么都没写（spec §6）。
    #[error("conversation {0} is busy")]
    Busy(ConversationId),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Durable(#[from] DurableError),
}

/// 撤回一个 submission 的结果（spec §6 `abort`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawResult {
    /// 排着队，已撤回（`unanswered{aborted}`）。
    Aborted,
    /// 输入已经放置、run 在跑：撤回不了。
    AlreadyPlaced,
    /// 已经 settle。
    Settled,
    NotFound,
}

#[derive(Clone)]
pub struct Harness {
    deps: Arc<HarnessDeps>,
    generation: GenerationTask,
    tool: ToolTask,
}

impl Harness {
    pub fn new(deps: HarnessDeps) -> Self {
        let deps = Arc::new(deps);
        let tool = ToolTask::new(deps.clone());
        let generation = GenerationTask::new(deps.clone(), tool.clone());
        Self {
            deps,
            generation,
            tool,
        }
    }

    pub fn deps(&self) -> &Arc<HarnessDeps> {
        &self.deps
    }

    /// 把内置任务种类装进注册表。
    pub fn install(&self, registry: &Registry) {
        registry.install(Arc::new(self.generation.clone()));
        registry.install(Arc::new(self.tool.clone()));
        registry.install(Arc::new(self.generation.compaction().clone()));
        registry.install(Arc::new(super::orchestration::BackgroundTask));
    }

    /// 给 `EngineOptions::hooks`：调度器自己写的结局（faulted / orphaned）的清理。
    pub fn hooks(&self) -> Arc<dyn TaskHooks> {
        Arc::new(HarnessHooks {
            generation: self.generation.clone(),
        })
    }

    fn modes(&self) -> QueueModes {
        let settings = self.deps.settings();
        QueueModes {
            steering: settings.steering_mode,
            follow_up: settings.follow_up_mode,
        }
    }

    /// 受理一个 submission（一次提交）：空闲时直接放置并起 run；忙时排队。已知的 `request_id`
    /// 原样返回已有的那条。受理会让引擎开始调度。
    pub async fn submit(
        &self,
        engine: &Engine,
        conversation: ConversationId,
        draft: SubmissionDraft,
    ) -> Result<SubmissionId, SubmitError> {
        draft.check().map_err(SubmitError::Invalid)?;
        engine.resume();
        let generation = self.generation.clone();
        let modes = self.modes();
        let admitted = engine
            .commit(move |tx| {
                Box::pin(
                    async move { inbox::admit(tx, &generation, conversation, &draft, modes).await },
                )
            })
            .await?;
        match admitted {
            Admitted::Id(id) => Ok(id),
            Admitted::Busy => Err(SubmitError::Busy(conversation)),
        }
    }

    /// `/new`：`komo.reset` write（`head: "self"`），可带交接文字。空闲时立刻落，忙时排在
    /// 收件箱里、在 boundary 放置。
    pub async fn reset(
        &self,
        engine: &Engine,
        conversation: ConversationId,
        handoff: Option<Message>,
    ) -> Result<SubmissionId, SubmitError> {
        self.submit(
            engine,
            conversation,
            SubmissionDraft::write(WriteDraft::reset(handoff)),
        )
        .await
    }

    /// 手动压缩（`/compact`）：起一个 conversation 所有的压缩任务，不是 background，所以
    /// `abort_conversation` 会取消它；摘要像别的被动 write 一样放置（空闲时立刻落，忙时在下一个
    /// boundary 落，过期了 `unanswered{stale}`）。返回任务 ID，结局从任务记录读。
    pub async fn compact(
        &self,
        engine: &Engine,
        conversation: ConversationId,
        instructions: Option<String>,
    ) -> Result<TaskId, DurableError> {
        engine.resume();
        let compaction: CompactionTask = self.generation.compaction().clone();
        engine
            .commit(move |tx| {
                Box::pin(async move {
                    let mut live = live::load(tx, conversation).await?;
                    let mut input = compaction::Input::new(compaction::Reason::Manual, None);
                    input.instructions = instructions;
                    let id = compaction::task::create(
                        tx,
                        &compaction,
                        conversation,
                        &mut live,
                        &input,
                        None,
                    )
                    .await?;
                    live::store(tx, conversation, &live).await?;
                    Ok(id)
                })
            })
            .await
    }

    /// 监听结束了（脚本退出 / 超时 / 刷屏 / 事件到上限，gateway 的泵调）：登记 `ended` 与结束消息
    /// `monitor-end:{id}` 同一次提交。只有第一个结束它的人赢：已经结束的（模型 `monitor_stop` 停掉
    /// 的、对账判过的）返回 `false`，什么都不写、不重复交回。见 [`orchestration::monitor`]。
    pub async fn finish_monitor(
        &self,
        engine: &Engine,
        id: u64,
        reason: &str,
        detail: Option<&str>,
        message: String,
    ) -> Result<bool, DurableError> {
        engine.resume();
        let generation = self.generation.clone();
        let modes = self.modes();
        let (reason, detail) = (reason.to_owned(), detail.map(str::to_owned));
        engine
            .commit(move |tx| {
                Box::pin(async move {
                    monitor::finish_in_tx(
                        tx,
                        &generation,
                        modes,
                        id,
                        &reason,
                        detail.as_deref(),
                        message,
                    )
                    .await
                })
            })
            .await
    }

    /// 重启后对账（gateway 启动时调一次）：每条 `running` 的登记——声明了 `resume_after_restart`
    /// 且离截止还有至少 1 分钟的，让 gateway 重挂（同一个 id）；其余登记为结束并交回"已中断"（
    /// 重挂不上、时限已过同理）。`starting` 的归挂它的工具任务接着判，这里不碰。
    pub async fn reconcile_monitors(
        &self,
        engine: &Engine,
    ) -> Result<Vec<monitor::Reconciled>, DurableError> {
        use monitor::{LaunchMode, MIN_RESUME, MonitorLaunch, MonitorState, Reconciled};
        let line = engine.line();
        let records = monitor::list(&**line.storage()).await?;
        let now = line.now_ms();
        let mut done = Vec::new();
        for record in records {
            if record.state != MonitorState::Running {
                continue;
            }
            let remaining = record.deadline - now;
            let mut why = if record.resume_after_restart {
                "时限已经到了（Gateway 重启时已过截止或只剩不到 1 分钟），没有重挂。".to_owned()
            } else {
                "已中断：Gateway 重启了，脚本没有在跑，也不会自动重挂。".to_owned()
            };
            let mut reason = if record.resume_after_restart {
                "timeout"
            } else {
                "interrupted"
            };
            if record.resume_after_restart && remaining >= MIN_RESUME.as_millis() as i64 {
                let launch = MonitorLaunch {
                    record: record.clone(),
                    mode: LaunchMode::Resume,
                    remaining_ms: remaining,
                };
                match self.deps.orchestration.start_monitor(&launch).await {
                    Ok(()) => {
                        done.push(Reconciled::Resumed(record.id));
                        continue;
                    }
                    Err(error) => {
                        why = format!("已中断：Gateway 重启后重挂失败：{error}");
                        reason = "interrupted";
                    }
                }
            }
            let message = monitor::interrupted_message(&record, &why);
            if self
                .finish_monitor(engine, record.id, reason, Some(&why), message)
                .await?
            {
                done.push(Reconciled::Ended(record.id, reason.to_owned()));
            }
        }
        Ok(done)
    }

    /// 撤回一个还在排队的 submission。`conversation` 给了就只认那个 conversation 里的。
    pub async fn withdraw(
        &self,
        engine: &Engine,
        id: SubmissionId,
        conversation: Option<ConversationId>,
    ) -> Result<WithdrawResult, DurableError> {
        let result = engine
            .commit(move |tx| Box::pin(async move { inbox::withdraw(tx, id, conversation).await }))
            .await?;
        Ok(match result {
            inbox::Withdrawn::Aborted => WithdrawResult::Aborted,
            inbox::Withdrawn::AlreadyPlaced => WithdrawResult::AlreadyPlaced,
            inbox::Withdrawn::Settled => WithdrawResult::Settled,
            inbox::Withdrawn::NotFound => WithdrawResult::NotFound,
        })
    }

    /// `Conversation.abort()`：撤回排队的输入（排队的 write 留着），再 abort 这个 conversation
    /// 里的任务（自底向上；每个任务在自己的 abort handler 里写 aborted 结果）。
    pub async fn abort_conversation(
        &self,
        engine: &Engine,
        conversation: ConversationId,
    ) -> Result<(), DurableError> {
        engine
            .commit(move |tx| {
                Box::pin(async move { inbox::withdraw_queued_inputs(tx, conversation).await })
            })
            .await?;
        engine.abort_conversation(conversation, false).await
    }

    /// 等一个 submission settle（`done` 或 `unanswered`）。线封口（关闭或失败）时报错。
    pub async fn wait(
        &self,
        engine: &Engine,
        id: SubmissionId,
    ) -> Result<SubmissionRecord, DurableError> {
        engine.resume();
        let line = engine.line();
        let notify = Arc::new(Notify::new());
        let subscription = {
            let notify = notify.clone();
            line.subscribe_commits(move |_| notify.notify_waiters())
        };
        let result = loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            // 先登记再查，免得 settle 的发布落在两者之间。
            notified.as_mut().enable();
            let record =
                line.storage().submission(id).await?.ok_or_else(|| {
                    DurableError::rejected(format!("Submission {id} does not exist"))
                })?;
            if matches!(
                record.status(),
                SubmissionStatus::Done | SubmissionStatus::Unanswered
            ) {
                break Ok(record);
            }
            if line.is_sealed() {
                break Err(line.closed_error());
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = line.sealed() => {}
            }
        };
        subscription.dispose();
        result
    }

    /// 已提交的 `komo.live`（读不经过线）。
    pub async fn live(
        storage: &dyn Storage,
        conversation: ConversationId,
    ) -> Result<LiveState, DurableError> {
        match storage.find_document(&live_address(conversation)).await? {
            None => Ok(LiveState::default()),
            Some(record) => serde_json::from_value(record.value)
                .map_err(|e| DurableError::rejected(format!("komo.live is malformed: {e}"))),
        }
    }

    /// 已提交的 `komo.inbox`。
    pub async fn inbox(
        storage: &dyn Storage,
        conversation: ConversationId,
    ) -> Result<inbox::InboxState, DurableError> {
        match storage
            .find_document(&inbox::inbox_address(conversation))
            .await?
        {
            None => Ok(inbox::InboxState::default()),
            Some(record) => serde_json::from_value(record.value)
                .map_err(|e| DurableError::rejected(format!("komo.inbox is malformed: {e}"))),
        }
    }
}

/// 调度器自己写的结局（`faulted` / `orphaned`）变成 terminal 的那笔提交里的清理
/// （spec §5.4；`harness/live.ts` 的 `settleSchedulerOutcome`）。
///
/// - 工具任务：槽位标记完成、没有 entry——上下文推导会补上缺的结果。
/// - 生成任务：已提交的部分回复转成 aborted entry，run 的输入以 `faulted` / 孤儿原因收场。
/// - 压缩任务：移除状态、记下 `komo.live.compactionFailure`。
pub struct HarnessHooks {
    /// 收尾要往外交回结果（后台任务的子 conversation，`orchestration::report`）时放置输入、起 run 用。
    generation: GenerationTask,
}

#[async_trait]
impl TaskHooks for HarnessHooks {
    async fn settle_outcome(
        &self,
        tx: &mut Tx,
        record: &TaskRecord,
        outcome: &TaskOutcome,
    ) -> Result<(), DurableError> {
        match record.kind.as_str() {
            TOOL_KIND => {
                let mut live = live::load(tx, record.conversation).await?;
                if let Some(slot) = live.slot_mut(record.id) {
                    slot.finish();
                    live::store(tx, record.conversation, &live).await?;
                }
                Ok(())
            }
            GENERATION_KIND => {
                let settlement = match outcome {
                    TaskOutcome::Faulted { error } => unanswered("faulted", Some(&error.message)),
                    TaskOutcome::Orphaned { reason } => unanswered(reason, None),
                    _ => return Ok(()),
                };
                generation::settle_scheduler_outcome(
                    tx,
                    &self.generation,
                    record.conversation,
                    record.id,
                    &settlement,
                )
                .await
            }
            COMPACTION_KIND => {
                let (reason, error) = match outcome {
                    TaskOutcome::Faulted { error } => ("faulted", error.message.as_str()),
                    TaskOutcome::Orphaned { reason } => ("orphaned", reason.as_str()),
                    _ => return Ok(()),
                };
                compaction::task::settle_scheduler_outcome(
                    tx,
                    record.conversation,
                    record.id,
                    reason,
                    error,
                )
                .await
            }
            _ => Ok(()),
        }
    }
}
