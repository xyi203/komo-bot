//! `komo.tool`：prepare、意图槽位、并行与屏障、结果 entry（设计 §4.4、§12.5；pi spec §8.4、§8.5）。
//! 对应 pi 的 `harness-tools`。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::durable::{Message, TaskOutcome, TaskState};
use serde_json::json;
use tokio::sync::Notify;

use super::super::SlotStatus;
use super::support::*;

fn position(lines: &[String], line: &str) -> usize {
    lines
        .iter()
        .position(|l| l == line)
        .unwrap_or_else(|| panic!("`{line}` not in {lines:?}"))
}

/// 设计 §12.5："只读的并行，有副作用的与 codemode 顺序执行"；pi harness-tools "runs a round in
/// parallel by default and sequentially when configured or required by a tool"。一轮里连续的只读
/// 调用同时在飞，写调用是屏障：在飞的先收尾，再做它，之后的读才开始。
#[tokio::test(start_paused = true)]
async fn consecutive_reads_overlap_and_a_write_is_a_barrier() {
    let world = World::new();
    let read =
        Arc::new(TestTool::reader("read", &world.log).with_delay(Duration::from_millis(100)));
    let rg = Arc::new(TestTool::reader("rg", &world.log).with_delay(Duration::from_millis(100)));
    let write =
        Arc::new(TestTool::writer("write", &world.log).with_delay(Duration::from_millis(50)));
    world.tools(vec![read, rg, write]);
    world.script.calls(&[
        ("c1", "read", json!({})),
        ("c2", "rg", json!({})),
        ("c3", "write", json!({})),
        ("c4", "read", json!({})),
    ]);
    world.script.reply("done");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let lines = world.log.lines();
    assert!(
        position(&lines, "start rg #1") < position(&lines, "finish read #1"),
        "the two reads overlap: {lines:?}"
    );
    assert!(position(&lines, "start write #1") > position(&lines, "finish rg #1"));
    assert!(position(&lines, "start write #1") > position(&lines, "finish read #1"));
    assert!(
        position(&lines, "start read #2") > position(&lines, "finish write #1"),
        "the read after the write waits for it: {lines:?}"
    );
    // 结果按原始调用顺序交给模型。
    assert_eq!(
        outline(&world.script.requests()[1]),
        [
            "system",
            "user: go",
            "assistant calls read(c1),rg(c2),write(c3),read(c4)",
            "result(c1)",
            "result(c2)",
            "result(c3)",
            "result(c4)",
        ]
    );
    engine.close().await.unwrap();
}

/// 一个工具里读写混着（`komo`）：并发分组按这次调用的参数判断，不按工具名——同名的写调用照样是屏障，
/// 连着的同名读调用照样同时在飞。
#[tokio::test(start_paused = true)]
async fn calls_of_one_mixed_tool_are_grouped_by_their_arguments() {
    let world = World::new();
    let komo = Arc::new(
        TestTool::reader("komo", &world.log)
            .mixed()
            .with_delay(Duration::from_millis(100)),
    );
    world.tools(vec![komo]);
    world.script.calls(&[
        ("c1", "komo", json!({ "read": true })),
        ("c2", "komo", json!({ "read": true })),
        ("c3", "komo", json!({ "read": false })),
        ("c4", "komo", json!({ "read": true })),
    ]);
    world.script.reply("done");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let lines = world.log.lines();
    assert!(
        position(&lines, "start komo #2") < position(&lines, "finish komo #1"),
        "the first two reads overlap: {lines:?}"
    );
    assert!(
        position(&lines, "start komo #3") > position(&lines, "finish komo #2"),
        "the write waits for the reads in flight: {lines:?}"
    );
    assert!(
        position(&lines, "start komo #4") > position(&lines, "finish komo #3"),
        "the read after the write waits for it: {lines:?}"
    );
    engine.close().await.unwrap();
}

