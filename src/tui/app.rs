//! TUI state + key handling, kept free of terminal I/O so it is unit-testable.
//! The event loop (`mod.rs`) feeds key events in and interprets the returned
//! [`Action`]s; rendering (`ui.rs`) reads the state.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use komo_core::domain::awaiting::Awaiting;

use super::paste;

/// The user's answer to an approval modal, in the words
/// `POST /api/interactions/{session}/approval` accepts.
///
/// There is no `always` here on purpose: a saved grant is described by the rule
/// it would write, and the rule is derived from the *running turn's* channel and
/// action — which live in the gateway, not in this process. Offering the key
/// without being able to show what it saves is the one thing the modal must not
/// do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Allow this one action.
    Once,
    /// Allow and remember the scope key for the rest of the session.
    Session,
    /// Refuse, optionally with a reason the agent is told (typed after `n`).
    Deny(Option<String>),
}

impl Answer {
    /// The wire decision the gateway parses (`api::parse_decision`).
    pub fn decision(&self) -> &'static str {
        match self {
            Answer::Once => "once",
            Answer::Session => "session",
            Answer::Deny(_) => "deny",
        }
    }

    /// The denial reason, relayed to the model so it can correct the call.
    pub fn feedback(&self) -> Option<String> {
        match self {
            Answer::Deny(reason) => reason.clone(),
            _ => None,
        }
    }
}

/// One approval rendered as a modal, as
/// `GET /api/interactions/{session}` reports it.
#[derive(Debug, Clone)]
pub struct ApprovalPrompt {
    pub summary: String,
    pub detail: Option<String>,
    pub dangerous: bool,
}

/// A pasted block the composer folds to a one-line label. Chips never overlap and
/// stay ordered, so the composer can treat one as a single atomic glyph — grok
/// build's `KIND_PASTE` element, without the general element machinery.
///
/// The range is carried twice on purpose: `range` is in **chars** (the unit
/// `cursor` uses), `bytes` in **bytes**, which is what lets the renderer *slice
/// past* a folded block instead of walking it. Without that, every frame would
/// still traverse a megabyte paste character by character — exactly the cost the
/// chip exists to avoid.
#[derive(Debug, Clone)]
pub struct PasteChip {
    pub range: std::ops::Range<usize>,
    pub bytes: std::ops::Range<usize>,
    pub label: String,
}

/// Who a transcript entry belongs to (drives the prefix + styling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    You,
    Agent,
    /// System notices (session started, …).
    Info,
    Error,
    /// A tool-call activity line (fed by the turn's live [`TurnEvent`] stream).
    /// Its glyph/color comes from [`Entry::tool_ok`], not a fixed prefix.
    ///
    /// [`TurnEvent`]: komo_core::domain::events::TurnEvent
    Tool,
}

pub struct Entry {
    pub role: Role,
    pub text: String,
    /// For [`Role::Tool`] entries: `None` while the call is running, `Some(true)`
    /// on success, `Some(false)` on failure. `None` (unused) for every other role.
    pub tool_ok: Option<bool>,
}

/// What the event loop should do in response to a key, beyond the state
/// mutation already applied.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Run a turn with this input. `text` is the full draft (what the agent
    /// gets); `shown` is the same draft with pasted blocks folded to their chip
    /// labels — what belongs in the transcript.
    Submit {
        text: String,
        shown: String,
    },
    /// Start a fresh session (`/new` / `/clear`).
    NewSession,
    /// `/workspace add <path>`: widen this task's workspace. The path is
    /// whatever was typed — the loop resolves a relative one against its own
    /// cwd, since only it knows where this TUI was launched.
    AddWorkspace(String),
    /// `/workspace`: say which directories this session works in.
    ShowWorkspace,
    /// The user answered the approval modal.
    Answered(Answer),
    /// The user answered a mid-turn `ask_user` question: resolve it into the
    /// suspended turn instead of starting a new one. Same `text`/`shown` split
    /// as [`Action::Submit`].
    Answer {
        text: String,
        shown: String,
    },
    /// Stop the turn in flight (Esc). Only produced while one *is* in flight —
    /// see [`App::on_key`].
    Interrupt,
    Quit,
}

