//! `komo.tool` 的 `probe` 阶段（设计 §5「工具任务在 execute，记录的子进程还活着」、§12.4 末条）：
//! 工具起子进程时 pid 经节流进度那条路径写进槽位；重启后子进程还活着就不急着判，每个间隔探一次，
//! 结束了再按 execute 那张表判。探测用 `FakeProbe` 替身；两条身份核对的用例用真进程与真探测。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::durable::{InputState, SubmissionKind, TaskOutcome, TaskState};
use komo_kernel::traits::{ChildStarted, ToolOutputStore};
use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
use komo_kernel::types::refs::{AttemptRef, InterruptedOutput, ToolResultBody, ToolResultStatus};
use serde_json::{Value, json};

use super::support::*;
use crate::recovery::Liveness;

const PID: u32 = 4242;

fn ghost(pid: u32, started_at_ms: i64) -> ChildStarted {
    ChildStarted {
        pid,
        pgid: pid,
        started_at_ms,
    }
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

fn only_task(tasks: Vec<komo_kernel::durable::TaskRecord>) -> komo_kernel::durable::TaskRecord {
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    tasks.into_iter().next().unwrap()
}

async fn checkpoint_of(world: &World) -> Value {
    match only_task(world.tasks("komo.tool").await).state {
        TaskState::Running { checkpoint } | TaskState::Pending { checkpoint } => checkpoint,
        other => panic!("a live task expected, got {other:?}"),
    }
}

/// 等槽位里出现 pid（工具起了进程，进度提交写进去了）。
async fn until_pid_recorded(world: &World) {
    world
        .until(|| async {
            world
                .live()
                .await
                .tools
                .is_some_and(|tools| tools[0].pid.is_some())
        })
        .await;
}

fn view(entry: &komo_kernel::durable::EntryRecord) -> String {
    text_of(&entry.model.as_ref().unwrap()[0])
}

/// 在盘上放一份完整输出：就像那个活着的进程（或接手它的一方）收尾时发布的。
async fn publish_complete_output(world: &World, preview: &str) {
    let task = only_task(world.tasks("komo.tool").await);
    let attempt = AttemptRef {
        session: SessionId::from_raw("s1"),
        run: RunId::from_raw("1"),
        call: ToolCallId::from_raw(task.id.to_string()),
        attempt: AttemptId::from_raw("1"),
    };
    let writer = world.mem.begin(&attempt).await.unwrap();
    world
        .mem
        .publish(
            writer,
            ToolResultBody {
                status: ToolResultStatus::Completed,
                result: Value::Null,
                error: None,
                exit_code: Some(0),
                artifacts: vec![],
                images: vec![],
                preview: Some(preview.into()),
            },
        )
        .await
        .unwrap();
}

async fn settled_unanswered(
    world: &World,
    engine: &crate::durable::Engine,
    id: komo_kernel::durable::SubmissionId,
) -> InputState {
    let record = world.harness.wait(engine, id).await.unwrap();
    match record.kind {
        SubmissionKind::Input { state } => state,
        other => panic!("an input expected, got {other:?}"),
    }
}

/// 设计 §12.5「子进程身份」：工具起了进程，pid、进程组与起来的时刻就进槽位，走 `commit_progress`
/// （不等下一个节流间隔）。
#[tokio::test(start_paused = true)]
async fn a_started_child_is_recorded_in_the_slot_through_the_progress_path() {
    let world = World::new();
    let started = now_ms();
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(ghost(PID, started))
            .hanging_first(1),
    );
    world.tools(vec![bash]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    world.submit(&engine, "go").await;
    until_pid_recorded(&world).await;

    let live = world.live().await;
    let slot = &live.tools.as_ref().unwrap()[0];
    assert_eq!(
        (slot.pid, slot.pgid, slot.started_at),
        (Some(PID), Some(PID), Some(started))
    );
    assert!(
        world
            .storage
            .progress_commits()
            .iter()
            .any(|commit| commit.writes == ["document.change"]),
        "the pid went through commit_progress"
    );
    engine.close().await.unwrap();
}

/// 设计 §5 倒数第三行 + 上一行：子进程还活着 → `probe`，每个间隔探一次，不重跑；结束时盘上已有完整
/// `output.json` → 补提交结果，复用原输出。
#[tokio::test(start_paused = true)]
async fn a_live_child_keeps_the_task_probing_and_a_complete_output_is_reused_once_it_ends() {
    let world = World::new();
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(ghost(PID, now_ms()))
            .hanging_first(1),
    );
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    world.probe.set(Liveness::Alive);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    until_pid_recorded(&world).await;

    let engine = world.restart(engine).await;
    tokio::time::sleep(Duration::from_secs(12)).await;

    let checkpoint = checkpoint_of(&world).await;
    assert_eq!(checkpoint["phase"], "probe");
    assert_eq!(checkpoint["child"]["pid"], PID);
    assert_eq!(checkpoint["attempt"], 1);
    let asked = world.probe.asked();
    assert!(
        asked.len() >= 3,
        "probed once per interval: {}",
        asked.len()
    );
    assert!(asked.iter().all(|child| child.pid == PID));
    assert!(
        asked[0].started_at.is_some(),
        "the identity travels with the pid"
    );
    assert_eq!(bash.runs(), 1, "never rerun while the child lives");
    assert!(world.entries_of("komo.tool-result").await.is_empty());

    publish_complete_output(&world, "survivor finished").await;
    world.probe.set(Liveness::Gone);
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(bash.runs(), 1);
    let entry = &world.entries_of("komo.tool-result").await[0];
    let data = entry.data.as_ref().unwrap();
    assert_eq!(
        (data["status"].as_str(), data["attempt"].as_u64()),
        (Some("completed"), Some(1))
    );
    assert!(view(entry).contains("survivor finished"), "{}", view(entry));
    engine.close().await.unwrap();
}

