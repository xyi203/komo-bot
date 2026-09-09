use komo_core::domain::message::Role;

/// Append one message as an event and make it durable — what a turn does,
/// condensed for a fixture that only cares that the message is there.
async fn say(db: &Db, session_id: &str, message: &Message) {
    let kind = match message.role {
        Role::Assistant => SessionEventKind::AssistantMessage(
            komo_core::domain::session_event::AssistantMessageEvent {
                turn_id: "t".into(),
                content: message.content.clone(),
                tool_note: message.tool_note.clone(),
                surface: komo_core::domain::session_event::SurfacePlacement::append(),
            },
        ),
        _ => SessionEventKind::UserMessage(komo_core::domain::session_event::UserMessageEvent {
            turn_id: "t".into(),
            content: message.content.clone(),
            source: komo_core::domain::session_event::MessageSource::User,
            surface: komo_core::domain::session_event::SurfacePlacement::append(),
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
use komo_core::domain::run_projection::ProjectedStep;

/// A komo home of this test's own, wiped first.
///
/// The whole directory, not just the db file: a home now holds transcripts
/// beside `state.db`, and two tests sharing a directory would read each
/// other's conversations.
fn sqlite_url(name: &str) -> String {
    let home = std::env::temp_dir().join(format!("komo-test-{name}"));
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    format!("turso:{}", home.join("state.db").display())
}

/// Every `CREATE …` statement sqlite_master holds for one table.
async fn table_schema_sql(path: &std::path::Path, table: &str) -> Vec<String> {
    let raw = turso::Builder::new_local(path.to_string_lossy().as_ref())
        .build()
        .await
        .unwrap();
    let conn = raw.connect().unwrap();
    let mut rows = conn
        .query(
            &format!(
                "SELECT sql FROM sqlite_master \
                     WHERE tbl_name = '{table}' AND sql IS NOT NULL \
                     ORDER BY name"
            ),
            (),
        )
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        if let turso::Value::Text(sql) = row.get_value(0).unwrap() {
            out.push(sql);
        }
    }
    out
}

#[tokio::test]
async fn a_correspondent_resolves_to_one_session_and_carries_no_transcript() {
    let db = Db::connect(&sqlite_url("komo_find_by_peer.db"))
        .await
        .unwrap();
    let alice = ChannelPeer::new("feishu", "oc_alice");
    let bob = ChannelPeer::new("feishu", "oc_bob");
    // A platform's ids are its own: the same string on telegram is a
    // different correspondent.
    let elsewhere = ChannelPeer::new("telegram", "oc_alice");

    assert!(
        SessionRepository::find_by_peer(&db, &alice)
            .await
            .unwrap()
            .is_none(),
        "nobody has written yet"
    );

    let session = Session::new("019fad15-8199-7461-9d48-0a6c779f1c8d").with_channel(alice.clone());
    SessionRepository::save(&db, &session).await.unwrap();
    say(&db, &session.id, &Message::user("在吗")).await;

    let found = SessionRepository::find_by_peer(&db, &alice)
        .await
        .unwrap()
        .expect("alice's session");
    assert_eq!(found.id, session.id);
    assert_eq!(found.channel.as_ref(), Some(&alice));
    // Metadata only: a channel asks this on every inbound message just to
    // learn which conversation it is, and loading the transcript to answer
    // that would pay a turn's read before the turn starts.
    assert!(
        found.messages.is_empty(),
        "find_by_peer must not load the transcript"
    );

    for stranger in [&bob, &elsewhere] {
        assert!(
            SessionRepository::find_by_peer(&db, stranger)
                .await
                .unwrap()
                .is_none(),
            "{stranger:?} is a different correspondent"
        );
    }

    // A local session has no correspondent at all, and must not answer for
    // one — an empty address is not an address.
    SessionRepository::save(&db, &Session::new("019fad16-0000-7461-9d48-0a6c779f1c8d"))
        .await
        .unwrap();
    assert!(
        SessionRepository::find_by_peer(&db, &ChannelPeer::new("", ""))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn inbox_claims_once_and_reports_every_redelivery() {
    let db = Db::connect(&sqlite_url("komo_inbox_claim.db"))
        .await
        .unwrap();
    let origin = InboundOrigin::new("telegram", "42");
    let peer = InboundPeer::new(ChannelPeer::new("telegram", "7"), true, true);

    assert_eq!(
        db.claim(&origin, &peer, "telegram:7", "hi").await.unwrap(),
        InboxClaim::Fresh
    );
    db.complete(&origin).await.unwrap();
    assert_eq!(
        db.claim(&origin, &peer, "telegram:7", "hi").await.unwrap(),
        InboxClaim::Duplicate
    );

    // A claim that never completed still blocks its own redelivery: the row
    // exists from the moment it is claimed, which is what makes a crash
    // mid-turn safe.
    let midturn = InboundOrigin::new("telegram", "43");
    assert_eq!(
        db.claim(&midturn, &peer, "telegram:7", "second")
            .await
            .unwrap(),
        InboxClaim::Fresh
    );
    assert_eq!(
        db.claim(&midturn, &peer, "telegram:7", "second")
            .await
            .unwrap(),
        InboxClaim::Duplicate
    );

    // The key is the pair: platforms number their messages independently,
    // so the same id elsewhere is a different message.
    assert_eq!(
        db.claim(&InboundOrigin::new("feishu", "42"), &peer, "feishu:9", "hi")
            .await
            .unwrap(),
        InboxClaim::Fresh
    );

    // Local input has no platform to redeliver it — never a duplicate.
    for _ in 0..2 {
        assert_eq!(
            db.claim(&InboundOrigin::local(), &peer, "cli:1", "run")
                .await
                .unwrap(),
            InboxClaim::Fresh
        );
    }

    // What startup recovery reads: everything still claimed, oldest first,
    // carrying the peer the channel handed in — the completed row is gone
    // from it, and the one whose turn never ran is not.
    let unfinished = db.unfinished(50).await.unwrap();
    assert!(
        !unfinished.iter().any(|row| row.origin == origin),
        "a completed message is finished business"
    );
    let held = unfinished
        .iter()
        .find(|row| row.origin == midturn)
        .expect("the claim that never completed is offered back");
    assert_eq!(held.text, "second");
    assert_eq!(held.session_id, "telegram:7");
    assert_eq!(held.peer, peer, "the correspondent survives the restart");
    assert!(held.claimed_at > 0);
    assert!(
        unfinished
            .windows(2)
            .all(|w| w[0].claimed_at <= w[1].claimed_at),
        "oldest first"
    );
}

/// The wakeup table arrived after `komo.db` did, so an existing file only
/// gets it through `ensure_table` — which has to build exactly what
/// `push_schema` would, index included, or the sweep queries a table with
/// the right name and the wrong shape.
#[tokio::test]
async fn wakeup_table_ddl_matches_push_schema() {
    let fresh = std::env::temp_dir().join("komo_wakeup_ddl_fresh.db");
    crate::persistence::reset_test_db(&fresh);
    let db = Db::connect(&format!("turso:{}", fresh.display()))
        .await
        .unwrap();
    drop(db);
    let reference = table_schema_sql(&fresh, WAKEUP_TABLE).await;
    assert!(!reference.is_empty(), "push_schema created the table");

    let old = std::env::temp_dir().join("komo_wakeup_ddl_old.db");
    crate::persistence::reset_test_db(&old);
    let db = Db::connect(&format!("turso:{}", old.display()))
        .await
        .unwrap();
    drop(db);
    {
        let raw = turso::Builder::new_local(old.to_string_lossy().as_ref())
            .build()
            .await
            .unwrap();
        let conn = raw.connect().unwrap();
        conn.pragma_update("journal_mode", "'mvcc'").await.ok();
        conn.execute("DROP TABLE \"wakeup_records\"", ())
            .await
            .unwrap();
    }
    let db = Db::connect(&format!("turso:{}", old.display()))
        .await
        .unwrap();
    // And it is usable, not merely present.
    komo_core::domain::wakeup::WakeupRepository::save(
        &db,
        &komo_core::domain::wakeup::WakeupRegistration::new(
            "s1",
            komo_core::domain::session_event::Wakeup::UserReply,
            1_000,
        ),
    )
    .await
    .unwrap();
    drop(db);
    assert_eq!(table_schema_sql(&old, WAKEUP_TABLE).await, reference);
}

#[tokio::test]
async fn inbox_table_ddl_matches_push_schema() {
    let fresh = std::env::temp_dir().join("komo_inbox_ddl_fresh.db");
    crate::persistence::reset_test_db(&fresh);
    let db = Db::connect(&format!("turso:{}", fresh.display()))
        .await
        .unwrap();
    drop(db);
    let reference = table_schema_sql(&fresh, INBOX_TABLE).await;
    assert!(!reference.is_empty(), "push_schema created the table");

    // Simulate a state.db that predates the table: drop it, reconnect, and
    // `ensure_table` must rebuild it byte-identically.
    let old = std::env::temp_dir().join("komo_inbox_ddl_old.db");
    crate::persistence::reset_test_db(&old);
    let db = Db::connect(&format!("turso:{}", old.display()))
        .await
        .unwrap();
    drop(db);
    {
        let raw = turso::Builder::new_local(old.to_string_lossy().as_ref())
            .build()
            .await
            .unwrap();
        let conn = raw.connect().unwrap();
        conn.pragma_update("journal_mode", "'mvcc'").await.ok();
        conn.execute("DROP TABLE \"inbox_records\"", ())
            .await
            .unwrap();
    }
    let db = Db::connect(&format!("turso:{}", old.display()))
        .await
        .unwrap();
    drop(db);
    assert_eq!(table_schema_sql(&old, INBOX_TABLE).await, reference);
}

/// The link from an answer back to the memories that shaped it. Stored as
/// ids so the ledger cannot drift from what a memory now says, and kept
/// even when the memory is later edited or archived — the turn was still
/// built with it.
#[tokio::test]
async fn a_runs_memories_roundtrip() {
    use komo_core::domain::run::{RecalledMemories, Run};
    let db = Db::connect(&sqlite_url("komo_run_memories_test.db"))
        .await
        .unwrap();

    // Recall reaches the row from the turn's own `turn/memories` event, so
    // the projection carries whatever the fold saw — including nothing.
    let mut run = Run::start("api:s", "why did you say that");
    run.memories = RecalledMemories {
        pinned: vec!["mem-pinned".into()],
        recall: vec!["mem-a".into(), "mem-b".into()],
    };
    run.status = komo_core::domain::run::RunStatus::Done;
    commit_run(&db, &run, &[], 10).await;

    let back = RunRepository::get(&db, &run.id).await.unwrap().unwrap();
    assert_eq!(back.memories.pinned, ["mem-pinned"]);
    assert_eq!(back.memories.recall, ["mem-a", "mem-b"]);

    // A turn that used none records none.
    let mut plain = Run::start("api:s", "hi");
    plain.status = komo_core::domain::run::RunStatus::Done;
    commit_run(&db, &plain, &[], 11).await;
    assert!(
        RunRepository::get(&db, &plain.id)
            .await
            .unwrap()
            .unwrap()
            .memories
            .is_empty()
    );
}

#[tokio::test]
async fn run_resumed_from_roundtrips() {
    use komo_core::domain::run::Run;
    let db = Db::connect(&sqlite_url("komo_resumed_from_test.db"))
        .await
        .unwrap();
    let mut run = Run::start("cli:s", "continue");
    run.resumed_from = Some("run-original".to_string());
    commit_run(&db, &run, &[], 0).await;
    let back = RunRepository::get(&db, &run.id).await.unwrap().unwrap();
    assert_eq!(back.resumed_from.as_deref(), Some("run-original"));
}

#[tokio::test]
async fn run_ledger_roundtrips_with_ordered_steps() {
    use komo_core::domain::run::{Run, RunStatus, RunStep};
    let db = Db::connect(&sqlite_url("komo_run_repo_test.db"))
        .await
        .unwrap();

    let mut run = Run::start("cli:session-1", "do the thing");

    // Two steps out of seq order; `steps` must return them sorted.
    let step = |seq: i64, tool: &str, ok: bool| RunStep {
        run_id: run.id.clone(),
        seq,
        tool_name: tool.to_string(),
        args: format!("{{\"a\":{seq}}}"),
        result: if ok { "ok".into() } else { String::new() },
        error: if ok { String::new() } else { "boom".into() },
        ok,
        uncertain: false,
        started_at: 100 + seq,
        ended_at: 101 + seq,
        elapsed_ms: 250 + seq,
        structured: if ok {
            serde_json::json!({ "exit": 0 })
        } else {
            serde_json::Value::Null
        },
        output_paths: if ok {
            vec!["/tmp/komo/out.txt".to_string()]
        } else {
            Vec::new()
        },
        approved_by: if ok { "human".into() } else { String::new() },
        approval_waited_ms: if ok { 4_200 } else { 0 },
    };
    run.plan = "multistep:2".into();
    run.status = RunStatus::Done;
    run.final_output = "all done".into();
    run.ended_at = Some(999);
    commit_run(
        &db,
        &run,
        &[step(1, "time", true), step(0, "shell", false)],
        0,
    )
    .await;

    let got = RunRepository::get(&db, &run.id).await.unwrap().unwrap();
    assert_eq!(got.status, RunStatus::Done);
    assert_eq!(got.final_output, "all done");
    assert_eq!(got.plan, "multistep:2");
    assert_eq!(got.ended_at, Some(999));

    let steps = RunRepository::steps(&db, &run.id).await.unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].seq, 0); // sorted by seq
    assert_eq!(steps[0].tool_name, "shell");
    assert!(!steps[0].ok);
    assert_eq!(steps[0].error, "boom");
    assert_eq!(steps[1].seq, 1);
    assert!(steps[1].ok);
    // The additive columns round-trip, and an absent structured view reads
    // back as `Null` — absence, never an empty object.
    assert_eq!(steps[1].structured, serde_json::json!({ "exit": 0 }));
    assert_eq!(steps[1].output_paths, vec!["/tmp/komo/out.txt".to_string()]);
    assert!(steps[0].structured.is_null());
    assert!(steps[0].output_paths.is_empty());

    let recent = RunRepository::list(&db, 10).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].id, run.id);
}

#[tokio::test]
async fn run_prune_drops_old_runs_and_their_steps() {
    use komo_core::domain::run::{Run, RunStatus, RunStep};
    let db = Db::connect(&sqlite_url("komo_run_prune_test.db"))
        .await
        .unwrap();

    // Three runs at increasing start times, each with one step.
    let make = |id: &str, started_at: i64| Run {
        id: id.to_string(),
        session_id: "cli:s".to_string(),
        input: "x".to_string(),
        plan: String::new(),
        status: RunStatus::Done,
        final_output: String::new(),
        error: String::new(),
        recoverable: false,
        started_at,
        ended_at: Some(started_at + 1),
        tokens_in: 0,
        tokens_out: 0,
        tokens_cached: 0,
        resumed_from: None,
        memories: Default::default(),
        learned: false,
        outcome: String::new(),
    };
    for (through, (id, t)) in [("run-a", 100), ("run-b", 200), ("run-c", 300)]
        .into_iter()
        .enumerate()
    {
        let run = make(id, t);
        let step = RunStep {
            run_id: id.to_string(),
            seq: 0,
            tool_name: "time".into(),
            args: "{}".into(),
            result: "ok".into(),
            error: String::new(),
            ok: true,
            uncertain: false,
            started_at: t,
            ended_at: t + 1,
            elapsed_ms: 12,
            structured: serde_json::Value::Null,
            output_paths: Vec::new(),
            approved_by: String::new(),
            approval_waited_ms: 0,
        };
        commit_run(&db, &run, &[step], through as u64).await;
    }

    // Cutoff drops run-a (100) and run-b (200), keeps run-c (300).
    let removed = RunRepository::prune(&db, 250).await.unwrap();
    assert_eq!(removed, 2);

    let remaining = RunRepository::list(&db, 10).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, "run-c");
    // Steps of pruned runs are gone; the survivor's step stays.
    assert!(RunRepository::steps(&db, "run-a").await.unwrap().is_empty());
    assert_eq!(RunRepository::steps(&db, "run-c").await.unwrap().len(), 1);

    // Nothing older than the floor → no-op.
    assert_eq!(RunRepository::prune(&db, 0).await.unwrap(), 0);
}

