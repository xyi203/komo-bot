use super::*;
use komo_core::domain::session::ChannelPeer;
use komo_core::domain::session_event::SurfaceRole;
use std::time::Duration;

#[test]
fn classify_matches_commands_case_insensitively() {
    assert_eq!(classify("/new"), Command::New);
    assert_eq!(classify("  /CLEAR "), Command::New);
    assert_eq!(classify("/approve"), Command::Approve(Answer::Once, None));
    assert_eq!(
        classify("/approve session"),
        Command::Approve(Answer::Session, None)
    );
    assert_eq!(classify("/deny"), Command::Deny(None, None));
    assert_eq!(classify("/sethome"), Command::SetHome);
    assert_eq!(classify(" /SetHome "), Command::SetHome);
    assert_eq!(classify("/wechat login"), Command::WechatLogin);
    assert_eq!(classify(" /WeChat "), Command::WechatLogin);
    assert_eq!(classify("hello"), Command::Plain("hello".to_string()));
    // A leading slash inside a longer message is plain text.
    assert_eq!(
        classify("/approve the budget"),
        Command::Plain("/approve the budget".to_string())
    );
}

#[test]
fn deny_takes_a_free_text_reason_for_the_model() {
    assert_eq!(
        classify("/deny 用 trash 代替 rm"),
        Command::Deny(Some("用 trash 代替 rm".to_string()), None)
    );
    // The verb is case-insensitive; the reason keeps its case.
    assert_eq!(
        classify("/DENY Use Trash"),
        Command::Deny(Some("Use Trash".to_string()), None)
    );
    // Whitespace-only argument is the same as a bare `/deny`.
    assert_eq!(classify("/deny    "), Command::Deny(None, None));
}

/// A routine's approval is answered from the home chat, so the command has
/// to be able to name a wait that belongs to another session — while
/// `/deny <reason>` keeps taking free text.
#[test]
fn an_approval_command_can_name_the_wait_it_answers() {
    assert_eq!(
        classify("/approve wk-0199abc"),
        Command::Approve(Answer::Once, Some("wk-0199abc".to_string()))
    );
    assert_eq!(
        classify("/approve wk-0199abc session"),
        Command::Approve(Answer::Session, Some("wk-0199abc".to_string()))
    );
    assert_eq!(
        classify("/deny wk-0199abc 太危险了"),
        Command::Deny(Some("太危险了".to_string()), Some("wk-0199abc".to_string()))
    );
    // Without the prefix it is a reason, not an id.
    assert_eq!(
        classify("/deny 太危险了"),
        Command::Deny(Some("太危险了".to_string()), None)
    );
}

#[test]
fn classify_parses_pair_subcommands_preserving_arg_case() {
    assert_eq!(classify("/pair"), Command::Pair(PairAction::List));
    assert_eq!(classify("/pair list"), Command::Pair(PairAction::List));
    // The verb is case-insensitive but the code/id keep their case.
    assert_eq!(
        classify("/PAIR approve aB12cD34"),
        Command::Pair(PairAction::Approve("aB12cD34".to_string()))
    );
    assert_eq!(
        classify("/pair revoke feishu:ou_AbC"),
        Command::Pair(PairAction::Revoke("feishu:ou_AbC".to_string()))
    );
    // Missing argument → usage, not a turn.
    assert_eq!(classify("/pair approve"), Command::Pair(PairAction::Usage));
    assert_eq!(
        classify("/pair frobnicate"),
        Command::Pair(PairAction::Usage)
    );
}

#[tokio::test]
async fn resolve_returns_false_when_nothing_pending() {
    let state = ApprovalState::new();
    assert!(!state.resolve("s1", Answer::Once));
}

fn sample_pending() -> PendingApproval {
    PendingApproval {
        summary: "run shell command: ls".to_string(),
        detail: None,
        risk: "normal".to_string(),
    }
}

/// The prompt cache the GUI's modal polls: what is being asked, until it
/// is answered.
#[tokio::test]
async fn a_noted_prompt_is_visible_until_it_is_answered() {
    let state = ApprovalState::new();
    state.note_pending("s1", sample_pending());
    assert_eq!(
        state.pending_info("s1").map(|p| p.summary),
        Some("run shell command: ls".to_string())
    );
    assert_eq!(
        state.resolve_scoped("s1", Answer::Session),
        Some(Answer::Session)
    );
    assert!(state.pending_info("s1").is_none());
    // And answering twice reports the second time as nothing pending.
    assert!(!state.resolve("s1", Answer::Once));
}

/// One round can gate several calls ("打开热水器和空调" is two), and they are
/// one question to the person reading them — so they queue rather than
/// replacing each other. As a single slot the newest prompt overwrote the rest.
#[tokio::test]
async fn a_rounds_questions_queue_instead_of_replacing_each_other() {
    let state = ApprovalState::new();
    assert_eq!(
        state.note_pending(
            "s1",
            PendingApproval {
                summary: "switch.turn_on".to_string(),
                ..sample_pending()
            }
        ),
        1
    );
    assert_eq!(
        state.note_pending(
            "s1",
            PendingApproval {
                summary: "climate.set_temperature".to_string(),
                ..sample_pending()
            }
        ),
        2,
        "the second question is the round's second, not a replacement"
    );
    assert_eq!(
        state.pending_info("s1").map(|p| p.summary),
        Some("switch.turn_on".to_string()),
        "the modal renders the front of the queue, not the last to arrive"
    );
    // Re-asking the same thing does not make the round look longer.
    assert_eq!(
        state.note_pending(
            "s1",
            PendingApproval {
                summary: "switch.turn_on".to_string(),
                ..sample_pending()
            }
        ),
        2
    );
    // One answer clears the whole round.
    assert!(state.resolve("s1", Answer::Once));
    assert!(state.pending_info("s1").is_none());
}

/// The reason the queue had to stop being a single slot. `resolve_scoped` reads
/// the risk off what is pending, so an ordinary action arriving after a
/// dangerous one used to overwrite it — and with it the narrowing that keeps
/// "always" from ever applying to the irreversible one.
#[tokio::test]
async fn a_dangerous_question_narrows_the_answer_even_beside_an_ordinary_one() {
    let state = ApprovalState::new();
    state.note_pending(
        "s1",
        PendingApproval {
            summary: "rm -rf /data".to_string(),
            detail: None,
            risk: "dangerous".to_string(),
        },
    );
    state.note_pending(
        "s1",
        PendingApproval {
            summary: "write a file".to_string(),
            detail: None,
            risk: "normal".to_string(),
        },
    );
    assert_eq!(
        state.resolve_scoped("s1", Answer::Always),
        Some(Answer::Once),
        "the strictest question in the batch decides what the one answer may widen to"
    );
}

/// A dangerous action is approved for the one call it was asked about,
/// whatever the user typed: widening pre-approves a *later* deletion nobody
/// has seen.
#[tokio::test]
async fn a_dangerous_prompt_narrows_a_widening_answer() {
    let state = ApprovalState::new();
    state.note_pending(
        "s1",
        PendingApproval {
            risk: "dangerous".to_string(),
            ..sample_pending()
        },
    );
    assert_eq!(
        state.resolve_scoped("s1", Answer::Always),
        Some(Answer::Once)
    );
}

#[tokio::test]
async fn session_approval_cache_remembers_scope_keys() {
    let state = ApprovalState::new();
    assert!(!state.is_session_approved("s1", "file:write"));
    state.remember("s1", "file:write");
    assert!(state.is_session_approved("s1", "file:write"));
    // Scoped per session.
    assert!(!state.is_session_approved("s2", "file:write"));
}

// --- GatewayDispatcher turn queue / panic recovery -----------------------

use komo_core::domain::{
    pairing::PairingRequest, repository::SessionRepository, session::Session, todo::TodoItem,
};
use tokio::sync::{Semaphore, mpsc};

/// A handler that announces each entered input on a channel and blocks until
/// the test grants a completion permit — so a test can hold a turn "in
/// flight" and observe dispatch order. Panics on the input `"boom"`.
struct GateHandler {
    entered: mpsc::UnboundedSender<String>,
    permits: Arc<Semaphore>,
}

#[async_trait]
impl MessageHandler for GateHandler {
    async fn handle(&self, _session_id: &str, input: String) -> anyhow::Result<String> {
        let _ = self.entered.send(input.clone());
        if input == "boom" {
            panic!("boom");
        }
        let permit = self.permits.acquire().await.unwrap();
        permit.forget();
        Ok(input)
    }
}

