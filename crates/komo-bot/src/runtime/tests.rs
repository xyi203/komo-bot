/// Append one message as an event and make it durable — what a turn does,
/// condensed for a fixture that only cares that the message is there.
async fn say(db: &Db, session_id: &str, message: Message) {
    use komo_core::domain::session_event::{
        AssistantMessageEvent, MessageSource, SurfacePlacement, UserMessageEvent,
    };
    let kind = match message.role {
        Role::Assistant => SessionEventKind::AssistantMessage(AssistantMessageEvent {
            turn_id: "t".into(),
            content: message.content,
            tool_note: message.tool_note,
            surface: SurfacePlacement::append(),
        }),
        _ => SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: "t".into(),
            content: message.content,
            source: MessageSource::User,
            surface: SurfacePlacement::append(),
        }),
    };
    SessionEventRepository::append(db, session_id, vec![kind])
        .await
        .unwrap();
    SessionEventRepository::durable_flush(db, session_id)
        .await
        .unwrap();
}
use super::*;
use komo_infra::persistence::db::Db;
use komo_tools::ask_user::AskUserTool;

use crate::interaction::{CancelState, CancelTicket};
use async_trait::async_trait;
use komo_core::domain::{
    cancel::CANCELLED_ERROR,
    llm::{LlmClient, Step, ToolCallReq, TurnDriver},
    message::Role,
    repository::SessionRepository,
    run::RunStatus,
    session::Session,
    session_event::Wakeup,
    tool::{Tool, ToolError, ToolOutput},
};
use std::collections::VecDeque;
use std::sync::Mutex;

/// An [`LlmClient`] that replays a scripted sequence of [`Step`]s and records
/// the tool results fed back to each `step()` — no rig, no network. Lets us
/// drive `run_agent_loop` deterministically and assert dispatch, threading,
/// the ledger, and the round budget.
struct ScriptedLlm {
    script: Mutex<VecDeque<Step>>,
    received: Arc<Mutex<Vec<Vec<ToolOutcome>>>>,
    /// Mid-turn user messages the loop handed to `step()`, in order.
    interjected: Arc<Mutex<Vec<String>>>,
    /// How many journal rows `resume_turn` was handed; `None` until called.
    resumed_entries: Arc<Mutex<Option<usize>>>,
    /// What the loop nudged the driver with, in order.
    nudged: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl LlmClient for ScriptedLlm {
    async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
        Ok("unused".to_string())
    }
    async fn begin_turn(
        &self,
        _session: &Session,
        _deltas: Option<Arc<dyn DeltaSink>>,
        recorder: Option<Arc<dyn TurnRecorder>>,
    ) -> anyhow::Result<Box<dyn TurnDriver>> {
        // One turn per test, so hand the whole script to the driver.
        let steps = std::mem::take(&mut *self.script.lock().unwrap());
        Ok(Box::new(ScriptedDriver {
            steps,
            received: self.received.clone(),
            interjected: self.interjected.clone(),
            nudged: self.nudged.clone(),
            recorder,
            round: 0,
        }))
    }
    async fn resume_turn(
        &self,
        session: &Session,
        events: &[SessionEvent],
        turn_id: &str,
        deltas: Option<Arc<dyn DeltaSink>>,
        recorder: Option<Arc<dyn TurnRecorder>>,
    ) -> anyhow::Result<Box<dyn TurnDriver>> {
        let of_turn = events
            .iter()
            .filter(|e| e.turn_id_of_work() == Some(turn_id))
            .count();
        self.resumed_entries.lock().unwrap().replace(of_turn);
        self.begin_turn(session, deltas, recorder).await
    }
}

struct ScriptedDriver {
    steps: VecDeque<Step>,
    received: Arc<Mutex<Vec<Vec<ToolOutcome>>>>,
    interjected: Arc<Mutex<Vec<String>>>,
    nudged: Arc<Mutex<Vec<String>>>,
    /// The real driver records one `assistant/round` per provider
    /// completion, and a fixture that skips it leaves a log claiming the
    /// turn never called a model — which is most of what these tests read
    /// the log back for.
    recorder: Option<Arc<dyn TurnRecorder>>,
    round: u32,
}

impl ScriptedDriver {
    /// Record the round this step is the completion of. The scripted usage
    /// is reported whole on the last round, the way a driver that counts
    /// once at the end would.
    async fn record_round(&mut self, step: &Step) {
        let Some(recorder) = self.recorder.clone() else {
            return;
        };
        let last = self.steps.is_empty();
        let usage = if last {
            self.usage()
        } else {
            TokenUsage::default()
        };
        let blocks = match step {
            Step::Final(text) => serde_json::json!([{ "Text": text }]),
            Step::ToolCalls { calls, .. } => serde_json::json!(
                calls
                    .iter()
                    .map(|c| serde_json::json!({
                        "ToolCall": {
                            "id": c.id,
                            "call_id": c.call_id,
                            "name": c.name,
                            "args": c.args,
                        }
                    }))
                    .collect::<Vec<_>>()
            ),
        };
        let round = self.round;
        self.round += 1;
        recorder
            .record(vec![SessionEventKind::AssistantRound(
                komo_core::domain::session_event::AssistantRoundEvent {
                    turn_id: recorder.turn_id().to_string(),
                    round,
                    response_id: format!("resp-{round}"),
                    blocks,
                    tokens_in: usage.input,
                    tokens_out: usage.output,
                    tokens_cached: usage.cached_input,
                },
            )])
            .await;
    }
}

#[async_trait]
impl TurnDriver for ScriptedDriver {
    async fn first(&mut self) -> anyhow::Result<Step> {
        let step = self.steps.pop_front().expect("script exhausted at first()");
        self.record_round(&step).await;
        Ok(step)
    }
    async fn step(
        &mut self,
        results: Vec<ToolOutcome>,
        interjected: Option<String>,
    ) -> anyhow::Result<Step> {
        if let Some(text) = interjected {
            self.interjected.lock().unwrap().push(text);
        }
        self.received.lock().unwrap().push(results);
        let step = self.steps.pop_front().expect("script exhausted at step()");
        self.record_round(&step).await;
        Ok(step)
    }
    async fn nudge(&mut self, text: String) -> anyhow::Result<Option<Step>> {
        self.nudged.lock().unwrap().push(text.clone());
        // The real driver records the nudge before the round it asks for;
        // a fixture that skipped it would leave a log these tests read back
        // for exactly that event.
        if let Some(recorder) = self.recorder.clone() {
            recorder
                .record(vec![SessionEventKind::UserMessage(UserMessageEvent {
                    turn_id: recorder.turn_id().to_string(),
                    content: text,
                    source: MessageSource::Runtime,
                    surface: SurfacePlacement::append(),
                })])
                .await;
        }
        let step = self.steps.pop_front().expect("script exhausted at nudge()");
        self.record_round(&step).await;
        Ok(Some(step))
    }
    fn usage(&self) -> TokenUsage {
        // Fixed, non-zero counts, so a test can tell "recorded" from
        // "unknown". `cached_input` is a subset of `input`, as the provider
        // layer guarantees.
        TokenUsage {
            input: 1_200,
            output: 340,
            cached_input: 900,
        }
    }
}

/// A trivial no-argument tool, for tests that only need *a* call to succeed.
/// Named `time` because that is what these scripts ask for; nothing here
/// depends on it telling the time.
struct TimeTool;
#[async_trait]
impl Tool for TimeTool {
    fn name(&self) -> &'static str {
        "time"
    }
    fn description(&self) -> &'static str {
        "reports the current moment"
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: &komo_core::domain::context::ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("2026-09-11T10:00:00Z"))
    }
}

/// A tool that echoes its raw input, for asserting result threading.
struct EchoArgsTool;
#[async_trait]
impl Tool for EchoArgsTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echoes its input args"
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: &komo_core::domain::context::ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        // Echo the payload, not its JSON encoding: the assertion is about
        // results threading back through the loop.
        let text = input
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| input.to_string());
        Ok(ToolOutput::text(format!("echo:{text}")))
    }
}

/// A tool that always errors, for asserting failures feed back (not abort).
struct FailTool;
#[async_trait]
impl Tool for FailTool {
    fn name(&self) -> &'static str {
        "fail"
    }
    fn description(&self) -> &'static str {
        "always errors"
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: &komo_core::domain::context::ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed(anyhow::anyhow!("boom")))
    }
}

/// A komo home of this test's own, wiped first.
///
/// The whole directory, not just the db file: a home now holds transcripts
/// beside `state.db`, and two tests sharing a directory would read each
/// other's conversations.
pub(crate) fn sqlite_url(name: &str) -> String {
    let home = std::env::temp_dir().join(format!("komo-test-{name}"));
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    format!("turso:{}", home.join("state.db").display())
}

/// A tool-call step with no narration — the shape most tests care about.
fn tool_calls(calls: Vec<ToolCallReq>) -> Step {
    Step::ToolCalls {
        calls,
        text: String::new(),
    }
}

fn call(name: &str, args: &str) -> ToolCallReq {
    ToolCallReq {
        id: format!("id-{name}"),
        call_id: None,
        name: name.to_string(),
        args: args.to_string(),
    }
}

/// Build a runtime whose LLM replays `script`, with `tools` registered and a
/// round budget of `max_turns`. Returns the runtime and a handle to the tool
/// results fed back to the driver, round by round.
fn scripted_runtime(
    db: Arc<Db>,
    script: Vec<Step>,
    tools: Vec<Arc<dyn Tool>>,
    max_turns: usize,
) -> (AgentRuntime, Arc<Mutex<Vec<Vec<ToolOutcome>>>>) {
    let (rt, received, _) = scripted_runtime_seeing_interjections(db, script, tools, max_turns);
    (rt, received)
}

/// [`scripted_runtime`] plus a handle on the mid-turn user messages the loop
/// fed to the driver — what an interjection test asserts on.
#[allow(clippy::type_complexity)]
fn scripted_runtime_seeing_interjections(
    db: Arc<Db>,
    script: Vec<Step>,
    tools: Vec<Arc<dyn Tool>>,
    max_turns: usize,
) -> (
    AgentRuntime,
    Arc<Mutex<Vec<Vec<ToolOutcome>>>>,
    Arc<Mutex<Vec<String>>>,
) {
    let (rt, received, interjected, _) = scripted_runtime_parts(db, script, tools, max_turns);
    (rt, received, interjected)
}

/// [`scripted_runtime`] plus a handle on what the loop *nudged* the driver
/// with — the runtime's own mid-turn message.
#[allow(clippy::type_complexity)]
fn scripted_runtime_seeing_nudges(
    db: Arc<Db>,
    script: Vec<Step>,
    tools: Vec<Arc<dyn Tool>>,
    max_turns: usize,
) -> (AgentRuntime, Arc<Mutex<Vec<String>>>) {
    let (rt, _, _, nudged) = scripted_runtime_parts(db, script, tools, max_turns);
    (rt, nudged)
}

