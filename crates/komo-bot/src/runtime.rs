use crate::compaction::Compactor;
use crate::learning_coordinator::{LearningCoordinator, LearningTrigger};
use komo_core::domain::{
    cancel::{CANCELLED_REPLY, CancelSignal, Cancelled, is_cancelled},
    events::{ToolEventSink, TurnEvent},
    llm::{DeltaSink, LlmClient, Step, TokenUsage, ToolOutcome},
    message::{Message, Role},
    repository::{MessageRepository, SessionEventRepository, SessionRepository},
    run::RecalledMemories,
    run::{Run, RunRepository, tool_digest, truncate},
    run_projection::{RunProjectionStore, project_runs, replay_floor},
    session::Session,
    session_event::{
        AssistantMessageEvent, MessageSource, SessionEvent, SessionEventKind, SurfacePlacement,
        TurnRecorder, TurnSuspendedEvent, UserMessageEvent, fold_turn_waits, root_of_chain,
    },
    wakeup::{Suspended, WakeupRegistration, WakeupRepository, is_suspended},
};
/// A turn whose opening the log never confirmed: settle folds the whole log
/// rather than a tail it cannot locate. Always correct, only slower.
const UNKNOWN_START: u64 = u64::MAX;

/// How many of a session's runs retention asks the ledger about. The watermark
/// it needs is the *oldest* unlearned run, and the sweep retires runs in
/// batches, so a session with more pending than this has its floor pinned by
/// one of them regardless.
const RETENTION_LEDGER_SCAN: usize = 500;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;

use tracing::{Instrument, info, info_span, warn};

use komo_services::tool_execution::{
    RunContext, SessionContext, SpinDetector, ToolExecutor, ToolTurnContext, TurnResultBudget,
    current_session, with_session,
};

/// Fed back to the model in place of tool results once the per-turn round
/// budget (`max_turns`) is exceeded, so it answers instead of calling more
/// tools. The turn then terminates regardless of the model's next move.
const BUDGET_REACHED_NOTE: &str = "Tool-call budget for this turn reached; do not call any \
     more tools. Reply to the user now using what you already have.";

/// Put in front of the model when it answers as if it had acted while the turn
/// made no tool call at all (see [`claims_completed_action`]). The failure it
/// answers is real: a non-thinking model asked to 打开热水器 replied "热水器已打开
/// ✅" having called nothing, and nothing in the runtime could tell.
const NUDGE_TEXT: &str = "Runtime check: your reply reports that an action was performed or a \
     state was observed, but this turn issued no tool call. Nothing about the user's devices, \
     files or external systems can be known without a tool call in this turn. If the action is \
     needed, perform it now with the appropriate tool and answer from its result. If you cannot \
     perform it, say plainly that it was not done. Do not restate the previous claim.";

/// Phrases that report a *completed* change to something outside the
/// conversation. Deliberately explicit and deliberately narrow: a generic 好的 /
/// "done" says nothing about external state, and nudging on one would interrupt
/// every ordinary reply.
const COMPLETION_CLAIMS_ZH: &[&str] = &[
    "已打开",
    "已开启",
    "已关闭",
    "已关掉",
    "已设置",
    "已设为",
    "已调到",
    "已调成",
    "已调整",
    "已发送",
    "已创建",
    "已删除",
    "已保存",
    "已更新",
    "已执行",
    "已重启",
    "已开好",
    "都开好了",
    "已完成设置",
];

/// The same claims in English, matched case-insensitively.
const COMPLETION_CLAIMS_EN: &[&str] = &[
    "turned on",
    "turned off",
    "has been set",
    "has been sent",
    "has been created",
    "has been deleted",
    "has been updated",
    "i've set",
    "i have set",
    "i've sent",
    "i've turned",
    "is now on",
    "is now off",
];

/// Whether a reply claims an action was carried out or an external state
/// observed — the thing a turn that called no tool cannot honestly say.
fn claims_completed_action(text: &str) -> bool {
    if COMPLETION_CLAIMS_ZH
        .iter()
        .any(|claim| text.contains(claim))
    {
        return true;
    }
    let lowered = text.to_lowercase();
    COMPLETION_CLAIMS_EN
        .iter()
        .any(|claim| lowered.contains(claim))
}

/// Sent to the user when the model ends a turn with no text at all (e.g. a final
/// round that is only tool calls the loop won't run, or an empty completion).
/// A chat channel rejects an empty message, so never hand one downstream.
const EMPTY_REPLY_FALLBACK: &str = "(我这次没能生成回复，请再说一次或换个说法。)";

/// Guard against handing an empty/whitespace-only reply to a channel (some
/// reject it outright); substitute a user-facing fallback.
fn non_empty(reply: String) -> String {
    if reply.trim().is_empty() {
        EMPTY_REPLY_FALLBACK.to_string()
    } else {
        reply
    }
}

