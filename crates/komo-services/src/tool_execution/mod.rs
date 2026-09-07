//! Tool execution as one deep module (architecture deepening plan §6).
//!
//! [`ToolExecutor`] owns the whole execution pipeline the agent loop used to
//! assemble by hand: catalog lookup, per-turn call budget, arg redaction,
//! panic-isolated spawning, transient-error retry, run-ledger recording, the
//! LLM-facing result cap, and error→outcome mapping. Callers hand it a round
//! of model-requested calls plus an explicit [`ToolTurnContext`] and get back
//! `ToolOutcome`s ready for `TurnDriver::step` — they never see lookup, retry,
//! ledger, or cap decisions. Execution policy (result cap, call budget) is
//! **instance-owned** [`ToolExecutionConfig`], not process globals, so two
//! executors can carry different policies.
//!
//! Every execution path funnels through the one internal core, reached via
//! [`ToolExecutor::execute_round`]: the LLM backend only ever *declares* the
//! tools to the provider, so nothing else can run one.

pub mod context;
mod result;
/// Transient-error classification. `pub(crate)` because the LLM adapter retries
/// its completions on the same classification (`infra::llm::with_retry`) — one
/// definition of "transient", so the tool path and the model path can't drift.
pub(crate) mod retry;

use std::sync::Arc;
use std::time::Duration;

use tracing::{Instrument, info, info_span, warn};

pub use context::{
    RunContext, SessionContext, SessionOrigin, SpinDetector, SpinVerdict, ToolContext,
    ToolTurnContext, TurnResultBudget, current_job_grants, current_session, with_job_grants,
    with_session,
};

use crate::tool_output_store::{Bounded, ToolOutputStore};
use komo_core::domain::approval::{ApprovalRequest, Approver, Decision};
use komo_core::domain::catalog::{CatalogSnapshot, ToolCatalog};
use komo_core::domain::context::ApprovalGate;
use komo_core::domain::events::TurnEvent;
use komo_core::domain::llm::{ToolCallReq, ToolOutcome};
use komo_core::domain::policy::{Access, Category, Policy};
use komo_core::domain::repository::SessionEventRepository;
use komo_core::domain::run::{RunStep, STEP_FIELD_CAP, truncate};
use komo_core::domain::session_event::{
    SessionEventKind, ToolCallSettledEvent, ToolCallStartedEvent, ToolOutcome as SettledOutcome,
};
use komo_core::domain::tool::{Tool, ToolError};

/// Live `TurnEvent` args/result use the **ledger's** cap, not a smaller one of
/// their own. A watcher renders a running call from the stream and the same call
/// from the ledger after a reload with one component, so a tighter live cap
/// showed a truncated result that silently grew on reload. The stream is
/// loopback SSE — a couple of KB per call costs nothing worth that.
const EVENT_SUMMARY_CAP: usize = STEP_FIELD_CAP;

use context::{JOB_GRANTS, SESSION};
use result::cap_tool_result;
use retry::{TOOL_RETRY_BACKOFF_MS, TOOL_RETRY_MAX_ATTEMPTS, settle, should_retry};

/// Soft per-turn tool-call budget default (backstop). The runtime's
/// `max_turns` bounds *round-trips*, but a single round can request many tools
/// at once; this caps the *total* calls per turn so a runaway loop can't fan
/// out unbounded. Set generously above any legitimate turn. Enforced against
/// the run-ledger seq, so it applies only to ledgered turns (the main agent),
/// never to callers without a run context.
const DEFAULT_MAX_TOOL_CALLS_PER_TURN: i64 = 500;

/// Hard ceiling on how many tool calls a *single* round may actually execute.
/// The per-turn budget bounds the total, but a single malformed round can
/// request thousands of calls at once; without this each one would spawn a
/// task and write a ledger step, flooding both. Calls past the ceiling get a
/// short note instead — logged, never silently dropped. Set well above any
/// legitimate parallel tool use in one round.
const MAX_CALLS_PER_ROUND: usize = 32;

/// A structured view over [`STEP_FIELD_CAP`] is replaced, not cut: half a JSON
/// document fails to parse, so every reader would have to treat a truncated cell
/// as corrupt. The marker keeps the cell valid and says what happened.
fn cap_structured(structured: serde_json::Value) -> serde_json::Value {
    if structured.is_null() {
        return structured;
    }
    let rendered = structured.to_string();
    if rendered.len() <= STEP_FIELD_CAP {
        return structured;
    }
    serde_json::json!({ "_elided": "structured view over the field cap", "bytes": rendered.len() })
}

