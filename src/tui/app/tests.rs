use super::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// The failure this reconciliation exists to prevent: the final round
/// streams its answer *and* delivers it again as the reply, so a naive
/// implementation renders it twice.
#[test]
fn a_streamed_answer_is_not_rendered_twice() {
    let mut app = App::new("s".into());
    app.begin_tools();
    for chunk in ["Hel", "lo ", "there"] {
        app.stream_delta(chunk);
    }
    assert_eq!(app.entries.len(), 1, "one growing entry, not one per chunk");
    assert_eq!(app.entries[0].text, "Hello there");

    // The reply arrives carrying the same text.
    app.finish_stream("Hello there");
    assert_eq!(app.entries.len(), 1, "the reply settles the live entry");
    assert_eq!(app.entries[0].text, "Hello there");
}

/// The stream and the final text legitimately differ — the runtime prefixes
/// a note when a turn stopped early — so the authoritative text wins.
#[test]
fn the_authoritative_text_replaces_what_streamed() {
    let mut app = App::new("s".into());
    app.stream_delta("partial thought");
    app.finish_stream("(Reached the tool-call limit.) partial thought");
    assert_eq!(app.entries.len(), 1);
    assert_eq!(
        app.entries[0].text,
        "(Reached the tool-call limit.) partial thought"
    );
}

/// With no deltas (an older gateway, or a round that only called tools) the
/// reply still has to land.
#[test]
fn a_reply_with_nothing_streamed_is_pushed() {
    let mut app = App::new("s".into());
    app.finish_stream("the answer");
    assert_eq!(app.entries.len(), 1);
    assert_eq!(app.entries[0].text, "the answer");
    assert!(matches!(app.entries[0].role, Role::Agent));
}

/// Each round gets its own entry: narration streamed before a tool call must
/// not be overwritten by the next round's stream.
#[test]
fn a_new_round_streams_into_a_new_entry() {
    let mut app = App::new("s".into());
    app.stream_delta("checking the config");
    app.finish_stream("checking the config");
    app.stream_delta("found it");
    app.finish_stream("found it");
    assert_eq!(
        app.entries
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>(),
        vec!["checking the config", "found it"]
    );
}

#[test]
fn empty_deltas_never_create_a_blank_bubble() {
    let mut app = App::new("s".into());
    app.stream_delta("");
    assert!(app.entries.is_empty(), "providers do emit empty deltas");
}

/// An empty reply must not blank out text the user already watched arrive.
#[test]
fn an_empty_reply_keeps_what_streamed() {
    let mut app = App::new("s".into());
    app.stream_delta("real content");
    app.finish_stream("   ");
    assert_eq!(app.entries[0].text, "real content");
}

#[test]
fn reasoning_is_counted_but_never_rendered() {
    let mut app = App::new("s".into());
    app.note_reasoning("думаю");
    app.note_reasoning("字");
    assert!(app.entries.is_empty(), "reasoning is progress, not answer");
    assert_eq!(app.reasoning_chars, 6, "counted in chars, not bytes");
    // A finished round resets the counter for the next one.
    app.finish_stream("done");
    assert_eq!(app.reasoning_chars, 0);
}

fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
}

/// Type into the modal's denial-reason line (same keys, different target).
fn type_reason(app: &mut App, s: &str) {
    assert!(app.modal_reason.is_some(), "not in reason-entry mode");
    type_str(app, s);
}

#[test]
fn shift_enter_and_ctrl_j_insert_a_newline_instead_of_sending() {
    let mut app = App::new("s".into());
    type_str(&mut app, "one");
    assert_eq!(
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        None,
        "Shift-Enter must not submit"
    );
    type_str(&mut app, "two");
    assert_eq!(app.on_key(ctrl('j')), None, "Ctrl-J must not submit");
    type_str(&mut app, "three");
    assert_eq!(app.input, "one\ntwo\nthree");
    assert_eq!(app.cursor, app.input.chars().count());
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::Submit {
            text: "one\ntwo\nthree".into(),
            shown: "one\ntwo\nthree".into(),
        }),
        "a bare Enter still sends the whole draft"
    );
}

#[test]
fn paste_keeps_newlines_and_never_submits() {
    let mut app = App::new("s".into());
    type_str(&mut app, "x");
    app.on_paste("first\r\nsecond\rthird");
    assert_eq!(app.input, "xfirst\nsecond\nthird");
    assert_eq!(app.cursor, app.input.chars().count());
    assert!(app.chips.is_empty(), "3 lines stays inline");
    // Nothing was submitted: the draft is still there for the user to edit.
    assert!(!app.in_flight);
}

