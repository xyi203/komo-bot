use super::*;
use time::format_description::well_known::Rfc3339;

fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).unwrap()
}

fn user(seq: u64, text: &str) -> SessionEvent {
    SessionEvent::new(
        seq,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: "turn-1".into(),
            content: text.into(),
            source: MessageSource::User,
            surface: SurfacePlacement::append(),
        }),
    )
}

fn assistant(seq: u64, text: &str) -> SessionEvent {
    SessionEvent::new(
        seq,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::AssistantMessage(AssistantMessageEvent {
            turn_id: "turn-1".into(),
            content: text.into(),
            tool_note: String::new(),
            surface: SurfacePlacement::append(),
        }),
    )
}

#[test]
fn an_event_round_trips_through_its_stored_line() {
    let event = SessionEvent::new(
        42,
        at("2026-09-01T10:30:00Z"),
        SessionEventKind::ToolCallStarted(ToolCallStartedEvent {
            turn_id: "turn-7".into(),
            call_id: "call-3".into(),
            call_index: 0,
            tool: "shell".into(),
            args: r#"{"command":"cargo test"}"#.into(),
        }),
    );
    let line = serde_json::to_string(&event).unwrap();
    // The type is a top-level, greppable field — an operator opening the
    // file should not have to know the payload shape to see what happened.
    assert!(line.contains(r#""type":"tool/call-started""#), "{line}");
    assert!(line.contains(r#""seq":42"#), "{line}");
    // Required is the default, so it costs no bytes.
    assert!(!line.contains("ignorable"), "{line}");
    assert_eq!(decode_event(&line).unwrap(), Some(event));
}

#[test]
fn a_message_event_writes_its_surface_declaration_inline() {
    // Locks the stored shape: everything downstream (segments, folds, the
    // human transcript) reads these bytes, so a silent change here is a
    // silent change to every session on disk.
    let event = SessionEvent::new(
        4,
        at("2026-09-01T10:30:00Z"),
        SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: "turn-2".into(),
            content: "[summary]".into(),
            source: MessageSource::Compaction,
            surface: SurfacePlacement::replace(0, 1, vec![0, 1]),
        }),
    );
    assert_eq!(
        serde_json::to_string(&event).unwrap(),
        r#"{"v":1,"seq":4,"at":"2026-09-01T10:30:00Z","type":"user/message","data":{"turn_id":"turn-2","content":"[summary]","source":"compaction","surfaceOp":{"replace":{"start":0,"end":1}},"sourceEventSeqs":[0,1]}}"#
    );

    let plain = user(0, "hi");
    assert_eq!(
        serde_json::to_string(&plain).unwrap(),
        r#"{"v":1,"seq":0,"at":"2026-08-31T00:00:00Z","type":"user/message","data":{"turn_id":"turn-1","content":"hi","source":"user","surfaceOp":"append"}}"#
    );
}

#[test]
fn a_fresh_turn_costs_no_bytes_for_the_continuation_link() {
    // Most turns are not continuations, and `turn/started` is written once
    // per turn on every session — an always-present null would be pure
    // overhead on the most common event there is.
    let fresh = SessionEvent::new(
        0,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::TurnStarted {
            turn_id: "turn-1".into(),
            resumed_from: None,
        },
    );
    assert_eq!(
        serde_json::to_string(&fresh).unwrap(),
        r#"{"v":1,"seq":0,"at":"2026-08-31T00:00:00Z","type":"turn/started","data":{"turn_id":"turn-1"}}"#
    );

    let continued = SessionEvent::new(
        1,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::TurnStarted {
            turn_id: "turn-2".into(),
            resumed_from: Some("turn-1".into()),
        },
    );
    assert_eq!(
        serde_json::to_string(&continued).unwrap(),
        r#"{"v":1,"seq":1,"at":"2026-08-31T00:00:00Z","type":"turn/started","data":{"turn_id":"turn-2","resumed_from":"turn-1"}}"#
    );
    assert_eq!(
        decode_event(&serde_json::to_string(&continued).unwrap()).unwrap(),
        Some(continued)
    );
}

/// The runtime's mid-turn nudge is recorded so a resume can rebuild the
/// exact history the live turn had — but it is not something anyone said,
/// so it makes no surface node and the transcript keeps alternating.
#[test]
fn a_runtime_nudge_leaves_no_trace_on_the_surface() {
    let round = |seq: u64, round: u32| {
        SessionEvent::new(
            seq,
            at("2026-08-31T00:00:00Z"),
            SessionEventKind::AssistantRound(AssistantRoundEvent {
                turn_id: "turn-1".into(),
                round,
                response_id: format!("resp-{round}"),
                blocks: serde_json::Value::Null,
                tokens_in: 0,
                tokens_out: 0,
                tokens_cached: 0,
            }),
        )
    };
    let nudge = SessionEvent::new(
        2,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: "turn-1".into(),
            content: "Runtime check: this turn issued no tool call.".into(),
            source: MessageSource::Runtime,
            surface: SurfacePlacement::append(),
        }),
    );
    let events = vec![
        user(0, "打开热水器"),
        round(1, 0),
        nudge,
        round(3, 1),
        assistant(4, "我没有执行任何操作。"),
    ];

    let folded = SurfaceProjection::fold(&events, 0).unwrap();
    assert_eq!(folded.surface.nodes(), &[0, 4]);
    let messages = folded.messages().unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|m| (m.role.clone(), m.content.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (Role::User, "打开热水器"),
            (Role::Assistant, "我没有执行任何操作。"),
        ]
    );
}

