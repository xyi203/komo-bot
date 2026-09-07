//! Behavior tests through the `ToolExecutor` interface — a round of calls
//! in, outcomes out. The executor owns lookup/retry/ledger/cap, so that is
//! where they are asserted.

use super::*;
use async_trait::async_trait;
use komo_core::domain::tool::ToolOutput;
use serde_json::Value;

/// Render args as the tool's *payload* rather than as JSON: these tests pass
/// bare text (the non-JSON arg path), and asserting on `"x"` with quotes
/// would be asserting about serde, not about the executor.
fn arg_text(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct EchoTool;
#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echoes its input"
    }
    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!("echoed: {}", arg_text(&input))))
    }
}

/// A stand-in registered under a real tool's name, so the catalog filter is
/// tested against the names it actually maps (`policy_scope`).
struct NamedTool(&'static str);
#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> &'static str {
        "stand-in"
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("ok"))
    }
}

fn catalog_with(names: &[&'static str]) -> ToolExecutor {
    let mut tools = ToolExecutor::new(ToolExecutionConfig::default());
    for name in names {
        tools.register(Arc::new(NamedTool(name)));
    }
    tools
}

fn wildcard_deny(category: Category, access: Option<Access>) -> Policy {
    use komo_core::domain::policy::{Effect, Matcher, Rule, Verdict};
    Policy::new(
        vec![Rule {
            channels: None,
            category,
            matcher: Matcher::Any,
            value: String::new(),
            access,
            effect: Effect::Deny,
            include_dangerous: false,
            unattended: false,
        }],
        Verdict::Ask,
    )
}

const FILE_AND_SHELL: &[&str] = &[
    "read",
    "grep",
    "glob",
    "write",
    "edit",
    "apply_patch",
    "shell",
    "time",
    "memory",
];

#[test]
fn a_wholly_denied_tool_leaves_the_catalog() {
    let mut tools = catalog_with(FILE_AND_SHELL);
    assert_eq!(
        tools.drop_policy_denied(&wildcard_deny(Category::Shell, None)),
        vec!["shell".to_string()]
    );
    let left: std::collections::BTreeSet<String> = tools
        .definitions()
        .iter()
        .map(|t| t.name().into())
        .collect();
    assert!(!left.contains("shell"));
    assert!(left.contains("read"), "only the denied tool goes");
    assert!(left.contains("time"));
}

/// Banning writes must not take the readers away — the file category is the
/// one that splits, and losing `read`/`grep` here would blind the model.
#[test]
fn denying_file_writes_keeps_the_readers() {
    let mut tools = catalog_with(FILE_AND_SHELL);
    let dropped = tools.drop_policy_denied(&wildcard_deny(
        Category::File,
        Some(komo_core::domain::policy::Access::Write),
    ));
    assert_eq!(dropped, vec!["apply_patch", "edit", "write"]);
    let left: std::collections::BTreeSet<String> = tools
        .definitions()
        .iter()
        .map(|t| t.name().into())
        .collect();
    assert!(left.contains("read") && left.contains("grep") && left.contains("glob"));
    assert!(left.contains("shell"), "shell is its own category");
}

#[test]
fn an_empty_policy_drops_nothing() {
    let mut tools = catalog_with(FILE_AND_SHELL);
    assert!(tools.drop_policy_denied(&Policy::default()).is_empty());
    assert_eq!(tools.definitions().len(), FILE_AND_SHELL.len());
}

struct SecretTool;
#[async_trait]
impl Tool for SecretTool {
    fn name(&self) -> &'static str {
        "secretive"
    }
    fn description(&self) -> &'static str {
        "redacts its args"
    }
    fn redact_args(&self, _args: &str) -> String {
        "[redacted]".to_string()
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("done"))
    }
}

struct PanickingTool;
#[async_trait]
impl Tool for PanickingTool {
    fn name(&self) -> &'static str {
        "boom"
    }
    fn description(&self) -> &'static str {
        "always panics"
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        panic!("kaboom");
    }
}

/// A tool that fails its first `fail_times` calls (with `error_msg`) then
/// succeeds, counting every call. Lets a test assert how many attempts the
/// retry loop made.
struct FlakyTool {
    calls: Arc<AtomicUsize>,
    fail_times: usize,
    error_msg: &'static str,
    idempotent: bool,
}

#[async_trait]
impl Tool for FlakyTool {
    fn name(&self) -> &'static str {
        "flaky"
    }
    fn description(&self) -> &'static str {
        "fails a few times then succeeds"
    }
    fn idempotent(&self) -> bool {
        self.idempotent
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let n = self.calls.fetch_add(1, Ordering::Relaxed);
        if n < self.fail_times {
            Err(ToolError::Failed(anyhow::anyhow!("{}", self.error_msg)))
        } else {
            Ok(ToolOutput::text("ok"))
        }
    }
}

fn flaky(
    fail_times: usize,
    error_msg: &'static str,
    idempotent: bool,
) -> (Arc<FlakyTool>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(FlakyTool {
        calls: calls.clone(),
        fail_times,
        error_msg,
        idempotent,
    });
    (tool, calls)
}