/// 设计 §5："进程结束了再按上一行判……否则 `uncertain` 带 `.partial` 末尾"。
#[tokio::test(start_paused = true)]
async fn a_child_that_ended_without_output_makes_the_call_uncertain_with_the_partial_tail() {
    let world = World::new();
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(ghost(PID, now_ms()))
            .hanging_first(1),
    );
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    world.probe.set(Liveness::Alive);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    until_pid_recorded(&world).await;
    world.mem.leave_interrupted(
        "1",
        InterruptedOutput {
            stdout_bytes: 12,
            stdout_tail: "half done...\n".into(),
            stderr_bytes: 0,
            stderr_tail: String::new(),
        },
    );

    let engine = world.restart(engine).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(checkpoint_of(&world).await["phase"], "probe");
    world.probe.set(Liveness::Gone);
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(bash.runs(), 1, "an unsafe call is never rerun");
    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["status"], "uncertain");
    assert!(view(entry).contains("half done..."), "{}", view(entry));
    let task = only_task(world.tasks("komo.tool").await);
    assert!(matches!(
        task.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Failed { .. }
        }
    ));
    engine.close().await.unwrap();
}

/// 设计 §5 的 safe / safe 行放在 probe 之后也成立：子进程结束、没有输出、双 Safe → 新尝试重跑。
#[tokio::test(start_paused = true)]
async fn a_safe_call_is_rerun_as_a_new_attempt_after_its_child_ends() {
    let world = World::new();
    let read = Arc::new(
        TestTool::reader("read", &world.log)
            .with_child(ghost(PID, now_ms()))
            .hanging_first(1),
    );
    world.tools(vec![read.clone()]);
    world.script.calls(&[("c1", "read", json!({}))]);
    world.script.reply("ok");
    world.probe.set(Liveness::Alive);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    until_pid_recorded(&world).await;

    let engine = world.restart(engine).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(checkpoint_of(&world).await["phase"], "probe");
    world.probe.set(Liveness::Gone);
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(read.runs(), 2);
    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["attempt"], 2);
    engine.close().await.unwrap();
}

/// 重启发生在 probe 中途：检查点里有子进程身份与下一次探测时刻，重开后接着探，不重判、不重跑。
#[tokio::test(start_paused = true)]
async fn a_restart_in_the_middle_of_probing_resumes_probing() {
    let world = World::new();
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(ghost(PID, now_ms()))
            .hanging_first(1),
    );
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    world.probe.set(Liveness::Alive);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    until_pid_recorded(&world).await;

    let mut engine = world.restart(engine).await;
    for _ in 0..2 {
        tokio::time::sleep(Duration::from_secs(7)).await;
        assert_eq!(checkpoint_of(&world).await["phase"], "probe");
        engine = world.restart(engine).await;
    }
    tokio::time::sleep(Duration::from_secs(7)).await;
    let checkpoint = checkpoint_of(&world).await;
    assert_eq!(checkpoint["phase"], "probe");
    assert_eq!(checkpoint["child"]["pid"], PID);
    assert_eq!(bash.runs(), 1);

    world.probe.set(Liveness::Gone);
    world.harness.wait(&engine, id).await.unwrap();
    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["status"], "uncertain");
    assert_eq!(bash.runs(), 1);
    engine.close().await.unwrap();
}