#[test]
fn recall_is_its_own_event_so_the_envelope_stays_deduped() {
    // `request/header` is written only when the envelope changes; recall
    // changes every turn, so folding it in there would rewrite the whole
    // envelope — system prompt and tool schemas included — each time.
    let event = SessionEvent::new(
        7,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::TurnMemories {
            turn_id: "turn-1".into(),
            memories: RecalledMemories {
                pinned: vec!["m1".into()],
                recall: vec!["m2".into()],
            },
        },
    );
    let line = serde_json::to_string(&event).unwrap();
    assert!(line.contains(r#""type":"turn/memories""#), "{line}");
    assert_eq!(decode_event(&line).unwrap(), Some(event));
}

#[test]
fn an_unknown_event_type_reads_as_inert_and_keeps_its_seq() {
    // A record a retired feature left behind, and one a newer komo writes:
    // neither may cost the session it sits in.
    for line in [
        r#"{"v":1,"seq":9,"at":"2026-08-31T00:00:00Z","type":"task/spawned","data":{"turn_id":"t1","task_id":"k1","kind":"shell","label":"sleep 1"}}"#,
        r#"{"v":1,"seq":9,"at":"2026-08-31T00:00:00Z","type":"workflow/step-entered","data":{}}"#,
    ] {
        let event = decode_event(line).unwrap().expect("read, not skipped");
        assert_eq!(event.seq, 9, "the fold needs the seq to stay contiguous");
        assert_eq!(event.kind, SessionEventKind::Unknown);
        assert_eq!(event.surface(), None, "and it says nothing");
        assert_eq!(event.turn_id_of_work(), None);
    }
}

#[test]
fn a_known_type_that_will_not_parse_still_refuses() {
    // The shape it declares is one this build understands, so a payload it
    // cannot read is a hole in history rather than a foreign record.
    let line = r#"{"v":1,"seq":9,"at":"2026-08-31T00:00:00Z","type":"turn/started","data":{}}"#;
    assert!(matches!(
        decode_event(line),
        Err(FoldError::Malformed { seq: Some(9), .. })
    ));
}

#[test]
fn an_ignorable_event_is_skipped_instead() {
    // The one escape: its writer promised losing it cannot change what the
    // rest of the log means.
    let line = r#"{"v":1,"seq":9,"at":"2026-08-31T00:00:00Z","ignorable":true,"type":"turn/started","data":{}}"#;
    assert_eq!(decode_event(line), Ok(None));
}

#[test]
fn a_newer_format_version_refuses_before_the_payload_is_read() {
    let line = r#"{"v":2,"seq":9,"at":"2026-08-31T00:00:00Z","type":"turn/started","data":{"turn_id":"t"}}"#;
    assert_eq!(
        decode_event(line),
        Err(FoldError::UnsupportedVersion { seq: 9, version: 2 })
    );
}

#[test]
fn the_first_version_marks_nothing_ignorable() {
    // The mechanism exists for a later version; shipping one now would mean
    // this build already tolerates losing something.
    let events = [
        user(0, "hi"),
        assistant(1, "hello"),
        SessionEvent::new(
            2,
            at("2026-08-31T00:00:00Z"),
            SessionEventKind::TurnStarted {
                turn_id: "t".into(),
                resumed_from: None,
            },
        ),
    ];
    assert!(events.iter().all(|e| !e.ignorable));
}

#[test]
fn only_message_events_can_declare_a_surface_placement() {
    assert!(user(0, "hi").surface().is_some());
    assert!(assistant(1, "hello").surface().is_some());
    // Not "invalid" — unrepresentable: the field lives in the two message
    // payloads, so no other variant has one to set.
    let round = SessionEvent::new(
        2,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::AssistantRound(AssistantRoundEvent {
            turn_id: "t".into(),
            round: 0,
            response_id: String::new(),
            blocks: serde_json::json!([]),
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
        }),
    );
    assert!(round.surface().is_none());
}

#[test]
fn derived_messages_are_the_surface_in_order() {
    let events = [user(0, "q1"), assistant(1, "a1"), user(2, "q2")];
    let messages = derive_messages(&events, 0).unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["q1", "a1", "q2"]
    );
    assert_eq!(messages[1].role, super::super::message::Role::Assistant);
}

#[test]
fn a_compaction_summary_stands_where_the_messages_it_covers_used_to() {
    let mut events = vec![user(0, "q1"), assistant(1, "a1"), user(2, "q2")];
    events.push(SessionEvent::new(
        3,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: "turn-2".into(),
            content: "[summary of q1/a1]".into(),
            source: MessageSource::Compaction,
            surface: SurfacePlacement::replace(0, 1, vec![0, 1]),
        }),
    ));
    // No special case in the projection: the surface already resolved it.
    assert_eq!(
        derive_messages(&events, 0)
            .unwrap()
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["[summary of q1/a1]", "q2"]
    );
}

