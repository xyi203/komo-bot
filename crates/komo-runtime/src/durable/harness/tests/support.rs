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
use komo_kernel::traits::{ChildStarted, OutputWriter, Tool, ToolOutputStore};
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
use crate::recovery::{ChildProcess, IdentityProbe, Liveness, ProcessProbe};

use super::super::hooks::{
    AgentHooks, AgentSetup, HarnessDeps, HookError, PromptSection, RunBudget, ToolEnv, ToolSet,
};
use super::super::orchestration::{LaunchMode, MonitorError, MonitorLaunch, OrchestrationHooks};
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
        StorageWrite::PurgeConversations { .. } => "purge",
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
    /// 按"最后一条用户消息含这段文字"认领的步骤（后台任务的子 conversation 与父共用一个脚本，
    /// 按到达次序出牌会互相抢）。
    routed: Mutex<Vec<(String, Step)>>,
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
            routed: Mutex::new(Vec::new()),
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

    /// 最后一条用户消息含 `needle` 的请求用这一步（先于主队列；每步只用一次）。
    pub fn push_for(&self, needle: &str, step: Step) {
        self.routed.lock().unwrap().push((needle.to_owned(), step));
    }

    pub fn reply_for(&self, needle: &str, text: &str) {
        self.push_for(needle, Step::Reply(text_reply(text)));
    }

    pub fn calls_for(&self, needle: &str, calls: &[(&str, &str, Value)]) {
        self.push_for(needle, Step::Reply(call_reply(calls)));
    }

    /// 最后一条用户消息含 `needle` 的那些请求（按到达次序）。
    pub fn requests_for(&self, needle: &str) -> Vec<Vec<Message>> {
        self.requests()
            .into_iter()
            .filter(|messages| last_user_text(messages).contains(needle))
            .collect()
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
            let last_user = last_user_text(request.messages);
            let routed = {
                let mut routed = self.routed.lock().unwrap();
                routed
                    .iter()
                    .position(|(needle, _)| last_user.contains(needle.as_str()))
                    .map(|index| routed.remove(index).1)
            };
            match routed {
                Some(step) => step,
                None => self
                    .steps
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("the script ran out of steps"),
            }
        };
        self.play(step, progress).await
    }
}

/// 请求里最后一条用户消息的文字（没有就是空）。
pub fn last_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| matches!(message, Message::User(_)))
        .map(text_of)
        .unwrap_or_default()
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
    pub execute_error: Option<ToolError>,
    /// 写完输出、返回之前让存储开始失败："输出落盘了，结果没提交就崩了"。
    pub crash: Option<Arc<RecordingStorage>>,
    /// 写完输出之后报告"起了一个子进程"（新存储的槽位据此记 pid）。
    pub child: Option<ChildStarted>,
    pub runs: AtomicUsize,
    pub prepares: AtomicUsize,
    pub input_runs: Mutex<Vec<Option<komo_kernel::types::ids::RunId>>>,
    /// `Tool::outward`（`docs/confirm.md`）。
    pub outward: bool,
    /// `Tool::exposure` 是 `Codemode`（只在脚本里调，不声明给模型）。
    pub codemode_only: bool,
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
            execute_error: None,
            crash: None,
            child: None,
            runs: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            input_runs: Mutex::new(Vec::new()),
            outward: false,
            codemode_only: false,
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

    pub fn with_child(mut self, child: ChildStarted) -> Self {
        self.child = Some(child);
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

    pub fn outward(mut self) -> Self {
        self.outward = true;
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
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        self.input_runs.lock().unwrap().push(ctx.input_run.clone());
        let run = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        self.log.push(format!("start {} #{run}", self.name));
        if let Some(stdout) = &self.stdout {
            sink.write_stdout(stdout.as_bytes())
                .await
                .map_err(|e| ToolError::Failed {
                    message: e.to_string(),
                })?;
        }
        if let Some(child) = self.child {
            sink.child_started(child);
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
        if let Some(error) = &self.execute_error {
            return Err(error.clone());
        }
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

    fn outward(&self) -> bool {
        self.outward
    }

    fn exposure(&self) -> komo_kernel::types::tool::ToolExposure {
        if self.codemode_only {
            komo_kernel::types::tool::ToolExposure::Codemode
        } else {
            komo_kernel::types::tool::ToolExposure::Direct
        }
    }
}

// ------------------------------------------------------------ 子进程探测替身

/// `ProcessProbe` 的替身：答案由测试定，并记下每次被问到的子进程。`delegate` 时交给真正的
/// [`IdentityProbe`](crate::recovery::IdentityProbe)。
#[derive(Debug)]
pub struct FakeProbe {
    answer: Mutex<Option<Liveness>>,
    asked: Mutex<Vec<ChildProcess>>,
}

impl FakeProbe {
    pub fn new(answer: Liveness) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(Some(answer)),
            asked: Mutex::new(Vec::new()),
        })
    }

    pub fn set(&self, answer: Liveness) {
        *self.answer.lock().unwrap() = Some(answer);
    }

    /// 之后的问题交给真正的探测。
    pub fn delegate(&self) {
        *self.answer.lock().unwrap() = None;
    }

    pub fn asked(&self) -> Vec<ChildProcess> {
        self.asked.lock().unwrap().clone()
    }
}

