//! `komo.generation`：prepare / request / retry / 预算 / 截断 / 失败（设计 §4.5；pi spec §8.3）。
//! 对应 pi 的 `harness-generation` / `harness-generation-recovery`。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::durable::{
    InputState, Message, StopReason, SubmissionKind, TaskOutcome, TaskState,
};
use serde_json::json;
use tokio::sync::Notify;

use super::super::PromptSection;
use super::super::hooks::{HookError, RetryPolicy, RunBudget};
use super::support::*;

fn input_state(record: &komo_kernel::durable::SubmissionRecord) -> InputState {
    match &record.kind {
        SubmissionKind::Input { state } => state.clone(),
        other => panic!("an input submission expected, got {other:?}"),
    }
}

fn unanswered(record: &komo_kernel::durable::SubmissionRecord) -> (String, Option<String>) {
    match input_state(record) {
        InputState::Unanswered { reason, detail, .. } => (
            reason,
            detail.map(|d| d.as_str().map_or_else(|| d.to_string(), str::to_owned)),
        ),
        other => panic!("unanswered expected, got {other:?}"),
    }
}

fn outcome(task: &komo_kernel::durable::TaskRecord) -> &TaskOutcome {
    match &task.state {
        TaskState::Terminal { outcome } => outcome,
        other => panic!("terminal expected, got {other:?}"),
    }
}

fn assistant_stop(entry: &komo_kernel::durable::EntryRecord) -> StopReason {
    match &entry.model.as_ref().unwrap()[0] {
        Message::Assistant(message) => message.stop_reason,
        other => panic!("assistant expected, got {other:?}"),
    }
}

/// 设计 §3.6 / D4b 验收："进度提交真的走 commit_progress"；pi harness-generation "commits partials
/// no more often than progress.partialIntervalMs"。部分回复节流提交：只替换 `komo.live` 的整值，
/// 所以经 `Line` 走 `commit_progress`；结构性提交（entry、任务、submission）从不走它。
#[tokio::test(start_paused = true)]
async fn a_partial_reply_is_throttled_and_committed_through_commit_progress() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    let partials: Vec<_> = ["a", "ab", "abc", "abcd", "abcde"].map(partial).into();
    world.script.push(Step::Stream {
        partials,
        gap: Duration::from_millis(100),
        end: Box::new(Step::Gate(
            gate.clone(),
            Box::new(Step::Reply(text_reply("abcde"))),
        )),
    });
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
    let live = world.live().await;
    let message = live.generation.unwrap().message.unwrap();
    assert_eq!(message["content"][0]["type"], "text");
    gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();

    let live = world.live().await;
    assert!(
        live.generation.is_none(),
        "the partial is cleared with the outcome"
    );
    let progress = world.storage.progress_commits();
    assert!(!progress.is_empty(), "partials were committed");
    // 500 ms 的流、250 ms 的节流：至多三次，绝不是每个 token 一次。
    assert!(progress.len() <= 3, "{} progress commits", progress.len());
    for commit in &progress {
        assert_eq!(commit.writes, ["document.change"], "{commit:?}");
    }
    // 反过来：任何带 entry / 任务 / submission 的提交都是结构性的。
    for commit in world.storage.commit_log() {
        let structural = commit
            .writes
            .iter()
            .any(|kind| ["entry", "task", "submission", "document.create"].contains(kind));
        if structural {
            assert!(!commit.progress, "{commit:?}");
        }
    }
    engine.close().await.unwrap();
}

/// pi harness-generation "converts the committed partial when aborted during streaming"；设计 §4.5
/// request 一行：已节流提交的部分回复转成 `aborted` 的 assistant entry。
#[tokio::test(start_paused = true)]
async fn aborting_a_stream_converts_the_committed_partial_and_settles_the_input() {
    let world = World::new();
    world.script.push(Step::Stream {
        partials: vec![partial("half an ans")],
        gap: Duration::from_millis(10),
        end: Box::new(Step::Hang),
    });
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

    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    let settled = world.harness.wait(&engine, id).await.unwrap();

    assert_eq!(unanswered(&settled).0, "aborted");
    let assistants = world.entries_of("komo.assistant").await;
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistant_stop(&assistants[0]), StopReason::Aborted);
    assert_eq!(
        text_of(&assistants[0].model.as_ref().unwrap()[0]),
        "half an ans"
    );
    let tasks = world.tasks("komo.generation").await;
    assert!(matches!(outcome(&tasks[0]), TaskOutcome::Aborted { .. }));
    let live = world.live().await;
    assert!(live.run.is_none() && live.generation.is_none());
    engine.close().await.unwrap();
}