#[test]
fn tool_activity_reaches_the_model_as_a_note_not_as_replayed_rounds() {
    // A round and its calls are durable, but they are not conversation: a
    // later turn must not pay for work its assistant message already
    // summarized — while still being able to tell that tools ran at all.
    let events = vec![
        user(0, "run the tests"),
        SessionEvent::new(
            1,
            at("2026-08-31T00:00:00Z"),
            SessionEventKind::AssistantRound(AssistantRoundEvent {
                turn_id: "turn-1".into(),
                round: 0,
                response_id: String::new(),
                blocks: serde_json::json!([{"tool_call": "shell"}]),
                tokens_in: 100,
                tokens_out: 20,
                tokens_cached: 0,
            }),
        ),
        SessionEvent::new(
            2,
            at("2026-08-31T00:00:00Z"),
            SessionEventKind::ToolCallSettled(ToolCallSettledEvent {
                turn_id: "turn-1".into(),
                call_id: "c0".into(),
                call_index: 0,
                outcome: ToolOutcome::Succeeded,
                result: "test result: ok".into(),
                error: String::new(),
                elapsed_ms: 4100,
                structured: serde_json::Value::Null,
                output_paths: vec![],
            }),
        ),
        SessionEvent::new(
            3,
            at("2026-08-31T00:00:00Z"),
            SessionEventKind::AssistantMessage(AssistantMessageEvent {
                turn_id: "turn-1".into(),
                content: "全部通过".into(),
                tool_note: "1. shell → test result: ok".into(),
                surface: SurfacePlacement::append(),
            }),
        ),
    ];
    let messages = derive_messages(&events, 0).unwrap();
    assert_eq!(
        messages.len(),
        2,
        "the round and the call are not conversation"
    );
    assert_eq!(messages[1].content, "全部通过");
    assert_eq!(messages[1].tool_note, "1. shell → test result: ok");
}

fn cancelled(seq: u64, turn: &str, pristine: bool) -> SessionEvent {
    SessionEvent::new(
        seq,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::TurnCancelled {
            turn_id: turn.into(),
            pristine,
        },
    )
}

fn boundary(seq: u64) -> SessionEvent {
    SessionEvent::new(
        seq,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::ConversationBoundary { turn_id: None },
    )
}

fn said(seq: u64, turn: &str, text: &str, source: MessageSource) -> SessionEvent {
    SessionEvent::new(
        seq,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: turn.into(),
            content: text.into(),
            source,
            surface: SurfacePlacement::append(),
        }),
    )
}

