#[path = "regression_tests.rs"]
mod regression_tests;

#[path = "feedback_tests.rs"]
mod feedback_tests;

use std::sync::{Arc, Mutex};

use komo_kernel::events::EventPayload;
use komo_kernel::traits::{Tool, ToolOutputStore};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{AttemptId, ToolCallId};
use komo_kernel::types::plan::{Operation, RecoveryMode, Replay};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::ToolError;
use serde_json::json;

use super::*;
use crate::executor::harness::{Harness, RecordingTool};

fn mcp(recovery: RecoveryMode, replay: Replay) -> Arc<RecordingTool> {
    Arc::new(
        RecordingTool::new(
            "mcp__test__write",
            Operation::McpCall {
                server: "test".into(),
                tool: "write".into(),
                read_only: false,
            },
        )
        .with_recovery(recovery)
        .with_replay(replay),
    )
}

async fn start(executor: &ToolExecutor, ctx: &ToolContext) -> Mutex<Journal> {
    executor
        .codemode_record(
            ctx,
            "start",
            Record::Start {
                fingerprint: json!({"test": 1}),
            },
        )
        .await
        .unwrap();
    Mutex::new(Journal {
        fingerprint: Some(json!({"test": 1})),
        ..Default::default()
    })
}

async fn pending(
    executor: &ToolExecutor,
    tool: &dyn Tool,
    ctx: &ToolContext,
    replay: Replay,
    started: bool,
) {
    start(executor, ctx).await;
    let mut child_ctx = ctx.clone();
    child_ctx.call = ToolExecutor::codemode_attempt(ctx, "c1").call;
    let intent = Intent {
        name: "mcp__test__write".into(),
        args: json!({"value": 1}),
        plan: Some(tool.prepare(json!({"value": 1}), &child_ctx).await.unwrap()),
        error: None,
        replay,
    };
    let output = executor
        .codemode_save(ctx, "intent-1", &intent)
        .await
        .unwrap();
    executor
        .codemode_record(ctx, "intent-1", Record::Intent { index: 1, output })
        .await
        .unwrap();
    if started {
        executor
            .codemode_record(
                ctx,
                "started-1",
                Record::Started {
                    index: 1,
                    attempt: ToolExecutor::codemode_attempt(ctx, "c1"),
                },
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn completed_unsafe_child_replays_exact_result_without_execution_or_top_level_messages() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = h.executor(vec![tool.clone()]);
    let (session, run) = h.open_run().await;
    let mut ctx = h.tool_context(&session, &run, &ToolCallId::from_raw("parent"));
    let env = h.env(&session, &run);
    let journal = start(&executor, &ctx).await;
    let first = executor
        .nested_call(
            "mcp__test__write",
            json!({"value": 1}),
            &env,
            &ctx,
            &journal,
        )
        .await
        .unwrap();
    assert_eq!(tool.ran(), 1);
    ctx.attempt = AttemptId::from_raw("after-restart");
    let recovered = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
    let second = executor
        .nested_call(
            "mcp__test__write",
            json!({"value": 1}),
            &env,
            &ctx,
            &recovered,
        )
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(tool.ran(), 1);
    recovered.lock().unwrap().check_end().unwrap();
    assert!(h.ledger.events().iter().all(|e| !matches!(
        e.payload,
        EventPayload::ToolPlanned(_) | EventPayload::ToolStarted(_) | EventPayload::ToolResult(_)
    )));
    let child = recovered.lock().unwrap().children[&1]
        .delivered
        .clone()
        .unwrap();
    let reply: Reply = executor.codemode_open(&child).await.unwrap();
    assert!(
        reply.execution.is_some(),
        "原工具输出引用也保存，不只是脚本摘要"
    );
}

#[tokio::test]
async fn started_without_delivery_requires_both_stored_and_current_safe() {
    for (stored, current, runs) in [
        (Replay::Unsafe, Replay::Unsafe, 0),
        (Replay::Unsafe, Replay::Safe, 0),
        (Replay::Safe, Replay::Unsafe, 0),
        (Replay::Safe, Replay::Safe, 1),
    ] {
        let h = Harness::new();
        let tool = mcp(RecoveryMode::SafeReread, current);
        let executor = h.executor(vec![tool.clone()]);
        let (session, run) = h.open_run().await;
        let ctx = h.tool_context(&session, &run, &ToolCallId::from_raw("parent"));
        let env = h.env(&session, &run);
        pending(&executor, tool.as_ref(), &ctx, stored, true).await;
        let journal = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
        let result = executor
            .nested_call(
                "mcp__test__write",
                json!({"value": 1}),
                &env,
                &ctx,
                &journal,
            )
            .await;
        assert_eq!(
            result.is_ok(),
            runs == 1,
            "stored={stored:?} current={current:?}"
        );
        assert_eq!(tool.ran(), runs);
        if runs == 0 {
            assert!(
                journal
                    .lock()
                    .unwrap()
                    .fatal
                    .as_ref()
                    .unwrap()
                    .is_uncertain()
            );
        }
    }
}

#[tokio::test]
async fn intent_without_started_can_execute_unsafe_once() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = h.executor(vec![tool.clone()]);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    pending(&executor, tool.as_ref(), &ctx, Replay::Unsafe, false).await;
    let journal = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
    executor
        .nested_call(
            "mcp__test__write",
            json!({"value": 1}),
            &env,
            &ctx,
            &journal,
        )
        .await
        .unwrap();
    assert_eq!(tool.ran(), 1);
}

#[tokio::test]
async fn divergence_latches_and_blocks_all_later_calls_including_caught_errors() {
    for (name, args) in [
        ("mcp__test__write", json!({"value": 2})),
        ("different", json!({"value": 1})),
    ] {
        let h = Harness::new();
        let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
        let executor = h.executor(vec![tool.clone()]);
        let (s, r) = h.open_run().await;
        let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
        let env = h.env(&s, &r);
        pending(&executor, tool.as_ref(), &ctx, Replay::Unsafe, false).await;
        let journal = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
        assert!(
            executor
                .nested_call(name, args, &env, &ctx, &journal)
                .await
                .unwrap_err()
                .contains("回放分歧")
        );
        assert!(
            executor
                .nested_call(
                    "mcp__test__write",
                    json!({"value": 1}),
                    &env,
                    &ctx,
                    &journal
                )
                .await
                .is_err()
        );
        assert_eq!(tool.ran(), 0);
        assert!(journal.lock().unwrap().check_end().is_err());
    }
}

#[tokio::test]
async fn calls_past_the_recorded_prefix_run_live_exactly_once() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = h.executor(vec![tool.clone()]);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let journal = start(&executor, &ctx).await;
    executor
        .nested_call(
            "mcp__test__write",
            json!({"value": 1}),
            &env,
            &ctx,
            &journal,
        )
        .await
        .unwrap();
    let recovered = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
    assert!(
        recovered
            .lock()
            .unwrap()
            .check_end()
            .unwrap_err()
            .to_string()
            .contains("提前结束")
    );
    executor
        .nested_call(
            "mcp__test__write",
            json!({"value": 1}),
            &env,
            &ctx,
            &recovered,
        )
        .await
        .unwrap();
    assert_eq!(tool.ran(), 1, "已记录的那一次只回放，不重做");
    executor
        .nested_call(
            "mcp__test__write",
            json!({"value": 2}),
            &env,
            &ctx,
            &recovered,
        )
        .await
        .unwrap();
    assert_eq!(tool.ran(), 2, "越过记录的前缀之后照常执行一次");
    recovered.lock().unwrap().check_end().unwrap();
    let again = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
    assert_eq!(again.lock().unwrap().children.len(), 2);
}

