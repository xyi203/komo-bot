//! codemode 嵌套调用（设计 §4.7、§5、§12.4；`docs/codemode.md` §7；pi-durable spec §7.3、§7.6）：
//! 外层是一个 `komo.tool` 任务，脚本里的每个子调用是它名下 `kind: "nested"` 的工具任务，结果写进
//! `komo.nested` 的成员而不是 transcript。脚本跑在进程内的 Monty worker 上（真引擎，同一套协议）；
//! 真 `komo __codemode-worker` 子进程的端到端在 `crates/komo/tests/codemode_nested.rs`。
//! 用真时钟：worker 在另一个线程里算，暂停的时钟会在它算的时候空转着把超时跳过去。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::durable::{ConversationId, Message, Storage, TaskOutcome, TaskRecord, TaskState};
use komo_kernel::traits::Tool;
use komo_kernel::types::plan::Operation;
use serde_json::json;
use tokio::sync::Notify;

use super::super::hooks::{AgentHooks, AgentSetup, HookError, ToolSet};
use super::super::{Harness, HarnessDeps, SubmissionDraft};
use super::support::*;
use crate::codemode::Worker;
use crate::durable::tests::support::Log;
use crate::tools::codemode::CodemodeTool;

/// 有副作用的 MCP 工具（脚本里放行，中断后不重做）。
fn mcp_writer(name: &'static str, log: &Log) -> TestTool {
    TestTool {
        operation: Operation::McpCall {
            server: "demo".into(),
            tool: name.into(),
            read_only: false,
        },
        ..TestTool::writer(name, log)
    }
}

/// 装上 codemode 与这些工具，worker 是进程内的。
fn setup(world: &World, tools: Vec<Arc<dyn Tool>>) {
    world
        .harness
        .deps()
        .set_codemode(Some(Worker::in_process()));
    let mut all: Vec<Arc<dyn Tool>> = vec![Arc::new(CodemodeTool::new())];
    all.extend(tools);
    world.hooks.set_tools(all);
}

/// 模型这一轮要的就是这段脚本，之后一句话收尾。
fn script_round(world: &World, code: &str) {
    world
        .script
        .calls(&[("m1", "codemode", json!({ "code": code }))]);
    world.script.reply("ok");
}

fn outer(tasks: &[TaskRecord]) -> &TaskRecord {
    tasks
        .iter()
        .find(|task| task.input["kind"] == "model")
        .expect("the codemode call's own task")
}

/// 外层名下的嵌套任务，按 ID（= 创建次序，也就是脚本里的调用次序）。
fn nested(tasks: &[TaskRecord]) -> Vec<&TaskRecord> {
    let mut found: Vec<&TaskRecord> = tasks
        .iter()
        .filter(|task| task.input["kind"] == "nested")
        .collect();
    found.sort_by_key(|task| task.id);
    found
}

fn outcome_of(task: &TaskRecord) -> &TaskOutcome {
    match &task.state {
        TaskState::Terminal { outcome } => outcome,
        other => panic!("task {} should be terminal, got {other:?}", task.id),
    }
}

fn completed(task: &TaskRecord) -> bool {
    matches!(outcome_of(task), TaskOutcome::Completed { .. })
}

/// 结果 entry 的 `data.status` 与给模型的文字。
async fn only_result(world: &World) -> (String, String) {
    let entries = world.entries_of("komo.tool-result").await;
    assert_eq!(
        entries.len(),
        1,
        "only the codemode call's result is in the transcript"
    );
    let entry = &entries[0];
    (
        entry.data.as_ref().unwrap()["status"]
            .as_str()
            .unwrap()
            .to_owned(),
        text_of(&entry.model.as_ref().unwrap()[0]),
    )
}