/// A turn that actually writes to the session log: reports the session it
/// ran on, and appends the user/assistant pair a real turn would. Enough to
/// prove two ingresses share one ordered timeline.
struct SayingHandler {
    entered: mpsc::UnboundedSender<String>,
    events: Arc<dyn SessionEventRepository>,
}

#[async_trait]
impl MessageHandler for SayingHandler {
    async fn handle(&self, session_id: &str, input: String) -> anyhow::Result<String> {
        said(
            self.events.as_ref(),
            session_id,
            "turn",
            SurfaceRole::User,
            &input,
        )
        .await;
        said(
            self.events.as_ref(),
            session_id,
            "turn",
            SurfaceRole::Assistant,
            "ok",
        )
        .await;
        let _ = self.entered.send(session_id.to_string());
        Ok("ok".to_string())
    }
}

/// Append one surface message to a session's log.
async fn said(
    events: &dyn SessionEventRepository,
    session_id: &str,
    turn: &str,
    role: SurfaceRole,
    text: &str,
) {
    let kind = match role {
        SurfaceRole::Assistant => SessionEventKind::AssistantMessage(
            komo_core::domain::session_event::AssistantMessageEvent {
                turn_id: turn.to_string(),
                content: text.to_string(),
                tool_note: String::new(),
                surface: SurfacePlacement::append(),
            },
        ),
        _ => SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: turn.to_string(),
            content: text.to_string(),
            source: MessageSource::User,
            surface: SurfacePlacement::append(),
        }),
    };
    events.append(session_id, vec![kind]).await.unwrap();
    events.durable_flush(session_id).await.unwrap();
}

/// A real store under a scratch home, so a test can read a log back.
async fn test_db(name: &str) -> Arc<komo_infra::persistence::db::Db> {
    let home = std::env::temp_dir().join(name);
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    Arc::new(
        komo_infra::persistence::db::Db::connect(&format!(
            "turso:{}",
            home.join("komo.db").display()
        ))
        .await
        .unwrap(),
    )
}

/// A sink that records every text sent through it.
struct RecordingSink {
    sent: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ReplySink for RecordingSink {
    async fn send(&self, text: &str) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

/// Just enough session store for the dispatcher: `handle` now resolves a
/// correspondent to its session, so the tests need that mapping to behave —
/// open one on first contact, return the same one after.
#[derive(Default)]
struct MemorySessions {
    rows: Mutex<Vec<Session>>,
}

impl MemorySessions {
    /// Pre-open `id` for `peer`, so a test can name the session a message
    /// will land in before it arrives.
    fn seeded(id: &str, peer: &ChannelPeer) -> Arc<Self> {
        let store = Self::default();
        store
            .rows
            .lock()
            .unwrap()
            .push(Session::new(id).with_channel(peer.clone()));
        Arc::new(store)
    }

    /// The session ids handed out so far, in order of creation.
    fn ids(&self) -> Vec<String> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.id.clone())
            .collect()
    }
}

#[async_trait]
impl SessionRepository for MemorySessions {
    async fn find_by_peer(&self, channel: &ChannelPeer) -> anyhow::Result<Option<Session>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.channel.as_ref() == Some(channel))
            .cloned())
    }
    async fn find(&self, id: &str) -> anyhow::Result<Option<Session>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.id == id)
            .cloned())
    }
    async fn find_windowed(&self, id: &str, _limit: usize) -> anyhow::Result<Option<Session>> {
        SessionRepository::find(self, id).await
    }
    async fn list(&self) -> anyhow::Result<Vec<Session>> {
        Ok(self.rows.lock().unwrap().clone())
    }
    async fn save(&self, session: &Session) -> anyhow::Result<()> {
        let mut rows = self.rows.lock().unwrap();
        match rows.iter_mut().find(|s| s.id == session.id) {
            Some(existing) => *existing = session.clone(),
            None => rows.push(session.clone()),
        }
        Ok(())
    }
    async fn delete_empty_sessions(&self) -> anyhow::Result<usize> {
        unimplemented!()
    }
}

/// The home conversation's stored id, minted on first ask like the real one.
#[derive(Default)]
struct MemoryHome {
    address: Mutex<Option<String>>,
    session: Mutex<Option<String>>,
}

#[async_trait]
impl HomeRepository for MemoryHome {
    async fn get(&self) -> anyhow::Result<Option<String>> {
        Ok(self.address.lock().unwrap().clone())
    }
    async fn set(&self, address: &str) -> anyhow::Result<()> {
        *self.address.lock().unwrap() = Some(address.to_string());
        Ok(())
    }
    async fn home_session(&self) -> anyhow::Result<String> {
        let mut held = self.session.lock().unwrap();
        Ok(held
            .get_or_insert_with(|| uuid::Uuid::now_v7().to_string())
            .clone())
    }
}

/// Records what a boundary cleared, so `/new`'s blast radius is testable.
#[derive(Default)]
struct MemoryTodos {
    cleared: Mutex<Vec<String>>,
}

#[async_trait]
impl SessionTodoRepository for MemoryTodos {
    async fn get(&self, _session_id: &str) -> anyhow::Result<Vec<TodoItem>> {
        Ok(Vec::new())
    }
    async fn set(&self, _session_id: &str, _items: &[TodoItem]) -> anyhow::Result<()> {
        Ok(())
    }
    async fn clear(&self, session_id: &str) -> anyhow::Result<()> {
        self.cleared.lock().unwrap().push(session_id.to_string());
        Ok(())
    }
}

struct UnusedPairings;
#[async_trait]
impl PairingRepository for UnusedPairings {
    async fn upsert(&self, _request: &PairingRequest) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn find(
        &self,
        _platform: &str,
        _sender_id: &str,
    ) -> anyhow::Result<Option<PairingRequest>> {
        unimplemented!()
    }
    async fn count_active_pending(&self, _platform: &str) -> anyhow::Result<usize> {
        unimplemented!()
    }
    async fn approve_code(&self, _code: &str) -> anyhow::Result<ApproveOutcome> {
        unimplemented!()
    }
    async fn list(&self) -> anyhow::Result<Vec<PairingRequest>> {
        unimplemented!()
    }
    async fn revoke(&self, _id: &str) -> anyhow::Result<bool> {
        unimplemented!()
    }
}

fn dispatcher_with(handler: Arc<GateHandler>) -> Arc<GatewayDispatcher> {
    dispatcher_with_parts(handler, Arc::new(AlwaysFreshInbox))
}

fn dispatcher_with_parts(
    handler: Arc<GateHandler>,
    inbox: Arc<dyn InboxRepository>,
) -> Arc<GatewayDispatcher> {
    dispatcher_with_sessions(handler, inbox, Arc::new(MemorySessions::default()))
}

fn dispatcher_with_sessions(
    handler: Arc<GateHandler>,
    inbox: Arc<dyn InboxRepository>,
    sessions: Arc<dyn SessionRepository>,
) -> Arc<GatewayDispatcher> {
    Arc::new(GatewayDispatcher::new(
        handler,
        Arc::new(ApprovalState::new()),
        sessions,
        Arc::new(MemoryHome::default()),
        Arc::new(MemoryTodos::default()),
        None,
        Arc::new(UnusedPairings),
        inbox,
    ))
}

/// A paired correspondent's own DM: private to *them*, not the operator's,
/// so it keys on the peer exactly as every chat did before D6.
fn peer() -> InboundPeer {
    InboundPeer::new(ChannelPeer::new("telegram", "1"), true, false)
}

/// The operator, writing from a private surface. Whichever one they pick
/// up, it is the same conversation.
fn operator_dm(platform: &str, peer_id: &str) -> InboundPeer {
    InboundPeer::new(ChannelPeer::new(platform, peer_id), true, true)
}

/// A chat with other people in it — a Feishu group.
fn group(peer_id: &str) -> InboundPeer {
    InboundPeer::new(ChannelPeer::new("feishu", peer_id), false, false)
}

/// Dedupe has its own test below; every other test wants each message
/// through.
struct AlwaysFreshInbox;

#[async_trait]
impl InboxRepository for AlwaysFreshInbox {
    async fn claim(
        &self,
        _origin: &InboundOrigin,
        _peer: &InboundPeer,
        _session_id: &str,
        _text: &str,
    ) -> anyhow::Result<InboxClaim> {
        Ok(InboxClaim::Fresh)
    }

    async fn complete(&self, _origin: &InboundOrigin) -> anyhow::Result<()> {
        Ok(())
    }

    async fn unfinished(&self, _limit: usize) -> anyhow::Result<Vec<UnfinishedInbound>> {
        Ok(Vec::new())
    }
}

/// The real repository's behaviour without a database: one row per platform
/// message, `claimed` until whoever handles it says otherwise.
#[derive(Default)]
struct DedupingInbox {
    rows: Mutex<Vec<(UnfinishedInbound, bool)>>,
}