impl ProcessProbe for FakeProbe {
    fn probe(&self, child: &ChildProcess) -> Liveness {
        self.asked.lock().unwrap().push(child.clone());
        match *self.answer.lock().unwrap() {
            Some(answer) => answer,
            None => IdentityProbe.probe(child),
        }
    }
}

// ------------------------------------------------------------ 编排钩子替身

/// `OrchestrationHooks` 的替身（D5 由 gateway 实现的那个东西）：记下每次被问到的，答案由测试定；
/// 监听的"进程"只是 `running` 里的一个 id。
#[derive(Default)]
pub struct TestOrchestration {
    /// `check_effort` 被问到的 `(conversation, 已知的 alias, 档位)`。
    pub effort_checks: Mutex<Vec<(ConversationId, Option<String>, String)>>,
    /// 有值就让 `check_effort` 拒绝，文字是这个。
    pub reject_effort: Mutex<Option<String>>,
    /// `start_monitor` 收到的每份启动说明（模式、id、剩余时限）。
    pub launches: Mutex<Vec<(LaunchMode, u64, i64)>>,
    pub stops: Mutex<Vec<u64>>,
    /// gateway 眼里"还在跑"的监听进程。
    pub running: Mutex<std::collections::HashSet<u64>>,
    /// 有值就让 `Fresh` / `Resume` 起不来。
    pub fail_start: Mutex<Option<String>>,
    /// 起进程时（进程已经算起来了）停在这里，直到放行：模拟"进程起了、结果还没提交"。
    pub start_gate: Mutex<Option<Arc<Notify>>>,
}

impl TestOrchestration {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn launches(&self) -> Vec<(LaunchMode, u64, i64)> {
        self.launches.lock().unwrap().clone()
    }

    pub fn stops(&self) -> Vec<u64> {
        self.stops.lock().unwrap().clone()
    }

    /// gateway 重启了：进程都没了。
    pub fn forget_processes(&self) {
        self.running.lock().unwrap().clear();
    }
}

