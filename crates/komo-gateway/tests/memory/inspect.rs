//! `GET /v1/runs/{id}` 的记忆那一面：注入了哪些条目、召回是怎么来的。

use std::sync::Arc;

use komo_kernel::protocol::http::RunDetail;
use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::types::ids::{MemoryId, RunId};
use komo_kernel::types::memory::{MemoryState, Provenance, RetrievalMode};

use super::{Scripted, a_run, run_in, wait_for_terminal};
use crate::harness::*;
use crate::{seeded, text_round};

async fn detail(gateway: &TestGateway, run: &RunId) -> RunDetail {
    let (status, body) = gateway.get(&format!("/v1/runs/{run}")).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).expect("RunDetail")
}

/// 每个 Run 的召回块投影成它自己的 `memory.injected`（§9.4）：`memories` 列出 `id@revision`，
/// `recall` 带着召回诊断。同一段里第二个 Run 问同一件事，那条已经展示过、不再追加：它的块是
/// 空的，召回诊断照记。
#[tokio::test]
async fn run_detail_lists_injected_memories_and_how_they_were_recalled() {
    let embeddings = ConceptEmbeddings::new();
    let llm = ScriptedLlm::new(vec![
        vec![text_round(1, "26 度。")],
        vec![text_round(1, "还是 26 度。")],
    ]);
    let gateway = super::gateway(&memory_config("hybrid", true))
        .embeddings(Arc::clone(&embeddings) as Arc<dyn komo_kernel::traits::EmbeddingClient>)
        .scripted(llm.clone())
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

    let (session, first) = a_run(&gateway, "ri-1", "空调几度来着").await;
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

    let second: RunId = run_in(&gateway, &session, "ri-2", "空调几度来着").await;
    wait_for_terminal(&gateway, &second).await;
    let two = detail(&gateway, &second).await;
    assert!(two.memories.is_empty(), "本段已展示过：{:?}", two.memories);
    let recall = two.recall.expect("自己召回过一次");
    assert_eq!(recall.mode, Some(RetrievalMode::Hybrid));
    assert!(!recall.reused_pin);
}
