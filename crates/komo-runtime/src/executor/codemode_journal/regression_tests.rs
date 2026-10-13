use super::*;

#[tokio::test]
async fn interrupted_delivery_validates_reply_and_execution_body() {
    for reply in [
        Reply {
            value: Some(json!({"status": "uncertain"})),
            error: None,
            uncertain: true,
            execution: None,
        },
        Reply {
            value: None,
            error: Some("remote uncertain".into()),
            uncertain: true,
            execution: None,
        },
        Reply {
            value: None,
            error: None,
            uncertain: false,
            execution: None,
        },
    ] {
        let h = Harness::new();
        let executor = h.executor(vec![]);
        let (s, r) = h.open_run().await;
        let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
        start(&executor, &ctx).await;
        let output = executor
            .codemode_save(&ctx, "intent-1", &json!({}))
            .await
            .unwrap();
        executor
            .codemode_record(&ctx, "intent-1", Record::Intent { index: 1, output })
            .await
            .unwrap();
        let output = executor
            .codemode_save(&ctx, "reply-1", &reply)
            .await
            .unwrap();
        executor
            .codemode_record(&ctx, "delivered-1", Record::Delivered { index: 1, output })
            .await
            .unwrap();
        assert!(
            executor
                .codemode_check_interrupted(&ctx)
                .await
                .unwrap_err()
                .is_uncertain()
        );
    }
    let h = Harness::new();
    let executor = h.executor(vec![]);
    let (s, r) = h.open_run().await;
    let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
    start(&executor, &ctx).await;
    let output = executor
        .codemode_save(&ctx, "intent-1", &json!({}))
        .await
        .unwrap();
    executor
        .codemode_record(&ctx, "intent-1", Record::Intent { index: 1, output })
        .await
        .unwrap();
    let execution = executor
        .codemode_save(&ctx, "execution", &json!({"ok": true}))
        .await
        .unwrap();
    let reply = Reply {
        value: Some(json!({"ok": true})),
        error: None,
        uncertain: false,
        execution: Some(execution),
    };
    let output = executor
        .codemode_save(&ctx, "reply-1", &reply)
        .await
        .unwrap();
    executor
        .codemode_record(&ctx, "delivered-1", Record::Delivered { index: 1, output })
        .await
        .unwrap();
    executor.codemode_check_interrupted(&ctx).await.unwrap();
    executor
        .codemode_save(&ctx, "execution", &json!({"changed": true}))
        .await
        .unwrap();
    assert!(
        executor
            .codemode_check_interrupted(&ctx)
            .await
            .unwrap_err()
            .is_uncertain()
    );
}

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, StoreError};
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::refs::{
    AttemptRef, InterruptedOutput, OutputRef, PublishedOutput, ToolResultBody, VerifiedOutput,
};
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolOutput};

struct FailingStore(Arc<komo_kernel::test_support::MemOutputStore>);
struct FailingWriter(Box<dyn OutputWriter>);
#[async_trait]
impl OutputWriter for FailingWriter {
    async fn write_stdout(&mut self, _: &[u8]) -> Result<(), StoreError> {
        Err(StoreError::Io("injected stdout failure".into()))
    }
    async fn write_stderr(&mut self, _: &[u8]) -> Result<(), StoreError> {
        Err(StoreError::Io("injected stderr failure".into()))
    }
    fn attempt(&self) -> &AttemptRef {
        self.0.attempt()
    }
    fn bytes_written(&self) -> u64 {
        self.0.bytes_written()
    }
}
#[async_trait]
impl ToolOutputStore for FailingStore {
    async fn begin(&self, attempt: &AttemptRef) -> Result<Box<dyn OutputWriter>, StoreError> {
        Ok(Box::new(FailingWriter(self.0.begin(attempt).await?)))
    }
    async fn publish(
        &self,
        writer: Box<dyn OutputWriter>,
        body: ToolResultBody,
    ) -> Result<PublishedOutput, StoreError> {
        self.0.publish(writer, body).await
    }
    async fn open(&self, reference: &OutputRef) -> Result<VerifiedOutput, StoreError> {
        self.0.open(reference).await
    }
    async fn interrupted(
        &self,
        attempt: &AttemptRef,
        limit: usize,
    ) -> Result<InterruptedOutput, StoreError> {
        self.0.interrupted(attempt, limit).await
    }
}
// 学 MCP 的样子：先远端写，再写流；流写失败时转成 Failed。
struct StreamingTool {
    inner: Arc<RecordingTool>,
    stderr: bool,
    swallow: bool,
}
#[async_trait]
impl Tool for StreamingTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }
    fn replay(&self) -> Replay {
        Replay::Unsafe
    }
    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        self.inner.prepare(args, ctx).await
    }
    async fn execute(
        &self,
        plan: ExecutionPlan,
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let output = self.inner.execute(plan, ctx, sink).await?;
        let result = if self.stderr {
            sink.write_stderr(b"remote write succeeded").await
        } else {
            sink.write_stdout(b"remote write succeeded").await
        };
        if !self.swallow {
            result.map_err(|e| ToolError::Failed {
                message: e.to_string(),
            })?;
        }
        Ok(output)
    }
}
async fn sandbox() -> Option<Arc<crate::codemode::Sandbox>> {
    match crate::codemode::Sandbox::probe(std::path::Path::new("/usr/bin/python3")).await {
        Ok(s) => Some(Arc::new(s)),
        Err(e) if cfg!(target_os = "macos") => panic!("sandbox unavailable: {e}"),
        Err(_) => None,
    }
}

