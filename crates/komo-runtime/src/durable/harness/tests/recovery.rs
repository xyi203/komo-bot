//! 恢复：重启发生在每个时刻，结果都对（设计 §5 的表；`docs/komo_bot.md` §8.6）。
//! 「重启」= 关掉 Engine，再用同一个 `MemoryStorage` 重开。对应 pi 的
//! `harness-generation-recovery` / `harness-tools-recovery`。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::durable::{
    EntryRecord, InputState, Message, StopReason, SubmissionKind, TaskOutcome, TaskState,
};
use komo_kernel::projection::{ProjectionContext, ToolResultFacts, project_views, select};
use komo_kernel::traits::ToolOutputStore;
use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
use komo_kernel::types::plan::Replay;
use komo_kernel::types::refs::{AttemptRef, InterruptedOutput};
use serde_json::json;
use tokio::sync::Notify;

use super::support::*;

fn input_state(record: &komo_kernel::durable::SubmissionRecord) -> InputState {
    match &record.kind {
        SubmissionKind::Input { state } => state.clone(),
        other => panic!("an input submission expected, got {other:?}"),
    }
}

fn result_view(entry: &EntryRecord) -> String {
    text_of(&entry.model.as_ref().unwrap()[0])
}

fn only_tool_task(
    tasks: Vec<komo_kernel::durable::TaskRecord>,
) -> komo_kernel::durable::TaskRecord {
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    tasks.into_iter().next().unwrap()
}

/// 设计 §5「工具任务在 call（意图未提交）：确定没执行过，正常执行」；pi harness-tools-recovery
/// "reruns beforeTool when interrupted before intent, and executes once"。
#[tokio::test(start_paused = true)]
async fn a_restart_before_the_intent_commit_runs_the_call_normally_once() {
    let world = World::new();
    let bash = Arc::new(TestTool::writer("bash", &world.log).hanging_first_prepares(1));
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("prepare bash").await;

    engine.close().await.unwrap();
    let task = only_tool_task(world.tasks("komo.tool").await);
    // 关闭不写任何结局：任务停在 running，重开时对账改回 pending，checkpoint 都还是 `call`。
    let (TaskState::Pending { checkpoint } | TaskState::Running { checkpoint }) = &task.state
    else {
        panic!("pending or running expected, got {:?}", task.state);
    };
    assert_eq!(checkpoint["phase"], "call", "no intent was committed");
    assert_eq!(bash.runs(), 0);

    let engine = world.open().await;
    world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(bash.runs(), 1, "executed exactly once");
    let entries = world.entries_of("komo.tool-result").await;
    assert_eq!(entries[0].data.as_ref().unwrap()["status"], "completed");
    engine.close().await.unwrap();
}

/// 设计 §5「工具任务在 execute，没有完整输出：已存与当前 replay 都 Safe → 新尝试重跑」/ §8.6 表第 1 行
/// （safe / safe）；pi harness-tools-recovery "reruns a tool only when both the stored and the current
/// replay policy are safe"。同一个 task，attempt + 1，输出落在新尝试的目录。
#[tokio::test(start_paused = true)]
async fn safe_and_safe_reruns_the_call_as_a_new_attempt() {
    let world = World::new();
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

    assert_eq!(
        read.runs(),
        2,
        "the second run is a new attempt of the same call"
    );
    let run = Some(komo_kernel::durable::legacy_ids::run_id(id));
    assert_eq!(*read.input_runs.lock().unwrap(), vec![run.clone(), run]);
    let tasks = world.tasks("komo.tool").await;
    assert_eq!(tasks.len(), 1, "one task, not a second call");
    let entries = world.entries_of("komo.tool-result").await;
    assert_eq!(entries.len(), 1);
    let data = entries[0].data.as_ref().unwrap();
    assert_eq!(data["attempt"], 2);
    assert_eq!(
        data["output"]["path"],
        format!("tool-output/1/{}/2/output.json", tasks[0].id)
    );
    assert!(matches!(
        tasks[0].state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    ));
    engine.close().await.unwrap();
}

