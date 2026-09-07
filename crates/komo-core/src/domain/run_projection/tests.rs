use super::*;
use crate::domain::run::RecalledMemories;
use crate::domain::session_event::{
    AssistantMessageEvent, AssistantRoundEvent, SurfacePlacement, ToolCallSettledEvent,
    ToolCallStartedEvent, UserMessageEvent,
};
use time::OffsetDateTime;

fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).unwrap()
}

fn ev(seq: u64, secs: i64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent::new(seq, at(secs), kind)
}

fn started(seq: u64, secs: i64, turn: &str) -> SessionEvent {
    ev(
        seq,
        secs,
        SessionEventKind::TurnStarted {
            turn_id: turn.into(),
            resumed_from: None,
        },
    )
}

fn asked(seq: u64, secs: i64, turn: &str, text: &str) -> SessionEvent {
    ev(
        seq,
        secs,
        SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: turn.into(),
            content: text.into(),
            source: MessageSource::User,
            surface: SurfacePlacement::append(),
        }),
    )
}

fn call_started(seq: u64, secs: i64, turn: &str, id: &str, tool: &str) -> SessionEvent {
    ev(
        seq,
        secs,
        SessionEventKind::ToolCallStarted(ToolCallStartedEvent {
            turn_id: turn.into(),
            call_id: id.into(),
            call_index: 0,
            tool: tool.into(),
            args: "{}".into(),
        }),
    )
}

fn call_settled(seq: u64, secs: i64, turn: &str, id: &str, outcome: ToolOutcome) -> SessionEvent {
    ev(
        seq,
        secs,
        SessionEventKind::ToolCallSettled(ToolCallSettledEvent {
            turn_id: turn.into(),
            call_id: id.into(),
            call_index: 0,
            outcome,
            result: "done".into(),
            error: String::new(),
            elapsed_ms: 12,
            structured: serde_json::Value::Null,
            output_paths: vec![],
        }),
    )
}

#[test]
fn a_finished_turn_projects_to_the_row_it_used_to_be_written_as() {
    let events = vec![
        started(0, 100, "t1"),
        asked(1, 100, "t1", "go"),
        ev(
            2,
            101,
            SessionEventKind::AssistantRound(AssistantRoundEvent {
                turn_id: "t1".into(),
                round: 0,
                response_id: "r1".into(),
                blocks: serde_json::Value::Null,
                tokens_in: 30,
                tokens_out: 4,
                tokens_cached: 20,
            }),
        ),
        call_started(3, 101, "t1", "c1", "read"),
        call_settled(4, 102, "t1", "c1", ToolOutcome::Succeeded),
        ev(
            5,
            103,
            SessionEventKind::AssistantMessage(AssistantMessageEvent {
                turn_id: "t1".into(),
                content: "here you go".into(),
                tool_note: String::new(),
                surface: SurfacePlacement::append(),
            }),
        ),
        ev(
            6,
            103,
            SessionEventKind::TurnCompleted {
                turn_id: "t1".into(),
            },
        ),
    ];
    let projected = project_runs("s1", &events);
    assert_eq!(projected.len(), 1);
    let ProjectedRun { run, steps, .. } = &projected[0];
    assert_eq!(run.id, "t1");
    assert_eq!(run.session_id, "s1");
    assert_eq!(run.input, "go");
    assert_eq!(run.status, RunStatus::Done);
    assert_eq!(run.final_output, "here you go");
    assert_eq!(run.plan, "1 tool call(s)");
    assert_eq!(
        (run.tokens_in, run.tokens_out, run.tokens_cached),
        (30, 4, 20)
    );
    assert_eq!(run.started_at, 100);
    assert_eq!(run.ended_at, Some(103));
    assert!(!run.recoverable);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].step.tool_name, "read");
    assert!(steps[0].step.ok);
    assert_eq!(steps[0].step.elapsed_ms, 12);
}

#[test]
fn a_turn_with_no_terminal_event_is_the_one_recovery_can_resume() {
    // The whole point of the fold: "interrupted" used to be a column another
    // process had to flip at startup, and a run whose row update was lost
    // read as still running forever. Here it is the absence of an event.
    let events = vec![
        started(0, 100, "t1"),
        asked(1, 100, "t1", "go"),
        call_started(2, 101, "t1", "c1", "shell"),
    ];
    let projected = project_runs("s1", &events);
    assert_eq!(projected[0].run.status, RunStatus::Running);
    assert!(projected[0].run.recoverable);
    assert_eq!(projected[0].run.ended_at, None);
    // The call is there, and marked as never having finished — the ledger
    // could not say this at all: its step row is written at settle, so a
    // call the process died inside leaves no row, and the record claims the
    // turn never made it.
    assert_eq!(projected[0].steps.len(), 1);
    assert!(!projected[0].steps[0].settled);
    assert!(!projected[0].steps[0].step.ok);
}