fn executor(tools: Vec<Arc<dyn Tool>>, config: ToolExecutionConfig) -> ToolExecutor {
    let mut executor = ToolExecutor::new(config);
    for t in tools {
        executor.register(t);
    }
    executor
}

fn call(name: &str, args: &str) -> ToolCallReq {
    ToolCallReq {
        id: format!("id-{name}"),
        call_id: None,
        name: name.to_string(),
        args: args.to_string(),
    }
}

fn ledgered() -> ToolTurnContext {
    ToolTurnContext {
        session: SessionContext::detached("cli:test"),
        run: Some(RunContext::new("run-1".into())),
        budget: TurnResultBudget::unlimited(),
        spin: SpinDetector::default(),
    }
}

fn unledgered() -> ToolTurnContext {
    ToolTurnContext {
        session: SessionContext::detached("cli:test"),
        run: None,
        budget: TurnResultBudget::unlimited(),
        spin: SpinDetector::default(),
    }
}

async fn one(executor: &ToolExecutor, req: ToolCallReq, context: &ToolTurnContext) -> ToolOutcome {
    executor
        .execute_round(std::slice::from_ref(&req), context)
        .await
        .remove(0)
}

/// Counts what reached the log, and how many barriers it took.
#[derive(Default)]
struct CountingEvents {
    appended: Mutex<Vec<SessionEventKind>>,
    flushes: AtomicUsize,
}