#[async_trait]
impl OrchestrationHooks for TestOrchestration {
    async fn check_effort(
        &self,
        conversation: ConversationId,
        model: Option<&str>,
        effort: &komo_kernel::types::model::Effort,
    ) -> Result<(), String> {
        self.effort_checks.lock().unwrap().push((
            conversation,
            model.map(str::to_owned),
            effort.as_str().to_owned(),
        ));
        match self.reject_effort.lock().unwrap().clone() {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    async fn start_monitor(&self, launch: &MonitorLaunch) -> Result<(), MonitorError> {
        let id = launch.record.id;
        self.launches
            .lock()
            .unwrap()
            .push((launch.mode, id, launch.remaining_ms));
        match launch.mode {
            LaunchMode::Recover => {
                if self.running.lock().unwrap().contains(&id) {
                    Ok(())
                } else {
                    Err(MonitorError::Interrupted("没有这个监听在跑".into()))
                }
            }
            LaunchMode::Fresh | LaunchMode::Resume => {
                if let Some(reason) = self.fail_start.lock().unwrap().clone() {
                    return Err(MonitorError::Failed(reason));
                }
                self.running.lock().unwrap().insert(id);
                let gate = self.start_gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                Ok(())
            }
        }
    }

    async fn stop_monitor(&self, id: u64) -> Result<(), MonitorError> {
        self.stops.lock().unwrap().push(id);
        self.running.lock().unwrap().remove(&id);
        Ok(())
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
    /// `komo.task` 里的 alias → 模型配置（gateway 的 `setup` 按名字解析的替身）。
    aliases: std::collections::HashMap<String, ModelConfig>,
    storage: Option<Arc<dyn Storage>>,
    /// `setup` 被问过的 conversation 与它给的缓存键。
    setups: Vec<(ConversationId, Option<String>)>,
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
            interjected: None,
            conversation: true,
            confirm_timeout: std::time::Duration::from_secs(60),
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
                aliases: std::collections::HashMap::new(),
                storage: None,
                setups: Vec::new(),
            }),
        })
    }

    /// 钩子读 `komo.task` 要的存储（引擎每次打开时换成当前的）。
    pub fn attach(&self, storage: Arc<dyn Storage>) {
        self.state.lock().unwrap().storage = Some(storage);
    }

    pub fn add_alias(&self, alias: &str, model: ModelConfig) {
        self.state
            .lock()
            .unwrap()
            .aliases
            .insert(alias.to_owned(), model);
    }

    pub fn setups(&self) -> Vec<(ConversationId, Option<String>)> {
        self.state.lock().unwrap().setups.clone()
    }

    pub fn set_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        self.state.lock().unwrap().tools = tools;
    }

    /// `ToolEnv::conversation`：`false` = 任务会话或 cron 会话。
    pub fn set_conversation(&self, conversation: bool) {
        self.state.lock().unwrap().env.conversation = conversation;
    }

    pub fn set_cwd(&self, cwd: PathBuf) {
        self.state.lock().unwrap().env.cwd = cwd;
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
        conversation: ConversationId,
        _context: &[Message],
    ) -> Result<AgentSetup, HookError> {
        // 后台任务的 conversation：`komo.task` 说它用哪个模型、fork 的缓存键用父的。
        let storage = self.state.lock().unwrap().storage.clone();
        let doc = match storage {
            Some(storage) => super::super::orchestration::task_doc(&*storage, conversation)
                .await
                .ok()
                .flatten(),
            None => None,
        };
        let mut state = self.state.lock().unwrap();
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        let Some(mut model) = state.model.clone() else {
            return Err(HookError::NoModel("No model is configured".into()));
        };
        let mut cache_key = Some("cache-1".to_owned());
        if let Some(doc) = &doc {
            if doc.fork {
                cache_key = Some(format!("conversation-{}", doc.parent));
            } else {
                if let Some(chosen) = doc
                    .model
                    .as_ref()
                    .and_then(|alias| state.aliases.get(alias))
                {
                    model = chosen.clone();
                }
                if let Some(effort) = &doc.effort {
                    model.effort = Some(effort.clone());
                }
            }
        }
        state.setups.push((conversation, cache_key.clone()));
        Ok(AgentSetup {
            sections: state.sections.clone(),
            tools: Self::tool_set(&state),
            model,
            budget: state.budget,
            cache_key,
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
    /// 工具任务 `probe` 阶段用的探测替身（默认答"已经结束"）。
    pub probe: Arc<FakeProbe>,
    /// 编排钩子替身（后台任务、监听）。
    pub orch: Arc<TestOrchestration>,
    pub log: Log,
    /// 内存输出存储（`World::new`）；`with_outputs` 给了别的存储时它是空的。
    pub mem: MemOutputStore,
    /// 引擎与读辅助实际用的存储。内存世界里就是 `storage`；Turso 世界（`World::turso`）里是
    /// 每次 `open` 重新打开的 `TursoStorage`——此时 `storage`（记录提交方式的替身）不用。
    db: Mutex<Arc<dyn Storage>>,
    /// Turso 世界的数据目录。
    turso_dir: Option<PathBuf>,
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
        let probe = FakeProbe::new(Liveness::Gone);
        let orch = TestOrchestration::new();
        let harness = Harness::new(
            HarnessDeps::new(hooks.clone(), script.clone())
                .with_probe(probe.clone())
                .with_orchestration(orch.clone()),
        );
        let registry = Registry::new();
        harness.install(&registry);
        let storage = RecordingStorage::new();
        Self {
            db: Mutex::new(storage.clone()),
            turso_dir: None,
            storage,
            registry,
            clock,
            reports: Log::default(),
            script,
            hooks,
            harness,
            probe,
            orch,
            log: Log::default(),
            mem,
        }
    }

    /// 跑在 Turso 文件库上的世界：`dir` 里是 `durable.db` 与工具输出（真文件存储）。每次
    /// `open` 都重新开库，所以 `restart` 是真的"关库、再开库"。读辅助只在引擎开着的时候能用。
    pub fn turso(dir: &std::path::Path) -> Self {
        let outputs = Arc::new(komo_store::FileToolOutputStore::new(
            komo_store::SessionPaths::at(dir),
            SessionId::from_raw("s1"),
        ));
        let mut world = Self::with_outputs(outputs);
        world.turso_dir = Some(dir.to_path_buf());
        world
    }

    fn db(&self) -> Arc<dyn Storage> {
        self.db.lock().unwrap().clone()
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

    /// 打开一个引擎并开始调度；内存世界关闭时不关存储，所以能在同一个库上重开；Turso 世界
    /// 关引擎时连库一起关，这里重新开库。
    pub async fn open(&self) -> Engine {
        let reports = self.reports.clone();
        if let Some(dir) = &self.turso_dir {
            let storage = komo_store::durable::TursoStorage::open(dir)
                .await
                .expect("open the Turso database");
            *self.db.lock().unwrap() = Arc::new(storage);
        }
        self.hooks.attach(self.db());
        let engine = Engine::open(
            self.db(),
            self.registry.clone(),
            self.clock.clone(),
            EngineOptions {
                close_storage: self.turso_dir.is_some(),
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
        self.live_of(CONV).await
    }

    pub async fn live_of(&self, conversation: ConversationId) -> LiveState {
        let db = self.db();
        Harness::live(&*db, conversation).await.expect("live")
    }

    pub async fn inbox(&self) -> super::super::InboxState {
        let db = self.db();
        Harness::inbox(&*db, CONV).await.expect("inbox")
    }

    /// 已提交的存储（读辅助；Turso 世界里引擎关着时不可用）。
    pub fn storage_handle(&self) -> Arc<dyn Storage> {
        self.db()
    }

    /// 最近派出的那个后台任务的子 conversation（所有者是锚任务的那个）。
    pub async fn child(&self) -> ConversationId {
        self.children().await.pop().expect("a spawned child")
    }

    pub async fn children(&self) -> Vec<ConversationId> {
        let page = self
            .db()
            .scan_conversations(&ConversationQuery::default(), 1000, None)
            .await
            .unwrap();
        page.items
            .into_iter()
            .filter(|record| record.owner.is_some())
            .map(|record| record.id)
            .collect()
    }

    pub async fn entries_in(&self, conversation: ConversationId) -> Vec<EntryRecord> {
        let mut all = Vec::new();
        let query = EntryQuery {
            order: Some(ScanOrder::Ascending),
            ..EntryQuery::new(conversation)
        };
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = self
                .db()
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

    /// 某个 conversation 推导出的模型消息（含它沿 fork 看到的祖先部分）。
    pub async fn messages_in(&self, conversation: ConversationId) -> Vec<Message> {
        // `scan_entries` 本来就沿 fork 祖先拼出可见的 entry。
        derive_context(&self.entries_in(conversation).await, None).messages
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
                .db()
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
        self.db().submission(id).await.unwrap().expect("submission")
    }

    pub async fn tasks(&self, kind: &str) -> Vec<TaskRecord> {
        let page = self
            .db()
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