#[tokio::test]
async fn reconcile_interrupted_fails_only_running_runs() {
    use komo_core::domain::run::{INTERRUPTED_ERROR, Run, RunStatus};
    let db = Db::connect(&sqlite_url("komo_run_reconcile_test.db"))
        .await
        .unwrap();

    // A run left mid-flight (status stays `Running`, as on a crash).
    let stuck = Run::start("cli:crashed", "long task");
    commit_run(&db, &stuck, &[], 0).await;

    // A run that finished cleanly before the restart — must be untouched.
    let mut done = Run::start("cli:ok", "quick task");
    done.status = RunStatus::Done;
    done.final_output = "reply".into();
    done.ended_at = Some(500);
    commit_run(&db, &done, &[], 0).await;

    let reconciled = RunRepository::reconcile_interrupted(&db, 1234)
        .await
        .unwrap();
    assert_eq!(reconciled, 1);

    let stuck = RunRepository::get(&db, &stuck.id).await.unwrap().unwrap();
    assert_eq!(stuck.status, RunStatus::Failed);
    assert_eq!(stuck.error, INTERRUPTED_ERROR);
    assert_eq!(stuck.ended_at, Some(1234));
    assert!(stuck.recoverable, "interrupted run must become resumable");

    let done = RunRepository::get(&db, &done.id).await.unwrap().unwrap();
    assert_eq!(done.status, RunStatus::Done);
    assert_eq!(done.final_output, "reply");
    assert!(!done.recoverable);

    // Idempotent: a second pass finds nothing still running.
    assert_eq!(
        RunRepository::reconcile_interrupted(&db, 9999)
            .await
            .unwrap(),
        0
    );
}