#[test]
fn a_big_paste_folds_to_a_chip_but_sends_in_full() {
    let mut app = App::new("s".into());
    type_str(&mut app, "look: ");
    let pasted = "one\ntwo\nthree\nfour";
    app.on_paste(pasted);

    assert_eq!(
        app.input,
        format!("look: {pasted}"),
        "draft keeps every char"
    );
    assert_eq!(app.chips.len(), 1);
    assert_eq!(app.chips[0].label, "[Pasted: 4 lines]");
    assert_eq!(app.chips[0].range, 6..6 + pasted.chars().count());
    assert_eq!(app.cursor, app.input.chars().count());

    let action = app.on_key(key(KeyCode::Enter));
    assert_eq!(
        action,
        Some(Action::Submit {
            text: format!("look: {pasted}"),
            shown: "look: [Pasted: 4 lines]".into(),
        }),
        "the agent gets the full text; the transcript shows the chip"
    );
    assert!(app.chips.is_empty(), "chips die with the draft");
}

#[test]
fn backspace_removes_a_whole_chip() {
    let mut app = App::new("s".into());
    app.on_paste("one\ntwo\nthree\nfour");
    type_str(&mut app, "!");
    assert_eq!(app.chips.len(), 1);

    // One Backspace takes the typed char, the next takes the entire paste —
    // never a single character out of text the user cannot see.
    app.on_key(key(KeyCode::Backspace));
    assert_eq!(app.chips.len(), 1, "still folded");
    app.on_key(key(KeyCode::Backspace));
    assert!(
        app.input.is_empty(),
        "the paste went whole: {:?}",
        app.input
    );
    assert!(app.chips.is_empty());
    assert_eq!(app.cursor, 0);
}

#[test]
fn arrows_step_over_a_chip_instead_of_into_it() {
    let mut app = App::new("s".into());
    let pasted = "one\ntwo\nthree\nfour";
    app.on_paste(pasted);
    type_str(&mut app, "ab");
    let end = app.input.chars().count();

    app.on_key(key(KeyCode::Left));
    assert_eq!(app.cursor, end - 1, "inside plain text, one char at a time");
    app.on_key(key(KeyCode::Left));
    assert_eq!(app.cursor, pasted.chars().count(), "now just past the chip");
    app.on_key(key(KeyCode::Left));
    assert_eq!(app.cursor, 0, "one step clears the whole chip");
    app.on_key(key(KeyCode::Right));
    assert_eq!(app.cursor, pasted.chars().count(), "and back over it");
}

#[test]
fn repasting_the_same_content_expands_the_chip() {
    let mut app = App::new("s".into());
    let pasted = "one\ntwo\nthree\nfour";
    app.on_paste(pasted);
    assert_eq!(app.chips.len(), 1);

    // grok build's "paste didn't do what I want? paste again" — the second
    // paste unfolds the block instead of inserting a second copy.
    app.on_paste(pasted);
    assert_eq!(app.input, pasted, "content is unchanged, not doubled");
    assert!(app.chips.is_empty(), "now shown inline");
    assert_eq!(app.cursor, pasted.chars().count());
}

#[test]
fn chip_offsets_survive_editing_in_front_of_it() {
    let mut app = App::new("s".into());
    let pasted = "one\ntwo\nthree\nfour";
    app.on_paste(pasted);
    app.cursor = 0;
    // A multibyte char in front of the chip must move both its char range and
    // its byte range, or the renderer slices the wrong bytes.
    type_str(&mut app, "中");
    assert_eq!(app.chips[0].range, 1..1 + pasted.chars().count());
    assert_eq!(app.chips[0].bytes, 3.."中".len() + pasted.len());
    assert_eq!(&app.input[app.chips[0].bytes.clone()], pasted);
}

#[test]
fn tool_line_updates_in_place_on_finish() {
    let mut app = App::new("s".into());
    app.begin_tools();
    app.tool_started(1, "shell".into(), "ls -la /very/long/path".into());
    assert_eq!(app.entries.len(), 1);
    assert_eq!(app.entries[0].role, Role::Tool);
    assert_eq!(app.entries[0].tool_ok, None);
    assert_eq!(app.active_tool.as_deref(), Some("shell"));

    // Finishing the same seq updates the existing line, not a new one.
    app.tool_finished(1, "shell".into(), true, "total 42\nfoo".into());
    assert_eq!(app.entries.len(), 1, "updated in place, no new entry");
    assert_eq!(app.entries[0].tool_ok, Some(true));
    assert!(app.entries[0].text.contains("total 42"));
    assert!(
        app.entries[0].text.contains("foo"),
        "newline collapsed inline"
    );
    assert_eq!(app.active_tool, None);

    // A new turn resets tracking so a reused seq starts a fresh line.
    app.begin_tools();
    app.tool_started(1, "web_fetch".into(), String::new());
    app.tool_finished(1, "web_fetch".into(), false, "404".into());
    assert_eq!(app.entries.len(), 2);
    assert_eq!(app.entries[1].tool_ok, Some(false));
}

