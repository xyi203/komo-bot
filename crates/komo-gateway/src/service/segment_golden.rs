//! 上下文装配的 golden：同一组事件、正文与配置，交给模型的系统提示与消息**逐字**不变。
//!
//! 这是 `docs/agent.md` Phase 0 锁下的基线：装配从 Gateway 搬进 `komo-agent` 前后，
//! 这里的输出必须逐字节相同。要刷新基线（**只在有意改变提示时**）：
//!
//! ```text
//! KOMO_UPDATE_GOLDEN=1 cargo test -p komo-gateway segment::golden
//! ```

use super::*;
use komo_agent::context::history::{self, ReplayScope};
use komo_agent::context::{ContextInput, InvocationContext, assemble};
use komo_agent::skills::SkillRegistry;
use komo_kernel::events::{ConversationBoundary, MessageAssistant, RunCompleted, RunStarted};
use komo_kernel::test_support::{MemOutputStore, MemOutputWriter};
use komo_kernel::types::delegate::{DelegateSpec, SchemaMode};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{AttemptId, EventId, ExecutorId, MemoryId, RequestKey};
use komo_kernel::types::memory::{
    Confirmation, ExtractionMetadata, MemoryItem, MemoryKind, MemoryScope, MemoryState, Provenance,
};
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::refs::{
    AttemptRef, ContentRef, PayloadRef, ToolResultBody, ToolResultStatus,
};
use komo_kernel::types::turn::ToolCallRequest;

const CWD: &str = "/work/komo";
const NOW: time::OffsetDateTime = time::macros::datetime!(2026-09-16 08:00:00 UTC);

/// 一份装配用的全部事实。
struct Case {
    run: RunId,
    events: Vec<Event>,
    payloads: PayloadStore,
    outputs: Option<MemOutputStore>,
    model_result_bytes: usize,
    tools: Vec<ToolDefinition>,
    instructions: Option<&'static str>,
    skills: Option<tempfile::TempDir>,
    memories: Vec<MemoryItem>,
}

impl Case {
    fn new(run: &str) -> Self {
        Case {
            run: RunId::from_raw(run),
            events: Vec::new(),
            payloads: payloads(),
            outputs: None,
            model_result_bytes: 8 * 1024,
            tools: tools(&["read", "rg", "shell"]),
            instructions: None,
            skills: None,
            memories: Vec::new(),
        }
    }
}

/// 按生产路径装配一份上下文，渲染成一段可比较的文本。
///
/// 这是 `segment()` 走的**同一条路**：`context_sources` 的取数函数 + 唯一的装配入口
/// `komo_agent::context::assemble`（`docs/agent.md` §20 Phase 0 那句"场景与基线文件都
/// 不动，只改这一个函数怎么装配"）。
async fn render(case: &Case) -> String {
    let surface = fold(&case.events);
    let delegate = surface
        .runs
        .get(&case.run)
        .and_then(|view| view.delegate.clone());
    let thread = delegate
        .as_ref()
        .map(|_| history::delegate_thread(&surface, &case.run));
    let scope = match &thread {
        Some(chain) => ReplayScope::Thread(chain),
        None => ReplayScope::Conversation(&case.run),
    };
    let tool_names: Vec<String> = case.tools.iter().map(|tool| tool.name.clone()).collect();
    // 子代理不看目录（§12）；主 Agent 才读一次活注册表。
    let skills = match (&delegate, &case.skills) {
        (None, Some(dir)) => {
            let registry =
                std::sync::RwLock::new(SkillRegistry::new(vec![dir.path().to_path_buf()]));
            context_sources::skill_catalog(Some(&registry), &tool_names)
        }
        _ => None,
    };
    let invocation = match &delegate {
        Some(spec) => InvocationContext::Delegated(spec.clone()),
        None => InvocationContext::Main,
    };

    let selected = history::entries(&surface, scope);
    let resolved = context_sources::resolve_history(
        selected,
        &case.payloads,
        case.outputs
            .as_ref()
            .map(|outputs| outputs as &dyn ToolOutputStore),
    )
    .await
    .expect("装配得出来");

    // 记忆段与生产路径同一个渲染函数（`MemoryManager` 钉住的正是它的输出，§13.2）。
    let memory = komo_agent::context::memory::render(&case.memories, 1_000).text;
    let context = assemble(ContextInput {
        instructions: case.instructions.map(str::to_string),
        workspace: std::path::PathBuf::from(CWD),
        tools: tool_names,
        history: resolved,
        memory,
        skills,
        tasks: None,
        invocation,
        model_result_bytes: case.model_result_bytes,
    });
    let prompt = context.system_prompt;

    let mut out = String::new();
    out.push_str("=== system ===\n");
    out.push_str(&prompt);
    out.push_str("\n=== messages ===\n");
    out.push_str(&serde_json::to_string_pretty(&context.messages).expect("序列化"));
    out.push('\n');
    match &case.skills {
        Some(dir) => out.replace(&dir.path().display().to_string(), "<SKILLS>"),
        None => out,
    }
}

