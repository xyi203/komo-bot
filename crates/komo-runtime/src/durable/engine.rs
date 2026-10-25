//! `Engine`：一条提交线 + 任务调度器，对外的入口。

use std::sync::Arc;

use komo_kernel::durable::{ConversationId, Storage, TaskId, TaskRecord};
use komo_kernel::traits::Clock;

use super::error::DurableError;
use super::kind::{NoHooks, Registry, Reporter, TaskHooks};
use super::line::{Line, TxFuture};
use super::scheduler::{AbortOutcome, Sched, TaskInspection};
use super::tx::Tx;

pub struct EngineOptions {
    /// 关闭时是否关掉底层 `Storage`（测试里"重启"要在同一个内存存储上重开，所以可以不关）。
    pub close_storage: bool,
    pub report: Reporter,
    pub hooks: Arc<dyn TaskHooks>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            close_storage: true,
            report: Arc::new(|_| {}),
            hooks: Arc::new(NoHooks),
        }
    }
}

pub struct Engine {
    line: Line,
    sched: Arc<Sched>,
    close_storage: bool,
}

impl Engine {
    /// 打开：读出活任务、把幸存的 `running` 改回 `pending`。调度默认暂停，调 [`Engine::resume`]
    /// 开始。开库失败时线会被封口，不留下订阅。
    pub async fn open(
        storage: Arc<dyn Storage>,
        registry: Registry,
        clock: Arc<dyn Clock>,
        options: EngineOptions,
    ) -> Result<Self, DurableError> {
        let line = Line::new(storage, clock);
        let sched = Sched::new(line.clone(), registry, options.report, options.hooks);
        if let Err(error) = sched.open().await {
            line.seal();
            return Err(error);
        }
        Ok(Self {
            line,
            sched,
            close_storage: options.close_storage,
        })
    }

    pub fn line(&self) -> &Line {
        &self.line
    }

    /// 开始（或继续）调度。幂等。
    pub fn resume(&self) {
        self.sched.resume();
    }

    /// 线失败的原因（存储错误让整条线失败，spec §10.1）。
    pub fn failure(&self) -> Option<DurableError> {
        self.line.failure()
    }

    /// 一次不绑定任务的提交。
    pub async fn commit<T: Send>(
        &self,
        f: impl for<'a> FnOnce(&'a mut Tx) -> TxFuture<'a, T> + Send,
    ) -> Result<T, DurableError> {
        self.line.commit(None, f).await
    }

    /// `abortTask`：提交 abort 标记，join run invocation；之后用 [`Engine::wait_for_task`] 等结局。
    pub async fn abort_task(&self, id: TaskId) -> Result<AbortOutcome, DurableError> {
        self.sched.abort(id, false).await
    }

    pub async fn wait_for_task(&self, id: TaskId) -> Result<TaskRecord, DurableError> {
        self.sched.wait_for_task(id).await
    }

    /// 等范围里没有活的非后台任务；`None` 是从每个没有所有者的 conversation 出发。
    pub async fn wait_for_idle(
        &self,
        conversation: Option<ConversationId>,
    ) -> Result<(), DurableError> {
        self.sched.wait_for_idle(conversation).await
    }

    /// `Conversation.abort()`：等被标记的任务终结（不等取消之后起的后继 run）；`background` 时穿过后台边界。
    pub async fn abort_conversation(
        &self,
        conversation: ConversationId,
        background: bool,
    ) -> Result<(), DurableError> {
        self.sched
            .abort_conversation(conversation, background)
            .await
    }

    /// 每条活任务与调度器眼里它现在的状态（运行中 / 在等 / 被阻塞 / 可跑）。
    pub fn inspect(&self) -> Vec<TaskInspection> {
        self.sched.inspect_now()
    }

    /// 关闭：封住受理与 reservation，通知 invocation，等已受理的提交结算、invocation 走完，
    /// 再（按选项）关存储。不写任何任务结局，重开后从 checkpoint 继续。
    pub async fn close(&self) -> Result<(), DurableError> {
        self.line.seal();
        self.sched.join().await;
        self.line.finish_close(self.close_storage).await
    }
}