#[test]
fn preview_collapses_whitespace_and_truncates() {
    assert_eq!(preview("a\n\n  b\tc", 100), "a b c");
    let long = "x".repeat(200);
    let p = preview(&long, 10);
    assert_eq!(p.chars().count(), 11); // 10 + ellipsis
    assert!(p.ends_with('…'));
}

#[test]
fn typing_and_multibyte_editing_keep_utf8_boundaries() {
    let mut app = App::new("s".into());
    type_str(&mut app, "你好a");
    assert_eq!(app.input, "你好a");
    // Backspace removes whole chars, not bytes.
    app.on_key(key(KeyCode::Backspace));
    app.on_key(key(KeyCode::Backspace));
    assert_eq!(app.input, "你");
    // Insert mid-string via Left.
    type_str(&mut app, "们");
    app.on_key(key(KeyCode::Left));
    type_str(&mut app, "x");
    assert_eq!(app.input, "你x们");
}

#[test]
fn enter_submits_and_clears_but_not_while_in_flight() {
    let mut app = App::new("s".into());
    type_str(&mut app, "hello");
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::Submit {
            text: "hello".into(),
            shown: "hello".into(),
        })
    );
    assert!(app.input.is_empty());

    app.in_flight = true;
    type_str(&mut app, "queued?");
    assert_eq!(app.on_key(key(KeyCode::Enter)), None, "one turn at a time");
    assert_eq!(app.input, "queued?", "draft preserved");
}

/// Esc is the stop key, and only while there is something to stop.
#[test]
fn esc_interrupts_only_while_a_turn_is_in_flight() {
    let mut app = App::new("s".into());
    assert_eq!(
        app.on_key(key(KeyCode::Esc)),
        None,
        "idle Esc must do nothing at all"
    );

    app.start_turn();
    assert_eq!(app.on_key(key(KeyCode::Esc)), Some(Action::Interrupt));
}

/// Idle Esc must not double as "clear the input": a stop key that sometimes
/// discards the draft instead is worse than one extra keystroke.
#[test]
fn idle_esc_leaves_the_draft_alone() {
    let mut app = App::new("s".into());
    type_str(&mut app, "半句话");
    assert_eq!(app.on_key(key(KeyCode::Esc)), None);
    assert_eq!(app.input, "半句话");
}

/// A turn parked on an `ask_user` question is still in flight, so Esc has to
/// reach it — that is the case where the cancel signal alone is not enough
/// (the loop is not at an await), and the event loop resolves the question.
#[test]
fn esc_interrupts_a_turn_waiting_on_a_question() {
    let mut app = App::new("s".into());
    app.in_flight = true;
    app.awaiting_answer = true;
    assert_eq!(app.on_key(key(KeyCode::Esc)), Some(Action::Interrupt));
}

/// Precedence: a modal is only ever shown *during* a turn, so both Esc
/// meanings are live at once. The modal wins — denying is the way out of it,
/// and the turn it belongs to cannot advance until the prompt is answered
/// anyway, so nothing is lost by not interrupting on the first press.
#[test]
fn esc_under_the_modal_denies_rather_than_interrupting() {
    let mut app = App::new("s".into());
    app.in_flight = true;
    app.modal = Some(ApprovalPrompt {
        summary: "rm -rf build".into(),
        detail: None,
        dangerous: true,
    });
    assert_eq!(
        app.on_key(key(KeyCode::Esc)),
        Some(Action::Answered(Answer::Deny(None)))
    );
}

#[test]
fn turn_lifecycle_tracks_running_state_and_elapsed_time() {
    let mut app = App::new("s".into());
    assert_eq!(app.turn_elapsed(), None);

    app.start_turn();
    assert!(app.in_flight);
    assert!(app.turn_elapsed().is_some());

    app.finish_turn();
    assert!(!app.in_flight);
    assert_eq!(app.turn_elapsed(), None);
}

#[test]
fn pending_clarify_lets_a_mid_turn_submit_through_as_answer() {
    let mut app = App::new("s".into());
    app.in_flight = true;
    app.awaiting_answer = true;
    type_str(&mut app, "蓝色");
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::Answer {
            text: "蓝色".into(),
            shown: "蓝色".into(),
        })
    );
    assert!(app.input.is_empty());
    assert!(!app.awaiting_answer, "one answer per question");
    // The next mid-turn submit is back to being blocked.
    type_str(&mut app, "more");
    assert_eq!(app.on_key(key(KeyCode::Enter)), None);
}

