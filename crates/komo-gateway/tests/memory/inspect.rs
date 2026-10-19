//! `GET /v1/runs/{id}` 的记忆那一面：注入了哪些条目、召回是怎么来的。

use std::sync::Arc;

use komo_kernel::protocol::http::RunDetail;
use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::traits::LlmClient;
use komo_kernel::types::ids::{MemoryId, RunId};
use komo_kernel::types::memory::{MemoryState, Provenance, RetrievalMode};

use crate::harness::*;
use crate::{seeded, text_round};

async fn detail(gateway: &TestGateway, run: &RunId) -> RunDetail {
    let (status, body) = gateway.get(&format!("/v1/runs/{run}")).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).expect("RunDetail")
}

/// 第一条 Run 自己记了 `memory.injected`：`memories` 列出 `id@revision`，`recall` 带着
/// 召回诊断；同一段里的第二条 Run 没有自己的事件，沿用并标出来。
#[tokio::test]
async fn run_detail_lists_injected_memories_and_how_they_were_recalled() {
    let embeddings = ConceptEmbeddings::new();
    let llm = ScriptedLlm::new(vec![
        vec![text_round(1, "26 度。")],
        vec![text_round(1, "还是 26 度。")],
    ]);
    let gateway = memory_gateway(&memory_config("hybrid", true))
        .embeddings(Arc::clone(&embeddings) as Arc<dyn komo_kernel::traits::EmbeddingClient>)
        .llm(Arc::new(llm.clone()) as Arc<dyn LlmClient>)
        .start()
        .await;
    gateway
        .state()
        .memory
        .put(
            seeded(
                "m-1",
                "客厅空调设 26 度",
                Provenance::UserStatement,
                MemoryState::Active,
            ),
            None,
        )
        .await
        .unwrap();
    gateway.state().memories.build_index().await.unwrap();

    let (session, first) = crate::a_run(&gateway, "ri-1", "空调几度来着").await;
    wait_for_terminal(&gateway, &first).await;
    let one = detail(&gateway, &first).await;
    assert_eq!(one.memories.len(), 1, "{:?}", one.memories);
    assert_eq!(one.memories[0].memory, MemoryId::from_raw("m-1"));
    assert_eq!(one.memories[0].revision, 1);
    let recall = one.recall.expect("自己记的事件带着召回诊断");
    assert_eq!(recall.mode, Some(RetrievalMode::Hybrid));
    assert!(!recall.degraded && !recall.reused_pin);
    assert!(
        recall.candidates_after_fusion.unwrap_or(0) >= 1,
        "{recall:?}"
    );
    assert_eq!(recall.dropped_by_budget, Some(0));

    let (status, body) = gateway
        .post_json(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({"request_key": "ri-2", "text": "空调几度来着"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let second: RunId = serde_json::from_value(body["run"].clone()).expect("run id");
    wait_for_terminal(&gateway, &second).await;
    let two = detail(&gateway, &second).await;
    assert_eq!(two.memories, one.memories, "同一段沿用同一块");
    assert!(two.recall.expect("沿用要说出来").reused_pin);
}