/// 设计 §4.5 retry、D4b 验收："模型错误的退避与 10 次后失败"；沿用 `RetryBudget`：2 s 起、封顶
/// 60 s、10 次。pi harness-generation "fails with model_error once retries are exhausted"。
#[tokio::test(start_paused = true)]
async fn model_errors_back_off_from_two_to_sixty_seconds_and_fail_after_ten_attempts() {
    let world = World::new();
    for _ in 0..10 {
        world.script.push(Step::Error(rejected(503)));
    }
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    let times = world.script.request_times();
    assert_eq!(times.len(), 10);
    let gaps: Vec<i64> = times.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert_eq!(
        gaps,
        [
            2_000, 4_000, 8_000, 16_000, 32_000, 60_000, 60_000, 60_000, 60_000
        ]
    );
    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "model_error");
    let detail = detail.unwrap();
    assert!(detail.contains("重试预算已用完（10/10）"), "{detail}");
    // 每次失败的尝试都是一条 `error` 的 assistant entry，不进之后的上下文。
    let failures = world.entries_of("komo.assistant").await;
    assert_eq!(failures.len(), 10);
    assert!(
        failures
            .iter()
            .all(|e| assistant_stop(e) == StopReason::Error)
    );
    let tasks = world.tasks("komo.generation").await;
    assert!(matches!(outcome(&tasks[0]), TaskOutcome::Failed { .. }));
    engine.close().await.unwrap();
}

/// D4b 验收："成功一轮后计数清零"；pi harness-generation "retries a retryable error after a
/// durable backoff and then answers"。
#[tokio::test(start_paused = true)]
async fn a_successful_round_clears_the_retry_count() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    world.tools(vec![read]);
    world.script.push(Step::Error(rejected(503)));
    world.script.calls(&[("c1", "read", json!({}))]);
    world.script.push(Step::Error(rejected(503)));
    world.script.reply("finally");
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    world.harness.wait(&engine, id).await.unwrap();

    let times = world.script.request_times();
    assert_eq!(times.len(), 4);
    assert_eq!(times[1] - times[0], 2_000, "the first retry waits 2 s");
    assert_eq!(
        times[3] - times[2],
        2_000,
        "after a successful round the count starts over: 2 s again, not 4 s"
    );
    engine.close().await.unwrap();
}

/// 设计 §5 retry 一行："保留次数与到点时刻"；pi harness-generation-recovery "resumes a retry
/// backoff after reopen"。
#[tokio::test(start_paused = true)]
async fn a_restart_during_a_backoff_keeps_the_attempt_and_the_deadline() {
    let world = World::new();
    world.script.push(Step::Error(rejected(503)));
    world.script.push(Step::Error(rejected(503)));
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    world
        .until(|| async {
            world
                .live()
                .await
                .generation
                .is_some_and(|g| g.retry.is_some())
        })
        .await;
    let first = world.script.request_times()[0];
    let due = world.live().await.generation.unwrap().retry.unwrap().at;
    assert_eq!(due, first + 2_000);

    let engine = world.restart(engine).await;
    // 重开之后 checkpoint 还是 `retry`，到点时刻没变。
    let task = &world.tasks("komo.generation").await[0];
    let TaskState::Pending { checkpoint } = &task.state else {
        panic!("pending expected after reopen, got {:?}", task.state);
    };
    assert_eq!(checkpoint["phase"], "retry");
    assert_eq!(checkpoint["until"], due);
    world.harness.wait(&engine, id).await.unwrap();

    let times = world.script.request_times();
    assert_eq!(
        times[1], due,
        "the retry fires at the stored deadline, not earlier"
    );
    assert_eq!(
        times[2] - times[1],
        4_000,
        "the attempt count survived the restart: the second failure waits 4 s"
    );
    engine.close().await.unwrap();
}

/// 设计 §4.5 prepare："重试回到这里，所以配置的变化在下一次尝试生效"；pi harness-generation "retry
/// sleeps until `until`, then returns to `prepare`"。模型与 effort 在一次请求里冻结。
#[tokio::test(start_paused = true)]
async fn a_retry_prepares_again_so_a_new_model_applies_from_the_next_attempt() {
    let world = World::new();
    world.script.push(Step::Error(rejected(503)));
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    world
        .until(|| async {
            world
                .live()
                .await
                .generation
                .is_some_and(|g| g.retry.is_some())
        })
        .await;
    let mut other = model_config();
    other.model = "better-model".into();
    world.hooks.set_model(Some(other.clone()));
    world.harness.wait(&engine, id).await.unwrap();

    let models = world.script.models();
    assert_eq!(models[0], model_config());
    assert_eq!(models[1], other);
    engine.close().await.unwrap();
}

