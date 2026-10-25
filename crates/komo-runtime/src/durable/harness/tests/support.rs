//! 测试脚手架：脚本化的 `ModelStream`、测试工具、测试钩子、记录提交方式的存储替身、"重启"。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::durable::storage::{
    ConversationQuery, DocumentQuery, EntryQuery, SessionCursor, SessionPage, SessionQuery,
    StoredEntry, SubmissionQuery, TaskQuery,
};
use komo_kernel::durable::{
    AssistantMessage, ContentBlock, ConversationId, ConversationRecord, Cursor, DocumentAddress,
    DocumentId, DocumentRecord, EntryId, EntryRecord, MemoryStorage, Message, Page,
    ROOT_CONVERSATION_ID, ScanOrder, Seq, SessionRecord, StopReason, Storage, StorageError,
    StorageWrite, SubmissionId, SubmissionRecord, TaskId, TaskRecord, derive_context,
};
use komo_kernel::projection::{DEFAULT_MODEL_RESULT_BYTES, ProjectionContext};
use komo_kernel::test_support::MemOutputStore;
use komo_kernel::traits::{OutputWriter, Tool, ToolOutputStore};
use komo_kernel::types::ids::{OperationId, SessionId};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::{
    ExecutionPlan, Operation, PlanSource, PlanVersions, RecoveryMode, Replay,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::resource::ResourceMounts;
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::{ToolContext, ToolError, ToolOutput};
use komo_kernel::types::turn::LlmError;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::durable::tests::support::{Log, TokioClock};
use crate::durable::{Engine, EngineOptions, Registry};
use crate::llm::{ModelRequest, ModelStream};

use super::super::hooks::{
    AgentHooks, AgentSetup, HarnessDeps, HookError, PromptSection, RunBudget, ToolEnv, ToolSet,
};
use super::super::{CompactionSettings, CompactionSetup, SummaryPrompt};
use super::super::{Harness, LiveState};

pub const CONV: ConversationId = ROOT_CONVERSATION_ID;

// ------------------------------------------------------------ 记录提交方式的存储

/// 一次存储提交：走的是 `commit` 还是 `commit_progress`，以及各条写入的种类。
#[derive(Debug, Clone)]
pub struct CommitRecord {
    pub progress: bool,
    pub writes: Vec<&'static str>,
}

fn kind_of(write: &StorageWrite) -> &'static str {
    match write {
        StorageWrite::Conversation(_) => "conversation",
        StorageWrite::Entry(_) => "entry",
        StorageWrite::Task(_) => "task",
        StorageWrite::Submission(_) => "submission",
        StorageWrite::DocumentCreate(_) => "document.create",
        StorageWrite::DocumentChange { .. } => "document.change",
        StorageWrite::DocumentRetire { .. } => "document.retire",
        StorageWrite::Session(_) => "session",
    }
}

/// 转发给内存存储，记下每次提交走的是哪个入口；可以让之后的提交失败（"崩溃"）。
pub struct RecordingStorage {
    pub inner: Arc<MemoryStorage>,
    pub fail_commits: AtomicBool,
    pub commits: Mutex<Vec<CommitRecord>>,
}

impl RecordingStorage {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryStorage::new()),
            fail_commits: AtomicBool::new(false),
            commits: Mutex::new(Vec::new()),
        })
    }

    fn record(&self, progress: bool, writes: &[StorageWrite]) {
        self.commits.lock().unwrap().push(CommitRecord {
            progress,
            writes: writes.iter().map(kind_of).collect(),
        });
    }

    pub fn commit_log(&self) -> Vec<CommitRecord> {
        self.commits.lock().unwrap().clone()
    }

    pub fn progress_commits(&self) -> Vec<CommitRecord> {
        self.commit_log()
            .into_iter()
            .filter(|commit| commit.progress)
            .collect()
    }
}