#[allow(clippy::type_complexity)]
fn scripted_runtime_parts(
    db: Arc<Db>,
    script: Vec<Step>,
    tools: Vec<Arc<dyn Tool>>,
    max_turns: usize,
) -> (
    AgentRuntime,
    Arc<Mutex<Vec<Vec<ToolOutcome>>>>,
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<Vec<String>>>,
) {
    let nudged = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::new(Mutex::new(Vec::new()));
    let interjected = Arc::new(Mutex::new(Vec::new()));
    let mut executor =
        ToolExecutor::new(komo_services::tool_execution::ToolExecutionConfig::default());
    for t in tools {
        executor.register(t);
    }
    // Same wiring as `cli::wiring`: the executor records each call's two
    // halves in the session log. Without it a fixture turn leaves a log that
    // says the turn ran no tools, which is the one thing these tests are
    // about.
    let executor = executor.with_events(db.clone());
    let rt = AgentRuntime {
        llm: Arc::new(ScriptedLlm {
            script: Mutex::new(script.into()),
            received: received.clone(),
            interjected: interjected.clone(),
            resumed_entries: Arc::new(Mutex::new(None)),
            nudged: nudged.clone(),
        }),
        sessions: db.clone(),
        messages: db.clone(),
        events: db.clone(),
        projection: db.clone(),
        runs: db.clone(),
        tool_executor: executor,
        max_turns,
        history_window: 0,
        learning: None,
        compaction: None,
        wakeups: None,
    };
    (rt, received, interjected, nudged)
}

/// A tool that parks until released, so a turn can be cancelled *while* a
/// round is in flight rather than only between rounds.
struct BlockingTool {
    released: Arc<tokio::sync::Notify>,
    started: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl Tool for BlockingTool {
    fn name(&self) -> &'static str {
        "block"
    }
    fn description(&self) -> &'static str {
        "parks until released"
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: &komo_core::domain::context::ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        self.started.notify_waiters();
        self.released.notified().await;
        Ok(ToolOutput::text("released"))
    }
}

/// A `SessionContext` carrying a cancellation signal, plus its trigger.
///
/// The ticket is leaked into the returned pair's lifetime by holding it in
/// the closure-free way a test can: dropping it would retire the slot, and
/// then `cancel` would have nothing to flip.
fn cancellable_ctx(session: &str) -> (SessionContext, Arc<CancelState>, CancelTicket) {
    let cancels = Arc::new(CancelState::new());
    let ticket = cancels.register(session);
    let ctx = SessionContext::detached(session).with_cancel(ticket.signal());
    (ctx, cancels, ticket)
}

#[tokio::test]
async fn cancelling_mid_round_stops_the_turn_and_notes_it_in_the_transcript() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_cancel_mid.db"))
            .await
            .unwrap(),
    );
    let started = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(tokio::sync::Notify::new());
    let (rt, _) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call("block", "{}")]),
            Step::Final("never reached".into()),
        ],
        vec![Arc::new(BlockingTool {
            started: started.clone(),
            released: released.clone(),
        })],
        30,
    );

    let (ctx, cancels, _ticket) = cancellable_ctx("cancel-mid");
    let wait_started = started.notified();
    let turn = tokio::spawn(with_session(ctx, async move {
        rt.handle_input("cancel-mid", "长任务".to_string()).await
    }));

    // Cancel while the tool round is still running.
    wait_started.await;
    assert!(cancels.cancel("cancel-mid"), "signal should be registered");

    let outcome = turn.await.unwrap();
    let error = outcome.expect_err("a cancelled turn fails");
    assert!(is_cancelled(&error), "expected Cancelled, got {error:#}");
    released.notify_waiters();

    // The transcript keeps alternating, with a note that says what happened.
    let messages = MessageRepository::list_by_session(&*db, "cancel-mid")
        .await
        .unwrap();
    let last = messages.last().unwrap();
    assert_eq!(last.role, Role::Assistant);
    assert_eq!(last.content, CANCELLED_REPLY);

    // The ledger says cancelled — not a failure, and not resumable.
    let run = RunRepository::list(&*db, 10).await.unwrap().pop().unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error, CANCELLED_ERROR);
    assert!(!run.recoverable);
    assert!(run.ended_at.is_some());

    assert_ledger_matches_log(&db, "cancel-mid").await;
}

#[tokio::test]
async fn cancelling_before_the_first_round_never_calls_the_model() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_cancel_early.db"))
            .await
            .unwrap(),
    );
    // An empty script: reaching the model at all would panic ("script
    // exhausted"), so this also proves the check happens before the round.
    let (rt, _) = scripted_runtime(db.clone(), vec![], vec![], 30);

    let (ctx, cancels, _ticket) = cancellable_ctx("cancel-early");
    cancels.cancel("cancel-early");
    let error = with_session(ctx, rt.handle_input("cancel-early", "算了".to_string()))
        .await
        .expect_err("a cancelled turn fails");
    assert!(is_cancelled(&error));
}

/// An [`InterjectSource`] that hands over a fixed message once — the shape
/// of a user typing while a round runs.
struct SaysOnce(Mutex<Option<String>>);
impl komo_core::domain::gateway::InterjectSource for SaysOnce {
    fn take(&self) -> Vec<String> {
        self.0.lock().unwrap().take().into_iter().collect()
    }
}

/// What the user says mid-turn reaches the model on the very next round —
/// the whole point, since a correction is worthless once the agent has
/// finished going the wrong way — and lands in the transcript folded into
/// this turn's user message (never as a second one, which would leave two
/// consecutive user messages for the next turn to replay).
#[tokio::test]
async fn a_mid_turn_interjection_reaches_the_model_and_the_transcript() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_interject.db"))
            .await
            .unwrap(),
    );
    let (rt, _, interjected) = scripted_runtime_seeing_interjections(
        db.clone(),
        vec![
            tool_calls(vec![call("time", "{}")]),
            Step::Final("好的，改看 B".into()),
        ],
        vec![Arc::new(TimeTool)],
        30,
    );

    let ctx = SessionContext::detached("cli:interject").with_interject(Arc::new(SaysOnce(
        Mutex::new(Some("不对，是 B 不是 A".to_string())),
    )));
    let reply = with_session(ctx, rt.handle_input("cli:interject", "看下 A".to_string()))
        .await
        .unwrap();
    assert_eq!(reply, "好的，改看 B");

    // Delivered to the model on the round right after it was said.
    assert_eq!(
        interjected.lock().unwrap().clone(),
        vec!["不对，是 B 不是 A"],
        "the interjection must reach the driver mid-turn"
    );

    // One user message for the turn, carrying both halves of what was said.
    let messages = MessageRepository::list_by_session(&*db, "cli:interject")
        .await
        .unwrap();
    let roles: Vec<Role> = messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant],
        "an interjection must not become a second user message"
    );
    assert!(
        messages[0].content.contains("看下 A") && messages[0].content.contains("是 B 不是 A"),
        "both halves belong to the turn's user message, got {:?}",
        messages[0].content
    );

    assert_ledger_matches_log(&db, "cli:interject").await;
}

/// A cancel that lands before any tool ran leaves nothing behind: the
/// turn's own user message is rewound out, so the transcript reads as if it
/// never happened and later turns don't replay a "(已取消)" pair forever.
/// The ledger still records the cancelled run — that is the audit trail.
#[tokio::test]
async fn a_pristine_cancel_rewinds_its_user_message() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_cancel_pristine.db"))
            .await
            .unwrap(),
    );
    let (rt, _) = scripted_runtime(db.clone(), vec![], vec![], 30);

    let (ctx, cancels, _ticket) = cancellable_ctx("cancel-pristine");
    cancels.cancel("cancel-pristine");
    let error = with_session(ctx, rt.handle_input("cancel-pristine", "算了".to_string()))
        .await
        .expect_err("a cancelled turn fails");
    assert!(is_cancelled(&error));

    let messages = MessageRepository::list_by_session(&*db, "cancel-pristine")
        .await
        .unwrap();
    assert!(
        messages.is_empty(),
        "a pristine cancel leaves no transcript, got {} message(s)",
        messages.len()
    );

    let run = RunRepository::list(&*db, 10).await.unwrap().pop().unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error, CANCELLED_ERROR);

    // The run is still in the ledger even though the conversation reads as
    // if the turn never happened — and so it must be in the projection.
    assert_ledger_matches_log(&db, &run.session_id).await;
}

#[tokio::test]
async fn a_turn_without_a_cancel_signal_runs_normally() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_cancel_absent.db"))
            .await
            .unwrap(),
    );
    let (rt, _) = scripted_runtime(db.clone(), vec![Step::Final("done".into())], vec![], 30);
    // Sweeps, cron and aux turns carry no signal; that must stay a no-op path.
    let reply = rt
        .handle_input("no-cancel", "hi".to_string())
        .await
        .unwrap();
    assert_eq!(reply, "done");
}

#[tokio::test]
async fn cancel_state_reports_whether_a_turn_was_listening() {
    let cancels = Arc::new(CancelState::new());
    assert!(
        !cancels.cancel("nobody"),
        "no turn in flight → nothing to do"
    );

    let ticket = cancels.register("s1");
    assert!(!ticket.is_cancelled());
    assert!(cancels.cancel("s1"));
    assert!(ticket.is_cancelled());
    // Awaiting an already-cancelled signal resolves immediately.
    ticket.cancelled().await;

    drop(ticket);
    assert!(!cancels.cancel("s1"), "finished turns are unreachable");
}

/// Stop is pressed on a *conversation*, so it has to reach the turn running
/// and the one queued behind it. With a single slot per session the queued
/// caller could not register at all, and it then ran the very work the user
/// had just stopped.
#[tokio::test]
async fn a_stop_reaches_the_queued_turn_as_well_as_the_running_one() {
    let cancels = Arc::new(CancelState::new());
    let running = cancels.register("s1");
    let queued = cancels.register("s1");

    assert!(cancels.cancel("s1"));
    assert!(running.is_cancelled());
    assert!(queued.is_cancelled(), "the caller waiting for the slot too");

    // Each registration retires only its own: the running turn finishing
    // must not make the queued one unstoppable.
    let still_queued = cancels.register("s1");
    drop(running);
    assert!(cancels.cancel("s1"));
    assert!(still_queued.is_cancelled());
    drop(queued);
    drop(still_queued);
    assert!(!cancels.cancel("s1"), "and the last one out clears the map");
}