#[async_trait]
impl SessionEventRepository for CountingEvents {
    async fn session_ids(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
    async fn append(
        &self,
        _session_id: &str,
        kinds: Vec<SessionEventKind>,
    ) -> anyhow::Result<Vec<komo_core::domain::session_event::SessionEvent>> {
        self.appended.lock().unwrap().extend(kinds);
        Ok(Vec::new())
    }
    async fn durable_flush(&self, _session_id: &str) -> anyhow::Result<()> {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    async fn events(
        &self,
        _session_id: &str,
    ) -> anyhow::Result<Vec<komo_core::domain::session_event::SessionEvent>> {
        Ok(Vec::new())
    }
    async fn events_from(
        &self,
        _session_id: &str,
        _seq: u64,
    ) -> anyhow::Result<Vec<komo_core::domain::session_event::SessionEvent>> {
        Ok(Vec::new())
    }
    async fn surface(
        &self,
        _session_id: &str,
    ) -> anyhow::Result<Option<komo_core::domain::session_event::SurfaceProjection>> {
        Ok(None)
    }
    async fn turn_boundary(&self, _session_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn retain(&self, _session_id: &str, _keep_from: u64) -> anyhow::Result<Option<u64>> {
        Ok(None)
    }
}

/// One round, **one** barrier: every call's dispatch intent is batched and
/// made durable once, before any tool runs. An fsync per call would put a
/// disk write between every dispatch in a round that exists to run them at
/// once — and the recovery rule needs only that the intent landed before
/// the effects, not that it landed ten times.
#[tokio::test]
async fn a_round_makes_its_whole_dispatch_intent_durable_in_one_barrier() {
    let events = Arc::new(CountingEvents::default());
    let mut executor = executor(vec![Arc::new(EchoTool)], ToolExecutionConfig::default());
    executor = executor.with_events(events.clone());
    let context = ledgered();

    let calls: Vec<ToolCallReq> = (0..10)
        .map(|i| ToolCallReq {
            id: format!("id-{i}"),
            call_id: Some(format!("call-{i}")),
            name: "echo".into(),
            args: format!("\"hi {i}\""),
        })
        .collect();
    let outcomes = executor.execute_round(&calls, &context).await;
    assert_eq!(outcomes.len(), 10);

    let appended = events.appended.lock().unwrap();
    let started = appended
        .iter()
        .filter(|kind| matches!(kind, SessionEventKind::ToolCallStarted(_)))
        .count();
    let settled = appended
        .iter()
        .filter(|kind| matches!(kind, SessionEventKind::ToolCallSettled(_)))
        .count();
    assert_eq!(started, 10, "every call declares itself before it runs");
    assert_eq!(settled, 10, "and settles on its own");
    assert_eq!(
        events.flushes.load(Ordering::Relaxed),
        1,
        "the started barrier is one flush for the whole round; settles ride \
             the next one"
    );
    // The order the continuation is rebuilt in comes from `call_index`, so
    // the started events have to carry the provider's order, not completion
    // order.
    let indexes: Vec<u32> = appended
        .iter()
        .filter_map(|kind| match kind {
            SessionEventKind::ToolCallStarted(call) => Some(call.call_index),
            _ => None,
        })
        .collect();
    assert_eq!(indexes, (0..10).collect::<Vec<u32>>());
}

#[test]
fn the_catalog_is_name_sorted_regardless_of_registration_order() {
    // The tool schemas are serialized into every request; a provider
    // prompt cache matches on exact bytes, so the order must be stable
    // across restarts no matter how wiring happens to register.
    let executor = executor(
        vec![
            Arc::new(NamedTool("zeta")),
            Arc::new(NamedTool("alpha")),
            Arc::new(NamedTool("mid")),
        ],
        ToolExecutionConfig::default(),
    );
    let names: Vec<&str> = executor.definitions().iter().map(|t| t.name()).collect();
    assert_eq!(names, vec!["alpha", "mid", "zeta"]);
}

/// A model that keeps re-issuing one call gets refused rather than served:
/// the first two answers are already in the transcript, so a third run
/// cannot say anything new — it would only burn rounds.
#[tokio::test]
async fn an_identical_call_repeated_is_refused_instead_of_run() {
    let (tool, calls) = flaky(0, "unused", false);
    let executor = executor(vec![tool], ToolExecutionConfig::default());
    let context = unledgered();

    for _ in 0..2 {
        let out = one(&executor, call("flaky", "{}"), &context).await;
        assert!(!out.content.starts_with("error:"), "{}", out.content);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 2);

    let refused = one(&executor, call("flaky", "{}"), &context).await;
    assert!(
        refused.content.contains("already called twice"),
        "{}",
        refused.content
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "the refused call must not reach the tool"
    );
    assert!(!context.spin.should_stop(), "one refusal is not a stop yet");

    // Asking again after the refusal ends the turn; the loop reads this.
    let _ = one(&executor, call("flaky", "{}"), &context).await;
    assert!(context.spin.should_stop());
}

/// Same tool, different arguments is progress — and anything else in
/// between resets the streak, so a poll interleaved with real work never
/// trips the detector.
#[tokio::test]
async fn differing_arguments_and_interleaved_calls_never_trip_the_detector() {
    let executor = executor(
        vec![Arc::new(EchoTool), Arc::new(NamedTool("other"))],
        ToolExecutionConfig::default(),
    );
    let context = unledgered();

    for i in 0..5 {
        let out = one(&executor, call("echo", &format!("{{\"n\":{i}}}")), &context).await;
        assert!(!out.content.contains("already called twice"));
    }
    // Alternating: each repeat is broken by the other tool.
    for _ in 0..5 {
        let echoed = one(&executor, call("echo", "{}"), &context).await;
        assert!(!echoed.content.contains("already called twice"));
        let _ = one(&executor, call("other", "{}"), &context).await;
    }
    assert!(!context.spin.should_stop());
}

/// Empty arguments mean the model's output was cut off mid-call. Running it
/// would act on defaults rather than on what was asked — but a tool that
/// genuinely takes none still sends `{}`, which must go through.
#[tokio::test]
async fn empty_arguments_are_refused_but_an_empty_object_is_not() {
    let (tool, calls) = flaky(0, "unused", false);
    let executor = executor(vec![tool], ToolExecutionConfig::default());
    let context = unledgered();

    let truncated = one(&executor, call("flaky", "   "), &context).await;
    assert!(
        truncated.content.contains("arrived empty"),
        "{}",
        truncated.content
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0, "nothing should have run");

    let no_args = one(&executor, call("flaky", "{}"), &context).await;
    assert!(
        !no_args.content.starts_with("error:"),
        "{}",
        no_args.content
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

/// A truncated call must not take the rest of its round down with it: the
/// calls streamed before it are complete.
#[tokio::test]
async fn a_truncated_call_does_not_block_the_rest_of_its_round() {
    let executor = executor(vec![Arc::new(EchoTool)], ToolExecutionConfig::default());
    let outcomes = executor
        .execute_round(
            &[
                call("echo", "{\"a\":1}"),
                call("echo", ""),
                call("echo", "{\"b\":2}"),
            ],
            &unledgered(),
        )
        .await;
    assert!(!outcomes[0].content.contains("arrived empty"));
    assert!(outcomes[1].content.contains("arrived empty"));
    assert!(!outcomes[2].content.contains("arrived empty"));
}

#[tokio::test]
async fn round_preserves_order_and_maps_unknown_tools() {
    let executor = executor(vec![Arc::new(EchoTool)], ToolExecutionConfig::default());
    let outcomes = executor
        .execute_round(
            &[call("echo", "a"), call("nope", "{}"), call("echo", "b")],
            &unledgered(),
        )
        .await;
    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0].content, "echoed: a");
    assert_eq!(outcomes[1].content, "error: unknown tool `nope`");
    assert_eq!(outcomes[2].content, "echoed: b");
    assert_eq!(outcomes[1].id, "id-nope", "ids line up with calls");
}

#[tokio::test]
async fn ledgered_call_records_one_step() {
    let context = ledgered();
    let run = context.run.clone().unwrap();
    let executor = executor(vec![Arc::new(EchoTool)], ToolExecutionConfig::default());
    let out = one(&executor, call("echo", "hi"), &context).await;
    assert_eq!(out.content, "echoed: hi");

    let steps = run.steps();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].run_id, "run-1");
    assert_eq!(steps[0].seq, 0);
    assert_eq!(steps[0].tool_name, "echo");
    assert!(steps[0].ok);
    assert!(steps[0].result.contains("echoed: hi"));
    assert!(steps[0].error.is_empty());
}

/// "Did that go through?" is the question an operator asks a week later,
/// and a run the model was told was uncertain must not read as a plain
/// failure in the ledger — by then the `ToolError` variant is long gone, so
/// the marker has to survive the trip through `anyhow`.
#[tokio::test]
async fn an_uncertain_call_is_recorded_as_uncertain_not_merely_failed() {
    struct FlakyWriter;
    #[async_trait]
    impl Tool for FlakyWriter {
        fn name(&self) -> &'static str {
            "flaky_write"
        }
        fn description(&self) -> &'static str {
            "mutates something, ambiguously"
        }
        async fn call(&self, _i: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
            Err(ToolError::Failed(anyhow::anyhow!(
                "upstream returned HTTP 503: service unavailable"
            )))
        }
    }

    let context = ledgered();
    let run = context.run.clone().unwrap();
    let executor = executor(vec![Arc::new(FlakyWriter)], ToolExecutionConfig::default());
    let out = one(&executor, call("flaky_write", "{}"), &context).await;
    assert!(
        out.content.contains("may or may not have taken effect"),
        "got: {}",
        out.content
    );

    let steps = run.steps();
    assert!(!steps[0].ok);
    assert!(
        steps[0].uncertain,
        "the ledger must keep failed and may-have-landed apart"
    );
}