pub struct App {
    pub session_id: String,
    /// What this sitting is, rendered in the identity row: `home`, or a task
    /// and the directory it runs in. Empty until the boot task lands — which
    /// conversation this is is not known before then on every entry point.
    pub session_label: String,
    /// The model this conversation runs on, rendered at the right of the status
    /// row: the session's own choice with its effort, else the gateway default.
    /// Empty until the boot task lands.
    pub model_label: String,
    pub entries: Vec<Entry>,
    pub input: String,
    /// Cursor as a char index into `input`.
    pub cursor: usize,
    /// Folded paste blocks inside `input`, ordered and non-overlapping. Display
    /// state only: `input` always holds the full text.
    pub chips: Vec<PasteChip>,
    /// Scroll offset in wrapped lines from the bottom; 0 = follow the tail.
    pub scroll_from_bottom: u16,
    pub in_flight: bool,
    /// The backend is still booting in the background (the UI paints before
    /// connect finishes). Drafting works; a submission is queued by the event
    /// loop and dispatched when the backend lands.
    pub connecting: bool,
    /// Monotonic start time for the active turn. Kept in the UI state so the
    /// status row can show useful progress without depending on wall-clock
    /// time (or being affected by a system clock adjustment).
    turn_started_at: Option<Instant>,
    /// A mid-turn `ask_user` question is pending: the next submit is its
    /// answer (allowed through even though a turn is in flight).
    pub awaiting_answer: bool,
    /// The wait a *suspended* turn of this session is stopped in, read from the
    /// session projection when a conversation is resumed. Unlike
    /// [`awaiting_answer`](Self::awaiting_answer) this outlives the process that
    /// asked: the turn is parked in the log, not in a channel this UI holds.
    pub awaiting: Option<Awaiting>,
    pub spinner: usize,
    pub modal: Option<ApprovalPrompt>,
    /// Set while the modal is collecting a *reason* for a denial (the user
    /// pressed `n`): a one-line buffer whose content is handed to the agent so
    /// it can correct the call instead of retrying it. `None` = the modal is in
    /// its normal key-per-answer mode.
    pub modal_reason: Option<String>,
    /// Maps a running tool's turn sequence → its transcript entry index, so a
    /// `ToolFinished` can update the same line in place. Reset each turn (seqs
    /// restart per turn); `-1` (un-ledgered) calls are not tracked here.
    tool_index: HashMap<i64, usize>,
    /// Name of the tool currently running, shown in the status line. `None`
    /// when nothing is mid-call.
    pub active_tool: Option<String>,
    /// Index of the agent entry currently being streamed into, if any. Deltas
    /// append to it; the authoritative end-of-round text replaces it
    /// ([`finish_stream`](Self::finish_stream)), which is what keeps a streamed
    /// answer from being rendered twice.
    streaming: Option<usize>,
    /// Characters of reasoning the model has streamed this round. Shown in the
    /// status line so a long think reads as progress rather than a hang — the
    /// reasoning text itself is deliberately not rendered into the transcript
    /// (it is not part of the answer and is never persisted).
    pub reasoning_chars: usize,
}

