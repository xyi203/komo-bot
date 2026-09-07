//! Full-screen chat TUI — `komo chat`'s interface (a terminal is required;
//! scripted access goes through the gateway's api channel instead). A ratatui
//! front end over the **gateway**, which is the only process that opens komo's
//! state: turns run server-side over
//! [`GatewayClient::chat_streaming`] (trusted loopback, so a side-effecting
//! tool the host operator asked for is not prompted for).
//!
//! The first frame paints before the connection exists: reaching the gateway —
//! and starting one when none answers — runs on a background task ([`Boot`])
//! while the event loop is already live. Drafting works immediately; one
//! submission queues (`in_flight` already enforces one turn at a time) and
//! dispatches the moment the backend lands.
//!
//! Layout: scrollable transcript · status line (spinner while a turn runs) ·
//! bordered input box. Enter sends; Shift/Alt-Enter or Ctrl-J insert a newline,
//! and the box grows with the draft. Pastes follow grok build (see
//! `tui/paste.rs`): a big one folds to a `[Pasted: N lines]` chip that edits and
//! deletes as one object while the draft keeps the text verbatim, and a burst of
//! keystrokes from a terminal without bracketed paste is coalesced back into one
//! paste instead of submitting at its first newline. As the agent works, each
//! tool call renders as a live activity line (`⚙ shell …` → `✓`/`✗` with a
//! result preview) parsed from the gateway's SSE stream; the running tool also
//! shows in the status line.
//!
//! **A stopped turn is answered out-of-band**, exactly as the desktop app does
//! it: the loop polls `GET /api/interactions/{session}` and renders an approval
//! as the `y`/`s`/`n` modal and an `ask_user` question as agent speech, then
//! posts the answer to `/approval` or `/answer`. The continuation runs
//! server-side, so its reply arrives by following the session's transcript tail
//! rather than on a stream this UI holds — which is also how a wait another
//! ingress parked (a `/approve` sent to Telegram) becomes answerable here.
//!
//! Logs: `main.rs::init_tracing` routes tracing to `~/.komo/logs/chat-tui.log`
//! when it detects the TUI will run — stderr writes would tear the alternate
//! screen. `ratatui::init` installs a panic hook that restores the terminal.

mod app;
mod markdown;
mod paste;
mod ui;

use komo_core::domain::awaiting::Awaiting;
use komo_core::domain::cancel::{CANCELLED_REPLY, is_cancelled};
use komo_core::domain::wakeup::{SUSPENDED_REPLY, is_suspended};
use std::{io, path::Path, path::PathBuf, sync::Arc, time::Duration};

use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::supports_keyboard_enhancement,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::{
    domain::{
        events::TurnEvent,
        message::{Message, Role as MessageRole},
    },
    infra::gateway_client::{GatewayClient, folder_workspace_id},
};

use app::{Action, App, ApprovalPrompt, Role};

/// How often the session's pending approval / question is re-read. A chat
/// client's own poll, like the desktop app's: slow enough to cost nothing on a
/// loopback socket, fast enough that a prompt does not feel lost.
const INTERACTION_POLL: Duration = Duration::from_secs(2);

/// How a turn reaches the agent. Cloneable (all `Arc`) so each turn can run on
/// its own task while the event loop keeps handling keys.
#[derive(Clone)]
struct Backend {
    gateway: Arc<GatewayClient>,
    /// Opaque, server-validated id for the directory from which this TUI was
    /// launched. The gateway resolves it per turn, for loopback callers only.
    workspace: String,
}

impl Backend {
    /// Run one turn server-side, forwarding its live tool events onto the
    /// loop's channel as the SSE stream delivers them.
    async fn turn(
        &self,
        session_id: &str,
        input: String,
        events: mpsc::UnboundedSender<TurnEvent>,
    ) -> anyhow::Result<String> {
        self.gateway
            .chat_streaming(session_id, &input, &self.workspace, |ev| {
                let _ = events.send(ev);
            })
            .await
    }
}

