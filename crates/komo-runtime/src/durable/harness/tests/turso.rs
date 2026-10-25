//! 生成与工具任务跑在 Turso 文件库上（komo-store 的 `TursoStorage`，临时目录）：D4b 的用例原本只在
//! `MemoryStorage` 上跑过，这里补接线——一条输入走完一轮工具调用到 final boundary；工具任务在
//! execute 中途"重启"（关库、重新开库、重建引擎）后按设计 §5 恢复。真时钟、真文件存储。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::durable::{InputState, SubmissionKind, TaskOutcome, TaskState};
use komo_kernel::traits::ChildStarted;
use serde_json::json;

use super::super::{HarnessSettings, SlotStatus};
use super::support::*;
use crate::recovery::Liveness;

fn input_state(record: &komo_kernel::durable::SubmissionRecord) -> InputState {
    match &record.kind {
        SubmissionKind::Input { state } => state.clone(),
        other => panic!("an input submission expected, got {other:?}"),
    }
}

fn terminal_completed(task: &komo_kernel::durable::TaskRecord) -> bool {
    matches!(
        task.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    )
}

/// 设计 §4.3–§4.5 / §12.3：一条输入 = submission + run + generation 任务，模型要了一轮工具，
/// 结果 entry 配上调用，下一轮回答，final boundary 收尾。每一步都在 Turso 的提交线上。
#[tokio::test]
async fn one_input_runs_a_tool_round_to_the_final_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let read = Arc::new(TestTool::reader("read", &world.log).with_text("file contents"));
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_stdout("hello\n")
            .with_text("made it"),
    );
    world.tools(vec![read.clone(), bash.clone()]);
    world
        .script
        .calls(&[("c1", "read", json!({})), ("c2", "bash", json!({}))]);
    world.script.reply("all done");
    let engine = world.open().await;
    let id = world.submit(&engine, "do the thing").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    let InputState::Done { .. } = input_state(&settled) else {
        panic!("done expected, got {:?}", input_state(&settled));
    };
    assert_eq!(
        outline(&world.messages().await),
        [
            "system",
            "user: do the thing",
            "assistant calls read(c1),bash(c2)",
            "result(c1)",
            "result(c2)",
            "assistant: all done",
        ]
    );
    // 第二次请求看到的上下文：调用与结果逐个配对。
    assert_eq!(
        outline(&world.script.requests()[1]),
        [
            "system",
            "user: do the thing",
            "assistant calls read(c1),bash(c2)",
            "result(c1)",
            "result(c2)",
        ]
    );
    let results = world.entries_of("komo.tool-result").await;
    assert_eq!(results.len(), 2);
    for result in &results {
        let path = result.data.as_ref().unwrap()["output"]["path"]
            .as_str()
            .unwrap();
        assert!(dir.path().join(path).exists(), "{path} is on disk");
    }
    assert!(
        world
            .tasks("komo.tool")
            .await
            .iter()
            .all(terminal_completed)
    );
    assert!(
        world
            .tasks("komo.generation")
            .await
            .iter()
            .all(terminal_completed)
    );
    let live = world.live().await;
    assert!(live.run.is_none() && live.tools.is_none(), "{live:?}");
    engine.close().await.unwrap();
}

/// 设计 §5「生成任务在 request」：节流提交的部分回复（进度提交，`synchronous=OFF`）在关库、
/// 重新开库后还在，转成 `aborted` entry，按同一份已提交上下文重发。
#[tokio::test]
async fn a_restart_in_request_converts_the_partial_after_reopening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    world.script.push(Step::Stream {
        partials: vec![partial("par")],
        gap: Duration::from_millis(10),
        end: Box::new(Step::Hang),
    });
    world.script.reply("whole answer");
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    world
        .until(|| async {
            world
                .live()
                .await
                .generation
                .is_some_and(|g| g.message.is_some())
        })
        .await;

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    let requests = world.script.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0], requests[1],
        "the same committed context is resent"
    );
    let assistants = world.entries_of("komo.assistant").await;
    assert_eq!(assistants.len(), 2);
    assert_eq!(text_of(&assistants[0].model.as_ref().unwrap()[0]), "par");
    assert_eq!(
        outline(&world.messages().await),
        ["system", "user: hi", "assistant: whole answer"]
    );
    engine.close().await.unwrap();
}