#[tokio::test]
async fn turn_with_a_tool_call_records_a_run_with_a_step() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_tool_run.db"))
            .await
            .unwrap(),
    );
    let (rt, _) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call("time", "{}")]),
            Step::Final("the time is now".into()),
        ],
        vec![Arc::new(TimeTool)],
        30,
    );

    rt.handle_input("cli:s1", "hi".into()).await.unwrap();

    let runs = RunRepository::list(&*db, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Done);
    assert_eq!(runs[0].plan, "1 tool call(s)");

    let steps = RunRepository::steps(&*db, &runs[0].id).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].tool_name, "time");
    assert!(steps[0].ok);

    assert_ledger_matches_log(&db, "cli:s1").await;
}

/// A tool that asks before it acts, and reports which answer it got.
struct Gated;

#[async_trait]
impl Tool for Gated {
    fn name(&self) -> &'static str {
        "gated"
    }
    fn description(&self) -> &'static str {
        "asks for approval, then claims to have acted"
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        ctx: &komo_core::domain::context::ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let request = komo_core::domain::approval::ApprovalRequest::normal("delete the tree");
        let decision = ctx.decide(&request).await;
        match decision.is_allowed() {
            true => Ok(ToolOutput::text("acted")),
            false => Err(ToolError::Denied(
                decision.feedback().unwrap_or("refused").to_string(),
            )),
        }
    }
}

/// An approver that answers "later" — the prompt is out, nobody has
/// replied.
struct Suspending;

#[async_trait]
impl komo_core::domain::approval::Approver for Suspending {
    async fn decide(
        &self,
        _request: &komo_core::domain::approval::ApprovalRequest,
    ) -> komo_core::domain::approval::Decision {
        komo_core::domain::approval::Decision::Suspend
    }
}

/// An approver that must never be consulted.
struct NeverAsked(Arc<Mutex<usize>>);

#[async_trait]
impl komo_core::domain::approval::Approver for NeverAsked {
    async fn decide(
        &self,
        _request: &komo_core::domain::approval::ApprovalRequest,
    ) -> komo_core::domain::approval::Decision {
        *self.0.lock().unwrap() += 1;
        komo_core::domain::approval::Decision::deny()
    }
}

/// A second gated tool. The same request under a *different* call id, which
/// is what makes it an approval nobody has answered rather than one the log
/// already settled for this turn.
struct GatedAgain;

#[async_trait]
impl Tool for GatedAgain {
    fn name(&self) -> &'static str {
        "gated2"
    }
    fn description(&self) -> &'static str {
        "asks for approval a second time"
    }
    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &komo_core::domain::context::ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        Gated.call(input, ctx).await
    }
}

pub(crate) fn gated_runtime(
    db: Arc<Db>,
    approver: Arc<dyn komo_core::domain::approval::Approver>,
) -> AgentRuntime {
    let (mut rt, _) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call("gated", "{}")]),
            Step::Final("done".into()),
        ],
        vec![],
        30,
    );
    let mut executor =
        ToolExecutor::new(komo_services::tool_execution::ToolExecutionConfig::default());
    executor.register(Arc::new(Gated));
    rt.tool_executor = executor.with_events(db.clone()).with_approver(approver);
    rt.wakeups = Some(db.clone());
    rt
}

/// The same, for a turn that meets *two* approvals in a row — what a
/// routine's work usually looks like, since one answer rarely covers a whole
/// job. Its script starts at the second call, so it is what a continuation
/// runs: the round that stopped is replayed from the log, and the driver is
/// asked for what comes after it.
pub(crate) fn twice_gated_runtime(
    db: Arc<Db>,
    approver: Arc<dyn komo_core::domain::approval::Approver>,
) -> AgentRuntime {
    let (mut rt, _) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call("gated2", "{}")]),
            Step::Final("done".into()),
        ],
        vec![],
        30,
    );
    let mut executor =
        ToolExecutor::new(komo_services::tool_execution::ToolExecutionConfig::default());
    executor.register(Arc::new(Gated));
    executor.register(Arc::new(GatedAgain));
    rt.tool_executor = executor.with_events(db.clone()).with_approver(approver);
    rt.wakeups = Some(db.clone());
    rt
}

/// A gated call whose answer has not arrived stops the turn instead of
/// holding the session slot — and leaves behind exactly what a
/// continuation needs: the request on record, the call unsettled, and a
/// standing wait.
#[tokio::test]
async fn a_turn_waiting_on_an_approval_suspends_rather_than_failing() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_suspend.db"))
            .await
            .unwrap(),
    );
    let rt = gated_runtime(db.clone(), Arc::new(Suspending));

    let outcome = rt.handle_input("cli:wait", "delete it".into()).await;
    assert!(
        komo_core::domain::wakeup::is_suspended(&outcome.unwrap_err()),
        "the turn stops as suspended, not as a failure"
    );

    // The ledger says waiting: not running (a restart must not reconcile
    // it), not finished (there is no conclusion).
    let run = RunRepository::list(&*db, 10).await.unwrap().pop().unwrap();
    assert_eq!(run.status, RunStatus::Suspended);
    assert!(!run.recoverable, "its return is scheduled, not manual");

    let events = SessionEventRepository::events(&*db, "cli:wait")
        .await
        .unwrap();
    // The wire tag, which is what the assertions below are about.
    let kinds: Vec<String> = events
        .iter()
        .map(|event| {
            serde_json::to_value(&event.kind).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(kinds.iter().any(|k| k == "approval/requested"), "{kinds:?}");
    assert!(kinds.iter().any(|k| k == "turn/suspended"), "{kinds:?}");
    assert!(
        !kinds.iter().any(|k| k == "tool/call-settled"),
        "a call that stopped to wait did not happen: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| k == "approval/resolved"),
        "and nobody has answered it: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| k == "assistant/message"),
        "a suspended turn has not answered, and the surface must still end \
             on the user message for the continuation to be one: {kinds:?}"
    );

    // And something is scheduled to come back for it.
    let waits = komo_core::domain::wakeup::WakeupRepository::list(&*db)
        .await
        .unwrap();
    assert_eq!(waits.len(), 1);
    assert_eq!(waits[0].turn_id.as_deref(), Some(run.id.as_str()));
    assert_eq!(
        waits[0].wakeup,
        komo_core::domain::session_event::Wakeup::Approval {
            call_id: "id-gated".into()
        }
    );
    assert!(
        waits[0].expires_at.is_some(),
        "a wait nobody answers has to come back and say so"
    );
}

/// The answer that arrived while the turn was suspended **is** the answer.
/// Re-dispatching the call must not ask the user to approve the same action
/// twice — and for a wait that expired, asking again would park the turn
/// forever.
#[tokio::test]
async fn a_gated_call_honours_the_answer_already_in_the_log() {
    use komo_core::domain::session_event::ApprovalResolvedEvent;

    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_answered.db"))
            .await
            .unwrap(),
    );
    let asked = Arc::new(Mutex::new(0usize));
    let rt = gated_runtime(db.clone(), Arc::new(NeverAsked(asked.clone())));

    // The turn opens, and the answer is already on record for the call it
    // is about to make.
    let turn_id = Run::new_id();
    SessionEventRepository::append(
        &*db,
        "cli:answered",
        vec![
            SessionEventKind::TurnStarted {
                turn_id: turn_id.clone(),
                resumed_from: None,
            },
            SessionEventKind::ApprovalResolved(ApprovalResolvedEvent {
                turn_id: turn_id.clone(),
                call_id: "id-gated".into(),
                call_index: 0,
                allowed: true,
                decided_by: "human".into(),
                reason: String::new(),
                waited_ms: 1_000,
            }),
        ],
    )
    .await
    .unwrap();
    SessionEventRepository::durable_flush(&*db, "cli:answered")
        .await
        .unwrap();

    let reply = rt
        .run_ledgered(
            "cli:answered",
            turn_id.clone(),
            TurnKind::Fresh {
                user_input: "delete it".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(reply, "done");
    assert_eq!(
        *asked.lock().unwrap(),
        0,
        "the approver must not be asked about an answer that already landed"
    );
    let steps = RunRepository::steps(&*db, &turn_id).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert!(steps[0].ok, "and the call ran: {:?}", steps[0].error);
}

/// The headline of the approval rework: the answer arrives after the
/// process that asked is gone, and the turn still comes back and acts.
///
/// Everything here is a fresh runtime over the same store — which is what a
/// gateway restart is.
#[tokio::test]
async fn an_approval_answered_after_a_restart_resumes_the_turn_and_runs_the_call() {
    use komo_core::domain::session_event::ApprovalResolvedEvent;

    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_answered_later.db"))
            .await
            .unwrap(),
    );

    // 1. A turn stops for an approval nobody has answered.
    let rt = gated_runtime(db.clone(), Arc::new(Suspending));
    assert!(
        rt.handle_input("cli:later", "delete it".into())
            .await
            .is_err()
    );
    drop(rt);
    let suspended = RunRepository::list(&*db, 10).await.unwrap().pop().unwrap();
    assert_eq!(suspended.status, RunStatus::Suspended);

    // 2. The answer lands — what `/approve` writes.
    SessionEventRepository::append(
        &*db,
        "cli:later",
        vec![SessionEventKind::ApprovalResolved(ApprovalResolvedEvent {
            turn_id: suspended.id.clone(),
            call_id: "id-gated".into(),
            call_index: 0,
            allowed: true,
            decided_by: "human".into(),
            reason: String::new(),
            waited_ms: 90_000,
        })],
    )
    .await
    .unwrap();
    SessionEventRepository::durable_flush(&*db, "cli:later")
        .await
        .unwrap();

    // 3. A new process picks the turn up. The approver here would deny
    //    anything it was asked — it must not be asked.
    let asked = Arc::new(Mutex::new(0usize));
    let rt = gated_runtime(db.clone(), Arc::new(NeverAsked(asked.clone())));
    let reply = rt
        .resume_interrupted(&suspended)
        .await
        .unwrap()
        .expect("a suspended turn ends on the user message, so it is continuable");

    assert_eq!(reply, "done");
    assert_eq!(
        *asked.lock().unwrap(),
        0,
        "the answer was already on record"
    );

    // The continuation is its own run, linked back, and it is the one that
    // ran the call.
    let runs = RunRepository::list(&*db, 10).await.unwrap();
    let continuation = runs
        .iter()
        .find(|r| r.resumed_from.as_deref() == Some(suspended.id.as_str()))
        .expect("the continuation links back to the turn it picked up");
    assert_eq!(continuation.status, RunStatus::Done);
    let steps = RunRepository::steps(&*db, &continuation.id).await.unwrap();
    assert_eq!(steps.len(), 1, "the gated call ran exactly once");
    assert!(steps[0].ok, "{}", steps[0].error);
    assert!(
        RunRepository::steps(&*db, &suspended.id)
            .await
            .unwrap()
            .is_empty(),
        "and the suspended attempt still has no step: it never ran the call"
    );
}

/// The waiting is visible. A suspended turn holds no slot and writes no
/// reply, so without this the conversation reads as idle in every list the
/// operator has — and the run ledger is not where anyone looks for "which
/// chat is stuck on me".
#[tokio::test]
async fn a_suspended_turn_shows_up_as_the_session_waiting() {
    use komo_core::domain::session_event::{ApprovalResolvedEvent, WakeupKind};

    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_awaiting.db"))
            .await
            .unwrap(),
    );

    let rt = gated_runtime(db.clone(), Arc::new(Suspending));
    assert!(
        rt.handle_input("cli:awaiting", "delete it".into())
            .await
            .is_err()
    );
    drop(rt);

    let waiting = SessionRepository::find(&*db, "cli:awaiting")
        .await
        .unwrap()
        .unwrap()
        .awaiting
        .expect("the session is stopped on an approval");
    assert_eq!(waiting.kind, WakeupKind::Approval);
    assert!(
        waiting.expires_at.is_some(),
        "and it says when the question runs out"
    );

    // The answer lands and the turn is picked up again: the badge comes off
    // when the work restarts, not when it next finishes.
    let suspended = RunRepository::list(&*db, 10).await.unwrap().pop().unwrap();
    SessionEventRepository::append(
        &*db,
        "cli:awaiting",
        vec![SessionEventKind::ApprovalResolved(ApprovalResolvedEvent {
            turn_id: suspended.id.clone(),
            call_id: "id-gated".into(),
            call_index: 0,
            allowed: true,
            decided_by: "human".into(),
            reason: String::new(),
            waited_ms: 1_000,
        })],
    )
    .await
    .unwrap();
    let rt = gated_runtime(db.clone(), Arc::new(NeverAsked(Arc::new(Mutex::new(0)))));
    rt.resume_interrupted(&suspended)
        .await
        .unwrap()
        .expect("a suspended turn is continuable");

    assert!(
        SessionRepository::find(&*db, "cli:awaiting")
            .await
            .unwrap()
            .unwrap()
            .awaiting
            .is_none(),
        "nothing is waiting once the continuation has the turn"
    );
}

