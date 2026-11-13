use crate::service::test_support::harness::FakeLlm;
use crate::service::test_support::{TestGateway, config_toml};
use komo_kernel::durable::{ContentBlock, Message, StopReason};
use komo_kernel::protocol::http::*;
use komo_kernel::session::{EntryKind, SessionTree};
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::turn::TurnRequest;
use komo_runtime::durable::EntryDraft;
use serde_json::{Value, json};

// 这些测试跑在 Durable 存储上（`docs/storage-durable-d5.md` §7，决定 1）：HTTP 调用与行为断言
// 照旧，只改帮手——`seed` 经提交线写真 entry 并返回铸出来的 entry ID（pi 里就是它的十进制），
// `finished` / `wait_done` 等 submission 结算，重启后的位置从存储里重读。

async fn setup() -> (TestGateway, SessionId) {
    let gw = TestGateway::with(&config_toml(""), FakeLlm::finisher("model answer")).await;
    let (status, body) = gw.post("/v1/sessions", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let summary: SessionSummary = serde_json::from_str(&body).unwrap();
    (gw, summary.session)
}

#[tokio::test]
async fn create_session_rejects_the_same_key_with_different_content() {
    let gw = TestGateway::with(&config_toml(""), FakeLlm::finisher("ok")).await;
    let (status, body) = gw
        .post(
            "/v1/sessions",
            json!({"request_key": "same", "title": "one"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = gw
        .post(
            "/v1/sessions",
            json!({"request_key": "same", "title": "two"}),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("request_key_conflict"), "{body}");
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
#[tokio::test]
async fn naming_a_session_is_an_entry_that_leaves_history_alone() {
    let (gw, session) = setup().await;
    let u = seed(&gw, &session, "user", "original").await;
    seed(&gw, &session, "assistant", "answer").await;
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/name"),
            json!({"name":"tree session"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let tree = snapshot(&gw, &session).await;
    assert_eq!(tree.name(), Some("tree session"));
    assert!(
        matches!(&tree.get(&u).unwrap().kind,EntryKind::Message{message} if message["content"]=="original")
    );
    gw.running.stop().await;
}

#[tokio::test]
async fn forked_messages_and_artifacts_survive_source_purge_without_copying_executable_runs() {
    let (gw, session) = setup().await;
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
        .post(&format!("/v1/sessions/{session}/fork"), json!({}))
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

// ---------------------------------------------------------------- 重启与并发

use crate::service::test_support::install_crypto;
use crate::service::{Running, ServiceOptions, start};
use komo_kernel::traits::LlmClient;
use komo_llm::{ModelRequest, ModelStream};

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        install_crypto();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), config_toml("")).unwrap();
        std::fs::write(dir.path().join(".env"), "KOMO_LLM_API_KEY=test-key\n").unwrap();
        Home { dir }
    }

    /// `llm` 同时当辅助调用的 `LlmClient` 与 harness 的 `ModelStream`（Durable）。
    async fn start<T: LlmClient + ModelStream + 'static>(&self, llm: std::sync::Arc<T>) -> Running {
        start(ServiceOptions {
            home: Some(self.dir.path().to_path_buf()),
            listen: Some("127.0.0.1:0".into()),
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
    let (status, text) = call(
        &running,
        reqwest::Method::POST,
        &format!("/v1/sessions/{session}/fork"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, 409, "fork: {text}");
    // 改名不等 Run：忙时只改 sessions.title，不动树（`DurableCore::name`）。
    let (status, text) = call(
        &running,
        reqwest::Method::POST,
        &format!("/v1/sessions/{session}/name"),
        Some(json!({"name": "x"})),
    )
    .await;
    assert_eq!(status, 200, "name: {text}");
    assert_eq!(
        tree_of(&running, &session)
            .await
            .leaf_id
            .unwrap_or_default(),
        target,
        "忙时改名不追加 entry"
    );
    release.notify_waiters();
    running.stop().await;
}