/// 探测答"查不出来"按还在处理（§8.7：阻止重复启动比重复执行安全）。
#[tokio::test(start_paused = true)]
async fn an_unverifiable_child_is_treated_as_still_running() {
    let world = World::new();
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(ghost(PID, now_ms()))
            .hanging_first(1),
    );
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    world.probe.set(Liveness::Unknown);
    let engine = world.open().await;
    world.submit(&engine, "go").await;
    until_pid_recorded(&world).await;

    let engine = world.restart(engine).await;
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert_eq!(checkpoint_of(&world).await["phase"], "probe");
    assert!(world.entries_of("komo.tool-result").await.is_empty());
    engine.close().await.unwrap();
}

/// 取消时 probe 阶段干净结束：写 `aborted` 结果（带 `.partial` 末尾），任务 `aborted`，槽位清掉，
/// 结果正文说明那个子进程取消时还在、komo 没有终止它；Run 照常收尾。
#[tokio::test(start_paused = true)]
async fn aborting_while_probing_ends_cleanly_and_says_the_child_is_still_running() {
    let world = World::new();
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(ghost(PID, now_ms()))
            .hanging_first(1),
    );
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.probe.set(Liveness::Alive);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    until_pid_recorded(&world).await;
    world.mem.leave_interrupted(
        "1",
        InterruptedOutput {
            stdout_bytes: 5,
            stdout_tail: "part\n".into(),
            stderr_bytes: 0,
            stderr_tail: String::new(),
        },
    );
    let engine = world.restart(engine).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(checkpoint_of(&world).await["phase"], "probe");

    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    let state = settled_unanswered(&world, &engine, id).await;
    assert!(matches!(state, InputState::Unanswered { ref reason, .. } if reason == "aborted"));

    let entry = &world.entries_of("komo.tool-result").await[0];
    let text = view(entry);
    assert!(text.contains("工具 bash 被取消"), "{text}");
    assert!(text.contains("part"), "{text}");
    assert!(text.contains(&format!("pid {PID}")), "{text}");
    let task = only_task(world.tasks("komo.tool").await);
    assert!(matches!(
        task.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Aborted { .. }
        }
    ));
    let live = world.live().await;
    assert!(live.run.is_none() && live.tools.is_none());
    assert_eq!(bash.runs(), 1);
    engine.close().await.unwrap();
}

/// pid 被复用：同一个 pid、起来的时刻对不上，探测说"不是那个进程"，按已结束处理——不进 probe
/// 循环。用真进程与真探测。
#[tokio::test(start_paused = true)]
async fn a_reused_pid_with_a_different_start_time_counts_as_ended() {
    let mut stranger = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let world = World::new();
    // 登记的起来时刻是一小时前：这个 pid 现在属于别人。
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(ghost(stranger.id(), now_ms() - 3_600_000))
            .hanging_first(1),
    );
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    world.probe.delegate();
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    until_pid_recorded(&world).await;

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(bash.runs(), 1);
    assert_eq!(world.probe.asked().len(), 1, "one probe, no loop");
    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["status"], "uncertain");
    stranger.kill().unwrap();
    stranger.wait().unwrap();
    engine.close().await.unwrap();
}

/// 对照：起来的时刻对得上的真进程是"那个进程"，探测一直说还在；它退出之后才继续。
#[tokio::test(start_paused = true)]
async fn a_real_child_keeps_the_task_probing_until_it_exits() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let world = World::new();
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_child(ghost(child.id(), now_ms()))
            .hanging_first(1),
    );
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    world.probe.delegate();
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    until_pid_recorded(&world).await;

    let engine = world.restart(engine).await;
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert_eq!(checkpoint_of(&world).await["phase"], "probe");
    assert!(world.entries_of("komo.tool-result").await.is_empty());

    child.kill().unwrap();
    child.wait().unwrap();
    world.harness.wait(&engine, id).await.unwrap();
    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["status"], "uncertain");
    assert_eq!(bash.runs(), 1);
    engine.close().await.unwrap();
}

/// 没有记录子进程的调用恢复时不进 probe（旧规则不变）。
#[tokio::test(start_paused = true)]
async fn a_call_without_a_recorded_child_never_probes() {
    let world = World::new();
    let bash = Arc::new(TestTool::writer("bash", &world.log).hanging_first(1));
    world.tools(vec![bash]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    world.probe.set(Liveness::Alive);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start bash #1").await;

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();
    assert!(world.probe.asked().is_empty());
    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["status"], "uncertain");
    engine.close().await.unwrap();
}