/// Nobody answered. The turn still comes back — and is told so, rather
/// than being asked the same question again and parking itself forever.
#[tokio::test]
async fn an_approval_that_expired_comes_back_as_a_refusal() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_expired.db"))
            .await
            .unwrap(),
    );

    let rt = gated_runtime(db.clone(), Arc::new(Suspending));
    assert!(
        rt.handle_input("cli:expired", "delete it".into())
            .await
            .is_err()
    );
    drop(rt);
    let suspended = RunRepository::list(&*db, 10).await.unwrap().pop().unwrap();

    // What the wake writes when the deadline passes.
    SessionEventRepository::append(
        &*db,
        "cli:expired",
        vec![SessionEventKind::ApprovalExpired {
            turn_id: suspended.id.clone(),
            call_id: "id-gated".into(),
            call_index: 0,
        }],
    )
    .await
    .unwrap();
    SessionEventRepository::durable_flush(&*db, "cli:expired")
        .await
        .unwrap();

    let asked = Arc::new(Mutex::new(0usize));
    let rt = gated_runtime(db.clone(), Arc::new(NeverAsked(asked.clone())));
    let reply = rt
        .resume_interrupted(&suspended)
        .await
        .unwrap()
        .expect("the turn is continuable");

    assert_eq!(reply, "done");
    assert_eq!(
        *asked.lock().unwrap(),
        0,
        "an expiry is an answer; asking again would park the turn forever"
    );
    let continuation = RunRepository::list(&*db, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.resumed_from.as_deref() == Some(suspended.id.as_str()))
        .unwrap();
    let steps = RunRepository::steps(&*db, &continuation.id).await.unwrap();
    assert_eq!(steps.len(), 1);
    // A refusal is a *recoverable, terminal* outcome, so it rides back as
    // the model-facing text rather than as a tool failure — but it must say
    // the action did not happen, and why.
    assert!(
        steps[0].result.contains("expired"),
        "the model is told nobody answered: {}",
        steps[0].result
    );
    assert_ne!(steps[0].result, "acted", "and the call did not run");
}

// ── the model's own waits (docs/bot-runtime.md §5.7 / §5.8) ──────────────

/// A runtime whose turn calls one sentinel tool with `args`, then answers.
/// Rebuilt per "process" in the tests below, which is what a restart is.
fn waiting_runtime(db: Arc<Db>, tool: Arc<dyn Tool>, args: &str) -> AgentRuntime {
    let name = tool.name();
    let (mut rt, _) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call(name, args)]),
            Step::Final("done".into()),
        ],
        vec![],
        30,
    );
    let mut executor =
        ToolExecutor::new(komo_services::tool_execution::ToolExecutionConfig::default());
    executor.register(tool);
    rt.tool_executor = executor.with_events(db.clone());
    rt.wakeups = Some(db.clone());
    rt
}

/// A session context somebody is watching, so `ask_user` has an addressee.
fn watched(session_id: &str, sent: Arc<Mutex<Vec<String>>>) -> SessionContext {
    struct Recording(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl komo_core::domain::gateway::ReplySink for Recording {
        async fn send(&self, text: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    SessionContext {
        sink: Arc::new(Recording(sent)),
        interactive: true,
        ..SessionContext::detached(session_id)
    }
}

/// What the sweep does when a wait comes due, minus the session slot the
/// gateway holds: record the wake through the one writer of
/// `wakeup/fired`, then continue the turn.
struct TestWaker {
    runtime: Arc<AgentRuntime>,
    waits: crate::interaction::WaitParts,
}

#[async_trait]
impl komo_core::domain::wakeup::WakeupDispatch for TestWaker {
    async fn fire(
        &self,
        registration: &WakeupRegistration,
        cause: komo_core::domain::session_event::WakeupCause,
        payload: &str,
    ) -> anyhow::Result<()> {
        let turn_id = registration.turn_id.clone().expect("a continuation");
        crate::interaction::record_wake(&self.waits, registration, &turn_id, cause, payload)
            .await?;
        let run = self
            .waits
            .runs
            .get(&turn_id)
            .await?
            .expect("the suspended run");
        self.runtime.resume_interrupted(&run).await?;
        Ok(())
    }
}

fn wait_parts(db: &Arc<Db>) -> crate::interaction::WaitParts {
    crate::interaction::WaitParts {
        runs: db.clone(),
        events: db.clone(),
        wakeups: db.clone(),
    }
}

/// `ask_user` is the same primitive with a person on the other end: the
/// question goes out, the turn stops, and the answer — arriving in another
/// process — comes back as that call's result.
#[tokio::test]
async fn a_question_answered_after_a_restart_comes_back_as_the_answer() {
    let db = Arc::new(Db::connect(&sqlite_url("komo_rt_ask.db")).await.unwrap());
    let asked = Arc::new(Mutex::new(Vec::new()));

    let rt = waiting_runtime(
        db.clone(),
        Arc::new(AskUserTool::new()),
        r#"{"question":"红的还是蓝的?"}"#,
    );
    let outcome = with_session(
        watched("cli:ask", asked.clone()),
        rt.handle_input("cli:ask", "买一个".into()),
    )
    .await;
    assert!(komo_core::domain::wakeup::is_suspended(
        &outcome.unwrap_err()
    ));
    assert!(asked.lock().unwrap()[0].contains("红的还是蓝的"));
    drop(rt);

    let suspended = RunRepository::list(&*db, 10).await.unwrap().pop().unwrap();
    assert_eq!(suspended.status, RunStatus::Suspended);
    let waits = komo_core::domain::wakeup::WakeupRepository::list(&*db)
        .await
        .unwrap();
    assert_eq!(waits.len(), 1);
    assert_eq!(waits[0].wakeup, Wakeup::UserReply);
    assert!(
        waits[0].expires_at.is_some(),
        "a question nobody answers has to come back and say so"
    );

    // The user answers — in a process that never asked.
    let parts = wait_parts(&db);
    crate::interaction::record_wake(
        &parts,
        &waits[0],
        &suspended.id,
        komo_core::domain::session_event::WakeupCause::Reply,
        "蓝的",
    )
    .await
    .unwrap();
    let rt = waiting_runtime(
        db.clone(),
        Arc::new(AskUserTool::new()),
        r#"{"question":"红的还是蓝的?"}"#,
    );
    let reply = with_session(
        watched("cli:ask", asked.clone()),
        rt.resume_interrupted(&suspended),
    )
    .await
    .unwrap()
    .expect("a suspended turn ends on the user message, so it is continuable");
    assert_eq!(reply, "done");

    let continuation = RunRepository::list(&*db, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.resumed_from.as_deref() == Some(suspended.id.as_str()))
        .unwrap();
    let steps = RunRepository::steps(&*db, &continuation.id).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].result, "User answered: 蓝的");
    assert_eq!(
        asked.lock().unwrap().len(),
        1,
        "and the question was not asked a second time"
    );
}

/// Seven days of silence. The turn still comes back, and is told nobody
/// answered — a question that simply vanished would leave the model
/// waiting on an answer that is never coming.
#[tokio::test]
async fn a_question_nobody_answered_comes_back_saying_so() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_ask_expired.db"))
            .await
            .unwrap(),
    );
    let asked = Arc::new(Mutex::new(Vec::new()));
    let rt = waiting_runtime(
        db.clone(),
        Arc::new(AskUserTool::new()),
        r#"{"question":"哪一个?"}"#,
    );
    assert!(
        with_session(
            watched("cli:silent", asked.clone()),
            rt.handle_input("cli:silent", "帮我改一下".into()),
        )
        .await
        .is_err()
    );
    drop(rt);
    let suspended = RunRepository::list(&*db, 10).await.unwrap().pop().unwrap();

    // The deadline passes: the sweep fires it as expired rather than
    // dropping the row.
    let rt = Arc::new(waiting_runtime(
        db.clone(),
        Arc::new(AskUserTool::new()),
        r#"{"question":"哪一个?"}"#,
    ));
    let sweep = crate::daemon::RoutineEventSource {
        jobs: db.clone(),
        notifier: Arc::new(SilentNotifier),
        wakeups: Some(crate::daemon::WakeupWiring {
            registrations: db.clone(),
            events: db.clone(),
            dispatch: Arc::new(TestWaker {
                runtime: rt.clone(),
                waits: wait_parts(&db),
            }),
        }),
        runtime: None,
    };
    let wiring = sweep.wakeups.as_ref().unwrap();
    assert_eq!(sweep.fire_due_wakeups(wiring, now() + 8 * 86_400).await, 1);

    let events = SessionEventRepository::events(&*db, "cli:silent")
        .await
        .unwrap();
    assert!(
        events.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::WakeupFired(fired)
                if fired.cause == komo_core::domain::session_event::WakeupCause::Expired
        )),
        "the expiry is on record, not silent"
    );
    let continuation = RunRepository::list(&*db, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.resumed_from.as_deref() == Some(suspended.id.as_str()))
        .unwrap();
    let steps = RunRepository::steps(&*db, &continuation.id).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert!(
        steps[0].result.starts_with("No answer from the user"),
        "the model is told to proceed on an assumption: {}",
        steps[0].result
    );
}