#[tokio::test]
async fn redaction_happens_before_the_ledger() {
    let context = ledgered();
    let run = context.run.clone().unwrap();
    let executor = executor(vec![Arc::new(SecretTool)], ToolExecutionConfig::default());
    one(&executor, call("secretive", "token=hunter2"), &context).await;
    let steps = run.steps();
    assert_eq!(steps[0].args, "[redacted]");
    assert!(!steps[0].args.contains("hunter2"));
}

#[tokio::test]
async fn panicking_tool_becomes_an_error_outcome_and_error_step() {
    let context = ledgered();
    let run = context.run.clone().unwrap();
    let executor = executor(
        vec![Arc::new(PanickingTool)],
        ToolExecutionConfig::default(),
    );
    let out = one(&executor, call("boom", "{}"), &context).await;
    assert!(out.content.contains("panicked"), "got: {}", out.content);
    assert!(out.content.contains("kaboom"));

    let steps = run.steps();
    assert_eq!(steps.len(), 1);
    assert!(!steps[0].ok);
    assert!(steps[0].error.contains("panicked"));
    assert!(steps[0].result.is_empty());
}

#[tokio::test(start_paused = true)]
async fn connection_error_is_retried_even_for_non_idempotent_tool() {
    // The request never reached the server, so a side effect can't have
    // landed — safe to retry regardless of idempotency.
    let (tool, calls) = flaky(2, "connection refused", false);
    let executor = executor(vec![tool], ToolExecutionConfig::default());
    let out = one(&executor, call("flaky", "{}"), &unledgered()).await;
    assert_eq!(out.content, "ok");
    assert_eq!(calls.load(Ordering::Relaxed), 3); // 2 failures + 1 success
}

#[tokio::test(start_paused = true)]
async fn terminal_error_is_not_retried() {
    let (tool, calls) = flaky(usize::MAX, "invalid arguments: bad json", true);
    let executor = executor(vec![tool], ToolExecutionConfig::default());
    let out = one(&executor, call("flaky", "{}"), &unledgered()).await;
    assert!(out.content.contains("invalid arguments"));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test(start_paused = true)]
async fn ambiguous_error_is_retried_only_for_idempotent_tool() {
    // Idempotent → retried.
    let (tool, calls) = flaky(1, "operation timed out", true);
    let executor = self::executor(vec![tool], ToolExecutionConfig::default());
    let out = one(&executor, call("flaky", "{}"), &unledgered()).await;
    assert_eq!(out.content, "ok");
    assert_eq!(calls.load(Ordering::Relaxed), 2);

    // Non-idempotent → a timeout might have applied server-side; don't retry.
    let (tool, calls) = flaky(usize::MAX, "operation timed out", false);
    let executor = self::executor(vec![tool], ToolExecutionConfig::default());
    let _ = one(&executor, call("flaky", "{}"), &unledgered()).await;
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test(start_paused = true)]
async fn retries_are_bounded_then_error_surfaces() {
    let (tool, calls) = flaky(usize::MAX, "connection refused", false);
    let executor = executor(vec![tool], ToolExecutionConfig::default());
    let out = one(&executor, call("flaky", "{}"), &unledgered()).await;
    assert!(out.content.to_lowercase().contains("connection refused"));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        retry::TOOL_RETRY_MAX_ATTEMPTS
    );
}

#[tokio::test(start_paused = true)]
async fn retry_collapses_into_a_single_ledger_step() {
    let context = ledgered();
    let run = context.run.clone().unwrap();
    let (tool, calls) = flaky(1, "connection refused", false);
    let executor = executor(vec![tool], ToolExecutionConfig::default());
    let out = one(&executor, call("flaky", "{}"), &context).await;
    assert_eq!(out.content, "ok");
    assert_eq!(calls.load(Ordering::Relaxed), 2);

    let steps = run.steps();
    assert_eq!(
        steps.len(),
        1,
        "retries must record one step, not one per attempt"
    );
    assert!(steps[0].ok);
    assert_eq!(steps[0].seq, 0);
}