/// §6 / `AgentLoop`："模型回复被截断，调用参数未收齐——不按半轮调用执行"。截断的一轮不执行任何
/// 调用，也不进之后的上下文。
#[tokio::test(start_paused = true)]
async fn a_truncated_reply_fails_the_run_without_running_its_calls() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    world.tools(vec![read.clone()]);
    let mut truncated = text_reply("a long ans");
    truncated.stop_reason = StopReason::Length;
    world.script.push(Step::Reply(truncated));
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "model_error");
    assert!(detail.unwrap().contains("模型回复被截断"));
    assert_eq!(read.runs(), 0);
    assert!(world.tasks("komo.tool").await.is_empty());
    let assistants = world.entries_of("komo.assistant").await;
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistant_stop(&assistants[0]), StopReason::Error);
    assert!(
        world
            .messages()
            .await
            .iter()
            .all(|m| !matches!(m, Message::Assistant(_))),
        "the truncated reply never reaches a later request"
    );
    engine.close().await.unwrap();
}

/// pi harness-generation "fails with no_model when no model is configured or the model is unknown"。
#[tokio::test(start_paused = true)]
async fn no_model_fails_the_input_with_no_model() {
    let world = World::new();
    world.hooks.set_model(None);
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "no_model");
    assert_eq!(detail.unwrap(), "No model is configured");
    assert_eq!(world.script.request_count(), 0);
    engine.close().await.unwrap();
}

/// pi harness-generation "faults a run task, settling its inputs and converting the committed
/// partial"：钩子失败只让这个任务 `faulted`；调度器写的结局由 `HarnessHooks` 清理 run。
#[tokio::test(start_paused = true)]
async fn a_hook_failure_faults_the_generation_and_settles_its_input() {
    let world = World::new();
    world
        .hooks
        .set_failure(Some(HookError::Failed("the registry is on fire".into())));
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "faulted");
    assert!(detail.unwrap().contains("the registry is on fire"));
    let tasks = world.tasks("komo.generation").await;
    assert!(matches!(outcome(&tasks[0]), TaskOutcome::Faulted { .. }));
    assert!(world.live().await.run.is_none());
    engine.close().await.unwrap();
}

/// 设计 §4.5 预算："预算（轮数、token）……照现有规则"，重启不重置（§8.5）：计数在 `komo.live.run`。
#[tokio::test(start_paused = true)]
async fn the_round_budget_stops_a_run_before_its_next_request() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    world.tools(vec![read]);
    world.hooks.set_budget(RunBudget {
        max_rounds: Some(2),
        ..RunBudget::default()
    });
    world.script.calls(&[("c1", "read", json!({}))]);
    world.script.calls(&[("c2", "read", json!({}))]);
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "budget");
    assert_eq!(detail.unwrap(), "超过总轮数预算（2）");
    assert_eq!(world.script.request_count(), 2, "no third request");
    engine.close().await.unwrap();
}

/// 设计 §4.5 预算 + D4b 验收："derive_context 产出的消息喂给 ModelStream 时调用与结果配对完整，取消或
/// 失败留下的半轮由推导规则 8 补上"：token 预算在回复落账之后、调用执行之前停下 run，那条带调用的
/// assistant entry 留在 transcript 里没有结果；下一条输入的请求里每个调用都有一条补出来的错误结果。
#[tokio::test(start_paused = true)]
async fn a_half_round_left_by_a_budget_stop_is_paired_by_derivation_rule_8() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    world.tools(vec![read.clone()]);
    world.hooks.set_budget(RunBudget {
        max_tokens: Some(50),
        ..RunBudget::default()
    });
    world
        .script
        .calls(&[("c1", "read", json!({})), ("c2", "read", json!({}))]);
    world.script.reply("fine");
    let engine = world.open().await;
    let first = world.submit(&engine, "one").await;
    let settled = world.harness.wait(&engine, first).await.unwrap();
    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "budget");
    assert_eq!(detail.unwrap(), "超过 token 预算（50）");
    assert_eq!(read.runs(), 0, "the calls of the stopped round never ran");

    world.hooks.set_budget(RunBudget::default());
    let second = world.submit(&engine, "two").await;
    world.harness.wait(&engine, second).await.unwrap();

    let request = &world.script.requests()[1];
    assert_eq!(
        outline(request),
        [
            "system",
            "user: one",
            "assistant calls read(c1),read(c2)",
            "result(c1)",
            "result(c2)",
            "user: two",
        ]
    );
    for message in &request[3..5] {
        let Message::ToolResult(result) = message else {
            panic!("tool result expected");
        };
        assert!(result.is_error);
        assert_eq!(
            text_of(message),
            komo_kernel::durable::context::MISSING_RESULT_TEXT
        );
    }
    engine.close().await.unwrap();
}