/// 设计 §5 / §8.6 表第 2 行（safe / unsafe）：现在的声明能否决记下的 safe（升级改了声明）→ 不重跑，
/// 中断结果（uncertain）交给模型；pi harness-tools-recovery "reruns a tool only when both ... safe"。
#[tokio::test(start_paused = true)]
async fn a_tool_that_now_says_unsafe_vetoes_a_stored_safe_plan() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log).hanging_first(1));
    world.tools(vec![read.clone()]);
    world.script.calls(&[("c1", "read", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start read #1").await;
    let task = only_tool_task(world.tasks("komo.tool").await);
    let TaskState::Running { checkpoint } = &task.state else {
        panic!("running expected");
    };
    assert_eq!(checkpoint["replay"], "safe", "the plan recorded safe");

    read.set_replay(Replay::Unsafe);
    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(read.runs(), 1, "not rerun");
    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["status"], "uncertain");
    assert!(result_view(entry).contains("上一次执行被 Gateway 重启打断"));
    engine.close().await.unwrap();
}

/// 设计 §5 / §8.6 表第 3 行（unsafe / 任意）：现在的 safe 不能把记下的 unsafe 升级。
#[tokio::test(start_paused = true)]
async fn a_safe_tool_cannot_upgrade_a_stored_unsafe_plan() {
    let world = World::new();
    let bash = Arc::new(TestTool::writer("bash", &world.log).hanging_first(1));
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start bash #1").await;

    bash.set_replay(Replay::Safe);
    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(bash.runs(), 1, "an unsafe plan is never rerun");
    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["status"], "uncertain");
    engine.close().await.unwrap();
}

/// 设计 §5 "否则 `uncertain` 结果附 `.partial` 末尾" + `docs/komo_bot.md` §8.6：中断结果的正文逐字照用
/// （同一个函数写出来的），写明"重启打断、不会自动重跑"，附判断方法与各 2 KiB 的部分输出；
/// 正文在收尾时固化进 `output.json`；任务以 `failed` 收场（取消意图）。
#[tokio::test(start_paused = true)]
async fn the_interrupted_result_carries_the_section_8_6_body_and_the_partial_tails() {
    let world = World::new();
    let bash = Arc::new(TestTool::writer("bash", &world.log).hanging_first(1));
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start bash #1").await;
    let left = InterruptedOutput {
        stdout_bytes: 5000,
        stdout_tail: "tail of stdout!\n".repeat(128),
        stderr_bytes: 13,
        stderr_tail: "boom: failed\n".into(),
    };
    world.mem.leave_interrupted("1", left.clone());

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(bash.runs(), 1);
    let expected = crate::executor::interrupted_message("bash", &left);
    let entry = &world.entries_of("komo.tool-result").await[0];
    let view = result_view(entry);
    assert!(view.contains(&expected), "{view}");
    // §8.6 的正文：打断、不重跑、判断方法、部分输出的标题与各流的大小。
    for fragment in [
        "上一次执行被 Gateway 重启打断，bash 重启后不会自动重跑，副作用是否已经发生未知。",
        "先核对目标的实际状态",
        "发消息、扣款、切换一次这类重复做会出问题、又核对不了的，先问用户",
        "中断前已落盘的部分输出（不是完成结果）：",
        "[stdout 共 5000 字节，末尾 2048 字节]",
        "[stderr 共 13 字节]",
        "boom: failed",
    ] {
        assert!(view.contains(fragment), "missing `{fragment}` in {view}");
    }
    // 正文固化进 output.json；状态是 uncertain，模型看到的是错误。
    let data = entry.data.as_ref().unwrap();
    assert_eq!(data["status"], "uncertain");
    let body = world
        .mem
        .published_body(data["output"]["path"].as_str().unwrap())
        .unwrap();
    assert_eq!(body.preview.as_deref(), Some(expected.as_str()));
    let Message::ToolResult(message) = &entry.model.as_ref().unwrap()[0] else {
        panic!("tool result expected");
    };
    assert!(message.is_error);
    let task = only_tool_task(world.tasks("komo.tool").await);
    assert!(
        matches!(
            task.state,
            TaskState::Terminal {
                outcome: TaskOutcome::Failed { .. }
            }
        ),
        "an interrupted call ends failed: {:?}",
        task.state
    );
    engine.close().await.unwrap();
}