#[async_trait]
impl Storage for RecordingStorage {
    async fn commit(&self, writes: &[StorageWrite]) -> Result<Seq, StorageError> {
        if self.fail_commits.load(Ordering::SeqCst) {
            return Err(StorageError::Backend("disk is on fire".into()));
        }
        self.record(false, writes);
        self.inner.commit(writes).await
    }

    async fn commit_progress(&self, writes: &[StorageWrite]) -> Result<Seq, StorageError> {
        if self.fail_commits.load(Ordering::SeqCst) {
            return Err(StorageError::Backend("disk is on fire".into()));
        }
        self.record(true, writes);
        self.inner.commit_progress(writes).await
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

    async fn session(&self, id: &str) -> Result<Option<SessionRecord>, StorageError> {
        self.inner.session(id).await
    }

    async fn scan_sessions(
        &self,
        query: &SessionQuery,
        limit: usize,
        cursor: Option<&SessionCursor>,
    ) -> Result<SessionPage, StorageError> {
        self.inner.scan_sessions(query, limit, cursor).await
    }

    async fn close(&self) -> Result<(), StorageError> {
        self.inner.close().await
    }
}

// ------------------------------------------------------------ 脚本化的模型

/// 脚本里的一步。
pub enum Step {
    Reply(AssistantMessage),
    Error(LlmError),
    /// 按间隔回放部分回复，再走 `end`。
    Stream {
        partials: Vec<AssistantMessage>,
        gap: Duration,
        end: Box<Step>,
    },
    /// 等放行再走下一步。
    Gate(Arc<Notify>, Box<Step>),
    /// 永远不返回（直到被取消）。
    Hang,
}

/// 脚本化的 `ModelStream`：按次序出牌，记下每次请求收到的消息与时刻。
pub struct Script {
    steps: Mutex<VecDeque<Step>>,
    /// 摘要请求（压缩任务发的，第一条是有文字的 system 消息）走自己的队列，免得和并发的
    /// 主请求抢次序。
    summary_steps: Mutex<VecDeque<Step>>,
    summary_requests: Mutex<Vec<Vec<Message>>>,
    requests: Mutex<Vec<Vec<Message>>>,
    times: Mutex<Vec<i64>>,
    models: Mutex<Vec<ModelConfig>>,
    revised: Mutex<Vec<Option<usize>>>,
    clock: Arc<TokioClock>,
}

impl Script {
    pub fn new(clock: Arc<TokioClock>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(VecDeque::new()),
            summary_steps: Mutex::new(VecDeque::new()),
            summary_requests: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            times: Mutex::new(Vec::new()),
            models: Mutex::new(Vec::new()),
            revised: Mutex::new(Vec::new()),
            clock,
        })
    }

    pub fn push(&self, step: Step) {
        self.steps.lock().unwrap().push_back(step);
    }

    pub fn reply(&self, text: &str) {
        self.push(Step::Reply(text_reply(text)));
    }

    pub fn calls(&self, calls: &[(&str, &str, Value)]) {
        self.push(Step::Reply(call_reply(calls)));
    }

    /// 摘要请求的下一步（压缩任务的）。
    pub fn push_summary(&self, step: Step) {
        self.summary_steps.lock().unwrap().push_back(step);
    }

    pub fn summary_requests(&self) -> Vec<Vec<Message>> {
        self.summary_requests.lock().unwrap().clone()
    }

    pub fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub fn request_times(&self) -> Vec<i64> {
        self.times.lock().unwrap().clone()
    }

    /// 每次主请求带的 `revised_from`（衰减那一轮 Anthropic 缓存断点要用）。
    pub fn revised_froms(&self) -> Vec<Option<usize>> {
        self.revised.lock().unwrap().clone()
    }

    pub fn models(&self) -> Vec<ModelConfig> {
        self.models.lock().unwrap().clone()
    }

    async fn play(
        &self,
        step: Step,
        progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
    ) -> Result<AssistantMessage, LlmError> {
        let mut step = step;
        loop {
            match step {
                Step::Reply(message) => return Ok(message),
                Step::Error(error) => return Err(error),
                Step::Hang => std::future::pending().await,
                Step::Gate(gate, next) => {
                    gate.notified().await;
                    step = *next;
                }
                Step::Stream { partials, gap, end } => {
                    for partial in &partials {
                        progress(partial);
                        tokio::time::sleep(gap).await;
                    }
                    step = *end;
                }
            }
        }
    }
}