impl App {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            session_label: String::new(),
            model_label: String::new(),
            entries: Vec::new(),
            input: String::new(),
            cursor: 0,
            chips: Vec::new(),
            scroll_from_bottom: 0,
            in_flight: false,
            connecting: false,
            turn_started_at: None,
            awaiting_answer: false,
            awaiting: None,
            spinner: 0,
            modal: None,
            modal_reason: None,
            tool_index: HashMap::new(),
            active_tool: None,
            streaming: None,
            reasoning_chars: 0,
        }
    }

    pub fn push(&mut self, role: Role, text: impl Into<String>) {
        self.entries.push(Entry {
            role,
            text: text.into(),
            tool_ok: None,
        });
        // New content: snap back to following the tail.
        self.scroll_from_bottom = 0;
    }

    /// Reset the per-turn tool state (call when a new turn is submitted): a new
    /// turn's sequence counter restarts, so stale seq→index mappings must go.
    pub fn begin_tools(&mut self) {
        self.tool_index.clear();
        self.active_tool = None;
        self.streaming = None;
        self.reasoning_chars = 0;
    }

    /// Append a streamed chunk of the agent's answer, starting a live entry if
    /// this is the round's first chunk.
    ///
    /// Empty chunks are ignored rather than creating an entry: providers do emit
    /// zero-length deltas, and one would otherwise leave a blank agent bubble in
    /// the transcript.
    pub fn stream_delta(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.streaming {
            Some(at) => self.entries[at].text.push_str(text),
            None => {
                self.push(Role::Agent, text);
                self.streaming = Some(self.entries.len() - 1);
            }
        }
        self.scroll_from_bottom = 0;
    }

    /// Settle a streamed round on its authoritative text.
    ///
    /// The stream and the final text can legitimately differ — the runtime
    /// substitutes a fallback for an empty reply, and prefixes a note when a
    /// turn stopped early — so the finished text *replaces* what streamed rather
    /// than being appended to it. With nothing streamed (an older gateway, or a
    /// round with no visible output) this is just a push.
    pub fn finish_stream(&mut self, text: impl Into<String>) {
        let text = text.into();
        match self.streaming.take() {
            Some(at) => {
                if text.trim().is_empty() {
                    // Nothing authoritative to show: keep what streamed.
                    return;
                }
                self.entries[at].text = text;
            }
            None => self.push(Role::Agent, text),
        }
        self.reasoning_chars = 0;
        self.scroll_from_bottom = 0;
    }

    /// Note streamed reasoning. Counted for the status line, not rendered: it is
    /// work in progress, not the answer.
    pub fn note_reasoning(&mut self, text: &str) {
        self.reasoning_chars += text.chars().count();
    }

    /// Mark a turn as running and start its elapsed-time counter.
    pub fn start_turn(&mut self) {
        self.in_flight = true;
        self.turn_started_at = Some(Instant::now());
        // Saying something else *is* the answer to a pending wait (the gateway
        // resolves it as moved-on), so the badge goes with the message.
        self.awaiting = None;
    }

    /// Mark a turn as complete and clear its elapsed-time counter.
    pub fn finish_turn(&mut self) {
        self.in_flight = false;
        self.turn_started_at = None;
    }

    /// Elapsed time for the current turn, if it was started through the UI.
    pub fn turn_elapsed(&self) -> Option<Duration> {
        self.turn_started_at.map(|started| started.elapsed())
    }

    /// A tool call started: append a running activity line and remember it so
    /// [`tool_finished`](Self::tool_finished) can update it in place.
    pub fn tool_started(&mut self, seq: i64, name: String, args: String) {
        let args = preview(&args, 100);
        let text = if args.is_empty() {
            name.clone()
        } else {
            format!("{name}  {args}")
        };
        self.entries.push(Entry {
            role: Role::Tool,
            text,
            tool_ok: None,
        });
        if seq >= 0 {
            self.tool_index.insert(seq, self.entries.len() - 1);
        }
        self.active_tool = Some(name);
        self.scroll_from_bottom = 0;
    }

    /// A tool call finished: mark its line ✓/✗ with a result preview, updating
    /// the running line in place when the seq is known, else appending one.
    pub fn tool_finished(&mut self, seq: i64, name: String, ok: bool, summary: String) {
        let summary = preview(&summary, 120);
        let text = if summary.is_empty() {
            name.clone()
        } else {
            format!("{name}  {summary}")
        };
        match self
            .tool_index
            .remove(&seq)
            .filter(|&i| i < self.entries.len())
        {
            Some(i) => {
                self.entries[i].text = text;
                self.entries[i].tool_ok = Some(ok);
            }
            None => self.entries.push(Entry {
                role: Role::Tool,
                text,
                tool_ok: Some(ok),
            }),
        }
        // Only the tracked run is "active"; a later start may have replaced it,
        // but on finish we clear the indicator (the next start re-sets it).
        self.active_tool = None;
        self.scroll_from_bottom = 0;
    }

    /// Close the approval modal and hand the answer to the event loop, which
    /// posts it to the gateway holding the suspended turn.
    fn resolve_modal(&mut self, answer: Answer) -> Option<Action> {
        self.modal_reason = None;
        self.modal = None;
        Some(Action::Answered(answer))
    }

    /// Handle one key press. Mutates the state and returns the action (if any)
    /// the event loop must carry out.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        // The approval modal captures the keyboard while shown.
        if self.modal.is_some() {
            // Sub-mode: `n` opened a one-line "why?" prompt. Enter sends the
            // reason with the denial (empty = a plain denial); Esc bails out.
            if let Some(reason) = self.modal_reason.as_mut() {
                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Some(Action::Quit);
                    }
                    KeyCode::Enter => {
                        let text = reason.trim().to_string();
                        return self
                            .resolve_modal(Answer::Deny((!text.is_empty()).then_some(text)));
                    }
                    KeyCode::Esc => return self.resolve_modal(Answer::Deny(None)),
                    KeyCode::Backspace => {
                        reason.pop();
                    }
                    KeyCode::Char(c) => reason.push(c),
                    _ => {}
                }
                return None;
            }

            let answer = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Answer::Once),
                KeyCode::Char('s') | KeyCode::Char('S') => Some(Answer::Session),
                // `n` asks for a reason first (one extra keystroke); Esc is the
                // immediate, explanation-free denial.
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.modal_reason = Some(String::new());
                    return None;
                }
                KeyCode::Esc => Some(Answer::Deny(None)),
                // Ctrl-C still quits even under a modal; the approval stays
                // pending in the gateway for whoever answers it next.
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Some(Action::Quit);
                }
                _ => None,
            };
            if let Some(answer) = answer {
                return self.resolve_modal(answer);
            }
            return None;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::Quit)
            }
            // Esc interrupts the turn in flight, and does nothing at all when
            // idle. Deliberately *not* "clear the input" when idle: the whole
            // point of a stop key is that it can be hit without thinking, and a
            // key that sometimes discards the draft instead is worse than one
            // extra keystroke. (Under the approval modal Esc still means "deny"
            // — that branch returned above; a turn parked on a prompt is not
            // going anywhere until the prompt is answered anyway.)
            KeyCode::Esc if self.in_flight => Some(Action::Interrupt),
            // Ctrl-D quits only on an empty input, shell-style.
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.input.is_empty() =>
            {
                Some(Action::Quit)
            }
            // Newline instead of send. Shift/Alt-Enter needs a terminal that
            // reports the modifier (kitty keyboard protocol, pushed in `drive`);
            // Ctrl-J is the fallback everywhere else.
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert_char('\n');
                None
            }
            KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => {
                self.insert_char('\n');
                None
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                if text == "/new" || text == "/clear" {
                    self.clear_input();
                    return Some(Action::NewSession);
                }
                if let Some(action) = workspace_command(&text) {
                    self.clear_input();
                    return Some(action);
                }
                // The transcript shows the draft as it looked — pasted blocks
                // stay folded to their chip label. The agent gets `text`, which
                // is always the full content.
                let shown = self.folded_input();
                // A question the turn stopped on takes the next thing typed as
                // its answer — the suspended turn continues with it rather than
                // a second turn starting beside it. Checked ahead of
                // `in_flight` because a suspended turn is *not* in flight: it
                // gave up its slot precisely so this could be typed.
                if self.awaiting_answer {
                    self.clear_input();
                    self.awaiting_answer = false;
                    return Some(Action::Answer { text, shown });
                }
                if self.in_flight {
                    // One turn at a time; keep the draft so nothing is lost.
                    return None;
                }
                self.clear_input();
                Some(Action::Submit { text, shown })
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_char(c);
                None
            }
            // A chip deletes whole: the cursor is never inside a pasted block,
            // so one Backspace behind it removes the paste rather than shaving a
            // character off content the user cannot see.
            KeyCode::Backspace => {
                match self.chip_ending_at(self.cursor) {
                    Some(i) => self.delete_chars(self.chips[i].range.clone()),
                    None if self.cursor > 0 => self.delete_chars(self.cursor - 1..self.cursor),
                    None => {}
                }
                None
            }
            KeyCode::Delete => {
                match self.chip_starting_at(self.cursor) {
                    Some(i) => self.delete_chars(self.chips[i].range.clone()),
                    None if self.cursor < self.input.chars().count() => {
                        self.delete_chars(self.cursor..self.cursor + 1)
                    }
                    None => {}
                }
                None
            }
            // Chips are atomic to the cursor too: stepping over one lands on the
            // far side instead of inside the hidden text.
            KeyCode::Left => {
                self.cursor = match self.chip_ending_at(self.cursor) {
                    Some(i) => self.chips[i].range.start,
                    None => self.cursor.saturating_sub(1),
                };
                None
            }
            KeyCode::Right => {
                self.cursor = match self.chip_starting_at(self.cursor) {
                    Some(i) => self.chips[i].range.end,
                    None => (self.cursor + 1).min(self.input.chars().count()),
                };
                None
            }
            KeyCode::Home => {
                self.cursor = 0;
                None
            }
            KeyCode::End => {
                self.cursor = self.input.chars().count();
                None
            }
            KeyCode::Up => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(1);
                None
            }
            KeyCode::Down => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(1);
                None
            }
            KeyCode::PageUp => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(10);
                None
            }
            _ => None,
        }
    }

    /// Byte offset of the char cursor (input is UTF-8; CJK chars are multibyte).
    fn byte_cursor(&self) -> usize {
        self.byte_at(self.cursor)
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.chips.clear();
    }

    fn insert_char(&mut self, c: char) {
        let at = self.byte_cursor();
        self.input.insert(at, c);
        self.shift_chips(self.cursor, 1, c.len_utf8() as isize);
        self.cursor += 1;
    }

    /// Bracketed paste (or a burst of keystrokes a terminal without bracketed
    /// paste turned into one — see `paste::coalesce_rapid_keys`): the clipboard
    /// text lands in the draft verbatim, newlines included, so a multi-line paste
    /// can never fire a send per line.
    ///
    /// A paste past the chip threshold is *displayed* as a one-line label. The
    /// draft still holds every character, so submitting needs no expansion — the
    /// fold is purely how it renders, which is also what keeps a megabyte paste
    /// from being re-wrapped on every frame.
    pub fn on_paste(&mut self, text: &str) {
        // The approval modal owns the keyboard; a stray paste must not leak into
        // the draft behind it.
        if self.modal.is_some() {
            return;
        }
        let text = paste::normalize_cr(text);
        if text.is_empty() {
            return;
        }

        // Repaste-to-expand, from grok build: "paste didn't do what I want?
        // paste again." Pasting a chip's exact content with the cursor on it
        // unfolds that chip instead of inserting a second copy.
        if let Some(i) = self
            .chip_ending_at(self.cursor)
            .or_else(|| self.chip_starting_at(self.cursor))
            && self.chip_text(i) == text
        {
            let chip = self.chips.remove(i);
            self.cursor = chip.range.end;
            return;
        }

        let at = self.byte_cursor();
        self.input.insert_str(at, &text);
        let len = text.chars().count();
        self.shift_chips(self.cursor, len as isize, text.len() as isize);
        if paste::is_chip_worthy(&text) {
            let chip = PasteChip {
                range: self.cursor..self.cursor + len,
                bytes: at..at + text.len(),
                label: paste::chip_label(&text),
            };
            let index = self
                .chips
                .partition_point(|c| c.range.start < chip.range.start);
            self.chips.insert(index, chip);
        }
        self.cursor += len;
    }

    /// The draft as it is shown: every chip range replaced by its label. Used for
    /// the transcript entry when the draft is sent (the agent gets `input`).
    fn folded_input(&self) -> String {
        if self.chips.is_empty() {
            return self.input.trim().to_string();
        }
        let mut out = String::new();
        let mut at = 0usize;
        for chip in &self.chips {
            out.extend(self.input.chars().skip(at).take(chip.range.start - at));
            out.push_str(&chip.label);
            at = chip.range.end;
        }
        out.extend(self.input.chars().skip(at));
        out.trim().to_string()
    }

    /// Index of the chip that ends exactly at `at` (the cursor sits behind it).
    fn chip_ending_at(&self, at: usize) -> Option<usize> {
        self.chips.iter().position(|c| c.range.end == at)
    }

    /// Index of the chip that starts exactly at `at` (the cursor sits in front).
    fn chip_starting_at(&self, at: usize) -> Option<usize> {
        self.chips.iter().position(|c| c.range.start == at)
    }

    fn chip_text(&self, i: usize) -> &str {
        &self.input[self.chips[i].bytes.clone()]
    }

    /// Move every chip starting at or after `at` (a char index) by `delta` chars
    /// / `delta_bytes` bytes — the draft changed length in front of them.
    fn shift_chips(&mut self, at: usize, delta: isize, delta_bytes: isize) {
        for chip in &mut self.chips {
            if chip.range.start >= at {
                chip.range.start = chip.range.start.saturating_add_signed(delta);
                chip.range.end = chip.range.end.saturating_add_signed(delta);
                chip.bytes.start = chip.bytes.start.saturating_add_signed(delta_bytes);
                chip.bytes.end = chip.bytes.end.saturating_add_signed(delta_bytes);
            }
        }
    }

    /// Delete a char range from the draft, dropping any chip it covers and
    /// pulling the later chips back. The cursor lands where the text was.
    fn delete_chars(&mut self, range: std::ops::Range<usize>) {
        let (start, end) = (self.byte_at(range.start), self.byte_at(range.end));
        self.input.replace_range(start..end, "");
        self.chips
            .retain(|c| !(c.range.start >= range.start && c.range.end <= range.end));
        self.shift_chips(
            range.end,
            -((range.end - range.start) as isize),
            -((end - start) as isize),
        );
        self.cursor = range.start;
    }

    /// Byte offset of char index `at` (the end of the string when past it).
    fn byte_at(&self, at: usize) -> usize {
        self.input
            .char_indices()
            .nth(at)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }
}

/// Read a `/workspace` line. `/workspace` alone reports; `/workspace add
/// <path>` widens. Anything else is not this command and goes to the agent as
/// ordinary text.
fn workspace_command(text: &str) -> Option<Action> {
    let rest = text.strip_prefix("/workspace")?.trim_start();
    if rest.is_empty() {
        return Some(Action::ShowWorkspace);
    }
    let path = rest.strip_prefix("add")?.trim();
    (!path.is_empty()).then(|| Action::AddWorkspace(path.to_string()))
}

/// Collapse a (possibly multi-line, possibly long) tool arg/result into a
/// single tidy line for the activity feed: newlines/tabs → spaces, runs of
/// whitespace squeezed, then truncated to `max` display chars with an ellipsis.
fn preview(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max + 1));
    let mut last_space = false;
    for c in s.chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if c == ' ' {
            if last_space || out.is_empty() {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(c);
    }
    let trimmed = out.trim_end();
    if trimmed.chars().count() > max {
        let kept: String = trimmed.chars().take(max).collect();
        format!("{kept}…")
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests;