/// Everything the event loop needs that is not ready at first paint. Produced
/// by the background boot task while the first frame is already on screen —
/// reaching (or starting) the gateway is the whole startup cost, and nothing in
/// it needs the terminal, so the paint must not wait for it.
struct Boot {
    backend: Backend,
    /// The session to drive: the home conversation, or the resolved one on resume.
    session: String,
    /// The resumed transcript; empty for a fresh session.
    history: Vec<Message>,
    /// Where this TUI's turns run: the directory it was started in, always.
    /// A session no longer carries a root of its own — one home conversation is
    /// opened from wherever the operator happens to be standing, so the
    /// workspace is a property of the turn (docs/bot-runtime.md §2 D6).
    workspace: PathBuf,
    /// The wait a turn of this session is stopped in, on resume. A fresh
    /// session has none.
    awaiting: Option<Awaiting>,
}

type BootTask = tokio::task::JoinHandle<anyhow::Result<Boot>>;

/// Start the TUI on the operator's **home conversation**: paint immediately, and
/// connect in the background.
///
/// Not a fresh session id per launch. One principal writing privately is one
/// conversation whichever surface they picked up (docs/bot-runtime.md §2 D6),
/// so closing the terminal and reopening it continues where the Telegram DM at
/// lunch left off. `komo resume <id>` is what opens some *other* session — a
/// correspondent's, or an old one being looked into.
pub async fn run() -> anyhow::Result<()> {
    let workspace = startup_workspace()?;
    let boot: BootTask = tokio::spawn({
        let workspace = workspace.clone();
        async move {
            let backend = connect(&workspace).await?;
            let session = backend.gateway.home_session().await?;
            let history = backend.gateway.session_messages(&session).await?;
            // The home conversation is shared, so it may already be stopped in
            // a wait another ingress parked — a `/approve` prompt sent to
            // Telegram, a question asked there. Same read as `resume`.
            let awaiting = resume_awaiting(&backend, &session).await?;
            Ok(Boot {
                backend,
                session,
                history,
                workspace,
                awaiting,
            })
        }
    });
    // The home id is not known until the backend is up, so the placeholder
    // stands in until the boot task installs the real one — the same shape
    // `resume` already used for an id it has to resolve.
    drive(boot, String::new(), workspace).await
}

/// Continue an existing session (`komo resume <id>` on a TTY). Errors if the
/// session doesn't exist — resume never creates one.
pub async fn resume(id: &str) -> anyhow::Result<()> {
    let fallback_workspace = startup_workspace()?;
    let id = id.to_string();
    let boot: BootTask = tokio::spawn({
        let fallback = fallback_workspace.clone();
        let id = id.clone();
        async move {
            let backend = connect(&fallback).await?;
            let session = resolve_resume_id(&backend, &id).await?;
            let history = backend.gateway.session_messages(&session).await?;
            let awaiting = resume_awaiting(&backend, &session).await?;
            Ok(Boot {
                backend,
                session,
                history,
                workspace: fallback,
                awaiting,
            })
        }
    });
    // The raw argument stands in as the session id until the boot task
    // resolves it; a queued draft dispatches only after the resolved id is
    // installed.
    drive(boot, id, fallback_workspace).await
}

/// Confirm the id names a session that exists. A session id is a UUID and
/// nothing else now, so there is no second form to try.
///
/// A non-UUID id is refused **here**, not at the first message. Sessions from an
/// older komo (`feishu:oc_x`, `api:<uuid>`) are still listed and still readable,
/// but the gateway will not run a turn on one — so opening a chat window that
/// hydrates fine and then rejects everything typed into it is the worse of the
/// two failures.
async fn resolve_resume_id(backend: &Backend, id: &str) -> anyhow::Result<String> {
    if uuid::Uuid::parse_str(id).is_err() {
        anyhow::bail!(
            "`{id}` is a session from an older komo and can no longer be continued \
             (its transcript is still in ~/.komo/sessions/); start a new one with `komo chat`"
        );
    }
    backend
        .gateway
        .sessions()
        .await?
        .into_iter()
        .find(|s| s.id == id)
        .map(|s| s.id)
        .ok_or_else(|| anyhow::anyhow!("no session with id `{id}` (see `komo session list`)"))
}

/// The wait this session is stopped in, if any — the session projection's
/// `awaiting`, folded from the log at the last turn boundary.
///
/// Read once, on resume: a turn suspended in *this* UI is one the interaction
/// poll below is already watching, so the only wait it can learn about here is
/// one another ingress parked.
async fn resume_awaiting(backend: &Backend, id: &str) -> anyhow::Result<Option<Awaiting>> {
    Ok(backend
        .gateway
        .sessions()
        .await?
        .into_iter()
        .find(|s| s.id == id)
        .and_then(|s| s.awaiting))
}