/// 设计 §4.7 / pi spec §7.6：脚本两次子调用都成功，只有脚本的输出回到模型；每个子调用是外层名下
/// 的一个嵌套任务（自己的任务 ID、自己的输出目录），不进 transcript。
#[tokio::test]
async fn a_script_with_two_sub_calls_returns_only_its_text_and_leaves_one_nested_task_per_call() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    let rg = Arc::new(TestTool::reader("rg", &world.log).with_stdout("stdout of rg"));
    setup(&world, vec![read.clone(), rg.clone()]);
    script_round(
        &world,
        "a = tools.read(path='a.txt')\nb = tools.rg(pattern='x')\ntext(a['text'] + ' | ' + b['text'])",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let (status, view) = only_result(&world).await;
    assert_eq!(status, "completed");
    // `rg` 写了 stdout：脚本拿到的是完整 stdout，不是预览。
    assert!(view.contains("read output | stdout of rg"), "{view}");
    assert!(view.contains("调用了 2 次工具：read×1、rg×1"), "{view}");
    assert_eq!((read.runs(), rg.runs()), (1, 1));

    let tasks = world.tasks("komo.tool").await;
    let outer = outer(&tasks);
    let children = nested(&tasks);
    assert_eq!(children.len(), 2);
    for (child, key, name) in [(children[0], "c1", "read"), (children[1], "c2", "rg")] {
        assert_eq!(child.owner, Some(outer.id), "owned by the codemode task");
        assert_eq!(child.input["key"], key);
        assert_eq!(child.input["name"], name);
        assert_eq!(child.input["callId"], format!("m1/{key}"));
        assert!(completed(child), "{:?}", child.state);
    }
    assert!(completed(outer));
    // 模型只看见外层那一个调用，请求里没有嵌套调用的痕迹。
    assert_eq!(
        outline(&world.script.requests()[1]),
        [
            "system",
            "user: go",
            "assistant calls codemode(m1)",
            "result(m1)"
        ]
    );
    engine.close().await.unwrap();
}

/// 设计 §5 的表 + `docs/codemode.md` §7：外层在第二个子调用执行中重启——第一个从槽位回放、不重做，
/// 第二个（Safe）新尝试重跑，整段脚本得出同样的结果。
#[tokio::test]
async fn a_restart_during_the_second_sub_call_replays_the_first_and_reruns_the_second_when_safe() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    let rg = Arc::new(TestTool::reader("rg", &world.log).hanging_first(1));
    setup(&world, vec![read.clone(), rg.clone()]);
    script_round(
        &world,
        "a = tools.read(path='a.txt')\nb = tools.rg(pattern='x')\ntext(a['text'] + ' / ' + b['text'])",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start rg #1").await;

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(read.runs(), 1, "the delivered first call is not redone");
    assert_eq!(rg.runs(), 2, "the safe second call runs again");
    let (status, view) = only_result(&world).await;
    assert_eq!(status, "completed");
    assert!(view.contains("read output / rg output"), "{view}");
    let tasks = world.tasks("komo.tool").await;
    assert_eq!(nested(&tasks).len(), 2, "no third task was created");
    assert!(tasks.iter().all(completed));
    engine.close().await.unwrap();
}

/// 设计 §5 的表（unsafe → uncertain）+ `docs/codemode.md` §7：执行中被打断的 unsafe 子调用不重做，
/// 结果不明——外层锁住后面的调用，整段结果是 uncertain 交给模型。
#[tokio::test]
async fn a_restart_during_an_unsafe_sub_call_ends_the_script_uncertain_and_never_redoes_it() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    let bump = Arc::new(mcp_writer("mcp__demo__bump", &world.log).hanging_first(1));
    let after = Arc::new(TestTool::reader("rg", &world.log));
    setup(&world, vec![read.clone(), bump.clone(), after.clone()]);
    script_round(
        &world,
        "a = tools.read(path='a.txt')\nb = tools.mcp__demo__bump(n=1)\ntools.rg(pattern='x')\ntext('unreachable')",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start mcp__demo__bump #1").await;

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(read.runs(), 1);
    assert_eq!(bump.runs(), 1, "the unsafe call is not run a second time");
    assert_eq!(after.runs(), 0, "nothing after the uncertain call runs");
    let (status, view) = only_result(&world).await;
    assert_eq!(status, "uncertain");
    assert!(view.contains("不会自动重跑"), "{view}");
    let tasks = world.tasks("komo.tool").await;
    let children = nested(&tasks);
    assert_eq!(children.len(), 2, "the call after it was never admitted");
    assert!(completed(children[0]));
    assert!(matches!(
        outcome_of(children[1]),
        TaskOutcome::Failed { .. }
    ));
    engine.close().await.unwrap();
}

