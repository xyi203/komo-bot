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

/// `docs/codemode.md` §5：`atools` 的调用是同时在跑的嵌套任务——第二个跑完时第一个还卡在闸门上；
/// 序号仍按脚本里发出的顺序，结果按参数顺序回到脚本。
#[tokio::test]
async fn atools_calls_run_as_concurrent_nested_tasks_keyed_in_script_order() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    let read = Arc::new(TestTool::reader("read", &world.log).with_gate(&gate));
    let rg = Arc::new(TestTool::reader("rg", &world.log));
    setup(&world, vec![read.clone(), rg.clone()]);
    script_round(
        &world,
        "import asyncio\na, b = await asyncio.gather(atools.read(path='a'), atools.rg(pattern='x'))\n\
         text(a['text'] + ' | ' + b['text'])",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start read #1").await;
    world.until_log("finish rg #1").await;
    gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();

    let (status, view) = only_result(&world).await;
    assert_eq!(status, "completed");
    assert!(view.contains("read output | rg output"), "{view}");
    assert!(view.contains("调用了 2 次工具：read×1、rg×1"), "{view}");
    let tasks = world.tasks("komo.tool").await;
    let children = nested(&tasks);
    assert_eq!(children.len(), 2);
    for (child, key, name) in [(children[0], "c1", "read"), (children[1], "c2", "rg")] {
        assert_eq!(child.input["key"], key);
        assert_eq!(child.input["name"], name);
        assert!(completed(child), "{:?}", child.state);
    }
    engine.close().await.unwrap();
}