/// pi harness-generation "renders sections ..." + 设计 §4.5 prepare："变了才写 `komo.system` entry"。
#[tokio::test(start_paused = true)]
async fn the_system_entry_is_appended_only_when_the_prompt_or_tools_change() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    let bash = Arc::new(TestTool::writer("bash", &world.log));
    world.tools(vec![read.clone()]);
    world.script.reply("one");
    world.script.reply("two");
    world.script.reply("three");
    world.script.reply("four");
    let engine = world.open().await;

    for text in ["a", "b"] {
        let id = world.submit(&engine, text).await;
        world.harness.wait(&engine, id).await.unwrap();
    }
    assert_eq!(
        world.entries_of("komo.system").await.len(),
        1,
        "the prompt did not change"
    );

    world.hooks.set_sections(vec![
        PromptSection::new("preamble", "You are a test agent."),
        PromptSection::new("cwd", "/work"),
    ]);
    let id = world.submit(&engine, "c").await;
    world.harness.wait(&engine, id).await.unwrap();
    let systems = world.entries_of("komo.system").await;
    assert_eq!(systems.len(), 2);
    let Message::System(patch) = &systems[1].model.as_ref().unwrap()[0] else {
        panic!("system expected");
    };
    assert_eq!(
        serde_json::to_value(&patch.sections).unwrap(),
        json!({"cwd": "/work"}),
        "only the new section is in the patch"
    );

    world.tools(vec![read, bash]);
    let id = world.submit(&engine, "d").await;
    world.harness.wait(&engine, id).await.unwrap();
    let systems = world.entries_of("komo.system").await;
    assert_eq!(systems.len(), 3);
    let Message::System(patch) = &systems[2].model.as_ref().unwrap()[0] else {
        panic!("system expected");
    };
    assert!(patch.sections.is_none());
    assert_eq!(patch.tools_added.as_ref().unwrap()[0]["name"], "bash");
    engine.close().await.unwrap();
}

/// 设计 §4.5 request："checkpoint 记 `cutoff`、模型、effort"：一次请求发的就是 cutoff 之前的上下文。
#[tokio::test(start_paused = true)]
async fn the_request_checkpoint_pins_the_model_cutoff_and_limits() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    world.hooks.set_budget(RunBudget {
        retry: RetryPolicy {
            max_attempts: 3,
            base_ms: 10,
            max_wait_ms: 20,
        },
        max_tokens: Some(1_000_000),
        ..RunBudget::default()
    });
    world.script.push(Step::Gate(
        gate.clone(),
        Box::new(Step::Reply(text_reply("ok"))),
    ));
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    world
        .until(|| async { world.script.request_count() == 1 })
        .await;

    let task = &world.tasks("komo.generation").await[0];
    let TaskState::Running { checkpoint } = &task.state else {
        panic!("running expected, got {:?}", task.state);
    };
    assert_eq!(checkpoint["phase"], "request");
    assert_eq!(checkpoint["attempt"], 1);
    assert_eq!(checkpoint["model"]["model"], "test-model");
    assert_eq!(checkpoint["limits"]["retry"]["maxAttempts"], 3);
    assert_eq!(checkpoint["limits"]["maxTokens"], 1_000_000);
    assert_eq!(checkpoint["cacheKey"], "cache-1");
    let cutoff = checkpoint["cutoff"].as_u64().unwrap();
    let newest = world.entries().await.last().unwrap().id.get();
    assert_eq!(
        cutoff, newest,
        "the cutoff is the newest entry the request includes"
    );
    gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();
    engine.close().await.unwrap();
}

/// §3.6：只有声明了 `mark_progress` 的提交走 `commit_progress`。一笔只改 document、
/// 但没声明的提交（例如 inbox 或 agent 设置这类已经确认给客户端的改动）仍是结构性的。
#[tokio::test(start_paused = true)]
async fn only_a_declared_progress_commit_skips_the_durable_path() {
    let world = World::new();
    let engine = world.open().await;
    let address = super::super::live::live_address(CONV);
    let declared = engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.put_document(address, 1, json!({"step": 1})).await?;
                Ok(())
            })
        })
        .await;
    declared.unwrap();
    let before = world.storage.commit_log().len();
    let address = super::super::live::live_address(CONV);
    engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.put_document(address, 1, json!({"step": 2})).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
    let address = super::super::live::live_address(CONV);
    engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.mark_progress();
                tx.put_document(address, 1, json!({"step": 3})).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
    let log = world.storage.commit_log();
    let undeclared = &log[before];
    assert_eq!(undeclared.writes, ["document.change"], "{undeclared:?}");
    assert!(
        !undeclared.progress,
        "an undeclared change stays structural"
    );
    let declared = &log[before + 1];
    assert_eq!(declared.writes, ["document.change"], "{declared:?}");
    assert!(
        declared.progress,
        "a declared progress commit uses commit_progress"
    );
    engine.close().await.unwrap();
}