#[tokio::test]
async fn published_but_uncommitted_reply_is_not_guessed_and_unsafe_is_not_repeated() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = h.executor(vec![tool.clone()]);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    pending(&executor, tool.as_ref(), &ctx, Replay::Unsafe, true).await;
    executor
        .codemode_save(
            &ctx,
            "reply-1",
            &Reply {
                value: Some(json!({"status":"completed", "text":"write done"})),
                error: None,
                uncertain: false,
                execution: None,
            },
        )
        .await
        .unwrap();
    let recovered = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
    assert!(
        executor
            .nested_call(
                "mcp__test__write",
                json!({"value": 1}),
                &env,
                &ctx,
                &recovered
            )
            .await
            .is_err()
    );
    assert_eq!(tool.ran(), 0);
}

#[tokio::test]
async fn uncertain_child_stops_later_writes_and_stays_uncertain_on_recovery() {
    let h = Harness::new();
    let tool = Arc::new(
        RecordingTool::new(
            "mcp__test__write",
            Operation::McpCall {
                server: "test".into(),
                tool: "write".into(),
                read_only: false,
            },
        )
        .with_outcome(Err(ToolError::Uncertain {
            message: "remote disconnected".into(),
        })),
    );
    let executor = h.executor(vec![tool.clone()]);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let journal = start(&executor, &ctx).await;
    for index in 0..2 {
        let recovered;
        let journal = if index == 0 {
            &journal
        } else {
            recovered = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
            &recovered
        };
        assert!(
            executor
                .nested_call("mcp__test__write", json!({}), &env, &ctx, &journal)
                .await
                .is_err()
        );
        assert!(
            executor
                .nested_call("mcp__test__write", json!({}), &env, &ctx, &journal)
                .await
                .is_err()
        );
        assert!(
            journal
                .lock()
                .unwrap()
                .check_end()
                .unwrap_err()
                .is_uncertain()
        );
    }
    assert_eq!(tool.ran(), 1);
}

