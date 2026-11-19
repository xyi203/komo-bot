//! `komo.generation`：prepare / request / retry / 预算 / 截断 / 失败（设计 §4.5；pi spec §8.3）。
//! 对应 pi 的 `harness-generation` / `harness-generation-recovery`。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::durable::{
    InputState, Message, StopReason, SubmissionKind, TaskOutcome, TaskState,
};
use komo_kernel::types::turn::LlmError;
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

#[tokio::test(start_paused = true)]
async fn three_consecutive_empty_timeouts_stop_but_timeouts_after_data_retry() {
    use komo_kernel::types::turn::LlmError;
    let world = World::new();
    for _ in 0..10 {
        world.script.push(Step::Error(LlmError::Timeout));
    }
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(world.script.request_times().len(), 3);
    let detail = unanswered(&settled).1.unwrap();
    assert!(detail.contains("连续 3 次在 5 秒内没收到数据"), "{detail}");
    engine.close().await.unwrap();

    let world = World::new();
    for _ in 0..4 {
        world.script.push(Step::Error(LlmError::TimeoutAfterData));
    }
    world.script.reply("finally");
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(world.script.request_times().len(), 5);
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

fn truncated_text(text: &str) -> Step {
    let mut message = text_reply(text);
    message.stop_reason = StopReason::Length;
    Step::Reply(message)
}

/// §6：有字的回复被输出上限截断不让 run 失败——这一段照常落账，补一条 `komo.continue`（"接着写"），
/// 下一轮从断开处接着写；最终回答的 `data.text` 是接起来的整段（渠道投递、任务交回读它）。
#[tokio::test(start_paused = true)]
async fn a_truncated_answer_is_continued_and_the_answer_is_stitched_whole() {
    let world = World::new();
    world.script.push(truncated_text("第一段，写到一半"));
    world.script.reply("接着写完。");
    let engine = world.open().await;
    let id = world.submit(&engine, "写一份长报告").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    let requests = world.script.requests();
    assert_eq!(requests.len(), 2);
    let second = outline(&requests[1]);
    assert_eq!(second[second.len() - 2], "assistant: 第一段，写到一半");
    assert!(
        second
            .last()
            .unwrap()
            .starts_with("user: （这条是 komo 自动发的"),
        "{second:?}"
    );
    assert_eq!(world.entries_of("komo.continue").await.len(), 1);
    let assistants = world.entries_of("komo.assistant").await;
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistant_stop(&assistants[0]), StopReason::Length);
    let data = assistants[1]
        .data
        .as_ref()
        .expect("续写收尾的回答带整段正文");
    assert_eq!(data["text"], "第一段，写到一半接着写完。");
    assert_eq!(data["continued"], json!([assistants[0].id]));
    assert!(world.live().await.run.is_none());
    engine.close().await.unwrap();
}

/// 续写有上限：连续 3 次之后再截断，按"模型回复被截断"失败；截断回复里的
/// 调用从不执行。
#[tokio::test(start_paused = true)]
async fn a_truncated_answer_fails_the_run_after_three_continuations() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    world.tools(vec![read.clone()]);
    for n in 0..4 {
        world.script.push(truncated_text(&format!("段{n}")));
    }
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "model_error");
    assert!(detail.unwrap().contains("模型回复被截断"));
    assert_eq!(world.script.request_count(), 4);
    assert_eq!(read.runs(), 0);
    let assistants = world.entries_of("komo.assistant").await;
    assert_eq!(assistants.len(), 4);
    assert_eq!(assistant_stop(&assistants[2]), StopReason::Length);
    assert_eq!(assistant_stop(&assistants[3]), StopReason::Error);
    engine.close().await.unwrap();
}

/// 只有思考、一个字没写就被截断：续写没有可接的，照旧失败，这一条不进之后的上下文。
#[tokio::test(start_paused = true)]
async fn a_truncated_reply_without_text_fails_the_run() {
    let world = World::new();
    let mut truncated = text_reply("");
    truncated.content.clear();
    truncated.stop_reason = StopReason::Length;
    world.script.push(Step::Reply(truncated));
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    let (reason, _) = unanswered(&settled);
    assert_eq!(reason, "model_error");
    let assistants = world.entries_of("komo.assistant").await;
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistant_stop(&assistants[0]), StopReason::Error);
    assert!(world.entries_of("komo.continue").await.is_empty());
    engine.close().await.unwrap();
}

