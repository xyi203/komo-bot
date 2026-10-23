//! 测试脚手架：脚本化的任务定义、暂停时钟、可注入故障的存储、"重启"。

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use time::OffsetDateTime;

use komo_kernel::durable::records::{JoinPolicy, TaskOutcome, TaskOutcomeError};
use komo_kernel::durable::storage::{
    ConversationQuery, DocumentQuery, EntryQuery, StoredEntry, SubmissionQuery, TaskQuery,
};
use komo_kernel::durable::task::{NextTaskState, RunningTask};
use komo_kernel::durable::{
    ConversationId, ConversationRecord, Cursor, DocumentAddress, DocumentId, DocumentRecord,
    EntryId, EntryRecord, MemoryStorage, Page, ROOT_CONVERSATION_ID, Seq, Storage, StorageError,
    StorageWrite, SubmissionId, SubmissionRecord, TaskId, TaskRecord, TaskState,
};
use komo_kernel::traits::Clock;

use super::super::kind::Migrated;
use super::super::{
    Engine, EngineOptions, Ownership, Registry, TaskError, TaskKind, TaskOptions, TaskRuntime,
};

// ------------------------------------------------------------------- 时钟

/// 跟着 tokio（暂停）时钟走的墙钟：`start_paused` 下 `sleep` 自动快进，这个钟同步前进。
pub struct TokioClock {
    base: OffsetDateTime,
    start: tokio::time::Instant,
}

impl TokioClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            base: time::macros::datetime!(2026-10-23 08:00:00 UTC),
            start: tokio::time::Instant::now(),
        })
    }

    pub fn now_ms(&self) -> i64 {
        (self.now().unix_timestamp_nanos() / 1_000_000) as i64
    }
}

impl Clock for TokioClock {
    fn now(&self) -> OffsetDateTime {
        self.base + self.start.elapsed()
    }
}

// ------------------------------------------------------------ 脚本化的任务定义

type PhaseFn = Arc<
    dyn Fn(RunningTask, TaskRuntime) -> BoxFuture<'static, Result<(), TaskError>> + Send + Sync,
>;
type MigrateFn = Arc<dyn Fn(&Value, &Value, u32) -> Result<Migrated, TaskError> + Send + Sync>;
type InitialFn = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

pub struct Kind {
    name: String,
    version: u32,
    initial: InitialFn,
    phases: HashMap<String, PhaseFn>,
    abort: Option<PhaseFn>,
    migrate: Option<MigrateFn>,
}

pub fn kind(name: &str) -> Kind {
    Kind {
        name: name.to_owned(),
        version: 1,
        initial: Arc::new(|_| json!({"phase": "start"})),
        phases: HashMap::new(),
        abort: None,
        migrate: None,
    }
}

fn boxed<F, Fut>(f: F) -> PhaseFn
where
    F: Fn(RunningTask, TaskRuntime) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
{
    Arc::new(move |task, rt| Box::pin(f(task, rt)))
}

impl Kind {
    pub fn version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    pub fn initial(mut self, f: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Self {
        self.initial = Arc::new(f);
        self
    }

    pub fn phase<F, Fut>(mut self, name: &str, f: F) -> Self
    where
        F: Fn(RunningTask, TaskRuntime) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
    {
        self.phases.insert(name.to_owned(), boxed(f));
        self
    }

    pub fn on_abort<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(RunningTask, TaskRuntime) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
    {
        self.abort = Some(boxed(f));
        self
    }

    pub fn migrate(
        mut self,
        f: impl Fn(&Value, &Value, u32) -> Result<Migrated, TaskError> + Send + Sync + 'static,
    ) -> Self {
        self.migrate = Some(Arc::new(f));
        self
    }

    pub fn build(self) -> Arc<Kind> {
        Arc::new(self)
    }
}

#[async_trait]
impl TaskKind for Kind {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn initial(&self, input: &Value) -> Value {
        (self.initial)(input)
    }

    async fn run_phase(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        match self.phases.get(task.phase()) {
            Some(phase) => phase(task.clone(), rt.clone()).await,
            None => Err(TaskError::new(format!("no phase {}", task.phase()))),
        }
    }