fn check(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/service/golden")
        .join(format!("{name}.txt"));
    if std::env::var_os("KOMO_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读不到 {}：{error}", path.display()));
    assert!(
        expected == actual,
        "{name} 与基线不同。\n--- 基线 ---\n{expected}\n--- 现在 ---\n{actual}"
    );
}

// ---------------------------------------------------------------- 场景

/// 最普通的一次：一句输入，没有指令、skills 与记忆。
#[tokio::test]
async fn golden_plain_main_agent() {
    let mut case = Case::new("run-1");
    case.events = vec![
        accepted(&case.run, 1, "看一下 a.txt"),
        started(&case.run, 2),
    ];
    check("plain_main_agent", &render(&case).await);
}

/// 一段没有工具的 Run：提示里要说"这一段没有工具"。
#[tokio::test]
async fn golden_main_agent_without_tools() {
    let mut case = Case::new("run-1");
    case.tools = Vec::new();
    case.events = vec![accepted(&case.run, 1, "你好"), started(&case.run, 2)];
    check("main_agent_without_tools", &render(&case).await);
}

/// 什么都有：身份指令、komo 自查段、skills（其中一条被工具门控掉）、记忆、`/new` 之前的旧话、
/// 历史 Run 的折叠、交错落盘、窗口外损坏的正文、外置的输入、截断的工具结果与产物、
/// 读不回 `output.json` 时退回账本预览的失败结果。
#[tokio::test]
async fn golden_full_main_agent() {
    let mut case = Case::new("run-2");
    case.instructions = Some("  你是 coder。\n只改 Rust，不碰前端。\n");
    case.model_result_bytes = 96;
    case.skills = Some(skills_dir());
    case.memories = vec![
        memory(
            "m-1",
            "用户偏好中文回答",
            Provenance::UserStatement,
            Confirmation::UserConfirmed,
            MemoryState::Active,
        ),
        memory(
            "m-2",
            "项目用 cargo nextest",
            Provenance::ToolObservation,
            Confirmation::Unconfirmed,
            MemoryState::Active,
        ),
        memory(
            "m-3",
            "用户可能在迁移 CI",
            Provenance::ModelInference,
            Confirmation::Unconfirmed,
            MemoryState::Candidate,
        ),
    ];

    let old = RunId::from_raw("run-0");
    let first = RunId::from_raw("run-1");
    let second = case.run.clone();
    let queued = RunId::from_raw("run-3");
    let c1 = ToolCallId::from_raw("call-1");
    let c2 = ToolCallId::from_raw("call-2");
    let c3 = ToolCallId::from_raw("call-3");

    // 窗口外（历史 Run 的中间一轮）有一条读不出来的外置正文：它不该被读。
    let missing = payloads().put("不在这个目录里".as_bytes()).await.unwrap();
    // 当前 Run 的输入超限，外置。
    let big = "把 segment.rs 里的装配搬走，".repeat(300);
    let big_ref = case.payloads.put(big.as_bytes()).await.unwrap();

    let outputs = MemOutputStore::new();
    let long = (1..=40)
        .map(|n| format!("第 {n} 行输出"))
        .collect::<Vec<_>>()
        .join("\n");
    let published = publish(
        &outputs,
        &second,
        &c2,
        "attempt-2",
        ToolResultBody {
            status: ToolResultStatus::Completed,
            result: serde_json::json!({ "ok": true }),
            error: None,
            exit_code: Some(0),
            artifacts: vec![ContentRef {
                path: "artifacts/run-2/报告.md".into(),
                size: 13,
                hash: ContentHash::of_str("产物正文\n"),
                pointer: None,
            }],
            preview: Some(long),
        },
    )
    .await;
    case.outputs = Some(outputs);

    case.events = vec![
        accepted(&old, 1, "这句在 /new 之前"),
        started(&old, 2),
        assistant(&old, 3, Some("旧的回答"), vec![], None),
        completed(&old, 4),
        boundary(5),
        accepted(&first, 6, "先查一下 db 里有哪些表"),
        started(&first, 7),
        assistant(
            &first,
            8,
            None,
            vec![call(&c1, "pc-1", "shell")],
            Some(serde_json::json!([{"type": "reasoning"}])),
        ),
        // 当前 Run 的输入在历史 Run 半轮中间落盘。
        accepted_ref(&second, 9, big_ref),
        // 还没被领走的 Run：整个不进转写。
        accepted(&queued, 10, "这句还没轮到"),
        result(
            &first,
            11,
            &c1,
            "attempt-1",
            ToolResultStatus::Completed,
            Some("orders\nusers".into()),
            None,
        ),
        assistant_ref(&first, 12, missing),
        assistant(
            &first,
            13,
            Some("有 orders 和 users 两张表。"),
            vec![],
            Some(serde_json::json!([{"type": "message"}])),
        ),
        completed(&first, 14),
        started(&second, 15),
        assistant(
            &second,
            16,
            Some("我先跑一下测试。"),
            vec![call(&c2, "pc-2", "shell"), call(&c3, "pc-3", "read")],
            Some(serde_json::json!([{"type": "thinking", "thinking": "…", "signature": "sig"}])),
        ),
        result(
            &second,
            17,
            &c2,
            "attempt-2",
            ToolResultStatus::Completed,
            published.preview.clone(),
            Some(published.output.clone()),
        ),
        result(
            &second,
            18,
            &c3,
            "attempt-3",
            ToolResultStatus::Failed,
            Some("没有这个文件".into()),
            None,
        ),
    ];
    check("full_main_agent", &render(&case).await);
}

