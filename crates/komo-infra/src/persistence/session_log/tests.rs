use super::*;
use komo_core::domain::session_event::{
    MessageSource, SurfacePlacement, UserMessageEvent, fold_surface,
};

fn header() -> SessionHeader {
    SessionHeader {
        session_id: "019fad15-8199-7461-9d48-0a6c779f1c8d".into(),
        origin: "user".into(),
        workspace: None,
        created_at: time::OffsetDateTime::now_utc(),
        format_version: SESSION_EVENT_VERSION,
    }
}

fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("komo_session_log_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn say(text: &str) -> SessionEventKind {
    SessionEventKind::UserMessage(UserMessageEvent {
        turn_id: "turn-1".into(),
        content: text.into(),
        source: MessageSource::User,
        surface: SurfacePlacement::append(),
    })
}

/// The seqs a batch was handed — what these assertions are about.
fn seqs(appended: Vec<SessionEvent>) -> Vec<u64> {
    appended.into_iter().map(|event| event.seq).collect()
}

async fn open(dir: &Path) -> SessionLog {
    SessionLog::open_or_create(dir.to_path_buf(), header())
        .await
        .unwrap()
}

#[tokio::test]
async fn events_survive_a_reopen_with_their_assigned_seqs() {
    let dir = dir("roundtrip");
    let log = open(&dir).await;
    assert_eq!(
        seqs(log.append_batch(vec![say("a"), say("b")]).await),
        vec![0, 1]
    );
    log.durable_flush().await.unwrap();
    assert_eq!(seqs(log.append_batch(vec![say("c")]).await), vec![2]);
    log.durable_flush().await.unwrap();

    // A fresh handle takes `next_seq` from the active segment's last whole
    // record, so the manifest never has to be rewritten per append.
    let reopened = open(&dir).await;
    let events = reopened.load().await.unwrap();
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(seqs(reopened.append_batch(vec![say("d")]).await), vec![3]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unflushed_events_are_not_on_disk() {
    // The whole point of the barrier: an event that was only assigned a seq
    // must not read as a fact that survived the crash.
    let dir = dir("unflushed");
    let log = open(&dir).await;
    log.append_batch(vec![say("assigned but never flushed")])
        .await;
    drop(log);

    let reopened = open(&dir).await;
    assert!(reopened.load().await.unwrap().is_empty());
    // And the seq it handed out is handed out again — nothing consumed it.
    assert_eq!(
        seqs(reopened.append_batch(vec![say("real")]).await),
        vec![0]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_torn_tail_loses_only_the_half_record() {
    let dir = dir("torn");
    let log = open(&dir).await;
    log.append_batch(vec![say("one"), say("two")]).await;
    log.durable_flush().await.unwrap();
    drop(log);

    // A process killed mid-append: the last line has no terminator.
    let segment = dir.join("000000.jsonl");
    let mut raw = std::fs::read_to_string(&segment).unwrap();
    raw.push_str(r#"{"v":1,"seq":2,"at":"2026-09-01T10:30:00Z","type":"user/mess"#);
    std::fs::write(&segment, raw).unwrap();

    let reopened = open(&dir).await;
    let events = reopened.load().await.unwrap();
    assert_eq!(events.len(), 2, "the two whole records survive");
    // Writing resumes at the last *whole* record, so the torn one's seq is
    // reused rather than skipped.
    assert_eq!(
        seqs(reopened.append_batch(vec![say("three")]).await),
        vec![2]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A record a retired feature left behind — `task/spawned` here — must not
/// cost the conversation it sits in: it reads as inert, keeps its seq, and
/// the next append lands after it.
#[tokio::test]
async fn an_unknown_event_type_is_read_as_inert_and_the_session_still_opens() {
    let dir = dir("unknown");
    let log = open(&dir).await;
    log.append_batch(vec![say("one")]).await;
    log.durable_flush().await.unwrap();
    drop(log);

    let segment = dir.join("000000.jsonl");
    let mut raw = std::fs::read_to_string(&segment).unwrap();
    raw.push_str("{\"v\":1,\"seq\":1,\"at\":\"2026-09-01T10:30:00Z\",\"type\":\"task/spawned\",\"data\":{\"turn_id\":\"t1\",\"task_id\":\"k1\",\"kind\":\"shell\",\"label\":\"sleep 1\"}}\n");
    std::fs::write(&segment, raw).unwrap();

    let log = SessionLog::open_or_create(dir.clone(), header())
        .await
        .expect("a foreign record must not refuse the session");
    log.append_batch(vec![say("two")]).await;
    log.durable_flush().await.unwrap();

    let events = log.read_from(0).await.unwrap();
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "the inert record keeps its seq, so the next append is contiguous"
    );
    assert_eq!(events[1].kind, SessionEventKind::Unknown);
    assert_eq!(fold_surface(&events, 0).unwrap().nodes(), &[0, 2]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fill the active segment past its target without going through the real
/// write path, so the roll can be exercised without a megabyte of events.
async fn force_seal(log: &SessionLog) {
    log.state.lock().await.active_bytes = SEGMENT_TARGET_BYTES;
    assert!(log.seal_if_full().await.unwrap());
}

#[tokio::test]
async fn sealing_rolls_to_a_new_segment_and_keeps_reading_across_both() {
    let dir = dir("roll");
    let log = open(&dir).await;
    log.append_batch(vec![say("a"), say("b")]).await;
    log.durable_flush().await.unwrap();
    force_seal(&log).await;
    log.append_batch(vec![say("c")]).await;
    log.durable_flush().await.unwrap();

    assert!(dir.join("000001.jsonl").exists());
    let reopened = open(&dir).await;
    assert_eq!(
        reopened
            .load()
            .await
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    // Reading from the middle skips the sealed segment entirely.
    assert_eq!(
        reopened
            .read_from(2)
            .await
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect::<Vec<_>>(),
        vec![2]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_segment_with_unflushed_events_refuses_to_seal() {
    let dir = dir("seal_dirty");
    let log = open(&dir).await;
    log.append_batch(vec![say("a")]).await;
    log.state.lock().await.active_bytes = SEGMENT_TARGET_BYTES;
    assert!(log.seal_if_full().await.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

async fn sealed_log_with_base(name: &str) -> (PathBuf, SessionLog, RetentionBase) {
    let dir = dir(name);
    let log = open(&dir).await;
    log.append_batch(vec![say("old-1"), say("old-2")]).await;
    log.durable_flush().await.unwrap();
    force_seal(&log).await;
    log.append_batch(vec![say("kept")]).await;
    log.durable_flush().await.unwrap();
    // What survives from below the cut: here, one summarized message.
    let base = RetentionBase {
        through_seq: 1,
        events: vec![SessionEvent::now(1, say("[summary of old-1 and old-2]"))],
    };
    (dir, log, base)
}

#[tokio::test]
async fn retention_cuts_the_oldest_segment_and_stops_at_what_must_survive() {
    let dir = dir("retention_cut");
    let log = open(&dir).await;
    log.append_batch(vec![say("a"), say("b")]).await;
    log.durable_flush().await.unwrap();
    force_seal(&log).await; // segment 0 ends at seq 1
    log.append_batch(vec![say("c"), say("d")]).await;
    log.durable_flush().await.unwrap();
    force_seal(&log).await; // segment 1 ends at seq 3
    log.append_batch(vec![say("e")]).await;
    log.durable_flush().await.unwrap();

    // Inside budget there is nothing to cut, however much may be dropped.
    assert_eq!(log.retention_cut(u64::MAX, u64::MAX).await.unwrap(), None);
    // Over budget it sheds the *oldest* segment, not everything sealed.
    assert_eq!(log.retention_cut(0, u64::MAX).await.unwrap(), Some(1));
    // A turn that must survive from seq 1 puts that cut out of reach, and
    // the session stays over budget rather than lose it.
    assert_eq!(log.retention_cut(0, 1).await.unwrap(), None);
    // The next segment is still off limits while the floor sits inside it.
    assert_eq!(log.retention_cut(0, 2).await.unwrap(), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cut_keeps_the_surface_and_the_envelope_and_nothing_else() {
    // What survives is what a later turn needs: the conversation, and the
    // envelope that says how to reopen it. The turn markers, rounds and
    // tool calls that made it are the bulk, and they are what goes.
    use komo_core::domain::session_event::{
        HeaderReason, RequestHeaderEvent, ToolCallStartedEvent,
    };
    let events = vec![
        SessionEvent::now(
            0,
            SessionEventKind::TurnStarted {
                turn_id: "t1".into(),
                resumed_from: None,
            },
        ),
        SessionEvent::now(1, say("q1")),
        SessionEvent::now(
            2,
            SessionEventKind::RequestHeader(RequestHeaderEvent {
                reason: HeaderReason::Initial,
                provider: "codex".into(),
                model: "gpt-test".into(),
                effort: String::new(),
                system: "SYSTEM".into(),
                tools: vec![],
                extra: None,
            }),
        ),
        SessionEvent::now(
            3,
            SessionEventKind::ToolCallStarted(ToolCallStartedEvent {
                turn_id: "t1".into(),
                call_id: "c1".into(),
                call_index: 0,
                tool: "read".into(),
                args: "{}".into(),
            }),
        ),
        SessionEvent::now(
            4,
            SessionEventKind::TurnCompleted {
                turn_id: "t1".into(),
            },
        ),
        SessionEvent::now(5, say("q2")),
    ];
    let base = RetentionBase::cut(&events, 4, 0).unwrap();
    assert_eq!(base.through_seq, 4);
    assert_eq!(
        base.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2],
        "the message and the envelope, not the turn markers or the call"
    );
    // Above the cut is the retained tail's business, not the base's.
    assert!(base.events.iter().all(|e| e.seq <= 4));
}

#[test]
fn a_cut_keeps_the_conversation_boundary_it_passes() {
    // The boundary is not a surface node, so it would fall out with the
    // turn markers — and everything the operator drew a line under would
    // quietly be in front of the model again.
    use komo_core::domain::session_event::{AssistantMessageEvent, SurfaceProjection};
    let answered = |text: &str| {
        SessionEventKind::AssistantMessage(AssistantMessageEvent {
            turn_id: "turn-1".into(),
            content: text.into(),
            tool_note: String::new(),
            surface: SurfacePlacement::append(),
        })
    };
    let events = vec![
        SessionEvent::now(0, say("q1")),
        SessionEvent::now(1, answered("a1")),
        SessionEvent::now(2, SessionEventKind::ConversationBoundary { turn_id: None }),
        SessionEvent::now(3, say("q2")),
    ];
    let base = RetentionBase::cut(&events, 2, 0).unwrap();
    assert_eq!(
        base.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "the line survives the cut alongside the surface"
    );
    let mut folded: Vec<SessionEvent> = base.events.clone();
    folded.push(events[3].clone());
    let projection = SurfaceProjection::fold(&folded, 3).unwrap();
    assert_eq!(
        projection
            .replay()
            .unwrap()
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>(),
        vec!["q2"],
    );
}

#[test]
fn a_surviving_replacement_becomes_an_append_so_the_base_can_be_folded() {
    // A summary is kept because it is on the surface; the messages it
    // shadowed are exactly what the cut drops. Replaying its `replace`
    // would then look for a `start` that no longer exists and refuse the
    // whole log — so what survives is re-declared as an append.
    use komo_core::domain::session_event::{
        MessageSource, SurfacePlacement, UserMessageEvent, fold_surface,
    };
    let summary = SessionEvent::now(
        2,
        SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: "t2".into(),
            content: "[summary]".into(),
            source: MessageSource::Compaction,
            surface: SurfacePlacement::replace(0, 1, vec![0, 1]),
        }),
    );
    let events = vec![
        SessionEvent::now(0, say("q1")),
        SessionEvent::now(1, say("a1")),
        summary,
        SessionEvent::now(3, say("q2")),
    ];
    // Before the cut the summary shadows the two messages it covers.
    assert_eq!(fold_surface(&events, 0).unwrap().nodes(), &[2, 3]);

    let base = RetentionBase::cut(&events, 2, 0).unwrap();
    assert_eq!(
        base.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![2]
    );
    // Folding the base with the retained tail gives the same surface, which
    // it could not if the replacement had been kept as one.
    let mut rebuilt = base.events.clone();
    rebuilt.push(events[3].clone());
    assert_eq!(fold_surface(&rebuilt, 3).unwrap().nodes(), &[2, 3]);
}

#[tokio::test]
async fn a_base_that_kept_only_what_still_matters_still_reads() {
    // A real base is **sparse**: it holds the surface's messages and the
    // latest envelope, not every event below the cut. Its seqs therefore
    // have holes, by design — the events those seqs named are gone on
    // purpose, which is not the same as a log that lost them.
    let dir = dir("sparse_base");
    let log = open(&dir).await;
    log.append_batch(vec![say("q1"), say("a1"), say("q2"), say("a2")])
        .await;
    log.durable_flush().await.unwrap();
    force_seal(&log).await;
    log.append_batch(vec![say("q3")]).await;
    log.durable_flush().await.unwrap();
    // Keep the two questions, drop the two answers: seqs 0 and 2.
    let base = RetentionBase {
        through_seq: 3,
        events: vec![
            SessionEvent::now(0, say("q1")),
            SessionEvent::now(2, say("q2")),
        ],
    };
    log.truncate(base).await.unwrap();

    let reopened = open(&dir).await;
    let events = reopened.load().await.unwrap();
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 2, 4]
    );
    komo_core::domain::session_event::derive_messages(&events, reopened.truncated_before().await)
        .expect("a sparse base is a truncation, not a hole");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn truncate_replaces_the_covered_segment_with_its_base() {
    let (dir, log, base) = sealed_log_with_base("truncate").await;
    log.truncate(base).await.unwrap();

    assert!(
        !dir.join("000000.jsonl").exists(),
        "covered segment is gone"
    );
    assert!(dir.join("base.1.json").exists());

    let reopened = open(&dir).await;
    let events = reopened.load().await.unwrap();
    // Base first, then the retained tail — one fold, two sources.
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].seq, 1);
    assert_eq!(events[1].seq, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_crash_before_the_manifest_leaves_the_log_exactly_as_it_was() {
    // Step 1 committed, step 2 did not: an unreferenced base file.
    let (dir, log, base) = sealed_log_with_base("crash_pre_manifest").await;
    drop(log);
    std::fs::write(
        dir.join(base_file_name(base.through_seq)),
        serde_json::to_vec(&base).unwrap(),
    )
    .unwrap();

    let reopened = open(&dir).await;
    let events = reopened.load().await.unwrap();
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "the manifest still names both segments, so nothing changed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_crash_before_the_delete_costs_space_and_nothing_else() {
    // Steps 1 and 2 committed, step 3 did not: the covered segment is still
    // on disk but the manifest no longer names it.
    let (dir, log, base) = sealed_log_with_base("crash_pre_delete").await;
    let covered = dir.join("000000.jsonl");
    log.truncate(base).await.unwrap();
    std::fs::write(&covered, "{\"v\":1,\"seq\":0,\"at\":\"2026-09-01T10:30:00Z\",\"type\":\"turn/started\",\"data\":{\"turn_id\":\"t\"}}\n").unwrap();

    let reopened = open(&dir).await;
    let events = reopened.load().await.unwrap();
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2],
        "an unreferenced file is not part of the log"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_retention_base_refuses_instead_of_serving_the_tail() {
    // The base is authoritative once the manifest points at it. Silently
    // answering with the retained tail would report a session that lost its
    // history as one that never had any.
    let (dir, log, base) = sealed_log_with_base("lost_base").await;
    log.truncate(base).await.unwrap();
    drop(log);
    std::fs::remove_file(dir.join("base.1.json")).unwrap();

    let reopened = open(&dir).await;
    assert!(matches!(reopened.load().await, Err(LogError::Corrupt(_))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn truncate_never_cuts_into_the_active_segment() {
    // The active segment can hold a turn still running; deleting through it
    // would destroy the recovery unit.
    let dir = dir("active_cut");
    let log = open(&dir).await;
    log.append_batch(vec![say("a"), say("b")]).await;
    log.durable_flush().await.unwrap();
    let cut = RetentionBase {
        through_seq: 1,
        events: vec![],
    };
    assert!(log.truncate(cut).await.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_log_inside_its_budget_offers_nothing_to_retention() {
    let dir = dir("budget");
    let log = open(&dir).await;
    log.append_batch(vec![say("a")]).await;
    log.durable_flush().await.unwrap();
    force_seal(&log).await;
    assert!(
        log.retention_candidates(SESSION_RETAINED_BYTES)
            .await
            .unwrap()
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