impl DedupingInbox {
    /// A row a dead process left behind — what startup recovery finds.
    fn left_claimed(row: UnfinishedInbound) -> Arc<Self> {
        Self::left_claimed_all(vec![row])
    }

    /// Several of them: a restart finds whatever was still open.
    fn left_claimed_all(rows: Vec<UnfinishedInbound>) -> Arc<Self> {
        let store = Self::default();
        store
            .rows
            .lock()
            .unwrap()
            .extend(rows.into_iter().map(|row| (row, false)));
        Arc::new(store)
    }

    fn completed(&self, origin: &InboundOrigin) -> bool {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .any(|(row, done)| row.origin == *origin && *done)
    }
}

#[async_trait]
impl InboxRepository for DedupingInbox {
    async fn claim(
        &self,
        origin: &InboundOrigin,
        peer: &InboundPeer,
        session_id: &str,
        text: &str,
    ) -> anyhow::Result<InboxClaim> {
        let mut rows = self.rows.lock().unwrap();
        if rows.iter().any(|(row, _)| row.origin == *origin) {
            return Ok(InboxClaim::Duplicate);
        }
        rows.push((
            UnfinishedInbound {
                origin: origin.clone(),
                session_id: session_id.to_string(),
                text: text.to_string(),
                peer: peer.clone(),
                claimed_at: 0,
            },
            false,
        ));
        Ok(InboxClaim::Fresh)
    }

    async fn complete(&self, origin: &InboundOrigin) -> anyhow::Result<()> {
        for (row, done) in self.rows.lock().unwrap().iter_mut() {
            if row.origin == *origin {
                *done = true;
            }
        }
        Ok(())
    }

    async fn unfinished(&self, limit: usize) -> anyhow::Result<Vec<UnfinishedInbound>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, done)| !done)
            .map(|(row, _)| row.clone())
            .take(limit)
            .collect())
    }
}

// A conversation's identity is its session id, and the map from a chat
// address to that id is stored — so the same correspondent keeps reaching
// the same conversation, and a different one never does. This used to be
// arithmetic on strings (`feishu:{chat_id}` *was* the session id), which is
// why nothing tested it.
#[tokio::test]
async fn a_correspondent_keeps_reaching_the_same_session() {
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let sessions = Arc::new(MemorySessions::default());
    let dispatcher = dispatcher_with_sessions(
        Arc::new(GateHandler {
            entered: entered_tx,
            permits: Arc::new(Semaphore::new(10)),
        }),
        Arc::new(AlwaysFreshInbox),
        sessions.clone(),
    );
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent }) as Arc<dyn ReplySink>;

    let alice = group("oc_alice");
    for text in ["第一句", "第二句"] {
        dispatcher
            .handle(&alice, InboundOrigin::local(), text.into(), sink.clone())
            .await;
        next_entered(&mut entered_rx).await;
    }
    assert_eq!(sessions.ids().len(), 1, "one correspondent, one session");

    // A different chat on the same platform is a different conversation.
    dispatcher
        .handle(
            &group("oc_bob"),
            InboundOrigin::local(),
            "你好".into(),
            sink,
        )
        .await;
    next_entered(&mut entered_rx).await;
    let ids = sessions.ids();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);

    // And the id itself carries nothing: it is a uuid, not an address.
    for id in ids {
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id}");
    }
    // The address lives in a field, where one reader can find it.
    let stored = sessions.find_by_peer(&alice.peer).await.unwrap().unwrap();
    assert_eq!(stored.channel.as_ref(), Some(&alice.peer));
}

/// D6: same principal + private conversation => one ordered timeline.
///
/// The operator's Telegram DM, their Feishu DM and the local client (which
/// asks the store for the same id rather than minting one) are one
/// conversation, and the messages land in one log with contiguous seqs. A
/// Feishu group has someone else in it, so it is a conversation of its own.
#[tokio::test]
async fn every_private_surface_of_the_operator_is_one_conversation() {
    let db = test_db("komo-home-conversation").await;

    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let sessions = Arc::new(MemorySessions::default());
    let home_repo: Arc<dyn HomeRepository> = db.clone();
    let dispatcher = Arc::new(GatewayDispatcher::new(
        Arc::new(SayingHandler {
            entered: entered_tx,
            events: db.clone(),
        }),
        Arc::new(ApprovalState::new()),
        sessions.clone(),
        home_repo.clone(),
        Arc::new(MemoryTodos::default()),
        None,
        Arc::new(UnusedPairings),
        Arc::new(AlwaysFreshInbox),
    ));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent }) as Arc<dyn ReplySink>;

    // The local client (TUI / desktop / web) opens the stored home id.
    let tui = home_repo.home_session().await.unwrap();

    for (from, text) in [
        (
            operator_dm("telegram", "42"),
            "\u{4e0a}\u{5348}\u{5728}\u{624b}\u{673a}\u{4e0a}\u{95ee}\u{7684}",
        ),
        (
            operator_dm("feishu", "ou_me"),
            "\u{4e2d}\u{5348}\u{6362}\u{98de}\u{4e66}\u{63a5}\u{7740}\u{8bf4}",
        ),
    ] {
        dispatcher
            .handle(&from, InboundOrigin::local(), text.into(), sink.clone())
            .await;
        assert_eq!(
            next_entered(&mut entered_rx).await,
            tui,
            "a private surface of the operator is the home conversation"
        );
    }

    // One log, one writer, no holes.
    let events = SessionEventRepository::events(db.as_ref(), &tui)
        .await
        .unwrap();
    assert_eq!(events.len(), 4, "two exchanges");
    assert!(
        events.iter().enumerate().all(|(i, e)| e.seq == i as u64),
        "seqs stay contiguous across ingresses: {:?}",
        events.iter().map(|e| e.seq).collect::<Vec<_>>()
    );
    assert!(
        sessions.ids().is_empty(),
        "the home conversation is not opened per peer"
    );

    // A group has other people in it, so it keys on the correspondent.
    dispatcher
        .handle(
            &group("oc_team"),
            InboundOrigin::local(),
            "\u{7fa4}\u{91cc}\u{95ee}\u{4e00}\u{53e5}".into(),
            sink,
        )
        .await;
    let group_session = next_entered(&mut entered_rx).await;
    assert_ne!(group_session, tui);
    assert_eq!(sessions.ids(), vec![group_session]);
}