/// Which `[policy]` category (and, for files, which access kind) a tool's
/// actions fall under — the mapping behind [`ToolExecutor::drop_policy_denied`].
///
/// `None` means "not subject to the permission policy": `time`, `todo`, `task`,
/// `memory`, `skill`, … carry no [`ActionRef`], so no rule can ever deny them and
/// they are never filtered. A tool added without an entry here defaults to that
/// safe side — it stays advertised.
///
/// [`ActionRef`]: komo_core::domain::approval::ActionRef
fn policy_scope(name: &str) -> Option<(Category, Option<Access>)> {
    match name {
        "shell" => Some((Category::Shell, None)),
        "read" | "grep" | "glob" | "logs" => Some((Category::File, Some(Access::Read))),
        "write" | "edit" | "apply_patch" => Some((Category::File, Some(Access::Write))),
        "web_fetch" | "web_search" => Some((Category::Network, None)),
        "homeassistant" => Some((Category::HomeAssistant, None)),
        "wiki_index" | "wiki_read" => Some((Category::Wiki, None)),
        // Every mounted MCP tool is named `mcp__<server>__<tool>`, so one
        // prefix covers them all — a `deny mcp any` rule drops the lot from the
        // catalog rather than paying a schema each to refuse them per call.
        name if name.starts_with("mcp__") => Some((Category::Mcp, None)),
        // Same trick for plugin-registered tools (`py__<tool>`): one prefix
        // covers a set whose members are only known at runtime, so a
        // `deny plugin any` rule is enforceable even though the names were not.
        name if name.starts_with("py__") => Some((Category::Plugin, None)),
        _ => None,
    }
}

/// Instance-owned execution policy.
#[derive(Debug, Clone, Copy)]
pub struct ToolExecutionConfig {
    /// Byte cap on a single tool result handed back to the LLM.
    pub max_result_bytes: usize,
    /// Cumulative per-turn cap on tool output fed back to the model (`0` =
    /// unlimited). Enforced via the turn's [`TurnResultBudget`].
    pub max_turn_result_bytes: usize,
    /// Per-turn cap on ledgered tool calls (logical calls, not retry attempts).
    pub max_calls_per_turn: i64,
    /// Wall-clock timeout for one tool call (`None` = no timeout). A hung tool
    /// is aborted and the call fails cleanly rather than wedging the turn.
    pub max_call_duration: Option<Duration>,
}

impl Default for ToolExecutionConfig {
    fn default() -> Self {
        Self {
            max_result_bytes: komo_config::DEFAULT_MAX_TOOL_RESULT_BYTES,
            max_turn_result_bytes: komo_config::DEFAULT_MAX_TURN_RESULT_BYTES,
            max_calls_per_turn: DEFAULT_MAX_TOOL_CALLS_PER_TURN,
            max_call_duration: Some(Duration::from_secs(komo_config::DEFAULT_TOOL_TIMEOUT_SECS)),
        }
    }
}

impl ToolExecutionConfig {
    /// The default policy with a specific single-result cap (the most commonly
    /// tuned setting, via `max_tool_result_bytes`).
    pub fn with_result_cap(max_result_bytes: usize) -> Self {
        Self {
            max_result_bytes,
            ..Self::default()
        }
    }

    /// Set the cumulative per-turn output budget (`0` = unlimited).
    pub fn with_turn_budget(mut self, max_turn_result_bytes: usize) -> Self {
        self.max_turn_result_bytes = max_turn_result_bytes;
        self
    }

    /// Set the per-call wall-clock timeout (`0` seconds = no timeout).
    pub fn with_call_timeout_secs(mut self, secs: u64) -> Self {
        self.max_call_duration = (secs > 0).then(|| Duration::from_secs(secs));
        self
    }
}

/// The tool-execution module's external interface. Cheap to clone (one `Arc`);
/// every caller shares the same core, so all execution paths carry identical
/// retry/ledger/cap semantics.
#[derive(Clone)]
pub struct ToolExecutor {
    core: Arc<ToolExecutionCore>,
}

/// The shared implementation: the catalog plus the execution policy and the
/// approver every migrated tool reaches through its [`ToolContext`].
pub struct ToolExecutionCore {
    /// What the model may call. Shared with whoever declares the schemas, so
    /// the set the model is told about and the set this dispatches against are
    /// the same object — see [`ToolCatalog`].
    ///
    /// Dispatch reads a [`CatalogSnapshot`], not this: within a turn the
    /// catalog may change under us, and a call the model was invited to make
    /// must not answer "unknown tool" a round later.
    catalog: Arc<ToolCatalog>,
    /// Set when this executor is pinned to one turn's view
    /// ([`ToolExecutor::pin`]). `None` — the wiring-time and test executors —
    /// reads the catalog's current snapshot per round.
    pinned: Option<Arc<CatalogSnapshot>>,
    config: ToolExecutionConfig,
    /// The approver placed into each call's [`ToolContext`]. Defaults to
    /// deny-all; wiring installs the real (policy-wrapped) approver via
    /// [`ToolExecutor::with_approver`].
    approver: Arc<dyn Approver>,
    /// Where an over-limit result is kept in full. `None` ⇒ over-limit results
    /// are truncated, with the tail lost — the behavior before roadmap item 10.
    output_store: Option<Arc<ToolOutputStore>>,
    /// Where a tool call is recorded in the session's event log, so the log
    /// holds the work and not only what was said. `None` (tests, aux executors)
    /// ⇒ nothing is recorded, which is what the transcript looked like before.
    events: Option<Arc<dyn SessionEventRepository>>,
}

