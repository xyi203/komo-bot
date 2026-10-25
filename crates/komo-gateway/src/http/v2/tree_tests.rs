use crate::service::test_support::harness::FakeLlm;
use crate::service::test_support::{TestGateway, config_toml};
use komo_kernel::durable::{ContentBlock, Message, StopReason};
use komo_kernel::protocol::http::*;
use komo_kernel::session::{EntryKind, SessionTree};
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::turn::TurnRequest;
use komo_runtime::durable::EntryDraft;
use serde_json::{Value, json};

// 这些测试跑在 Durable 存储上（`docs/storage-durable-d5.md` §7，决定 1）：HTTP 调用与行为断言
// 照旧，只改帮手——`seed` 经提交线写真 entry 并返回铸出来的 entry ID（pi 里就是它的十进制），
// `finished` / `wait_done` 等 submission 结算，重启后的位置从存储里重读。

async fn setup() -> (TestGateway, std::sync::Arc<FakeLlm>, SessionId) {
    let llm = FakeLlm::finisher("model answer");
    // Durable 的手动压缩只收"超出 `keep_recent_tokens` 之外"的旧内容，整段比它短就什么都不收
    // （旧实现总是把整段摘掉）：这些几十字的测试历史要压得动，把它调到最小。
    let gw = TestGateway::with(
        &config_toml("[compaction]\nkeep_recent_tokens = 1\n"),
        llm.clone(),
    )
    .await;
    let (status, body) = gw.post("/v1/sessions", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let summary: SessionSummary = serde_json::from_str(&body).unwrap();
    (gw, llm, summary.session)
}

/// 在当前分支末端追加一条消息，返回它的 entry ID（pi 的 entry id）。
async fn seed(gw: &TestGateway, session: &SessionId, role: &str, text: &str) -> String {
    let core = gw.state().core().unwrap();
    let conversation = core.session(session.as_str()).await.unwrap().current_branch;
    let (kind, message) = if role == "assistant" {
        (
            "komo.assistant",
            Message::assistant(vec![ContentBlock::text(text)], StopReason::Stop, 1),
        )
    } else {
        ("komo.user", Message::user(text, 1))
    };
    let id = core
        .engine
        .commit(move |tx| {
            Box::pin(async move {
                Ok(tx
                    .append_entry(
                        conversation,
                        EntryDraft::new(kind).with_model(vec![message]),
                    )
                    .await?
                    .id)
            })
        })
        .await
        .unwrap();
    id.get().to_string()
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
    let record = gw.wait_run(run).await;
    assert_eq!(
        record.status(),
        komo_kernel::durable::SubmissionStatus::Done,
        "{record:?}"
    );
}

#[tokio::test]
async fn navigating_and_editing_an_input_creates_a_sibling_that_alone_reaches_the_model() {
    let (gw, llm, session) = setup().await;
    let u1 = seed(&gw, &session, "user", "shared task").await;
    let a1 = seed(&gw, &session, "assistant", "shared answer").await;
    let u2 = seed(&gw, &session, "user", "abandoned direction").await;
    let a2 = seed(&gw, &session, "assistant", "abandoned answer").await;
    let _ = u1;
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/navigate"),
            json!({"targetId":u2}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let response: NavigateSessionResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(response.leaf_id.as_deref(), Some(a1.as_str()));
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
    let text = serde_json::to_string(&llm.requests.lock().unwrap()[0].messages).unwrap();
    assert!(text.contains("shared task"));
    assert!(text.contains("shared answer"));
    assert!(text.contains("replacement direction"));
    assert!(!text.contains("abandoned direction"));
    assert!(!text.contains("abandoned answer"));
    let tree = snapshot(&gw, &session).await;
    assert!(tree.get(&a2).is_some());
    // 重开存储（先停 Gateway 放掉库锁）：恢复出的位置与停机前 `/tree` 说的一样。
    let leaf_before = tree.leaf_id().map(str::to_owned);
    let home = gw.home.path().to_path_buf();
    gw.running.stop().await;
    let storage = komo_store::durable::TursoStorage::open(&home)
        .await
        .unwrap();
    let data = crate::service::durable::tree::load_tree(&storage, session.as_str())
        .await
        .unwrap();
    let conversations: Vec<_> = data
        .branches
        .iter()
        .map(
            |branch| komo_kernel::durable::pi_tree::ConversationEntries {
                record: &branch.conversation,
                entries: &branch.entries,
            },
        )
        .collect();
    let restored = komo_kernel::durable::pi_tree::pi_tree(
        &conversations,
        data.session.current_branch,
        data.session.leaf,
    );
    assert_eq!(restored.leaf_id, leaf_before);
}

#[tokio::test]
async fn context_edits_and_labels_preserve_raw_history_and_apply_only_on_the_selected_path() {
    let (gw, _, session) = setup().await;
    let u = seed(&gw, &session, "user", "original").await;
    let a = seed(&gw, &session, "assistant", "answer").await;
    for (route, body) in [
        (
            "context-edit",
            json!({"targetId":u,"replacement":{"content":"replacement"}}),
        ),
        ("label", json!({"targetId":a,"label":"checkpoint"})),
        ("name", json!({"name":"tree session"})),
    ] {
        let (status, body) = gw
            .post(&format!("/v1/sessions/{session}/{route}"), body)
            .await;
        assert_eq!(status, 200, "{body}");
    }
    let tree = snapshot(&gw, &session).await;
    assert_eq!(tree.label(&a), Some("checkpoint"));
    assert_eq!(tree.name(), Some("tree session"));
    assert!(
        tree.project()
            .unwrap()
            .messages
            .iter()
            .any(|m| m["content"] == "replacement")
    );
    assert!(
        matches!(&tree.get(&u).unwrap().kind,EntryKind::Message{message} if message["content"]=="original")
    );
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/navigate"),
            json!({"targetId":a}),
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
    let _u = seed(&gw, &session, "user", "task").await;
    let _a = seed(&gw, &session, "assistant", "result").await;
    let source_dir = gw.home.path().join("sessions").join(session.as_str());
    tokio::fs::create_dir_all(source_dir.join("artifacts/r"))
        .await
        .unwrap();
    tokio::fs::write(source_dir.join("artifacts/r/report.txt"), "report evidence")
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
    // 新会话没有 task 与 submission（复制的是可见 entry，不是可执行的东西）。
    let core = gw.state().core().unwrap();
    let clone_row = core.session(response.session.as_str()).await.unwrap();
    let submissions = core
        .storage
        .scan_submissions(
            &komo_kernel::durable::storage::SubmissionQuery {
                conversation: Some(clone_row.root_conversation),
                ..Default::default()
            },
            10,
            None,
        )
        .await
        .unwrap();
    assert!(submissions.items.is_empty());
    let tasks = core
        .storage
        .scan_tasks(
            &komo_kernel::durable::storage::TaskQuery {
                conversation: Some(clone_row.root_conversation),
                ..Default::default()
            },
            10,
            None,
        )
        .await
        .unwrap();
    assert!(tasks.items.is_empty());
    // 源会话被回收（全部 conversation 与目录）：两次 HTTP 调用——逻辑删除，再 purge。
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/delete"),
            json!({"now": true}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = gw
        .post(&format!("/v1/sessions/{session}/purge"), json!({}))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(!source_dir.exists(), "源会话的目录被 purge 删了");
    let clone = snapshot(&gw, &response.session).await;
    assert_eq!(
        tokio::fs::read_to_string(
            gw.home
                .path()
                .join("sessions")
                .join(response.session.as_str())
                .join("artifacts/r/report.txt")
        )
        .await
        .unwrap(),
        "report evidence"
    );
    // 副本的 `/tree` 里有那两条消息（新铸的 ID，所以数条数而不是按原 ID 找）。
    let messages = clone
        .entries()
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Message { .. }))
        .count();
    assert_eq!(messages, 2);
    gw.running.stop().await;
}

#[tokio::test]
async fn summaries_and_compaction_are_entries_on_the_new_path() {
    let (gw, llm, session) = setup().await;
    let _u = seed(&gw, &session, "user", "task").await;
    let a = seed(&gw, &session, "assistant", "answer").await;
    let _left = seed(&gw, &session, "user", "left branch").await;
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/navigate"),
            json!({"targetId":a,"summarize":true}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let tree = snapshot(&gw, &session).await;
    assert!(
        matches!(&tree.get(tree.leaf_id().unwrap()).unwrap().kind,EntryKind::BranchSummary{from_id,..} if *from_id==_left)
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
    {
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
    }
    gw.running.stop().await;
}

// ---------------------------------------------------------------- 重启与并发

use crate::service::test_support::{install_crypto, telegram_config};
use crate::service::{Running, ServiceOptions, start};
use komo_kernel::traits::LlmClient;
use komo_runtime::llm::{ModelRequest, ModelStream};

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

    /// `llm` 同时当辅助调用的 `LlmClient` 与 harness 的 `ModelStream`（Durable）。
    async fn start<T: LlmClient + ModelStream + 'static>(&self, llm: std::sync::Arc<T>) -> Running {
        start(ServiceOptions {
            home: Some(self.dir.path().to_path_buf()),
            listen: Some("127.0.0.1:0".into()),
            channels: Vec::new(),
            llm: Some(llm.clone() as std::sync::Arc<dyn LlmClient>),
            stream: Some(llm as std::sync::Arc<dyn ModelStream>),
            embeddings: None,
            shared_home: Some(self.dir.path().to_path_buf()),
            telemetry: None,
        })
        .await
        .expect("Gateway 起得来")
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
    let record = crate::service::test_support::harness::wait_run(&running.state, run).await;
    assert_eq!(
        record.status(),
        komo_kernel::durable::SubmissionStatus::Done,
        "{record:?}"
    );
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

#[async_trait::async_trait]
impl ModelStream for Stalled {
    async fn stream(
        &self,
        _request: &ModelRequest<'_>,
        _progress: &mut (dyn for<'m> FnMut(&'m komo_kernel::durable::AssistantMessage) + Send),
    ) -> Result<komo_kernel::durable::AssistantMessage, komo_kernel::types::turn::LlmError> {
        self.0.notified().await;
        // 不可重试的拒绝：放行之后这条 run 干净地失败，停机不用等退避。
        Err(komo_kernel::types::turn::LlmError::Rejected {
            status: 400,
            message: "released".into(),
        })
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