/// `/new` is a line in the log, not a rotate and not a cleanup button.
///
/// After it the model is replayed only what came since — but the transcript
/// and the run ledger still hold everything before, and the approval the
/// operator left parked is still answerable, still on the same session.
#[tokio::test]
async fn a_boundary_moves_the_replay_without_ending_anything() {
    use komo_core::domain::run::Run;

    let db = test_db("komo-boundary").await;
    let home_repo: Arc<dyn HomeRepository> = db.clone();
    let session = home_repo.home_session().await.unwrap();

    // One finished exchange, then a turn suspended on an approval.
    let mut run = Run::start(&session, "delete the tree");
    run.status = komo_core::domain::run::RunStatus::Suspended;
    komo_core::domain::run_projection::RunProjectionStore::commit(
        db.as_ref(),
        &session,
        &[komo_core::domain::run_projection::ProjectedRun {
            run: run.clone(),
            steps: Vec::new(),
            start_seq: 0,
        }],
        0,
    )
    .await
    .unwrap();
    said(
        db.as_ref(),
        &session,
        "turn-1",
        SurfaceRole::User,
        "\u{7b2c}\u{4e00}\u{8f6e}",
    )
    .await;
    said(
        db.as_ref(),
        &session,
        "turn-1",
        SurfaceRole::Assistant,
        "\u{7b54}\u{7b2c}\u{4e00}\u{8f6e}",
    )
    .await;
    SessionEventRepository::append(
        db.as_ref(),
        &session,
        vec![
            SessionEventKind::TurnStarted {
                turn_id: run.id.clone(),
                resumed_from: None,
            },
            SessionEventKind::UserMessage(UserMessageEvent {
                turn_id: run.id.clone(),
                content: "\u{5220}\u{6389}\u{90a3}\u{4e2a}\u{76ee}\u{5f55}".into(),
                source: MessageSource::User,
                surface: SurfacePlacement::append(),
            }),
            SessionEventKind::ApprovalRequested(
                komo_core::domain::session_event::ApprovalRequestedEvent {
                    turn_id: run.id.clone(),
                    call_id: "c1".into(),
                    call_index: 0,
                    scope_key: String::new(),
                },
            ),
        ],
    )
    .await
    .unwrap();
    SessionEventRepository::durable_flush(db.as_ref(), &session)
        .await
        .unwrap();
    let registration = WakeupRegistration::new(
        &session,
        Wakeup::Approval {
            call_id: "c1".into(),
        },
        1_000,
    )
    .continuing(&run.id);
    WakeupRepository::save(db.as_ref(), &registration)
        .await
        .unwrap();

    let todos = Arc::new(MemoryTodos::default());
    let resumed = Arc::new(RecordingResume(Mutex::new(Vec::new())));
    let dispatcher = Arc::new(
        GatewayDispatcher::new(
            resumed.clone(),
            Arc::new(ApprovalState::new()),
            Arc::new(MemorySessions::default()),
            home_repo.clone(),
            todos.clone(),
            None,
            Arc::new(UnusedPairings),
            Arc::new(AlwaysFreshInbox),
        )
        .with_waits(WaitParts {
            runs: db.clone(),
            events: db.clone(),
            wakeups: db.clone(),
        }),
    );
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent }) as Arc<dyn ReplySink>;

    dispatcher
        .handle(
            &operator_dm("telegram", "42"),
            InboundOrigin::local(),
            "/new".into(),
            sink,
        )
        .await;

    // The conversation keeps its id — this is the same session throughout.
    assert_eq!(home_repo.home_session().await.unwrap(), session);
    assert_eq!(
        todos.cleared.lock().unwrap().as_slice(),
        [session.clone()],
        "the working todo list is what a boundary retires"
    );

    let projection = SessionEventRepository::surface(db.as_ref(), &session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        projection.messages().unwrap().len(),
        3,
        "the transcript still holds everything before the line"
    );
    assert_eq!(
        projection
            .replay()
            .unwrap()
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>(),
        vec!["\u{5220}\u{6389}\u{90a3}\u{4e2a}\u{76ee}\u{5f55}"],
        "the model sees nothing before the line — bar the turn still waiting"
    );

    // The ledger reads back every turn, boundary or no boundary.
    let events = SessionEventRepository::events(db.as_ref(), &session)
        .await
        .unwrap();
    let runs = komo_core::domain::run_projection::project_runs(&session, &events);
    assert!(
        runs.iter().any(|r| r.run.id == run.id),
        "the suspended turn is still in the ledger"
    );

    // And the approval left parked is still answerable.
    assert!(
        dispatcher
            .answer_approval(&session, None, Answer::Once)
            .await,
        "`/new` does not end a turn that is still owed an answer"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        resumed.0.lock().unwrap().as_slice(),
        [run.id.clone()],
        "the woken turn continued"
    );
}

// An HTTP turn takes the same per-session slot a chat turn takes, so two
// clients on one session (a second TUI resuming it, the desktop app beside
// the terminal) can never run turns side by side. They used to: the api
// channel called the handler directly, and the later turn assembled its
// history before the earlier one had written a word of its answer — so it
// started over from the original question and re-ran everything the first
// was still doing.
#[tokio::test]
async fn a_second_claim_on_one_session_waits_for_the_first() {
    let (entered_tx, _entered_rx) = mpsc::unbounded_channel();
    let dispatcher = dispatcher_with(Arc::new(GateHandler {
        entered: entered_tx,
        permits: Arc::new(Semaphore::new(0)),
    }));

    let first = dispatcher.claim_session("s1").await;

    let mut waiting = {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move { dispatcher.claim_session("s1").await })
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut waiting)
            .await
            .is_err(),
        "a second turn must not start while the first holds the session"
    );

    // The gate is per session, not global: an unrelated session is free.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), dispatcher.claim_session("s2"))
            .await
            .is_ok(),
        "another session must not be blocked by this one"
    );

    first.release();
    let second = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the slot must be handed over once the first turn releases")
        .expect("the waiting task must not panic");
    second.release();
}

/// The reply goes where the *answer* came from, not where the question was
/// asked. Suspended in the TUI, released by `/approve` from Telegram: the
/// continuation answers into Telegram, because that is where the operator
/// is standing. The transcript has it either way.
#[tokio::test]
async fn a_woken_turn_answers_the_surface_that_released_it() {
    use komo_core::domain::run::Run;

    let db = test_db("komo-woken-reply").await;
    let home_repo: Arc<dyn HomeRepository> = db.clone();
    let session = home_repo.home_session().await.unwrap();

    let mut run = Run::start(&session, "delete the tree");
    run.status = komo_core::domain::run::RunStatus::Suspended;
    komo_core::domain::run_projection::RunProjectionStore::commit(
        db.as_ref(),
        &session,
        &[komo_core::domain::run_projection::ProjectedRun {
            run: run.clone(),
            steps: Vec::new(),
            start_seq: 0,
        }],
        0,
    )
    .await
    .unwrap();
    WakeupRepository::save(
        db.as_ref(),
        &WakeupRegistration::new(
            &session,
            Wakeup::Approval {
                call_id: "c1".into(),
            },
            1_000,
        )
        .continuing(&run.id),
    )
    .await
    .unwrap();

    let dispatcher = Arc::new(
        GatewayDispatcher::new(
            Arc::new(RecordingResume(Mutex::new(Vec::new()))),
            Arc::new(ApprovalState::new()),
            Arc::new(MemorySessions::default()),
            home_repo,
            Arc::new(MemoryTodos::default()),
            None,
            Arc::new(UnusedPairings),
            Arc::new(AlwaysFreshInbox),
        )
        .with_waits(WaitParts {
            runs: db.clone(),
            events: db.clone(),
            wakeups: db.clone(),
        }),
    );

    // `/approve` arrives from Telegram — the same home conversation, a
    // different surface from the one that asked.
    let sent = Arc::new(Mutex::new(Vec::new()));
    let telegram = Arc::new(RecordingSink { sent: sent.clone() }) as Arc<dyn ReplySink>;
    dispatcher
        .handle(
            &operator_dm("telegram", "42"),
            InboundOrigin::local(),
            "/approve".into(),
            telegram,
        )
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let sent = sent.lock().unwrap().clone();
    assert!(
        sent.iter().any(|line| line == "continued"),
        "the continuation's answer reaches the surface that answered: {sent:?}"
    );
}

/// The waker's own three jobs: record *why* the turn came back, retire the
/// other waits it was holding, and hand the run to whoever continues it.
#[tokio::test]
async fn waking_a_turn_records_the_cause_and_retires_its_other_waits() {
    use komo_core::domain::run::Run;
    use komo_core::domain::session_event::Wakeup;

    let home = std::env::temp_dir().join("komo-waker-fire");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    let db = Arc::new(
        komo_infra::persistence::db::Db::connect(&format!(
            "turso:{}",
            home.join("komo.db").display()
        ))
        .await
        .unwrap(),
    );

    // A suspended turn in the ledger, with two waits on it: the approval
    // and the deadline watching the same wait.
    let mut run = Run::start("s1", "delete it");
    run.status = komo_core::domain::run::RunStatus::Suspended;
    let projected = komo_core::domain::run_projection::ProjectedRun {
        run: run.clone(),
        steps: Vec::new(),
        start_seq: 0,
    };
    komo_core::domain::run_projection::RunProjectionStore::commit(
        db.as_ref(),
        "s1",
        &[projected],
        0,
    )
    .await
    .unwrap();
    let approval = WakeupRegistration::new(
        "s1",
        Wakeup::Approval {
            call_id: "c1".into(),
        },
        1_000,
    )
    .continuing(&run.id);
    let deadline = WakeupRegistration::new("s1", Wakeup::UserReply, 1_000)
        .continuing(&run.id)
        .expiring_at(Some(2_000));
    for registration in [&approval, &deadline] {
        WakeupRepository::save(db.as_ref(), registration)
            .await
            .unwrap();
    }

    let handler = Arc::new(RecordingResume(Mutex::new(Vec::new())));
    let dispatcher = Arc::new(
        GatewayDispatcher::new(
            handler.clone(),
            Arc::new(ApprovalState::new()),
            Arc::new(MemorySessions::default()),
            Arc::new(MemoryHome::default()),
            Arc::new(MemoryTodos::default()),
            None,
            Arc::new(UnusedPairings),
            Arc::new(AlwaysFreshInbox),
        )
        .with_waits(WaitParts {
            runs: db.clone(),
            events: db.clone(),
            wakeups: db.clone(),
        }),
    );
    let waker = TurnWaker::new(dispatcher);

    waker
        .fire(&approval, WakeupCause::Approve, "")
        .await
        .expect("the wake itself must land");

    // Why it came back is on the record, durably.
    let events = SessionEventRepository::events(db.as_ref(), "s1")
        .await
        .unwrap();
    let fired = events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::WakeupFired(fired) => Some(fired),
            _ => None,
        })
        .next()
        .expect("the log says what ended the wait");
    assert_eq!(fired.turn_id, run.id);
    assert_eq!(fired.cause, WakeupCause::Approve);
    assert_eq!(
        fired.wakeup_id, approval.id,
        "traceable to what scheduled it"
    );

    // Both waits are gone: the deadline must not wake the same turn again.
    assert!(
        WakeupRepository::list(db.as_ref())
            .await
            .unwrap()
            .is_empty(),
        "a woken turn takes every wait it was holding with it"
    );

    // And the continuation was handed the suspended run. Spawned, so give
    // it a moment.
    for _ in 0..200 {
        if !handler.0.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(*handler.0.lock().unwrap(), vec![run.id.clone()]);
}