/// `docs/codemode.md` §7「时钟与熵」：宿主应答作为 memo 记在外层任务上；重启后脚本整段重跑，同样的
/// 值回放出来——否则第一个子调用的参数（用了时钟与随机数）会和记录对不上、整段分歧。
#[tokio::test]
async fn clock_and_random_readings_are_memoized_on_the_outer_task_and_replayed_unchanged() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    let rg = Arc::new(TestTool::reader("rg", &world.log).hanging_first(1));
    setup(&world, vec![read.clone(), rg.clone()]);
    script_round(
        &world,
        "import time, random\nstamp = repr(time.time())\nroll = repr(random.random())\n\
         tools.read(path=stamp + roll)\ntools.rg(pattern='x')\ntext('stamp ' + stamp)",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start rg #1").await;

    let tasks = world.tasks("komo.tool").await;
    let memos = outer(&tasks)
        .memos
        .clone()
        .expect("memos on the outer task");
    assert!(memos.contains_key("codemode.start"), "{memos:?}");
    let batch = memos["codemode.nondet.1"].as_array().expect("a batch");
    assert!(
        batch.iter().any(|entry| entry["request"]["op"] == "clock"),
        "{batch:?}"
    );
    assert!(
        batch
            .iter()
            .any(|entry| entry["request"]["op"] == "urandom"),
        "{batch:?}"
    );
    let clock = batch
        .iter()
        .find(|entry| entry["request"]["op"] == "clock")
        .unwrap()["value"]
        .as_f64()
        .unwrap();

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    let (status, view) = only_result(&world).await;
    assert_eq!(status, "completed", "no divergence on replay: {view}");
    let replayed: f64 = view
        .lines()
        .find_map(|line| line.strip_prefix("stamp "))
        .expect("the script's own text")
        .trim()
        .parse()
        .unwrap();
    assert_eq!(replayed, clock, "the replayed clock is the memoized one");
    assert_eq!(read.runs(), 1, "replayed, not redone");
    engine.close().await.unwrap();
}

/// 记录的读数与重放的脚本对不上（脚本换了读法）是分歧：整段 uncertain，不执行任何新调用。
#[tokio::test]
async fn a_script_that_reads_differently_on_replay_diverges_and_runs_nothing_new() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    let rg = Arc::new(TestTool::reader("rg", &world.log).hanging_first(1));
    setup(&world, vec![read.clone(), rg.clone()]);
    script_round(
        &world,
        "import time\nt = time.time()\ntools.read(path='a')\ntools.rg(pattern='x')\ntext('done')",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start rg #1").await;
    engine.close().await.unwrap();

    // 外层的脚本在两次开库之间"被改了"：指纹对不上。
    let tasks = world.tasks("komo.tool").await;
    assert!(outer(&tasks).memos.is_some());
    world.hooks.set_surface(Some(vec!["codemode", "read"]));
    let engine = world.open().await;
    world.harness.wait(&engine, id).await.unwrap();

    let (status, view) = only_result(&world).await;
    assert_eq!(status, "uncertain");
    assert!(view.contains("回放分歧"), "{view}");
    assert_eq!(rg.runs(), 1, "no new call after a divergence");
    engine.close().await.unwrap();
}

/// 设计 §12.4「取消」：外层被 abort 时，名下正在跑的嵌套任务自底向上 abort，各自的 abort handler
/// 写 `aborted` 结果；外层随后收尾。
#[tokio::test]
async fn aborting_the_outer_task_aborts_the_running_nested_task_first() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log).hanging_first(1));
    setup(&world, vec![read.clone()]);
    script_round(&world, "tools.read(path='a')\ntext('unreachable')");
    let engine = world.open().await;
    let _id = world.submit(&engine, "go").await;
    world.until_log("start read #1").await;

    let tasks = world.tasks("komo.tool").await;
    let outer_id = outer(&tasks).id;
    engine.abort_task(outer_id).await.unwrap();
    let outer_record = engine.wait_for_task(outer_id).await.unwrap();

    let tasks = world.tasks("komo.tool").await;
    let children = nested(&tasks);
    assert_eq!(children.len(), 1);
    assert!(
        matches!(outcome_of(children[0]), TaskOutcome::Aborted { .. }),
        "{:?}",
        children[0].state
    );
    assert!(matches!(
        outer_record.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Aborted { .. }
        }
    ));
    assert!(
        children[0].ended_at <= outer_record.ended_at,
        "the nested task settles before its owner"
    );
    assert_eq!(read.runs(), 1);
    engine.close().await.unwrap();
}