struct SilentNotifier;

#[async_trait]
impl crate::notify::Notifier for SilentNotifier {
    async fn notify(&self, _title: &str, _body: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The turn has to be in the ledger *before* it ends, or a crash leaves
/// nothing for `run list` to show and nothing for `run resume` to pick up.
/// The rows are a projection now, and this is the one commit that happens
/// while the turn is still running.
#[tokio::test]
async fn a_turn_is_in_the_ledger_while_it_is_still_running() {
    /// Reads the ledger from inside the turn that is writing it.
    struct Peek(Arc<Db>, Arc<Mutex<Vec<Run>>>);
    #[async_trait]
    impl Tool for Peek {
        fn name(&self) -> &'static str {
            "peek"
        }
        fn description(&self) -> &'static str {
            "reads the run ledger mid-turn"
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &komo_core::domain::context::ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            let runs = RunRepository::list(&*self.0, 10).await.unwrap();
            *self.1.lock().unwrap() = runs;
            Ok(ToolOutput::text("peeked"))
        }
    }

    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_open_row.db"))
            .await
            .unwrap(),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (rt, _) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call("peek", "{}")]),
            Step::Final("done".into()),
        ],
        vec![Arc::new(Peek(db.clone(), seen.clone()))],
        30,
    );

    rt.handle_input("cli:open", "go".into()).await.unwrap();

    let mid_turn = seen.lock().unwrap().clone();
    assert_eq!(mid_turn.len(), 1, "the open turn is already a row");
    assert_eq!(mid_turn[0].input, "go");
    assert_eq!(mid_turn[0].status, RunStatus::Running);
    assert!(
        mid_turn[0].recoverable,
        "an unterminated turn is what a crash leaves behind, and it is resumable"
    );
    // And the finished turn overwrites it rather than adding a second row.
    let runs = RunRepository::list(&*db, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Done);
    assert!(!runs[0].recoverable);
    assert_ledger_matches_log(&db, "cli:open").await;
}

/// Assert the run ledger rows for `session_id` are exactly what folding its
/// event log produces.
///
/// The claim the projection rests on: the rows are a query index, so if the
/// fold disagrees with the writer on a real turn, dropping the authoritative
/// write loses whatever the two disagree about. Called from the tests that
/// produce each turn shape rather than from one fixture of its own — a
/// cancel, a failure and a tool round exercise different writer paths.
async fn assert_ledger_matches_log(db: &Db, session_id: &str) {
    use komo_core::domain::run_projection::project_runs;

    let events = SessionEventRepository::events(db, session_id)
        .await
        .unwrap();
    let projected = project_runs(session_id, &events);
    let written: Vec<_> = RunRepository::list(db, 50)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.session_id == session_id)
        .rev()
        .collect();

    assert_eq!(projected.len(), written.len(), "run count");
    for folded in projected.iter() {
        // Paired by id, not by position: turns inside one second tie on
        // `started_at`, and the row order within a tie is the query's.
        let row = written
            .iter()
            .find(|row| row.id == folded.run.id)
            .unwrap_or_else(|| panic!("no row for folded run {}", folded.run.id));
        let run = &folded.run;
        assert_eq!(run.session_id, row.session_id);
        assert_eq!(run.input, row.input, "input of {}", row.id);
        assert_eq!(run.plan, row.plan, "plan of {}", row.id);
        assert_eq!(run.status, row.status, "status of {}", row.id);
        assert_eq!(run.final_output, row.final_output, "reply of {}", row.id);
        assert_eq!(run.error, row.error, "error of {}", row.id);
        assert_eq!(
            run.recoverable, row.recoverable,
            "recoverable of {}",
            row.id
        );
        assert_eq!(run.tokens_in, row.tokens_in, "tokens of {}", row.id);
        assert_eq!(run.tokens_out, row.tokens_out);
        assert_eq!(run.tokens_cached, row.tokens_cached);
        assert_eq!(run.resumed_from, row.resumed_from);
        assert_eq!(run.memories, row.memories);
        assert_eq!(run.learned, row.learned, "watermark of {}", row.id);
        // Exactly the same stamps, not merely close ones. They used to
        // differ by the append between them, because the row took `now()`
        // while the fold took the bracketing events' own timestamps — that
        // divergence *was* the double write, and it went away with it.
        assert_eq!(run.started_at, row.started_at, "started_at of {}", row.id);
        assert_eq!(run.ended_at, row.ended_at, "ended_at of {}", row.id);

        // The rows are exactly the calls that *settled*. A call the turn
        // died inside has no row at all — the step is written at settle —
        // which is the fact the log keeps and the ledger cannot.
        let rows = RunRepository::steps(db, &row.id).await.unwrap();
        let settled: Vec<_> = folded.steps.iter().filter(|s| s.settled).collect();
        assert_eq!(
            settled.len(),
            rows.len(),
            "settled step count of {}",
            row.id
        );
        for (folded, row) in settled.into_iter().map(|s| &s.step).zip(&rows) {
            assert_eq!(folded.run_id, row.run_id);
            assert_eq!(folded.seq, row.seq);
            assert_eq!(folded.tool_name, row.tool_name);
            assert_eq!(folded.args, row.args);
            assert_eq!(folded.result, row.result);
            assert_eq!(folded.error, row.error);
            assert_eq!(folded.ok, row.ok);
            assert_eq!(folded.uncertain, row.uncertain);
            assert_eq!(folded.elapsed_ms, row.elapsed_ms);
            assert_eq!(folded.structured, row.structured);
            assert_eq!(folded.output_paths, row.output_paths);
        }
    }
}

/// Wraps the real store, reports every turn as a roll, and records the
/// retention floor the runtime computed instead of cutting.
/// Records which shape of read the runtime asked the log for.
struct ReadSpy {
    inner: Arc<Db>,
    reads: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl SessionEventRepository for ReadSpy {
    async fn session_ids(&self) -> anyhow::Result<Vec<String>> {
        self.inner.session_ids().await
    }

    async fn surface(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<komo_core::domain::session_event::SurfaceProjection>> {
        self.inner.surface(session_id).await
    }

    async fn append(
        &self,
        session_id: &str,
        kinds: Vec<SessionEventKind>,
    ) -> anyhow::Result<Vec<SessionEvent>> {
        self.inner.append(session_id, kinds).await
    }
    async fn durable_flush(&self, session_id: &str) -> anyhow::Result<()> {
        self.inner.durable_flush(session_id).await
    }
    async fn events(&self, session_id: &str) -> anyhow::Result<Vec<SessionEvent>> {
        self.reads.lock().unwrap().push("whole log".to_string());
        self.inner.events(session_id).await
    }
    async fn events_from(&self, session_id: &str, seq: u64) -> anyhow::Result<Vec<SessionEvent>> {
        self.reads.lock().unwrap().push(format!("from {seq}"));
        self.inner.events_from(session_id, seq).await
    }
    async fn turn_boundary(&self, session_id: &str) -> anyhow::Result<bool> {
        self.inner.turn_boundary(session_id).await
    }
    async fn retain(&self, session_id: &str, keep_from: u64) -> anyhow::Result<Option<u64>> {
        self.inner.retain(session_id, keep_from).await
    }
}

/// A session that has been talking for a while must not re-fold itself
/// every turn. A turn settles from where it opened; the whole log is read
/// only when a segment rolls, which is once per segment's worth of writing.
#[tokio::test]
async fn a_turn_settles_from_its_own_start_not_the_whole_log() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_tail_settle.db"))
            .await
            .unwrap(),
    );
    let reads = Arc::new(Mutex::new(Vec::new()));
    // The scripted driver is handed its whole script on the first turn, so
    // each turn gets its own runtime over the same store.
    for text in ["one", "two", "three"] {
        let (mut rt, _) = scripted_runtime(db.clone(), vec![Step::Final(text.into())], vec![], 30);
        rt.events = Arc::new(ReadSpy {
            inner: db.clone(),
            reads: reads.clone(),
        });
        rt.handle_input("cli:tail", text.into()).await.unwrap();
    }

    let reads = reads.lock().unwrap().clone();
    assert!(
        !reads.iter().any(|read| read == "whole log"),
        "no roll happened, so nothing had to read the whole log: {reads:?}"
    );
    // Five events a turn here: `turn/started`, the user message, the one
    // assistant round, the assistant message, `turn/completed`.
    assert_eq!(
        reads,
        vec!["from 0", "from 5", "from 10"],
        "each settle starts where its own turn did"
    );
    assert_eq!(RunRepository::list(&*db, 10).await.unwrap().len(), 3);
    assert_ledger_matches_log(&db, "cli:tail").await;
}

struct RetentionSpy {
    inner: Arc<Db>,
    floors: Arc<Mutex<Vec<u64>>>,
}

#[async_trait]
impl SessionEventRepository for RetentionSpy {
    async fn session_ids(&self) -> anyhow::Result<Vec<String>> {
        self.inner.session_ids().await
    }