#[test]
fn slash_new_is_a_new_session_even_mid_turn() {
    let mut app = App::new("s".into());
    app.in_flight = true;
    type_str(&mut app, "/new");
    assert_eq!(app.on_key(key(KeyCode::Enter)), Some(Action::NewSession));
}

#[test]
fn modal_captures_keys_and_answers() {
    let mut app = App::new("s".into());
    app.modal = Some(ApprovalPrompt {
        summary: "run shell".into(),
        detail: None,
        dangerous: false,
    });
    // Ordinary typing is captured by the modal.
    assert_eq!(app.on_key(key(KeyCode::Char('x'))), None);
    assert!(app.input.is_empty());
    // Answering closes the modal and reports the decision to the loop.
    assert_eq!(
        app.on_key(key(KeyCode::Char('y'))),
        Some(Action::Answered(Answer::Once))
    );
    assert!(app.modal.is_none());
}

/// `n` opens a one-line reason prompt whose text rides the denial, so a
/// refusal can tell the agent what to do instead.
#[test]
fn denying_with_n_collects_a_reason() {
    let mut app = App::new("s".into());
    app.modal = Some(ApprovalPrompt {
        summary: "rm -rf build".into(),
        detail: None,
        dangerous: true,
    });

    // `n` does not answer yet — it switches the modal to reason entry.
    assert_eq!(app.on_key(key(KeyCode::Char('n'))), None);
    assert_eq!(app.modal_reason.as_deref(), Some(""));
    assert!(app.modal.is_some(), "modal stays open while typing");

    for c in "用 trash".chars() {
        assert_eq!(app.on_key(key(KeyCode::Char(c))), None);
    }
    // The reason buffer is separate from the composer draft.
    assert!(app.input.is_empty());
    assert_eq!(app.modal_reason.as_deref(), Some("用 trash"));

    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::Answered(Answer::Deny(Some("用 trash".into()))))
    );
    assert!(app.modal.is_none());
    assert!(app.modal_reason.is_none());
}

#[test]
fn esc_denies_immediately_without_a_reason() {
    let mut app = App::new("s".into());
    app.modal = Some(ApprovalPrompt {
        summary: "rm -rf build".into(),
        detail: None,
        dangerous: true,
    });
    assert_eq!(
        app.on_key(key(KeyCode::Esc)),
        Some(Action::Answered(Answer::Deny(None)))
    );
}

/// Esc out of reason entry is still a denial — just an unexplained one.
#[test]
fn esc_during_reason_entry_denies_without_the_partial_text() {
    let mut app = App::new("s".into());
    app.modal = Some(ApprovalPrompt {
        summary: "rm -rf build".into(),
        detail: None,
        dangerous: true,
    });
    app.on_key(key(KeyCode::Char('n')));
    type_reason(&mut app, "half-typed");
    assert_eq!(
        app.on_key(key(KeyCode::Esc)),
        Some(Action::Answered(Answer::Deny(None)))
    );
}

/// An empty reason is the same as a plain denial.
#[test]
fn enter_with_a_blank_reason_is_a_plain_denial() {
    let mut app = App::new("s".into());
    app.modal = Some(ApprovalPrompt {
        summary: "write".into(),
        detail: None,
        dangerous: false,
    });
    app.on_key(key(KeyCode::Char('n')));
    type_reason(&mut app, "  ");
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::Answered(Answer::Deny(None)))
    );
}

#[test]
fn ctrl_c_quits_everywhere_ctrl_d_only_on_empty_input() {
    let mut app = App::new("s".into());
    assert_eq!(app.on_key(ctrl('d')), Some(Action::Quit));
    type_str(&mut app, "draft");
    assert_eq!(app.on_key(ctrl('d')), None, "Ctrl-D with a draft is inert");
    assert_eq!(app.on_key(ctrl('c')), Some(Action::Quit));
}

#[test]
fn a_workspace_line_is_read_by_the_ui_not_sent_to_the_agent() {
    let mut app = App::new("s".into());
    type_str(&mut app, "/workspace");
    assert_eq!(app.on_key(key(KeyCode::Enter)), Some(Action::ShowWorkspace));

    type_str(&mut app, "/workspace add ../lib");
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::AddWorkspace("../lib".into()))
    );

    // Anything else that merely starts with the word is a message.
    type_str(&mut app, "/workspaces");
    assert!(matches!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::Submit { .. })
    ));
}