/// Startup reconciliation rules on turns that were *working* when the
/// process died. A turn that had stopped to wait is not residue: something
/// is scheduled to wake it, and flipping it to failed would both lie about
/// what happened and take it out of the set that can still come back.
#[tokio::test]
async fn reconciliation_leaves_a_suspended_turn_alone() {
    use komo_core::domain::run::{Run, RunStatus};
    let db = Db::connect(&sqlite_url("komo_run_reconcile_suspended.db"))
        .await
        .unwrap();

    let working = Run::start("cli:s", "long task");
    commit_run(&db, &working, &[], 0).await;

    let mut waiting = Run::start("cli:s", "needs approval");
    waiting.status = RunStatus::Suspended;
    commit_run(&db, &waiting, &[], 1).await;

    assert_eq!(
        RunRepository::reconcile_interrupted(&db, 1234)
            .await
            .unwrap(),
        1,
        "only the turn that was still working"
    );

    let waiting = RunRepository::get(&db, &waiting.id).await.unwrap().unwrap();
    assert_eq!(waiting.status, RunStatus::Suspended);
    assert!(!waiting.recoverable, "it is waiting, not lost");
    let working = RunRepository::get(&db, &working.id).await.unwrap().unwrap();
    assert_eq!(working.status, RunStatus::Failed);
    assert!(working.recoverable);
}

#[tokio::test]
async fn session_repository_lists_sessions() {
    let db = Db::connect(&sqlite_url("komo_session_repo_test.db"))
        .await
        .unwrap();
    let first = Session::with_roots("first", vec!["/home/u/alpha".into()]);
    let second = Session::new("second");

    SessionRepository::save(&db, &first).await.unwrap();
    // A later attempt to reuse the id with another workspace must not
    // rebind the existing conversation.
    SessionRepository::save(
        &db,
        &Session::with_roots("first", vec!["/home/u/beta".into()]),
    )
    .await
    .unwrap();
    say(&db, "first", &Message::user("hello")).await;
    SessionRepository::save(&db, &second).await.unwrap();

    let rows = SessionRepository::list(&db).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "first");
    assert_eq!(rows[0].roots, vec!["/home/u/alpha".to_string()]);
    assert_eq!(rows[0].user_turns(), 1);
    assert_eq!(rows[1].id, "second");
    // Everything that is not a task session is unbound.
    assert!(rows[1].roots.is_empty());
}