    async fn surface(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<komo_core::domain::session_event::SurfaceProjection>> {
        self.inner.surface(session_id).await
    }

    async fn append(
        &self,
        session_id: &str,
        kinds: Vec<SessionEventKind>,
    ) -> anyhow::Result<Vec<SessionEvent>> {
        self.inner.append(session_id, kinds).await
    }
    async fn durable_flush(&self, session_id: &str) -> anyhow::Result<()> {
        self.inner.durable_flush(session_id).await
    }
    async fn events(&self, session_id: &str) -> anyhow::Result<Vec<SessionEvent>> {
        self.inner.events(session_id).await
    }
    async fn events_from(&self, session_id: &str, seq: u64) -> anyhow::Result<Vec<SessionEvent>> {
        self.inner.events_from(session_id, seq).await
    }
    async fn turn_boundary(&self, _session_id: &str) -> anyhow::Result<bool> {
        Ok(true)
    }
    async fn retain(&self, _session_id: &str, keep_from: u64) -> anyhow::Result<Option<u64>> {
        self.floors.lock().unwrap().push(keep_from);
        Ok(None)
    }
}

/// A conversation longer than its window keeps what fell out of it, as a
/// summary standing where those messages did — and the log still holds
/// them, which is what a human transcript reads.
#[tokio::test]
async fn a_conversation_past_its_window_is_compacted_into_a_summary() {
    struct FixedAux(&'static str);
    #[async_trait]
    impl LlmClient for FixedAux {
        async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
    }

    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_compaction.db"))
            .await
            .unwrap(),
    );
    const SUMMARY: &str = "earlier: five questions about the log";
    // Six: small enough that five exchanges outgrow it, big enough to hold
    // the summary plus what stays verbatim.
    const WINDOW: usize = 6;
    let compactor = Arc::new(crate::compaction::Compactor::new(
        Arc::new(FixedAux(SUMMARY)),
        db.clone(),
        WINDOW,
    ));
    // The scripted driver takes its whole script on the first turn, so each
    // turn gets its own runtime over the same store.
    for i in 0..5 {
        let (mut rt, _) = scripted_runtime(
            db.clone(),
            vec![Step::Final(format!("answer {i}"))],
            vec![],
            30,
        );
        rt.compaction = Some(compactor.clone());
        rt.handle_input("cli:long", format!("question {i}"))
            .await
            .unwrap();
    }

    // What the *model* replays: the window, not the whole surface. The
    // summary has to be inside it, or compaction bought nothing.
    let session = SessionRepository::find_windowed(&*db, "cli:long", WINDOW)
        .await
        .unwrap()
        .unwrap();
    let history: Vec<(Role, String)> = session
        .messages
        .iter()
        .map(|m| (m.role.clone(), m.content.clone()))
        .collect();
    assert_eq!(
        history[0],
        (Role::User, SUMMARY.to_string()),
        "the summary stands where the messages it covers did"
    );
    assert!(
        !history.iter().any(|(_, text)| text == "question 0"),
        "and the model no longer replays them"
    );
    assert!(
        history.iter().any(|(_, text)| text == "answer 4"),
        "while the newest exchanges stay verbatim"
    );
    // The invariant a replacement is easiest to break: a summary is a user
    // message, so what follows it has to be the assistant's side.
    for pair in history.windows(2) {
        assert_ne!(
            (&pair[0].0, &pair[1].0),
            (&Role::User, &Role::User),
            "two user messages in a row: {history:?}"
        );
    }

    // Nothing was rewritten: what the summary covers is still in the log.
    let events = SessionEventRepository::events(&*db, "cli:long")
        .await
        .unwrap();
    assert!(
        events.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage(m) if m.content == "question 0"
        )),
        "a human transcript still shows what the summary replaced"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, SessionEventKind::CompactionCompleted { .. }))
            .count(),
        1,
        "one compaction, on the turn that pushed the surface past the window"
    );
}

#[tokio::test]
async fn retention_will_not_cut_into_a_turn_nobody_has_finished_with() {
    // Space never outranks a turn that is still resumable or still
    // unlearned. The floor is the oldest such turn's own start, so a session
    // that has never been learned from cannot be cut at all — and once the
    // sweep retires that turn, the floor moves up to the next one.
    let db = Arc::new(Db::connect(&sqlite_url("komo_rt_retain")).await.unwrap());
    let floors = Arc::new(Mutex::new(Vec::new()));
    // The scripted driver is handed the whole script on its first turn, so
    // each turn here gets its own runtime over the same store.
    let spy = |floors: &Arc<Mutex<Vec<u64>>>| RetentionSpy {
        inner: db.clone(),
        floors: floors.clone(),
    };
    let (mut rt, _) = scripted_runtime(db.clone(), vec![Step::Final("one".into())], vec![], 30);
    rt.events = Arc::new(spy(&floors));
    rt.handle_input("cli:s-retain", "hi".into()).await.unwrap();
    assert_eq!(
        floors.lock().unwrap().as_slice(),
        &[0],
        "the turn that just ran is unlearned, so nothing below its start may go"
    );

    // The sweep retires it; now only the next turn holds the floor.
    let first = RunRepository::list(&*db, 10).await.unwrap().pop().unwrap();
    RunRepository::mark_learned(&*db, &[first.id.clone()])
        .await
        .unwrap();
    floors.lock().unwrap().clear();
    let (mut rt, _) = scripted_runtime(db.clone(), vec![Step::Final("two".into())], vec![], 30);
    rt.events = Arc::new(spy(&floors));
    rt.handle_input("cli:s-retain", "again".into())
        .await
        .unwrap();
    assert!(
        floors.lock().unwrap()[0] > 0,
        "a learned turn no longer pins the floor at the start of the log, got {:?}",
        floors.lock().unwrap()
    );
}

#[tokio::test]
async fn a_turn_that_grew_the_log_past_a_segment_seals_it_on_its_way_out() {
    // Segments are retention's unit of deletion, so one may only be cut
    // where a turn ended. Nothing sealed them at all until this seam
    // existed, which left every session as one file that grows forever and
    // gave retention no candidate to ever consider.
    let home = std::env::temp_dir().join("komo-test-komo_rt_seal");
    let db = Arc::new(Db::connect(&sqlite_url("komo_rt_seal")).await.unwrap());
    let (rt, _) = scripted_runtime(db.clone(), vec![Step::Final("ok".into())], vec![], 30);

    // One turn whose own user message is bigger than a segment.
    let big = "x".repeat(1024 * 1024 + 1024);
    rt.handle_input("cli:s-seal", big).await.unwrap();

    // The directory name is an encoding of the session id, so the segment
    // is found by walking rather than by rebuilding that encoding here.
    let sessions = std::fs::read_dir(home.join("sessions"))
        .expect("the session log directory")
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    assert_eq!(sessions.len(), 1);
    let segments = std::fs::read_dir(sessions[0].path())
        .expect("segments")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".jsonl"))
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        segments.contains("000001.jsonl"),
        "the turn boundary should have opened a second segment, found {segments:?}"
    );
    // And the log still reads as one conversation across the two files.
    let session = SessionRepository::find(&*db, "cli:s-seal")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[1].content, "ok");
}

#[tokio::test]
async fn turn_without_tools_records_a_run_without_steps() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_direct_run.db"))
            .await
            .unwrap(),
    );
    let (rt, _) = scripted_runtime(
        db.clone(),
        vec![Step::Final("hello there".into())],
        vec![],
        30,
    );

    let reply = rt.handle_input("cli:s2", "hi".into()).await.unwrap();
    assert_eq!(reply, "hello there");

    let runs = RunRepository::list(&*db, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Done);
    assert_eq!(runs[0].plan, "respond");
    assert_eq!(runs[0].final_output, "hello there");

    let steps = RunRepository::steps(&*db, &runs[0].id).await.unwrap();
    assert!(steps.is_empty());

    assert_ledger_matches_log(&db, "cli:s2").await;
}

#[test]
fn a_completion_claim_is_told_from_an_offer_or_an_observation() {
    // The incident: the model reported a device change it never made.
    assert!(claims_completed_action("热水器已打开（switch.xxx → on）✅"));
    assert!(claims_completed_action("都开好了：✅"));
    assert!(claims_completed_action("The heater has been turned on"));
    assert!(claims_completed_action("I've set the temperature to 45"));

    // Talking *about* an action is not claiming one.
    assert!(!claims_completed_action("现在热水器是关的"));
    assert!(!claims_completed_action("要不要我帮你打开？"));
    assert!(!claims_completed_action("I can turn it on if you want"));
    assert!(!claims_completed_action("好的，我明白了"));
    assert!(!claims_completed_action(
        "komo 的工具循环每轮只发一次补全请求。"
    ));
}

#[tokio::test]
async fn a_reply_claiming_an_action_with_no_tool_call_is_nudged_once() {
    let db = Arc::new(Db::connect(&sqlite_url("komo_rt_nudge.db")).await.unwrap());
    let (rt, nudged) = scripted_runtime_seeing_nudges(
        db.clone(),
        vec![
            Step::Final("热水器已打开 ✅".into()),
            Step::Final("我没有执行任何操作，需要我现在打开吗？".into()),
        ],
        vec![Arc::new(EchoArgsTool)],
        30,
    );

    let reply = rt
        .handle_input("cli:nudge1", "打开热水器".into())
        .await
        .unwrap();
    assert_eq!(reply, "我没有执行任何操作，需要我现在打开吗？");
    assert_eq!(nudged.lock().unwrap().len(), 1);

    // Recorded, so a resume rebuilds the history the live turn had — and
    // recorded as the runtime, not as the user.
    let events = SessionEventRepository::events(&*db, "cli:nudge1")
        .await
        .unwrap();
    let sources: Vec<MessageSource> = events
        .iter()
        .filter_map(|e| match &e.kind {
            SessionEventKind::UserMessage(m) => Some(m.source),
            _ => None,
        })
        .collect();
    assert_eq!(
        sources,
        vec![MessageSource::User, MessageSource::Runtime],
        "the question and the nudge, and nothing else said"
    );
}

#[tokio::test]
async fn an_ordinary_reply_is_left_alone() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_no_nudge.db"))
            .await
            .unwrap(),
    );
    let (rt, nudged) = scripted_runtime_seeing_nudges(
        db.clone(),
        vec![Step::Final("好的，有什么可以帮你？".into())],
        vec![Arc::new(EchoArgsTool)],
        30,
    );

    let reply = rt.handle_input("cli:nudge2", "在吗".into()).await.unwrap();
    assert_eq!(reply, "好的，有什么可以帮你？");
    assert!(nudged.lock().unwrap().is_empty());
}

/// The claim is only suspect when nothing was called: a turn that ran a tool
/// and then reports what it did is doing exactly the right thing.
#[tokio::test]
async fn a_claim_after_a_tool_call_is_not_nudged() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_nudge_after_tool.db"))
            .await
            .unwrap(),
    );
    let (rt, nudged) = scripted_runtime_seeing_nudges(
        db.clone(),
        vec![
            tool_calls(vec![call("echo", "on")]),
            Step::Final("已打开".into()),
        ],
        vec![Arc::new(EchoArgsTool)],
        30,
    );

    let reply = rt
        .handle_input("cli:nudge3", "打开热水器".into())
        .await
        .unwrap();
    assert_eq!(reply, "已打开");
    assert!(nudged.lock().unwrap().is_empty());
}

/// One nudge, then the model's answer stands whatever it says. A model that
/// repeats the claim after being told is not going to be talked out of it,
/// and a second nudge would be a loop.
#[tokio::test]
async fn a_model_that_keeps_claiming_is_nudged_only_once() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_nudge_twice.db"))
            .await
            .unwrap(),
    );
    let (rt, nudged) = scripted_runtime_seeing_nudges(
        db.clone(),
        vec![Step::Final("已打开".into()), Step::Final("已打开".into())],
        vec![Arc::new(EchoArgsTool)],
        30,
    );

    let reply = rt
        .handle_input("cli:nudge4", "打开热水器".into())
        .await
        .unwrap();
    assert_eq!(reply, "已打开");
    assert_eq!(nudged.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn multi_round_threads_tool_results_back() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_threading.db"))
            .await
            .unwrap(),
    );
    let (rt, received) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call("echo", "A")]),
            tool_calls(vec![call("echo", "B")]),
            Step::Final("done".into()),
        ],
        vec![Arc::new(EchoArgsTool)],
        30,
    );

    let reply = rt.handle_input("cli:s3", "hi".into()).await.unwrap();
    assert_eq!(reply, "done");

    let rec = received.lock().unwrap();
    assert_eq!(rec.len(), 2, "two tool rounds before the final answer");
    assert_eq!(rec[0][0].content, "echo:A");
    assert_eq!(rec[0][0].id, "id-echo");
    assert_eq!(rec[1][0].content, "echo:B");
}

