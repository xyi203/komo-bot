//! 交给 phase handler 的 `TaskRuntime`（spec §5.1）。
//!
//! 每个操作都绑在发出它的 invocation 上：invocation 结束之后一律拒绝（即使 Session 还开着），
//! 它的信号（`cancelled()`）在结束时取消。

use std::sync::Arc;

use serde_json::Value;

use komo_kernel::durable::records::TaskOutcome;
use komo_kernel::durable::storage::EntryQuery;
use komo_kernel::durable::task::{NextTaskState, RunningTask, memo_of, put_memo};
use komo_kernel::durable::{
    ContextView, ConversationId, Cursor, EntryId, EntryRecord, ScanOrder, TaskId, TaskRecord,
    TaskState, derive_context,
};
use tokio_util::sync::CancellationToken;

use super::error::DurableError;
use super::line::TxFuture;
use super::scheduler::{Invocation, Sched};
use super::tx::Tx;

const SCAN_PAGE: usize = 256;

#[derive(Clone)]
pub struct TaskRuntime {
    sched: Arc<Sched>,
    invocation: Arc<Invocation>,
}

impl TaskRuntime {
    pub(super) fn new(sched: Arc<Sched>, invocation: Arc<Invocation>) -> Self {
        Self { sched, invocation }
    }

    pub fn task_id(&self) -> TaskId {
        self.invocation.task
    }

    pub fn conversation(&self) -> ConversationId {
        self.invocation.conversation
    }

    /// invocation 的信号：abort 标记、关闭或结束时取消。handler 里自己的等待可以拿它
    /// 和别的 future 一起 `select!`。
    pub fn cancelled(&self) -> CancellationToken {
        self.invocation.token.clone()
    }

    fn check(&self) -> Result<(), DurableError> {
        self.invocation.check()
    }

    /// 在线上重读并把关当前任务，调 `change`，再把它返回的状态（若有）与回调的 entry、
    /// document、子任务写在同一笔提交里。`None` 保持状态不变；`Waiting` 即使 checkpoint
    /// 没变也结束这次 invocation 的运行。
    ///
    /// 被拒的情况：invocation 已结束、Session 关闭、任务已 terminal、run 模式下已有 abort 标记、
    /// 等待指向不存在的任务或会成环。
    pub async fn commit<F>(&self, change: F) -> Result<(), DurableError>
    where
        F: for<'a> FnOnce(&'a mut Tx, &'a RunningTask) -> TxFuture<'a, Option<NextTaskState>>
            + Send
            + 'static,
    {
        let sched = self.sched.clone();
        let invocation = self.invocation.clone();
        self.sched
            .gated(&self.invocation, move |tx, current| {
                Box::pin(async move {
                    let running = RunningTask::from_record(current).ok_or_else(|| {
                        DurableError::rejected(format!("Task {} is not running", current.id))
                    })?;
                    if let Some(next) = change(tx, &running).await? {
                        sched.commit_state(tx, &invocation, current, next).await?;
                    }
                    Ok(())
                })
            })
            .await
    }

    /// 只替换状态的提交。
    pub async fn commit_next(&self, next: NextTaskState) -> Result<(), DurableError> {
        self.commit(move |_tx, _current| Box::pin(async move { Ok(Some(next)) }))
            .await
    }

    /// 读已提交记录里的 memo。
    pub async fn memo(&self, name: &str) -> Result<Option<Value>, DurableError> {
        self.check()?;
        let sched = self.sched.clone();
        let task = self.invocation.task;
        let name = name.to_owned();
        self.sched
            .line
            .read_on_line(move || {
                Box::pin(async move {
                    let found = sched.st().graph.live_record(task).cloned();
                    Ok(found.and_then(|record| memo_of(&record, &name).cloned()))
                })
            })
            .await
    }

    /// 先写者赢的 memo：候选与读赢家是同一笔提交，并发的候选拿到同一个持久的赢家。
    pub async fn put_memo(&self, name: &str, candidate: Value) -> Result<Value, DurableError> {
        let sched = self.sched.clone();
        let name = name.to_owned();
        self.sched
            .gated(&self.invocation, move |tx, current| {
                Box::pin(async move {
                    let (next, winner) = put_memo(current, &name, candidate);
                    if next != *current {
                        sched.write_task(tx, next).await?;
                    }
                    Ok(winner)
                })
            })
            .await
    }

    /// 睡到时钟（`Line` 的 `now_ms`）到 `until`；invocation 被发信号或结束时拒绝。
    pub async fn sleep_until(&self, until_ms: i64) -> Result<(), DurableError> {
        const MAX_STEP_MS: i64 = 3_600_000;
        loop {
            self.check()?;
            let remaining = until_ms - self.sched.line.now_ms();
            if remaining <= 0 {
                return Ok(());
            }
            let step = std::time::Duration::from_millis(remaining.min(MAX_STEP_MS) as u64);
            tokio::select! {
                _ = tokio::time::sleep(step) => {}
                _ = self.invocation.token.cancelled() => {
                    return Err(DurableError::InvocationEnded(self.invocation.task));
                }
            }
        }
    }

    /// 已提交的任务记录。
    pub async fn get_task(&self, id: TaskId) -> Result<Option<TaskRecord>, DurableError> {
        self.check()?;
        let sched = self.sched.clone();
        self.sched
            .line
            .read_on_line(move || Box::pin(async move { Ok(sched.storage.task(id).await?) }))
            .await
    }