#[tokio::test]
async fn budget_counts_logical_calls_and_refuses_past_the_cap() {
    let (tool, calls) = flaky(0, "unused", false); // never fails; just counts
    let executor = executor(
        vec![tool],
        ToolExecutionConfig {
            max_calls_per_turn: 5,
            ..Default::default()
        },
    );
    let context = ledgered();
    let run = context.run.clone().unwrap();
    // Distinct arguments per call: this exercises the *budget*, and calls
    // that are byte-identical would be stopped earlier by the spin detector.
    for i in 0..5 {
        let out = one(
            &executor,
            call("flaky", &format!("{{\"i\":{i}}}")),
            &context,
        )
        .await;
        assert_eq!(out.content, "ok");
    }
    // The next call is refused without ever reaching the tool.
    let out = one(&executor, call("flaky", "{\"i\":9}"), &context).await;
    assert!(out.content.contains("budget"), "got: {}", out.content);
    assert_eq!(calls.load(Ordering::Relaxed), 5);

    // The refusal is still recorded as a failed step, for audit visibility.
    let steps = run.steps();
    assert_eq!(steps.len(), 6);
    assert!(!steps.last().unwrap().ok);
    assert!(steps.last().unwrap().error.contains("budget"));
}

struct BigTool;
#[async_trait]
impl Tool for BigTool {
    fn name(&self) -> &'static str {
        "big"
    }
    fn description(&self) -> &'static str {
        "returns a large result"
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("x".repeat(10_000)))
    }
}

/// A structured view rides to the ledger and nowhere near the model — the
/// whole point of the third view is that the context window doesn't pay for it.
struct StructuredTool;
#[async_trait]
impl Tool for StructuredTool {
    fn name(&self) -> &'static str {
        "structured"
    }
    fn description(&self) -> &'static str {
        "returns a structured view"
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("done")
            .with_structured(serde_json::json!({ "exit": 0, "truncated": false })))
    }
}

#[tokio::test]
async fn a_structured_view_reaches_the_ledger_but_not_the_model() {
    let context = ledgered();
    let run = context.run.clone().unwrap();
    let executor = executor(
        vec![Arc::new(StructuredTool)],
        ToolExecutionConfig::default(),
    );
    let out = one(&executor, call("structured", "{}"), &context).await;

    assert_eq!(out.content, "done", "the model sees text only");
    let steps = run.steps();
    assert_eq!(
        steps[0].structured,
        serde_json::json!({ "exit": 0, "truncated": false })
    );
}

/// A tool that claims the cancel signal (as `shell` does) ends the call, and
/// the step it leaves must read like the run's own stop — one wording for one
/// event — and must **not** be retried: a deliberate stop is not a transient
/// failure, and `web_fetch` is `idempotent`, so the classifier is what stands
/// between a cancel and two more attempts.
#[tokio::test]
async fn a_claimed_cancel_ends_the_call_once_and_reads_as_cancelled() {
    use komo_core::domain::cancel::{CANCELLED_ERROR, CancelSignal, Cancelled};

    struct AlreadyCancelled;
    #[async_trait]
    impl CancelSignal for AlreadyCancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
        async fn cancelled(&self) {}
    }

    /// Waits for the signal like `shell` does, counting attempts.
    struct Claiming(Arc<AtomicUsize>);
    #[async_trait]
    impl Tool for Claiming {
        fn name(&self) -> &'static str {
            "claiming"
        }
        fn description(&self) -> &'static str {
            "waits for cancellation"
        }
        fn idempotent(&self) -> bool {
            true
        }
        async fn call(&self, _input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            ctx.cancelled().await;
            Err(ToolError::Failed(Cancelled.into()))
        }
    }

    let attempts = Arc::new(AtomicUsize::new(0));
    let executor = executor(
        vec![Arc::new(Claiming(attempts.clone()))],
        ToolExecutionConfig::default(),
    );
    let context = ToolTurnContext {
        session: SessionContext::detached("cli:test").with_cancel(Arc::new(AlreadyCancelled)),
        run: Some(RunContext::new("run-1".into())),
        budget: TurnResultBudget::unlimited(),
        spin: SpinDetector::default(),
    };
    let run = context.run.clone().unwrap();

    let out = one(&executor, call("claiming", "{}"), &context).await;
    assert!(out.content.contains(CANCELLED_ERROR), "{}", out.content);
    assert_eq!(attempts.load(Ordering::Relaxed), 1, "a cancel is terminal");
    let steps = run.steps();
    assert!(!steps[0].ok);
    assert_eq!(steps[0].error, CANCELLED_ERROR);
}

#[tokio::test]
async fn a_failed_call_records_no_structured_view() {
    let context = ledgered();
    let run = context.run.clone().unwrap();
    let executor = executor(
        vec![Arc::new(PanickingTool)],
        ToolExecutionConfig::default(),
    );
    one(&executor, call("boom", "{}"), &context).await;
    let steps = run.steps();
    assert!(!steps[0].ok);
    assert!(steps[0].structured.is_null());
}