/// `/workspace add` widens a task, and the anchor stays where it was — a
/// relative path must not start meaning something else because a second project
/// was admitted.
#[tokio::test]
async fn set_roots_widens_a_task_workspace_and_keeps_the_anchor() {
    let db = Db::connect(&sqlite_url("komo_session_roots_test.db"))
        .await
        .unwrap();
    let session = Session::with_roots("task", vec!["/home/u/proj".into()]);
    SessionRepository::save(&db, &session).await.unwrap();

    SessionRepository::set_roots(
        &db,
        "task",
        &["/home/u/proj".to_string(), "/home/u/lib".to_string()],
    )
    .await
    .unwrap();

    let stored = SessionRepository::find(&db, "task").await.unwrap().unwrap();
    assert_eq!(
        stored.roots,
        vec!["/home/u/proj".to_string(), "/home/u/lib".to_string()]
    );

    // A session that does not exist is not an error — there is nothing to widen.
    SessionRepository::set_roots(&db, "nobody", &["/tmp".to_string()])
        .await
        .unwrap();
}

#[tokio::test]
async fn delete_empty_sessions_prunes_only_sessions_without_messages() {
    let db = Db::connect(&sqlite_url("komo_delete_empty_test.db"))
        .await
        .unwrap();

    // Session with messages — must survive.
    let keep = Session::new("keep");
    SessionRepository::save(&db, &keep).await.unwrap();
    say(&db, "keep", &Message::user("hello")).await;

    // Empty session — must be pruned.
    let drop = Session::new("drop");
    SessionRepository::save(&db, &drop).await.unwrap();

    // Another empty session.
    let drop2 = Session::new("drop2");
    SessionRepository::save(&db, &drop2).await.unwrap();

    let removed = SessionRepository::delete_empty_sessions(&db).await.unwrap();
    assert_eq!(removed, 2);

    let survivors = SessionRepository::list(&db).await.unwrap();
    assert_eq!(survivors.len(), 1);
    assert_eq!(survivors[0].id, "keep");
}

#[tokio::test]
async fn delete_empty_sessions_returns_zero_when_none_empty() {
    let db = Db::connect(&sqlite_url("komo_delete_none_test.db"))
        .await
        .unwrap();

    let s = Session::new("only");
    SessionRepository::save(&db, &s).await.unwrap();
    say(&db, "only", &Message::user("hi")).await;

    let removed = SessionRepository::delete_empty_sessions(&db).await.unwrap();
    assert_eq!(removed, 0);
    assert_eq!(SessionRepository::list(&db).await.unwrap().len(), 1);
}