    /// 默认的 abort handler：直接记 `aborted`。
    async fn abort(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        match &self.abort {
            Some(abort) => abort(task.clone(), rt.clone()).await,
            None => {
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        }
    }

    fn migrate(
        &self,
        input: &Value,
        checkpoint: &Value,
        from_version: u32,
    ) -> Option<Result<Migrated, TaskError>> {
        self.migrate
            .as_ref()
            .map(|migrate| migrate(input, checkpoint, from_version))
    }

    fn has_migrate(&self) -> bool {
        self.migrate.is_some()
    }
}

// ----------------------------------------------------------- 状态的简写

pub fn go(phase: &str) -> NextTaskState {
    NextTaskState::Running {
        checkpoint: json!({"phase": phase}),
    }
}

pub fn go_with(checkpoint: Value) -> NextTaskState {
    NextTaskState::Running { checkpoint }
}

pub fn done(result: Value) -> NextTaskState {
    NextTaskState::Terminal {
        outcome: TaskOutcome::Completed { result },
    }
}

pub fn failed(message: &str) -> NextTaskState {
    NextTaskState::Terminal {
        outcome: TaskOutcome::Failed {
            error: TaskOutcomeError {
                message: message.to_owned(),
                detail: None,
            },
            result: None,
        },
    }
}

pub fn aborted() -> NextTaskState {
    NextTaskState::Terminal {
        outcome: TaskOutcome::Aborted {
            reason: None,
            result: None,
        },
    }
}

pub fn wait(phase: &str, on: Vec<TaskId>, policy: JoinPolicy) -> NextTaskState {
    NextTaskState::Waiting {
        checkpoint: json!({"phase": phase}),
        on,
        policy,
    }
}

pub fn status(outcome: &TaskOutcome) -> &'static str {
    match outcome {
        TaskOutcome::Completed { .. } => "completed",
        TaskOutcome::Failed { .. } => "failed",
        TaskOutcome::Aborted { .. } => "aborted",
        TaskOutcome::Orphaned { .. } => "orphaned",
        TaskOutcome::Faulted { .. } => "faulted",
    }
}

pub fn outcome_of(record: &TaskRecord) -> &TaskOutcome {
    match &record.state {
        TaskState::Terminal { outcome } | TaskState::Completing { outcome } => outcome,
        other => panic!("task {} has no outcome yet: {other:?}", record.id),
    }
}

pub fn faulted_message(record: &TaskRecord) -> String {
    match outcome_of(record) {
        TaskOutcome::Faulted { error } => error.message.clone(),
        other => panic!("expected faulted, got {other:?}"),
    }
}

/// 共享的事件记录。
#[derive(Clone, Default)]
pub struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    pub fn push(&self, line: impl Into<String>) {
        self.0.lock().unwrap().push(line.into());
    }

    pub fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    pub fn count(&self, line: &str) -> usize {
        self.lines().iter().filter(|l| *l == line).count()
    }
}

// ------------------------------------------------------------------ 存储替身

/// 转发给内存存储，可以让之后的提交失败。
pub struct FlakyStorage {
    pub inner: Arc<MemoryStorage>,
    pub fail_commits: AtomicBool,
    pub commits: Mutex<usize>,
}

impl FlakyStorage {
    pub fn new(inner: Arc<MemoryStorage>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fail_commits: AtomicBool::new(false),
            commits: Mutex::new(0),
        })
    }
}

#[async_trait]
impl Storage for FlakyStorage {
    async fn commit(&self, writes: &[StorageWrite]) -> Result<Seq, StorageError> {
        *self.commits.lock().unwrap() += 1;
        if self.fail_commits.load(Ordering::SeqCst) {
            return Err(StorageError::Backend("disk is on fire".into()));
        }
        self.inner.commit(writes).await
    }

    async fn mint_id(&self) -> Result<u64, StorageError> {
        self.inner.mint_id().await
    }

    async fn conversation(
        &self,
        id: ConversationId,
    ) -> Result<Option<ConversationRecord>, StorageError> {
        self.inner.conversation(id).await
    }

    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>, StorageError> {
        self.inner.scan_conversations(query, limit, cursor).await
    }

    async fn entry(&self, id: EntryId) -> Result<Option<StoredEntry>, StorageError> {
        self.inner.entry(id).await
    }

    async fn visible_entry(
        &self,
        conversation: ConversationId,
        id: EntryId,
    ) -> Result<Option<StoredEntry>, StorageError> {
        self.inner.visible_entry(conversation, id).await
    }