/// 子调用走和顶层调用同样的能力面检查：热重载把工具撤出能力面之后，脚本里再调它得到的是
/// "脚本里不能调"，工具没有被 prepare 或执行；脚本接住这个 `ToolError` 继续往下走。
#[tokio::test]
async fn a_sub_call_to_a_tool_the_surface_no_longer_allows_is_refused_and_the_script_goes_on() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    let read = Arc::new(TestTool::reader("read", &world.log).with_gate(&gate));
    let rg = Arc::new(TestTool::reader("rg", &world.log));
    setup(&world, vec![read.clone(), rg.clone()]);
    script_round(
        &world,
        "tools.read(path='a')\ntry:\n    tools.rg(pattern='x')\nexcept ToolError as e:\n    text('refused: ' + str(e))",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start read #1").await;
    // 第一个调用还没返回：能力面收窄，rg 不在了。
    world.hooks.set_surface(Some(vec!["codemode", "read"]));
    gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();

    let (status, view) = only_result(&world).await;
    assert_eq!(status, "completed");
    assert!(view.contains("refused: 脚本里不能调 rg"), "{view}");
    assert_eq!(rg.prepares.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(rg.runs(), 0);
    let tasks = world.tasks("komo.tool").await;
    assert!(
        nested(&tasks).iter().all(|task| completed(task)),
        "a refused call is a settled nested task, not a hanging one"
    );
    engine.close().await.unwrap();
}

/// 脚本里只放行读文件与 MCP：本地写入与命令（`Operation` 不是 `ReadFile` / `McpCall`）在
/// `prepare` 之后被拒，不执行；`bash` 一类名字直接不认识。
#[tokio::test]
async fn a_local_write_inside_a_script_is_refused_after_prepare_and_never_executed() {
    let world = World::new();
    let mutate = Arc::new(TestTool::writer("mutate", &world.log));
    setup(&world, vec![mutate.clone()]);
    script_round(
        &world,
        "try:\n    tools.mutate()\nexcept ToolError as e:\n    text('refused: ' + str(e))",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let (_, view) = only_result(&world).await;
    assert!(
        view.contains("mutate 不是只读工具或 MCP；脚本里不支持此操作"),
        "{view}"
    );
    assert_eq!(mutate.runs(), 0);
    engine.close().await.unwrap();
}

/// 没接 worker 的 Gateway：codemode 调用以失败结果收场，不建任何嵌套任务。
#[tokio::test]
async fn without_a_worker_the_call_fails_and_creates_no_nested_task() {
    let world = World::new();
    setup(&world, vec![Arc::new(TestTool::reader("read", &world.log))]);
    world.harness.deps().set_codemode(None);
    script_round(&world, "text('x')");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let (status, view) = only_result(&world).await;
    assert_eq!(status, "failed");
    assert!(view.contains("没有接 codemode"), "{view}");
    assert!(nested(&world.tasks("komo.tool").await).is_empty());
    engine.close().await.unwrap();
}

/// worker 起不来（可执行文件不在）：同旧路径，脚本没有运行，失败结果带原因。
#[tokio::test]
async fn a_worker_that_cannot_start_is_a_failed_result() {
    let world = World::new();
    setup(&world, vec![Arc::new(TestTool::reader("read", &world.log))]);
    world
        .harness
        .deps()
        .set_codemode(Some(Worker::exe("/nonexistent/komo")));
    script_round(&world, "text('x')");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let (status, view) = only_result(&world).await;
    assert_eq!(status, "failed");
    assert!(view.contains("脚本进程起不来"), "{view}");
    engine.close().await.unwrap();
}

/// 把工具的 `call_timeout` 压短的钩子（其余照 `TestHooks`）。
struct ShortTimeout {
    inner: Arc<TestHooks>,
    timeout: Duration,
}

#[async_trait]
impl AgentHooks for ShortTimeout {
    async fn setup(
        &self,
        conversation: ConversationId,
        context: &[Message],
    ) -> Result<AgentSetup, HookError> {
        let mut setup = self.inner.setup(conversation, context).await?;
        setup.tools.env.call_timeout = self.timeout;
        Ok(setup)
    }

    async fn tools(&self, conversation: ConversationId) -> Result<ToolSet, HookError> {
        let mut tools = self.inner.tools(conversation).await?;
        tools.env.call_timeout = self.timeout;
        Ok(tools)
    }
}

/// `docs/codemode.md` §4、§7：墙钟到了，宿主杀 worker；正在执行的子调用被收掉，它结果不明，所以
/// 整段结果是 uncertain（不是普通超时失败）。
#[tokio::test]
async fn a_script_timeout_aborts_the_in_flight_sub_call_and_ends_uncertain() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log).hanging_first(1));
    setup(&world, vec![read.clone()]);
    let harness = Harness::new(
        HarnessDeps::new(
            Arc::new(ShortTimeout {
                inner: world.hooks.clone(),
                timeout: Duration::from_millis(400),
            }),
            world.script.clone(),
        )
        .with_probe(world.probe.clone())
        .with_codemode(Some(Worker::in_process())),
    );
    harness.install(&world.registry);
    script_round(&world, "tools.read(path='a')\ntext('unreachable')");
    let engine = world.open().await;
    let id = harness
        .submit(&engine, CONV, SubmissionDraft::input("go"))
        .await
        .unwrap();
    harness.wait(&engine, id).await.unwrap();

    let (status, view) = only_result(&world).await;
    assert_eq!(status, "uncertain");
    assert!(view.contains("结果不明"), "{view}");
    let tasks = world.tasks("komo.tool").await;
    let children = nested(&tasks);
    assert_eq!(children.len(), 1);
    assert!(matches!(
        outcome_of(children[0]),
        TaskOutcome::Aborted { .. }
    ));
    engine.close().await.unwrap();
}

