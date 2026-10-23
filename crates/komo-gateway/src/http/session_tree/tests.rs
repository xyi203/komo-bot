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
    gw.running.stop().await;
}