/// The property the checkpoint rests on: folding a prefix, carrying it, and
/// folding the rest gives the history the cold read gives. Split at *every*
/// point, including inside a turn and across a compaction, because a cache
/// that is right only at convenient boundaries is a cache that hands the
/// model a conversation that never happened.
#[test]
fn a_checkpoint_plus_its_tail_is_the_whole_log() {
    let events = vec![
        said(0, "turn-1", "q1", MessageSource::User),
        assistant(1, "a1"),
        said(2, "turn-2", "q2", MessageSource::User),
        said(3, "turn-2", "and also this", MessageSource::Injected),
        assistant(4, "a2"),
        // A compaction shadows the first exchange.
        SessionEvent::new(
            5,
            at("2026-08-31T00:00:00Z"),
            SessionEventKind::UserMessage(UserMessageEvent {
                turn_id: "turn-3".into(),
                content: "earlier: q1/a1".into(),
                source: MessageSource::Compaction,
                surface: SurfacePlacement::replace(0, 1, vec![0, 1]),
            }),
        ),
        // A turn that asked and was stopped before it did anything.
        said(6, "turn-4", "never mind".into(), MessageSource::User),
        cancelled(7, "turn-4", true),
        said(8, "turn-5", "q3", MessageSource::User),
        assistant(9, "a3"),
        // `/new`: nothing leaves the surface, so the transcript is
        // unchanged — but every split has to fold the same boundary.
        boundary(10),
        said(11, "turn-6", "q4", MessageSource::User),
        assistant(12, "a4"),
    ];

    // `Message` is not comparable, so compare what it carries.
    fn shape(messages: &[Message]) -> Vec<(String, String, String)> {
        messages
            .iter()
            .map(|m| {
                (
                    format!("{:?}", m.role),
                    m.content.clone(),
                    m.tool_note.clone(),
                )
            })
            .collect()
    }

    let cold = SurfaceProjection::fold(&events, 0).unwrap();
    let expected = shape(&cold.messages().unwrap());
    assert_eq!(
        expected
            .iter()
            .map(|(_, content, _)| content.as_str())
            .collect::<Vec<_>>(),
        vec![
            "earlier: q1/a1",
            "q2\nand also this",
            "a2",
            "q3",
            "a3",
            "q4",
            "a4"
        ],
    );
    let replayed: Vec<String> = cold
        .replay()
        .unwrap()
        .iter()
        .map(|m| m.content.clone())
        .collect();
    assert_eq!(
        replayed,
        vec!["q4", "a4"],
        "the model starts after the line"
    );

    for split in 0..=events.len() {
        let head = SurfaceProjection::fold(&events[..split], 0).unwrap();
        assert!(head.resumable(0));
        let warm = head
            .extend(&events[split..])
            .unwrap_or_else(|e| panic!("split at {split}: {e}"));
        assert_eq!(
            shape(&warm.messages().unwrap()),
            expected,
            "a checkpoint after {split} events must not change the history"
        );
        assert_eq!(
            shape(&warm.replay().unwrap()),
            shape(&cold.replay().unwrap()),
            "nor where the boundary puts the model's replay"
        );
        assert_eq!(warm.surface, cold.surface, "split at {split}");
    }
}

/// `/new` draws a line; it does not delete. The transcript keeps every
/// word — that is what `komo run inspect`, episodic search and a client
/// hydrating the window read — and only the model's replay moves.
#[test]
fn a_boundary_moves_the_replay_and_leaves_the_transcript_whole() {
    let mut events = vec![
        said(0, "turn-1", "q1", MessageSource::User),
        assistant(1, "a1"),
        boundary(2),
        said(3, "turn-2", "q2", MessageSource::User),
        assistant(4, "a2"),
    ];
    let folded = SurfaceProjection::fold(&events, 0).unwrap();
    assert_eq!(folded.surface.nodes(), &[0, 1, 3, 4], "nothing left");
    assert_eq!(folded.messages().unwrap().len(), 4);
    assert_eq!(
        folded
            .replay()
            .unwrap()
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>(),
        vec!["q2", "a2"],
    );

    // A second line supersedes the first.
    events.push(boundary(5));
    let folded = SurfaceProjection::fold(&events, 0).unwrap();
    assert!(folded.replay().unwrap().is_empty());
    assert_eq!(folded.messages().unwrap().len(), 4, "still all of it");
}