/// 子代理：没有契约，只看得见自己那条 Run；父的指令跟着它走。
#[tokio::test]
async fn golden_subagent_free_text() {
    let mut case = Case::new("run-child");
    case.instructions = Some("你是 coder。");
    let parent = RunId::from_raw("run-parent");
    let spec = DelegateSpec::new(
        parent.clone(),
        ToolCallId::from_raw("call-9"),
        "看一下这个 PR 的测试",
    );
    case.tools = tools(&["read", "rg"]);
    case.events = vec![
        accepted(&parent, 1, "父的输入"),
        started(&parent, 2),
        accepted_delegate(&case.run, 3, "看一下这个 PR 的测试", spec),
        started(&case.run, 4),
        assistant(&case.run, 5, Some("子代理的过程"), vec![], None),
    ];
    check("subagent_free_text", &render(&case).await);
}

/// 子代理：带结果契约，schema 原样进提示。
#[tokio::test]
async fn golden_subagent_with_contract() {
    let mut case = Case::new("run-child");
    let parent = RunId::from_raw("run-parent");
    let spec = DelegateSpec::new(
        parent.clone(),
        ToolCallId::from_raw("call-9"),
        "列出失败的测试",
    )
    .with_contract(
        serde_json::json!({
            "type": "object",
            "properties": { "failed": { "type": "array", "items": { "type": "string" } } },
            "required": ["failed"]
        }),
        SchemaMode::Strict,
    );
    case.events = vec![
        accepted(&parent, 1, "父的输入"),
        started(&parent, 2),
        accepted_delegate(&case.run, 3, "列出失败的测试", spec),
        started(&case.run, 4),
    ];
    check("subagent_with_contract", &render(&case).await);
}

// ---------------------------------------------------------------- 夹具

fn payloads() -> PayloadStore {
    let dir = tempfile::tempdir().expect("临时目录");
    PayloadStore::new(komo_store::SessionPaths::at(dir.keep()))
}

fn tools(names: &[&str]) -> Vec<ToolDefinition> {
    names
        .iter()
        .map(|name| ToolDefinition {
            name: (*name).into(),
            description: format!("{name} 工具"),
            parameters: serde_json::json!({}),
        })
        .collect()
}

fn skills_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, body: &str| {
        std::fs::create_dir_all(dir.path().join(name)).unwrap();
        std::fs::write(dir.path().join(name).join("SKILL.md"), body).unwrap();
    };
    write(
        "pr-review",
        "---\nname: pr-review\ndescription: 怎么审一个 PR\n---\n正文",
    );
    write(
        "db-query",
        "---\nname: db-query\ndescription: 查线上库\nrequires_tools: [psql]\n---\n正文",
    );
    write(
        "log-diagnosis",
        "---\nname: log-diagnosis\ndescription: 看日志找原因\n---\n正文",
    );
    dir
}

fn memory(
    id: &str,
    content: &str,
    provenance: Provenance,
    confirmation: Confirmation,
    state: MemoryState,
) -> MemoryItem {
    MemoryItem {
        id: MemoryId::from_raw(id),
        revision: 2,
        content: content.into(),
        kind: MemoryKind::Preference,
        scope: MemoryScope::Personal,
        provenance,
        confirmation,
        state,
        evidence: vec![],
        observed_at: NOW,
        valid_until: None,
        created_at: NOW,
        updated_at: NOW,
        extraction: ExtractionMetadata::new("memory-model", None, "v1"),
        usage: Default::default(),
        supersedes: None,
    }
}