/// 设计 §12.5 第一步 + pi harness-tools "answers calls to tools the request did not offer"：能力面之外的
/// 名字等于不认识，得到的是一条列出可用工具的失败结果，Run 接着往下走。
#[tokio::test(start_paused = true)]
async fn a_tool_outside_the_surface_is_refused_with_the_available_names() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    let bash = Arc::new(TestTool::writer("bash", &world.log));
    world.tools(vec![read.clone(), bash.clone()]);
    world.hooks.set_surface(Some(vec!["read"]));
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(bash.runs(), 0);
    assert_eq!(bash.prepares.load(std::sync::atomic::Ordering::SeqCst), 0);
    let result = &world.script.requests()[1][3];
    let Message::ToolResult(message) = result else {
        panic!("tool result expected");
    };
    assert!(message.is_error);
    assert!(
        text_of(result).contains("这次运行的工具集里没有 bash；可用的是：read"),
        "{}",
        text_of(result)
    );
    // 失败结果也落了盘（不是悬着的调用）：结果 entry 的 data 里有 output.json 引用。
    let entries = world.entries_of("komo.tool-result").await;
    assert_eq!(entries[0].data.as_ref().unwrap()["status"], "failed");
    assert!(entries[0].data.as_ref().unwrap()["output"]["path"].is_string());
    engine.close().await.unwrap();
}

/// §8.5：`prepare` 报错的调用是确定没执行过的失败结果，交给模型修正。
#[tokio::test(start_paused = true)]
async fn a_prepare_error_is_a_failed_result_for_the_model() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log).failing_prepare("bad path"));
    world.tools(vec![read.clone()]);
    world.script.calls(&[("c1", "read", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(read.runs(), 0);
    let result = &world.script.requests()[1][3];
    assert!(text_of(result).contains("bad path"), "{}", text_of(result));
    let tasks = world.tasks("komo.tool").await;
    assert!(matches!(
        tasks[0].state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    ));
    engine.close().await.unwrap();
}

/// 设计 §12.5："工具槽位：即意图，带重放策略、尝试序号、子进程身份和输出尾巴"；
/// pi spec §8.2 "A tool task sets its slot running in its intent commit, publishes throttled output
/// ... and in its terminal commit sets done and entry and removes output"。输出尾巴的节流写入走
/// `commit_progress`。
#[tokio::test(start_paused = true)]
async fn the_slot_is_the_intent_and_carries_the_output_tail_while_the_call_runs() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_stdout("building...\n")
            .with_gate(&gate)
            .with_text("built"),
    );
    world.tools(vec![bash]);
    world
        .script
        .calls(&[("c1", "bash", json!({"command": "make"}))]);
    world.script.reply("done");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world
        .until(|| async {
            world
                .live()
                .await
                .tools
                .and_then(|tools| tools[0].output.clone())
                .is_some()
        })
        .await;

    let live = world.live().await;
    let slot = &live.tools.as_ref().unwrap()[0];
    let tool_task = &world.tasks("komo.tool").await[0];
    assert_eq!(slot.call_id, "c1");
    assert_eq!(slot.name, "bash");
    assert_eq!(slot.task_id, Some(tool_task.id));
    assert_eq!(slot.status, SlotStatus::Running);
    assert_eq!(slot.replay, Some(komo_kernel::types::plan::Replay::Unsafe));
    assert_eq!(slot.attempt, Some(1));
    assert_eq!(
        slot.pid, None,
        "a tool that spawns nothing records no child"
    );
    assert_eq!(slot.output.as_deref(), Some("building...\n"));
    // 意图同时在 checkpoint 里：参数、计划、重放策略、尝试序号。
    let TaskState::Running { checkpoint } = &tool_task.state else {
        panic!("running expected");
    };
    assert_eq!(checkpoint["phase"], "execute");
    assert_eq!(checkpoint["arguments"], json!({"command": "make"}));
    assert_eq!(checkpoint["replay"], "unsafe");
    assert_eq!(checkpoint["attempt"], 1);
    assert_eq!(checkpoint["plan"]["tool"], "bash");

    gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();
    let result = &world.entries_of("komo.tool-result").await[0];
    // 槽位随结果收尾：done + entry，输出尾巴清掉（这一轮的槽位随下一个 generation 的接手移除）。
    assert!(
        world
            .storage
            .progress_commits()
            .iter()
            .any(|commit| commit.writes == ["document.change"]),
        "the tail went through commit_progress"
    );
    assert!(world.live().await.tools.is_none());
    assert_eq!(result.data.as_ref().unwrap()["tool"], "bash");
    engine.close().await.unwrap();
}