/// 并行调用中途重启：已交付的那个从槽位回放、不重做，没跑完的（Safe）重跑；结果成批交回，
/// 所以重放的脚本发出同样顺序的调用，之后的新调用照常执行。
#[tokio::test]
async fn a_restart_during_parallel_calls_replays_the_delivered_one_and_continues() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log).hanging_first(1));
    let rg = Arc::new(TestTool::reader("rg", &world.log));
    let ls = Arc::new(TestTool::reader("ls", &world.log));
    setup(&world, vec![read.clone(), rg.clone(), ls.clone()]);
    script_round(
        &world,
        "rs = await settle(atools.read(path='a'), atools.rg(pattern='x'))\nc = tools.ls()\n\
         text(' / '.join([r['value']['text'] for r in rs] + [c['text']]))",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start read #1").await;
    world.until_log("finish rg #1").await;

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    let (status, view) = only_result(&world).await;
    assert_eq!(status, "completed", "{view}");
    assert!(
        view.contains("read output / rg output / ls output"),
        "{view}"
    );
    assert_eq!(rg.runs(), 1, "the delivered call is replayed, not redone");
    assert_eq!(read.runs(), 2, "the safe interrupted call runs again");
    assert_eq!(ls.runs(), 1);
    let tasks = world.tasks("komo.tool").await;
    let keys: Vec<_> = nested(&tasks)
        .iter()
        .map(|task| (task.input["key"].clone(), task.input["name"].clone()))
        .collect();
    assert_eq!(
        keys,
        [
            (json!("c1"), json!("read")),
            (json!("c2"), json!("rg")),
            (json!("c3"), json!("ls"))
        ]
    );
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

/// 照 pi：计划是 WriteFile 的工具（write / edit）在脚本里照常执行；只认 prepare 交出的计划，名字绕不过去——
/// 计划是读写文件、bash、MCP 之外的操作（这里是 MemoryChange）在 prepare 之后被拒、不执行。
#[tokio::test]
async fn a_file_write_inside_a_script_runs_but_other_operations_are_refused_after_prepare() {
    let world = World::new();
    let mutate = Arc::new(TestTool {
        operation: Operation::WriteFile,
        ..TestTool::writer("mutate", &world.log)
    });
    let remember = Arc::new(TestTool {
        operation: Operation::MemoryChange,
        ..TestTool::writer("remember", &world.log)
    });
    setup(&world, vec![mutate.clone(), remember.clone()]);
    script_round(
        &world,
        "text(tools.mutate()['status'])\ntry:\n    tools.remember()\nexcept ToolError as e:\n    text('refused: ' + str(e))",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let (_, view) = only_result(&world).await;
    assert!(view.contains("completed"), "{view}");
    assert!(
        view.contains("remember 不是读写文件、bash 或 MCP；脚本里不支持此操作"),
        "{view}"
    );
    assert_eq!(mutate.runs(), 1);
    assert_eq!(remember.runs(), 0);
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

async fn nested_with_pid(world: &World) -> TaskRecord {
    world
        .until(|| async {
            world
                .live()
                .await
                .nested_tools
                .as_ref()
                .is_some_and(|slots| slots.iter().any(|nested| nested.slot.pid.is_some()))
        })
        .await;
    let tasks = world.tasks("komo.tool").await;
    nested(&tasks)
        .into_iter()
        .find(|task| task.input["name"] == "bash" || task.input["name"] == "read")
        .unwrap()
        .clone()
}

async fn child_phase(world: &World, id: komo_kernel::durable::TaskId) -> serde_json::Value {
    let record = world
        .tasks("komo.tool")
        .await
        .into_iter()
        .find(|task| task.id == id)
        .unwrap();
    match record.state {
        TaskState::Running { checkpoint } | TaskState::Pending { checkpoint } => checkpoint,
        _ => serde_json::Value::Null,
    }
}

fn quick_probe(world: &World) {
    let mut settings = (*world.harness.deps().settings()).clone();
    settings.probe_interval = Duration::from_millis(20);
    world.harness.deps().set_settings(settings);
}

fn fake_child() -> komo_kernel::traits::ChildStarted {
    komo_kernel::traits::ChildStarted {
        pid: 4242,
        pgid: 4242,
        started_at_ms: 1761206400000,
    }
}

#[tokio::test]
async fn nested_live_slots_follow_admission_intent_progress_and_delivery_then_disappear() {
    use super::super::live::SlotStatus;
    use komo_kernel::durable::StorageWrite;
    let world = World::new();
    let bash_gate = Arc::new(Notify::new());
    let after_gate = Arc::new(Notify::new());
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(fake_child())
            .with_stdout("stream tail")
            .with_gate(&bash_gate),
    );
    let read = Arc::new(TestTool::reader("read", &world.log).with_gate(&after_gate));
    setup(&world, vec![bash, read]);
    script_round(
        &world,
        "r = tools.bash(command='test')\ntools.read(path='hold')\ntext(r['text'])",
    );
    let engine = world.open().await;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = seen.clone();
    engine.line().subscribe_commits(move |publication| {
        for write in &publication.writes {
            if let StorageWrite::DocumentChange { value, .. } = write
                && let Some(slots) = value.get("nestedTools")
            {
                observed.lock().unwrap().push(slots.clone());
            }
        }
    });
    let id = world.submit(&engine, "go").await;
    let child = nested_with_pid(&world).await;
    let live = world.live().await;
    let slot = live.slot(child.id).unwrap();
    assert_eq!(slot.status, SlotStatus::Running);
    assert_eq!(
        (slot.pid, slot.pgid, slot.started_at),
        (Some(4242), Some(4242), Some(1761206400000))
    );
    assert_eq!(slot.output.as_deref(), Some("stream tail"));
    assert_eq!(slot.attempt, Some(1));
    assert_eq!(slot.replay, Some(komo_kernel::types::plan::Replay::Unsafe));
    assert!(!world.storage.progress_commits().is_empty());
    bash_gate.notify_one();
    world.until_log("start read #1").await;
    let live = world.live().await;
    let slot = live.slot(child.id).unwrap();
    assert_eq!(slot.status, SlotStatus::Done);
    assert!(slot.pid.is_none() && slot.output.is_none() && slot.entry.is_none());
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|slots| slots[0]["status"] == "pending"));
    assert!(seen.iter().any(|slots| slots[0]["status"] == "running"));
    assert!(seen.iter().any(|slots| slots[0]["status"] == "done"));
    drop(seen);
    after_gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();
    assert!(world.live().await.nested_tools.is_none());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn a_bash_view_is_bounded_in_the_nested_receipt_while_the_complete_stdout_stays_in_a_blob() {
    use super::super::nested::{NestedDoc, nested_address};
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let full = format!(
        "FIRST_OUT\n{}LAST_OUT\n",
        "line: content for the full blob\n".repeat(10_000)
    );
    let gate = Arc::new(Notify::new());
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_stdout(&full)
            .with_text(&full),
    );
    let read = Arc::new(TestTool::reader("read", &world.log).with_gate(&gate));
    setup(&world, vec![bash, read]);
    script_round(
        &world,
        "r = tools.bash(command='test')\ntools.read(path='hold')\ntext('last: ' + str('LAST_OUT' in r['text']))",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start read #1").await;
    let tasks = world.tasks("komo.tool").await;
    let parent = outer(&tasks).id;
    let child = nested(&tasks)[0];
    let doc = world
        .storage_handle()
        .find_document(&nested_address(parent, "c1"))
        .await
        .unwrap()
        .unwrap();
    let encoded = serde_json::to_string(&doc.value).unwrap();
    let doc: NestedDoc = serde_json::from_value(doc.value).unwrap();
    let receipt = doc.result.unwrap().script_result().unwrap();
    let text = receipt["text"].as_str().unwrap();
    assert!(
        text.contains("LAST_OUT") && !text.contains("FIRST_OUT"),
        "{text}"
    );
    assert!(text.contains(&format!("artifact://1/{}/1/result", child.id)));
    assert!(text.len() < 52 * 1024);
    assert!(
        encoded.len() < 60 * 1024,
        "the full stdout is not in komo.nested"
    );
    let stored = tokio::fs::read_to_string(
        dir.path()
            .join(format!("tool-output/1/{}/1/stdout.txt", child.id)),
    )
    .await
    .unwrap();
    assert_eq!(stored, full);
    gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();
    assert!(world.live().await.nested_tools.is_none());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn an_unsafe_nested_bash_probes_a_surviving_child_before_becoming_uncertain() {
    use crate::recovery::Liveness;
    let world = World::new();
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(fake_child())
            .hanging_first(1),
    );
    let after = Arc::new(TestTool::reader("rg", &world.log));
    setup(&world, vec![bash.clone(), after.clone()]);
    quick_probe(&world);
    script_round(
        &world,
        "tools.bash(command='test')\ntools.rg(pattern='never')",
    );
    world.probe.set(Liveness::Alive);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    let child = nested_with_pid(&world).await;
    let engine = world.restart(engine).await;
    world
        .until(|| async { world.probe.asked().len() >= 2 })
        .await;
    assert_eq!(child_phase(&world, child.id).await["phase"], "probe");
    assert!(world.probe.asked().iter().all(|child| child.pid == 4242));
    assert_eq!((bash.runs(), after.runs()), (1, 0));
    world.probe.set(Liveness::Unknown);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(child_phase(&world, child.id).await["phase"], "probe");
    world.probe.set(Liveness::Gone);
    world.harness.wait(&engine, id).await.unwrap();
    let (status, view) = only_result(&world).await;
    assert_eq!(status, "uncertain", "{view}");
    assert_eq!((bash.runs(), after.runs()), (1, 0));
    assert!(world.live().await.nested_tools.is_none());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_nested_bash_in_probe_records_its_surviving_pid_and_clears_slots() {
    use crate::recovery::Liveness;
    use komo_kernel::traits::ToolOutputStore;
    let world = World::new();
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(fake_child())
            .hanging_first(1),
    );
    setup(&world, vec![bash.clone()]);
    quick_probe(&world);
    script_round(&world, "tools.bash(command='test')");
    world.probe.set(Liveness::Alive);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    let child = nested_with_pid(&world).await;
    let engine = world.restart(engine).await;
    world
        .until(|| async { child_phase(&world, child.id).await["phase"] == "probe" })
        .await;
    let parent = outer(&world.tasks("komo.tool").await).id;
    engine.abort_task(parent).await.unwrap();
    world.harness.wait(&engine, id).await.unwrap();
    let set = world.hooks.tools(CONV).await.unwrap();
    let attempt = super::super::tool::attempt_ref(&set.env, CONV, child.id, 1);
    let published = world.mem.find_published(&attempt).await.unwrap().unwrap();
    assert!(published.body.unwrap().preview.unwrap().contains("4242"));
    let tasks = world.tasks("komo.tool").await;
    assert!(matches!(
        outcome_of(nested(&tasks)[0]),
        TaskOutcome::Aborted { .. }
    ));
    assert_eq!(bash.runs(), 1);
    assert!(world.live().await.nested_tools.is_none());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn a_complete_truncated_bash_output_is_reused_from_disk_before_probing_the_child() {
    use super::super::nested::{NestedDoc, NestedResult, nested_address};
    use crate::recovery::Liveness;
    use komo_kernel::types::refs::{ToolResultBody, ToolResultStatus};
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let gate = Arc::new(Notify::new());
    let stdout = "a full stdout line\n".repeat(10_000);
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(fake_child())
            .with_stdout(&stdout)
            .hanging_first(1),
    );
    let after = Arc::new(TestTool::reader("read", &world.log).with_gate(&gate));
    setup(&world, vec![bash.clone(), after]);
    quick_probe(&world);
    script_round(
        &world,
        "r = tools.bash(command='test')\ntools.read(path='hold')\ntext('done')",
    );
    world.probe.set(Liveness::Alive);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    let child = nested_with_pid(&world).await;
    let set = world.hooks.tools(CONV).await.unwrap();
    let attempt = super::super::tool::attempt_ref(&set.env, CONV, child.id, 1);
    engine.close().await.unwrap();
    // 结果已经发布、交付还没提交时崩溃：stdout 比 preview 长，bash 仍可准确重建脚本视图。
    let writer = set.env.outputs.begin(&attempt).await.unwrap();
    let published = set
        .env
        .outputs
        .publish(
            writer,
            ToolResultBody {
                status: ToolResultStatus::Completed,
                result: json!({"exit_code": 0}),
                error: None,
                exit_code: Some(0),
                artifacts: vec![],
                images: vec![],
                preview: Some("LAST_OUT\n".into()),
            },
        )
        .await
        .unwrap();
    assert!(!super::super::nested::reusable(&published, "rg"));
    let expected =
        NestedResult::from_bash(&published, &attempt, &set.env.projection, false).unwrap();
    let engine = world.open().await;
    world.until_log("start read #1").await;
    let parent = outer(&world.tasks("komo.tool").await).id;
    let doc = world
        .storage_handle()
        .find_document(&nested_address(parent, "c1"))
        .await
        .unwrap()
        .unwrap();
    let doc: NestedDoc = serde_json::from_value(doc.value).unwrap();
    assert_eq!(
        doc.result.unwrap(),
        expected,
        "same persisted facts produce the same receipt"
    );
    assert!(
        world.probe.asked().is_empty(),
        "complete output wins before probe"
    );
    assert_eq!(bash.runs(), 1, "unsafe bash is never executed again");
    gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();
    assert!(world.live().await.nested_tools.is_none());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn a_nested_host_timeout_stays_uncertain_on_disk_and_stops_following_calls() {
    use komo_kernel::traits::ToolOutputStore;
    use komo_kernel::types::refs::ToolResultStatus;
    use komo_kernel::types::tool::ToolError;
    let world = World::new();
    let bash = Arc::new(TestTool {
        execute_error: Some(ToolError::Timeout { after_secs: 1 }),
        ..TestTool::writer("bash", &world.log)
    });
    let after = Arc::new(TestTool::reader("rg", &world.log));
    setup(&world, vec![bash, after.clone()]);
    script_round(
        &world,
        "try:\n    tools.bash(command='test')\nexcept ToolError:\n    pass\ntools.rg(pattern='never')",
    );
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let (status, view) = only_result(&world).await;
    assert_eq!(status, "uncertain", "{view}");
    assert_eq!(after.runs(), 0);
    let tasks = world.tasks("komo.tool").await;
    let child = nested(&tasks)[0];
    let set = world.hooks.tools(CONV).await.unwrap();
    let attempt = super::super::tool::attempt_ref(&set.env, CONV, child.id, 1);
    let published = world.mem.find_published(&attempt).await.unwrap().unwrap();
    assert_eq!(published.status, ToolResultStatus::Uncertain);
    assert!(
        super::super::nested::NestedResult::from_bash(
            &published,
            &attempt,
            &set.env.projection,
            false
        )
        .unwrap()
        .uncertain
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn a_safe_nested_call_reruns_after_probe_and_clears_the_old_attempts_progress() {
    use crate::recovery::Liveness;
    let world = World::new();
    let gate = Arc::new(Notify::new());
    let read = Arc::new(
        TestTool::reader("read", &world.log)
            .with_child(fake_child())
            .hanging_first(1),
    );
    let after = Arc::new(TestTool::reader("rg", &world.log).with_gate(&gate));
    setup(&world, vec![read.clone(), after]);
    quick_probe(&world);
    script_round(&world, "tools.read(path='test')\ntools.rg(pattern='hold')");
    world.probe.set(Liveness::Alive);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    let child = nested_with_pid(&world).await;
    let engine = world.restart(engine).await;
    world
        .until(|| async { child_phase(&world, child.id).await["phase"] == "probe" })
        .await;
    world.probe.set(Liveness::Gone);
    world.until_log("start rg #1").await;
    let live = world.live().await;
    let slot = live.slot(child.id).unwrap();
    assert_eq!(read.runs(), 2);
    assert_eq!(slot.attempt, Some(2));
    assert!(slot.pid.is_none() && slot.output.is_none());
    gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();
    assert!(world.live().await.nested_tools.is_none());
    engine.close().await.unwrap();
}