fn event(seq: u64, run: Option<&RunId>, payload: EventPayload) -> Event {
    Event {
        v: 1,
        seq: Seq(seq),
        event_id: EventId::from_raw(format!("evt-{seq}")),
        session: SessionId::from_raw("sess-1"),
        run: run.cloned(),
        ts: NOW,
        payload,
    }
}

fn accepted_with(
    run: &RunId,
    seq: u64,
    text: Option<&str>,
    text_ref: Option<PayloadRef>,
    delegate: Option<DelegateSpec>,
) -> Event {
    event(
        seq,
        Some(run),
        EventPayload::RunAccepted(komo_kernel::events::RunAccepted {
            request_key: RequestKey::new(format!("key-{seq}")),
            input_hash: ContentHash::of_str(text.unwrap_or_default()),
            text: text.map(str::to_string),
            text_ref,
            source: PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            peer: None,
            model: None,
            effort: None,
            delegate,
            snapshot: None,
        }),
    )
}

fn accepted(run: &RunId, seq: u64, text: &str) -> Event {
    accepted_with(run, seq, Some(text), None, None)
}

fn accepted_ref(run: &RunId, seq: u64, text_ref: PayloadRef) -> Event {
    accepted_with(run, seq, None, Some(text_ref), None)
}

fn accepted_delegate(run: &RunId, seq: u64, text: &str, spec: DelegateSpec) -> Event {
    accepted_with(run, seq, Some(text), None, Some(spec))
}

fn started(run: &RunId, seq: u64) -> Event {
    event(
        seq,
        Some(run),
        EventPayload::RunStarted(RunStarted {
            executor: ExecutorId::from_raw("exec-1"),
            generation: 1,
        }),
    )
}

fn completed(run: &RunId, seq: u64) -> Event {
    event(
        seq,
        Some(run),
        EventPayload::RunCompleted(RunCompleted {
            final_message: None,
            final_message_ref: None,
            rounds: 1,
        }),
    )
}

fn boundary(seq: u64) -> Event {
    event(
        seq,
        None,
        EventPayload::ConversationBoundary(ConversationBoundary { by: None }),
    )
}

fn call(id: &ToolCallId, provider: &str, tool: &str) -> ToolCallRequest {
    ToolCallRequest {
        call_id: id.clone(),
        provider_call_id: provider.into(),
        name: tool.into(),
        arguments: serde_json::json!({ "command": "cargo test" }),
        arguments_ref: None,
    }
}

fn assistant(
    run: &RunId,
    seq: u64,
    text: Option<&str>,
    calls: Vec<ToolCallRequest>,
    blocks: Option<serde_json::Value>,
) -> Event {
    event(
        seq,
        Some(run),
        EventPayload::MessageAssistant(MessageAssistant {
            round: seq as u32,
            text: text.map(str::to_string),
            text_ref: None,
            tool_calls: calls,
            provider_blocks: blocks,
            input_tokens: None,
            output_tokens: None,
        }),
    )
}

fn assistant_ref(run: &RunId, seq: u64, text_ref: PayloadRef) -> Event {
    event(
        seq,
        Some(run),
        EventPayload::MessageAssistant(MessageAssistant {
            round: seq as u32,
            text: None,
            text_ref: Some(text_ref),
            tool_calls: vec![],
            provider_blocks: None,
            input_tokens: None,
            output_tokens: None,
        }),
    )
}

async fn publish(
    outputs: &MemOutputStore,
    run: &RunId,
    call: &ToolCallId,
    attempt: &str,
    body: ToolResultBody,
) -> komo_kernel::types::refs::PublishedOutput {
    let writer = MemOutputWriter::new(AttemptRef {
        session: SessionId::from_raw("sess-1"),
        run: run.clone(),
        call: call.clone(),
        attempt: AttemptId::from_raw(attempt),
    });
    outputs
        .publish(Box::new(writer), body)
        .await
        .expect("发布输出")
}

fn result(
    run: &RunId,
    seq: u64,
    call: &ToolCallId,
    attempt: &str,
    status: ToolResultStatus,
    preview: Option<String>,
    output: Option<komo_kernel::types::refs::OutputRef>,
) -> Event {
    let output = output.unwrap_or_else(|| {
        komo_kernel::types::refs::OutputRef(ContentRef {
            path: format!("tool-output/{run}/{call}/{attempt}/output.json"),
            size: 0,
            hash: ContentHash::of_str(""),
            pointer: None,
        })
    });
    event(
        seq,
        Some(run),
        EventPayload::ToolResult(komo_kernel::events::ToolResult {
            call_id: call.clone(),
            attempt_id: AttemptId::from_raw(attempt),
            status,
            output_ref: output,
            elapsed_ms: 1200,
            preview,
            stdout: None,
            stderr: None,
            attempt_state: None,
        }),
    )
}