/// Records the run it was asked to continue.
struct RecordingResume(Mutex<Vec<String>>);

#[async_trait]
impl MessageHandler for RecordingResume {
    async fn handle(&self, _session_id: &str, input: String) -> anyhow::Result<String> {
        Ok(input)
    }
    async fn resume_interrupted(
        &self,
        run: &komo_core::domain::run::Run,
    ) -> anyhow::Result<Option<String>> {
        self.0.lock().unwrap().push(run.id.clone());
        Ok(Some("continued".to_string()))
    }
}

/// A pending wait is **replaced** by the next thing the user says, not kept
/// beside it. The message joins the suspended turn, the approval is
/// refused citing it, and no second turn starts — the model answers both in
/// one turn.
#[tokio::test]
async fn saying_something_else_takes_the_place_of_a_pending_approval() {
    use komo_core::domain::run::Run;

    let home = std::env::temp_dir().join("komo-moved-on");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    let db = Arc::new(
        komo_infra::persistence::db::Db::connect(&format!(
            "turso:{}",
            home.join("komo.db").display()
        ))
        .await
        .unwrap(),
    );

    let mut run = Run::start("s1", "delete the tree");
    run.status = komo_core::domain::run::RunStatus::Suspended;
    komo_core::domain::run_projection::RunProjectionStore::commit(
        db.as_ref(),
        "s1",
        &[komo_core::domain::run_projection::ProjectedRun {
            run: run.clone(),
            steps: Vec::new(),
            start_seq: 0,
        }],
        0,
    )
    .await
    .unwrap();
    WakeupRepository::save(
        db.as_ref(),
        &WakeupRegistration::new(
            "s1",
            Wakeup::Approval {
                call_id: "c1".into(),
            },
            1_000,
        )
        .continuing(&run.id),
    )
    .await
    .unwrap();

    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let dispatcher = Arc::new(
        GatewayDispatcher::new(
            Arc::new(GateHandler {
                entered: entered_tx,
                permits: Arc::new(Semaphore::new(1)),
            }),
            Arc::new(ApprovalState::new()),
            Arc::new(MemorySessions::default()),
            Arc::new(MemoryHome::default()),
            Arc::new(MemoryTodos::default()),
            None,
            Arc::new(UnusedPairings),
            Arc::new(AlwaysFreshInbox),
        )
        .with_waits(WaitParts {
            runs: db.clone(),
            events: db.clone(),
            wakeups: db.clone(),
        }),
    );
    assert!(
        dispatcher
            .moved_on(
                "s1",
                "算了，先别删",
                Arc::new(RecordingSink {
                    sent: Arc::new(Mutex::new(Vec::new())),
                }),
            )
            .await,
        "the message answers the pending approval"
    );
    assert!(
        entered_rx.try_recv().is_err(),
        "and no second turn is started for it"
    );

    let events = SessionEventRepository::events(db.as_ref(), "s1")
        .await
        .unwrap();
    let said = events
        .iter()
        .find_map(|event| match &event.kind {
            SessionEventKind::UserMessage(m) => Some(m),
            _ => None,
        })
        .expect("what the user said is on the record");
    assert_eq!(said.turn_id, run.id, "it belongs to the suspended turn");
    assert_eq!(
        said.source,
        MessageSource::Injected,
        "as an interjection, so the surface still alternates"
    );

    let resolved = events
        .iter()
        .find_map(|event| match &event.kind {
            SessionEventKind::ApprovalResolved(resolved) => Some(resolved),
            _ => None,
        })
        .expect("the approval is answered, not left hanging");
    assert!(!resolved.allowed);
    assert!(
        resolved.reason.contains("算了，先别删"),
        "and the refusal cites what was said: {}",
        resolved.reason
    );
    assert!(
        WakeupRepository::list(db.as_ref())
            .await
            .unwrap()
            .is_empty(),
        "the wait is retired with it"
    );
}

/// A wait that ran out is written down as such *before* the turn comes
/// back, so the gate reads it as a refusal instead of asking again.
#[tokio::test]
async fn an_expired_wait_records_the_expiry_before_continuing() {
    use komo_core::domain::run::Run;

    let home = std::env::temp_dir().join("komo-waker-expired");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    let db = Arc::new(
        komo_infra::persistence::db::Db::connect(&format!(
            "turso:{}",
            home.join("komo.db").display()
        ))
        .await
        .unwrap(),
    );

    // The gate asked, and recorded where in its round the call sat.
    let mut run = Run::start("s1", "delete it");
    run.status = komo_core::domain::run::RunStatus::Suspended;
    komo_core::domain::run_projection::RunProjectionStore::commit(
        db.as_ref(),
        "s1",
        &[komo_core::domain::run_projection::ProjectedRun {
            run: run.clone(),
            steps: Vec::new(),
            start_seq: 0,
        }],
        0,
    )
    .await
    .unwrap();
    SessionEventRepository::append(
        db.as_ref(),
        "s1",
        vec![SessionEventKind::ApprovalRequested(
            komo_core::domain::session_event::ApprovalRequestedEvent {
                turn_id: run.id.clone(),
                call_id: "c1".into(),
                call_index: 2,
                scope_key: String::new(),
            },
        )],
    )
    .await
    .unwrap();
    SessionEventRepository::durable_flush(db.as_ref(), "s1")
        .await
        .unwrap();

    let registration = WakeupRegistration::new(
        "s1",
        Wakeup::Approval {
            call_id: "c1".into(),
        },
        1_000,
    )
    .continuing(&run.id);
    WakeupRepository::save(db.as_ref(), &registration)
        .await
        .unwrap();

    let handler = Arc::new(RecordingResume(Mutex::new(Vec::new())));
    let dispatcher = Arc::new(
        GatewayDispatcher::new(
            handler.clone(),
            Arc::new(ApprovalState::new()),
            Arc::new(MemorySessions::default()),
            Arc::new(MemoryHome::default()),
            Arc::new(MemoryTodos::default()),
            None,
            Arc::new(UnusedPairings),
            Arc::new(AlwaysFreshInbox),
        )
        .with_waits(WaitParts {
            runs: db.clone(),
            events: db.clone(),
            wakeups: db.clone(),
        }),
    );

    TurnWaker::new(dispatcher)
        .fire(&registration, WakeupCause::Expired, "")
        .await
        .unwrap();

    let events = SessionEventRepository::events(db.as_ref(), "s1")
        .await
        .unwrap();
    let expired = events
        .iter()
        .find_map(|event| match &event.kind {
            SessionEventKind::ApprovalExpired {
                turn_id,
                call_id,
                call_index,
            } => Some((turn_id, call_id, *call_index)),
            _ => None,
        })
        .expect("an expiry the turn can read as a refusal");
    assert_eq!(expired.0, &run.id);
    assert_eq!(expired.1, "c1");
    assert_eq!(
        expired.2, 2,
        "it names the same call the request did, read back from the request"
    );
}

/// Stop is pressed on a conversation, so it has to reach the caller *queued*
/// for the session as well as the one holding it — and that caller should
/// give up its wait rather than run, once the turn it was queued behind
/// finishes, the very work the user just stopped.
#[tokio::test]
async fn a_caller_queued_for_the_session_gives_up_when_it_is_cancelled() {
    let (entered_tx, _entered_rx) = mpsc::unbounded_channel();
    let dispatcher = dispatcher_with(Arc::new(GateHandler {
        entered: entered_tx,
        permits: Arc::new(Semaphore::new(0)),
    }));
    let cancels = Arc::new(CancelState::new());

    let running = dispatcher.claim_session("s1").await;
    // The api ingress's shape: register, then race the wait for the slot
    // against the signal.
    let ticket = cancels.register("s1");
    let queued = {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move {
            tokio::select! {
                _claim = dispatcher.claim_session("s1") => "ran",
                () = ticket.cancelled() => "stopped",
            }
        })
    };
    // Parked, not running.
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert!(cancels.cancel("s1"), "the session has listeners");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .expect("a cancelled waiter must not keep waiting")
            .unwrap(),
        "stopped"
    );
    running.release();
}