#[tokio::test]
async fn db_session_todo_set_get_clear() {
    use komo_core::domain::todo::{TodoItem, TodoStatus};
    let db = Db::connect(&sqlite_url("komo_session_todo_test.db"))
        .await
        .unwrap();

    // Absent session reads as empty.
    assert!(
        SessionTodoRepository::get(&db, "s1")
            .await
            .unwrap()
            .is_empty()
    );

    let items = vec![
        TodoItem {
            content: "step one".to_string(),
            status: TodoStatus::InProgress,
            active_form: "doing step one".to_string(),
        },
        TodoItem {
            content: "step two".to_string(),
            status: TodoStatus::Pending,
            active_form: String::new(),
        },
    ];
    SessionTodoRepository::set(&db, "s1", &items).await.unwrap();
    let got = SessionTodoRepository::get(&db, "s1").await.unwrap();
    assert_eq!(got, items);

    // set replaces the whole list (upsert, not append).
    let replaced = vec![TodoItem {
        content: "only step".to_string(),
        status: TodoStatus::Completed,
        active_form: String::new(),
    }];
    SessionTodoRepository::set(&db, "s1", &replaced)
        .await
        .unwrap();
    assert_eq!(
        SessionTodoRepository::get(&db, "s1").await.unwrap(),
        replaced
    );

    // Scoped per session.
    assert!(
        SessionTodoRepository::get(&db, "s2")
            .await
            .unwrap()
            .is_empty()
    );

    SessionTodoRepository::clear(&db, "s1").await.unwrap();
    assert!(
        SessionTodoRepository::get(&db, "s1")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn db_pairing_upsert_approve_revoke_roundtrip() {
    use komo_core::domain::pairing::ApproveOutcome;

    let db = Db::connect(&sqlite_url("komo_pairing_repo_test.db"))
        .await
        .unwrap();
    let (request, code) = PairingRequest::mint("telegram", "777", "777");

    PairingRepository::upsert(&db, &request).await.unwrap();
    let found = PairingRepository::find(&db, "telegram", "777")
        .await
        .unwrap()
        .unwrap();
    // The plaintext code is never persisted — only the salted hash.
    assert_eq!(found.code_hash, request.code_hash);
    assert_ne!(found.code_hash, code);
    assert_eq!(
        found.status,
        komo_core::domain::pairing::PairingStatus::Pending
    );
    assert_eq!(
        PairingRepository::count_active_pending(&db, "telegram")
            .await
            .unwrap(),
        1
    );

    // Upsert with a fresh code replaces the row (one row per sender).
    let (refreshed, refreshed_code) = PairingRequest::mint("telegram", "777", "777");
    PairingRepository::upsert(&db, &refreshed).await.unwrap();
    assert_eq!(PairingRepository::list(&db).await.unwrap().len(), 1);

    assert!(matches!(
        PairingRepository::approve_code(&db, "NOSUCHCD")
            .await
            .unwrap(),
        ApproveOutcome::NotFound
    ));
    let ApproveOutcome::Approved(approved) = PairingRepository::approve_code(&db, &refreshed_code)
        .await
        .unwrap()
    else {
        panic!("expected approval");
    };
    assert_eq!(approved.sender_id, "777");
    let found = PairingRepository::find(&db, "telegram", "777")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        found.status,
        komo_core::domain::pairing::PairingStatus::Approved
    );

    assert!(
        PairingRepository::revoke(&db, "telegram:777")
            .await
            .unwrap()
    );
    assert!(
        !PairingRepository::revoke(&db, "telegram:777")
            .await
            .unwrap()
    );
    assert!(
        PairingRepository::find(&db, "telegram", "777")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn db_pairing_locks_out_after_repeated_bad_codes() {
    use komo_core::domain::pairing::{APPROVE_MAX_FAILURES, ApproveOutcome};

    let db = Db::connect(&sqlite_url("komo_pairing_lockout_test.db"))
        .await
        .unwrap();

    // The first APPROVE_MAX_FAILURES - 1 wrong codes are NotFound; the
    // attempt that reaches the limit locks out.
    for _ in 0..APPROVE_MAX_FAILURES - 1 {
        assert!(matches!(
            PairingRepository::approve_code(&db, "BADCODE1")
                .await
                .unwrap(),
            ApproveOutcome::NotFound
        ));
    }
    assert!(matches!(
        PairingRepository::approve_code(&db, "BADCODE1")
            .await
            .unwrap(),
        ApproveOutcome::Locked { .. }
    ));
}

#[tokio::test]
async fn home_repository_roundtrips_and_overwrites() {
    let db = Db::connect(&sqlite_url("komo_home_repo_test.db"))
        .await
        .unwrap();

    assert!(HomeRepository::get(&db).await.unwrap().is_none());

    HomeRepository::set(&db, "telegram:123456").await.unwrap();
    assert_eq!(
        HomeRepository::get(&db).await.unwrap().as_deref(),
        Some("telegram:123456")
    );

    // /sethome from another chat replaces the home (one row per key).
    HomeRepository::set(&db, "feishu:oc_home").await.unwrap();
    assert_eq!(
        HomeRepository::get(&db).await.unwrap().as_deref(),
        Some("feishu:oc_home")
    );
}

#[tokio::test]
async fn find_windowed_returns_recent_messages_in_order() {
    let db = Db::connect(&sqlite_url("komo_find_windowed_test.db"))
        .await
        .unwrap();
    let sid = "telegram:win";
    SessionRepository::save(&db, &Session::new(sid))
        .await
        .unwrap();
    // All six messages deliberately share one second-precision timestamp,
    // the way a fast turn's user/assistant pair does. Insertion order must
    // still survive, which is what ordering by the UUIDv7 id buys.
    for i in 0..6i64 {
        let msg = Message {
            role: if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: format!("m{i}"),
            timestamp: 1_000,
            tool_note: String::new(),
        };
        say(&db, sid, &msg).await;
    }

    // Window of 3 keeps the three most recent, still chronological.
    let windowed = SessionRepository::find_windowed(&db, sid, 3)
        .await
        .unwrap()
        .unwrap();
    let contents: Vec<_> = windowed.messages.iter().map(|m| &m.content).collect();
    assert_eq!(contents, ["m3", "m4", "m5"]);

    // limit == 0 loads the whole transcript (same as `find`).
    let full = SessionRepository::find_windowed(&db, sid, 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(full.messages.len(), 6);

    // A window larger than the transcript returns everything.
    let all = SessionRepository::find_windowed(&db, sid, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(all.messages.len(), 6);

    assert!(
        SessionRepository::find_windowed(&db, "nope", 3)
            .await
            .unwrap()
            .is_none()
    );
}

/// A db created before the session columns existed must gain them
/// **in place** on connect (additive ALTER) — an upgraded gateway must not
/// hard-fail every session query.
#[tokio::test]
async fn adds_missing_session_columns_in_place() {
    // Its own home, so a shared directory cannot carry a previous run's
    // session logs into this one.
    let home = std::env::temp_dir().join("komo-test-db-addcol");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    let path = home.join("state.db");

    // 1. Seed a turso file with the OLD session_records shape (without the
    //    added columns), then drop the handle. (connect skips push_schema
    //    for an existing file, so every table a session query touches must
    //    pre-exist, as it would in a real old db.)
    {
        let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.pragma_update("journal_mode", "'mvcc'").await.ok();
        conn.execute(
            "CREATE TABLE \"session_records\" (\
                 \"id\" TEXT NOT NULL, \"created_at\" BIGINT NOT NULL, PRIMARY KEY (\"id\"))",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO \"session_records\" VALUES ('cli:old', 100)",
            (),
        )
        .await
        .unwrap();
    }
    // 2. Connect via Db: ensure_columns adds the session columns in place.
    let db = Db::connect(&format!("turso:{}", path.display()))
        .await
        .unwrap();
    let session = SessionRepository::find(&db, "cli:old").await.unwrap();
    let session = session.expect("the pre-column session survives");

    // 3. An added column is fully usable: it reads as its default and is
    //    writable straight away.
    assert!(session.title.is_empty(), "new column defaults to empty");
    SessionRepository::set_title(&db, "cli:old", "old chat")
        .await
        .unwrap();
    let retitled = SessionRepository::find(&db, "cli:old")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retitled.title, "old chat");
}

/// A db created before `recoverable` existed must gain the column
/// **in place** on connect, like the session columns above — otherwise an
/// upgraded gateway 500s every run-ledger read ("no such column:
/// recoverable").
#[tokio::test]
async fn adds_missing_run_columns_in_place() {
    let path = std::env::temp_dir().join("komo_db_addcol_runs.db");
    crate::persistence::reset_test_db(&path);

    // 1. Seed a turso file with the OLD run_records shape (no recoverable):
    //    one crash-residue row, still `running` with the ended_at sentinel.
    {
        let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.pragma_update("journal_mode", "'mvcc'").await.ok();
        conn.execute(
            "CREATE TABLE \"run_records\" (\
                 \"id\" TEXT NOT NULL, \"session_id\" TEXT NOT NULL, \
                 \"input\" TEXT NOT NULL, \"plan\" TEXT NOT NULL, \
                 \"status\" TEXT NOT NULL, \"final_output\" TEXT NOT NULL, \
                 \"error\" TEXT NOT NULL, \"started_at\" BIGINT NOT NULL, \
                 \"ended_at\" BIGINT NOT NULL, PRIMARY KEY (\"id\"))",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO \"run_records\" VALUES \
                 ('r-old', 'cli:old', 'hi', 'respond', 'running', '', '', 100, 0)",
            (),
        )
        .await
        .unwrap();
        // And the settings row the projection keeps its watermark in.
        conn.execute(
            "CREATE TABLE \"setting_records\" (\
                 \"id\" TEXT NOT NULL, \"value\" TEXT NOT NULL, PRIMARY KEY (\"id\"))",
            (),
        )
        .await
        .unwrap();
        // The step table in its old shape too — the projection writes both,
        // so a migrated file has to be writable in both.
        conn.execute(
            "CREATE TABLE \"run_step_records\" (\
                 \"id\" TEXT NOT NULL, \"run_id\" TEXT NOT NULL, \
                 \"seq\" BIGINT NOT NULL, \"tool_name\" TEXT NOT NULL, \
                 \"args\" TEXT NOT NULL, \"result\" TEXT NOT NULL, \
                 \"error\" TEXT NOT NULL, \"ok\" BOOLEAN NOT NULL, \
                 \"started_at\" BIGINT NOT NULL, \"ended_at\" BIGINT NOT NULL, \
                 PRIMARY KEY (\"id\"))",
            (),
        )
        .await
        .unwrap();
    }
    // 2. Connect via Db: ensure_columns adds `recoverable` in place, and
    //    run-ledger reads work again.
    let db = Db::connect(&format!("turso:{}", path.display()))
        .await
        .unwrap();
    let runs = RunRepository::list(&db, 10).await.unwrap();
    assert_eq!(runs.len(), 1, "pre-migration run survives");
    assert!(!runs[0].recoverable, "new column defaults to false");

    // 3. The added column is fully writable: startup reconciliation flips
    //    the crash residue to failed + recoverable.
    let flipped = RunRepository::reconcile_interrupted(&db, 200)
        .await
        .unwrap();
    assert_eq!(flipped, 1);
    let runs = RunRepository::list(&db, 10).await.unwrap();
    assert!(runs[0].recoverable, "interrupted run became resumable");
    assert_eq!(
        (runs[0].tokens_in, runs[0].tokens_out, runs[0].tokens_cached),
        (0, 0, 0),
        "pre-column rows read as unknown usage, not as a free turn"
    );

    // 4. The token columns are writable on the same connection.
    let mut fresh = Run::start("cli:old", "how much did that cost");
    fresh.tokens_in = 900;
    fresh.tokens_out = 120;
    fresh.tokens_cached = 700;
    fresh.status = RunStatus::Done;
    let step = RunStep {
        run_id: fresh.id.clone(),
        seq: 0,
        tool_name: "time".into(),
        args: "{}".into(),
        result: "09:00".into(),
        error: String::new(),
        ok: true,
        uncertain: false,
        started_at: 100,
        ended_at: 101,
        elapsed_ms: 12,
        structured: serde_json::Value::Null,
        output_paths: Vec::new(),
        approved_by: String::new(),
        approval_waited_ms: 0,
    };
    commit_run(&db, &fresh, &[step], 0).await;
    let stored = RunRepository::get(&db, &fresh.id).await.unwrap().unwrap();
    assert_eq!(
        (stored.tokens_in, stored.tokens_out, stored.tokens_cached),
        (900, 120, 700)
    );
    // The step table's own added columns are writable on the same file.
    let steps = RunRepository::steps(&db, &fresh.id).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].elapsed_ms, 12);
}

/// The learning watermark: `unlearned` offers finished, not-yet-learned runs
/// oldest first, `mark_learned` retires them, and a turn still in flight is
/// never offered.
#[tokio::test]
async fn unlearned_offers_finished_runs_until_they_are_marked() {
    let db = Db::connect(&sqlite_url("komo_unlearned.db")).await.unwrap();

    // Committed newest-first, so the watermark cannot be the run's own
    // start: it has to advance per commit or the older ones are skipped.
    let through = std::cell::Cell::new(0u64);
    let save = async |id: &str, session: &str, status: RunStatus, at: i64| {
        let mut run = Run::start(session, "q");
        run.id = id.to_string();
        run.started_at = at;
        run.status = status;
        through.set(through.get() + 1);
        commit_run(&db, &run, &[], through.get()).await;
    };
    // Inserted newest-first to prove the ordering is the query's, not the
    // insertion order's.
    save("run-c", "cli:a", RunStatus::Done, 300).await;
    save("run-b", "cli:b", RunStatus::Failed, 200).await;
    save("run-a", "cli:a", RunStatus::Done, 100).await;

    let ids = |runs: Vec<Run>| runs.into_iter().map(|r| r.id).collect::<Vec<_>>();

    assert_eq!(
        ids(RunRepository::unlearned(&db, None, 10).await.unwrap()),
        ["run-a", "run-b", "run-c"],
        "oldest first, so a correction is learned after the claim it corrects"
    );
    assert_eq!(
        ids(RunRepository::unlearned(&db, Some("cli:a"), 10)
            .await
            .unwrap()),
        ["run-a", "run-c"],
        "scoping to one conversation is the query's job, not the caller's"
    );

    RunRepository::mark_learned(&db, &["run-a".to_string(), "run-c".to_string()])
        .await
        .unwrap();
    assert_eq!(
        ids(RunRepository::unlearned(&db, None, 10).await.unwrap()),
        ["run-b"],
        "a retired run is never offered again"
    );
    assert!(
        RunRepository::get(&db, "run-a")
            .await
            .unwrap()
            .unwrap()
            .learned
    );

    // A run still in flight has no decided outcome and no complete step
    // list, so it is not an episode yet.
    let running = Run::start("cli:a", "in flight");
    commit_run(&db, &running, &[], through.get() + 1).await;
    assert_eq!(
        ids(RunRepository::unlearned(&db, None, 10).await.unwrap()),
        ["run-b"]
    );
}

// ── run projection ───────────────────────────────────────────────────────

/// One finished turn's worth of events: a question, a round, a tool call
/// that settled, the recall that shaped it, the reply, and the terminal
/// event. Enough that every projected table has something in it.
async fn log_a_finished_turn(db: &Db, session_id: &str, turn: &str, memory: &str) {
    use komo_core::domain::session_event::{
        AssistantMessageEvent, AssistantRoundEvent, MessageSource, SurfacePlacement,
        ToolCallSettledEvent, ToolCallStartedEvent, ToolOutcome, UserMessageEvent,
    };
    let kinds = vec![
        SessionEventKind::TurnStarted {
            turn_id: turn.into(),
            resumed_from: None,
        },
        SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: turn.into(),
            content: "what time is it".into(),
            source: MessageSource::User,
            surface: SurfacePlacement::append(),
        }),
        SessionEventKind::AssistantRound(AssistantRoundEvent {
            turn_id: turn.into(),
            round: 0,
            response_id: "resp-1".into(),
            blocks: serde_json::json!([]),
            tokens_in: 120,
            tokens_out: 30,
            tokens_cached: 100,
        }),
        SessionEventKind::ToolCallStarted(ToolCallStartedEvent {
            turn_id: turn.into(),
            call_id: "c1".into(),
            call_index: 0,
            tool: "time".into(),
            args: "{}".into(),
        }),
        SessionEventKind::ToolCallSettled(ToolCallSettledEvent {
            turn_id: turn.into(),
            call_id: "c1".into(),
            call_index: 0,
            outcome: ToolOutcome::Succeeded,
            result: "09:00".into(),
            error: String::new(),
            elapsed_ms: 12,
            structured: serde_json::Value::Null,
            output_paths: Vec::new(),
        }),
        SessionEventKind::TurnMemories {
            turn_id: turn.into(),
            memories: komo_core::domain::run::RecalledMemories {
                pinned: Vec::new(),
                recall: vec![memory.to_string()],
            },
        },
        SessionEventKind::AssistantMessage(AssistantMessageEvent {
            turn_id: turn.into(),
            content: "it is 09:00".into(),
            tool_note: String::new(),
            surface: SurfacePlacement::append(),
        }),
        SessionEventKind::TurnCompleted {
            turn_id: turn.into(),
        },
    ];
    SessionEventRepository::append(db, session_id, kinds)
        .await
        .unwrap();
    SessionEventRepository::durable_flush(db, session_id)
        .await
        .unwrap();
}