#[tokio::test]
async fn stream_failure_caught_by_python_cannot_continue_writes() {
    let Some(sandbox) = sandbox().await else {
        return;
    };
    for (stderr, swallow) in [(false, false), (true, false), (false, true), (true, true)] {
        let h = Harness::new();
        let remote = mcp(RecoveryMode::NoSafeRecovery, Replay::Unsafe);
        let tool = Arc::new(StreamingTool {
            inner: remote.clone(),
            stderr,
            swallow,
        });
        let _registered = h.executor(vec![tool.clone()]);
        let executor = ToolExecutor::new(
            vec![tool],
            h.ledger.clone(),
            Arc::new(FailingStore(h.outputs.clone())),
            Arc::new(h.clock.clone()),
        )
        .with_codemode(Some(sandbox.clone()));
        let (s, r) = h.open_run().await;
        let ctx = h.tool_context(&s, &r, &ToolCallId::from_raw("p"));
        let code = "try:\n    tools.mcp__test__write(value=1)\nexcept ToolError:\n    pass\ntry:\n    tools.mcp__test__write(value=2)\nexcept ToolError:\n    pass\ntext('caught')";
        let mut sink = h
            .outputs
            .begin(&ToolExecutor::codemode_attempt(&ctx, "sink"))
            .await
            .unwrap();
        let error = executor
            .run_codemode(code, &h.env(&s, &r), &ctx, sink.as_mut())
            .await
            .unwrap_err();
        assert!(error.is_uncertain(), "{error}");
        assert!(error.to_string().contains("输出写入失败"), "{error}");
        assert_eq!(
            remote.ran(),
            1,
            "caught stream error must prohibit a second remote write"
        );
        let journal = executor.codemode_load(&ctx).await.unwrap();
        assert!(journal.children[&1].started);
        assert!(journal.children[&1].delivered.is_none());
        assert!(
            executor
                .run_codemode(code, &h.env(&s, &r), &ctx, sink.as_mut())
                .await
                .unwrap_err()
                .is_uncertain()
        );
        assert_eq!(remote.ran(), 1, "unsafe write must not rerun on recovery");
    }
}

#[tokio::test]
async fn cancellation_after_delivered_uncertain_preserves_uncertain_and_stops_round() {
    let Some(sandbox) = sandbox().await else {
        return;
    };
    let h = Harness::new();
    let remote = Arc::new(
        RecordingTool::new(
            "mcp__test__write",
            Operation::McpCall {
                server: "test".into(),
                tool: "write".into(),
                read_only: false,
            },
        )
        .with_outcome(Err(ToolError::Uncertain {
            message: "remote response lost".into(),
        })),
    );
    let codemode = Arc::new(crate::tools::codemode::CodemodeTool::new());
    let _registered = h.executor(vec![remote.clone(), codemode.clone()]);
    let executor = ToolExecutor::new(
        vec![remote.clone(), codemode],
        h.ledger.clone(),
        h.outputs.clone(),
        Arc::new(h.clock.clone()),
    )
    .with_codemode(Some(sandbox));
    let (s, r) = h.open_run().await;
    let code = "try:\n    tools.mcp__test__write(value=1)\nexcept ToolError:\n    pass\nwhile True:\n    pass";
    let calls = h
        .record_round(
            &r,
            &[
                ("codemode", json!({"code": code})),
                ("mcp__test__write", json!({"value": 2})),
            ],
        )
        .await;
    let cancel = komo_kernel::types::tool::CancelToken::new();
    let monitor_cancel = cancel.clone();
    let ledger = h.ledger.clone();
    let monitor = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if ledger.events().iter().any(|e| matches!(&e.payload, EventPayload::CodemodeJournal(body) if body.record.get("phase").and_then(|v| v.as_str()) == Some("delivered"))) { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("delivered must be committed before cancellation");
        monitor_cancel.cancel();
    });
    let outcome = executor
        .execute_round(calls, &h.env_with_cancel(&s, &r, cancel))
        .await
        .unwrap();
    monitor.await.unwrap();
    assert!(
        matches!(outcome.stop, Some(crate::executor::RoundStop::Cancelled)),
        "{:?}",
        outcome.stop
    );
    assert_eq!(
        outcome.remaining.len(),
        2,
        "cancel retains the parent and must stop the next write"
    );
    assert_eq!(remote.ran(), 1);
    let result = h
        .ledger
        .events()
        .into_iter()
        .find_map(|e| match e.payload {
            EventPayload::ToolResult(result) => Some(result),
            _ => None,
        })
        .unwrap();
    assert_eq!(result.status, ToolResultStatus::Uncertain);
    let body = h.outputs.open(&result.output_ref).await.unwrap().body;
    assert!(body.error.unwrap().contains("remote response lost"));
}