#[test]
fn a_cancelled_turn_is_failed_and_not_offered_for_resume() {
    // The user asked it to stop; there is nothing to hand back.
    let events = vec![
        started(0, 100, "t1"),
        asked(1, 100, "t1", "go"),
        ev(
            2,
            101,
            SessionEventKind::TurnCancelled {
                turn_id: "t1".into(),
                pristine: false,
            },
        ),
    ];
    let run = &project_runs("s1", &events)[0].run;
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error, CANCELLED_ERROR);
    assert!(!run.recoverable);
}

#[test]
fn an_approval_lands_on_the_call_it_gated() {
    use crate::domain::session_event::ApprovalResolvedEvent;

    // Two calls in flight; only one of them was gated. The approval belongs
    // to its own call, and it resolves while both are still open — so the
    // match is by id, like a settle.
    let events = vec![
        started(0, 100, "t1"),
        call_started(1, 100, "t1", "c1", "read"),
        call_started(2, 100, "t1", "c2", "shell"),
        ev(
            3,
            101,
            SessionEventKind::ApprovalResolved(ApprovalResolvedEvent {
                turn_id: "t1".into(),
                call_id: "c2".into(),
                call_index: 1,
                allowed: true,
                decided_by: "human".into(),
                reason: String::new(),
                waited_ms: 4_200,
            }),
        ),
        call_settled(4, 102, "t1", "c2", ToolOutcome::Succeeded),
        call_settled(5, 102, "t1", "c1", ToolOutcome::Succeeded),
    ];

    let steps = &project_runs("s1", &events)[0].steps;
    assert_eq!(steps[0].step.tool_name, "read");
    assert!(
        steps[0].step.approved_by.is_empty(),
        "a call nobody gated says nothing about approval"
    );
    assert_eq!(steps[1].step.tool_name, "shell");
    assert_eq!(steps[1].step.approved_by, "human");
    assert_eq!(steps[1].step.approval_waited_ms, 4_200);
}

/// The routine case (docs/bot-runtime.md §5.4): the turn stopped for an
/// approval at 03:00, the operator answered at 08:00, and the call ran in
/// the *continuation*. The answer was recorded against the turn that asked,
/// so without following the `resumed_from` chain the one call whose whole
/// story is its approval is the one the ledger cannot explain.
#[test]
fn a_call_re_dispatched_after_a_wait_carries_the_answer_that_licensed_it() {
    use crate::domain::session_event::ApprovalResolvedEvent;

    let events = vec![
        started(0, 100, "t1"),
        asked(1, 100, "t1", "tidy the tree"),
        call_started(2, 100, "t1", "c1", "shell"),
        ev(
            3,
            100,
            SessionEventKind::TurnSuspended(crate::domain::session_event::TurnSuspendedEvent {
                turn_id: "t1".into(),
                wakeup: crate::domain::session_event::Wakeup::Approval {
                    call_id: "c1".into(),
                },
                call_id: "c1".into(),
                summary: "run shell command: git push".into(),
                expires_at: None,
            }),
        ),
        ev(
            4,
            18_100,
            SessionEventKind::ApprovalResolved(ApprovalResolvedEvent {
                turn_id: "t1".into(),
                call_id: "c1".into(),
                call_index: 0,
                allowed: true,
                decided_by: "human".into(),
                reason: String::new(),
                waited_ms: 18_000_000,
            }),
        ),
        ev(
            5,
            18_100,
            SessionEventKind::TurnStarted {
                turn_id: "t2".into(),
                resumed_from: Some("t1".into()),
            },
        ),
        call_started(6, 18_100, "t2", "c1", "shell"),
        call_settled(7, 18_101, "t2", "c1", ToolOutcome::Succeeded),
    ];

    let runs = project_runs("s1", &events);
    let continuation = runs.iter().find(|r| r.run.id == "t2").unwrap();
    assert_eq!(continuation.steps[0].step.approved_by, "human");
    assert_eq!(continuation.steps[0].step.approval_waited_ms, 18_000_000);
}

