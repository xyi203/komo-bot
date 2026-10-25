//! codemode 嵌套调用的端到端（`docs/storage-durable.md` §4.7）：新存储的 harness 跑在 Task 引擎上，
//! 脚本在**真的** `komo __codemode-worker` 子进程里跑，里面的每个工具调用是外层任务名下的嵌套
//! 工具任务。进程内 worker 的细分用例在 `komo-runtime` 的 `durable/harness/tests/codemode.rs`。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::durable::storage::{EntryQuery, TaskQuery};
use komo_kernel::durable::{
    AssistantMessage, ContentBlock, ConversationId, EntryRecord, MemoryStorage, Message,
    ROOT_CONVERSATION_ID, ScanOrder, StopReason, Storage, TaskOutcome, TaskRecord, TaskState,
};
use komo_kernel::projection::{DEFAULT_MODEL_RESULT_BYTES, ProjectionContext};
use komo_kernel::test_support::MemOutputStore;
use komo_kernel::traits::{Clock, OutputWriter, Tool};
use komo_kernel::types::ids::{OperationId, SessionId};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::{
    ExecutionPlan, Operation, PlanSource, PlanVersions, RecoveryMode, Replay,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::resource::ResourceMounts;
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use komo_kernel::types::turn::LlmError;
use komo_runtime::codemode::Worker;
use komo_runtime::durable::harness::{
    AgentHooks, AgentSetup, Harness, HarnessDeps, HookError, PromptSection, RunBudget,
    SubmissionDraft, ToolEnv, ToolSet,
};
use komo_runtime::durable::{Engine, EngineOptions, Registry};
use komo_runtime::llm::{ModelRequest, ModelStream};
use komo_runtime::tools::{CodemodeTool, ReadTool};
use serde_json::{Value, json};
use time::OffsetDateTime;

const CONV: ConversationId = ROOT_CONVERSATION_ID;

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_komo"))
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// 脚本化的模型：第一轮要 codemode，第二轮一句话收尾。
struct Model {
    steps: Mutex<VecDeque<AssistantMessage>>,
}

impl Model {
    fn new(code: &str) -> Arc<Self> {
        let call = AssistantMessage {
            content: vec![ContentBlock::tool_call(
                "m1",
                "codemode",
                json!({ "code": code }),
            )],
            stop_reason: StopReason::ToolUse,
            timestamp: 1,
            extra: Default::default(),
        };
        let done = AssistantMessage {
            content: vec![ContentBlock::text("ok")],
            stop_reason: StopReason::Stop,
            timestamp: 1,
            extra: Default::default(),
        };
        Arc::new(Self {
            steps: Mutex::new(VecDeque::from([call, done])),
        })
    }
}

#[async_trait]
impl ModelStream for Model {
    async fn stream(
        &self,
        _request: &ModelRequest<'_>,
        _progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
    ) -> Result<AssistantMessage, LlmError> {
        Ok(self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("the script ran out of steps"))
    }
}

/// 数着自己跑了几次的工具；第一次执行可以永远挂住（"重启时正在执行"）。
struct Tick {
    runs: AtomicUsize,
    hang_first: bool,
}

#[async_trait]
impl Tool for Tick {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tick".into(),
            description: "counts its runs".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            doc: None,
        }
    }

    async fn prepare(&self, args: Value, ctx: &ToolContext) -> Result<ExecutionPlan, ToolError> {
        Ok(ExecutionPlan {
            operation_id: OperationId::from_raw(format!("op-{}", ctx.call)),
            source: ctx.source.clone(),
            tool: "tick".into(),
            operation: Operation::ReadFile,
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![],
            versions: PlanVersions::default(),
            recovery: RecoveryMode::SafeReread,
            then_run: None,
        })
    }

    async fn execute(
        &self,
        _plan: ExecutionPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let run = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        if self.hang_first && run == 1 {
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput {
            status: ToolResultStatus::Completed,
            result: Value::Null,
            exit_code: None,
            artifacts: vec![],
            images: vec![],
            preview: Some(format!("tick #{run}")),
        })
    }

    fn replay(&self) -> Replay {
        Replay::Safe
    }
}

struct Hooks {
    tools: Vec<Arc<dyn Tool>>,
    env: ToolEnv,
}

impl Hooks {
    fn tool_set(&self) -> ToolSet {
        let surface = AgentSurface::new(self.tools.iter().map(|tool| tool.definition().name));
        ToolSet::new(self.tools.clone(), surface, self.env.clone())
    }
}

#[async_trait]
impl AgentHooks for Hooks {
    async fn setup(
        &self,
        _conversation: ConversationId,
        _context: &[Message],
    ) -> Result<AgentSetup, HookError> {
        Ok(AgentSetup {
            sections: vec![PromptSection::new("preamble", "You are a test agent.")],
            tools: self.tool_set(),
            model: ModelConfig {
                provider: "responses".into(),
                base_url: "http://localhost".into(),
                model: "test-model".into(),
                api_key_env: "KEY".into(),
                auth: None,
                effort: None,
                efforts: None,
                timeout_secs: 5,
                context_window: None,
            },
            budget: RunBudget::default(),
            cache_key: None,
            max_output_tokens: None,
        })
    }

    async fn tools(&self, _conversation: ConversationId) -> Result<ToolSet, HookError> {
        Ok(self.tool_set())
    }
}

struct World {
    storage: Arc<MemoryStorage>,
    registry: Registry,
    harness: Harness,
}