impl ToolExecutor {
    pub fn new(config: ToolExecutionConfig) -> Self {
        Self::with_catalog(Arc::new(ToolCatalog::new()), config)
    }

    /// An executor over an existing catalog — the wiring path, where the model
    /// backend needs the same catalog to declare schemas from.
    pub fn with_catalog(catalog: Arc<ToolCatalog>, config: ToolExecutionConfig) -> Self {
        Self {
            core: Arc::new(ToolExecutionCore {
                catalog,
                pinned: None,
                config,
                approver: Arc::new(DenyAllApprover),
                output_store: None,
                events: None,
            }),
        }
    }

    /// The catalog this executor dispatches against, for a caller that mounts
    /// tools into it or declares its schemas.
    pub fn catalog(&self) -> &Arc<ToolCatalog> {
        &self.core.catalog
    }

    /// An executor pinned to the catalog as it is *now*, for one turn.
    ///
    /// The runtime pins at turn start and uses the result for every round: the
    /// model is handed one set of schemas, so the executor has to keep
    /// dispatching against that set even if a plugin mounts or unmounts
    /// mid-turn. The mutation is not lost — the next turn pins the new set.
    pub fn pin(&self) -> Self {
        let snapshot = self.snapshot();
        Self {
            core: Arc::new(ToolExecutionCore {
                catalog: self.core.catalog.clone(),
                pinned: Some(snapshot),
                config: self.core.config,
                approver: self.core.approver.clone(),
                output_store: self.core.output_store.clone(),
                events: self.core.events.clone(),
            }),
        }
    }

    /// The catalog view this executor reads: its pinned turn snapshot, else the
    /// catalog's current one.
    pub fn snapshot(&self) -> Arc<CatalogSnapshot> {
        self.core.snapshot()
    }

    /// A non-owning handle, for a tool that needs to dispatch *other* tools.
    ///
    /// Weak on purpose: such a tool lives in the very catalog this executor
    /// reads, so an owning handle would be a cycle that never drops. Nothing is
    /// lost by it — an executor gone while one of its own tools is running
    /// cannot happen, and if it somehow did, [`WeakToolExecutor::upgrade`]
    /// saying so beats a leak.
    pub fn downgrade(&self) -> WeakToolExecutor {
        WeakToolExecutor {
            core: Arc::downgrade(&self.core),
        }
    }