#[tokio::test]
async fn journal_reader_crosses_page_boundaries_without_skipping_a_child() {
    let h = Harness::new();
    let executor = h.executor(vec![]);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    start(&executor, &ctx).await;
    for index in 1..=256 {
        let output = executor
            .codemode_save(&ctx, &format!("i{index}"), &json!({}))
            .await
            .unwrap();
        executor
            .codemode_record(
                &ctx,
                &format!("intent-{index}"),
                Record::Intent {
                    index,
                    output: output.clone(),
                },
            )
            .await
            .unwrap();
        executor
            .codemode_record(
                &ctx,
                &format!("delivered-{index}"),
                Record::Delivered { index, output },
            )
            .await
            .unwrap();
    }
    let journal = executor.codemode_load(&ctx).await.unwrap();
    assert_eq!(journal.children.len(), 256);
    assert!(journal.children[&256].delivered.is_some());
}

#[tokio::test]
async fn script_returns_committed_outcome_without_repeating_child() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let _registered = h.executor(vec![tool.clone()]);
    // 围着同一套持久化服务重建 ToolExecutor 很便宜；两次尝试之间不共享内存里的 journal。
    let executor = ToolExecutor::new(
        vec![tool.clone()],
        h.ledger.clone(),
        h.outputs.clone(),
        Arc::new(h.clock.clone()),
    )
    .with_codemode(Some(crate::codemode::Worker::in_process()));
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let code = "text(tools.mcp__test__write(value=1)['text'])";
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "parent-sink"))
        .await
        .unwrap();
    let output = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap();
    assert_eq!(output.status, ToolResultStatus::Completed);
    assert_eq!(tool.ran(), 1);
    let output2 = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap();
    assert_eq!(output, output2);
    assert_eq!(tool.ran(), 1);
    assert!(
        executor
            .run_codemode("text('changed')", &env, &ctx, sink.as_mut())
            .await
            .unwrap_err()
            .is_uncertain()
    );
}