    async fn find_latest_head_marker(
        &self,
        conversation: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> Result<Option<EntryRecord>, StorageError> {
        self.inner
            .find_latest_head_marker(conversation, at_or_before)
            .await
    }

    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<EntryRecord>, StorageError> {
        self.inner.scan_entries(query, limit, cursor).await
    }

    async fn task(&self, id: TaskId) -> Result<Option<TaskRecord>, StorageError> {
        self.inner.task(id).await
    }

    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<TaskRecord>, StorageError> {
        self.inner.scan_tasks(query, limit, cursor).await
    }

    async fn submission(&self, id: SubmissionId) -> Result<Option<SubmissionRecord>, StorageError> {
        self.inner.submission(id).await
    }

    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<SubmissionRecord>, StorageError> {
        self.inner.scan_submissions(query, limit, cursor).await
    }

    async fn submission_by_request(
        &self,
        conversation: ConversationId,
        request_id: &str,
    ) -> Result<Option<SubmissionRecord>, StorageError> {
        self.inner
            .submission_by_request(conversation, request_id)
            .await
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
    ) -> Result<Option<DocumentRecord>, StorageError> {
        self.inner.find_document(address).await
    }

    async fn document(&self, id: DocumentId) -> Result<Option<DocumentRecord>, StorageError> {
        self.inner.document(id).await
    }

    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<DocumentRecord>, StorageError> {
        self.inner.scan_documents(query, limit, cursor).await
    }

    async fn close(&self) -> Result<(), StorageError> {
        self.inner.close().await
    }
}

// ---------------------------------------------------------------- 测试环境

/// 一个"库"：存储、注册表、时钟、报告。`open` 起一个引擎；关掉它再 `open` 就是重启。
pub struct Env {
    pub storage: Arc<FlakyStorage>,
    pub registry: Registry,
    pub clock: Arc<TokioClock>,
    pub reports: Log,
}

impl Env {
    pub fn new() -> Self {
        let memory = Arc::new(MemoryStorage::new());
        Self {
            storage: FlakyStorage::new(memory),
            registry: Registry::new(),
            clock: TokioClock::new(),
            reports: Log::default(),
        }
    }

    pub fn install(&self, kind: &Arc<Kind>) {
        self.registry.install(kind.clone());
    }

    /// 打开一个引擎（不 resume）；关闭时不关存储，所以能在同一个库上重开。
    pub async fn open(&self) -> Engine {
        let reports = self.reports.clone();
        let engine = Engine::open(
            self.storage.clone(),
            self.registry.clone(),
            self.clock.clone(),
            EngineOptions {
                close_storage: false,
                report: Arc::new(move |text| reports.push(text)),
                ..EngineOptions::default()
            },
        )
        .await
        .expect("open the engine");
        engine
            .commit(|tx| {
                Box::pin(async move {
                    if tx.conversation(ROOT_CONVERSATION_ID).await?.is_none() {
                        tx.create_root_conversation().await?;
                    }
                    Ok(())
                })
            })
            .await
            .expect("root conversation");
        engine
    }

    pub async fn open_running(&self) -> Engine {
        let engine = self.open().await;
        engine.resume();
        engine
    }
}

/// 顶层任务，落在根 conversation。
pub async fn spawn(engine: &Engine, kind: &Arc<Kind>, input: Value) -> TaskId {
    let kind = kind.clone();
    engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.create_task(
                    &*kind,
                    input,
                    Ownership::Conversation,
                    TaskOptions {
                        conversation: Some(ROOT_CONVERSATION_ID),
                        ..TaskOptions::default()
                    },
                )
                .await
            })
        })
        .await
        .expect("create the task")
}

pub async fn spawn_background(engine: &Engine, kind: &Arc<Kind>, input: Value) -> TaskId {
    let kind = kind.clone();
    engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.create_task(
                    &*kind,
                    input,
                    Ownership::Conversation,
                    TaskOptions {
                        conversation: Some(ROOT_CONVERSATION_ID),
                        background: true,
                        ..TaskOptions::default()
                    },
                )
                .await
            })
        })
        .await
        .expect("create the task")
}

/// 等任务 terminal，返回记录。
pub async fn settle(engine: &Engine, id: TaskId) -> TaskRecord {
    engine.wait_for_task(id).await.expect("wait for the task")
}

pub async fn record(env: &Env, id: TaskId) -> TaskRecord {
    env.storage.task(id).await.unwrap().expect("task exists")
}

/// 让出执行直到条件成立（暂停时钟下每轮睡 1ms）。
pub async fn until(mut condition: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if condition() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    panic!("condition never became true");
}