#[test]
fn an_oversized_structured_view_is_replaced_rather_than_cut() {
    let big = serde_json::json!({ "blob": "x".repeat(STEP_FIELD_CAP) });
    let capped = cap_structured(big);
    assert!(capped["_elided"].is_string(), "{capped}");
    // Still valid JSON — a reader must never have to handle half a document.
    assert!(capped.is_object());
    // Under the cap it passes through untouched.
    let small = serde_json::json!({ "exit": 1 });
    assert_eq!(cap_structured(small.clone()), small);
    assert!(cap_structured(serde_json::Value::Null).is_null());
}

/// 10's core promise: an over-limit result keeps its tail, the full text is on
/// disk, and the step says where.
#[tokio::test]
async fn an_over_limit_result_is_stored_and_previewed_with_its_path_on_the_step() {
    struct Chatty;
    #[async_trait]
    impl Tool for Chatty {
        fn name(&self) -> &'static str {
            "chatty"
        }
        fn description(&self) -> &'static str {
            "returns many lines"
        }
        async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(
                (0..400).map(|i| format!("line {i}\n")).collect::<String>(),
            ))
        }
    }

    let root = std::env::temp_dir().join("komo_exec_output_store");
    let _ = std::fs::remove_dir_all(&root);
    let store = Arc::new(crate::tool_output_store::ToolOutputStore::new(root.clone()));
    let context = ledgered();
    let run = context.run.clone().unwrap();
    let mut executor = ToolExecutor::new(ToolExecutionConfig {
        max_result_bytes: 512,
        ..Default::default()
    })
    .with_output_store(store);
    executor.register(Arc::new(Chatty));

    let out = one(&executor, call("chatty", "{}"), &context).await;
    assert!(out.content.contains("line 0"));
    assert!(out.content.contains("line 399"), "the tail must survive");

    let steps = run.steps();
    let stored = &steps[0].output_paths[0];
    assert!(out.content.contains(stored), "the preview names the file");
    assert!(
        std::fs::read_to_string(stored)
            .unwrap()
            .contains("line 200")
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// No ledger (aux sub-agent, a sweep) ⇒ no file: there is no run to point an
/// operator at and no follow-up turn to `read` it, so it would be litter.
#[tokio::test]
async fn an_unledgered_call_truncates_instead_of_storing() {
    let root = std::env::temp_dir().join("komo_exec_output_store_unledgered");
    let _ = std::fs::remove_dir_all(&root);
    let store = Arc::new(crate::tool_output_store::ToolOutputStore::new(root.clone()));
    let mut executor = ToolExecutor::new(ToolExecutionConfig {
        max_result_bytes: 1024,
        ..Default::default()
    })
    .with_output_store(store);
    executor.register(Arc::new(BigTool));

    let out = one(&executor, call("big", "{}"), &unledgered()).await;
    assert!(out.content.contains("truncated"));
    assert!(!root.exists(), "nothing should be written without a ledger");
}

#[tokio::test]
async fn two_executors_carry_different_result_caps() {
    // The cap is instance policy, not a process global: the same tool
    // through two executors gets two different ceilings.
    let tight = executor(
        vec![Arc::new(BigTool)],
        ToolExecutionConfig {
            max_result_bytes: 1024,
            ..Default::default()
        },
    );
    let roomy = executor(
        vec![Arc::new(BigTool)],
        ToolExecutionConfig {
            max_result_bytes: 64 * 1024,
            ..Default::default()
        },
    );
    let capped = one(&tight, call("big", "{}"), &unledgered()).await;
    assert!(capped.content.len() <= 1024 + 200);
    assert!(capped.content.contains("truncated"));

    let free = one(&roomy, call("big", "{}"), &unledgered()).await;
    assert_eq!(free.content.len(), 10_000, "no truncation under the cap");
}

#[tokio::test]
async fn unledgered_context_records_nothing_and_still_works() {
    let executor = executor(vec![Arc::new(EchoTool)], ToolExecutionConfig::default());
    let out = one(&executor, call("echo", "x"), &unledgered()).await;
    assert_eq!(out.content, "echoed: x");
}

/// Reports the ambient job-grant count from *inside* a tool call — the
/// vantage point of an approver consulted mid-tool.
struct GrantProbe;
#[async_trait]
impl Tool for GrantProbe {
    fn name(&self) -> &'static str {
        "grant_probe"
    }
    fn description(&self) -> &'static str {
        "reports the ambient job grants"
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(current_job_grants().len().to_string()))
    }
}

