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
    ConversationId, Message, Storage, SubmissionId, SubmissionRecord, SubmissionStatus, TaskRecord,
};
use tokio::sync::Notify;

use super::generation::{self, GenerationTask};
use super::hooks::HarnessDeps;
use super::inbox::{self, Admitted, QueueModes, SubmissionDraft, WriteDraft};
use super::live::{self, LiveState, live_address};
use super::outcome::unanswered;
use super::tool::ToolTask;
use super::{GENERATION_KIND, TOOL_KIND};
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
    }

    /// 给 `EngineOptions::hooks`：调度器自己写的结局（faulted / orphaned）的清理。
    pub fn hooks(&self) -> Arc<dyn TaskHooks> {
        Arc::new(HarnessHooks)
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
pub struct HarnessHooks;

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
                    record.conversation,
                    record.id,
                    &settlement,
                )
                .await
            }
            _ => Ok(()),
        }
    }
}