// The other direction of the same invariant: a chat message that arrives
// while a self-driven (HTTP) turn holds the session queues behind it and
// runs when it finishes, rather than opening a concurrent turn.
#[tokio::test]
async fn a_chat_message_arriving_during_a_claimed_turn_runs_after_it() {
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let dispatcher = dispatcher_with(Arc::new(GateHandler {
        entered: entered_tx,
        permits: Arc::new(Semaphore::new(1)),
    }));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent }) as Arc<dyn ReplySink>;

    let claim = dispatcher.claim_session("s1").await;
    dispatcher
        .handle(&peer(), InboundOrigin::local(), "看下 A".into(), sink)
        .await;
    assert!(
        entered_rx.try_recv().is_err(),
        "the chat message must queue, not run beside the claimed turn"
    );

    claim.release();
    assert_eq!(next_entered(&mut entered_rx).await, "看下 A");
}

/// Wait for the next entered input, failing the test on timeout so a wedge
/// surfaces as a failure rather than a hang.
async fn next_entered(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for a turn to start")
        .expect("handler channel closed")
}

/// "Approve for the session" widens an approval to calls the user has not
/// seen yet. For an irreversible action the next such call is a *second*
/// deletion, not a repeat of the one that was shown — so the widening is
/// refused and the user is told, rather than silently granted less than
/// they asked for.
#[tokio::test]
async fn a_dangerous_action_is_never_approved_beyond_the_call_it_was_asked_about() {
    let state = ApprovalState::new();
    let dangerous = PendingApproval {
        summary: "rm -rf /data".to_string(),
        detail: None,
        risk: "dangerous".to_string(),
    };
    state.note_pending("s1", dangerous);
    assert_eq!(
        state.resolve_scoped("s1", Answer::Session),
        Some(Answer::Once),
        "a session-wide grant must narrow to this one call"
    );

    // `always` would have written a persisted rule; it narrows the same way.
    state.note_pending(
        "s2",
        PendingApproval {
            summary: "drop the table".to_string(),
            detail: None,
            risk: "dangerous".to_string(),
        },
    );
    assert_eq!(
        state.resolve_scoped("s2", Answer::Always),
        Some(Answer::Once)
    );

    // A normal action is untouched — that is what the scopes are for.
    state.note_pending(
        "s3",
        PendingApproval {
            summary: "write a file".to_string(),
            detail: None,
            risk: "normal".to_string(),
        },
    );
    assert_eq!(
        state.resolve_scoped("s3", Answer::Session),
        Some(Answer::Session)
    );
}

/// A message the process died owing an answer to is re-delivered at
/// startup — exactly once, and completed only after its turn has settled.
///
/// The claim is what makes the platform's own redelivery a duplicate, so
/// nothing else will ever bring this message back.
#[tokio::test]
async fn a_claimed_message_whose_turn_never_ran_is_redelivered_once() {
    let origin = InboundOrigin::new("telegram", "77");
    let inbox = DedupingInbox::left_claimed(UnfinishedInbound {
        origin: origin.clone(),
        session_id: "s-lost".to_string(),
        text: "把昨天的日志整理一下".to_string(),
        peer: peer(),
        claimed_at: 100,
    });
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let dispatcher = dispatcher_with_sessions(
        Arc::new(GateHandler {
            entered: entered_tx,
            permits: Arc::new(Semaphore::new(4)),
        }),
        inbox.clone(),
        Arc::new(MemorySessions::default()),
    );

    assert_eq!(dispatcher.recover_inbox(50).await, 1);
    assert_eq!(next_entered(&mut entered_rx).await, "把昨天的日志整理一下");

    // The row closes when the turn does, not when it was dispatched.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(inbox.completed(&origin), "the settled turn closes its row");
    assert_eq!(
        dispatcher.recover_inbox(50).await,
        0,
        "a recovered message is not offered again"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        entered_rx.try_recv().is_err(),
        "the message must run exactly once"
    );
}

/// The other half: the turn *did* start before the crash. Its user message
/// is in the transcript, so the run ledger and the suspended-turn repair
/// own it — re-running it here would duplicate work somebody else is
/// already recovering.
#[tokio::test]
async fn a_claimed_message_already_in_the_transcript_is_only_closed() {
    let origin = InboundOrigin::new("telegram", "78");
    let text = "查一下昨天的告警";
    let inbox = DedupingInbox::left_claimed(UnfinishedInbound {
        origin: origin.clone(),
        session_id: "s-started".to_string(),
        text: text.to_string(),
        peer: peer(),
        claimed_at: 100,
    });
    let sessions = Arc::new(MemorySessions::default());
    let mut session = Session::new("s-started");
    session.messages.push(komo_core::domain::message::Message {
        role: Role::User,
        content: text.to_string(),
        timestamp: 100,
        tool_note: String::new(),
    });
    SessionRepository::save(sessions.as_ref(), &session)
        .await
        .unwrap();

    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let dispatcher = dispatcher_with_sessions(
        Arc::new(GateHandler {
            entered: entered_tx,
            permits: Arc::new(Semaphore::new(4)),
        }),
        inbox.clone(),
        sessions,
    );

    assert_eq!(dispatcher.recover_inbox(50).await, 0);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        entered_rx.try_recv().is_err(),
        "the turn that already started is not started again"
    );
    assert!(inbox.completed(&origin), "the row is closed all the same");
}

/// Recovery goes back through the command-honouring path, not straight to a
/// turn: an `/approve` the crash swallowed still approves.
#[tokio::test]
async fn a_recovered_command_is_still_a_command() {
    let origin = InboundOrigin::new("telegram", "79");
    let inbox = DedupingInbox::left_claimed(UnfinishedInbound {
        origin: origin.clone(),
        session_id: "s-cmd".to_string(),
        text: "/approve".to_string(),
        peer: peer(),
        claimed_at: 100,
    });
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let approvals = Arc::new(ApprovalState::new());
    approvals.note_pending("s-cmd", sample_pending());
    let dispatcher = Arc::new(GatewayDispatcher::new(
        Arc::new(GateHandler {
            entered: entered_tx,
            permits: Arc::new(Semaphore::new(4)),
        }),
        approvals.clone(),
        Arc::new(MemorySessions::default()),
        Arc::new(MemoryHome::default()),
        Arc::new(MemoryTodos::default()),
        None,
        Arc::new(UnusedPairings),
        inbox.clone(),
    ));

    assert_eq!(dispatcher.recover_inbox(50).await, 1);
    assert!(
        approvals.pending_info("s-cmd").is_none(),
        "the recovered /approve answered the pending prompt"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        entered_rx.try_recv().is_err(),
        "a command never starts a turn"
    );
    assert!(
        inbox.completed(&origin),
        "a command completes once answered"
    );
}

/// Shutdown drains a session's queue with a "please resend" notice, which
/// makes those messages *handled* — so their rows close with them. Left
/// `claimed`, the next startup would re-deliver a message the sender was
/// just told to send again: one message, two answers.
#[tokio::test]
async fn a_queued_message_discarded_at_shutdown_closes_its_row() {
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let inbox = Arc::new(DedupingInbox::default());
    let dispatcher = dispatcher_with_sessions(
        Arc::new(GateHandler {
            entered: entered_tx,
            permits: Arc::new(Semaphore::new(0)),
        }),
        inbox.clone(),
        MemorySessions::seeded("s-busy", &peer().peer),
    );
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent: sent.clone() }) as Arc<dyn ReplySink>;
    let queued = InboundOrigin::new("telegram", "90");

    // A turn holds the session, so the message queues behind it.
    let claim = dispatcher.claim_session("s-busy").await;
    dispatcher
        .handle(&peer(), queued.clone(), "等会儿看一下".into(), sink)
        .await;
    assert!(!inbox.completed(&queued), "nothing has answered it yet");

    // Gateway shutdown: the claim is dropped rather than released, which is
    // the path that discards the queue.
    drop(claim);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        sent.lock().unwrap().iter().any(|t| t.contains("请重发")),
        "the sender is told to resend, got {:?}",
        sent.lock().unwrap()
    );
    assert!(inbox.completed(&queued), "so the message counts as handled");
    assert!(
        inbox.unfinished(10).await.unwrap().is_empty(),
        "and the next startup does not deliver it a second time"
    );
    assert!(entered_rx.try_recv().is_err(), "no turn ever ran it");
}

