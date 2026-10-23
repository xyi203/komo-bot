use super::*;
use crate::service::test_support::harness::FakeLlm;
use crate::service::test_support::{TestGateway, config_toml};

async fn setup() -> (TestGateway, std::sync::Arc<FakeLlm>, SessionId) {
    let llm = FakeLlm::finisher("model answer");
    let gw = TestGateway::with(&config_toml(""), Some(llm.clone())).await;
    let (status, body) = gw.post("/v1/sessions", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let summary: SessionSummary = serde_json::from_str(&body).unwrap();
    (gw, llm, summary.session)
}
async fn seed(
    gw: &TestGateway,
    session: &SessionId,
    id: &str,
    parent: Option<&str>,
    role: &str,
    text: &str,
) {
    let item:SessionEntry=serde_json::from_value(json!({"id":id,"parentId":parent,"timestamp":"2026-10-23T00:00:00Z","type":"message","message":{"role":role,"content":if role=="assistant"{json!([{"type":"text","text":text}])}else{json!(text)},"provider":"responses","model":"gpt-test"}})).unwrap();
    gw.state()
        .ledgers
        .open(session, "agent")
        .await
        .unwrap()
        .coordinator
        .append_session_entry(item)
        .await
        .unwrap();
}
async fn snapshot(gw: &TestGateway, session: &SessionId) -> SessionTree {
    let (status, body) = gw.get(&format!("/v1/sessions/{session}/tree")).await;
    assert_eq!(status, 200, "{body}");
    let response: SessionTreeResponse = serde_json::from_str(&body).unwrap();
    let mut tree = SessionTree::from_entries(response.entries).unwrap();
    tree.branch(response.leaf_id.as_deref()).unwrap();
    tree
}
async fn finished(gw: &TestGateway, run: &RunId) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let record = komo_store::repos::runs::get(&gw.state().db, run)
                .await
                .unwrap()
                .unwrap();
            if record.state.is_terminal() {
                assert_eq!(
                    record.state,
                    komo_kernel::types::status::RunState::Completed,
                    "{:?}",
                    record.last_error
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("run finishes");
}

#[tokio::test]
async fn navigating_and_editing_an_input_creates_a_sibling_that_alone_reaches_the_model() {
    let (gw, llm, session) = setup().await;
    seed(&gw, &session, "u1", None, "user", "shared task").await;
    seed(
        &gw,
        &session,
        "a1",
        Some("u1"),
        "assistant",
        "shared answer",
    )
    .await;
    seed(
        &gw,
        &session,
        "u2",
        Some("a1"),
        "user",
        "abandoned direction",
    )
    .await;
    seed(
        &gw,
        &session,
        "a2",
        Some("u2"),
        "assistant",
        "abandoned answer",
    )
    .await;
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/navigate"),
            json!({"targetId":"u2"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let response: NavigateSessionResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(response.leaf_id.as_deref(), Some("a1"));
    assert_eq!(response.editor_text.as_deref(), Some("abandoned direction"));
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/runs"),
            json!({"request_key":"branch-input","text":"replacement direction"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let accepted: SubmitRunResponse = serde_json::from_str(&body).unwrap();
    finished(&gw, &accepted.run).await;
    let requests = llm.requests.lock().unwrap();
    let text = serde_json::to_string(&requests[0].messages).unwrap();
    assert!(text.contains("shared task"));
    assert!(text.contains("shared answer"));
    assert!(text.contains("replacement direction"));
    assert!(!text.contains("abandoned direction"));
    assert!(!text.contains("abandoned answer"));
    drop(requests);
    let tree = snapshot(&gw, &session).await;
    assert!(tree.get("a2").is_some());
    let events =
        komo_store::session_log::read_all(&gw.state().ledgers.paths_for(&session), &session)
            .await
            .unwrap();
    let restored = komo_kernel::session_events::TreeState::from_events(&events)
        .unwrap()
        .tree()
        .unwrap();
    assert_eq!(restored.leaf_id(), tree.leaf_id());
    gw.running.stop().await;
}

#[tokio::test]
async fn context_edits_and_labels_preserve_raw_history_and_apply_only_on_the_selected_path() {
    let (gw, _, session) = setup().await;
    seed(&gw, &session, "u", None, "user", "original").await;
    seed(&gw, &session, "a", Some("u"), "assistant", "answer").await;
    for (route, body) in [
        (
            "context-edit",
            json!({"targetId":"u","replacement":{"content":"replacement"}}),
        ),
        ("label", json!({"targetId":"a","label":"checkpoint"})),
        ("name", json!({"name":"tree session"})),
    ] {
        let (status, body) = gw
            .post(&format!("/v1/sessions/{session}/{route}"), body)
            .await;
        assert_eq!(status, 200, "{body}");
    }
    let tree = snapshot(&gw, &session).await;
    assert_eq!(tree.label("a"), Some("checkpoint"));
    assert_eq!(tree.name(), Some("tree session"));
    assert!(
        tree.project()
            .unwrap()
            .messages
            .iter()
            .any(|m| m["content"] == "replacement")
    );
    assert!(
        matches!(&tree.get("u").unwrap().kind,EntryKind::Message{message} if message["content"]=="original")
    );
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/navigate"),
            json!({"targetId":"a"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let tree = snapshot(&gw, &session).await;
    assert!(
        tree.project()
            .unwrap()
            .messages
            .iter()
            .any(|m| m["content"] == "original")
    );
    gw.running.stop().await;
}

#[tokio::test]
async fn cloned_messages_and_artifacts_survive_source_purge_without_copying_executable_runs() {
    let (gw, _, session) = setup().await;
    seed(&gw, &session, "u", None, "user", "task").await;
    seed(&gw, &session, "a", Some("u"), "assistant", "result").await;
    let root = gw.state().ledgers.paths_for(&session);
    tokio::fs::create_dir_all(root.artifacts().join("r"))
        .await
        .unwrap();
    tokio::fs::write(root.artifacts().join("r/report.txt"), "report evidence")
        .await
        .unwrap();
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/fork"),
            json!({"clone":true}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let response: ForkSessionResponse = serde_json::from_str(&body).unwrap();
    assert!(
        komo_store::repos::runs::list_for_session(&gw.state().db, &response.session)
            .await
            .unwrap()
            .is_empty()
    );
    for route in ["delete", "purge"] {
        let (status, body) = gw
            .post(&format!("/v1/sessions/{session}/{route}"), json!({}))
            .await;
        assert_eq!(status, 200, "{body}");
    }
    let clone = snapshot(&gw, &response.session).await;
    assert!(clone.get("a").is_some());
    assert_eq!(
        tokio::fs::read_to_string(
            gw.state()
                .ledgers
                .paths_for(&response.session)
                .artifacts()
                .join("r/report.txt")
        )
        .await
        .unwrap(),
        "report evidence"
    );
    let records = komo_store::session_log::scan_records(
        &gw.state().ledgers.paths_for(&response.session),
        &response.session,
    )
    .await
    .unwrap();
    assert_eq!(records.len(), 2);
    gw.running.stop().await;
}

#[tokio::test]
async fn summaries_and_compaction_are_entries_on_the_new_path() {
    let (gw, llm, session) = setup().await;
    seed(&gw, &session, "u", None, "user", "task").await;
    seed(&gw, &session, "a", Some("u"), "assistant", "answer").await;
    seed(&gw, &session, "left", Some("a"), "user", "left branch").await;
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/navigate"),
            json!({"targetId":"a","summarize":true}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let tree = snapshot(&gw, &session).await;
    assert!(
        matches!(&tree.get(tree.leaf_id().unwrap()).unwrap().kind,EntryKind::BranchSummary{from_id,..} if from_id=="left")
    );
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/compact"),
            json!({"instructions":"keep decisions"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let tree = snapshot(&gw, &session).await;
    assert_eq!(
        tree.project()
            .unwrap()
            .messages
            .iter()
            .filter(|m| m["role"] == "compactionSummary")
            .count(),
        1
    );
    assert_eq!(llm.turns(), 2);
    // The summary input is pi's serialized conversation, not raw entry JSON.
    let requests = llm.requests.lock().unwrap();
    for request in requests.iter() {
        let input = request.messages[0].text.as_deref().unwrap();
        assert!(input.starts_with("<conversation>"), "{input}");
        assert!(input.contains("[User]: "), "{input}");
        assert!(!input.contains("parentId"), "{input}");
    }
    assert!(
        requests[0].messages[0]
            .text
            .as_deref()
            .unwrap()
            .contains("left branch")
    );
    assert!(
        !requests[0].messages[0]
            .text
            .as_deref()
            .unwrap()
            .contains("[User]: task")
    );
    gw.running.stop().await;
}

// ---------------------------------------------------------------- 重启、并发与旧数据

use crate::service::test_support::{install_crypto, telegram_config};
use crate::service::{Running, ServiceOptions, start};
use komo_kernel::traits::LlmClient;

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        install_crypto();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), telegram_config("111")).unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "KOMO_LLM_API_KEY=test-key\nTELEGRAM_BOT_TOKEN=test-bot-token\n",
        )
        .unwrap();
        Home { dir }
    }

    async fn start(&self, llm: std::sync::Arc<dyn LlmClient>) -> Running {
        start(ServiceOptions {
            home: Some(self.dir.path().to_path_buf()),
            listen: Some("127.0.0.1:0".into()),
            channels: Vec::new(),
            llm: Some(llm),
            embeddings: None,
            shared_home: Some(self.dir.path().to_path_buf()),
            telemetry: None,
        })
        .await
        .expect("Gateway 起得来")
    }

    fn events(&self, session: &SessionId) -> std::path::PathBuf {
        self.dir
            .path()
            .join("sessions")
            .join(session.as_str())
            .join("events.jsonl")
    }
}

async fn call(
    running: &Running,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, String) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{}{path}", running.base_url))
        .bearer_auth(&running.state.token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    (
        response.status().as_u16(),
        response.text().await.unwrap_or_default(),
    )
}

async fn tree_of(running: &Running, session: &SessionId) -> SessionTreeResponse {
    let (status, body) = call(
        running,
        reqwest::Method::GET,
        &format!("/v1/sessions/{session}/tree"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

async fn wait_done(running: &Running, run: &RunId) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let record = komo_store::repos::runs::get(&running.state.db, run)
                .await
                .unwrap()
                .unwrap();
            if record.state.is_terminal() {
                assert_eq!(
                    record.state,
                    komo_kernel::types::status::RunState::Completed
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("run finishes");
}

async fn say(running: &Running, session: &SessionId, key: &str, text: &str) -> RunId {
    let (status, body) = call(
        running,
        reqwest::Method::POST,
        &format!("/v1/sessions/{session}/runs"),
        Some(json!({"request_key": key, "text": text})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let run = serde_json::from_str::<SubmitRunResponse>(&body)
        .unwrap()
        .run;
    wait_done(running, &run).await;
    run
}

/// A model that never answers until released: the Run stays unfinished.
struct Stalled(std::sync::Arc<tokio::sync::Notify>);

#[async_trait::async_trait]
impl LlmClient for Stalled {
    async fn begin_turn(
        &self,
        _request: TurnRequest,
    ) -> Result<Box<dyn komo_kernel::traits::TurnDriver>, komo_kernel::types::turn::LlmError> {
        self.0.notified().await;
        Err(komo_kernel::types::turn::LlmError::Unknown(
            "released".into(),
        ))
    }
}

#[tokio::test]
async fn a_branch_and_its_current_position_survive_a_gateway_restart() {
    let home = Home::new();
    let llm = FakeLlm::finisher("model answer");
    let running = home.start(llm.clone()).await;
    let (_, body) = call(
        &running,
        reqwest::Method::POST,
        "/v1/sessions",
        Some(json!({})),
    )
    .await;
    let session = serde_json::from_str::<SessionSummary>(&body)
        .unwrap()
        .session;
    say(&running, &session, "k1", "first question").await;
    let tree = tree_of(&running, &session).await;
    let first_input = tree
        .entries
        .iter()
        .find(|entry| matches!(&entry.kind, EntryKind::Message { message } if message["content"] == "first question"))
        .unwrap()
        .id
        .clone();
    // Edit the first input: navigation puts the leaf before it, the new input branches.
    let (status, body) = call(
        &running,
        reqwest::Method::POST,
        &format!("/v1/sessions/{session}/navigate"),
        Some(json!({"targetId": first_input})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    say(&running, &session, "k2", "edited question").await;
    let before = tree_of(&running, &session).await;
    running.stop().await;

    let llm = FakeLlm::finisher("after restart");
    let running = home.start(llm.clone()).await;
    let after = tree_of(&running, &session).await;
    assert_eq!(
        after.leaf_id, before.leaf_id,
        "最后写入的 entry 就是恢复出的位置"
    );
    assert_eq!(after.entries, before.entries, "两个分支都在");
    say(&running, &session, "k3", "third question").await;
    let sent = serde_json::to_string(&llm.requests.lock().unwrap()[0].messages).unwrap();
    assert!(sent.contains("edited question") && sent.contains("third question"));
    assert!(!sent.contains("first question"), "旧分支不进模型：{sent}");
    running.stop().await;
}

#[tokio::test]
async fn tree_changes_are_refused_while_a_run_is_unfinished() {
    let home = Home::new();
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let running = home
        .start(std::sync::Arc::new(Stalled(release.clone())))
        .await;
    let (_, body) = call(
        &running,
        reqwest::Method::POST,
        "/v1/sessions",
        Some(json!({})),
    )
    .await;
    let session = serde_json::from_str::<SessionSummary>(&body)
        .unwrap()
        .session;
    let (status, _) = call(
        &running,
        reqwest::Method::POST,
        &format!("/v1/sessions/{session}/runs"),
        Some(json!({"request_key": "busy", "text": "work"})),
    )
    .await;
    assert_eq!(status, 200);
    let target = tree_of(&running, &session)
        .await
        .leaf_id
        .unwrap_or_default();
    for (route, body) in [
        ("navigate", json!({"targetId": target})),
        ("compact", json!({})),
        ("name", json!({"name": "x"})),
        ("fork", json!({"clone": true})),
    ] {
        let (status, text) = call(
            &running,
            reqwest::Method::POST,
            &format!("/v1/sessions/{session}/{route}"),
            Some(body),
        )
        .await;
        assert_eq!(status, 409, "{route}: {text}");
    }
    release.notify_waiters();
    running.stop().await;
}

fn legacy_line(
    seq: u64,
    run: &str,
    payload: komo_kernel::events::EventPayload,
    session: &SessionId,
) -> String {
    use komo_kernel::events::{EVENT_FORMAT_VERSION, Event};
    Event {
        entry: None,
        v: EVENT_FORMAT_VERSION,
        seq: Seq(seq),
        event_id: EventId::from_raw(format!("legacy-{seq}")),
        session: session.clone(),
        run: Some(RunId::from_raw(run)),
        ts: time::macros::datetime!(2026-10-01 00:00 UTC) + time::Duration::seconds(seq as i64),
        payload,
    }
    .to_line()
    .unwrap()
}

#[tokio::test]
async fn a_pre_tree_session_is_browsable_without_rewriting_and_continues_in_the_same_tree() {
    use komo_kernel::events::*;
    use komo_kernel::types::digest::ContentHash;
    use komo_kernel::types::plan::PlanSource;
    let home = Home::new();
    let running = home.start(FakeLlm::finisher("unused")).await;
    let (_, body) = call(
        &running,
        reqwest::Method::POST,
        "/v1/sessions",
        Some(json!({})),
    )
    .await;
    let session = serde_json::from_str::<SessionSummary>(&body)
        .unwrap()
        .session;
    running.stop().await;

    // The session as an older komo wrote it: no header, v:1 lines, no pi entries.
    let accepted = RunAccepted {
        request_key: komo_kernel::types::ids::RequestKey::new("old-1"),
        input_hash: ContentHash::of_str("old question"),
        text: Some("old question".into()),
        text_ref: None,
        source: PlanSource::Interactive {
            session: session.clone(),
        },
        peer: None,
        model: None,
        effort: None,
        delegate: None,
        snapshot: None,
        interject: false,
    };
    let assistant = MessageAssistant {
        round: 1,
        text: Some("old answer".into()),
        text_ref: None,
        tool_calls: vec![],
        provider_blocks: None,
        input_tokens: None,
        output_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
    };
    let lines = [
        legacy_line(1, "old-run", EventPayload::RunAccepted(accepted), &session),
        legacy_line(
            2,
            "old-run",
            EventPayload::RunQueued(RunQueued {
                input_ref: EventId::from_raw("legacy-1"),
            }),
            &session,
        ),
        legacy_line(
            3,
            "old-run",
            EventPayload::RunStarted(RunStarted {
                executor: komo_kernel::types::ids::ExecutorId::from_raw("ex"),
                generation: 1,
            }),
            &session,
        ),
        legacy_line(
            4,
            "old-run",
            EventPayload::MessageAssistant(assistant),
            &session,
        ),
        legacy_line(
            5,
            "old-run",
            EventPayload::RunCompleted(RunCompleted {
                final_message: Some("old answer".into()),
                final_message_ref: None,
                rounds: 1,
            }),
            &session,
        ),
    ];
    let original = lines.join("\n") + "\n";
    std::fs::write(home.events(&session), &original).unwrap();

    let llm = FakeLlm::finisher("new answer");
    let running = home.start(llm.clone()).await;
    // Read-only browsing: the file is byte-identical afterwards.
    let tree = tree_of(&running, &session).await;
    let texts: Vec<_> = tree
        .entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::Message { message } => Some(message["role"].as_str().unwrap().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["user", "assistant"], "{:?}", tree.entries);
    let (status, _) = call(
        &running,
        reqwest::Method::GET,
        &format!("/v1/sessions/{session}/events"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        std::fs::read_to_string(home.events(&session)).unwrap(),
        original
    );

    // Continuing writes new-format records after the old bytes, in one tree.
    say(&running, &session, "new-1", "new question").await;
    let text = std::fs::read_to_string(home.events(&session)).unwrap();
    assert!(text.starts_with(&original), "旧字节原样保留为前缀");
    let sent = serde_json::to_string(&llm.requests.lock().unwrap()[0].messages).unwrap();
    assert!(
        sent.contains("old question")
            && sent.contains("old answer")
            && sent.contains("new question")
    );
    // The new records carry v:2 and no seq: an older komo cannot read them as a linear log.
    let appended = text[original.len()..].lines().next().unwrap();
    let value: Value = serde_json::from_str(appended).unwrap();
    assert_eq!(value["v"], 2);
    assert!(value.get("seq").is_none());
    running.stop().await;
}