/// 换引擎（2026-10-15）之前在途的调用：`start` 指纹是 CPython driver 时代的形状，对不上，恢复时
/// 按分歧返回 uncertain，不起 worker、不执行任何子调用。
#[tokio::test]
async fn a_start_fingerprint_from_the_cpython_driver_settles_as_uncertain() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let _registered = h.executor(vec![tool.clone()]);
    let executor = ToolExecutor::new(
        vec![tool.clone()],
        h.ledger.clone(),
        h.outputs.clone(),
        Arc::new(h.clock.clone()),
    )
    .with_codemode(Some(crate::codemode::Worker::in_process()));
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let code = "text(tools.mcp__test__write(value=1)['text'])";
    executor
        .codemode_record(
            &ctx,
            "start",
            Record::Start {
                fingerprint: json!({
                    "version": 1,
                    "code": ContentHash::of_str(code),
                    "driver": ContentHash::of_str("# 旧 driver.py"),
                    "tools": ["mcp__test__write"],
                    "cwd": ctx.cwd,
                    "env": null,
                }),
            },
        )
        .await
        .unwrap();
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let error = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap_err();
    assert!(error.is_uncertain(), "{error}");
    assert!(error.to_string().contains("回放分歧"), "{error}");
    assert_eq!(tool.ran(), 0);
}

/// 4a（Monty 第一版）的指纹：worker 协议 1、没有前置代码哈希，那时时钟与随机数不进 journal。
/// 在途的调用恢复时对不上，按分歧返回 uncertain，不起 worker、不执行任何子调用。
#[tokio::test]
async fn a_start_fingerprint_from_before_the_clock_was_journaled_settles_as_uncertain() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = scripted(&h, &tool);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let code = "text(tools.mcp__test__write(value=1)['text'])";
    executor
        .codemode_record(
            &ctx,
            "start",
            Record::Start {
                fingerprint: json!({
                    "version": 2,
                    "code": ContentHash::of_str(code),
                    "engine": format!("monty-{}", crate::codemode::MONTY_VERSION),
                    "worker": 1,
                    "tools": ["mcp__test__write"],
                    "cwd": ctx.cwd,
                }),
            },
        )
        .await
        .unwrap();
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let error = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap_err();
    assert!(error.is_uncertain(), "{error}");
    assert!(error.to_string().contains("回放分歧"), "{error}");
    assert_eq!(tool.ran(), 0);
}

fn scripted(h: &Harness, tool: &Arc<RecordingTool>) -> ToolExecutor {
    let _registered = h.executor(vec![tool.clone()]);
    ToolExecutor::new(
        vec![tool.clone()],
        h.ledger.clone(),
        h.outputs.clone(),
        Arc::new(h.clock.clone()),
    )
    .with_codemode(Some(crate::codemode::Worker::in_process()))
}

#[tokio::test]
async fn a_resumed_script_replays_the_recorded_prefix_then_runs_the_rest_live() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = scripted(&h, &tool);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let code = "text(tools.mcp__test__write(value=1)['text'])\ntry:\n    tools.mcp__test__write(value=2)\nexcept ToolError:\n    pass\ntools.mcp__test__write(value=3)";
    let fingerprint = ToolExecutor::codemode_fingerprint(code, &["mcp__test__write".into()], &ctx);
    executor
        .codemode_record(
            &ctx,
            "start",
            Record::Start {
                fingerprint: fingerprint.clone(),
            },
        )
        .await
        .unwrap();
    let journal = Mutex::new(Journal {
        fingerprint: Some(fingerprint),
        ..Default::default()
    });
    executor
        .nested_call("mcp__test__write", json!({"value":1}), &env, &ctx, &journal)
        .await
        .unwrap();
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let output = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap();
    assert_eq!(output.status, ToolResultStatus::Completed);
    assert_eq!(tool.ran(), 3, "第 1 次回放，第 2、3 次各执行一次");
}