/// Reach the gateway, starting one if none is running — komo's state lives in
/// its process, so there is nothing else to connect to.
async fn connect(workspace: &Path) -> anyhow::Result<Backend> {
    Ok(Backend {
        gateway: Arc::new(GatewayClient::connect_or_start().await?),
        workspace: folder_workspace_id(workspace)?,
    })
}

/// Set up the terminal, run the event loop, and always restore — including on
/// an error path (the panic path is covered by `ratatui::init`'s hook).
async fn drive(boot: BootTask, session: String, workspace: PathBuf) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    // Bracketed paste turns a multi-line clipboard dump into one `Event::Paste`
    // instead of a stream of Enters (which used to send a message per line).
    // The kitty keyboard flags are what let a terminal report Shift/Alt-Enter as
    // a modified Enter; where they are unsupported, Ctrl-J still inserts a
    // newline.
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let enhanced = matches!(supports_keyboard_enhancement(), Ok(true))
        && execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
    let result = event_loop(&mut terminal, boot, session, workspace).await;
    if enhanced {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    // Print only after leaving the alternate screen, so the command survives in
    // the user's normal terminal scrollback and is immediately copyable. A quit
    // before the backend connected has no session to point at (fresh sessions
    // are only created once the boot task lands), so the hint is skipped.
    if let Ok(Some(session_id)) = &result {
        println!("komo resume {session_id}");
    }
    result.map(|_| ())
}