/// The cron-job regression: each tool call runs on a spawned task, and a
/// task-local does not cross `tokio::spawn` — so unless the executor
/// re-installs the turn's job grants (like it does the session), a grant
/// approved at job creation never reaches the approver at run time.
#[tokio::test]
async fn job_grants_reach_a_tool_across_its_spawn() {
    use komo_core::domain::policy::{Category, Effect, Matcher, Rule};
    let executor = executor(vec![Arc::new(GrantProbe)], ToolExecutionConfig::default());
    let grant = Rule {
        channels: None,
        category: Category::HomeAssistant,
        matcher: Matcher::Exact,
        value: "climate.set_temperature".to_string(),
        access: None,
        effect: Effect::Allow,
        include_dangerous: false,
        unattended: true,
    };
    let out = with_job_grants(
        vec![grant],
        one(&executor, call("grant_probe", "{}"), &unledgered()),
    )
    .await;
    assert_eq!(out.content, "1", "the job's grant must be visible in-tool");

    // …and outside the scope the same tool sees none.
    let out = one(&executor, call("grant_probe", "{}"), &unledgered()).await;
    assert_eq!(out.content, "0");
}

/// A tool that never returns on its own — stands in for a hung shell command
/// or a timeout-less HTTP client.
struct HangingTool;
#[async_trait]
impl Tool for HangingTool {
    fn name(&self) -> &'static str {
        "hang"
    }
    fn description(&self) -> &'static str {
        "never returns"
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(ToolOutput::text("unreachable"))
    }
}

#[tokio::test(start_paused = true)]
async fn hung_tool_is_aborted_by_the_call_timeout() {
    // Without a timeout this call would await forever and wedge the turn.
    let executor = executor(
        vec![Arc::new(HangingTool)],
        ToolExecutionConfig {
            max_call_duration: Some(Duration::from_secs(1)),
            ..Default::default()
        },
    );
    let out = one(&executor, call("hang", "{}"), &unledgered()).await;
    assert!(
        out.content.contains("did not report back within its 1s"),
        "got: {}",
        out.content
    );
    // `HangingTool` takes the default `idempotent() == false`, so aborting
    // the wait says nothing about whether the work landed. The model has to
    // be told that, or it will simply call the tool again.
    assert!(
        out.content.contains("may or may not have taken effect"),
        "an aborted non-idempotent call must not read as a plain failure, got: {}",
        out.content
    );
}

/// The same abort on a tool that can be safely repeated stays an ordinary
/// failure — there is nothing to check first.
#[tokio::test(start_paused = true)]
async fn a_hung_idempotent_tool_is_just_a_failure() {
    struct HangingReader;
    #[async_trait]
    impl Tool for HangingReader {
        fn name(&self) -> &'static str {
            "hang_ro"
        }
        fn description(&self) -> &'static str {
            "never returns, changes nothing"
        }
        fn idempotent(&self) -> bool {
            true
        }
        async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(ToolOutput::text("unreachable"))
        }
    }

    let executor = executor(
        vec![Arc::new(HangingReader)],
        ToolExecutionConfig {
            max_call_duration: Some(Duration::from_secs(1)),
            ..Default::default()
        },
    );
    let out = one(&executor, call("hang_ro", "{}"), &unledgered()).await;
    assert!(
        out.content.contains("did not report back within its 1s"),
        "got: {}",
        out.content
    );
    assert!(
        !out.content.contains("may or may not have taken effect"),
        "a read-only tool has nothing to check, got: {}",
        out.content
    );
}

/// A tool that legitimately waits (a sub-agent completion, a human at an
/// approval prompt) declares its own ceiling, and the executor honors it over
/// the config default — the bug being that `delegate` was killed at 120s
/// mid-completion.
#[tokio::test(start_paused = true)]
async fn a_tools_own_ceiling_overrides_the_config_default() {
    struct PatientTool;
    #[async_trait]
    impl Tool for PatientTool {
        fn name(&self) -> &'static str {
            "patient"
        }
        fn description(&self) -> &'static str {
            "waits longer than the default allows"
        }
        fn max_duration(&self) -> Option<Duration> {
            Some(Duration::from_secs(600))
        }
        async fn call(&self, _i: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
            tokio::time::sleep(Duration::from_secs(300)).await;
            Ok(ToolOutput::text("finished"))
        }
    }

    let executor = executor(
        vec![Arc::new(PatientTool)],
        ToolExecutionConfig {
            // The default would abort this at 1s.
            max_call_duration: Some(Duration::from_secs(1)),
            ..Default::default()
        },
    );
    let out = one(&executor, call("patient", "{}"), &unledgered()).await;
    assert_eq!(out.content, "finished");
}

/// …but its ceiling is still a ceiling: a genuine hang inside a patient tool
/// is caught, just later.
#[tokio::test(start_paused = true)]
async fn a_patient_tool_is_still_bounded() {
    struct PatientButHung;
    #[async_trait]
    impl Tool for PatientButHung {
        fn name(&self) -> &'static str {
            "patient_hang"
        }
        fn description(&self) -> &'static str {
            "never returns, but claims patience"
        }
        fn max_duration(&self) -> Option<Duration> {
            Some(Duration::from_secs(5))
        }
        async fn call(&self, _i: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(ToolOutput::text("unreachable"))
        }
    }

    let executor = executor(
        vec![Arc::new(PatientButHung)],
        ToolExecutionConfig::default(),
    );
    let out = one(&executor, call("patient_hang", "{}"), &unledgered()).await;
    assert!(
        out.content.contains("did not report back within its 5s"),
        "the tool's own ceiling is what bounds it, got: {}",
        out.content
    );
}