/// 同一个调用原样连发 6 次：第 7 次请求末尾多一句 `komo.nudge` 提醒；只提醒、不停 run。
#[tokio::test(start_paused = true)]
async fn identical_calls_in_a_row_get_one_nudge_and_the_run_goes_on() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    world.tools(vec![read.clone()]);
    for n in 0..6 {
        world
            .script
            .calls(&[(&format!("c{n}"), "read", json!({"path": "a"}))]);
    }
    world.script.reply("换个办法");
    let engine = world.open().await;
    let id = world.submit(&engine, "看看 a").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    assert_eq!(read.runs(), 6);
    let requests = world.script.requests();
    assert_eq!(requests.len(), 7);
    for request in &requests[..6] {
        assert!(!last_user_text(request).contains("完全相同的参数"));
    }
    let last = outline(&requests[6]);
    assert!(
        last.last()
            .unwrap()
            .contains("连续 6 次用完全相同的参数调用 `read`"),
        "{last:?}"
    );
    let nudges = world.entries_of("komo.nudge").await;
    assert_eq!(nudges.len(), 1);
    assert_eq!(nudges[0].data.as_ref().unwrap()["count"], 6);
    engine.close().await.unwrap();
}

/// 参数不同的连续调用不算打转。
#[tokio::test(start_paused = true)]
async fn different_arguments_are_not_a_streak() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    world.tools(vec![read.clone()]);
    for n in 0..7 {
        world
            .script
            .calls(&[(&format!("c{n}"), "read", json!({"path": format!("f{n}")}))]);
    }
    world.script.reply("读完了");
    let engine = world.open().await;
    let id = world.submit(&engine, "都读一遍").await;
    world.harness.wait(&engine, id).await.unwrap();

    assert!(world.entries_of("komo.nudge").await.is_empty());
    engine.close().await.unwrap();
}

/// 429 带了 Retry-After：退避至少等服务端要的那么久（这里 30 s，比指数退避的 2 s 长）。
#[tokio::test(start_paused = true)]
async fn a_throttled_request_waits_for_retry_after() {
    let world = World::new();
    world.script.push(Step::Error(LlmError::Throttled {
        message: "slow down".into(),
        retry_after_ms: Some(30_000),
    }));
    world.script.push(Step::Error(LlmError::Throttled {
        message: "slow down".into(),
        retry_after_ms: None,
    }));
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "hi").await;
    world.harness.wait(&engine, id).await.unwrap();

    let times = world.script.request_times();
    assert_eq!(times.len(), 3);
    assert_eq!(times[1] - times[0], 30_000, "Retry-After wins over 2 s");
    assert_eq!(
        times[2] - times[1],
        4_000,
        "no Retry-After: the exponential backoff as before"
    );
    engine.close().await.unwrap();
}

/// 一轮里的只读调用一组至多 10 个同时在飞，多出来的等前一组收尾。
#[tokio::test(start_paused = true)]
async fn at_most_ten_read_only_calls_run_at_once() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    let read = Arc::new(TestTool::reader("read", &world.log).with_gate(&gate));
    world.tools(vec![read.clone()]);
    let calls: Vec<(String, serde_json::Value)> = (0..12)
        .map(|n| (format!("c{n}"), json!({"path": format!("f{n}")})))
        .collect();
    let calls: Vec<(&str, &str, serde_json::Value)> = calls
        .iter()
        .map(|(id, args)| (id.as_str(), "read", args.clone()))
        .collect();
    world.script.calls(&calls);
    world.script.reply("读完了");
    let engine = world.open().await;
    let id = world.submit(&engine, "都读一遍").await;
    world.until_log("start read #10").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(read.runs(), 10, "the eleventh waits for the first group");
    let wait = world.harness.wait(&engine, id);
    tokio::pin!(wait);
    let settled = loop {
        gate.notify_one();
        tokio::select! {
            settled = &mut wait => break settled.unwrap(),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    };
    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    assert_eq!(read.runs(), 12);
    engine.close().await.unwrap();
}

fn truncated_calls(calls: &[(&str, &str, serde_json::Value)]) -> Step {
    let mut message = call_reply(calls);
    message.stop_reason = StopReason::Length;
    Step::Reply(message)
}