pub struct AgentRuntime {
    pub llm: Arc<dyn LlmClient>,
    pub sessions: Arc<dyn SessionRepository>,
    pub messages: Arc<dyn MessageRepository>,
    /// The session's authoritative event log. Everything a turn does is
    /// appended here; `messages` is one projection of it.
    pub events: Arc<dyn SessionEventRepository>,
    /// Run ledger, for reading: the rows are a projection of the event log,
    /// so nothing here writes a turn or a step. See `domain/run.rs`, roadmap §7.
    pub runs: Arc<dyn RunRepository>,
    /// Where a turn's fold is committed as ledger rows — the one writer those
    /// tables have.
    pub projection: Arc<dyn RunProjectionStore>,
    /// Tool catalog the in-house loop dispatches against. komo (not rig) now
    /// owns the multi-step loop and hands each round of requested calls to the
    /// executor, which owns lookup/retry/ledger/cap. See `run_agent_loop`.
    pub tool_executor: ToolExecutor,
    /// Max tool-calling rounds per turn before the loop forces a final answer
    /// (config `max_turns`). The hard, loop-level budget — distinct from the
    /// executor's per-call fan-out cap.
    pub max_turns: usize,
    /// How many recent messages to load for the turn's agent loop (mirrors the
    /// LLM's `max_history_messages`; `0` = load the whole transcript). Keeps the
    /// per-turn hot path off a full-transcript read for long-lived chat
    /// sessions — the LLM windows again to the same bound, so this is loss-free.
    pub history_window: usize,
    /// Post-run learning goes through the shared coordinator (also driven by the
    /// gateway's scheduled sweep); `None` = this runtime never learns.
    pub learning: Option<Arc<LearningCoordinator>>,
    /// Summarises a conversation's oldest messages once the window has started
    /// dropping them. `None` = this runtime's long sessions keep the plain
    /// window, and what falls out of it is simply gone.
    pub compaction: Option<Arc<Compactor>>,
    /// Where a suspended turn's wait is registered, so a sweep comes back for
    /// it after this process is gone. `None` = nothing schedules a return, and
    /// only the startup re-check would find the turn.
    pub wakeups: Option<Arc<dyn WakeupRepository>>,
}

/// Records one turn's events into its session's log, disarming itself after the
/// first failed write — recording buys resumability, and a broken store must
/// cost exactly that, not per-round latency and not the turn.
struct RunRecorder {
    events: Arc<dyn SessionEventRepository>,
    session_id: String,
    turn_id: String,
    broken: AtomicBool,
}