#[async_trait]
impl ModelStream for Script {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
    ) -> Result<AssistantMessage, LlmError> {
        let summary = matches!(
            request.messages.first(),
            Some(Message::System(system))
                if matches!(&system.content, komo_kernel::durable::Content::Text(text) if !text.is_empty())
        );
        let step = if summary {
            self.summary_requests
                .lock()
                .unwrap()
                .push(request.messages.to_vec());
            self.summary_steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("the script ran out of summary steps")
        } else {
            self.requests
                .lock()
                .unwrap()
                .push(request.messages.to_vec());
            self.times.lock().unwrap().push(self.clock.now_ms());
            self.models.lock().unwrap().push(request.model.clone());
            self.revised
                .lock()
                .unwrap()
                .push(request.options.revised_from);
            self.steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("the script ran out of steps")
        };
        self.play(step, progress).await
    }
}

fn usage(input: u64, output: u64) -> Value {
    json!({ "input": input, "output": output })
}

pub fn text_reply(text: &str) -> AssistantMessage {
    let mut message = AssistantMessage {
        content: vec![ContentBlock::text(text)],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        extra: Default::default(),
    };
    message.extra.insert("usage".into(), usage(100, 10));
    message
}

pub fn call_reply(calls: &[(&str, &str, Value)]) -> AssistantMessage {
    let mut message = AssistantMessage {
        content: calls
            .iter()
            .map(|(id, name, args)| ContentBlock::tool_call(*id, *name, args.clone()))
            .collect(),
        stop_reason: StopReason::ToolUse,
        timestamp: 1,
        extra: Default::default(),
    };
    message.extra.insert("usage".into(), usage(100, 10));
    message
}

pub fn partial(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::text(text)],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        extra: Default::default(),
    }
}

pub fn rejected(status: u16) -> LlmError {
    LlmError::Rejected {
        status,
        message: format!("HTTP {status}"),
    }
}

// ------------------------------------------------------------ 测试工具

/// 一个可调行为的工具：写一点 stdout、等一等、挂住、"崩溃"。
pub struct TestTool {
    pub name: &'static str,
    pub operation: Operation,
    pub recovery: RecoveryMode,
    /// `Tool::replay`：工具**现在**声明的（可以在两次"开库"之间改）。
    pub replay: Mutex<Replay>,
    pub stdout: Option<String>,
    pub text: String,
    pub delay: Duration,
    pub gate: Option<Arc<Notify>>,
    /// 前几次执行永远不返回（直到被取消）。
    pub hang_runs: usize,
    /// 前几次 `prepare` 永远不返回。
    pub hang_prepares: AtomicUsize,
    pub fail_prepare: Option<String>,
    /// 写完输出、返回之前让存储开始失败："输出落盘了，结果没提交就崩了"。
    pub crash: Option<Arc<RecordingStorage>>,
    pub runs: AtomicUsize,
    pub prepares: AtomicUsize,
    pub log: Log,
}

impl TestTool {
    /// 只读、重放安全（`read` / `rg` 那一类）。
    pub fn reader(name: &'static str, log: &Log) -> Self {
        Self {
            name,
            operation: Operation::ReadFile,
            recovery: RecoveryMode::SafeReread,
            replay: Mutex::new(Replay::Safe),
            stdout: None,
            text: format!("{name} output"),
            delay: Duration::ZERO,
            gate: None,
            hang_runs: 0,
            hang_prepares: AtomicUsize::new(0),
            fail_prepare: None,
            crash: None,
            runs: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            log: log.clone(),
        }
    }