/// Write a run and its steps the only way anything writes them now: as a
/// committed projection. `through` is the watermark, which every commit for
/// one session has to advance.
async fn commit_run(db: &Db, run: &Run, steps: &[RunStep], through: u64) {
    let projected = ProjectedRun {
        run: run.clone(),
        steps: steps
            .iter()
            .map(|step| ProjectedStep {
                step: step.clone(),
                settled: true,
            })
            .collect(),
        start_seq: 0,
    };
    RunProjectionStore::commit(db, &run.session_id, &[projected], through)
        .await
        .unwrap();
}

/// Commit the session's whole log as the projector would after a turn.
async fn project(db: &Db, session_id: &str) {
    let events = SessionEventRepository::events(db, session_id)
        .await
        .unwrap();
    let through = events.last().map(|e| e.seq).unwrap_or(0);
    let runs = project_runs(session_id, &events);
    RunProjectionStore::commit(db, session_id, &runs, through)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_ledger_rebuilds_from_the_log_alone() {
    let db = Db::connect(&sqlite_url("komo_projection.db"))
        .await
        .unwrap();
    log_a_finished_turn(&db, "s-proj", "t1", "m1").await;

    project(&db, "s-proj").await;

    let runs = RunRepository::list(&db, 10).await.unwrap();
    assert_eq!(runs.len(), 1, "the turn the log holds");
    let run = &runs[0];
    assert_eq!(run.id, "t1");
    assert_eq!(run.session_id, "s-proj");
    assert_eq!(run.input, "what time is it");
    assert_eq!(run.final_output, "it is 09:00");
    assert_eq!(run.status, RunStatus::Done);
    assert!(!run.recoverable, "a completed turn is not resumable");
    assert_eq!(
        (run.tokens_in, run.tokens_out, run.tokens_cached),
        (120, 30, 100)
    );
    assert_eq!(run.memories.recall, vec!["m1".to_string()]);

    let steps = RunRepository::steps(&db, "t1").await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].tool_name, "time");
    assert_eq!(steps[0].result, "09:00");
    assert_eq!(steps[0].elapsed_ms, 12);

    let pending = RunRepository::unlearned(&db, None, 10).await.unwrap();
    assert_eq!(pending.len(), 1, "nobody has learned from it yet");
}

