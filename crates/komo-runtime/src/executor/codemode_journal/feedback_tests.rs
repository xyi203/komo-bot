//! 模型应当看见失败原因、已执行结果与真实子状态，恢复不能重复写。
use super::*;
use komo_kernel::projection::{ProjectionContext, ToolResultFacts, project};
use komo_kernel::types::tool::ToolOutput;

#[tokio::test]
async fn a_handled_failed_payload_is_still_failed_in_the_call_summary() {
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
        .with_outcome(Ok(ToolOutput {
            status: ToolResultStatus::Failed,
            result: json!({"isError": true}),
            exit_code: None,
            artifacts: vec![],
            images: vec![],
            preview: Some("remote failed".into()),
        })),
    );
    let executor = scripted(&h, &tool);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let output = executor
        .run_codemode(
            "r = tools.mcp__test__write()\nif r['status'] == 'failed':\n    text('handled')",
            &h.env(&s, &r),
            &ctx,
            sink.as_mut(),
        )
        .await
        .unwrap();
    assert_eq!(
        output.status,
        ToolResultStatus::Completed,
        "脚本允许自行处理失败"
    );
    assert_eq!(output.result["calls"][0]["ok"], true, "保留宿主返回语义");
    assert_eq!(output.result["calls"][0]["status"], "failed");
    assert!(output.preview.as_ref().unwrap().contains("1 次失败"));
}

#[tokio::test]
async fn a_caught_host_error_is_not_labeled_as_a_completed_tool_result() {
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
        .with_outcome(Err(ToolError::Failed {
            message: "fixture unavailable".into(),
        })),
    );
    let executor = scripted(&h, &tool);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let output = executor
        .run_codemode(
            "try:\n    tools.mcp__test__write()\nexcept ToolError:\n    text('kept other items')",
            &h.env(&s, &r),
            &ctx,
            sink.as_mut(),
        )
        .await
        .unwrap();
    assert_eq!(output.status, ToolResultStatus::Completed);
    let call = &output.result["calls"][0];
    assert_eq!(call["ok"], false);
    assert!(call.get("status").is_none());
    assert!(call["output"].as_str().unwrap().starts_with("artifact://"));
    assert!(output.preview.unwrap().contains("1 次失败"));
}

#[tokio::test]
async fn a_script_error_exposes_prior_write_before_large_output_and_replays_verbatim() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = scripted(&h, &tool);
    let (s, r) = h.open_run().await;
    let mut ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let code =
        "tools.mcp__test__write(value=1)\ntext('x' * 70000)\ntext(json.dumps({'done': True}))";
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let output = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap();
    assert_eq!(output.status, ToolResultStatus::Failed);
    let text = output.preview.as_deref().unwrap();
    assert!(
        text.starts_with("脚本出错：NameError:"),
        "{}",
        &text[..100.min(text.len())]
    );
    let journal = executor.codemode_load(&ctx).await.unwrap();
    let call = &output.result["calls"][0];
    assert_eq!(call["id"], "c1");
    assert_eq!(call["status"], "completed");
    let uri = call["output"].as_str().unwrap();
    let attempt = journal.children[&1].attempt.as_ref().unwrap();
    assert!(uri.contains(attempt.attempt.as_str()), "引用必须指向原尝试");
    let view = project(
        &ToolResultFacts {
            tool: "codemode",
            status: output.status,
            elapsed_ms: 1,
            text: Some(text),
            output: journal.finished.as_ref().unwrap(),
            stdout: None,
            stderr: None,
            artifacts: &[],
            images: &[],
        },
        &ProjectionContext {
            model_result_bytes: 2048,
            decay: None,
        },
    );
    for expected in [
        "NameError",
        "import json",
        "c1",
        "completed",
        "不会因脚本报错回滚",
        uri,
    ] {
        assert!(view.contains(expected), "missing {expected}: {view}");
    }
    ctx.attempt = AttemptId::from_raw("after-restart");
    let replayed = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(output).unwrap(),
        serde_json::to_value(replayed).unwrap()
    );
    assert_eq!(tool.ran(), 1);
}