#[test]
fn settles_match_their_call_by_id_not_by_arrival_order() {
    // A round runs concurrently, so the settles come back in completion
    // order. Pairing them by adjacency would file each result under the
    // wrong tool.
    let events = vec![
        started(0, 100, "t1"),
        call_started(1, 100, "t1", "a", "read"),
        call_started(2, 100, "t1", "b", "shell"),
        call_settled(3, 101, "t1", "b", ToolOutcome::Uncertain),
        call_settled(4, 102, "t1", "a", ToolOutcome::Succeeded),
    ];
    let steps = &project_runs("s1", &events)[0].steps;
    assert_eq!(steps[0].step.tool_name, "read");
    assert!(steps[0].step.ok);
    assert!(steps.iter().all(|s| s.settled));
    assert_eq!(steps[1].step.tool_name, "shell");
    assert!(
        steps[1].step.uncertain,
        "an uncertain call may still have landed"
    );
    assert!(!steps[1].step.ok);
}

#[test]
fn one_log_projects_every_turn_it_holds() {
    let events = vec![
        started(0, 100, "t1"),
        asked(1, 100, "t1", "first"),
        ev(
            2,
            101,
            SessionEventKind::TurnCompleted {
                turn_id: "t1".into(),
            },
        ),
        started(3, 200, "t2"),
        asked(4, 200, "t2", "second"),
        ev(
            5,
            201,
            SessionEventKind::TurnFailed {
                turn_id: "t2".into(),
                error: "boom".into(),
            },
        ),
    ];
    let projected = project_runs("s1", &events);
    assert_eq!(projected.len(), 2);
    assert_eq!(projected[0].run.input, "first");
    assert_eq!(projected[1].run.status, RunStatus::Failed);
    assert_eq!(projected[1].run.error, "boom");
    assert_eq!(projected[1].run.plan, "respond");
}

#[test]
fn a_continuation_projects_its_link_back_to_the_turn_it_picked_up() {
    let events = vec![
        started(0, 100, "t1"),
        ev(
            1,
            200,
            SessionEventKind::TurnStarted {
                turn_id: "t2".into(),
                resumed_from: Some("t1".into()),
            },
        ),
    ];
    let projected = project_runs("s1", &events);
    assert_eq!(projected[0].run.resumed_from, None);
    assert_eq!(projected[1].run.resumed_from, Some("t1".into()));
}

#[test]
fn recall_reaches_the_row_it_shaped() {
    let events = vec![
        started(0, 100, "t1"),
        ev(
            1,
            100,
            SessionEventKind::TurnMemories {
                turn_id: "t1".into(),
                memories: RecalledMemories {
                    pinned: vec!["m1".into()],
                    recall: vec!["m2".into()],
                },
            },
        ),
    ];
    let run = &project_runs("s1", &events)[0].run;
    assert_eq!(run.memories.pinned, vec!["m1".to_string()]);
    assert_eq!(run.memories.recall, vec!["m2".to_string()]);
}

#[test]
fn events_for_a_turn_that_never_opened_are_ignored() {
    // The log is contiguous and checked on read, so a turn with no
    // `turn/started` is not a gap to paper over with a synthesized run.
    let events = vec![
        asked(0, 100, "ghost", "go"),
        call_started(1, 100, "ghost", "c1", "read"),
    ];
    assert!(project_runs("s1", &events).is_empty());
}

/// A turn that stopped to wait is not crash residue. Recovery must not
/// offer it — its return is already scheduled, and resuming it by hand
/// would run the same work twice — and the startup reconciler must not
/// rule it dead.
#[test]
fn a_suspended_turn_is_waiting_rather_than_interrupted() {
    use crate::domain::session_event::{TurnSuspendedEvent, Wakeup};

    let suspended = |seq: u64, turn: &str| {
        ev(
            seq,
            200,
            SessionEventKind::TurnSuspended(TurnSuspendedEvent {
                turn_id: turn.into(),
                wakeup: Wakeup::Approval {
                    call_id: "c1".into(),
                },
                call_id: "c1".into(),
                summary: "waiting for approval to run: rm -rf build".into(),
                expires_at: Some(200 + 86_400),
            }),
        )
    };

    // Crashed *before* suspending: nothing scheduled its return, so it is
    // the interrupted turn recovery exists for.
    let working = vec![started(0, 100, "t1"), asked(1, 100, "t1", "go")];
    let run = &project_runs("s1", &working)[0].run;
    assert_eq!(run.status, RunStatus::Running);
    assert!(run.recoverable);

    // Crashed *after* suspending: it is parked, and something else will
    // wake it.
    let mut waiting = working.clone();
    waiting.push(suspended(2, "t1"));
    let run = &project_runs("s1", &waiting)[0].run;
    assert_eq!(run.status, RunStatus::Suspended);
    assert!(!run.recoverable, "its return is scheduled, not lost");
    assert!(
        !run.status.is_terminal(),
        "and it is still not an episode: nothing has been decided"
    );
}