/// A row claimed before the peer columns existed carries no correspondent.
/// Its session id still routes plain text, but a chat command reads the
/// peer — a recovered `/sethome` would make the empty address the
/// operator's home chat.
#[tokio::test]
async fn a_legacy_row_without_a_peer_never_re_runs_a_command() {
    let legacy = InboundPeer::new(ChannelPeer::new("", ""), false, false);
    let command = InboundOrigin::new("telegram", "91");
    let plain = InboundOrigin::new("telegram", "92");
    let inbox = DedupingInbox::left_claimed_all(vec![
        UnfinishedInbound {
            origin: command.clone(),
            session_id: "s-legacy".to_string(),
            text: "/sethome".to_string(),
            peer: legacy.clone(),
            claimed_at: 100,
        },
        UnfinishedInbound {
            origin: plain.clone(),
            session_id: "s-legacy".to_string(),
            text: "顺手看下磁盘".to_string(),
            peer: legacy,
            claimed_at: 100,
        },
    ]);
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let home = Arc::new(MemoryHome::default());
    let dispatcher = Arc::new(GatewayDispatcher::new(
        Arc::new(GateHandler {
            entered: entered_tx,
            permits: Arc::new(Semaphore::new(4)),
        }),
        Arc::new(ApprovalState::new()),
        Arc::new(MemorySessions::default()),
        home.clone(),
        Arc::new(MemoryTodos::default()),
        None,
        Arc::new(UnusedPairings),
        inbox.clone(),
    ));

    assert_eq!(
        dispatcher.recover_inbox(50).await,
        1,
        "only the plain message is re-delivered"
    );
    assert_eq!(next_entered(&mut entered_rx).await, "顺手看下磁盘");
    assert!(
        home.get().await.unwrap().is_none(),
        "the peerless /sethome never ran"
    );
    assert!(
        inbox.completed(&command),
        "and its row is closed, not offered again"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(dispatcher.recover_inbox(50).await, 0);
}

/// A message waiting behind a busy session has not been answered yet, so
/// its row stays `claimed` — a crash while it waits re-delivers it.
#[tokio::test]
async fn a_queued_message_stays_claimed_until_its_turn_runs() {
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let permits = Arc::new(Semaphore::new(0));
    let inbox = Arc::new(DedupingInbox::default());
    let dispatcher = dispatcher_with_parts(
        Arc::new(GateHandler {
            entered: entered_tx,
            permits: permits.clone(),
        }),
        inbox.clone(),
    );
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent }) as Arc<dyn ReplySink>;
    let first = InboundOrigin::new("telegram", "80");
    let second = InboundOrigin::new("telegram", "81");

    dispatcher
        .handle(&peer(), first.clone(), "m1".into(), sink.clone())
        .await;
    assert_eq!(next_entered(&mut entered_rx).await, "m1");
    dispatcher
        .handle(&peer(), second.clone(), "m2".into(), sink.clone())
        .await;
    assert!(
        !inbox.completed(&first) && !inbox.completed(&second),
        "nothing has been answered yet"
    );

    // The running turn settles: its own row closes, the queued one does not.
    permits.add_permits(1);
    assert_eq!(next_entered(&mut entered_rx).await, "m2");
    assert!(inbox.completed(&first));
    assert!(
        !inbox.completed(&second),
        "the queued message is only now running"
    );
    assert_eq!(
        inbox.unfinished(10).await.unwrap().len(),
        1,
        "a crash here re-delivers exactly the message nobody answered"
    );

    permits.add_permits(1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(inbox.completed(&second));
}

/// Chat platforms deliver at-least-once: Telegram redelivers a whole batch
/// when the offset never got committed, Feishu retries what it thinks was
/// not acked, and either survives a gateway restart. A redelivery must not
/// run a second turn — and the gate sits in front of *commands* too, so a
/// redelivered `/approve` cannot approve twice.
#[tokio::test]
async fn a_redelivered_message_never_runs_a_second_turn() {
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let dispatcher = dispatcher_with_parts(
        Arc::new(GateHandler {
            entered: entered_tx,
            permits: Arc::new(Semaphore::new(8)),
        }),
        Arc::new(DedupingInbox::default()),
    );
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent }) as Arc<dyn ReplySink>;
    let origin = InboundOrigin::new("telegram", "42");

    dispatcher
        .handle(&peer(), origin.clone(), "hello".into(), sink.clone())
        .await;
    assert_eq!(next_entered(&mut entered_rx).await, "hello");

    // The same platform message, delivered again.
    dispatcher
        .handle(&peer(), origin, "hello".into(), sink.clone())
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        entered_rx.try_recv().is_err(),
        "a redelivered message must not start a second turn"
    );

    // A genuinely new message still gets through.
    dispatcher
        .handle(
            &peer(),
            InboundOrigin::new("telegram", "43"),
            "and another".into(),
            sink,
        )
        .await;
    assert_eq!(next_entered(&mut entered_rx).await, "and another");
}

#[tokio::test]
async fn mid_turn_messages_merge_into_one_turn_and_cap() {
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let permits = Arc::new(Semaphore::new(0));
    let handler = Arc::new(GateHandler {
        entered: entered_tx,
        permits: permits.clone(),
    });
    let dispatcher = dispatcher_with(handler);
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent: sent.clone() }) as Arc<dyn ReplySink>;

    // m1 dispatches and blocks in the handler.
    dispatcher
        .handle(&peer(), InboundOrigin::local(), "m1".into(), sink.clone())
        .await;
    assert_eq!(next_entered(&mut entered_rx).await, "m1");

    // m2, m3 queue behind it; m4 overflows the cap and is rejected.
    dispatcher
        .handle(&peer(), InboundOrigin::local(), "m2".into(), sink.clone())
        .await;
    dispatcher
        .handle(&peer(), InboundOrigin::local(), "m3".into(), sink.clone())
        .await;
    dispatcher
        .handle(&peer(), InboundOrigin::local(), "m4".into(), sink.clone())
        .await;

    // Everything queued behind m1 runs as ONE turn, in order — a user who
    // splits a thought across messages gets one answer, not one per line.
    permits.add_permits(1);
    assert_eq!(next_entered(&mut entered_rx).await, "m2\nm3");
    permits.add_permits(1);

    // Let the final reply + rejection settle, then assert the overflow hint
    // was delivered and no fourth turn ever started.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sent = sent.lock().unwrap();
    assert!(
        sent.iter().any(|t| t.contains("队列已满")),
        "m4 should be rejected with the queue-full hint, got {sent:?}"
    );
    assert!(entered_rx.try_recv().is_err(), "m4 must not have run");
}

/// The running turn takes queued messages out of the same queue the next
/// turn would drain, so a message is delivered exactly once — and once
/// taken, `finish_turn` finds nothing left to dispatch.
#[tokio::test]
async fn a_running_turn_takes_queued_messages_for_itself() {
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let permits = Arc::new(Semaphore::new(0));
    let dispatcher = dispatcher_with_sessions(
        Arc::new(GateHandler {
            entered: entered_tx,
            permits: permits.clone(),
        }),
        Arc::new(AlwaysFreshInbox),
        MemorySessions::seeded("s7", &peer().peer),
    );
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent }) as Arc<dyn ReplySink>;

    dispatcher
        .handle(&peer(), InboundOrigin::local(), "m1".into(), sink.clone())
        .await;
    assert_eq!(next_entered(&mut entered_rx).await, "m1");
    dispatcher
        .handle(&peer(), InboundOrigin::local(), "m2".into(), sink.clone())
        .await;
    dispatcher
        .handle(&peer(), InboundOrigin::local(), "m3".into(), sink.clone())
        .await;

    // What the agent loop does between rounds.
    let interjector = QueueInterjector {
        dispatcher: dispatcher.clone(),
        session: "s7".to_string(),
        owned: Arc::new(Mutex::new(Vec::new())),
    };
    assert_eq!(interjector.take(), vec!["m2".to_string(), "m3".to_string()]);
    assert!(interjector.take().is_empty(), "taken exactly once");

    // The turn finishes with an empty queue, so no follow-up turn runs.
    permits.add_permits(1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        entered_rx.try_recv().is_err(),
        "the messages were already handled inside the first turn"
    );
    assert!(
        !dispatcher.inflight.lock().unwrap().contains_key("s7"),
        "the session should be idle once the turn ends"
    );
}