#[tokio::test]
async fn unsupported_timestamp_apis_get_a_specific_tested_alternative() {
    for method in ["fromtimestamp", "utcfromtimestamp"] {
        let h = Harness::new();
        let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
        let executor = scripted(&h, &tool);
        let (s, r) = h.open_run().await;
        let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
        let mut sink = h
            .outputs
            .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
            .await
            .unwrap();
        let code = format!("from datetime import datetime\ntext(datetime.{method}(0))");
        let output = executor
            .run_codemode(&code, &h.env(&s, &r), &ctx, sink.as_mut())
            .await
            .unwrap();
        assert_eq!(output.status, ToolResultStatus::Failed);
        let text = output.preview.unwrap();
        assert!(text.contains("time.gmtime"), "{text}");
        assert!(text.contains("time.strftime"), "{text}");
        assert_eq!(tool.ran(), 0);
    }
}

#[tokio::test]
async fn a_legacy_finished_output_is_returned_without_new_hints_or_fields() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = scripted(&h, &tool);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let code = "text('old')";
    executor
        .codemode_record(
            &ctx,
            "start",
            Record::Start {
                fingerprint: ToolExecutor::codemode_fingerprint(
                    code,
                    &["mcp__test__write".into()],
                    &ctx,
                ),
            },
        )
        .await
        .unwrap();
    let old = ToolOutput {
        status: ToolResultStatus::Failed,
        result: json!({"calls": [{"tool": "read", "ok": true}], "error": "old"}),
        exit_code: None,
        artifacts: vec![],
        images: vec![],
        preview: Some("old text\n脚本出错：old".into()),
    };
    let reference = executor
        .codemode_save(&ctx, "finished", &old)
        .await
        .unwrap();
    executor
        .codemode_record(&ctx, "finished", Record::Finished { output: reference })
        .await
        .unwrap();
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let replayed = executor
        .run_codemode(code, &h.env(&s, &r), &ctx, sink.as_mut())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(old).unwrap(),
        serde_json::to_value(replayed).unwrap()
    );
    assert_eq!(tool.ran(), 0);
}

#[tokio::test]
async fn a_failure_bounds_the_list_and_keeps_original_child_refs_when_resuming() {
    let h = Harness::new();
    let tool = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
    let executor = scripted(&h, &tool);
    let (s, r) = h.open_run().await;
    let mut ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    let env = h.env(&s, &r);
    let code = "for i in range(12):\n    tools.mcp__test__write(value=i)\nraise ValueError('after writes')";
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
        .nested_call(
            "mcp__test__write",
            json!({"value": 0}),
            &env,
            &ctx,
            &journal,
        )
        .await
        .unwrap();
    let original = journal.lock().unwrap().children[&1]
        .attempt
        .clone()
        .unwrap();
    ctx.attempt = AttemptId::from_raw("after-restart");
    let mut sink = h
        .outputs
        .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
        .await
        .unwrap();
    let output = executor
        .run_codemode(code, &env, &ctx, sink.as_mut())
        .await
        .unwrap();
    assert_eq!(output.status, ToolResultStatus::Failed);
    let calls = output.result["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 12);
    assert!(
        calls[0]["output"]
            .as_str()
            .unwrap()
            .contains(original.attempt.as_str())
    );
    let header = output
        .preview
        .as_ref()
        .unwrap()
        .split("<traceback>")
        .next()
        .unwrap();
    assert!(header.contains("- c8 "));
    assert!(!header.contains("- c9 "));
    assert!(header.contains("另 4 次子调用"));
    assert_eq!(calls[11]["id"], "c12");
    assert_eq!(tool.ran(), 12, "已交付的第一笔写不能再执行");
}