/// 设计 §4.4：结果的模型视图在收尾时由 `projection.rs` 算一次、存进 entry；完整输出在 blob。
#[tokio::test(start_paused = true)]
async fn the_result_entry_holds_the_projected_view_and_a_reference_to_the_full_output() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log).with_text("file contents"));
    world.tools(vec![read]);
    world.script.calls(&[("c1", "read", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let entry = &world.entries_of("komo.tool-result").await[0];
    let Message::ToolResult(message) = &entry.model.as_ref().unwrap()[0] else {
        panic!("tool result expected");
    };
    assert_eq!(message.tool_call_id, "c1");
    assert_eq!(message.tool_name, "read");
    assert!(!message.is_error);
    let view = text_of(&entry.model.as_ref().unwrap()[0]);
    assert!(view.starts_with("[read · "), "{view}");
    assert!(view.contains("file contents"));
    let data = entry.data.as_ref().unwrap();
    assert_eq!(data["status"], "completed");
    assert_eq!(data["attempt"], 1);
    let task = &world.tasks("komo.tool").await[0];
    assert_eq!(
        data["output"]["path"],
        format!("tool-output/1/{}/1/output.json", task.id)
    );
    assert_eq!(entry.by_task, Some(task.id));
    engine.close().await.unwrap();
}

fn limit_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "limit": {"type": "integer"},
            "all": {"type": "boolean"},
        },
        "required": ["path"],
    })
}

/// P3（`docs/tasks.md` §4.10）：数字写成字符串、可选字段给 null，工具照常执行——`prepare` 收到
/// 的已经是按 schema 转换后的参数。
#[tokio::test(start_paused = true)]
async fn string_numbers_and_optional_nulls_are_coerced_before_prepare() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log).with_parameters(limit_schema()));
    world.tools(vec![read.clone()]);
    world.script.calls(&[(
        "c1",
        "read",
        json!({"path": "a", "limit": "20", "all": "true"}),
    )]);
    world
        .script
        .calls(&[("c2", "read", json!({"path": "b", "limit": null}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(read.runs(), 2);
    assert_eq!(
        *read.prepared_args.lock().unwrap(),
        vec![
            json!({"path": "a", "limit": 20, "all": true}),
            json!({"path": "b"})
        ]
    );
    assert_eq!(
        *read.executed_args.lock().unwrap(),
        *read.prepared_args.lock().unwrap()
    );
    engine.close().await.unwrap();
}

/// 转换发生在计划写进 `execute` 检查点之前：重启后重跑用的是检查点里的计划，参数与首次一致。
#[tokio::test(start_paused = true)]
async fn a_restart_replays_the_coerced_arguments_from_the_execute_checkpoint() {
    let world = World::new();
    let read = Arc::new(
        TestTool::reader("read", &world.log)
            .with_parameters(limit_schema())
            .hanging_first(1),
    );
    world.tools(vec![read.clone()]);
    world
        .script
        .calls(&[("c1", "read", json!({"path": "a", "limit": "20"}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start read #1").await;

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(read.runs(), 2, "the second run is a new attempt");
    let expected = json!({"path": "a", "limit": 20});
    assert_eq!(
        *read.executed_args.lock().unwrap(),
        vec![expected.clone(), expected]
    );
    engine.close().await.unwrap();
}

/// 不能无歧义转换的原样交给 `prepare`（这里不报错，只看收到的没被动过）。
#[tokio::test(start_paused = true)]
async fn ambiguous_arguments_reach_prepare_unchanged() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log).with_parameters(limit_schema()));
    world.tools(vec![read.clone()]);
    let args = json!({"path": "a", "limit": "2.5", "all": "yes"});
    world.script.calls(&[("c1", "read", args.clone())]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(*read.prepared_args.lock().unwrap(), vec![args]);
    engine.close().await.unwrap();
}
