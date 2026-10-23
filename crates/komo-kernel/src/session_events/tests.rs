use super::*;
use crate::events::*;
use crate::types::digest::ContentHash;
use crate::types::ids::*;
use crate::types::plan::PlanSource;
use time::macros::datetime;

fn event(seq: u64, run: Option<&str>, payload: EventPayload) -> Event {
    Event {
        entry: None,
        v: EVENT_FORMAT_VERSION,
        seq: Seq(seq),
        event_id: EventId::from_raw(format!("e{seq}")),
        session: SessionId::from_raw("s"),
        run: run.map(RunId::from_raw),
        ts: datetime!(2026-10-23 00:00 UTC),
        payload,
    }
}
fn input(seq: u64, run: &str, text: &str) -> Event {
    event(
        seq,
        Some(run),
        EventPayload::RunAccepted(RunAccepted {
            request_key: RequestKey::new(run),
            input_hash: ContentHash::of_str(text),
            text: Some(text.into()),
            text_ref: None,
            source: PlanSource::Interactive {
                session: SessionId::from_raw("s"),
            },
            peer: None,
            model: None,
            effort: None,
            delegate: None,
            snapshot: None,
            interject: false,
        }),
    )
}
fn start(seq: u64, run: &str) -> Event {
    event(
        seq,
        Some(run),
        EventPayload::RunStarted(RunStarted {
            executor: ExecutorId::from_raw("ex"),
            generation: 1,
        }),
    )
}
fn reply(seq: u64, run: &str, text: &str) -> Event {
    event(
        seq,
        Some(run),
        EventPayload::MessageAssistant(MessageAssistant {
            round: 1,
            text: Some(text.into()),
            text_ref: None,
            tool_calls: vec![],
            provider_blocks: None,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
        }),
    )
}
fn append(state: &mut TreeState, event: &mut Event) {
    let entry = state.entry_for(event);
    event.entry = Some(Box::new(entry.clone()));
    state.apply(event, entry).unwrap();
}

#[test]
fn queued_input_links_after_the_previous_reply_without_copying_its_body() {
    let mut state = TreeState::default();
    let mut events = vec![
        input(1, "r1", "first"),
        start(2, "r1"),
        input(3, "r2", "queued"),
        reply(4, "r1", "first answer"),
        start(5, "r2"),
        reply(6, "r2", "second answer"),
    ];
    for event in &mut events {
        append(&mut state, event);
    }
    let tree = state.tree().unwrap();
    let texts = tree
        .project()
        .unwrap()
        .messages
        .into_iter()
        .filter(|m| m["role"] != "system")
        .map(|m| m["content"].clone())
        .collect::<Vec<_>>();
    assert_eq!(texts.len(), 4);
    assert_eq!(texts[0], "first");
    assert_eq!(texts[2], "queued");
    assert_eq!(TreeState::from_events(&events).unwrap(), state);
    let line = events[2].to_line().unwrap();
    assert_eq!(line.matches("\"queued\"").count(), 1);
    assert_eq!(Event::from_line(&line).unwrap().payload, events[2].payload);
}

#[test]
fn steering_links_to_the_host_tool_boundary_and_keeps_a_single_input() {
    let mut state = TreeState::default();
    let mut events = vec![
        input(1, "host", "task"),
        start(2, "host"),
        input(3, "steer", "correction"),
        reply(4, "host", "working"),
        event(
            5,
            Some("steer"),
            EventPayload::RunAbsorbed(RunAbsorbed {
                into: RunId::from_raw("host"),
            }),
        ),
        reply(6, "host", "done"),
    ];
    for event in &mut events {
        append(&mut state, event);
    }
    let tree = state.tree().unwrap();
    let path = tree.path(tree.leaf_id()).unwrap();
    let ids = path.iter().map(|e| e.id.as_str()).collect::<Vec<_>>();
    assert!(
        ids.iter().position(|&id| id == "e4").unwrap()
            < ids.iter().position(|&id| id == "e3").unwrap()
    );
    assert_eq!(
        tree.project()
            .unwrap()
            .messages
            .iter()
            .filter(|m| m["content"] == "correction")
            .count(),
        1
    );
    assert_eq!(state.head_for(&RunId::from_raw("host")), Some("e6"));
}

#[test]
fn navigation_does_not_change_an_existing_runs_recovery_path() {
    let mut state = TreeState::default();
    let mut events = vec![
        input(1, "r", "task"),
        start(2, "r"),
        reply(3, "r", "answer"),
    ];
    for e in &mut events {
        append(&mut state, e);
    }
    state.branch(Some("e1")).unwrap();
    assert_eq!(state.leaf.as_deref(), Some("e1"));
    assert_eq!(state.head_for(&RunId::from_raw("r")), Some("e3"));
    let restored = TreeState::from_events(&events).unwrap();
    assert_eq!(restored.leaf.as_deref(), Some("e3"));
}

#[test]
fn completion_references_the_saved_reply_and_hydrates_without_a_second_body() {
    let mut state = TreeState::default();
    let mut events = vec![
        input(1, "r", "task"),
        start(2, "r"),
        reply(3, "r", "unique answer"),
        event(
            4,
            Some("r"),
            EventPayload::RunCompleted(RunCompleted {
                final_message: Some("unique answer".into()),
                final_message_ref: None,
                rounds: 1,
            }),
        ),
    ];
    for e in &mut events {
        append(&mut state, e);
    }
    let line = events[3].to_line().unwrap();
    assert!(!line.contains("unique answer"));
    let mut restored = Event::from_line(&line).unwrap();
    state.hydrate_final(&mut restored);
    assert_eq!(restored.payload, events[3].payload);
}