#[tokio::test]
async fn round_caps_fan_out_and_notes_the_overflow() {
    // A single round requesting far more calls than the ceiling must not
    // execute (or ledger) them all — the overflow gets a note.
    let executor = executor(vec![Arc::new(EchoTool)], ToolExecutionConfig::default());
    let calls: Vec<ToolCallReq> = (0..MAX_CALLS_PER_ROUND + 5)
        .map(|i| call("echo", &format!("{i}")))
        .collect();
    let outcomes = executor.execute_round(&calls, &unledgered()).await;
    assert_eq!(outcomes.len(), calls.len());
    assert!(outcomes[0].content.starts_with("echoed:"));
    assert!(
        outcomes[MAX_CALLS_PER_ROUND - 1]
            .content
            .starts_with("echoed:")
    );
    assert!(
        outcomes[MAX_CALLS_PER_ROUND]
            .content
            .contains("too many tool calls"),
        "got: {}",
        outcomes[MAX_CALLS_PER_ROUND].content
    );
}

fn budgeted(cap: usize) -> ToolTurnContext {
    ToolTurnContext {
        session: SessionContext::detached("cli:test"),
        run: None,
        budget: TurnResultBudget::new(cap),
        spin: SpinDetector::default(),
    }
}

#[tokio::test]
async fn turn_budget_omits_results_once_the_turn_is_over_budget() {
    // BigTool returns 10 KB. With a 5 KB per-turn budget, the first call is
    // admitted (nothing consumed yet) and the second is omitted with a note —
    // so a long tool chain can't quietly overflow the context window.
    let executor = executor(vec![Arc::new(BigTool)], ToolExecutionConfig::default());
    let ctx = budgeted(5 * 1024);
    let first = one(&executor, call("big", "{}"), &ctx).await;
    assert_eq!(first.content.len(), 10_000, "first result admitted in full");
    let second = one(&executor, call("big", "{}"), &ctx).await;
    assert!(
        second.content.contains("per-turn budget"),
        "second result should be omitted once over budget, got len {}",
        second.content.len()
    );
}

#[tokio::test]
async fn unlimited_turn_budget_never_omits() {
    let executor = executor(vec![Arc::new(BigTool)], ToolExecutionConfig::default());
    let ctx = budgeted(0); // 0 = unlimited
    // Distinct arguments per call, as above: the budget is what is under
    // test here, not the spin detector.
    for i in 0..5 {
        let out = one(&executor, call("big", &format!("{{\"i\":{i}}}")), &ctx).await;
        assert_eq!(out.content.len(), 10_000);
    }
}

// ── Runtime mounting (domain::catalog) ───────────────────────────────────

/// A tool mounted after the executor was built is callable, and callable
/// through a *clone* — every executor over one catalog sees the same set,
/// which is what lets a plugin host mount into a running process.
#[tokio::test]
async fn a_tool_mounted_at_runtime_is_dispatchable_through_every_clone() {
    let executor = executor(vec![], ToolExecutionConfig::default());
    let shared = executor.clone();
    let context = unledgered();

    // Distinct arguments throughout: what is under test is the catalog,
    // and byte-identical repeats would be stopped by the spin detector
    // before they ever reached it.
    let missing = one(&executor, call("echo", "before"), &context).await;
    assert_eq!(missing.content, "error: unknown tool `echo`");

    let mounted = executor.catalog().mount(Arc::new(EchoTool));
    assert_eq!(
        shared.definitions().len(),
        1,
        "a clone shares the catalog, not a copy of it"
    );
    let out = one(&shared, call("echo", "mounted"), &context).await;
    assert_eq!(out.content, "echoed: mounted");

    // And unmounting takes it back out of both.
    drop(mounted);
    let gone = one(&shared, call("echo", "after"), &context).await;
    assert_eq!(gone.content, "error: unknown tool `echo`");
    assert!(executor.definitions().is_empty());
}

/// A pinned executor keeps dispatching against the set it pinned. This is
/// the turn-scoped guarantee the runtime relies on: the model was handed
/// those schemas, so those are the tools that must answer.
#[tokio::test]
async fn a_pinned_executor_ignores_later_mounts_and_unmounts() {
    let executor = executor(vec![Arc::new(EchoTool)], ToolExecutionConfig::default());
    let pinned = executor.pin();
    let context = unledgered();

    let removed = executor.catalog().retain(|name| name == "echo");
    assert_eq!(removed, vec!["echo"]);
    let _late = executor.catalog().mount(Arc::new(NamedTool("late")));

    // The pinned view still has echo and still does not have `late`.
    let out = one(&pinned, call("echo", "hi"), &context).await;
    assert_eq!(out.content, "echoed: hi");
    let unseen = one(&pinned, call("late", "{}"), &context).await;
    assert_eq!(unseen.content, "error: unknown tool `late`");

    // A freshly pinned executor — the next turn — sees the new set.
    let next = executor.pin();
    assert_eq!(
        next.definitions()
            .iter()
            .map(|t| t.name())
            .collect::<Vec<_>>(),
        vec!["late"]
    );
}
