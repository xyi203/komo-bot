//! W2a 验收：run 的视图与取消、`!` 命令的幂等（HTTP 层，Durable 存储）。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::durable::AssistantMessage;
use komo_kernel::protocol::http::{
    CancelRunResponse, ResumeResponse, RunDetail, SessionDetail, SessionSummary, SubmitRunResponse,
};
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::LlmError;
use komo_runtime::llm::{ModelRequest, ModelStream};
use tokio::sync::Semaphore;

use crate::service::test_support::harness::{FakeLlm, GatewayBuilder, TestGateway};
use crate::service::test_support::{config_toml, harness::telegram_config};

/// 放行之前一个请求都不回的模型：在跑的 run 停在"请求模型"上。
struct Gated {
    inner: Arc<FakeLlm>,
    gate: Semaphore,
}

#[async_trait]
impl ModelStream for Gated {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
    ) -> Result<AssistantMessage, LlmError> {
        let _permit = self.gate.acquire().await.expect("闸门没关");
        self.inner.stream(request, progress).await
    }
}

async fn gated() -> (TestGateway, Arc<Gated>, SessionId) {
    let llm = FakeLlm::finisher("好");
    let gated = Arc::new(Gated {
        inner: llm.clone(),
        gate: Semaphore::new(0),
    });
    let gateway = GatewayBuilder::new(&config_toml(""))
        .fake_llm(llm)
        .stream(gated.clone())
        .start()
        .await;
    let (status, body) = gateway.post("/v1/sessions", serde_json::json!({})).await;
    assert_eq!(status, 200, "{body}");
    let session = serde_json::from_str::<SessionSummary>(&body)
        .unwrap()
        .session;
    (gateway, gated, session)
}