/// The one thing a boundary must not do: strand a turn that was still in
/// flight when it was drawn. A turn suspended on an approval has a user
/// message and no answer under it; hiding that would leave its continuation
/// replying to a conversation it cannot see, and `/new` does not end turns.
#[test]
fn a_boundary_does_not_hide_a_turn_that_was_still_open() {
    let events = vec![
        said(0, "turn-1", "q1", MessageSource::User),
        assistant(1, "a1"),
        said(2, "turn-2", "删掉那个目录", MessageSource::User),
        // turn-2 suspends on an approval — no assistant message.
        boundary(3),
    ];
    let folded = SurfaceProjection::fold(&events, 0).unwrap();
    assert_eq!(
        folded
            .replay()
            .unwrap()
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>(),
        vec!["删掉那个目录"],
        "the unanswered question stays; the settled exchange before it goes"
    );
}

/// A checkpoint written before a retention cut is not resumable: every seq
/// below the new `truncated_before` means something different afterwards.
#[test]
fn a_truncation_retires_the_checkpoint_that_predates_it() {
    let folded = SurfaceProjection::fold(&[said(0, "t", "q", MessageSource::User)], 0).unwrap();
    assert!(folded.resumable(0));
    assert!(!folded.resumable(4), "the log has been cut since");
    let stale = SurfaceProjection {
        v: SURFACE_PROJECTION_VERSION + 1,
        ..folded
    };
    assert!(!stale.resumable(0), "and another shape is another meaning");
}

#[test]
fn a_pristine_cancel_takes_its_own_question_back_off_the_surface() {
    // The log keeps every event — an operator can still see that the user
    // asked and then stopped — but a later turn must not replay a question
    // nobody answered.
    let events = vec![
        said(0, "turn-1", "q1", MessageSource::User),
        assistant(1, "a1"),
        said(2, "turn-2", "oops, never mind", MessageSource::User),
        cancelled(3, "turn-2", true),
        said(4, "turn-3", "the real question", MessageSource::User),
    ];
    assert_eq!(
        derive_messages(&events, 0)
            .unwrap()
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["q1", "a1", "the real question"]
    );
}

#[test]
fn a_cancel_after_work_keeps_its_question() {
    // Tools ran. Those effects happened, so the turn is part of the
    // conversation whatever the user pressed afterwards.
    let events = vec![
        said(
            0,
            "turn-1",
            "delete the stale migrations",
            MessageSource::User,
        ),
        cancelled(1, "turn-1", false),
    ];
    assert_eq!(derive_messages(&events, 0).unwrap().len(), 1);
}

#[test]
fn an_interjection_joins_the_turn_it_interrupted() {
    // Not a second user message: several providers reject two in a row on
    // replay, and both halves really are one person's input for one turn.
    let events = vec![
        said(0, "turn-1", "看下 A", MessageSource::User),
        said(1, "turn-1", "顺便也看下 B", MessageSource::Injected),
        assistant(2, "两个都看了"),
    ];
    let messages = derive_messages(&events, 0).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "看下 A\n顺便也看下 B");
}

#[test]
fn a_seq_gap_refuses_the_log() {
    let events = [user(0, "hi"), assistant(2, "hello")];
    assert_eq!(
        fold_surface(&events, 0),
        Err(FoldError::SeqGap {
            expected: 1,
            found: 2
        })
    );
}

#[test]
fn a_truncated_log_folds_from_its_retention_base() {
    // After a truncate the log legitimately starts above zero; contiguity is
    // checked against the base, not against 0.
    let events = [user(100, "hi"), assistant(101, "hello")];
    let surface = fold_surface(&events, 100).unwrap();
    assert_eq!(surface.nodes(), &[100, 101]);
}

#[test]
fn a_base_is_sparse_on_purpose_but_the_retained_tail_is_not() {
    // A retention base keeps what still matters, so the seqs it did not keep
    // are missing *by decision*. Above the cut the same absence means the
    // log lost something, and a reader that served it would be inventing a
    // conversation. One boundary tells the two apart.
    let base_then_tail = [user(0, "q1"), user(2, "q2"), user(4, "q3")];
    assert_eq!(
        fold_surface(&base_then_tail, 4).unwrap().nodes(),
        &[0, 2, 4]
    );
    // The same events read as a complete log: seq 1 is now a hole.
    assert_eq!(
        fold_surface(&base_then_tail, 0),
        Err(FoldError::SeqGap {
            expected: 1,
            found: 2
        })
    );
    // A hole in the retained tail is refused however the base was cut.
    let torn = [user(0, "q1"), user(4, "q3"), user(6, "q4")];
    assert_eq!(
        fold_surface(&torn, 4),
        Err(FoldError::SeqGap {
            expected: 5,
            found: 6
        })
    );
    // Sparse is not the same as unordered: the base is written in seq order
    // and read back in it, so a reversal is a corrupt file, not a cut.
    let unordered = [user(2, "q2"), user(0, "q1"), user(4, "q3")];
    assert!(fold_surface(&unordered, 4).is_err());
}