    /// Install the approver handed to every tool via its [`ToolContext`]. Called
    /// during wiring before the executor is shared (like [`register`]).
    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        let core = Arc::get_mut(&mut self.core)
            .expect("set the approver during wiring, before the executor is shared");
        core.approver = approver;
        self
    }

    /// Install the store that keeps an over-limit result in full, so the model
    /// gets a head+tail preview and a path instead of a one-sided truncation.
    /// Absent (tests, and any executor wiring hasn't given one) ⇒ plain
    /// truncation, the previous behavior.
    pub fn with_output_store(mut self, store: Arc<ToolOutputStore>) -> Self {
        let core = Arc::get_mut(&mut self.core)
            .expect("set the output store during wiring, before the executor is shared");
        core.output_store = Some(store);
        self
    }

    /// Install the transcript a tool call is recorded in. Absent ⇒ calls are
    /// not recorded there, which is every aux executor: their sessions are
    /// synthetic and a file per one-shot turn is litter.
    pub fn with_events(mut self, events: Arc<dyn SessionEventRepository>) -> Self {
        let core = Arc::get_mut(&mut self.core)
            .expect("set the transcript during wiring, before the executor is shared");
        core.events = Some(events);
        self
    }

    /// Add a tool for the life of the process (the wiring path). A tool that
    /// can be taken back out is mounted on the [`catalog`](Self::catalog)
    /// instead, which hands back a guard.
    ///
    /// Takes `&mut self` although the catalog no longer needs it: registering
    /// through an executor is the wiring-time gesture, and the borrow keeps it
    /// from being reached for once the executor is shared.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.core.catalog.register(tool);
    }

    /// The catalog as the model adapter needs it (schemas for function
    /// calling), name-sorted so the serialized tool block is byte-stable — a
    /// provider prompt cache matches on exact bytes. A read-only view —
    /// execution always goes through the executor.
    pub fn definitions(&self) -> Vec<Arc<dyn Tool>> {
        self.snapshot().tools().cloned().collect()
    }

    /// Drop the tools `policy` denies outright, returning their names (sorted) so
    /// wiring can log what it removed. Called during wiring, right after
    /// registration and before the catalog is read — the prompt's tool-name list
    /// and the model's function schemas both come from [`definitions`], so
    /// filtering here keeps them from ever disagreeing about what exists.
    ///
    /// Only a wholly-denied tool goes (see [`Policy::wholly_denied`]): a tool
    /// that *can* act, just not everywhere, stays advertised and refuses the
    /// individual call — the model gets an explanation it can work with, which a
    /// missing tool never is.
    ///
    /// [`definitions`]: Self::definitions
    /// [`Policy::wholly_denied`]: komo_core::domain::policy::Policy::wholly_denied
    pub fn drop_policy_denied(&mut self, policy: &Policy) -> Vec<String> {
        let mut removed = self.core.catalog.retain(|name| {
            policy_scope(name)
                .is_some_and(|(category, access)| policy.wholly_denied(category, access))
        });
        removed.sort();
        removed
    }

    /// The cumulative per-turn tool-output budget this executor enforces (`0` =
    /// unlimited). The runtime seeds each turn's [`TurnResultBudget`] from it.
    pub fn turn_result_cap(&self) -> usize {
        self.core.config.max_turn_result_bytes
    }

    /// Execute one round of model-requested tool calls concurrently, preserving
    /// order. Unknown tools and tool errors are mapped into the outcome content
    /// (the model can recover); nothing here aborts the turn.
    ///
    /// Concurrency is safe for approval prompts: the interactive approver
    /// serializes them per session, so two side-effecting tools in one round
    /// still prompt one at a time.
    pub async fn execute_round(
        &self,
        calls: &[ToolCallReq],
        context: &ToolTurnContext,
    ) -> Vec<ToolOutcome> {
        // One view for the whole round, so two calls in it can never see
        // different catalogs. On a pinned executor this is the turn's view.
        let catalog = self.snapshot();
        // Bound the per-round fan-out: a single malformed round can request far
        // more calls than any real parallel tool use. Calls past the ceiling get
        // a note without spawning a task or writing a ledger step, so a runaway
        // round can't flood either. Never silent — log what was skipped.
        if calls.len() > MAX_CALLS_PER_ROUND {
            warn!(
                requested = calls.len(),
                ceiling = MAX_CALLS_PER_ROUND,
                "tool round exceeded the per-round call ceiling; extra calls skipped"
            );
        }
        // Decide the spin verdicts here, in dispatch order, before anything is
        // spawned: the calls themselves run concurrently, so asking mid-flight
        // would make "three in a row" depend on which future happened to get
        // there first. See `SpinDetector`.
        let verdicts: Vec<SpinVerdict> = calls
            .iter()
            .map(|call| context.spin.observe(&call.name, &call.args))
            .collect();

        // Every call in the round is dispatched at once below, so the whole
        // round's intent is one append and **one** durable flush — not an fsync
        // per call. Written before anything runs, because "the tool never
        // started" and "it started and we lost the answer" need different
        // answers on recovery and are otherwise indistinguishable.
        //
        // Best-effort like the ledger, with one difference: a failed flush
        // means the round's intent did not survive, so recovery will read those
        // calls as never-dispatched. Logged loudly for that reason.
        if let (Some(events), Some(run)) = (&self.core.events, &context.run) {
            let started: Vec<SessionEventKind> = calls
                .iter()
                .take(MAX_CALLS_PER_ROUND)
                .enumerate()
                .map(|(i, call)| {
                    SessionEventKind::ToolCallStarted(ToolCallStartedEvent {
                        turn_id: run.run_id.clone(),
                        call_id: call.call_id.clone().unwrap_or_else(|| call.id.clone()),
                        call_index: i as u32,
                        tool: call.name.clone(),
                        args: catalog
                            .get(&call.name)
                            .map(|tool| tool.redact_args(&call.args))
                            .unwrap_or_else(|| call.args.clone()),
                    })
                })
                .collect();
            let session = context.session.session_id.clone();
            if let Err(error) = events.append(&session, started).await {
                warn!(%error, "failed to record the round's dispatch intent (non-fatal)");
            } else if let Err(error) = events.durable_flush(&session).await {
                warn!(%error, "the round's dispatch intent is not durable; a crash now would read these calls as never started");
            }
        }

        let catalog = &catalog;
        let futures = calls.iter().zip(&verdicts).enumerate().map(
            |(i, (call, verdict))| async move {
                // Only a call that actually reached its tool has one; every
                // refusal below answers with text alone.
                let mut structured = serde_json::Value::Null;
                let content = if i >= MAX_CALLS_PER_ROUND {
                    format!(
                        "error: too many tool calls in one round (limit {MAX_CALLS_PER_ROUND}); \
                         this call was skipped. Request fewer tools per round."
                    )
                } else if matches!(verdict, SpinVerdict::Refuse | SpinVerdict::Stop) {
                    warn!(
                        tool = %call.name,
                        "identical tool call repeated; refusing to run it again"
                    );
                    format!(
                        "error: `{}` was already called twice with these exact arguments and \
                         returned the same result; running it again cannot change anything. \
                         Use what those calls returned, try a different approach, or answer \
                         the user with what you have.",
                        call.name
                    )
                } else if call.args.trim().is_empty() {
                    // Empty arguments mean the model's output was cut off
                    // mid-call — a tool that genuinely takes none still gets
                    // `{}`. Running it would act on whatever the defaults are
                    // rather than on what was asked.
                    format!(
                        "error: the arguments for `{}` arrived empty, which usually means the \
                         response was truncated. Re-issue the call with its arguments.",
                        call.name
                    )
                } else {
                    match catalog.get(&call.name) {
                        Some(tool) => match self
                            .core
                            .execute(
                                tool.clone(),
                                call.args.clone(),
                                context,
                                call.call_id.as_deref().unwrap_or(&call.id),
                                i as u32,
                            )
                            .await
                        {
                            Ok((out, view)) => {
                                structured = view;
                                out
                            }
                            Err(error) => format!("tool `{}` failed: {error:#}", call.name),
                        },
                        None => format!("error: unknown tool `{}`", call.name),
                    }
                };
                ToolOutcome {
                    id: call.id.clone(),
                    call_id: call.call_id.clone(),
                    content,
                    structured,
                }
            },
        );
        futures_util::future::join_all(futures).await
    }
}

