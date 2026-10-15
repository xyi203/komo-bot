#[path = "regression_tests.rs"]
mod regression_tests;

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
async fn fewer_and_extra_calls_fail_without_new_side_effects() {
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
            .unwrap_err()
            .contains("未提交窗口")
    );
    assert_eq!(tool.ran(), 1);
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

#[tokio::test]
async fn script_replays_prefix_and_caught_extra_call_cannot_write() {
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
    let error = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap_err();
    assert!(error.is_uncertain());
    assert!(error.to_string().contains("回放分歧"));
    assert_eq!(
        tool.ran(),
        1,
        "Python catches host error but cannot invoke more writes"
    );
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