/// P1（`docs/tasks.md` §4.10）：带工具调用的截断不让任务失败——保留 assistant entry，给每个
/// 调用落一条错误结果，下一轮请求让模型重发；截断的调用一个也不执行。
#[tokio::test(start_paused = true)]
async fn a_truncated_reply_with_calls_gets_error_results_and_the_model_resends() {
    let world = World::new();
    let write = Arc::new(TestTool::writer("write", &world.log));
    world.tools(vec![write.clone()]);
    world
        .script
        .push(truncated_calls(&[("c1", "write", json!({"path": "a"}))]));
    world.script.calls(&[("c2", "write", json!({"path": "a"}))]);
    world.script.reply("done");
    let engine = world.open().await;
    let id = world.submit(&engine, "write it").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    assert_eq!(write.runs(), 1, "only the resent call runs");
    let requests = world.script.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        outline(&requests[1]),
        [
            "system",
            "user: write it",
            "assistant calls write(c1)",
            "result(c1)"
        ]
    );
    let Message::ToolResult(result) = &requests[1][3] else {
        panic!("tool result expected");
    };
    assert!(result.is_error);
    assert!(text_of(&requests[1][3]).contains("输出上限"));
    let results = world.entries_of("komo.tool-result").await;
    assert_eq!(results[0].data.as_ref().unwrap()["harness"], "truncated");
    let assistants = world.entries_of("komo.assistant").await;
    assert_eq!(assistant_stop(&assistants[0]), StopReason::Length);
    engine.close().await.unwrap();
}

/// 连续截断有上限：两次可以重发，第三次按不带调用的截断那样失败。
#[tokio::test(start_paused = true)]
async fn consecutive_truncated_replies_fail_the_run_after_two_retries() {
    let world = World::new();
    let write = Arc::new(TestTool::writer("write", &world.log));
    world.tools(vec![write.clone()]);
    for n in 0..3 {
        world
            .script
            .push(truncated_calls(&[(&format!("c{n}"), "write", json!({}))]));
    }
    let engine = world.open().await;
    let id = world.submit(&engine, "write it").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "model_error");
    assert!(detail.unwrap().contains("模型回复被截断"));
    assert_eq!(world.script.request_count(), 3);
    assert_eq!(write.runs(), 0);
    engine.close().await.unwrap();
}

/// 中间有一轮正常的工具调用，连续截断的计数就清零。
#[tokio::test(start_paused = true)]
async fn a_normal_tool_round_resets_the_truncation_count() {
    let world = World::new();
    let write = Arc::new(TestTool::writer("write", &world.log));
    world.tools(vec![write.clone()]);
    for n in 0..2 {
        world
            .script
            .push(truncated_calls(&[(&format!("t{n}"), "write", json!({}))]));
    }
    world.script.calls(&[("ok", "write", json!({}))]);
    for n in 2..4 {
        world
            .script
            .push(truncated_calls(&[(&format!("t{n}"), "write", json!({}))]));
    }
    world.script.reply("done");
    let engine = world.open().await;
    let id = world.submit(&engine, "write it").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    assert_eq!(write.runs(), 1);
    engine.close().await.unwrap();
}

/// 重启后回放必须与首次一致：截断那一轮的 assistant 与错误结果在同一次提交里落地，重启后
/// 重发的请求与重启前那次逐条相同。
#[tokio::test(start_paused = true)]
async fn a_restart_after_a_truncated_round_resends_the_same_context() {
    let world = World::new();
    let write = Arc::new(TestTool::writer("write", &world.log));
    world.tools(vec![write.clone()]);
    world
        .script
        .push(truncated_calls(&[("c1", "write", json!({"path": "a"}))]));
    world.script.push(Step::Hang);
    let engine = world.open().await;
    let id = world.submit(&engine, "write it").await;
    world
        .until(|| async { world.script.request_count() == 2 })
        .await;

    world.script.reply("done");
    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();

    let requests = world.script.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1], requests[2]);
    assert_eq!(write.runs(), 0);
    engine.close().await.unwrap();
}

/// 截断后重发的那一轮同样计 token 预算：超了就以 `budget` 收场，不再发下一次请求。
#[tokio::test(start_paused = true)]
async fn a_truncated_round_counts_against_the_token_budget() {
    let world = World::new();
    let write = Arc::new(TestTool::writer("write", &world.log));
    world.tools(vec![write.clone()]);
    world.hooks.set_budget(RunBudget {
        max_tokens: Some(50),
        ..RunBudget::default()
    });
    world
        .script
        .push(truncated_calls(&[("c1", "write", json!({}))]));
    world.script.reply("never asked");
    let engine = world.open().await;
    let id = world.submit(&engine, "write it").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "budget");
    assert_eq!(detail.unwrap(), "超过 token 预算（50）");
    assert_eq!(
        world.script.request_count(),
        1,
        "no resend after the budget stop"
    );
    assert_eq!(write.runs(), 0);
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