/// How far this UI has rendered the transcript the gateway holds, and whether
/// it is currently following it.
///
/// A turn this UI started arrives on its own SSE stream. A turn it merely
/// *released* — by answering an approval or a question — continues server-side
/// with no stream to hold, so its reply is read off the transcript instead.
/// `seen` is refreshed the moment following starts, so the two never
/// double-render the same message.
#[derive(Default)]
struct Tail {
    seen: usize,
    following: bool,
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    boot: BootTask,
    session: String,
    workspace: PathBuf,
) -> anyhow::Result<Option<String>> {
    // Live tool-call events for the activity feed. The sender is cloned per
    // turn; this original stays alive so the arm never closes.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TurnEvent>();

    // The backend arrives from the boot task while the UI is already live.
    // Until then the user can draft freely and queue one submission (the same
    // one-turn-at-a-time discipline `in_flight` already enforces); everything
    // that needs the backend — dispatch, `/new`, resume history — waits.
    let mut boot = Some(boot);
    let mut backend: Option<Backend> = None;
    let mut pending: Option<String> = None;
    let mut workspace = workspace;
    // The question text currently on screen, so a poll that keeps reporting the
    // same pending question does not re-print it.
    let mut shown_question: Option<String> = None;
    let mut tail = Tail::default();

    let mut app = App::new(session);
    app.connecting = true;
    app.push(
        Role::Info,
        if app.session_id.is_empty() {
            // `komo chat` opens the home conversation, whose id only the store
            // knows — it lands with the backend a moment from now.
            format!("Komo v0.1 — opening…\nworkspace: `{}`", workspace.display())
        } else {
            format!("Komo v0.1 — resuming `{}`…", app.session_id)
        },
    );

    let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<TurnEnd>();
    // Terminal input arrives over a channel rather than being awaited inline: a
    // paste that a terminal delivers as keystrokes has to be *collected* (see
    // `paste::extend_for_paste`), which needs the receiver free while the batch
    // is assembled.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Event>();
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(Ok(event)) = events.next().await {
            if input_tx.send(event).is_err() {
                break; // the loop is gone
            }
        }
    });
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let mut poll = tokio::time::interval(INTERACTION_POLL);

    'main: loop {
        terminal.draw(|frame| ui::render(frame, &app))?;

        let mut batch: Vec<Event> = Vec::new();
        tokio::select! {
            // Biased so ordering is deterministic: keys stay responsive, and
            // queued tool events always drain before the final reply — so the
            // ✓ activity lines never render below the agent's answer.
            biased;
            maybe_event = input_rx.recv() => {
                // Collected below, once the receiver is free again.
                let Some(event) = maybe_event else { break 'main };
                batch.push(event);
            }
            // The backend landing (fires once). Install it, render what it
            // brought — resume history, the mode line — and dispatch the
            // draft queued while it was starting. A boot failure ends the
            // loop: the terminal is restored on the way out and the error
            // prints where the pre-paint failure used to.
            booted = async { boot.as_mut().expect("guarded by is_some").await }, if boot.is_some() => {
                boot = None;
                let ready = booted.map_err(anyhow::Error::from).and_then(|r| r)?;
                app.connecting = false;
                app.session_id = ready.session;
                workspace = ready.workspace;
                app.awaiting = ready.awaiting;
                tail.seen = ready.history.len();
                for message in ready.history {
                    let role = match message.role {
                        MessageRole::User => Role::You,
                        MessageRole::Assistant => Role::Agent,
                        // Persisted system/tool entries are history, not live
                        // tool activity (which has special spinner/status
                        // rendering in the TUI).
                        MessageRole::System | MessageRole::Tool => Role::Info,
                    };
                    app.push(role, message.content);
                }
                app.push(
                    Role::Info,
                    format!(
                        "connected to the gateway (trusted), session `{}`\nworkspace: `{}`",
                        app.session_id,
                        workspace.display(),
                    ),
                );
                backend = Some(ready.backend);
                if let Some(text) = pending.take() {
                    spawn_turn(
                        backend.as_ref().expect("installed above"),
                        &app.session_id, text, &turn_tx, &event_tx,
                    );
                }
            }
            // Live tool-call events: render each as an activity line, updating
            // the same line in place when it finishes. Kept above `turn_rx`
            // (under `biased`) so a turn's events drain before its reply.
            Some(event) = event_rx.recv() => {
                match event {
                    TurnEvent::ToolStarted { seq, name, args, .. } => {
                        app.tool_started(seq, name, args);
                    }
                    TurnEvent::ToolFinished { seq, name, ok, summary, .. } => {
                        app.tool_finished(seq, name, ok, summary);
                    }
                    // The model's answer as it is generated. Grows a live entry
                    // so a long round reads as progress instead of a hang.
                    TurnEvent::AssistantDelta { text } => {
                        app.stream_delta(&text);
                    }
                    // Reasoning is progress, not answer: counted for the status
                    // line, never rendered into the transcript.
                    TurnEvent::ReasoningDelta { text } => {
                        app.note_reasoning(&text);
                    }
                    // Mid-turn narration: the agent saying what it is about to
                    // do, and the authoritative version of whatever just
                    // streamed. Rendered as agent speech (which it is) ahead of
                    // the tool lines it explains; the turn's answer still
                    // arrives separately via `turn_rx`.
                    TurnEvent::AssistantText { text } => {
                        app.finish_stream(text);
                    }
                }
            }
            Some(result) = turn_rx.recv() => {
                app.finish_turn();
                // No tool is running once the turn is done.
                app.active_tool = None;
                match result {
                    // The final round streamed this same text, so settle that
                    // live entry on the reply rather than appending a duplicate.
                    TurnEnd::Reply(reply) => app.finish_stream(reply),
                    // Stopped to wait. Nothing of the turn is rendered: what it
                    // is waiting for is read out-of-band, right now rather than
                    // at the next tick — a question that took two seconds to
                    // appear reads as the turn having died.
                    TurnEnd::Waiting => {
                        if let Some(backend) = &backend {
                            poll_interactions(backend, &mut app, &mut shown_question).await;
                        }
                    }
                    TurnEnd::Failed(error) => app.push(Role::Error, error),
                }
            }
            // Show one approval at a time, and pick up a question the turn
            // stopped on. Also follows the transcript when a continuation this
            // UI released is running with no stream of its own.
            _ = poll.tick() => {
                if let Some(backend) = &backend {
                    poll_interactions(backend, &mut app, &mut shown_question).await;
                    follow_tail(backend, &mut app, &mut tail).await;
                }
            }
            _ = tick.tick() => {
                if app.in_flight || app.connecting {
                    app.spinner = app.spinner.wrapping_add(1);
                }
            }
        }

        // Terminal input. The batch is assembled here, outside the select, so a
        // paste that arrived as a burst of keystrokes can be collected and folded
        // back into one `Event::Paste` before anything is interpreted — otherwise
        // its first newline would submit the message.
        if batch.is_empty() {
            continue;
        }
        paste::drain_ready(&mut batch, &mut input_rx);
        if paste::should_extend(&batch) {
            paste::extend_for_paste(&mut batch, &mut input_rx).await;
        }
        for event in paste::coalesce_rapid_keys(batch) {
            if let Event::Paste(text) = &event {
                app.on_paste(text);
                continue;
            }
            let Event::Key(key) = event else { continue };
            // kitty-protocol terminals also send Release/Repeat.
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match app.on_key(key) {
                Some(Action::Quit) => break 'main,
                // `shown` folds pasted blocks to their chip label; `text` is
                // the full draft the agent receives.
                Some(Action::Submit { text, shown }) => {
                    app.push(Role::You, shown);
                    app.start_turn();
                    // Fresh tool feed for the new turn (seqs restart).
                    app.begin_tools();
                    match &backend {
                        Some(backend) => {
                            spawn_turn(backend, &app.session_id, text, &turn_tx, &event_tx)
                        }
                        // Still booting: hold the message (`in_flight` blocks a
                        // second one); the boot arm dispatches it on arrival.
                        None => pending = Some(text),
                    }
                }
                Some(Action::NewSession) => {
                    // A line in the log, not a new session id: the conversation
                    // continues, the model just stops being replayed what came
                    // before (docs/bot-runtime.md §3.8). Nothing else is torn
                    // down — a suspended turn is still owed its answer.
                    let Some(backend) = &backend else {
                        app.push(Role::Info, "正在启动，稍候再 /new。".to_string());
                        continue;
                    };
                    match backend.gateway.conversation_boundary(&app.session_id).await {
                        Ok(_) => app.push(Role::Info, "已开始新的上下文。".to_string()),
                        Err(error) => app.push(Role::Info, format!("开始新上下文失败：{error}")),
                    }
                }
                Some(Action::Answer { text, shown }) => {
                    let answered = match &backend {
                        Some(backend) => backend
                            .gateway
                            .answer_question(&app.session_id, &text)
                            .await
                            .unwrap_or(false),
                        None => false,
                    };
                    shown_question = None;
                    if answered {
                        app.push(Role::You, shown);
                        app.start_turn();
                        app.begin_tools();
                        // The continuation runs server-side with no stream of
                        // its own; its reply is read off the transcript.
                        start_following(backend.as_ref(), &app.session_id, &mut tail).await;
                    } else {
                        app.push(Role::Info, "问题已失效（已被回答或已过期）。".to_string());
                    }
                }
                Some(Action::Answered(answer)) => {
                    let Some(backend) = &backend else { continue };
                    match backend
                        .gateway
                        .resolve_approval(&app.session_id, answer.decision(), answer.feedback())
                        .await
                    {
                        Ok(true) => {
                            app.start_turn();
                            start_following(Some(backend), &app.session_id, &mut tail).await;
                        }
                        Ok(false) => app.push(Role::Info, "这条审批已经被处理过了。".to_string()),
                        Err(error) => app.push(Role::Error, format!("审批提交失败：{error:#}")),
                    }
                }
                Some(Action::Interrupt) => {
                    // Still booting: the only thing running is the queued
                    // draft, so Esc takes that back.
                    let Some(backend) = &backend else {
                        let message = if pending.take().is_some() {
                            app.finish_turn();
                            "已取消排队的消息。"
                        } else {
                            "没有正在运行的回合可中断。"
                        };
                        app.push(Role::Info, message.to_string());
                        continue;
                    };
                    // One request covers every way a turn can be stuck: the
                    // endpoint denies a pending approval and answers a pending
                    // question before flipping the cancel signal, because a
                    // turn parked on either never reaches another await.
                    let stopped = backend
                        .gateway
                        .cancel_turn(&app.session_id)
                        .await
                        .unwrap_or(false);
                    // Said out loud either way: "nothing happened" and "stopping,
                    // give it a moment" look identical otherwise, and the turn
                    // ends at its *next* await, not instantly.
                    app.push(
                        Role::Info,
                        if stopped {
                            "正在中断…（工具调用可能要跑完当前这一步）".to_string()
                        } else {
                            "没有正在运行的回合可中断。".to_string()
                        },
                    );
                }
                None => {}
            }
        }
    }
    // No backend means no session was ever created or resumed — there is
    // nothing for the resume hint to point at.
    Ok(backend.is_some().then_some(app.session_id))
}

