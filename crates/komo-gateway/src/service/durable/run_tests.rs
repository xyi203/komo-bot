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
