//! confirm / forget 与它们对 resume 的影响（§9.6、§9.7、§14 ⑦）。

use std::sync::Arc;

use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::traits::LlmClient;
use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::memory::{MemoryState, Provenance};

use crate::harness::*;
use crate::{seeded, text_round};

/// §14 ⑦：confirm / forget **带着预期 revision**，重复请求幂等。
#[tokio::test]
async fn confirm_and_forget_are_idempotent_over_http() {
    let (gateway, _embeddings) = crate::with_memories(
        "keyword",
        vec![seeded(
            "m-1",
            "客厅空调设 26 度",
            Provenance::UserStatement,
            MemoryState::Active,
        )],
    )
    .await;

    // 版本不符：拒绝，并说清当前是第几版。
    let (status, body) = gateway
        .post(
            "/v1/memories/m-1/confirm",
            serde_json::json!({"expected_revision": 7}),
        )
        .await;
    assert_ne!(status, 200, "{body}");
    assert_eq!(
        body["error"]["code"],
        serde_json::json!("version_conflict"),
        "{body}"
    );

    // 对上了就确认。**只有这条路抬得起确认等级**（§9.2）。
    let (status, body) = gateway
        .post(
            "/v1/memories/m-1/confirm",
            serde_json::json!({"expected_revision": 1, "request_key": "cli-confirm-1"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["memory"]["confirmation"],
        serde_json::json!("user_confirmed")
    );
    assert_eq!(
        body["memory"]["revision"],
        serde_json::json!(1),
        "确认不产生新版本"
    );

    // 同一个请求键重发：原样返回，不做第二次。
    let (status, again) = gateway
        .post(
            "/v1/memories/m-1/confirm",
            serde_json::json!({"expected_revision": 1, "request_key": "cli-confirm-1"}),
        )
        .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again, body, "重复请求幂等（§9.6）");

    // forget 同理。
    let (status, body) = gateway
        .post(
            "/v1/memories/m-1/forget",
            serde_json::json!({"expected_revision": 1, "request_key": "cli-forget-1"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memory"]["state"], serde_json::json!("forgotten"));

    let (status, again) = gateway
        .post(
            "/v1/memories/m-1/forget",
            serde_json::json!({"expected_revision": 1, "request_key": "cli-forget-1"}),
        )
        .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again, body);

    // **遗忘不等于删除原文**（§9.6）：正文还读得到，界面要说清遗忘范围。
    let (status, detail) = gateway.get("/v1/memories/m-1").await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(
        detail["memory"]["content"],
        serde_json::json!("客厅空调设 26 度")
    );
    assert_eq!(detail["memory"]["state"], serde_json::json!("forgotten"));

    // 但召回不到了，关键词与向量的引用都失效了（§9.6）。
    let (status, body) = gateway.get("/v1/memories?query=%E7%A9%BA%E8%B0%83").await;
    assert_eq!(status, 200, "{body}");
    assert!(body["memories"].as_array().unwrap().is_empty(), "{body}");
}

/// §14 ⑦ 的后半：**已遗忘的内容不会回到后面的轮次**（§9.7）。
///
/// 第一轮注入了它；`forget` 之后第二轮的 `TurnRequest::memories` 里就没有它了——
/// 沿用上一轮的选择那条快路必须先过一次"重新核对当前状态"。
#[tokio::test]
async fn a_forgotten_memory_never_comes_back_into_a_turn() {
    let embeddings = ConceptEmbeddings::new();
    let llm = ScriptedLlm::new(vec![
        vec![text_round(1, "26 度。")],
        vec![text_round(1, "我这边看不到了。")],
    ]);
    let gateway = GatewayBuilder::new(&memory_config("hybrid", true))
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

    // 第一轮：注入了它。
    let (session, run) = crate::a_run(&gateway, "rk-1", "空调几度来着").await;
    wait_for_terminal(&gateway, &run).await;
    {
        let seen = llm.requests.lock().expect("请求表").clone();
        assert_eq!(
            seen[0].memories.len(),
            1,
            "第一轮注入了：{:?}",
            seen[0].memories
        );
        assert_eq!(seen[0].memories[0].memory, MemoryId::from_raw("m-1"));
    }

    // 操作者遗忘它。
    let (status, body) = gateway
        .post(
            "/v1/memories/m-1/forget",
            serde_json::json!({"expected_revision": 1}),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    // 第二轮：同一个会话，同一个问题。
    let (status, body) = gateway
        .post(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({"request_key": "rk-2", "text": "空调几度来着"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let second: komo_kernel::types::ids::RunId =
        serde_json::from_value(body["run"].clone()).expect("run id");
    wait_for_terminal(&gateway, &second).await;

    let seen = llm.requests.lock().expect("请求表").clone();
    assert_eq!(seen.len(), 2, "跑了两轮");
    assert!(
        seen[1].memories.is_empty(),
        "遗忘的内容不能回到上下文（§9.7）：{:?}",
        seen[1].memories
    );
}

/// 一条**不存在**的记忆：404，而不是一个空对象。
#[tokio::test]
async fn an_unknown_memory_is_a_not_found_not_an_empty_answer() {
    let (gateway, _embeddings) = crate::with_memories("keyword", vec![]).await;
    let (status, body) = gateway.get("/v1/memories/nope").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(
        body["error"]["code"],
        serde_json::json!("not_found"),
        "{body}"
    );
}