    /// 有副作用、重放不安全（`bash` / `write` 那一类）。
    pub fn writer(name: &'static str, log: &Log) -> Self {
        Self {
            operation: Operation::ShellCommand {
                command: "do it".into(),
            },
            recovery: RecoveryMode::NoSafeRecovery,
            replay: Mutex::new(Replay::Unsafe),
            ..Self::reader(name, log)
        }
    }

    pub fn with_text(mut self, text: &str) -> Self {
        self.text = text.to_owned();
        self
    }

    pub fn with_stdout(mut self, stdout: &str) -> Self {
        self.stdout = Some(stdout.to_owned());
        self
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn with_gate(mut self, gate: &Arc<Notify>) -> Self {
        self.gate = Some(gate.clone());
        self
    }

    pub fn hanging_first(mut self, runs: usize) -> Self {
        self.hang_runs = runs;
        self
    }

    pub fn crashing(mut self, storage: &Arc<RecordingStorage>) -> Self {
        self.crash = Some(storage.clone());
        self
    }

    pub fn failing_prepare(mut self, message: &str) -> Self {
        self.fail_prepare = Some(message.to_owned());
        self
    }

    pub fn hanging_first_prepares(self, prepares: usize) -> Self {
        self.hang_prepares.store(prepares, Ordering::SeqCst);
        self
    }

    pub fn set_replay(&self, replay: Replay) {
        *self.replay.lock().unwrap() = replay;
    }

    pub fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Tool for TestTool {
    fn definition(&self) -> komo_kernel::types::tool::ToolDefinition {
        komo_kernel::types::tool::ToolDefinition {
            name: self.name.to_owned(),
            description: format!("the {} tool", self.name),
            parameters: json!({ "type": "object", "properties": {} }),
            doc: None,
        }
    }

    async fn prepare(&self, args: Value, ctx: &ToolContext) -> Result<ExecutionPlan, ToolError> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        self.log.push(format!("prepare {}", self.name));
        let remaining = self.hang_prepares.load(Ordering::SeqCst);
        if remaining > 0 {
            self.hang_prepares.store(remaining - 1, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
        if let Some(message) = &self.fail_prepare {
            return Err(ToolError::Failed {
                message: message.clone(),
            });
        }
        Ok(ExecutionPlan {
            operation_id: OperationId::from_raw(format!("op-{}", ctx.call)),
            source: ctx.source.clone(),
            tool: self.name.to_owned(),
            operation: self.operation.clone(),
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![],
            versions: PlanVersions::default(),
            recovery: self.recovery.clone(),
            then_run: None,
        })
    }

    async fn execute(
        &self,
        _plan: ExecutionPlan,
        _ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let run = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        self.log.push(format!("start {} #{run}", self.name));
        if let Some(stdout) = &self.stdout {
            sink.write_stdout(stdout.as_bytes())
                .await
                .map_err(|e| ToolError::Failed {
                    message: e.to_string(),
                })?;
        }
        if run <= self.hang_runs {
            std::future::pending::<()>().await;
        }
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if let Some(storage) = &self.crash {
            storage.fail_commits.store(true, Ordering::SeqCst);
        }
        self.log.push(format!("finish {} #{run}", self.name));
        Ok(ToolOutput {
            status: ToolResultStatus::Completed,
            result: Value::Null,
            exit_code: Some(0),
            artifacts: vec![],
            images: vec![],
            preview: Some(self.text.clone()),
        })
    }

    fn replay(&self) -> Replay {
        *self.replay.lock().unwrap()
    }
}

// ------------------------------------------------------------ 测试钩子

pub fn model_config() -> ModelConfig {
    ModelConfig {
        provider: "responses".into(),
        base_url: "http://localhost".into(),
        model: "test-model".into(),
        api_key_env: "KEY".into(),
        auth: None,
        effort: None,
        efforts: None,
        timeout_secs: 5,
        context_window: None,
    }
}

struct HookState {
    sections: Vec<PromptSection>,
    tools: Vec<Arc<dyn Tool>>,
    surface: Option<Vec<String>>,
    model: Option<ModelConfig>,
    budget: RunBudget,
    env: ToolEnv,
    failure: Option<HookError>,
    tools_failure: Option<HookError>,
    /// 压缩钩子的答案（设置、摘要提示）；没设就走 trait 的默认（压缩关着）。
    compaction: Option<CompactionSettings>,
    compaction_enabled: bool,
    /// 先放过这么多次 `compaction()`，再失败一次。
    compaction_failure: Option<(usize, HookError)>,
}

/// 测试里的钩子：D5 由 gateway 实现的那个东西的替身。每个决定点都按当前内容重新答。
pub struct TestHooks {
    state: Mutex<HookState>,
}

impl TestHooks {
    pub fn new(outputs: Arc<dyn ToolOutputStore>) -> Arc<Self> {
        let session = SessionId::from_raw("s1");
        let env = ToolEnv {
            session: session.clone(),
            source: PlanSource::Interactive { session },
            cwd: PathBuf::from("/tmp/komo-test"),
            roots: vec![],
            mounts: ResourceMounts::default(),
            projection: ProjectionContext {
                model_result_bytes: DEFAULT_MODEL_RESULT_BYTES,
                decay: None,
            },
            call_timeout: Duration::from_secs(60),
            detach_after: None,
            conversation: true,
            outputs,
            images: None,
        };
        Arc::new(Self {
            state: Mutex::new(HookState {
                sections: vec![PromptSection::new("preamble", "You are a test agent.")],
                tools: Vec::new(),
                surface: None,
                model: Some(model_config()),
                budget: RunBudget::default(),
                env,
                failure: None,
                tools_failure: None,
                compaction: None,
                compaction_enabled: false,
                compaction_failure: None,
            }),
        })
    }

    pub fn set_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        self.state.lock().unwrap().tools = tools;
    }

    pub fn set_surface(&self, surface: Option<Vec<&str>>) {
        self.state.lock().unwrap().surface =
            surface.map(|names| names.into_iter().map(str::to_owned).collect());
    }

    pub fn set_sections(&self, sections: Vec<PromptSection>) {
        self.state.lock().unwrap().sections = sections;
    }

    pub fn set_model(&self, model: Option<ModelConfig>) {
        self.state.lock().unwrap().model = model;
    }

    pub fn set_budget(&self, budget: RunBudget) {
        self.state.lock().unwrap().budget = budget;
    }

    /// 打开在线压缩并给设置（没调就是 trait 默认：压缩关着、旋钮取默认值）。
    pub fn set_compaction(&self, enabled: bool, settings: CompactionSettings) {
        let mut state = self.state.lock().unwrap();
        state.compaction_enabled = enabled;
        state.compaction = Some(settings);
    }

    /// 工具结果的投影设置（衰减策略在里面）。
    pub fn set_projection(&self, projection: ProjectionContext) {
        self.state.lock().unwrap().env.projection = projection;
    }

    /// 放过 `skip` 次 `compaction()` 之后失败一次（`setup()` 不受影响）：生成的每次 `prepare`
    /// 都问一次，所以跳过那几次就能让之后问的压缩任务 faulted。
    pub fn fail_compaction_after(&self, skip: usize, failure: HookError) {
        self.state.lock().unwrap().compaction_failure = Some((skip, failure));
    }

    pub fn set_failure(&self, failure: Option<HookError>) {
        self.state.lock().unwrap().failure = failure;
    }

    /// 下一次 `tools()` 失败一次（`setup()` 不受影响）：让工具任务 faulted，而生成照常继续。
    pub fn fail_tools_once(&self, failure: HookError) {
        self.state.lock().unwrap().tools_failure = Some(failure);
    }

    fn tool_set(state: &HookState) -> ToolSet {
        let surface = match &state.surface {
            Some(names) => AgentSurface::new(names.clone()),
            None => AgentSurface::new(state.tools.iter().map(|tool| tool.definition().name)),
        };
        ToolSet::new(state.tools.clone(), surface, state.env.clone())
    }
}

#[async_trait]
impl AgentHooks for TestHooks {
    async fn setup(
        &self,
        _conversation: ConversationId,
        _context: &[Message],
    ) -> Result<AgentSetup, HookError> {
        let state = self.state.lock().unwrap();
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        let Some(model) = state.model.clone() else {
            return Err(HookError::NoModel("No model is configured".into()));
        };
        Ok(AgentSetup {
            sections: state.sections.clone(),
            tools: Self::tool_set(&state),
            model,
            budget: state.budget,
            cache_key: Some("cache-1".into()),
            max_output_tokens: None,
        })
    }

    async fn compaction(&self, conversation: ConversationId) -> Result<CompactionSetup, HookError> {
        let setup = self.setup(conversation, &[]).await?;
        let mut state = self.state.lock().unwrap();
        match state.compaction_failure.take() {
            Some((0, failure)) => return Err(failure),
            Some((skip, failure)) => state.compaction_failure = Some((skip - 1, failure)),
            None => {}
        }
        Ok(CompactionSetup {
            enabled: state.compaction_enabled,
            settings: state.compaction.clone().unwrap_or_default(),
            model: setup.model,
            prompt: SummaryPrompt::default(),
            retry: setup.budget.retry,
            cache_key: setup.cache_key,
        })
    }

    async fn tools(&self, _conversation: ConversationId) -> Result<ToolSet, HookError> {
        let mut state = self.state.lock().unwrap();
        if let Some(failure) = state.tools_failure.take() {
            return Err(failure);
        }
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        Ok(Self::tool_set(&state))
    }
}

// ------------------------------------------------------------ 世界

/// 一个"库"：存储、注册表、时钟、脚本、钩子。`open` 起一个引擎；关掉它再 `open` 就是重启。
pub struct World {
    pub storage: Arc<RecordingStorage>,
    pub registry: Registry,
    pub clock: Arc<TokioClock>,
    pub reports: Log,
    pub script: Arc<Script>,
    pub hooks: Arc<TestHooks>,
    pub harness: Harness,
    pub log: Log,
    /// 内存输出存储（`World::new`）；`with_outputs` 给了别的存储时它是空的。
    pub mem: MemOutputStore,
}

impl World {
    pub fn new() -> Self {
        let mem = MemOutputStore::new();
        Self::build(Arc::new(mem.clone()), mem)
    }

    pub fn with_outputs(outputs: Arc<dyn ToolOutputStore>) -> Self {
        Self::build(outputs, MemOutputStore::new())
    }

    fn build(outputs: Arc<dyn ToolOutputStore>, mem: MemOutputStore) -> Self {
        let clock = TokioClock::new();
        let script = Script::new(clock.clone());
        let hooks = TestHooks::new(outputs);
        let harness = Harness::new(HarnessDeps::new(hooks.clone(), script.clone()));
        let registry = Registry::new();
        harness.install(&registry);
        Self {
            storage: RecordingStorage::new(),
            registry,
            clock,
            reports: Log::default(),
            script,
            hooks,
            harness,
            log: Log::default(),
            mem,
        }
    }

    /// 装上工具（返回同一批，方便测试读计数）。
    pub fn tools(&self, tools: Vec<Arc<TestTool>>) {
        self.hooks.set_tools(
            tools
                .into_iter()
                .map(|tool| tool as Arc<dyn Tool>)
                .collect(),
        );
    }

    /// 打开一个引擎并开始调度；关闭时不关存储，所以能在同一个库上重开。
    pub async fn open(&self) -> Engine {
        let reports = self.reports.clone();
        let engine = Engine::open(
            self.storage.clone(),
            self.registry.clone(),
            self.clock.clone(),
            EngineOptions {
                close_storage: false,
                report: Arc::new(move |text| reports.push(text)),
                hooks: self.harness.hooks(),
            },
        )
        .await
        .expect("open the engine");
        engine
            .commit(|tx| {
                Box::pin(async move {
                    if tx.conversation(CONV).await?.is_none() {
                        tx.create_root_conversation().await?;
                    }
                    Ok(())
                })
            })
            .await
            .expect("root conversation");
        engine.resume();
        engine
    }

    pub async fn restart(&self, engine: Engine) -> Engine {
        engine.close().await.expect("close the engine");
        self.storage.fail_commits.store(false, Ordering::SeqCst);
        self.open().await
    }

    pub async fn submit(&self, engine: &Engine, text: &str) -> SubmissionId {
        self.harness
            .submit(engine, CONV, super::super::SubmissionDraft::input(text))
            .await
            .expect("submit")
    }

    pub async fn live(&self) -> LiveState {
        Harness::live(&*self.storage, CONV).await.expect("live")
    }

    pub async fn entries(&self) -> Vec<EntryRecord> {
        let mut all = Vec::new();
        let query = EntryQuery {
            order: Some(ScanOrder::Ascending),
            ..EntryQuery::new(CONV)
        };
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = self
                .storage
                .scan_entries(&query, 256, cursor.as_ref())
                .await
                .unwrap();
            all.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => return all,
            }
        }
    }