/// 设计 §5「工具任务在 execute，没有完整输出：否则 `uncertain` 带 `.partial` 末尾」：真盘上
/// `.partial` 里留着中断前写的字节；关库、重新开库、重建引擎后，模型看到的是中断结果，工具没有重跑。
#[tokio::test]
async fn an_unsafe_call_interrupted_mid_execute_is_uncertain_after_reopening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_stdout("half way there\n")
            .hanging_first(1),
    );
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start bash #1").await;

    let engine = world.restart(engine).await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    assert_eq!(bash.runs(), 1, "an unsafe call is not rerun");
    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["status"], "uncertain");
    let view = text_of(&entry.model.as_ref().unwrap()[0]);
    assert!(view.contains("上一次执行被 Gateway 重启打断"), "{view}");
    assert!(view.contains("half way there"), "the .partial tail: {view}");
    engine.close().await.unwrap();
}

/// 设计 §5 safe / safe：新尝试重跑，输出落在 `tool-output/{conv}/{task}/2/`。
#[tokio::test]
async fn a_safe_call_interrupted_mid_execute_is_rerun_as_a_new_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let read = Arc::new(
        TestTool::reader("read", &world.log)
            .hanging_first(1)
            .with_stdout("part"),
    );
    world.tools(vec![read.clone()]);
    world.script.calls(&[("c1", "read", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start read #1").await;

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(read.runs(), 2);
    let tasks = world.tasks("komo.tool").await;
    assert_eq!(tasks.len(), 1);
    let data = world.entries_of("komo.tool-result").await[0]
        .data
        .clone()
        .unwrap();
    assert_eq!(data["attempt"], 2);
    let path = data["output"]["path"].as_str().unwrap();
    assert_eq!(path, format!("tool-output/1/{}/2/output.json", tasks[0].id));
    assert!(dir.path().join(path).exists());
    engine.close().await.unwrap();
}

/// 子进程身份经节流进度那条路径（`synchronous=OFF`）写进槽位，关库再开库后还在；`probe` 阶段读它，
/// 子进程结束后按 §5 判。
#[tokio::test]
async fn the_child_pid_survives_reopening_the_database_and_drives_the_probe_phase() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let started = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(ChildStarted {
                pid: 4242,
                pgid: 4242,
                started_at_ms: started,
            })
            .with_stdout("working\n")
            .hanging_first(1),
    );
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    world.probe.set(Liveness::Alive);
    // 真时钟：默认的 5 秒探测间隔缩短到 20 毫秒。
    world.harness.deps().set_settings(HarnessSettings {
        probe_interval: Duration::from_millis(20),
        ..HarnessSettings::default()
    });
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world
        .until(|| async {
            world
                .live()
                .await
                .tools
                .is_some_and(|tools| tools[0].pid.is_some() && tools[0].output.is_some())
        })
        .await;

    let engine = world.restart(engine).await;
    let live = world.live().await;
    let slot = &live.tools.as_ref().unwrap()[0];
    assert_eq!(slot.status, SlotStatus::Running);
    assert_eq!(
        (slot.pid, slot.pgid, slot.started_at),
        (Some(4242), Some(4242), Some(started))
    );
    world
        .until(|| async { !world.probe.asked().is_empty() })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(world.entries_of("komo.tool-result").await.is_empty());
    assert_eq!(bash.runs(), 1, "not rerun while the child lives");

    world.probe.set(Liveness::Gone);
    world.harness.wait(&engine, id).await.unwrap();

    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["status"], "uncertain");
    assert_eq!(bash.runs(), 1);
    engine.close().await.unwrap();
}

/// 取消在 Turso 上也干净：abort 自底向上，工具写 `aborted` 结果，输入 `unanswered{aborted}`，下一条
/// 输入照常跑完。
#[tokio::test]
async fn aborting_a_running_call_settles_the_input_and_the_next_one_runs() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_stdout("so far\n")
            .hanging_first(1),
    );
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("fresh start");
    let engine = world.open().await;
    let first = world.submit(&engine, "first").await;
    world.until_log("start bash #1").await;

    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    let settled = world.harness.wait(&engine, first).await.unwrap();
    assert!(matches!(
        input_state(&settled),
        InputState::Unanswered { ref reason, .. } if reason == "aborted"
    ));
    let entry = &world.entries_of("komo.tool-result").await[0];
    let view = text_of(&entry.model.as_ref().unwrap()[0]);
    assert!(view.contains("工具 bash 被取消"), "{view}");
    assert!(view.contains("so far"), "{view}");

    let second = world.submit(&engine, "second").await;
    let settled = world.harness.wait(&engine, second).await.unwrap();
    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    engine.close().await.unwrap();
}