#[tokio::test]
async fn tool_error_feeds_back_without_aborting() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_toolerr.db"))
            .await
            .unwrap(),
    );
    let (rt, received) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call("fail", "{}")]),
            Step::Final("recovered".into()),
        ],
        vec![Arc::new(FailTool)],
        30,
    );

    let reply = rt.handle_input("cli:s4", "hi".into()).await.unwrap();
    assert_eq!(reply, "recovered");
    assert!(received.lock().unwrap()[0][0].content.contains("failed"));

    let runs = RunRepository::list(&*db, 10).await.unwrap();
    assert_eq!(runs[0].status, RunStatus::Done);
}

#[tokio::test]
async fn unknown_tool_feeds_back_without_aborting() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_unknown.db"))
            .await
            .unwrap(),
    );
    let (rt, received) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call("nope", "{}")]),
            Step::Final("ok".into()),
        ],
        vec![],
        30,
    );

    let reply = rt.handle_input("cli:s5", "hi".into()).await.unwrap();
    assert_eq!(reply, "ok");
    assert!(
        received.lock().unwrap()[0][0]
            .content
            .contains("unknown tool")
    );
}

/// An LLM whose turn always fails — stands in for a dead provider / a
/// completion timeout.
struct FailingLlm;
#[async_trait]
impl LlmClient for FailingLlm {
    async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
        anyhow::bail!("provider down")
    }
    async fn begin_turn(
        &self,
        _session: &Session,
        _deltas: Option<Arc<dyn DeltaSink>>,
        _recorder: Option<Arc<dyn TurnRecorder>>,
    ) -> anyhow::Result<Box<dyn TurnDriver>> {
        anyhow::bail!("provider down")
    }
}

#[tokio::test]
async fn failed_turn_persists_an_assistant_placeholder_for_alternation() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_failed_turn.db"))
            .await
            .unwrap(),
    );
    let rt = AgentRuntime {
        llm: Arc::new(FailingLlm),
        sessions: db.clone(),
        messages: db.clone(),
        events: db.clone(),
        projection: db.clone(),
        runs: db.clone(),
        tool_executor: ToolExecutor::new(
            komo_services::tool_execution::ToolExecutionConfig::default(),
        ),
        max_turns: 30,
        history_window: 0,
        learning: None,
        compaction: None,
        wakeups: None,
    };

    let result = rt.handle_input("cli:sf", "hi".into()).await;
    assert!(result.is_err(), "the turn must surface the failure");

    // The transcript must still alternate user → assistant, so the next
    // turn's history doesn't hold two consecutive user messages.
    let session = SessionRepository::find(&*db, "cli:sf")
        .await
        .unwrap()
        .unwrap();
    let roles: Vec<Role> = session.messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant]);
    assert!(session.messages[1].content.contains("处理失败"));

    // The run is recorded as failed.
    let runs = RunRepository::list(&*db, 10).await.unwrap();
    assert_eq!(runs[0].status, RunStatus::Failed);

    assert_ledger_matches_log(&db, "cli:sf").await;
}

#[tokio::test]
async fn empty_final_answer_gets_a_fallback() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_empty_final.db"))
            .await
            .unwrap(),
    );
    let (rt, _) = scripted_runtime(db.clone(), vec![Step::Final("   ".into())], vec![], 30);
    let reply = rt.handle_input("cli:se", "hi".into()).await.unwrap();
    assert_eq!(reply, EMPTY_REPLY_FALLBACK);
}

/// #5: what the turn cost is recorded on the run, so `komo run list` can price
/// a conversation — and how much of the prompt the provider's cache served,
/// which is the only way to tell a prompt change that broke prefix
/// stability from one that didn't. 0 stays reserved for "the provider told
/// us nothing".
#[tokio::test]
async fn a_finished_turn_records_its_token_usage_and_cache_hits() {
    let db = Arc::new(Db::connect(&sqlite_url("komo_rt_tokens.db")).await.unwrap());
    let (rt, _) = scripted_runtime(db.clone(), vec![Step::Final("hi".into())], vec![], 30);
    rt.handle_input("cli:tok", "hello".into()).await.unwrap();

    let runs = RunRepository::list(&*db, 10).await.unwrap();
    assert_eq!(runs[0].tokens_in, 1_200);
    assert_eq!(runs[0].tokens_out, 340);
    assert_eq!(runs[0].tokens_cached, 900);
}

/// A turn dispatches against the catalog as it stood when the turn began.
///
/// The model was handed one set of schemas; if a plugin unmounts a tool
/// mid-turn, the call the model was invited to make must still run rather
/// than come back "unknown tool" a round later. The mutation is not lost —
/// it lands in the catalog and the next turn sees it.
#[tokio::test]
async fn a_turn_keeps_dispatching_against_the_catalog_it_started_with() {
    use komo_core::domain::catalog::Registration;

    /// Unmounts itself the first time it is called — the sharpest version
    /// of "the catalog changed mid-turn", since the change happens inside
    /// the very round that is running.
    struct SelfUnmounting(Mutex<Option<Registration>>);
    #[async_trait]
    impl Tool for SelfUnmounting {
        fn name(&self) -> &'static str {
            "vanishing"
        }
        fn description(&self) -> &'static str {
            "unmounts itself when called"
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &komo_core::domain::context::ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            // Dropping the registration takes it out of the catalog.
            drop(self.0.lock().unwrap().take());
            Ok(ToolOutput::text("still here"))
        }
    }

    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_catalog_pin.db"))
            .await
            .unwrap(),
    );
    let (rt, received) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call("vanishing", "{}")]),
            tool_calls(vec![call("vanishing", "{}")]),
            Step::Final("done".into()),
        ],
        vec![],
        30,
    );

    let catalog = rt.tool_executor.catalog().clone();
    let tool = Arc::new(SelfUnmounting(Mutex::new(None)));
    let registration = catalog.mount(tool.clone());
    *tool.0.lock().unwrap() = Some(registration);
    assert_eq!(catalog.snapshot().len(), 1);

    let reply = rt.handle_input("cli:pin", "go".into()).await.unwrap();
    assert_eq!(reply, "done");

    // Both rounds reached the tool, including the one that ran after it had
    // already removed itself.
    let rounds = received.lock().unwrap();
    assert_eq!(rounds[0][0].content, "still here");
    assert_eq!(
        rounds[1][0].content, "still here",
        "the turn's view is pinned; the unmount takes effect next turn"
    );

    // And the unmount really happened — the next turn would not see it.
    assert!(catalog.snapshot().is_empty(), "the catalog itself moved on");
}

/// #3: the turn's tool activity is folded onto the assistant message, so the
/// next turn knows tools ran — while the user-visible reply stays the reply.
#[tokio::test]
async fn a_tool_turn_leaves_a_note_for_the_next_turn() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_tool_note.db"))
            .await
            .unwrap(),
    );
    let (rt, _) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call("echo", "hello")]),
            Step::Final("it said hello".into()),
        ],
        vec![Arc::new(EchoArgsTool)],
        30,
    );
    rt.handle_input("cli:note", "echo something".into())
        .await
        .unwrap();

    let messages = MessageRepository::list_by_session(&*db, "cli:note")
        .await
        .unwrap();
    let assistant = messages.last().unwrap();
    assert_eq!(assistant.content, "it said hello", "reply stays clean");
    assert!(
        assistant.tool_note.contains("echo"),
        "the note should name the tool: {:?}",
        assistant.tool_note
    );
}

#[tokio::test]
async fn a_tool_less_turn_leaves_no_note() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_no_note.db"))
            .await
            .unwrap(),
    );
    let (rt, _) = scripted_runtime(
        db.clone(),
        vec![Step::Final("just talk".into())],
        vec![],
        30,
    );
    rt.handle_input("cli:nonote", "hi".into()).await.unwrap();

    let messages = MessageRepository::list_by_session(&*db, "cli:nonote")
        .await
        .unwrap();
    assert!(messages.last().unwrap().tool_note.is_empty());
}