/// 设计 §5「盘上有身份齐全、哈希对得上的 output.json：补提交结果，复用原输出」（今天 §8.4 第 5 行那
/// 一类：blob 写完、结果没提交就崩）。真文件存储：工具返回前让存储开始失败，输出已经落盘而结果
/// 没提交，线失败；重开后复用原输出，工具不再跑。
#[tokio::test]
async fn a_complete_output_on_disk_is_reused_without_rerunning_the_tool() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionId::from_raw("s1");
    let store = Arc::new(komo_store::FileToolOutputStore::new(
        komo_store::SessionPaths::at(dir.path()),
        session,
    ));
    let world = World::with_outputs(store.clone());
    let bash = Arc::new(
        TestTool::writer("bash", &world.log)
            .with_stdout("hello\n")
            .with_text("made it")
            .crashing(&world.storage),
    );
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    while engine.failure().is_none() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let task = only_tool_task(world.tasks("komo.tool").await);
    assert!(
        world.entries_of("komo.tool-result").await.is_empty(),
        "the result was not committed"
    );
    assert!(
        dir.path()
            .join(format!("tool-output/1/{}/1/output.json", task.id))
            .exists(),
        "but the full output is on disk"
    );

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(bash.runs(), 1, "not rerun");
    let entry = &world.entries_of("komo.tool-result").await[0];
    let data = entry.data.as_ref().unwrap();
    assert_eq!(data["attempt"], 1);
    assert_eq!(data["status"], "completed");
    assert!(result_view(entry).contains("made it"));
    assert!(
        data["stdout"]["path"].is_string(),
        "the stream refs came back from disk"
    );
    engine.close().await.unwrap();
}