fn journal_phases(h: &Harness) -> Vec<String> {
    h.ledger
        .events()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::CodemodeJournal(body) => body.record["phase"].as_str().map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// 第一次尝试在第一次调用交付之后"崩"（脚本睡着，外层 future 被丢掉，等于进程没了）；
/// 第二次从头重跑：时钟 / 随机数按记录还回去，已交付的调用回放，下一次调用执行一次。
#[tokio::test]
async fn a_crash_after_n_delivered_calls_resumes_with_the_same_clock_and_entropy() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = scripted(&h, &tool);
    let (s, r) = h.open_run().await;
    let mut ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let dir = tempfile::tempdir().unwrap();
    let gate = dir.path().join("gate");
    let code = format!(
        r#"
import time, random
from datetime import datetime
first = [time.time(), random.random(), str(datetime.now())]
tools.mcp__test__write(value=first)
if open("{gate}").read() == "crash":
    time.sleep(8)
tools.mcp__test__write(value=[time.time() > first[0], random.random() != first[1]])
text(first)
"#,
        gate = gate.display()
    );
    std::fs::write(&gate, "crash").unwrap();
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let crashed = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.run_codemode(&code, &env, &ctx, sink.as_mut()),
    )
    .await;
    assert!(crashed.is_err(), "第一次尝试应当卡在 sleep 里");
    assert_eq!(tool.ran(), 1);

    std::fs::write(&gate, "go").unwrap();
    ctx.attempt = AttemptId::from_raw("after-restart");
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink-2"))
        .await
        .unwrap();
    let output = executor
        .run_codemode(&code, &env, &ctx, sink.as_mut())
        .await
        .unwrap();
    assert_eq!(output.status, ToolResultStatus::Completed, "{output:?}");
    assert_eq!(tool.ran(), 2, "第 1 次回放，第 2 次执行一次");
    let first = output.preview.unwrap();
    let recorded: Intent = {
        let journal = executor.codemode_load(&ctx).await.unwrap();
        executor
            .codemode_open(journal.children[&1].intent.as_ref().unwrap())
            .await
            .unwrap()
    };
    assert_eq!(
        serde_json::Deserializer::from_str(&first)
            .into_iter::<serde_json::Value>()
            .next()
            .unwrap()
            .unwrap(),
        recorded.args["value"],
        "时钟与随机数与崩溃前记下的一致"
    );
    let phases = journal_phases(&h);
    assert_eq!(
        phases.iter().filter(|p| *p == "nondet").count(),
        2,
        "{phases:?}"
    );
}

#[tokio::test]
async fn a_file_that_changed_between_runs_and_changes_the_next_call_is_uncertain() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = scripted(&h, &tool);
    let (s, r) = h.open_run().await;
    let mut ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let dir = tempfile::tempdir().unwrap();
    let (data, gate) = (dir.path().join("data"), dir.path().join("gate"));
    let code = format!(
        r#"
import time
tools.mcp__test__write(value=1)
tools.mcp__test__write(value=open("{data}").read())
if open("{gate}").read() == "crash":
    time.sleep(8)
"#,
        data = data.display(),
        gate = gate.display()
    );
    std::fs::write(&data, "A").unwrap();
    std::fs::write(&gate, "crash").unwrap();
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let crashed = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.run_codemode(&code, &env, &ctx, sink.as_mut()),
    )
    .await;
    assert!(crashed.is_err());
    assert_eq!(tool.ran(), 2);

    std::fs::write(&data, "B").unwrap();
    std::fs::write(&gate, "go").unwrap();
    ctx.attempt = AttemptId::from_raw("after-restart");
    let error = executor
        .run_codemode(&code, &env, &ctx, sink.as_mut())
        .await
        .unwrap_err();
    assert!(error.is_uncertain(), "{error}");
    assert!(error.to_string().contains("回放分歧"), "{error}");
    assert_eq!(tool.ran(), 2, "分歧之后不再执行任何子调用");
}

#[tokio::test]
async fn a_clock_loop_is_journaled_as_one_batch_before_the_next_call() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = scripted(&h, &tool);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let code = "import time\nfor _ in range(500):\n    time.time()\ntools.mcp__test__write(value=1)\nfor _ in range(500):\n    time.time()";
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let output = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap();
    assert_eq!(output.status, ToolResultStatus::Completed);
    let phases = journal_phases(&h);
    assert_eq!(
        phases.iter().filter(|p| *p == "nondet").count(),
        2,
        "每 500 次读数一条记录，不是 1000 条：{phases:?}"
    );
}