/// #6: text the model wrote alongside its tool calls reaches a watching
/// client. Nothing else in komo surfaces the model's mid-turn reasoning.
#[tokio::test]
async fn narration_alongside_tool_calls_reaches_the_event_sink() {
    use komo_core::domain::events::ToolEventSink;

    #[derive(Default)]
    struct Captured(Mutex<Vec<TurnEvent>>);
    impl ToolEventSink for Captured {
        fn emit(&self, event: TurnEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_narration.db"))
            .await
            .unwrap(),
    );
    let (rt, _) = scripted_runtime(
        db.clone(),
        vec![
            Step::ToolCalls {
                calls: vec![call("time", "{}")],
                text: "Checking the clock first.".into(),
            },
            Step::Final("it is late".into()),
        ],
        vec![Arc::new(TimeTool)],
        30,
    );

    let sink = Arc::new(Captured::default());
    let ctx = SessionContext::detached("cli:narr").with_event_sink(sink.clone());
    let reply = with_session(ctx, rt.handle_input("cli:narr", "what time".into()))
        .await
        .unwrap();

    assert_eq!(reply, "it is late", "narration is not the answer");
    let narrated: Vec<String> = sink
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            TurnEvent::AssistantText { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(narrated, vec!["Checking the clock first."]);
}

#[test]
fn the_budget_cutoff_prefers_the_models_own_words() {
    // This round's text wins; the last narration is the fallback; a silent
    // model gets the canned line. Either way the user is told it stopped early.
    let reply = stop_reply(
        BUDGET_STOP,
        "Still digging through the logs.",
        "earlier note",
    );
    assert!(reply.starts_with("Still digging through the logs."));
    assert!(reply.contains("tool-call limit"));

    let fallback = stop_reply(BUDGET_STOP, "  ", "earlier note");
    assert!(fallback.starts_with("earlier note"));

    let silent = stop_reply(BUDGET_STOP, "", "");
    assert!(silent.contains("tool-call limit"));
    assert!(!silent.contains("\n\n"));
}

/// A model that will not stop re-issuing one call ends the turn well short
/// of the round budget: the executor refuses the repeats, and when it keeps
/// asking anyway the loop stops rather than spending 120 rounds on it.
#[tokio::test]
async fn a_turn_repeating_one_call_stops_long_before_the_round_budget() {
    let db = Arc::new(Db::connect(&sqlite_url("komo_rt_spin.db")).await.unwrap());
    // The driver would happily keep asking for the same call forever.
    let (rt, received) = scripted_runtime(
        db.clone(),
        (0..8)
            .map(|_| tool_calls(vec![call("time", "{}")]))
            .collect(),
        vec![Arc::new(TimeTool)],
        120,
    );

    let reply = rt
        .handle_input("cli:spin", "什么时候".into())
        .await
        .unwrap();
    assert!(
        reply.contains("repeating the same step"),
        "the user is told why it stopped: {reply}"
    );

    // Two real executions, then refusals — not 120 rounds of them.
    let runs = RunRepository::list(&*db, 10).await.unwrap();
    let steps = RunRepository::steps(&*db, &runs[0].id).await.unwrap();
    assert_eq!(steps.len(), 2, "only the first two calls reached the tool");
    let rounds = received.lock().unwrap().len();
    assert!(rounds <= 4, "the turn ended after {rounds} rounds");
}

#[tokio::test]
async fn round_budget_forces_a_final_answer() {
    let db = Arc::new(Db::connect(&sqlite_url("komo_rt_budget.db")).await.unwrap());
    // Driver keeps requesting tools; with max_turns=2 the loop must stop.
    let (rt, _) = scripted_runtime(
        db.clone(),
        vec![
            tool_calls(vec![call("time", "{}")]),
            tool_calls(vec![call("time", "{}")]),
            tool_calls(vec![call("time", "{}")]),
            tool_calls(vec![call("time", "{}")]),
        ],
        vec![Arc::new(TimeTool)],
        2,
    );

    let reply = rt.handle_input("cli:s6", "hi".into()).await.unwrap();
    assert!(reply.contains("tool-call limit"), "got: {reply}");

    let runs = RunRepository::list(&*db, 10).await.unwrap();
    assert_eq!(runs[0].status, RunStatus::Done);
    // Only the first two rounds actually dispatched; round 3 got the budget
    // note instead of executing, so exactly two ledger steps.
    let steps = RunRepository::steps(&*db, &runs[0].id).await.unwrap();
    assert_eq!(steps.len(), 2);
}

/// Stage an interrupted turn: a session whose conversation ends on the user
/// message (the crash landed before any reply), its failed ledger run, and
/// the turn's recorded events. Returns the original run.
async fn seed_interrupted(db: &Arc<Db>, session_id: &str, rounds: u32) -> Run {
    use komo_core::domain::session_event::{
        AssistantRoundEvent, HeaderReason, MessageSource, RequestHeaderEvent, SurfacePlacement,
        UserMessageEvent,
    };
    SessionRepository::save(&**db, &Session::new(session_id))
        .await
        .unwrap();
    let run = Run::start(session_id, "do the thing");
    // The turn as the log holds it: it opened, it was asked, it got as far
    // as `rounds` completions — and then the process died, so there is no
    // terminal event. Its `turn/started` is what makes it a turn at all.
    let mut kinds = vec![
        SessionEventKind::TurnStarted {
            turn_id: run.id.clone(),
            resumed_from: None,
        },
        SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: run.id.clone(),
            content: "do the thing".into(),
            source: MessageSource::User,
            surface: SurfacePlacement::append(),
        }),
        SessionEventKind::RequestHeader(RequestHeaderEvent {
            reason: HeaderReason::Initial,
            provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            effort: String::new(),
            system: "You are komo.".into(),
            tools: vec![],
            extra: None,
        }),
    ];
    for round in 0..rounds {
        kinds.push(SessionEventKind::AssistantRound(AssistantRoundEvent {
            turn_id: run.id.clone(),
            round,
            response_id: format!("resp-{round}"),
            blocks: serde_json::json!([]),
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
        }));
    }
    SessionEventRepository::append(&**db, session_id, kinds)
        .await
        .unwrap();
    SessionEventRepository::durable_flush(&**db, session_id)
        .await
        .unwrap();
    // The row the interrupted turn left behind: opened and never closed,
    // exactly as the runtime commits it when a turn starts.
    let events = SessionEventRepository::events(&**db, session_id)
        .await
        .unwrap();
    let folded = project_runs(session_id, &events);
    RunProjectionStore::commit(
        &**db,
        session_id,
        &folded,
        events.last().map(|e| e.seq).unwrap(),
    )
    .await
    .unwrap();
    run
}

#[tokio::test]
async fn resume_interrupted_continues_without_a_new_user_message() {
    let db = Arc::new(Db::connect(&sqlite_url("komo_rt_resume.db")).await.unwrap());
    let original = seed_interrupted(&db, "cli:rs1", 2).await;

    let resumed_entries = Arc::new(Mutex::new(None));
    let rt = AgentRuntime {
        llm: Arc::new(ScriptedLlm {
            script: Mutex::new(vec![Step::Final("resumed reply".into())].into()),
            received: Arc::new(Mutex::new(Vec::new())),
            interjected: Arc::new(Mutex::new(Vec::new())),
            resumed_entries: resumed_entries.clone(),
            nudged: Arc::new(Mutex::new(Vec::new())),
        }),
        sessions: db.clone(),
        messages: db.clone(),
        events: db.clone(),
        projection: db.clone(),
        runs: db.clone(),
        tool_executor: ToolExecutor::new(
            komo_services::tool_execution::ToolExecutionConfig::default(),
        ),
        max_turns: 30,
        history_window: 0,
        learning: None,
        compaction: None,
        wakeups: None,
    };

    let reply = rt
        .resume_interrupted(&original)
        .await
        .unwrap()
        .expect("this run is continuable");
    assert_eq!(reply, "resumed reply");
    // The driver was reopened from the log, not begun fresh: the turn's own
    // events — the pair that opened it, plus the two rounds it got through.
    assert_eq!(*resumed_entries.lock().unwrap(), Some(4));

    // The continuation appended exactly one assistant message — the
    // interrupted turn's own user message still opens the pair.
    let session = SessionRepository::find(&*db, "cli:rs1")
        .await
        .unwrap()
        .unwrap();
    let roles: Vec<Role> = session.messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant]);
    assert_eq!(session.messages[1].content, "resumed reply");

    // The continuation is its own ledger run, linked back.
    let runs = RunRepository::list(&*db, 10).await.unwrap();
    let continuation = runs
        .iter()
        .find(|r| r.resumed_from.as_deref() == Some(original.id.as_str()))
        .expect("a continuation run linked to the original");
    assert_eq!(continuation.status, RunStatus::Done);

    // The turn's events live with the session, not with the run: nothing to
    // clear, and the conversation keeps the continuation's reply.
    let messages = MessageRepository::list_by_session(&*db, &original.session_id)
        .await
        .unwrap();
    assert_eq!(messages.last().unwrap().role, Role::Assistant);
}

#[tokio::test]
async fn resume_refuses_a_transcript_that_already_ends_in_a_reply() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_resume_guard.db"))
            .await
            .unwrap(),
    );
    let original = seed_interrupted(&db, "cli:rs2", 1).await;
    // The reply actually landed (crash in the gap before the ledger
    // closed) — the transcript ends on an assistant message.
    say(&db, "cli:rs2", Message::assistant("already delivered")).await;

    let (rt, _) = scripted_runtime(
        db.clone(),
        vec![Step::Final("should not run".into())],
        vec![],
        30,
    );
    let rt = AgentRuntime { ..rt };

    let outcome = rt.resume_interrupted(&original).await.unwrap();
    assert!(outcome.is_none(), "must decline, not continue");
    // Nothing was appended to the transcript, and no ledger run opened.
    let session = SessionRepository::find(&*db, "cli:rs2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.messages.len(), 2);
    let runs = RunRepository::list(&*db, 10).await.unwrap();
    assert_eq!(runs.len(), 1, "only the original run exists");
}

#[tokio::test]
async fn resume_without_journal_rows_fails_before_touching_anything() {
    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_resume_norows.db"))
            .await
            .unwrap(),
    );
    // An interrupted run whose turn left no events at all (a pre-log
    // build, or the appends failed) — the caller must fall back to the
    // digest path. The conversation is there; the turn is not.
    SessionRepository::save(&*db, &Session::new("cli:rs3"))
        .await
        .unwrap();
    say(&db, "cli:rs3", Message::user("do the thing")).await;
    let original = Run::start("cli:rs3", "do the thing");
    let (rt, _) = scripted_runtime(db.clone(), vec![], vec![], 30);
    let rt = AgentRuntime { ..rt };
    let outcome = rt.resume_interrupted(&original).await.unwrap();
    assert!(
        outcome.is_none(),
        "no rows ⇒ decline so the digest path runs"
    );
    let session = SessionRepository::find(&*db, "cli:rs3")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.messages.len(), 1, "transcript untouched");
}

/// The ordering fix, asserted end to end: learning is dispatched **after**
/// `runs.finish`, so the episode it assembles is a finished one.
///
/// The failure this guards against is silent, not loud. Dispatched from
/// inside the turn — where the post-turn review used to live — the run is
/// still `Running`, so it is not offered as an episode and the turn is
/// simply never learned from. Nothing errors; komo just stops learning.
#[tokio::test]
async fn learning_sees_a_finished_run_because_it_is_dispatched_after_the_ledger_closes() {
    /// Records the status each episode carried when the extractor saw it.
    struct StatusSpy(Arc<Mutex<Vec<(String, RunStatus)>>>);
    #[async_trait]
    impl komo_core::domain::reviewer::Reviewer for StatusSpy {
        async fn review(
            &self,
            _session: &Session,
            episodes: &[komo_core::domain::episode::AssessedEpisode],
        ) -> anyhow::Result<komo_core::domain::reviewer::ReviewOutcome> {
            self.0.lock().unwrap().extend(
                episodes
                    .iter()
                    .map(|e| (e.view.id().to_string(), e.view.run.status)),
            );
            Ok(Default::default())
        }
    }

    let db = Arc::new(
        Db::connect(&sqlite_url("komo_rt_learning_order.db"))
            .await
            .unwrap(),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (mut rt, _) =
        scripted_runtime(db.clone(), vec![Step::Final("done".into())], Vec::new(), 30);
    rt.learning = Some(Arc::new(
        crate::learning_coordinator::LearningCoordinator::new(
            db.clone(),
            db.clone(),
            db.clone(),
            Arc::new(StatusSpy(seen.clone())),
            1,
        ),
    ));

    rt.handle_input("cli:s1", "hi".into()).await.unwrap();

    // Learning runs detached, so wait for it rather than racing it.
    for _ in 0..200 {
        if !seen.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let runs = RunRepository::list(&*db, 10).await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        1,
        "the finished turn must reach the extractor — an empty list here \
             means learning ran while the run was still open"
    );
    assert_eq!(seen[0].0, runs[0].id);
    assert_eq!(seen[0].1, RunStatus::Done, "and it was already terminal");
}