/// A commit runs after every turn and again on a rebuild, over rows it has
/// already written. Duplicating a step would double the tool's history in
/// `run inspect`.
#[tokio::test]
async fn committing_the_same_fold_twice_changes_nothing() {
    let db = Db::connect(&sqlite_url("komo_projection_idem.db"))
        .await
        .unwrap();
    log_a_finished_turn(&db, "s-idem", "t1", "m1").await;

    project(&db, "s-idem").await;
    // Same watermark: the second call is the one that must not double-write.
    project(&db, "s-idem").await;
    // And once more with the watermark ignored, as a rebuild does.
    db.rebuild_projections().await.unwrap();

    assert_eq!(RunRepository::list(&db, 10).await.unwrap().len(), 1);
    assert_eq!(RunRepository::steps(&db, "t1").await.unwrap().len(), 1);
}

/// The two row-held fields. `outcome` is revised by a *later* turn and the
/// log never carries it; `learned` is a watermark that may predate the
/// events that now record it. A rebuild that overwrote either would throw
/// away the user's own verdict, or re-extract every turn ever learned from.
#[tokio::test]
async fn a_rebuild_keeps_what_the_log_does_not_know() {
    let db = Db::connect(&sqlite_url("komo_projection_merge.db"))
        .await
        .unwrap();
    log_a_finished_turn(&db, "s-merge", "t1", "m1").await;
    project(&db, "s-merge").await;

    RunRepository::set_outcome(&db, "t1", "{\"verdict\":\"success\"}")
        .await
        .unwrap();
    RunRepository::mark_learned(&db, &["t1".to_string()])
        .await
        .unwrap();

    db.rebuild_projections().await.unwrap();

    let run = RunRepository::get(&db, "t1").await.unwrap().unwrap();
    assert_eq!(run.outcome, "{\"verdict\":\"success\"}");
    assert!(run.learned, "a learned turn must not return to the backlog");
    assert!(
        RunRepository::unlearned(&db, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The session's own projection: a turn that stopped to wait is invisible
/// everywhere else, and the column that says so is a cache — clearing it and
/// re-folding the log has to put back exactly what the fold says.
#[tokio::test]
async fn the_wait_a_session_is_stopped_in_rebuilds_from_the_log() {
    use komo_core::domain::session_event::{TurnSuspendedEvent, Wakeup, WakeupKind};

    let db = Db::connect(&sqlite_url("komo_awaiting_rebuild.db"))
        .await
        .unwrap();
    SessionRepository::save(&db, &Session::new("s-wait"))
        .await
        .unwrap();
    SessionEventRepository::append(
        &db,
        "s-wait",
        vec![
            SessionEventKind::TurnStarted {
                turn_id: "t1".into(),
                resumed_from: None,
            },
            SessionEventKind::TurnSuspended(TurnSuspendedEvent {
                turn_id: "t1".into(),
                wakeup: Wakeup::Approval {
                    call_id: "c1".into(),
                },
                call_id: "c1".into(),
                summary: "shell: rm -rf build".into(),
                expires_at: Some(9_999),
            }),
        ],
    )
    .await
    .unwrap();
    SessionEventRepository::durable_flush(&db, "s-wait")
        .await
        .unwrap();

    let events = SessionEventRepository::events(&db, "s-wait").await.unwrap();
    SessionRepository::commit_awaiting(&db, "s-wait", &events)
        .await
        .unwrap();
    let waiting = SessionRepository::find(&db, "s-wait")
        .await
        .unwrap()
        .unwrap()
        .awaiting
        .expect("the session is waiting on an approval");
    assert_eq!(waiting.kind, WakeupKind::Approval);
    assert_eq!(waiting.summary, "shell: rm -rf build");

    // Clear the cache and let the log speak.
    db.write_awaiting("s-wait", None).await.unwrap();
    assert!(
        SessionRepository::find(&db, "s-wait")
            .await
            .unwrap()
            .unwrap()
            .awaiting
            .is_none()
    );
    db.rebuild_projections().await.unwrap();
    assert_eq!(
        SessionRepository::find(&db, "s-wait")
            .await
            .unwrap()
            .unwrap()
            .awaiting,
        project_awaiting(None, &events),
        "the column is a query index over the fold, not a second record"
    );
}

/// Two turns half a month apart, as bare open/close events. Timestamps a
/// prune cutoff can actually fall between — which the real log cannot give
/// a test, since it stamps every append with the same second.
fn folded_turns(session: &str, turns: &[(&str, i64)]) -> Vec<ProjectedRun> {
    let events: Vec<_> = turns
        .iter()
        .enumerate()
        .flat_map(|(i, (turn, at))| {
            let at = time::OffsetDateTime::from_unix_timestamp(*at).unwrap();
            [
                SessionEvent::new(
                    i as u64 * 2,
                    at,
                    SessionEventKind::TurnStarted {
                        turn_id: (*turn).into(),
                        resumed_from: None,
                    },
                ),
                SessionEvent::new(
                    i as u64 * 2 + 1,
                    at,
                    SessionEventKind::TurnCompleted {
                        turn_id: (*turn).into(),
                    },
                ),
            ]
        })
        .collect();
    project_runs(session, &events)
}

/// The completion criterion for making these rows a projection: `state.db`
/// is disposable, so deleting it entirely must cost nothing but the time to
/// fold the logs back. Every query an operator surface makes has to answer
/// the same afterwards.
#[tokio::test]
async fn a_deleted_state_db_rebuilds_the_ledger_from_the_logs() {
    let url = sqlite_url("komo_projection_rebuild.db");
    let path = std::path::PathBuf::from(url.trim_start_matches("turso:"));

    async fn snapshot(db: &Db) -> String {
        let runs = RunRepository::list(db, 10).await.unwrap();
        let mut out = format!("{runs:?}");
        for run in &runs {
            out.push_str(&format!(
                "{:?}",
                RunRepository::steps(db, &run.id).await.unwrap()
            ));
        }
        out.push_str(&format!(
            "{:?}",
            RunRepository::unlearned(db, None, 10).await.unwrap(),
        ));
        out
    }

    let before = {
        let db = Db::connect(&url).await.unwrap();
        log_a_finished_turn(&db, "s-a", "t1", "m1").await;
        log_a_finished_turn(&db, "s-b", "t2", "m1").await;
        project(&db, "s-a").await;
        project(&db, "s-b").await;
        snapshot(&db).await
    };

    // Drop the whole file — rows, watermarks and all. The logs under
    // `sessions/` are untouched, and they are the authority.
    crate::persistence::reset_test_db(&path);
    let db = Db::connect(&url).await.unwrap();
    assert!(
        RunRepository::list(&db, 10).await.unwrap().is_empty(),
        "a fresh state.db holds no ledger"
    );

    assert_eq!(db.rebuild_projections().await.unwrap(), 2);
    assert_eq!(snapshot(&db).await, before);
}

/// A pruned run must stay gone. Its log outlives it — retention keeps
/// whatever is still resumable or unlearned — so without a tombstone the
/// next commit hands the operator back exactly what they deleted.
#[tokio::test]
async fn a_pruned_run_is_never_projected_again() {
    let db = Db::connect(&sqlite_url("komo_projection_prune.db"))
        .await
        .unwrap();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let folded = folded_turns("s-prune", &[("t-old", now - 30 * 86_400), ("t-new", now)]);
    RunProjectionStore::commit(&db, "s-prune", &folded, 3)
        .await
        .unwrap();
    assert_eq!(RunRepository::list(&db, 10).await.unwrap().len(), 2);

    assert_eq!(
        RunRepository::prune(&db, now - 86_400).await.unwrap(),
        1,
        "only the older turn is stale"
    );

    // The same fold, offered again with a watermark that advances — which is
    // what a rebuild, or the next turn in this session, does.
    RunProjectionStore::commit(&db, "s-prune", &folded, 99)
        .await
        .unwrap();

    let runs = RunRepository::list(&db, 10).await.unwrap();
    assert_eq!(runs.len(), 1, "the tombstone outranks the fold");
    assert_eq!(
        runs[0].id, "t-new",
        "and the turn nobody pruned is still here"
    );
}

/// Whether an open turn is running or dead is the one fact the log cannot
/// hold — the startup reconciler rules on it. The fold's silence must not
/// overturn that ruling, or the next turn in the session puts every
/// interrupted run back to "running" and nothing is ever resumable.
#[tokio::test]
async fn a_reconciled_run_is_not_reopened_by_the_next_commit() {
    let db = Db::connect(&sqlite_url("komo_projection_interrupted.db"))
        .await
        .unwrap();
    // An open turn: `turn/started` with no terminal event.
    let events = vec![SessionEvent::new(
        0,
        time::OffsetDateTime::now_utc(),
        SessionEventKind::TurnStarted {
            turn_id: "t-open".into(),
            resumed_from: None,
        },
    )];
    let folded = project_runs("s-open", &events);
    RunProjectionStore::commit(&db, "s-open", &folded, 0)
        .await
        .unwrap();
    assert_eq!(
        RunRepository::get(&db, "t-open")
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Running
    );

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    assert_eq!(
        RunRepository::reconcile_interrupted(&db, now)
            .await
            .unwrap(),
        1
    );

    // The same fold again, as the session's next turn would commit it.
    RunProjectionStore::commit(&db, "s-open", &folded, 5)
        .await
        .unwrap();

    let run = RunRepository::get(&db, "t-open").await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error, INTERRUPTED_ERROR);
    assert!(
        run.recoverable,
        "and it is still the turn resume can pick up"
    );
}

/// `run prune --before` takes any date, including one past every run there
/// is. The fence still has to describe what was deleted rather than the
/// cutoff asked for, or every later turn falls behind it.
#[tokio::test]
async fn a_prune_of_everything_does_not_fence_off_later_turns() {
    let db = Db::connect(&sqlite_url("komo_projection_prune_all.db"))
        .await
        .unwrap();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let old = folded_turns("s-all", &[("t-old", now - 30 * 86_400)]);
    RunProjectionStore::commit(&db, "s-all", &old, 1)
        .await
        .unwrap();

    assert_eq!(
        RunRepository::prune(&db, now + 86_400).await.unwrap(),
        1,
        "a future cutoff prunes everything that exists"
    );

    let later = folded_turns("s-all", &[("t-later", now)]);
    RunProjectionStore::commit(&db, "s-all", &later, 9)
        .await
        .unwrap();
    let runs = RunRepository::list(&db, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, "t-later");
}

/// The watermark exists so a session whose log has not moved costs nothing
/// to re-commit. A stale one must still be able to catch up.
#[tokio::test]
async fn a_commit_that_would_not_advance_the_watermark_is_skipped() {
    let db = Db::connect(&sqlite_url("komo_projection_mark.db"))
        .await
        .unwrap();
    log_a_finished_turn(&db, "s-mark", "t1", "m1").await;
    project(&db, "s-mark").await;

    // A fold the projection has already committed, offered again with a
    // *lower* watermark: it must not be treated as new.
    let events = SessionEventRepository::events(&db, "s-mark").await.unwrap();
    let mut folded = project_runs("s-mark", &events);
    folded[0].run.input = "rewritten behind the log's back".into();
    RunProjectionStore::commit(&db, "s-mark", &folded, 0)
        .await
        .unwrap();
    let run = RunRepository::get(&db, "t1").await.unwrap().unwrap();
    assert_eq!(run.input, "what time is it");

    // A second turn moves the log on, and the projection follows.
    log_a_finished_turn(&db, "s-mark", "t2", "m2").await;
    project(&db, "s-mark").await;
    assert_eq!(RunRepository::list(&db, 10).await.unwrap().len(), 2);
}