    pub async fn messages(&self) -> Vec<Message> {
        derive_context(&self.entries().await, None).messages
    }

    pub async fn entries_of(&self, kind: &str) -> Vec<EntryRecord> {
        self.entries()
            .await
            .into_iter()
            .filter(|entry| entry.kind == kind)
            .collect()
    }

    pub async fn submission(&self, id: SubmissionId) -> SubmissionRecord {
        self.storage
            .submission(id)
            .await
            .unwrap()
            .expect("submission")
    }

    pub async fn tasks(&self, kind: &str) -> Vec<TaskRecord> {
        let page = self
            .storage
            .scan_tasks(&TaskQuery::default(), 1000, None)
            .await
            .unwrap();
        page.items
            .into_iter()
            .filter(|task| task.kind == kind)
            .collect()
    }

    /// 等到条件成立（暂停时钟下每轮睡 1ms；条件里可以读存储）。
    pub async fn until<F, Fut>(&self, mut condition: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..20_000 {
            if condition().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("condition never became true");
    }

    pub async fn until_log(&self, line: &str) {
        let log = self.log.clone();
        let line = line.to_owned();
        self.until(|| {
            let found = log.lines().contains(&line);
            async move { found }
        })
        .await;
    }
}

/// 模型消息的文字，便于断言。
pub fn text_of(message: &Message) -> String {
    match message {
        Message::User(user) => match &user.content {
            komo_kernel::durable::Content::Text(text) => text.clone(),
            komo_kernel::durable::Content::Blocks(blocks) => blocks_text(blocks),
        },
        Message::Assistant(assistant) => blocks_text(&assistant.content),
        Message::ToolResult(result) => blocks_text(&result.content),
        Message::System(_) => String::new(),
    }
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `role: text` 的一行行摘要（system 消息写 `system`）。
pub fn outline(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| match message {
            Message::User(_) => format!("user: {}", text_of(message)),
            Message::Assistant(assistant) => {
                let calls: Vec<String> = assistant
                    .tool_calls()
                    .map(|(id, name)| format!("{name}({id})"))
                    .collect();
                if calls.is_empty() {
                    format!("assistant: {}", text_of(message))
                } else {
                    format!("assistant calls {}", calls.join(","))
                }
            }
            Message::ToolResult(result) => format!("result({})", result.tool_call_id),
            Message::System(_) => "system".to_owned(),
        })
        .collect()
}