#[test]
fn a_replacement_shadows_its_range_and_counts_as_a_rewrite() {
    let mut events = vec![
        user(0, "q1"),
        assistant(1, "a1"),
        user(2, "q2"),
        assistant(3, "a2"),
    ];
    events.push(SessionEvent::new(
        4,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: "turn-2".into(),
            content: "[summary of the first exchange]".into(),
            source: MessageSource::Compaction,
            surface: SurfacePlacement::replace(0, 1, vec![0, 1]),
        }),
    ));
    let surface = fold_surface(&events, 0).unwrap();
    // The summary stands where the two it covers used to; everything after
    // is untouched, and the shadowed events remain in the log.
    assert_eq!(surface.nodes(), &[4, 2, 3]);
    assert_eq!(surface.replace_generation(), 1);
}

#[test]
fn a_second_compaction_can_replace_the_first_summary() {
    // The reason a reader cannot "stop at the first compaction": a later
    // summary may target an earlier one, so the ops have to be folded.
    let mut surface = Surface::default();
    for seq in 0..4 {
        surface.apply(seq, &SurfacePlacement::append()).unwrap();
    }
    surface
        .apply(4, &SurfacePlacement::replace(0, 1, vec![0, 1]))
        .unwrap();
    assert_eq!(surface.nodes(), &[4, 2, 3]);
    surface
        .apply(5, &SurfacePlacement::replace(4, 2, vec![4, 2]))
        .unwrap();
    assert_eq!(surface.nodes(), &[5, 3]);
    assert_eq!(surface.replace_generation(), 2);
}

#[test]
fn an_invalid_replacement_refuses_rather_than_guessing() {
    let mut surface = Surface::default();
    for seq in 0..3 {
        surface.apply(seq, &SurfacePlacement::append()).unwrap();
    }

    // A range the surface does not hold.
    let off_surface = surface
        .clone()
        .apply(9, &SurfacePlacement::replace(7, 8, vec![7, 8]));
    assert!(matches!(
        off_surface,
        Err(FoldError::InvalidReplacement { seq: 9, .. })
    ));

    // Backwards.
    let backwards = surface
        .clone()
        .apply(9, &SurfacePlacement::replace(2, 0, vec![0, 1, 2]));
    assert!(matches!(
        backwards,
        Err(FoldError::InvalidReplacement { seq: 9, .. })
    ));

    // Covers three nodes but cites two: the uncited one would vanish from
    // the human transcript's account of what the summary replaced.
    let undercited = surface
        .clone()
        .apply(9, &SurfacePlacement::replace(0, 2, vec![0, 2]));
    assert!(matches!(
        undercited,
        Err(FoldError::InvalidReplacement { seq: 9, .. })
    ));
}

fn header(system: &str, tools: &[&str]) -> RequestHeaderEvent {
    RequestHeaderEvent {
        reason: HeaderReason::Initial,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        effort: String::new(),
        system: system.into(),
        tools: tools.iter().map(|t| (*t).to_string()).collect(),
        extra: None,
    }
}