impl RunRecorder {
    fn new(events: Arc<dyn SessionEventRepository>, session_id: &str, turn_id: &str) -> Arc<Self> {
        Arc::new(Self {
            events,
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            broken: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl TurnRecorder for RunRecorder {
    fn turn_id(&self) -> &str {
        &self.turn_id
    }

    async fn record(&self, kinds: Vec<SessionEventKind>) {
        if self.broken.load(Ordering::Relaxed) {
            return;
        }
        if let Err(error) = self.events.append(&self.session_id, kinds).await {
            warn!(%error, turn_id = %self.turn_id,
                "turn event write failed; recording disabled for this turn");
            self.broken.store(true, Ordering::Relaxed);
        }
    }

    async fn durable(&self) {
        if self.broken.load(Ordering::Relaxed) {
            return;
        }
        if let Err(error) = self.events.durable_flush(&self.session_id).await {
            warn!(%error, turn_id = %self.turn_id, "turn events are not durable");
        }
    }
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

impl AgentRuntime {
    pub async fn handle_input(
        &self,
        session_id: &str,
        user_input: String,
    ) -> anyhow::Result<String> {
        // Session-scoped tools (e.g. `todo`) read the turn's session from the
        // ambient context. The gateway dispatcher sets it (with a real reply
        // sink); the REPL calls us directly, so establish a detached context
        // here when none exists. Don't override an existing one — that would
        // drop the gateway's sink and break mid-turn approval.
        if current_session().is_none() {
            let ctx = SessionContext::detached(session_id);
            return with_session(ctx, self.run_turn(session_id, user_input)).await;
        }
        self.run_turn(session_id, user_input).await
    }

    /// Continue an interrupted run from its turn journal: rebuild the exact
    /// provider-level state the turn died with and drive the same agent loop
    /// forward — the tool rounds already paid for are replayed from the
    /// journal, not re-run. The continuation is its own ledger [`Run`], linked
    /// back through `resumed_from`.
    ///
    /// `Ok(None)` = this run *cannot* be continued (no journal store, no rows,
    /// or the transcript already ends in a reply); nothing was touched, and the
    /// caller falls back to the digest-primed fresh turn. `Err` = the
    /// continuation was attempted and genuinely failed.
    pub async fn resume_interrupted(&self, original: &Run) -> anyhow::Result<Option<String>> {
        let events = self.events.events(&original.session_id).await?;
        if !events
            .iter()
            .any(|e| e.turn_id_of_work() == Some(original.id.as_str()))
        {
            info!(run_id = %original.id, "the session log has nothing for this turn; falling back to digest resume");
            return Ok(None);
        }
        // A continuation appends an assistant reply with no new user message,
        // so the transcript must still end on the interrupted turn's user
        // message. Ending on anything else means the reply actually landed
        // (crash in the gap before the ledger closed), or the crash predated
        // the user message — either way a fresh turn is the right shape.
        // Checked here, before a ledger run is opened, so the refusal leaves
        // no failed-run residue; `turn_body` re-checks as a backstop.
        let ends_on_user = self
            .sessions
            .find_windowed(&original.session_id, self.history_window)
            .await?
            .and_then(|s| s.messages.last().map(|m| m.role == Role::User))
            .unwrap_or(false);
        if !ends_on_user {
            info!(run_id = %original.id,
                "transcript does not end on a user message; falling back to digest resume");
            return Ok(None);
        }

        let turn = self.run_ledgered(
            &original.session_id,
            Run::new_id(),
            TurnKind::Resume {
                events,
                turn_id: original.id.clone(),
            },
        );
        // Same ambient-context bridge as `handle_input`: session-scoped tools
        // and the approvers read the turn's session from the task-local.
        let reply = if current_session().is_none() {
            let ctx = SessionContext::detached(&original.session_id);
            with_session(ctx, turn).await?
        } else {
            turn.await?
        };
        Ok(Some(reply))
    }

    /// One turn = one id, carried by every event the turn appends and by the
    /// ledger row folded out of them. Runs the turn body under a `RunContext`
    /// (which orders the turn's tool steps) and a `run` tracing span, then
    /// settles the turn: project the ledger, then let the log do its upkeep.
    async fn run_turn(&self, session_id: &str, user_input: String) -> anyhow::Result<String> {
        self.run_ledgered(session_id, Run::new_id(), TurnKind::Fresh { user_input })
            .await
    }

    async fn run_ledgered(
        &self,
        session_id: &str,
        turn_id: String,
        kind: TurnKind,
    ) -> anyhow::Result<String> {
        let span = info_span!("run", run_id = %turn_id, session = %session_id);
        let started = std::time::Instant::now();
        let ctx = RunContext::new(turn_id.clone());
        // Where this turn starts in the log, filled in as it opens. It is what
        // lets the settle below fold the turn's own tail instead of the whole
        // conversation; `UNKNOWN_START` means "read it all", which is always
        // correct and only slower.
        let opened_at = AtomicU64::new(UNKNOWN_START);

        let outcome = self
            .turn_body(session_id, kind, ctx, &opened_at)
            .instrument(span)
            .await;

        // The turn's own account of itself is the log's, not this function's:
        // every field the ledger row used to be assigned here — the reply, what
        // it cost, which memories shaped it, how it ended — is already an event
        // that `run_projection` folds. What is left is saying so out loud.
        // What the turn cost, alongside how it ended. A turn that failed or was
        // cancelled reports only what is known — its duration; inventing zeros
        // for the rest would read as a turn that spent nothing.
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &outcome {
            Ok(report) => info!(
                run_id = %turn_id,
                rounds = report.rounds,
                tool_calls = report.tool_calls,
                tokens_in = report.usage.input,
                tokens_out = report.usage.output,
                tokens_cached = report.usage.cached_input,
                reply_chars = report.reply.chars().count(),
                elapsed_ms,
                "run done"
            ),
            // Cancelled, not broken, and deliberately not resumable: there is
            // nothing to resume, the user asked it to stop.
            Err(error) if is_cancelled(error) => {
                info!(run_id = %turn_id, elapsed_ms, "run cancelled")
            }
            Err(error) => warn!(run_id = %turn_id, elapsed_ms, %error, "run failed"),
        }
        let outcome = outcome.map(|report| report.reply);
        self.settle_turn(session_id, opened_at.load(Ordering::Relaxed))
            .await;

        // Learning, detached from the reply path and dispatched **after** the
        // ledger closed. It reads this run back as an episode — status, steps
        // and all — so starting it from inside the turn would have it assemble a
        // run whose outcome had not been written yet. Whether the interval is
        // due, which episodes the extractor sees, and the watermark are all the
        // coordinator's knowledge; the runtime only reports that a run ended.
        if let Some(learning) = &self.learning {
            let learning = learning.clone();
            let run_id = turn_id.clone();
            tokio::spawn(async move {
                match learning.run(LearningTrigger::AfterRun { run_id }).await {
                    Ok(report) if !report.is_empty() => {
                        info!(?report, "self-improvement learning")
                    }
                    Ok(_) => {}
                    Err(error) => warn!(%error, "learning failed (non-fatal)"),
                }
            });
        }

        outcome
    }

    /// The turn's actual work: persist the user message (fresh turns), drive
    /// the agent loop (komo owns it — model round-trip, execute requested
    /// tools, feed results back, repeat), persist the reply, and kick off the
    /// periodic reviewer. A resumed turn differs only at the edges: its user
    /// message is already in the transcript, and its driver reopens mid-loop
    /// from the journal instead of starting fresh.
    async fn turn_body(
        &self,
        session_id: &str,
        kind: TurnKind,
        run: RunContext,
        opened_at: &AtomicU64,
    ) -> anyhow::Result<TurnReport> {
        // Load only the recent window for the agent loop — the LLM windows the
        // history to the same bound anyway, so a long-lived chat session no
        // longer deserializes its whole transcript every turn. The reviewer
        // (below) still gets the full transcript, on the turns it actually runs.
        let mut session = match self
            .sessions
            .find_windowed(session_id, self.history_window)
            .await?
        {
            Some(s) => s,
            None => {
                // First turn on this id: the record inherits what is driving
                // the turn, so a sweep's session is marked one at creation.
                // That mark is what later decides how it is titled, whether the
                // session list shows it, and whether the learning pass may
                // extract from it — all of which used to be read off a prefix
                // in the id.
                let origin = current_session().map(|c| c.origin).unwrap_or_default();
                let s = Session::new(session_id).with_origin(origin);
                self.sessions.save(&s).await?;
                s
            }
        };

        // The turn's opening line in the log. Shape only — how it was started,
        // over which platform, how much was said and how much history it was
        // given — never the message itself or the chat it came from.
        let (kind_label, prompt_chars) = match &kind {
            TurnKind::Fresh { user_input } => ("fresh", user_input.chars().count()),
            TurnKind::Resume { .. } => ("resume", 0),
        };
        info!(
            origin = ?session.origin,
            channel = session
                .channel
                .as_ref()
                .map(|c| c.platform.as_str())
                .unwrap_or("none"),
            kind = kind_label,
            prompt_chars,
            history_messages = session.messages.len(),
            "turn started"
        );

        let resume_entries = match kind {
            TurnKind::Fresh { user_input } => {
                let user_msg = Message::user(&user_input);
                let opening = self
                    .record(
                        session_id,
                        vec![
                            SessionEventKind::TurnStarted {
                                turn_id: run.run_id.clone(),
                                resumed_from: None,
                            },
                            SessionEventKind::UserMessage(UserMessageEvent {
                                turn_id: run.run_id.clone(),
                                content: user_input.clone(),
                                source: MessageSource::User,
                                surface: SurfacePlacement::append(),
                            }),
                        ],
                    )
                    .await;
                if let Some(first) = opening.first().map(|event| event.seq) {
                    opened_at.store(first, Ordering::Relaxed);
                }
                self.open_in_ledger(session_id, &opening).await;
                session.messages.push(user_msg);
                None
            }
            TurnKind::Resume { events, turn_id } => {
                // A continuation appends an assistant reply without a new user
                // message, so the transcript must still end on the interrupted
                // turn's user message. Ending on an assistant means the reply
                // actually landed (crash in the gap before the ledger closed) —
                // nothing to resume.
                anyhow::ensure!(
                    session.messages.last().map(|m| m.role == Role::User) == Some(true),
                    "transcript already ends in a reply — nothing to resume"
                );
                // A continuation is its own turn in the log too, linked back to
                // the one it picks up. Without this the log has no record that
                // the attempt happened at all — the interrupted turn's events
                // are all it would show.
                let opening = self
                    .record(
                        session_id,
                        vec![SessionEventKind::TurnStarted {
                            turn_id: run.run_id.clone(),
                            resumed_from: Some(turn_id.clone()),
                        }],
                    )
                    .await;
                // Deliberately not recorded: a continuation's settle has to
                // re-commit the turns it claimed, and those are earlier turns of
                // their own. A resume is rare; a full fold there is the cheap
                // way to keep the claim honest.
                self.open_in_ledger(session_id, &opening).await;
                // What the chain has waited for, and what ended the wait that
                // brought this attempt back. Read here, where the events are
                // already in hand, and carried on the run: the call that
                // stopped is about to be re-dispatched and has to recognise its
                // own wake instead of registering a second one.
                run.resumed_with(fold_turn_waits(&events, &turn_id));
                // And which chain this attempt belongs to. A call's scratch is
                // keyed by the chain's root, not by the turn running it: the
                // whole point is that a re-dispatched call finds what its
                // earlier attempt left behind, and every attempt has an id of
                // its own.
                run.resumed_from_root(root_of_chain(&events, &turn_id));
                Some((events, turn_id))
            }
        };
        let is_fresh = resume_entries.is_none();

        // Keep a handle on the run to read the tool-step count after the loop (the
        // counter is shared via `Arc`) and to fetch the steps themselves.
        let probe = run.clone();
        let TurnOutcome {
            reply,
            usage,
            memories,
            rounds,
            interjections,
        } = match self.run_agent_loop(&session, run, resume_entries).await {
            Ok(outcome) => outcome,
            // Stopped to wait for something outside itself. **Not** a failure
            // and not an answer: the turn gives up its session slot and comes
            // back when the wake arrives.
            //
            // Deliberately no assistant message — a suspended turn has not
            // answered, and the surface has to still end on the user message
            // for the continuation to be a continuation rather than a second
            // question. The prompt the user sees was delivered by whoever asked
            // for the approval, not by this transcript.
            Err(error) if is_suspended(&error) => {
                let pending = probe
                    .suspension()
                    .expect("a suspended turn carries what it is waiting for");
                let expires_at = pending.expires_at.or_else(|| {
                    komo_core::domain::wakeup::default_expiry_secs(&pending.wakeup)
                        .map(|secs| now() + secs)
                });
                // Durable before the wait is registered, and before the caller
                // is told: a registration for a suspension the log does not
                // hold would wake a turn that never stopped.
                self.record_durable(
                    session_id,
                    vec![SessionEventKind::TurnSuspended(TurnSuspendedEvent {
                        turn_id: probe.run_id.clone(),
                        wakeup: pending.wakeup.clone(),
                        call_id: pending.call_id.clone(),
                        summary: pending.summary.clone(),
                        expires_at,
                    })],
                )
                .await;
                self.register_wait(session_id, &probe.run_id, &pending, expires_at)
                    .await;
                info!(
                    run_id = %probe.run_id,
                    summary = %pending.summary,
                    "run suspended, waiting"
                );
                return Err(error);
            }
            Err(error) => {
                // The turn failed *after* the user message was persisted. Persist
                // an assistant turn too, so the transcript stays user/assistant-
                // alternating: the next turn's history would otherwise hold two
                // consecutive user messages, which several providers reject (and
                // the history-window repair only fixes a *leading* assistant
                // message, not an interior double-user). The stored note is
                // concise — the full error lives in the run ledger.
                //
                // A user cancel is not a failure, so it gets its own note: the
                // transcript should read as "I stopped this", not as an error.
                // A cancel that landed before the turn did anything is recorded
                // as such instead of leaving a tombstone: the transcript then
                // reads as if the turn never happened, while the log still
                // knows it did (the surface fold in `domain::session_event`).
                // "Did anything" means a tool ran — the only way a cancelled
                // turn can have effects worth remembering. Without this, a user
                // who sends a message and immediately stops it is left with a
                // "(已取消)" pair that every later turn replays. The run ledger
                // still records the cancelled run: the transcript is the
                // conversation, the ledger is the audit trail.
                // (Never on a resume: the trailing user message there belongs
                // to the interrupted turn, not to this continuation.)
                if is_fresh && is_cancelled(&error) && probe.steps_count() == 0 {
                    self.record_durable(
                        session_id,
                        vec![SessionEventKind::TurnCancelled {
                            turn_id: probe.run_id.clone(),
                            pristine: true,
                        }],
                    )
                    .await;
                    return Err(error);
                }
                let note = if is_cancelled(&error) {
                    CANCELLED_REPLY.to_string()
                } else {
                    format!(
                        "(上一条消息处理失败，未能完成回复：{})",
                        truncate(&format!("{error:#}"), 400)
                    )
                };
                let ended = if is_cancelled(&error) {
                    SessionEventKind::TurnCancelled {
                        turn_id: probe.run_id.clone(),
                        pristine: false,
                    }
                } else {
                    SessionEventKind::TurnFailed {
                        turn_id: probe.run_id.clone(),
                        error: truncate(&format!("{error:#}"), 400),
                    }
                };
                self.record_durable(
                    session_id,
                    vec![
                        SessionEventKind::AssistantMessage(AssistantMessageEvent {
                            turn_id: probe.run_id.clone(),
                            content: note,
                            tool_note: String::new(),
                            surface: SurfacePlacement::append(),
                        }),
                        ended,
                    ],
                )
                .await;
                return Err(error);
            }
        };

        // Anything the user said mid-turn is folded into *this turn's* stored
        // user message rather than appended as its own. Two consecutive user
        // messages is exactly what the transcript may not contain (several
        // providers reject it), and both halves really are one user's input for
        // one turn — the same merge a follow-up gets when it waits for the next
        // turn instead. Best-effort: the model already acted on them, so a
        // failure here costs the *next* turn context, not this one's answer.
        if !interjections.is_empty() {
            self.record(
                session_id,
                vec![SessionEventKind::UserMessage(UserMessageEvent {
                    turn_id: probe.run_id.clone(),
                    content: interjections.join("\n"),
                    source: MessageSource::Injected,
                    surface: SurfacePlacement::append(),
                })],
            )
            .await;
        }

        // Fold this turn's tool activity into a note on the assistant message, so
        // the *next* turn knows tools ran, what they found, and where an
        // over-limit output was kept. Without it the transcript carries only
        // user/assistant text: a follow-up question about something a tool just
        // read has to re-run the tool or be answered from nothing. Taken from
        // the turn's own steps — already redacted and truncated to exactly what
        // the log records — because the ledger's rows are a projection now and
        // this turn's are not committed until it closes.
        let tool_note = match probe.steps_count() {
            0 => String::new(),
            _ => tool_digest(&probe.steps()),
        };

        let assistant_msg = Message::assistant(&reply).with_tool_note(&tool_note);
        let mut closing = vec![SessionEventKind::AssistantMessage(AssistantMessageEvent {
            turn_id: probe.run_id.clone(),
            content: reply.clone(),
            tool_note,
            surface: SurfacePlacement::append(),
        })];
        // Which memories shaped this answer. Inside the turn's own closing batch
        // rather than after it: `turn/completed` is what ends a turn, and a
        // segment is sealed on that boundary, so an event recorded past it would
        // land in the next segment and outlive the turn it describes.
        if !memories.is_empty() {
            closing.push(SessionEventKind::TurnMemories {
                turn_id: probe.run_id.clone(),
                memories: memories.clone(),
            });
        }
        closing.push(SessionEventKind::TurnCompleted {
            turn_id: probe.run_id.clone(),
        });
        self.record_durable(session_id, closing).await;
        session.messages.push(assistant_msg);

        Ok(TurnReport {
            reply,
            usage,
            rounds,
            tool_calls: probe.steps_count(),
        })
    }

    /// Register the wait a suspended turn is holding, so something comes back
    /// for it after this process is gone.
    ///
    /// Best-effort with a loud failure: a suspension the scheduler never learns
    /// about is a turn nobody wakes — but the log already says the turn is
    /// waiting, and the startup re-check (`reregister_suspended_turns`) reads it
    /// back from there, which is why this can fail without stranding anything
    /// permanently.
    async fn register_wait(
        &self,
        session_id: &str,
        turn_id: &str,
        pending: &komo_core::domain::context::PendingSuspension,
        expires_at: Option<i64>,
    ) {
        let Some(wakeups) = &self.wakeups else {
            warn!(
                turn_id,
                "no wakeup store wired: this turn waits until the next startup check"
            );
            return;
        };
        let registration = WakeupRegistration::new(session_id, pending.wakeup.clone(), now())
            .continuing(turn_id)
            .expiring_at(expires_at)
            // The job's grants ride across the wait: a routine that stopped to
            // ask still has to be able to act when it comes back.
            .with_grants(
                komo_services::tool_execution::current_job_grants()
                    .iter()
                    .map(komo_core::domain::policy::RuleSpec::from_rule)
                    .collect(),
            );
        if let Err(error) = wakeups.save(&registration).await {
            warn!(%error, turn_id, "failed to register a suspended turn's wait");
        }
    }

    /// Append this turn's events, best-effort, answering with them as the log
    /// stamped them. A record that fails to land must never fail the turn it
    /// describes — an empty answer is that failure.
    async fn record(&self, session_id: &str, kinds: Vec<SessionEventKind>) -> Vec<SessionEvent> {
        match self.events.append(session_id, kinds).await {
            Ok(appended) => appended,
            Err(error) => {
                warn!(%error, "failed to append session events (non-fatal)");
                Vec::new()
            }
        }
    }

    /// The turn's row, the moment the log says the turn exists.
    ///
    /// A crash leaves it `running`, which is exactly what the startup
    /// reconciler looks for: an interrupted turn has to be in the ledger to be
    /// listed, inspected or resumed, and after the flip to a projection nothing
    /// else would put it there until the turn was over. Folded from the opening
    /// events themselves — the only commit that needs no read of the log,
    /// because its caller has just written everything the fold needs.
    async fn open_in_ledger(&self, session_id: &str, opening: &[SessionEvent]) {
        let Some(through) = opening.last().map(|event| event.seq) else {
            return;
        };
        let runs = project_runs(session_id, opening);
        if let Err(error) = self.projection.commit(session_id, &runs, through).await {
            warn!(%error, "failed to open the turn in the ledger (non-fatal)");
        }
        // A continuation's opening is what takes the badge off: the wait ended
        // when the work restarted, not when it next finishes.
        self.commit_awaiting(session_id, opening).await;
    }

    /// Fold this turn's events onto the session's cached wait, best-effort.
    ///
    /// Rides on the reads the ledger commit already did — a suspension and its
    /// wake are events in the same tail — so a session list can say which
    /// conversations are stopped on someone without folding every transcript.
    async fn commit_awaiting(&self, session_id: &str, events: &[SessionEvent]) {
        if let Err(error) = self.sessions.commit_awaiting(session_id, events).await {
            warn!(%error, "failed to project the session's wait (non-fatal)");
        }
    }

    /// Append and make durable. Every way a turn can end goes through here:
    /// past this point the turn is over, so whatever it recorded has to have
    /// survived — including the ways that end badly. A failed turn whose events
    /// were only buffered reads afterwards as a turn that never happened.
    async fn record_durable(&self, session_id: &str, kinds: Vec<SessionEventKind>) {
        let _ = self.record(session_id, kinds).await;
        if let Err(error) = self.events.durable_flush(session_id).await {
            warn!(%error, "failed to make a finished turn durable (non-fatal)");
            // The log still holds unwritten events, and its upkeep is defined
            // over what has landed. Skipped rather than attempted and refused.
            return;
        }
    }

    /// Close the turn out: commit the ledger, then let the log do its upkeep.
    ///
    /// **One read of the log serves both.** The fold *is* the ledger — the rows
    /// `run list` reads are committed from it here — and the
    /// same fold says which turns retention may not cut, so reading the log
    /// twice per turn would be paying twice for one answer.
    ///
    /// Ordered after the turn's terminal event and before learning is
    /// dispatched: an episode is assembled from these rows, and the retention
    /// rule that protects the turn that just ended reads it as *unlearned*,
    /// which it cannot be until it is a row at all.
    async fn settle_turn(&self, session_id: &str, from_seq: u64) {
        let tail = match from_seq {
            // A turn whose start the log never confirmed, and every resume.
            UNKNOWN_START => self.events.events(session_id).await,
            from => self.events.events_from(session_id, from).await,
        };
        let events = match tail {
            Ok(events) => events,
            Err(error) => {
                warn!(%error, "failed to read the session log to settle the turn (non-fatal)");
                return;
            }
        };
        let runs = project_runs(session_id, &events);
        let through = events.last().map(|event| event.seq).unwrap_or(0);
        if let Err(error) = self.projection.commit(session_id, &runs, through).await {
            warn!(%error, "failed to project the run ledger (non-fatal)");
        }
        self.commit_awaiting(session_id, &events).await;
        // Before the boundary, so the checkpoint written there already holds the
        // summary — and inside this turn's session slot, which is what keeps two
        // compactions from planning against the same surface.
        if let Some(compaction) = &self.compaction
            && let Some(turn) = runs.last().map(|projected| projected.run.id.clone())
        {
            compaction.compact_if_long(session_id, &turn).await;
        }
        match self.events.turn_boundary(session_id).await {
            // Only a roll can put the log over its budget, so the cut is
            // considered only then.
            Ok(true) => self.retain(session_id).await,
            Ok(false) => {}
            Err(error) => warn!(%error, "session log upkeep failed at a turn boundary (non-fatal)"),
        }
    }

    /// Cut the session's log back toward its budget, keeping every turn that is
    /// still resumable or still unlearned.
    ///
    /// The floor is over every turn the session still holds, so this is the one
    /// place that reads the whole log — and a roll is the only moment the
    /// retained size can cross budget, so it is paid per *segment*, not per
    /// turn. `recoverable` comes off the fold; `learned` is the sweep's
    /// watermark, read from the rows it advances. A turn nobody has finished
    /// with outranks the space it costs: finding no safe cut is a normal
    /// answer, and leaves the session over budget.
    async fn retain(&self, session_id: &str) {
        let events = match self.events.events(session_id).await {
            Ok(events) => events,
            Err(error) => {
                warn!(%error, "failed to read the session log for retention (non-fatal)");
                return;
            }
        };
        let runs = &project_runs(session_id, &events);
        let unlearned: std::collections::HashSet<String> = self
            .runs
            .unlearned(Some(session_id), RETENTION_LEDGER_SCAN)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|run| run.id)
            .collect();
        let keep_from = runs
            .iter()
            // A turn that has not finished keeps its own log whatever else is
            // true of it: `recoverable` covers the one that crashed, and a
            // *suspended* turn is not recoverable — its return is scheduled —
            // but cutting its rounds away would leave the continuation
            // replaying nothing.
            .filter(|projected| {
                !projected.run.status.is_terminal() || unlearned.contains(&projected.run.id)
            })
            // Its own start is not enough: a resumable turn replays every
            // earlier attempt at it, and those are turns of their own in the log.
            .map(|projected| replay_floor(runs, projected))
            .min()
            .unwrap_or(u64::MAX);

        match self.events.retain(session_id, keep_from).await {
            Ok(Some(through)) => {
                info!(session_id, through, "cut the session log back into budget")
            }
            Ok(None) => {}
            Err(error) => warn!(%error, "failed to cut the session log (non-fatal)"),
        }
    }

    /// Await `work`, unless the turn is cancelled first.
    ///
    /// `Err(Cancelled)` rather than an `Option` so the loop's control points read
    /// as one `?` each: a cancel propagates out of the loop like any other turn
    /// failure, and every layer above tells it apart by downcasting.
    async fn until_cancelled<T>(
        cancel: Option<&Arc<dyn CancelSignal>>,
        work: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let Some(cancel) = cancel else {
            return work.await;
        };
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        tokio::select! {
            // Bias the work: when both are ready, finishing beats discarding.
            biased;
            done = work => done,
            () = cancel.cancelled() => Err(Cancelled.into()),
        }
    }

    /// komo's own tool-calling loop (roadmap §7 — the loop lives here, not in
    /// rig, so control points can sit between rounds). Drive the model a round
    /// at a time: a [`Step::Final`] ends the turn; [`Step::ToolCalls`] go to the
    /// tool executor as one round (it owns lookup, retry, the per-call budget,
    /// the ledger, and the result cap) and the outcomes are threaded back. Once
    /// the per-turn *round* budget is exceeded, feed [`BUDGET_REACHED_NOTE`]
    /// back in place of results and force a final answer.
    async fn run_agent_loop(
        &self,
        session: &Session,
        run: RunContext,
        resume: Option<(Vec<SessionEvent>, String)>,
    ) -> anyhow::Result<TurnOutcome> {
        // Pin the tool catalog for this turn. The model is handed one set of
        // schemas below; a plugin mounting or unmounting mid-turn must not
        // change what the loop then dispatches against, or a call the model was
        // invited to make would answer "unknown tool" a round later. The
        // mutation is not lost — the next turn pins the new set.
        let tools = self.tool_executor.pin();

        // This turn's event recorder, bound to its ledger run — the loop and
        // the driver stay run-id-free. A run *is* a turn in komo, so the run id
        // is the turn id the events carry.
        let recorder: Option<Arc<dyn TurnRecorder>> = Some({
            RunRecorder::new(self.events.clone(), &session.id, &run.run_id) as Arc<dyn TurnRecorder>
        });
        // The executor gets the turn's context explicitly: the run handle this
        // turn opened, and the session established by the dispatcher / api /
        // handle_input (read once here — the one ambient-to-explicit bridge).
        let context = ToolTurnContext {
            // The stored session is the authority on which correspondent this
            // conversation answers; an ingress only knows how *it* was
            // addressed. Filling the address in here — where the record is
            // already loaded — means every ingress gets the same answer with no
            // plumbing of its own, and a client free to name a session id can
            // never name the channel its turn is evaluated against.
            session: current_session()
                .unwrap_or_else(|| SessionContext::detached(&session.id))
                .with_channel(session.channel.clone()),
            run: Some(run),
            // Bound the turn's cumulative tool output (0 = unlimited), so a long
            // tool chain can't quietly overflow the context window.
            budget: TurnResultBudget::new(tools.turn_result_cap()),
            // Fresh per turn: a repeat only means anything within the one
            // sequence of calls that is trying to accomplish one thing.
            spin: SpinDetector::default(),
        };
        // Cancellation, if this caller offers a stop. Raced against each await
        // rather than only checked between rounds: the model round-trip is the
        // longest wait in a turn and the likeliest thing a user interrupts.
        let cancel = context.session.cancel.clone();
        let cancel = cancel.as_ref();

        // Stream the model's output to whoever is watching. Only built when a
        // watcher is actually attached: an unwatched turn (every chat channel,
        // every sweep) hands the backend `None` and pays nothing per chunk.
        let deltas: Option<Arc<dyn DeltaSink>> = context
            .session
            .event_sink
            .clone()
            .map(|sink| Arc::new(StreamingDeltas(sink)) as Arc<dyn DeltaSink>);

        let mut driver = match &resume {
            None => self.llm.begin_turn(session, deltas, recorder).await?,
            Some((events, turn_id)) => {
                self.llm
                    .resume_turn(session, events, turn_id, deltas, recorder)
                    .await?
            }
        };
        let mut step = Self::until_cancelled(cancel, driver.first()).await?;
        let mut rounds = 0usize;
        // Once per turn: the nudge is a correction, and a model that repeats the
        // claim after being told is not going to be talked out of it.
        let mut nudged = false;
        // Model round-trips, which `rounds` (the tool-round budget) is not: the
        // completion just awaited is round 1, and every `driver.step` adds one.
        let mut model_rounds = 1usize;
        // The model's most recent narration alongside its tool calls. Kept so the
        // budget cutoff below can answer in the model's own words instead of a
        // canned line — by then it has usually said what it was doing.
        let mut narration = String::new();
        // What the user said mid-turn, in order, for the caller to fold into
        // the transcript once the turn ends.
        let mut interjections: Vec<String> = Vec::new();

        let reply = loop {
            match step {
                Step::Final(text) => {
                    // The model answered as if it had acted, having called
                    // nothing — the incident this guard exists for. `rounds`
                    // covers this loop and `steps_count` the whole turn, so a
                    // continuation that already ran tools before it was
                    // suspended is not nudged for the answer it comes back
                    // with. A turn with no tools at all (every aux runtime) has
                    // nothing to have called.
                    if !nudged
                        && rounds == 0
                        && context
                            .run
                            .as_ref()
                            .is_none_or(|run| run.steps_count() == 0)
                        && !tools.snapshot().is_empty()
                        && claims_completed_action(&text)
                    {
                        warn!(
                            reply_chars = text.len(),
                            "reply claims an action but the turn made no tool call; nudging once"
                        );
                        nudged = true;
                        match Self::until_cancelled(cancel, driver.nudge(NUDGE_TEXT.to_string()))
                            .await?
                        {
                            Some(next) => {
                                model_rounds += 1;
                                step = next;
                                continue;
                            }
                            // This driver cannot be nudged; keep the reply.
                            None => break non_empty(text),
                        }
                    }
                    break non_empty(text);
                }
                Step::ToolCalls { calls, text } => {
                    rounds += 1;
                    let over_budget = rounds > self.max_turns;

                    // Text the model wrote in the same breath as its tool calls.
                    // It never reaches a chat channel (the turn hasn't answered
                    // yet), but a watching client can render it, which is the
                    // only view komo offers into the model's reasoning mid-turn.
                    if !text.trim().is_empty() {
                        if let Some(sink) = &context.session.event_sink {
                            sink.emit(TurnEvent::AssistantText { text: text.clone() });
                        }
                        narration = text;
                    }

                    let results: Vec<ToolOutcome> = if over_budget {
                        calls
                            .iter()
                            .map(|call| ToolOutcome {
                                id: call.id.clone(),
                                call_id: call.call_id.clone(),
                                content: BUDGET_REACHED_NOTE.to_string(),
                                structured: serde_json::Value::Null,
                            })
                            .collect()
                    } else {
                        // One round, delegated whole: the executor runs the
                        // calls concurrently (order-preserving) and maps tool
                        // errors / unknown names into outcome content the model
                        // can recover from — only a driver/LLM error aborts the
                        // turn. A cancel here abandons the round's results; the
                        // calls themselves are spawned and still finish (see
                        // `domain::cancel`).
                        Self::until_cancelled(cancel, async {
                            Ok(tools.execute_round(&calls, &context).await)
                        })
                        .await?
                    };

                    // A call that stopped to wait — for an approval, or because
                    // the tool asked to be woken — ends the turn here.
                    // Checked between rounds rather than inside the round: the
                    // round's other calls have already run and settled, and
                    // their results are on record for the continuation to
                    // replay — what must not happen is another provider request
                    // carrying a result for a call that never ran.
                    if let Some(run) = &context.run
                        && run.suspension().is_some()
                    {
                        return Err(Suspended.into());
                    }

                    // Anything the user said while that round ran joins this
                    // step instead of waiting for a whole new turn — a
                    // correction is only worth anything before the agent
                    // finishes going the wrong way. Drained here, between
                    // rounds, so the model sees it at the one point it can
                    // change course. Kept for the transcript too: the next
                    // turn has to know what was said.
                    let said = context
                        .session
                        .interject
                        .as_ref()
                        .map(|source| source.take())
                        .unwrap_or_default();
                    if !said.is_empty() {
                        info!(count = said.len(), "user interjected mid-turn");
                        interjections.extend(said.iter().cloned());
                    }

                    let interjected = if said.is_empty() {
                        None
                    } else {
                        Some(said.join("\n"))
                    };

                    // The model kept re-issuing one call even after the executor
                    // refused it (see `SpinDetector`). The refusals went back as
                    // well-formed results, so it gets this round to answer with
                    // what it has — but the turn ends either way rather than
                    // spending the rest of its rounds on the same call.
                    let spun = context.spin.should_stop();
                    let next =
                        Self::until_cancelled(cancel, driver.step(results, interjected)).await?;
                    model_rounds += 1;
                    // Over budget, the note went back as well-formed tool results;
                    // terminate now no matter what the model did with it.
                    step = if over_budget || spun {
                        let stopped = if spun { SPUN_STOP } else { BUDGET_STOP };
                        break non_empty(match next {
                            Step::Final(text) => text,
                            // It asked for more tools instead of answering. Its
                            // own last narration is a better account of where the
                            // turn got to than a canned apology, so prefer it.
                            Step::ToolCalls { text, .. } => stop_reply(stopped, &text, &narration),
                        });
                    } else {
                        next
                    };
                }
            }
        };
        Ok(TurnOutcome {
            reply,
            rounds: model_rounds,
            usage: driver.usage(),
            memories: driver.memories(),
            interjections,
        })
    }
}

/// Forwards the provider's streamed output onto the turn's event sink.
///
/// The two sinks exist for different reasons and are deliberately not merged:
/// [`ToolEventSink`] is the fire-and-forget channel every watcher already reads
/// (tool starts and finishes travel it), while [`DeltaSink`] is the seam the LLM
/// backend writes into and knows nothing about sessions. This is the one adapter
/// between them.
struct StreamingDeltas(Arc<dyn ToolEventSink>);

impl DeltaSink for StreamingDeltas {
    fn text(&self, delta: &str) {
        self.0.emit(TurnEvent::AssistantDelta {
            text: delta.to_string(),
        });
    }

    fn reasoning(&self, delta: &str) {
        self.0.emit(TurnEvent::ReasoningDelta {
            text: delta.to_string(),
        });
    }
}

/// What kind of turn [`AgentRuntime::turn_body`] is driving.
enum TurnKind {
    /// An ordinary user turn: persist the input, open a fresh driver.
    Fresh { user_input: String },
    /// A continuation of an interrupted turn: the user message is already in
    /// the conversation, and the driver reopens from the session's own events.
    Resume {
        events: Vec<SessionEvent>,
        turn_id: String,
    },
}

/// What a finished turn amounted to, for the caller that has to say so out
/// loud. A tuple grew a field per question the log could not answer; this is
/// the same data with the fields named.
struct TurnReport {
    reply: String,
    usage: TokenUsage,
    /// Model round-trips this turn spent (the first completion is round 1).
    rounds: usize,
    /// Tool steps the turn claimed — settled and in-flight alike.
    tool_calls: i64,
}

/// What one pass of the agent loop produced.
struct TurnOutcome {
    reply: String,
    usage: TokenUsage,
    /// Model round-trips the loop drove, counted here because the driver trait
    /// does not expose its own count.
    rounds: usize,
    /// The memories prompt assembly injected, on its way to the ledger — the
    /// same trip `usage` makes, for the same reason: both are facts about the
    /// turn that only the layer below knows.
    memories: RecalledMemories,
    /// User messages that arrived mid-turn and were folded into it. The loop
    /// already showed them to the model; the caller still has to get them into
    /// the transcript, or the next turn has no idea they were ever said.
    interjections: Vec<String>,
}

/// Told to the user when the round budget ran out.
const BUDGET_STOP: &str = "(Reached the tool-call limit for this turn; \
     answering with what I have.)";
/// Told to the user when the turn was ended for repeating one call — see
/// `SpinDetector`. Named rather than folded into [`BUDGET_STOP`] because the
/// two situations call for different next moves from the user: a budget stop
/// invites "keep going", a spin stop invites rephrasing.
const SPUN_STOP: &str = "(I was repeating the same step without progress, so I \
     stopped there. Answering with what I have — try rephrasing if this misses \
     what you needed.)";

/// The reply for a turn something cut short. The model's own words (this round's
/// text, else the last narration it managed) beat a canned line — but the user
/// still has to be told the turn stopped early rather than finished, and why.
fn stop_reply(stopped: &str, current: &str, narration: &str) -> String {
    let said = [current, narration]
        .into_iter()
        .map(str::trim)
        .find(|t| !t.is_empty());
    match said {
        Some(text) => format!("{text}\n\n{stopped}"),
        None => stopped.to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests;