/// Read what a stopped turn on this session is waiting on and surface it: an
/// approval as the modal, a question as agent speech that unlocks the input as
/// its answer. A poll failure is silent — the next tick retries, and a red line
/// every two seconds would bury the transcript.
async fn poll_interactions(backend: &Backend, app: &mut App, shown_question: &mut Option<String>) {
    if app.session_id.is_empty() {
        return;
    }
    let Ok(pending) = backend.gateway.interactions(&app.session_id).await else {
        return;
    };
    if let Some(approval) = pending.approval
        && app.modal.is_none()
    {
        app.modal = Some(ApprovalPrompt {
            summary: approval.summary,
            detail: approval.detail,
            dangerous: approval.risk == "dangerous",
        });
    }
    match pending.question {
        Some(question) => {
            if shown_question.as_deref() != Some(question.as_str()) {
                app.push(Role::Agent, question.clone());
                *shown_question = Some(question);
            }
            app.awaiting_answer = true;
        }
        // Answered here, or elsewhere: either way the next thing typed is a new
        // message, not an answer.
        None => {
            *shown_question = None;
            app.awaiting_answer = false;
        }
    }
}

/// Start following the transcript from wherever it stands now, so the
/// continuation this UI just released renders and nothing already on screen
/// renders twice.
async fn start_following(backend: Option<&Backend>, session: &str, tail: &mut Tail) {
    let Some(backend) = backend else { return };
    if let Ok(messages) = backend.gateway.session_messages(session).await {
        tail.seen = messages.len();
    }
    tail.following = true;
}