/// 嵌套任务被调度器写了 `faulted`（没来得及写结果）：做没做过不知道，脚本拿到的是结果不明，
/// 外层锁住后面的调用、整段 uncertain。
#[tokio::test]
async fn a_nested_task_the_scheduler_faulted_is_an_unknown_result_for_the_script() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    let read = Arc::new(TestTool::reader("read", &world.log).with_gate(&gate));
    let rg = Arc::new(TestTool::reader("rg", &world.log));
    setup(&world, vec![read.clone(), rg.clone()]);
    script_round(
        &world,
        "tools.read(path='a')\ntools.rg(pattern='x')\ntext('unreachable')",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start read #1").await;
    // 第二个子调用的任务问钩子要工具表时钩子坏了：任务 faulted。
    world
        .hooks
        .fail_tools_once(HookError::Failed("hook is down".into()));
    gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();

    let (status, view) = only_result(&world).await;
    assert_eq!(status, "uncertain");
    assert!(view.contains("没有正常收场"), "{view}");
    assert_eq!(rg.runs(), 0);
    let tasks = world.tasks("komo.tool").await;
    let children = nested(&tasks);
    assert_eq!(children.len(), 2);
    assert!(matches!(
        outcome_of(children[1]),
        TaskOutcome::Faulted { .. }
    ));
    engine.close().await.unwrap();
}

/// 嵌套结果随外层终结一起消失：外层结果里的 `result.calls` 才是留给模型的摘要。
#[tokio::test]
async fn nested_documents_retire_with_the_outer_task() {
    let world = World::new();
    setup(&world, vec![Arc::new(TestTool::reader("read", &world.log))]);
    script_round(&world, "tools.read(path='a')\ntext('x')");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let tasks = world.tasks("komo.tool").await;
    let owner = outer(&tasks).id;
    for key in ["c1"] {
        let address = super::super::nested::nested_address(owner, key);
        assert!(
            world
                .storage
                .find_document(&address)
                .await
                .unwrap()
                .is_none(),
            "the member {key} is gone once its owner is terminal"
        );
    }
    engine.close().await.unwrap();
}

/// 同 §5 表跑在 Turso 文件库上（真文件存储）：第二个子调用执行中"重启"（关库、重开、重建引擎），
/// 第一个从 `komo.nested` 回放不重做，第二个（Safe）重跑；脚本的时钟读数从外层的 memo 回放。
#[tokio::test]
async fn on_a_turso_database_a_restart_in_the_second_sub_call_replays_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let read = Arc::new(TestTool::reader("read", &world.log));
    let rg = Arc::new(TestTool::reader("rg", &world.log).hanging_first(1));
    setup(&world, vec![read.clone(), rg.clone()]);
    script_round(
        &world,
        "import time\nstamp = repr(time.time())\ntools.read(path=stamp)\n\
         tools.rg(pattern='x')\ntext('stamp ' + stamp)",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start rg #1").await;

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(read.runs(), 1);
    assert_eq!(rg.runs(), 2);
    let (status, view) = only_result(&world).await;
    assert_eq!(status, "completed", "{view}");
    let tasks = world.tasks("komo.tool").await;
    assert_eq!(nested(&tasks).len(), 2);
    assert!(tasks.iter().all(completed));
    // 嵌套调用的输出落在各自任务的目录里。
    let child = nested(&tasks)[0].id;
    assert!(
        dir.path()
            .join(format!("tool-output/1/{child}/1/output.json"))
            .exists()
    );
    engine.close().await.unwrap();
}