/// The wake is what ends the wait. Between it and the continuation's own
/// `turn/started` the turn is running again — and if the process dies in
/// that window, it is interrupted like any other.
#[test]
fn a_fired_wakeup_takes_the_turn_out_of_waiting() {
    use crate::domain::session_event::{TurnSuspendedEvent, Wakeup, WakeupCause, WakeupFiredEvent};

    let events = vec![
        started(0, 100, "t1"),
        asked(1, 100, "t1", "go"),
        ev(
            2,
            200,
            SessionEventKind::TurnSuspended(TurnSuspendedEvent {
                turn_id: "t1".into(),
                wakeup: Wakeup::UserReply,
                call_id: "c1".into(),
                summary: "asked the user which environment".into(),
                expires_at: None,
            }),
        ),
        ev(
            3,
            300,
            SessionEventKind::WakeupFired(WakeupFiredEvent {
                turn_id: "t1".into(),
                wakeup_id: "wk-1".into(),
                cause: WakeupCause::Reply,
                payload: "staging".into(),
            }),
        ),
    ];

    let run = &project_runs("s1", &events)[0].run;
    assert_eq!(run.status, RunStatus::Running);
    assert!(
        run.recoverable,
        "woken and then lost is an interrupted turn again"
    );
}

#[test]
fn a_claimed_turn_is_not_offered_for_resume_again() {
    // The continuation's own `turn/started` is the claim. Without it a
    // crashed turn stays resumable forever and every restart re-runs it.
    let events = vec![
        started(0, 100, "t1"),
        asked(1, 100, "t1", "go"),
        ev(
            2,
            200,
            SessionEventKind::TurnStarted {
                turn_id: "t2".into(),
                resumed_from: Some("t1".into()),
            },
        ),
    ];
    let runs = project_runs("s1", &events);
    assert!(
        !runs[0].run.recoverable,
        "t1 has a continuation, so it is not the log's open turn any more"
    );
    assert!(
        runs[1].run.recoverable,
        "t2 has no terminal event of its own yet"
    );
    assert_eq!(runs[1].run.resumed_from.as_deref(), Some("t1"));
    assert_eq!(
        runs[1].run.input, "go",
        "a continuation is answering the same question, and the row says so"
    );
}

#[test]
fn a_resumable_turn_holds_the_log_back_to_its_first_attempt() {
    // A→B→C, C still open: retention may not cut A's rounds away, because
    // rebuilding C replays them.
    let events = vec![
        started(0, 100, "A"),
        asked(1, 100, "A", "go"),
        ev(
            2,
            200,
            SessionEventKind::TurnStarted {
                turn_id: "B".into(),
                resumed_from: Some("A".into()),
            },
        ),
        ev(
            3,
            300,
            SessionEventKind::TurnStarted {
                turn_id: "C".into(),
                resumed_from: Some("B".into()),
            },
        ),
    ];
    let runs = project_runs("s1", &events);
    let open = runs.iter().find(|p| p.run.id == "C").unwrap();
    assert!(open.run.recoverable, "C is the attempt still in flight");
    assert_eq!(open.start_seq, 3);
    assert_eq!(
        replay_floor(&runs, open),
        0,
        "the floor reaches back to the turn that first asked the question"
    );
    // A turn nobody resumed answers with its own start.
    let first = runs.iter().find(|p| p.run.id == "A").unwrap();
    assert_eq!(replay_floor(&runs, first), 0);
}

#[test]
fn the_learning_watermark_folds_from_either_verdict() {
    // Both verdicts retire the turn. "Considered and declined" has to read
    // as learned, or every sweep offers the same turn again forever.
    for verdict in [
        SessionEventKind::LearningCompleted {
            turn_id: "t1".into(),
        },
        SessionEventKind::LearningSkipped {
            turn_id: "t1".into(),
            reason: "cancelled turn".into(),
        },
    ] {
        let events = vec![
            started(0, 100, "t1"),
            ev(
                1,
                101,
                SessionEventKind::TurnCompleted {
                    turn_id: "t1".into(),
                },
            ),
            ev(2, 102, verdict),
        ];
        assert!(project_runs("s1", &events)[0].run.learned);
    }
}

#[test]
fn a_turn_nobody_has_learned_from_folds_unlearned() {
    let events = vec![
        started(0, 100, "t1"),
        ev(
            1,
            101,
            SessionEventKind::TurnCompleted {
                turn_id: "t1".into(),
            },
        ),
        // Another turn's watermark says nothing about this one.
        started(2, 102, "t2"),
        ev(
            3,
            103,
            SessionEventKind::LearningCompleted {
                turn_id: "t2".into(),
            },
        ),
    ];
    let runs = project_runs("s1", &events);
    assert!(!runs[0].run.learned);
    assert!(runs[1].run.learned);
}