/// A [`ToolExecutor`] handle that does not keep it alive. See
/// [`ToolExecutor::downgrade`].
#[derive(Clone)]
pub struct WeakToolExecutor {
    core: std::sync::Weak<ToolExecutionCore>,
}

impl WeakToolExecutor {
    /// The executor, if it still exists.
    pub fn upgrade(&self) -> Option<ToolExecutor> {
        self.core.upgrade().map(|core| ToolExecutor { core })
    }
}

impl ToolExecutionCore {
    /// This core's catalog view: the pinned turn snapshot when there is one,
    /// else whatever the catalog holds right now.
    fn snapshot(&self) -> Arc<CatalogSnapshot> {
        match &self.pinned {
            Some(pinned) => pinned.clone(),
            None => self.catalog.snapshot(),
        }
    }

    /// Run one tool call through the full pipeline. The invariant order:
    ///
    /// 1. claim a ledger seq (budget counts logical calls, not attempts)
    /// 2. redact args for the audit record
    /// 3. execute on an isolated, panic-catching task with the session context
    ///    installed and a `tool` tracing span
    /// 4. map panics/cancellation to errors
    /// 5. retry per the transient classification (typed hint first)
    /// 6. record the (original, truncated) step — best-effort
    /// 7. cap the LLM-facing result
    ///
    /// Answers with the model-facing text *and* the tool's structured view: the
    /// text has been capped and may have been swapped for a preview, so a caller
    /// that needs the result as data cannot recover it by parsing what comes
    /// back. `Null` for a tool that reports no structured view.
    pub async fn execute(
        &self,
        tool: Arc<dyn Tool>,
        input: String,
        context: &ToolTurnContext,
        call_id: &str,
        call_index: u32,
    ) -> anyhow::Result<(String, serde_json::Value)> {
        let name = tool.name();

        // Ledger bookkeeping (only when this turn is recorded). Capture the
        // redacted args and seq up front: the raw `input` is cloned per attempt
        // below, and the seq must be claimed before the tool runs so the span
        // and the persisted step agree.
        let ledger = context.run.as_ref().map(|r| (r, r.next_seq()));
        let redacted_args = ledger.as_ref().map(|_| tool.redact_args(&input));
        let started_at = now();
        // Wall-clock timestamps (`now()`) are integer unix seconds — fine for
        // the ledger's started/ended fields, but differencing them only yields
        // whole seconds, so any sub-second tool would log `elapsed_ms = 0`.
        // Measure the duration off a monotonic `Instant` instead.
        let started_instant = std::time::Instant::now();
        let seq_field = ledger.as_ref().map(|(_, s)| *s).unwrap_or(-1);

        // Live event: a watcher (streaming client) sees the call start. Args are
        // the redacted form when ledgered, else redacted on the spot — never the
        // raw input. No-op when no sink is attached (the common case).
        if let Some(sink) = &context.session.event_sink {
            let args = redacted_args
                .clone()
                .unwrap_or_else(|| tool.redact_args(&input));
            sink.emit(TurnEvent::ToolStarted {
                seq: seq_field,
                name: name.to_string(),
                args: truncate(&args, EVENT_SUMMARY_CAP),
                started_at_ms: now_ms(),
            });
        }

        // Parse the model's JSON arguments once, here, so every tool sees a
        // typed `Value` and `parse_args` can produce the canonical
        // `InvalidInput` error. Args that aren't JSON at all (a model emitting
        // bare text) become a `Value::String`, which every tool's `parse_args`
        // rejects with that same canonical error — the text is preserved in it
        // so the model can see what it sent.
        let value = serde_json::from_str::<serde_json::Value>(&input)
            .unwrap_or_else(|_| serde_json::Value::String(input.clone()));

        // Filled in by a successful call below; stays `Null` for a failure or a
        // tool that has no structured view.
        let mut structured = serde_json::Value::Null;

        // Soft tool-call budget (backstop): once this turn has reached the cap,
        // refuse further calls with an error the model sees instead of
        // executing them. Inactive without a run ledger (seq_field = -1).
        let result: anyhow::Result<String> = if seq_field >= self.config.max_calls_per_turn {
            warn!(
                tool = name,
                seq = seq_field,
                budget = self.config.max_calls_per_turn,
                "tool-call budget reached for this turn; refusing"
            );
            Err(anyhow::anyhow!(
                "tool-call budget of {} reached for this turn; \
                 stop calling tools and answer the user with what you already have.",
                self.config.max_calls_per_turn
            ))
        } else {
            let mut attempt: usize = 0;
            let outcome: Result<komo_core::domain::tool::ToolOutput, ToolError> = loop {
                // Span so the tool's own logs carry the run's `seq`/`name`.
                // Spans don't cross `tokio::spawn` on their own — instrument
                // the spawned future. A fresh span per attempt keeps each
                // retry's logs distinct.
                let span = info_span!("tool", name, seq = seq_field, attempt);
                let tool_attempt = tool.clone();
                let value_attempt = value.clone();
                // Build the explicit per-call context, and also install the
                // turn's session and job grants as the ambient scope for the
                // spawned task — the approvers still read them (they don't
                // take a context parameter), and a fresh task doesn't inherit
                // task-locals.
                let mut ctx = ToolContext::new(
                    context.session.clone(),
                    context.run.clone(),
                    self.approver.clone(),
                )
                // Which call this is — what a tool that stops to wait names, so
                // the continuation re-dispatches it as itself.
                .with_call(call_id, call_index);
                // Makes this call's approval a durable fact — the widest crash
                // window in a turn is a person deciding.
                if let (Some(events), Some(run)) = (&self.events, &context.run) {
                    ctx = ctx.with_approval_gate(ApprovalGate::new(
                        events.clone(),
                        &context.session.session_id,
                        &run.run_id,
                        call_id,
                        call_index,
                    ));
                }
                let scope = context.session.clone();
                let grants = current_job_grants();
                let join = tokio::spawn(
                    SESSION
                        .scope(
                            scope,
                            JOB_GRANTS.scope(grants, async move {
                                tool_attempt.call(value_attempt, &ctx).await
                            }),
                        )
                        .instrument(span),
                );
                // Wall-clock timeout backstop: a tool that hangs forever (a
                // shell command waiting on stdin, a timeout-less HTTP client)
                // would otherwise await indefinitely and wedge the turn — and
                // the session, since the loop can't finish. On elapse, abort the
                // task (kill_on_drop tools reap their child) and fail the call.
                // The message deliberately avoids the "timeout/timed out"
                // markers so the retry classifier treats it as terminal — a
                // wall-clock exhaustion won't succeed on an immediate retry.
                let abort = join.abort_handle();
                // A tool that legitimately waits (a sub-agent completion, a human
                // reading an approval prompt, a build it was given ten minutes
                // for) declares its own ceiling; the config default only applies
                // to tools for which waiting means hanging.
                let limit = tool.max_duration().or(self.config.max_call_duration);
                let joined = match limit {
                    Some(d) => match tokio::time::timeout(d, join).await {
                        Ok(r) => r,
                        Err(_) => {
                            abort.abort();
                            let elapsed = anyhow::anyhow!(
                                "did not report back within its {}s execution limit and was aborted",
                                d.as_secs()
                            );
                            // Aborting stops us waiting; it does not undo
                            // whatever the tool had already done. For an
                            // idempotent tool that distinction costs nothing —
                            // for any other, saying "failed" would invite the
                            // model to apply the effect a second time.
                            break Err(if tool.idempotent() {
                                ToolError::Failed(elapsed)
                            } else {
                                ToolError::Uncertain(elapsed)
                            });
                        }
                    },
                    None => join.await,
                };
                let attempt_result: Result<komo_core::domain::tool::ToolOutput, ToolError> =
                    match joined {
                        Ok(result) => result,
                        Err(join_err) if join_err.is_panic() => {
                            let panic = join_err.into_panic();
                            let msg = panic
                                .downcast_ref::<String>()
                                .map(String::as_str)
                                .or_else(|| panic.downcast_ref::<&str>().copied())
                                .unwrap_or("unknown panic");
                            Err(ToolError::Failed(anyhow::anyhow!(
                                "tool `{name}` panicked: {msg}"
                            )))
                        }
                        Err(join_err) => Err(ToolError::Failed(anyhow::anyhow!(
                            "tool `{name}` was cancelled: {join_err}"
                        ))),
                    };

                match &attempt_result {
                    // Only genuine failures retry; InvalidInput/Denied are
                    // recoverable and terminal.
                    Err(ToolError::Failed(error))
                        if attempt + 1 < TOOL_RETRY_MAX_ATTEMPTS
                            && should_retry(error, tool.idempotent()) =>
                    {
                        let delay =
                            TOOL_RETRY_BACKOFF_MS[attempt.min(TOOL_RETRY_BACKOFF_MS.len() - 1)];
                        warn!(
                            tool = name,
                            seq = seq_field,
                            attempt = attempt + 1,
                            delay_ms = delay,
                            error = %format!("{error:#}"),
                            "transient tool error; retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        attempt += 1;
                    }
                    // Not retrying is not the same as knowing nothing happened.
                    // An ambiguous error on a non-idempotent tool is exactly the
                    // case the retry classifier declined to touch — and until
                    // now the model was told "failed" and re-issued the call
                    // itself, which is the double-apply the classifier existed
                    // to prevent.
                    _ => break settle(attempt_result, tool.idempotent()),
                }
            };

            // Classify into the ledger-facing `anyhow::Result<String>`:
            // recoverable errors become model-facing content (never retried);
            // a genuine failure stays an `Err` so the ledger marks the step
            // failed and `execute_round` surfaces it.
            match outcome {
                Ok(out) => {
                    // The tool's machine-readable view rides to the ledger, not
                    // to the model — it never pays tokens for it.
                    structured = out.structured;
                    Ok(out.text)
                }
                Err(ToolError::InvalidInput(m)) => Ok(format!(
                    "invalid input for tool `{name}`: {m}. \
                     Rewrite the arguments to match the tool's schema."
                )),
                Err(ToolError::Denied(m)) => Ok(m),
                Err(ToolError::Failed(e)) => Err(e),
                // Still an `Err` — the call did not confirm success, so the
                // ledger marks the step failed. What changes is what the model
                // is told: the reply below composes into "tool `x` failed: did
                // not confirm …", which asks for a check rather than a retry.
                Err(ToolError::Uncertain(e)) => Err(anyhow::Error::new(
                    komo_core::domain::tool::UncertainOutcome::new(format!(
                        "did not confirm its result ({e:#}). It may or may not have taken \
                             effect — check the target's state before calling it again; repeating \
                             it blindly can apply the same change twice."
                    )),
                )),
            }
        };

        // Measured once and shared by the live event and the ledger step, so a
        // watcher and `run inspect` report the same duration for the same call.
        let elapsed_ms = started_instant.elapsed().as_millis() as i64;

        // Live event: the call finished (after retries collapse). Emitted
        // regardless of ledger state so a watcher sees every call resolve.
        if let Some(sink) = &context.session.event_sink {
            let (ok, summary) = match &result {
                Ok(out) => (true, truncate(out, EVENT_SUMMARY_CAP)),
                Err(e) => (false, truncate(&format!("{e:#}"), EVENT_SUMMARY_CAP)),
            };
            sink.emit(TurnEvent::ToolFinished {
                seq: seq_field,
                name: name.to_string(),
                ok,
                summary,
                elapsed_ms,
            });
        }

        // The ledger's view of the outcome, taken from the *original* result —
        // the audit record keeps what the model was not shown.
        let (ok, result_s, error_s) = match &result {
            Ok(out) => (true, truncate(out, STEP_FIELD_CAP), String::new()),
            Err(e) => (
                false,
                String::new(),
                truncate(&format!("{e:#}"), STEP_FIELD_CAP),
            ),
        };
        // Not `ok`, but not the same as failed either: the call may have landed
        // and only its answer was lost. An operator asking "did that go
        // through?" a week later needs the two told apart, and by here the
        // `ToolError` variant is gone — the marker rides in the error chain.
        let uncertain = result
            .as_ref()
            .err()
            .is_some_and(komo_core::domain::tool::UncertainOutcome::marks);

        // Size the model's view. Over the cap, the full output is written out and
        // the model gets a head+tail preview naming that file — so this has to
        // run before the step is recorded, which is what carries the path.
        let bounded = result.map(|out| self.bound(out, context, seq_field));

        // A call that stopped to wait **did not happen**: no step, no
        // `tool/call-settled`, nothing for a continuation to replay. Its
        // `tool/call-started` stands, beside either the `approval/requested`
        // that has no answer yet or the `turn/suspended` naming this call —
        // which is exactly the "asked for, never ran" reading recovery already
        // has, and what lets the re-dispatch after the wake be the first and
        // only run of it. The two cases differ only in who raised the wait
        // (the gate, or the tool through `ToolContext::wait_for`).
        let suspended = context
            .run
            .as_ref()
            .is_some_and(|run| run.suspended_call(call_id));

        // Record the step — best-effort, never affecting the tool's own result.
        // Retries collapse into this one step: the retry is a robustness
        // detail, not extra audit rows.
        if let (Some((run, seq)), Some(args)) = (ledger, redacted_args)
            && !suspended
        {
            let ended_at = now();
            if ok {
                info!(tool = name, seq, elapsed_ms, "tool ok");
            } else {
                warn!(tool = name, seq, error = %error_s, "tool failed");
            }
            let step = RunStep {
                run_id: run.run_id.clone(),
                seq,
                tool_name: name.to_string(),
                args: truncate(&args, STEP_FIELD_CAP),
                result: result_s,
                error: error_s,
                ok,
                uncertain,
                started_at,
                ended_at,
                elapsed_ms,
                structured: cap_structured(structured.clone()),
                output_paths: bounded
                    .as_ref()
                    .map(|b| {
                        b.output_paths
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
                // Projected from this call's `approval/resolved`, not carried
                // here: the executor never sees which rung answered, and this
                // copy of the step only feeds the turn's own tool note.
                approved_by: String::new(),
                approval_waited_ms: 0,
            };
            // The turn's own copy, for the tool note its closing message
            // carries. The ledger's step rows are a projection of the event
            // below, committed when the turn closes — so there is nothing to
            // read back from them while the turn is still running.
            run.record_step(step.clone());

            // Settled, in the session's own log — **per call**, the moment it
            // settles. Not once per round: a round of three that crashes with
            // two finished has two real results, and a single batched record
            // written at the end would report all three as lost. Written from
            // the step's values so the log inherits the ledger's redaction and
            // cap. Best-effort, like the step: the record of the work must
            // never cost the work.
            if let (Some(events), Some(run)) = (&self.events, &context.run) {
                let settled = SessionEventKind::ToolCallSettled(ToolCallSettledEvent {
                    turn_id: run.run_id.clone(),
                    call_id: call_id.to_string(),
                    call_index,
                    outcome: if step.uncertain {
                        SettledOutcome::Uncertain
                    } else if step.ok {
                        SettledOutcome::Succeeded
                    } else {
                        SettledOutcome::Failed
                    },
                    result: step.result.clone(),
                    error: step.error.clone(),
                    elapsed_ms,
                    structured: step.structured.clone(),
                    output_paths: step.output_paths.clone(),
                });
                if let Err(error) = events
                    .append(&context.session.session_id, vec![settled])
                    .await
                {
                    warn!(%error, tool = name, "failed to record the settled call (non-fatal)");
                }
            }
        }

        // Charge the bounded result against the turn's cumulative budget: once
        // the turn is over budget, it is swapped for a short note so a long tool
        // chain can't quietly overflow the context window (the ledger — and, for
        // an over-limit result, the stored file — still have the real thing).
        let text = bounded.map(|b| match context.budget.admit(b.text) {
            Ok(out) => out,
            Err(note) => note,
        })?;
        Ok((text, structured))
    }

    /// Size one result for the model: the store's head+tail preview when a store
    /// is wired and this is a ledgered turn, else the old one-sided truncation.
    ///
    /// The store is skipped without a ledger seq (aux sub-agents, sweeps): those
    /// have no run to point an operator back at and no `read`-capable follow-up
    /// turn, so a file on disk nobody will open is just litter.
    fn bound(&self, out: String, context: &ToolTurnContext, seq: i64) -> Bounded {
        let cap = self.config.max_result_bytes;
        match (&self.output_store, seq >= 0) {
            (Some(store), true) => store.bound(
                &context.session.session_id,
                &format!(
                    "{}-{seq:04}",
                    context
                        .run
                        .as_ref()
                        .map(|r| r.run_id.as_str())
                        .unwrap_or("run")
                ),
                out,
                cap,
            ),
            _ => Bounded {
                text: cap_tool_result(out, cap),
                output_paths: Vec::new(),
            },
        }
    }
}

/// The executor's default approver until wiring installs the real one
/// (`ToolExecutor::with_approver`): deny everything. A tool that reaches
/// `ctx.approve(..)` before an approver is set is refused rather than silently
/// allowed — matters only in tests that don't exercise a migrated gated tool.
struct DenyAllApprover;

#[async_trait::async_trait]
impl Approver for DenyAllApprover {
    async fn decide(&self, _request: &ApprovalRequest) -> Decision {
        Decision::deny_because("没有配置审批入口（executor 未安装 approver）")
    }
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Unix milliseconds. The ledger keeps whole seconds, but a live watcher renders
/// a ticking duration off the start instant, and whole seconds make every
/// sub-second call read as zero.
fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

#[cfg(test)]
mod tests;