impl World {
    fn new(code: &str, cwd: PathBuf, tick: Arc<Tick>) -> Self {
        let session = SessionId::from_raw("s1");
        let env = ToolEnv {
            session: session.clone(),
            source: PlanSource::Interactive { session },
            cwd,
            roots: vec![],
            mounts: ResourceMounts::default(),
            projection: ProjectionContext {
                model_result_bytes: DEFAULT_MODEL_RESULT_BYTES,
                decay: None,
            },
            call_timeout: Duration::from_secs(60),
            detach_after: None,
            conversation: true,
            outputs: Arc::new(MemOutputStore::new()),
            images: None,
        };
        let hooks = Arc::new(Hooks {
            tools: vec![
                Arc::new(CodemodeTool::new()),
                Arc::new(ReadTool::new()),
                tick,
            ],
            env,
        });
        let harness = Harness::new(
            HarnessDeps::new(hooks, Model::new(code)).with_codemode(Some(Worker::exe(exe()))),
        );
        let registry = Registry::new();
        harness.install(&registry);
        Self {
            storage: Arc::new(MemoryStorage::new()),
            registry,
            harness,
        }
    }

    async fn open(&self) -> Engine {
        let engine = Engine::open(
            self.storage.clone(),
            self.registry.clone(),
            Arc::new(SystemClock),
            EngineOptions {
                close_storage: false,
                hooks: self.harness.hooks(),
                ..EngineOptions::default()
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

    async fn tool_tasks(&self) -> Vec<TaskRecord> {
        let page = self
            .storage
            .scan_tasks(&TaskQuery::default(), 1000, None)
            .await
            .unwrap();
        page.items
            .into_iter()
            .filter(|task| task.kind == "komo.tool")
            .collect()
    }

    /// 唯一的工具结果 entry：`(status, 给模型的文字)`。
    async fn only_result(&self) -> (String, String) {
        let query = EntryQuery {
            order: Some(ScanOrder::Ascending),
            ..EntryQuery::new(CONV)
        };
        let page = self.storage.scan_entries(&query, 256, None).await.unwrap();
        let results: Vec<EntryRecord> = page
            .items
            .into_iter()
            .filter(|entry| entry.kind == "komo.tool-result")
            .collect();
        assert_eq!(results.len(), 1, "嵌套调用不进 transcript");
        let entry = &results[0];
        let Message::ToolResult(message) = &entry.model.as_ref().unwrap()[0] else {
            panic!("a tool result expected");
        };
        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        (
            entry.data.as_ref().unwrap()["status"]
                .as_str()
                .unwrap()
                .to_owned(),
            text,
        )
    }
}

fn nested(tasks: &[TaskRecord]) -> Vec<&TaskRecord> {
    let mut found: Vec<&TaskRecord> = tasks
        .iter()
        .filter(|task| task.input["kind"] == "nested")
        .collect();
    found.sort_by_key(|task| task.id);
    found
}

fn completed(task: &TaskRecord) -> bool {
    matches!(
        task.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    )
}

/// 真 worker 里的脚本读两个真文件、只把过滤后的文字带回来；两次读各是一个嵌套任务。
#[tokio::test]
async fn a_real_worker_script_reads_two_files_through_nested_tasks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.log"), "ok 1\nERROR a\nok 2\n").unwrap();
    std::fs::write(dir.path().join("b.log"), "ERROR b\nok 3\n").unwrap();
    let code = format!(
        "hits = []\nfor name in ['a.log', 'b.log']:\n    body = tools.read(path='{}/' + name)['text']\n    \
         hits += [line for line in body.splitlines() if 'ERROR' in line]\ntext('\\n'.join(hits))",
        dir.path().display()
    );
    let world = World::new(
        &code,
        dir.path().to_path_buf(),
        Arc::new(Tick {
            runs: AtomicUsize::new(0),
            hang_first: false,
        }),
    );
    let engine = world.open().await;
    let id = world
        .harness
        .submit(&engine, CONV, SubmissionDraft::input("go"))
        .await
        .unwrap();
    world.harness.wait(&engine, id).await.unwrap();

    let (status, view) = world.only_result().await;
    assert_eq!(status, "completed", "{view}");
    assert!(view.contains("ERROR a\nERROR b"), "{view}");
    assert!(!view.contains("ok 1"), "only the filtered text: {view}");
    let tasks = world.tool_tasks().await;
    assert_eq!(nested(&tasks).len(), 2);
    assert!(tasks.iter().all(completed));
    engine.close().await.unwrap();
}

/// 第二个子调用执行中"重启"（引擎关闭，真 worker 随之被杀）：第一个从槽位回放，第二个（Safe）
/// 重跑，新起的真 worker 把整段脚本重放到同样的结果。
#[tokio::test]
async fn a_killed_real_worker_is_replayed_from_the_nested_slots_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.log"), "alpha\n").unwrap();
    let code = format!(
        "a = tools.read(path='{}/a.log')['text']\nb = tools.tick()['text']\ntext(a.strip() + ' ' + b)",
        dir.path().display()
    );
    let tick = Arc::new(Tick {
        runs: AtomicUsize::new(0),
        hang_first: true,
    });
    let world = World::new(&code, dir.path().to_path_buf(), tick.clone());
    let engine = world.open().await;
    let id = world
        .harness
        .submit(&engine, CONV, SubmissionDraft::input("go"))
        .await
        .unwrap();
    for _ in 0..2000 {
        if tick.runs.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        tick.runs.load(Ordering::SeqCst),
        1,
        "the second call is in flight"
    );

    engine.close().await.unwrap();
    let engine = world.open().await;
    world.harness.wait(&engine, id).await.unwrap();

    let (status, view) = world.only_result().await;
    assert_eq!(status, "completed", "{view}");
    assert!(view.contains("alpha tick #2"), "{view}");
    assert_eq!(tick.runs.load(Ordering::SeqCst), 2);
    let tasks = world.tool_tasks().await;
    assert_eq!(nested(&tasks).len(), 2, "no third nested task");
    engine.close().await.unwrap();
}