    /// 等任务 terminal 并返回它的记录；invocation 被发信号或结束时拒绝。
    pub async fn wait_for_task(&self, id: TaskId) -> Result<TaskRecord, DurableError> {
        self.check()?;
        tokio::select! {
            record = self.sched.wait_for_task(id) => record,
            _ = self.invocation.token.cancelled() => {
                Err(DurableError::InvocationEnded(self.invocation.task))
            }
        }
    }

    /// 对自己拥有的任务 `abort_task`，并等它 terminal；别人的任务拒绝。不替换 restart 标记。
    pub async fn abort_owned(&self, id: TaskId) -> Result<(), DurableError> {
        self.check()?;
        let record = self
            .get_task(id)
            .await?
            .ok_or(DurableError::NoSuchTask(id))?;
        if record.owner != Some(self.invocation.task) {
            return Err(DurableError::rejected(format!(
                "Task {id} is not owned by task {}",
                self.invocation.task
            )));
        }
        if matches!(record.state, TaskState::Terminal { .. }) {
            return Ok(());
        }
        tokio::select! {
            result = async {
                self.sched.abort(id, true).await?;
                self.sched.wait_for_task(id).await.map(|_| ())
            } => result,
            _ = self.invocation.token.cancelled() => {
                Err(DurableError::InvocationEnded(self.invocation.task))
            }
        }
    }

    /// 自己直接拥有的活任务，按 ID 升序（不含它的 conversation 里别的任务）。
    pub async fn owned_tasks(&self) -> Result<Vec<TaskRecord>, DurableError> {
        self.check()?;
        let sched = self.sched.clone();
        let invocation = self.invocation.clone();
        self.sched
            .line
            .read_on_line(move || {
                Box::pin(async move {
                    // 结束 invocation 的 step 可能先于这次读跑过。
                    invocation.check()?;
                    let mut owned: Vec<TaskRecord> = sched
                        .st()
                        .graph
                        .live()
                        .filter(|record| record.owner == Some(invocation.task))
                        .cloned()
                        .collect();
                    owned.sort_by_key(|record| record.id);
                    Ok(owned)
                })
            })
            .await
    }

    /// 已 terminal 的任务的结局，按给定次序；有一个不是 terminal 就拒绝。等待恢复后用。
    pub async fn outcomes(&self, ids: &[TaskId]) -> Result<Vec<TaskOutcome>, DurableError> {
        self.check()?;
        let sched = self.sched.clone();
        let ids = ids.to_vec();
        self.sched
            .line
            .read_on_line(move || {
                Box::pin(async move {
                    let mut outcomes = Vec::with_capacity(ids.len());
                    for id in ids {
                        match sched.storage.task(id).await?.map(|record| record.state) {
                            Some(TaskState::Terminal { outcome }) => outcomes.push(outcome),
                            _ => {
                                return Err(DurableError::rejected(format!(
                                    "Task {id} is not terminal"
                                )));
                            }
                        }
                    }
                    Ok(outcomes)
                })
            })
            .await
    }

    /// 从任务所在的 conversation 看得见的已提交 entry。
    pub async fn entry(&self, id: EntryId) -> Result<Option<EntryRecord>, DurableError> {
        self.check()?;
        let sched = self.sched.clone();
        let conversation = self.invocation.conversation;
        self.sched
            .line
            .read_on_line(move || {
                Box::pin(async move {
                    Ok(sched
                        .storage
                        .visible_entry(conversation, id)
                        .await?
                        .map(|stored| stored.entry))
                })
            })
            .await
    }

    /// conversation 沿 fork 祖先拼出来的已提交 entry，按 ID 升序（可截到 `at`，含）。D4
    /// 的生成阶段要按 entry 的 `kind` 规划 system 段，所以单独露出来。
    pub async fn entries(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
    ) -> Result<Vec<EntryRecord>, DurableError> {
        self.check()?;
        let sched = self.sched.clone();
        let invocation = self.invocation.clone();
        self.sched
            .line
            .read_on_line(move || {
                Box::pin(async move {
                    invocation.check()?;
                    let mut entries = Vec::new();
                    let query = EntryQuery {
                        order: Some(ScanOrder::Ascending),
                        max: at,
                        ..EntryQuery::new(conversation)
                    };
                    let mut cursor: Option<Cursor> = None;
                    loop {
                        let page = sched
                            .storage
                            .scan_entries(&query, SCAN_PAGE, cursor.as_ref())
                            .await?;
                        entries.extend(page.items);
                        match page.next {
                            Some(next) => cursor = Some(next),
                            None => break,
                        }
                    }
                    Ok(entries)
                })
            })
            .await
    }

    /// conversation 已提交的活动 transcript 与模型上下文（可截到 `at`）。不缓存：每次读
    /// 可见历史再推导（pi 的 range 缓存是性能优化，这里留给 D4 需要时再加）。
    pub async fn context(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
    ) -> Result<ContextView, DurableError> {
        let entries = self.entries(conversation, at).await?;
        Ok(derive_context(&entries, at))
    }

    /// 时钟（毫秒）。
    pub fn now(&self) -> Result<i64, DurableError> {
        self.check()?;
        Ok(self.sched.line.now_ms())
    }

    /// 转发非致命失败。
    pub fn report(&self, message: &str) -> Result<(), DurableError> {
        self.check()?;
        (self.sched.report)(message);
        Ok(())
    }
}