/// Append whatever the gateway has written past what this UI has rendered. Ends
/// at the continuation's answer — a turn that stops again is picked up by the
/// interaction poll instead.
async fn follow_tail(backend: &Backend, app: &mut App, tail: &mut Tail) {
    if !tail.following || app.session_id.is_empty() {
        return;
    }
    let Ok(messages) = backend.gateway.session_messages(&app.session_id).await else {
        return;
    };
    for message in messages.iter().skip(tail.seen) {
        let role = match message.role {
            MessageRole::User => Role::You,
            MessageRole::Assistant => Role::Agent,
            MessageRole::System | MessageRole::Tool => Role::Info,
        };
        app.push(role, message.content.clone());
        if message.role == MessageRole::Assistant {
            tail.following = false;
            app.finish_turn();
        }
    }
    tail.seen = messages.len();
}

/// How a turn ended, as the loop has to render it.
enum TurnEnd {
    Reply(String),
    /// It stopped to wait — for an approval, or for the user's answer to an
    /// `ask_user` question. Neither an answer nor a failure.
    Waiting,
    Failed(String),
}

/// Dispatch one turn onto its own task so the loop keeps handling keys. The
/// result lands on `turn_tx`, live tool events on `event_tx`.
fn spawn_turn(
    backend: &Backend,
    session_id: &str,
    text: String,
    turn_tx: &mpsc::UnboundedSender<TurnEnd>,
    event_tx: &mpsc::UnboundedSender<TurnEvent>,
) {
    let session_id = session_id.to_string();
    let turn_tx = turn_tx.clone();
    let events = event_tx.clone();
    let backend = backend.clone();
    tokio::spawn(async move {
        let result = classify_end(backend.turn(&session_id, text, events).await);
        let _ = turn_tx.send(result);
    });
}

/// Read a turn's outcome.
///
/// The gateway answers a cancelled or suspended turn with its own sentinel text
/// rather than an error, so those are recognised by the reply — while the
/// `is_cancelled` / `is_suspended` downcasts still cover a local transport
/// failure. Classified **before** the error is stringified: `{e:#}` would leave
/// a deliberate stop, or a turn that is merely waiting, looking like a failure.
fn classify_end(outcome: anyhow::Result<String>) -> TurnEnd {
    match outcome {
        Ok(reply) if reply == SUSPENDED_REPLY => TurnEnd::Waiting,
        Ok(reply) => TurnEnd::Reply(reply),
        Err(error) if is_cancelled(&error) => TurnEnd::Reply(CANCELLED_REPLY.to_string()),
        Err(error) if is_suspended(&error) => TurnEnd::Waiting,
        Err(error) => TurnEnd::Failed(format!("{error:#}")),
    }
}

/// Snapshot the TUI's startup folder once, so later `cd`s in child shells
/// cannot redirect this sitting's tools. It is this *process's* root, not the
/// session's: the same conversation opened from another directory runs its
/// turns there.
fn startup_workspace() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    cwd.canonicalize().map_err(Into::into)
}