#[test]
fn reads_that_do_not_match_the_recording_are_a_divergence() {
    use crate::codemode::Nondet;
    let mut journal = Journal {
        replay: vec![NondetEntry {
            request: Nondet::Clock,
            value: json!(1.5),
        }],
        ..Default::default()
    };
    assert!(journal.read(&Nondet::Urandom { size: 4 }).is_err());
    assert!(journal.fatal.as_ref().is_some_and(ToolError::is_uncertain));
    let mut journal = Journal {
        replay: vec![NondetEntry {
            request: Nondet::Clock,
            value: json!(1.5),
        }],
        ..Default::default()
    };
    assert_eq!(journal.read(&Nondet::Clock).unwrap(), json!(1.5));
    assert!(journal.check_end().is_ok());
    let journal = Journal {
        replay: vec![NondetEntry {
            request: Nondet::Clock,
            value: json!(1.5),
        }],
        ..Default::default()
    };
    assert!(
        journal
            .check_end()
            .unwrap_err()
            .to_string()
            .contains("时钟 / 随机数")
    );
}

#[test]
fn one_script_reads_the_clock_a_bounded_number_of_times() {
    use crate::codemode::Nondet;
    let mut journal = Journal::default();
    for _ in 0..MAX_NONDET {
        journal.read(&Nondet::Clock).unwrap();
    }
    assert!(journal.read(&Nondet::Clock).unwrap_err().contains("最多读"));
}

#[tokio::test]
async fn a_worker_from_another_build_fails_the_call_cleanly_before_running_anything() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let _registered = h.executor(vec![tool.clone()]);
    let stale = crate::codemode::Hello {
        protocol: crate::codemode::WORKER_PROTOCOL + 1,
        ..crate::codemode::Hello::current()
    };
    let executor = ToolExecutor::new(
        vec![tool.clone()],
        h.ledger.clone(),
        h.outputs.clone(),
        Arc::new(h.clock.clone()),
    )
    .with_codemode(Some(crate::codemode::Worker::in_process_as(stale)));
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let code = "tools.mcp__test__write(value=1)";
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let error = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap_err();
    assert!(!error.is_uncertain(), "{error}");
    assert!(error.to_string().contains("重启 Gateway"), "{error}");
    assert_eq!(tool.ran(), 0);
    assert!(!journal_phases(&h).contains(&"finished".to_string()));

    // 重启之后是同一版，同一个调用能照常跑。
    let executor = scripted(&h, &tool);
    let output = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap();
    assert_eq!(output.status, ToolResultStatus::Completed);
    assert_eq!(tool.ran(), 1);
}

#[tokio::test]
async fn script_timeout_during_unsafe_child_returns_uncertain() {
    let h = Harness::new();
    let tool = Arc::new(
        RecordingTool::new(
            "mcp__test__write",
            Operation::McpCall {
                server: "test".into(),
                tool: "write".into(),
                read_only: false,
            },
        )
        .with_delay(std::time::Duration::from_secs(10)),
    );
    let _registered = h.executor(vec![tool.clone()]);
    let executor = ToolExecutor::new(
        vec![tool.clone()],
        h.ledger.clone(),
        h.outputs.clone(),
        Arc::new(h.clock.clone()),
    )
    .with_codemode(Some(crate::codemode::Worker::in_process()));
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let mut env = h.env(&s, &r);
    env.call_timeout = std::time::Duration::from_secs(1);
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    assert!(
        executor
            .run_codemode("tools.mcp__test__write()", &env, &ctx, sink.as_mut())
            .await
            .unwrap_err()
            .is_uncertain()
    );
    assert_eq!(tool.ran(), 1);
    assert!(
        executor
            .run_codemode("tools.mcp__test__write()", &env, &ctx, sink.as_mut())
            .await
            .unwrap_err()
            .is_uncertain()
    );
    assert_eq!(tool.ran(), 1);
}