/// 设计 §5「工具名已经不存在的调用按中断处理」：在途调用的工具名后来没了，没有工具可问，不看重放策略
/// （存下的是 safe 也一样）。
#[tokio::test(start_paused = true)]
async fn a_started_call_to_a_vanished_tool_is_interrupted_even_when_stored_safe() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log).hanging_first(1));
    world.tools(vec![read.clone()]);
    world.script.calls(&[("c1", "read", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start read #1").await;

    world.tools(vec![]);
    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(read.runs(), 1);
    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["status"], "uncertain");
    assert!(result_view(entry).contains("read 重启后不会自动重跑"));
    engine.close().await.unwrap();
}

/// 设计 §5 + §8.6："只落了计划、没 `started` 的确定没执行过，与模型这一轮新发的调用一样，叫到不存在
/// 的工具名照旧被拒，并列出这次能用的工具"。
#[tokio::test(start_paused = true)]
async fn a_planned_only_call_to_a_vanished_tool_is_refused_not_interrupted() {
    let world = World::new();
    let ghost = Arc::new(TestTool::reader("ghost", &world.log).hanging_first_prepares(1));
    let read = Arc::new(TestTool::reader("read", &world.log));
    world.tools(vec![ghost.clone(), read.clone()]);
    world.script.calls(&[("c1", "ghost", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("prepare ghost").await;

    world.tools(vec![read]);
    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(ghost.runs(), 0);
    let entry = &world.entries_of("komo.tool-result").await[0];
    assert_eq!(entry.data.as_ref().unwrap()["status"], "failed");
    assert!(result_view(entry).contains("这次运行的工具集里没有 ghost；可用的是：read"));
    assert!(!result_view(entry).contains("重启打断"));
    engine.close().await.unwrap();
}

/// 设计 §4.4 + D4b 验收："中断结果、full 视图在'刚跑完'与'重启后回放'时逐字节相同"：收尾时算一次、存进
/// entry，所以重启前后读到的是同一份；而且它就是 `projection.rs` 对落盘事实重算的结果（同一个函数）。
#[tokio::test(start_paused = true)]
async fn the_full_view_is_byte_identical_before_and_after_a_restart_and_matches_the_projection() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log).with_text("alpha\nbeta\n"));
    let bash = Arc::new(TestTool::writer("bash", &world.log).hanging_first(1));
    world.tools(vec![read, bash]);
    world
        .script
        .calls(&[("c1", "read", json!({})), ("c2", "bash", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start bash #1").await;
    world.mem.leave_interrupted(
        "1",
        InterruptedOutput {
            stdout_bytes: 4,
            stdout_tail: "half".into(),
            stderr_bytes: 0,
            stderr_tail: String::new(),
        },
    );
    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();
    let before: Vec<_> = world.entries_of("komo.tool-result").await;
    assert_eq!(before.len(), 2);

    // 「刚跑完」那一份是 entry 里存下的；它与对落盘事实重算的结果逐字节相同。
    for entry in &before {
        let data = entry.data.as_ref().unwrap();
        let attempt = AttemptRef {
            session: SessionId::from_raw("s1"),
            run: RunId::from_raw("1"),
            call: ToolCallId::from_raw(entry.by_task.unwrap().to_string()),
            attempt: AttemptId::from_raw(data["attempt"].to_string()),
        };
        let published = world.mem.find_published(&attempt).await.unwrap().unwrap();
        let body = published.body.as_deref().unwrap();
        let tool = data["tool"].as_str().unwrap();
        let facts = ToolResultFacts {
            tool,
            status: published.status,
            elapsed_ms: data["elapsedMs"].as_u64().unwrap(),
            text: body.preview.as_deref(),
            output: &published.output,
            stdout: published.stdout.as_ref(),
            stderr: published.stderr.as_ref(),
            artifacts: &body.artifacts,
            images: &body.images,
        };
        let ctx = ProjectionContext {
            model_result_bytes: komo_kernel::projection::DEFAULT_MODEL_RESULT_BYTES,
            decay: None,
        };
        let recomputed = select(project_views(&facts, &ctx), false);
        assert_eq!(result_view(entry), recomputed, "{tool}");
    }

    // 「重启后回放」：再重启一次，读出的 entry 与消息序列一字不差。
    let snapshot = serde_json::to_string(&before).unwrap();
    let replayed_before = serde_json::to_string(&world.messages().await).unwrap();
    let engine = world.restart(engine).await;
    assert_eq!(
        serde_json::to_string(&world.entries_of("komo.tool-result").await).unwrap(),
        snapshot
    );
    assert_eq!(
        serde_json::to_string(&world.messages().await).unwrap(),
        replayed_before
    );
    engine.close().await.unwrap();
}

/// 设计 §5「生成任务在 request：已提交的部分回复转成 aborted entry；按已提交上下文重发」；pi
/// harness-generation-recovery "converts a committed partial into an aborted entry and resends the same
/// messages"。
#[tokio::test(start_paused = true)]
async fn a_restart_in_request_converts_the_partial_and_resends_the_same_messages() {
    let world = World::new();
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
    let Message::Assistant(aborted) = &assistants[0].model.as_ref().unwrap()[0] else {
        panic!("assistant expected");
    };
    assert_eq!(aborted.stop_reason, StopReason::Aborted);
    assert_eq!(text_of(&assistants[0].model.as_ref().unwrap()[0]), "par");
    assert_eq!(
        text_of(&assistants[1].model.as_ref().unwrap()[0]),
        "whole answer"
    );
    // 被中断的那条不进后续上下文。
    assert_eq!(
        outline(&world.messages().await),
        ["system", "user: hi", "assistant: whole answer"]
    );
    engine.close().await.unwrap();
}

/// pi harness-tools-recovery "answers an aborted tool with its partial output and continues the run"；
/// 设计 §12.4 取消："abort 自底向上，每个任务在自己的 abort handler 里写 aborted 结果"。一轮里没开始
/// 的调用由生成的 abort handler 写 aborted 错误结果，所以下一条输入的请求里每个调用都配上了结果。
#[tokio::test(start_paused = true)]
async fn aborting_a_round_answers_every_call_and_the_next_request_is_fully_paired() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    let slow = Arc::new(
        TestTool::writer("slow", &world.log)
            .with_gate(&gate)
            .with_stdout("so far\n"),
    );
    let after = Arc::new(TestTool::writer("after", &world.log));
    world.tools(vec![slow.clone(), after.clone()]);
    world
        .script
        .calls(&[("c1", "slow", json!({})), ("c2", "after", json!({}))]);
    world.script.reply("again");
    let engine = world.open().await;
    let first = world.submit(&engine, "first").await;
    world.until_log("start slow #1").await;
    world.mem.leave_interrupted(
        "1",
        InterruptedOutput {
            stdout_bytes: 8,
            stdout_tail: "so far\n".into(),
            stderr_bytes: 0,
            stderr_tail: String::new(),
        },
    );
    let queued = world.submit(&engine, "queued").await;

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
    assert!(matches!(
        input_state(&world.submission(queued).await),
        InputState::Unanswered { ref reason, .. } if reason == "aborted"
    ));
    assert_eq!(after.runs(), 0, "the call that never started never ran");
    let results = world.entries_of("komo.tool-result").await;
    assert_eq!(results.len(), 2);
    assert!(result_view(&results[0]).contains("工具 slow 被取消"));
    assert!(result_view(&results[0]).contains("so far"));
    assert!(result_view(&results[1]).contains("工具 after 在开始之前被取消"));
    let live = world.live().await;
    assert!(live.run.is_none() && live.tools.is_none());

    let second = world.submit(&engine, "second").await;
    world.harness.wait(&engine, second).await.unwrap();
    assert_eq!(
        outline(&world.script.requests()[1]),
        [
            "system",
            "user: first",
            "assistant calls slow(c1),after(c2)",
            "result(c1)",
            "result(c2)",
            "user: second",
        ]
    );
    let outcomes: Vec<_> = world
        .tasks("komo.tool")
        .await
        .into_iter()
        .map(|task| {
            matches!(
                task.state,
                TaskState::Terminal {
                    outcome: TaskOutcome::Aborted { .. }
                }
            )
        })
        .collect();
    assert_eq!(outcomes, [true]);
    engine.close().await.unwrap();
}

/// pi harness-tools-recovery "lets context derivation answer a faulted tool and continues the run"；
/// 设计 §5 / spec §5.4：调度器写的 `faulted` 由 `HarnessHooks` 清理（槽位标记完成、没有 entry），
/// 一轮的 `allSettled` 照常往下走，下一次请求里缺的结果由推导规则 8 补成错误结果。
#[tokio::test(start_paused = true)]
async fn a_faulted_tool_is_answered_by_derivation_rule_8_and_the_run_continues() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    let read = Arc::new(TestTool::reader("read", &world.log));
    world.tools(vec![read.clone()]);
    world.script.push(Step::Gate(
        gate.clone(),
        Box::new(Step::Reply(call_reply(&[("c1", "read", json!({}))]))),
    ));
    world.script.reply("carried on");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world
        .until(|| async { world.script.request_count() == 1 })
        .await;
    // 生成已经拿到过它的工具表；工具任务的 `call` 阶段问钩子时失败一次。
    world.hooks.fail_tools_once(super::super::HookError::Failed(
        "the registry blinked".into(),
    ));
    gate.notify_one();
    let settled = world.harness.wait(&engine, id).await.unwrap();

    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    assert_eq!(read.runs(), 0);
    let task = only_tool_task(world.tasks("komo.tool").await);
    assert!(matches!(
        task.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Faulted { .. }
        }
    ));
    assert!(
        world.entries_of("komo.tool-result").await.is_empty(),
        "the faulted task wrote no result"
    );
    let request = &world.script.requests()[1];
    assert_eq!(
        outline(request),
        [
            "system",
            "user: go",
            "assistant calls read(c1)",
            "result(c1)"
        ]
    );
    assert_eq!(
        text_of(&request[3]),
        komo_kernel::durable::context::MISSING_RESULT_TEXT
    );
    engine.close().await.unwrap();
}

/// 设计 §4.5 预算："重启不重置预算"（`docs/komo_bot.md` §8.5）：轮数与 token 计数在 `komo.live.run`
/// 里，和 run 一起提交，重启后接着数。
#[tokio::test(start_paused = true)]
async fn the_run_counters_live_in_komo_live_and_survive_a_restart() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    let bash = Arc::new(TestTool::writer("bash", &world.log).hanging_first(1));
    world.tools(vec![read, bash]);
    world.script.calls(&[("c1", "read", json!({}))]);
    world.script.calls(&[("c2", "bash", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start bash #1").await;

    let run = world.live().await.run.unwrap();
    assert_eq!(run.rounds, 2);
    assert_eq!(run.tokens, 220, "two replies at 100 + 10 tokens each");
    let engine = world.restart(engine).await;
    let after = world.live().await.run.unwrap();
    assert_eq!(after, run, "reopening changes nothing");
    world.harness.wait(&engine, id).await.unwrap();
    assert!(world.live().await.run.is_none());
    engine.close().await.unwrap();
}