#[test]
fn ten_identical_rounds_write_one_header_and_the_change_writes_a_second() {
    let steady = header("You are komo.", &["read", "shell"]);
    let mut log: Vec<SessionEvent> = Vec::new();
    let mut seq = 0u64;

    for _ in 0..10 {
        let latest = fold_request_header(&log, seq);
        if let Some(reason) = header_snapshot_reason(latest, &steady, false) {
            log.push(SessionEvent::new(
                seq,
                at("2026-08-31T00:00:00Z"),
                SessionEventKind::RequestHeader(RequestHeaderEvent {
                    reason,
                    ..steady.clone()
                }),
            ));
            seq += 1;
        }
        // Each round also logs *something*, so `seq` moves whether or not a
        // snapshot was written.
        log.push(user(seq, "another question"));
        seq += 1;
    }
    let headers: Vec<HeaderReason> = log
        .iter()
        .filter_map(|e| match &e.kind {
            SessionEventKind::RequestHeader(h) => Some(h.reason),
            _ => None,
        })
        .collect();
    assert_eq!(
        headers,
        vec![HeaderReason::Initial],
        "ten rounds, one snapshot"
    );

    // A tool mounts: exactly one `change`.
    let widened = header("You are komo.", &["read", "shell", "edit"]);
    let reason = header_snapshot_reason(fold_request_header(&log, seq), &widened, false);
    assert_eq!(reason, Some(HeaderReason::Change));
    log.push(SessionEvent::new(
        seq,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::RequestHeader(RequestHeaderEvent {
            reason: HeaderReason::Change,
            ..widened.clone()
        }),
    ));
    let change_seq = seq;

    // And a continuation always marks itself, identical or not.
    assert_eq!(
        header_snapshot_reason(fold_request_header(&log, change_seq), &widened, true),
        Some(HeaderReason::Resume)
    );
}

#[test]
fn a_header_fold_answers_with_the_envelope_in_force_at_that_seq() {
    let first = header("v1", &["read"]);
    let second = header("v2", &["read", "edit"]);
    let log = vec![
        SessionEvent::new(
            0,
            at("2026-08-31T00:00:00Z"),
            SessionEventKind::RequestHeader(first.clone()),
        ),
        user(1, "q"),
        SessionEvent::new(
            2,
            at("2026-08-31T00:00:00Z"),
            SessionEventKind::RequestHeader(second.clone()),
        ),
        user(3, "q2"),
    ];
    assert_eq!(fold_request_header(&log, 1).unwrap().system, "v1");
    assert_eq!(fold_request_header(&log, 3).unwrap().system, "v2");
    // Before any snapshot there is no envelope to report — not an empty one.
    assert!(fold_request_header(&log[1..], 1).is_none());
}

#[test]
fn a_route_change_is_not_an_envelope_change() {
    // The whole reason capacity lives outside `RequestHeaderEvent`: a
    // provider advertising a different context window must not force the
    // system prompt and every tool schema to be copied again.
    let steady = header("You are komo.", &["read"]);
    let log = vec![SessionEvent::new(
        0,
        at("2026-08-31T00:00:00Z"),
        SessionEventKind::RequestHeader(steady.clone()),
    )];
    assert_eq!(
        header_snapshot_reason(fold_request_header(&log, 0), &steady, false),
        None
    );

    let routes = vec![
        SessionEvent::new(
            1,
            at("2026-08-31T00:00:00Z"),
            SessionEventKind::RequestContext(RequestContextEvent {
                provider: "anthropic".into(),
                model: "claude-sonnet-4-6".into(),
                context_window: Some(200_000),
            }),
        ),
        SessionEvent::new(
            2,
            at("2026-08-31T00:00:00Z"),
            SessionEventKind::RequestContext(RequestContextEvent {
                provider: "anthropic".into(),
                model: "claude-sonnet-4-6".into(),
                context_window: None,
            }),
        ),
    ];
    // A route that advertises nothing clears the older capacity rather than
    // leaving a stale number in force.
    assert_eq!(
        fold_request_context(&routes, 2).unwrap().context_window,
        None
    );
    assert_eq!(
        fold_request_context(&routes, 1).unwrap().context_window,
        Some(200_000)
    );
}

#[test]
fn an_unchanged_request_header_needs_no_new_snapshot() {
    let initial = RequestHeaderEvent {
        reason: HeaderReason::Initial,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        effort: String::new(),
        system: "You are komo.".into(),
        tools: vec!["read".into(), "shell".into()],
        extra: None,
    };
    // Same inputs, different reason: still the same request envelope, so no
    // snapshot — this is what keeps a rendered system prompt out of every
    // round, the habit that made the old turn journal dwarf its transcript.
    let same = RequestHeaderEvent {
        reason: HeaderReason::Change,
        ..initial.clone()
    };
    assert!(!same.differs_from(&initial));

    let one_more_tool = RequestHeaderEvent {
        tools: vec!["read".into(), "shell".into(), "edit".into()],
        ..initial.clone()
    };
    assert!(one_more_tool.differs_from(&initial));
}