/// A turn killed mid-flight (gateway shutdown) can't run what queued behind
/// it, but the sender must not be left waiting for an answer that will
/// never come. Drives [`TurnGuard`]'s emergency path directly — the normal
/// completion path drains the queue instead.
#[tokio::test]
async fn a_dropped_turn_tells_queued_senders_to_resend() {
    let (entered_tx, _entered_rx) = mpsc::unbounded_channel();
    let dispatcher = dispatcher_with(Arc::new(GateHandler {
        entered: entered_tx,
        permits: Arc::new(Semaphore::new(0)),
    }));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent: sent.clone() }) as Arc<dyn ReplySink>;

    // The state a turn in flight with one queued follow-up leaves behind.
    dispatcher.inflight.lock().unwrap().insert(
        "s8".to_string(),
        VecDeque::from(vec![QueuedMessage {
            input: "跟进的一条".into(),
            sink: sink.clone(),
            origin: None,
        }]),
    );

    drop(TurnGuard {
        dispatcher: dispatcher.clone(),
        session: "s8".to_string(),
        armed: true,
    });

    // The notice is spawned, so give it a turn of the runtime to land.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        sent.lock().unwrap().iter().any(|t| t.contains("请重发")),
        "the queued sender should be told to resend, got {:?}",
        sent.lock().unwrap()
    );
    assert!(
        !dispatcher.inflight.lock().unwrap().contains_key("s8"),
        "the session must not be left wedged"
    );
}

#[tokio::test]
async fn a_panicking_turn_does_not_wedge_the_session() {
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let permits = Arc::new(Semaphore::new(0));
    let handler = Arc::new(GateHandler {
        entered: entered_tx,
        permits: permits.clone(),
    });
    let dispatcher = dispatcher_with(handler);
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent: sent.clone() }) as Arc<dyn ReplySink>;

    // First turn panics — the catch keeps the task alive and the guard/finish
    // path releases the session.
    dispatcher
        .handle(&peer(), InboundOrigin::local(), "boom".into(), sink.clone())
        .await;
    assert_eq!(next_entered(&mut entered_rx).await, "boom");

    // A later message must still be handled (session not permanently busy).
    dispatcher
        .handle(
            &peer(),
            InboundOrigin::local(),
            "after".into(),
            sink.clone(),
        )
        .await;
    permits.add_permits(1);
    assert_eq!(next_entered(&mut entered_rx).await, "after");
}

/// Reports the [`SessionContext`] its turn ran in.
struct ContextProbe(Mutex<Option<SessionContext>>);

#[async_trait]
impl MessageHandler for ContextProbe {
    async fn handle(&self, _session_id: &str, input: String) -> anyhow::Result<String> {
        *self.0.lock().unwrap() = current_session();
        Ok(input)
    }
    async fn resume_interrupted(
        &self,
        _run: &komo_core::domain::run::Run,
    ) -> anyhow::Result<Option<String>> {
        *self.0.lock().unwrap() = current_session();
        Ok(Some("continued".to_string()))
    }
}

/// A suspended turn plus the wait that is holding it, ready to be answered.
async fn parked_on_an_approval(
    home: &str,
) -> (
    Arc<komo_infra::persistence::db::Db>,
    komo_core::domain::run::Run,
) {
    let db = test_db(home).await;
    let mut run = komo_core::domain::run::Run::start("s1", "do the thing");
    run.status = komo_core::domain::run::RunStatus::Suspended;
    komo_core::domain::run_projection::RunProjectionStore::commit(
        db.as_ref(),
        "s1",
        &[komo_core::domain::run_projection::ProjectedRun {
            run: run.clone(),
            steps: Vec::new(),
            start_seq: 0,
        }],
        0,
    )
    .await
    .unwrap();
    WakeupRepository::save(
        db.as_ref(),
        &WakeupRegistration::new(
            "s1",
            Wakeup::Approval {
                call_id: "c1".into(),
            },
            1_000,
        )
        .continuing(&run.id),
    )
    .await
    .unwrap();
    (db, run)
}

fn probing_dispatcher(
    probe: Arc<ContextProbe>,
    db: Arc<komo_infra::persistence::db::Db>,
) -> Arc<GatewayDispatcher> {
    Arc::new(
        GatewayDispatcher::new(
            probe,
            Arc::new(ApprovalState::new()),
            Arc::new(MemorySessions::default()),
            Arc::new(MemoryHome::default()),
            Arc::new(MemoryTodos::default()),
            None,
            Arc::new(UnusedPairings),
            Arc::new(AlwaysFreshInbox),
        )
        .with_waits(WaitParts {
            runs: db.clone(),
            events: db.clone(),
            wakeups: db.clone(),
        }),
    )
}

async fn probed(probe: &Arc<ContextProbe>) -> SessionContext {
    for _ in 0..200 {
        if let Some(ctx) = probe.0.lock().unwrap().clone() {
            return ctx;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the continuation never ran");
}

/// The person who answered `/approve` is still there — so the continuation is
/// interactive and prompts back through the surface that answered.
///
/// It used to run detached: `interactive` false for the rest of the turn, so
/// the *next* thing needing approval was auto-denied with "this session is
/// non-interactive, nobody can answer", and the refusal reached the model as
/// the user's own. Answering one approval was what made the second impossible.
#[tokio::test]
async fn a_chat_answered_continuation_can_still_ask() {
    let (db, _run) = parked_on_an_approval("komo-wake-ctx-chat").await;
    let probe = Arc::new(ContextProbe(Mutex::new(None)));
    let dispatcher = probing_dispatcher(probe.clone(), db);

    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent: sent.clone() }) as Arc<dyn ReplySink>;
    dispatcher
        .answer_suspended("s1", None, &Answer::Once, WakeReply::Sink(sink))
        .await;

    let ctx = probed(&probe).await;
    assert!(ctx.interactive, "someone answered; they can answer again");
    ctx.sink.send("⚠️ 需要审批").await.unwrap();
    assert_eq!(
        sent.lock().unwrap().last().map(String::as_str),
        Some("⚠️ 需要审批"),
        "and the next prompt goes to the surface that answered the last one"
    );
    assert!(
        ctx.interject.is_some(),
        "a continuation hears an interjection like any other turn"
    );
}

/// The GUI answers through surfaces it polls rather than a sink — which is a
/// human all the same. The absent sink used to make it indistinguishable from
/// a sweep firing a timer at nobody.
#[tokio::test]
async fn a_polling_client_counts_as_someone_who_can_answer() {
    let (db, _run) = parked_on_an_approval("komo-wake-ctx-gui").await;
    let probe = Arc::new(ContextProbe(Mutex::new(None)));
    let dispatcher = probing_dispatcher(probe.clone(), db);

    dispatcher.answer_approval("s1", None, Answer::Once).await;

    assert!(probed(&probe).await.interactive);
}

/// …and a sweep firing an expiry really is nobody: the continuation stays
/// non-interactive, so a further approval is refused rather than parked on a
/// prompt no one will ever read.
#[tokio::test]
async fn a_sweep_fired_continuation_has_nobody_to_ask() {
    let (db, _run) = parked_on_an_approval("komo-wake-ctx-sweep").await;
    let probe = Arc::new(ContextProbe(Mutex::new(None)));
    let dispatcher = probing_dispatcher(probe.clone(), db.clone());

    let registration = WakeupRepository::list(db.as_ref())
        .await
        .unwrap()
        .pop()
        .expect("the wait is registered");
    dispatcher
        .continue_turn_with(&registration, WakeupCause::Expired, "", WakeReply::Nobody)
        .await
        .unwrap();

    assert!(!probed(&probe).await.interactive);
}

/// A turn that stopped to wait has not failed and has not answered: the prompt
/// or the question already went to this chat, and an error message beside it
/// reads as the request having been refused. Every other ingress told the two
/// apart; this one said "处理消息时出错了: suspended, waiting".
#[tokio::test]
async fn a_suspended_turn_says_nothing_more_to_the_chat() {
    struct Suspends;
    #[async_trait]
    impl MessageHandler for Suspends {
        async fn handle(&self, _session_id: &str, _input: String) -> anyhow::Result<String> {
            Err(komo_core::domain::wakeup::Suspended.into())
        }
    }

    let dispatcher = Arc::new(GatewayDispatcher::new(
        Arc::new(Suspends),
        Arc::new(ApprovalState::new()),
        Arc::new(MemorySessions::default()),
        Arc::new(MemoryHome::default()),
        Arc::new(MemoryTodos::default()),
        None,
        Arc::new(UnusedPairings),
        Arc::new(AlwaysFreshInbox),
    ));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingSink { sent: sent.clone() }) as Arc<dyn ReplySink>;
    dispatcher.dispatch_turn("s1".to_string(), "rm the tree".to_string(), sink, vec![]);

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        sent.lock().unwrap().is_empty(),
        "nothing is said on top of the prompt: {:?}",
        sent.lock().unwrap()
    );
}