async fn submit(gateway: &TestGateway, session: &SessionId, key: &str) -> SubmitRunResponse {
    let (status, body) = gateway
        .post(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({"request_key": key, "text": key}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

async fn detail(gateway: &TestGateway, run: &RunId) -> RunDetail {
    let (status, body) = gateway.get(&format!("/v1/runs/{run}")).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

async fn cancel(gateway: &TestGateway, run: &RunId) -> CancelRunResponse {
    let (status, body) = gateway
        .post(&format!("/v1/runs/{run}/cancel"), serde_json::json!({}))
        .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn cancelling_stops_the_running_run_and_withdraws_a_queued_one() {
    let (mut gateway, gated, session) = gated().await;
    let first = submit(&gateway, &session, "first").await;
    let second = submit(&gateway, &session, "second").await;
    assert_eq!(second.state, RunState::Queued);

    // 会话详情与 resume 都列得出这两条没结束的 run。
    let (_, body) = gateway.get(&format!("/v1/sessions/{session}")).await;
    let shown: SessionDetail = serde_json::from_str(&body).unwrap();
    let runs: Vec<_> = shown.unfinished.iter().map(|run| run.run.clone()).collect();
    assert_eq!(runs, [first.run.clone(), second.run.clone()], "{shown:?}");
    assert_eq!(shown.summary.current_run.as_ref(), Some(&first.run));
    let (status, body) = gateway
        .post(
            &format!("/v1/sessions/{session}/resume"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let resumed: ResumeResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(resumed.resumed.len(), 2);

    // 排队中的单条：撤回，不碰正在跑的。
    assert_eq!(
        cancel(&gateway, &second.run).await.state,
        RunState::Cancelled
    );
    assert_eq!(
        detail(&gateway, &first.run).await.summary.state,
        RunState::Running
    );
    // 在跑的：只停这一轮；重复取消是幂等的。
    assert_eq!(
        cancel(&gateway, &first.run).await.state,
        RunState::Cancelled
    );
    assert_eq!(
        cancel(&gateway, &first.run).await.state,
        RunState::Cancelled
    );
    let stopped = detail(&gateway, &first.run).await;
    assert_eq!(stopped.summary.state, RunState::Cancelled, "{stopped:?}");
    assert!(stopped.summary.ended_at.is_some());
    let (_, body) = gateway.get(&format!("/v1/sessions/{session}")).await;
    let shown: SessionDetail = serde_json::from_str(&body).unwrap();
    assert!(shown.unfinished.is_empty(), "{shown:?}");

    gated.gate.add_permits(100);
    gateway.stop().await;
}

#[tokio::test]
async fn an_unknown_or_legacy_run_id_is_not_found() {
    let gateway = GatewayBuilder::new(&config_toml(""))
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;
    for id in ["999999", "0192f0a0-0000-7000-8000-000000000000", "abc"] {
        let (status, body) = gateway.get(&format!("/v1/runs/{id}")).await;
        assert_eq!(status, 404, "{id}: {body}");
        let (status, body) = gateway
            .post(&format!("/v1/runs/{id}/cancel"), serde_json::json!({}))
            .await;
        assert_eq!(status, 404, "{id}: {body}");
    }
}

#[tokio::test]
async fn a_replayed_bang_command_runs_once_and_other_content_conflicts() {
    let llm = FakeLlm::finisher("好");
    let gateway = GatewayBuilder::new(&telegram_config("111"))
        .fake_llm(llm.clone())
        .start()
        .await;
    let (_, body) = gateway.post("/v1/sessions", serde_json::json!({})).await;
    let session = serde_json::from_str::<SessionSummary>(&body)
        .unwrap()
        .session;
    let count = gateway.home.join("workspaces").join("bang-once.count");
    std::fs::create_dir_all(count.parent().unwrap()).unwrap();
    let text = format!("echo hi; echo x >> {}", count.display());
    let send = |text: String| {
        let path = format!("/v1/sessions/{session}/runs");
        let gateway = &gateway;
        async move {
            gateway
                .post(
                    &path,
                    serde_json::json!({"request_key": "bang-k", "text": text, "shell": true}),
                )
                .await
        }
    };
    let (status, body) = send(text.clone()).await;
    assert_eq!(status, 200, "{body}");
    let first: SubmitRunResponse = serde_json::from_str(&body).unwrap();
    assert!(!first.deduplicated);
    let (status, body) = send(text.clone()).await;
    assert_eq!(status, 200, "{body}");
    let again: SubmitRunResponse = serde_json::from_str(&body).unwrap();
    assert!(again.deduplicated);
    assert_eq!(again.run, first.run);
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1);
    let (status, body) = send("echo other".into()).await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("request_key_conflict"), "{body}");

    // `!` 的 run 有详情：已完成，输出就是最终答复；全程没有模型请求。
    let shown = detail(&gateway, &first.run).await;
    assert_eq!(shown.summary.state, RunState::Completed, "{shown:?}");
    assert!(shown.final_message.is_some_and(|text| text.contains("hi")));
    assert_eq!(llm.turns(), 0);
}

/// `!` 命令转后台的阈值设成 1 秒的一台。
fn detaching_config() -> String {
    config_toml("[execution]\ndetach_after_secs = 1\n")
}

async fn open_session(gateway: &TestGateway) -> SessionId {
    let (status, body) = gateway.post("/v1/sessions", serde_json::json!({})).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str::<SessionSummary>(&body)
        .unwrap()
        .session
}

async fn bang(
    gateway: &TestGateway,
    session: &SessionId,
    key: &str,
    command: &str,
) -> SubmitRunResponse {
    let (status, body) = gateway
        .post(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({"request_key": key, "text": command, "shell": true}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// 会话当前分支上所有 entry 的 JSON（断言用）。
async fn history(gateway: &TestGateway, session: &SessionId) -> String {
    let core = gateway.state().core().unwrap();
    let record = core.session(session.as_str()).await.unwrap();
    let page = core
        .storage
        .scan_entries(
            &komo_kernel::durable::storage::EntryQuery::new(record.current_branch),
            10_000,
            None,
        )
        .await
        .unwrap();
    page.items
        .iter()
        .map(|stored| serde_json::to_string(stored).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn wait_history(gateway: &TestGateway, session: &SessionId, needle: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let text = history(gateway, session).await;
        if text.contains(needle) {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等不到 {needle}：{text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// 跑过阈值的 `!` 命令立刻回一张“已转后台”的回执，命令接着跑完、照常落进历史；重发不再跑一遍；
/// 全程没有模型请求。子进程带着 `KOMO_RUN` / `KOMO_SESSION`：里面的 `komo gateway restart`
/// 认得出自己跑在 Gateway 里、改走延后重启（`units::inside_gateway`）。
#[tokio::test]
async fn a_bang_command_past_the_threshold_detaches_and_lands_when_it_ends() {
    let llm = FakeLlm::finisher("好");
    let gateway = GatewayBuilder::new(&detaching_config())
        .fake_llm(llm.clone())
        .start()
        .await;
    let session = open_session(&gateway).await;
    let count = gateway.home.join("workspaces").join("bang-detach.count");
    std::fs::create_dir_all(count.parent().unwrap()).unwrap();
    let command = format!(
        "echo x >> {}; echo started; sleep 3; echo \"run=[$KOMO_RUN] session=[$KOMO_SESSION]\"; echo finished-line",
        count.display()
    );

    let started = std::time::Instant::now();
    let receipt = bang(&gateway, &session, "bang-d", &command).await;
    assert!(
        started.elapsed() < std::time::Duration::from_millis(2500),
        "没等命令跑完：{:?}",
        started.elapsed()
    );
    assert!(receipt.detached, "{receipt:?}");
    assert!(!receipt.deduplicated);
    assert_eq!(receipt.state, RunState::Running);
    assert!(receipt.run.as_str().starts_with("shell-"), "{receipt:?}");

    // 还在后台跑时重发：认得出是同一条，不再跑；内容不同是冲突。
    let again = bang(&gateway, &session, "bang-d", &command).await;
    assert!(again.detached && again.deduplicated, "{again:?}");
    assert_eq!(again.run, receipt.run);
    let (status, body) = gateway
        .post(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({"request_key": "bang-d", "text": "echo other", "shell": true}),
        )
        .await;
    assert_eq!(status, 409, "{body}");

    let text = wait_history(&gateway, &session, "finished-line").await;
    assert!(text.contains("shell.command"), "{text}");
    assert!(!text.contains("run=[]"), "子进程带着 KOMO_RUN：{text}");
    assert!(
        text.contains(&format!("session=[{session}]")),
        "子进程带着 KOMO_SESSION：{text}"
    );
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1);

    // 落账之后重发：拿到的是落下的那条 submission，仍然不再跑。
    let landed = bang(&gateway, &session, "bang-d", &command).await;
    assert!(landed.deduplicated && !landed.detached, "{landed:?}");
    assert_eq!(landed.state, RunState::Completed);
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1);
    assert_eq!(llm.turns(), 0, "不起模型 Run");
}

/// 转了后台的 `!` 命令仍有硬上限 `call_timeout_secs`：到点终止进程组，超时的结果照样落进历史。
#[tokio::test]
async fn a_detached_bang_command_is_still_stopped_at_the_call_timeout() {
    let config = config_toml("[execution]\ndetach_after_secs = 1\ncall_timeout_secs = 3\n");
    let gateway = GatewayBuilder::new(&config)
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;
    let session = open_session(&gateway).await;
    let started = std::time::Instant::now();
    let receipt = bang(
        &gateway,
        &session,
        "bang-t",
        "echo cap-start; sleep 30; echo late-$((40+2))",
    )
    .await;
    assert!(receipt.detached, "{receipt:?}");

    let text = wait_history(&gateway, &session, "cap-start").await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "到点就停，没等 sleep 30：{:?}",
        started.elapsed()
    );
    assert!(text.contains("timeout=3s"), "报的是超时：{text}");
    assert!(!text.contains("late-42"), "{text}");
    assert!(!text.contains("\"interrupted\":true"), "不是停机：{text}");
}

/// 正常停机时还在后台跑的 `!` 命令被终止，历史里留一条说明（带着到那时为止的输出）；重启后不重跑。
#[tokio::test]
async fn stopping_the_gateway_ends_a_detached_bang_command_and_notes_it_in_history() {
    let mut gateway = GatewayBuilder::new(&detaching_config())
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;
    let session = open_session(&gateway).await;
    let receipt = bang(&gateway, &session, "bang-s", "echo early-output; sleep 60").await;
    assert!(receipt.detached, "{receipt:?}");

    let stopping = std::time::Instant::now();
    gateway.restart_with(&detaching_config(), Vec::new()).await;
    assert!(
        stopping.elapsed() < std::time::Duration::from_secs(15),
        "没等那条 sleep：{:?}",
        stopping.elapsed()
    );
    let text = history(&gateway, &session).await;
    assert!(text.contains("Gateway 停机时它还没跑完"), "{text}");
    assert!(
        text.contains("early-output"),
        "到那时为止的输出也在：{text}"
    );
    assert!(text.contains("\"interrupted\":true"), "{text}");
}