#[tokio::test]
async fn removed_capability_and_changed_prepared_operation_do_not_execute_pending_child() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = h.executor(vec![tool.clone()]);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let mut env = h.env(&s, &r);
    pending(&executor, tool.as_ref(), &ctx, Replay::Unsafe, false).await;
    env.surface = komo_kernel::types::surface::AgentSurface::new(Vec::<String>::new());
    let recovered = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
    assert!(
        executor
            .nested_call(
                "mcp__test__write",
                json!({"value":1}),
                &env,
                &ctx,
                &recovered
            )
            .await
            .is_err()
    );
    assert_eq!(tool.ran(), 0);
    let changed = Arc::new(RecordingTool::new(
        "mcp__test__write",
        Operation::ShellCommand {
            command: "write".into(),
        },
    ));
    let executor = h.executor(vec![changed.clone()]);
    let env = h.env(&s, &r);
    let recovered = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
    assert!(
        executor
            .nested_call(
                "mcp__test__write",
                json!({"value":1}),
                &env,
                &ctx,
                &recovered
            )
            .await
            .is_err()
    );
    assert_eq!(changed.ran(), 0);
}

#[tokio::test]
async fn planned_parent_can_start_fresh_but_started_parent_without_journal_cannot() {
    use komo_kernel::types::status::ToolCallState;
    use komo_kernel::types::tool::ResumedCall;
    let h = Harness::new();
    let executor = h.executor(vec![]);
    let (s, r) = h.open_run().await;
    let mut ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    ctx.resumed = Some(ResumedCall {
        previous_attempt: None,
        previous_state: ToolCallState::Planned,
        attempts_so_far: 0,
    });
    assert!(!executor.codemode_load(&ctx).await.unwrap().recovering);
    ctx.resumed.as_mut().unwrap().previous_state = ToolCallState::Started;
    assert!(
        executor
            .codemode_load(&ctx)
            .await
            .err()
            .unwrap()
            .is_uncertain()
    );
}

#[tokio::test]
async fn committed_result_corruption_fails_closed_without_repeating_write() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = h.executor(vec![tool.clone()]);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let journal = start(&executor, &ctx).await;
    executor
        .nested_call("mcp__test__write", json!({"value":1}), &env, &ctx, &journal)
        .await
        .unwrap();
    // 换掉存下的回复正文，journal 里还留着旧哈希。
    executor
        .codemode_save(&ctx, "reply-1", &json!({"corrupt": true}))
        .await
        .unwrap();
    let recovered = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
    assert!(
        executor
            .nested_call(
                "mcp__test__write",
                json!({"value":1}),
                &env,
                &ctx,
                &recovered
            )
            .await
            .is_err()
    );
    assert_eq!(tool.ran(), 1);
}

#[tokio::test]
async fn unsafe_interruption_returns_original_child_partial_output_without_execution() {
    use komo_kernel::types::refs::InterruptedOutput;
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = h.executor(vec![tool.clone()]);
    let (s, r) = h.open_run().await;
    let mut ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    pending(&executor, tool.as_ref(), &ctx, Replay::Unsafe, true).await;
    let child_attempt = ToolExecutor::codemode_attempt(&ctx, "c1");
    h.outputs.leave_interrupted(
        child_attempt.attempt.as_str(),
        InterruptedOutput {
            stdout_bytes: 18,
            stdout_tail: "remote write begun".into(),
            ..Default::default()
        },
    );
    ctx.attempt = AttemptId::from_raw("new-parent-attempt");
    let recovered = Mutex::new(executor.codemode_load(&ctx).await.unwrap());
    let error = executor
        .nested_call(
            "mcp__test__write",
            json!({"value":1}),
            &env,
            &ctx,
            &recovered,
        )
        .await
        .unwrap_err();
    assert!(error.contains("remote write begun"), "{error}");
    assert!(error.contains(child_attempt.attempt.as_str()), "{error}");
    assert_eq!(tool.ran(), 0);
}
