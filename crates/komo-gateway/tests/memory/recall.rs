//! 混合检索走 HTTP 的那一面（§9.4、§14 前两条与第六条）。

use std::sync::Arc;

use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::memory::{MemoryState, Provenance};

use crate::harness::*;
use crate::{seeded, with_memories};

/// §14 ①：同一偏好换一种中文表述仍可语义召回，**同时保留原始来源**。
#[tokio::test]
async fn a_paraphrase_finds_it_over_http() {
    let (gateway, _embeddings) = with_memories(
        "hybrid",
        vec![seeded(
            "m-1",
            "喜欢深色主题",
            Provenance::UserStatement,
            MemoryState::Active,
        )],
    )
    .await;

    // 一个字都不重合的中文查询。
    let (status, body) = gateway
        .get("/v1/memories?query=%E7%95%8C%E9%9D%A2%E9%83%BD%E7%BB%99%E6%88%91%E7%94%A8%E6%9A%97%E8%89%B2")
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["degraded"], serde_json::json!(false));
    let memories = body["memories"].as_array().expect("数组");
    assert_eq!(memories.len(), 1, "换一种说法仍然召回得到：{body}");
    assert_eq!(memories[0]["id"], serde_json::json!("m-1"));
    // **保留原始来源**——召回不改写它是谁说的，也不改正文。
    assert_eq!(
        memories[0]["provenance"],
        serde_json::json!("user_statement")
    );
    assert_eq!(memories[0]["content"], serde_json::json!("喜欢深色主题"));

    // 同一个查询在纯关键词下够不着：这正是向量臂存在的理由。
    let (status, body) = gateway
        .get("/v1/memories?query=%E7%95%8C%E9%9D%A2%E9%83%BD%E7%BB%99%E6%88%91%E7%94%A8%E6%9A%97%E8%89%B2&mode=keyword")
        .await;
    assert_eq!(status, 200);
    assert!(body["memories"].as_array().unwrap().is_empty(), "{body}");
}

/// §14 ①的后半：中文两字查询**直接命中 bigram**，不需要子串后备（§9.4）。
#[tokio::test]
async fn a_two_character_chinese_query_hits_the_bigram_arm() {
    let (gateway, _embeddings) = with_memories(
        "hybrid",
        vec![
            seeded(
                "m-1",
                "客厅空调设 26 度",
                Provenance::UserStatement,
                MemoryState::Active,
            ),
            seeded(
                "m-2",
                "用 cargo test 跑测试",
                Provenance::ToolObservation,
                MemoryState::Active,
            ),
        ],
    )
    .await;

    // "空调" = 一个 bigram。
    let (status, body) = gateway
        .get("/v1/memories?query=%E7%A9%BA%E8%B0%83&mode=keyword")
        .await;
    assert_eq!(status, 200, "{body}");
    let memories = body["memories"].as_array().expect("数组");
    assert_eq!(memories.len(), 1, "{body}");
    assert_eq!(memories[0]["id"], serde_json::json!("m-1"));
}

/// §14 ⑥：向量端点不可用时 **hybrid 明示降级、vector-only 明确报不可用**；
/// 两者都不能被解释成"没有相关记忆"。
#[tokio::test]
async fn a_dead_endpoint_degrades_hybrid_and_fails_vector_only() {
    let (gateway, embeddings) = with_memories(
        "hybrid",
        vec![seeded(
            "m-1",
            "客厅空调设 26 度",
            Provenance::UserStatement,
            MemoryState::Active,
        )],
    )
    .await;
    embeddings.set_down(true);

    let (status, body) = gateway.get("/v1/memories?query=%E7%A9%BA%E8%B0%83").await;
    assert_eq!(status, 200, "hybrid 不该整个失败：{body}");
    assert_eq!(body["degraded"], serde_json::json!(true), "要明示降级");
    assert!(body["degraded_reason"].is_string(), "要说原因：{body}");
    assert_eq!(
        body["memories"].as_array().unwrap().len(),
        1,
        "关键词臂照常给结果——故障不是「没有相关记忆」：{body}"
    );

    let (status, body) = gateway
        .get("/v1/memories?query=%E7%A9%BA%E8%B0%83&mode=vector")
        .await;
    assert_ne!(status, 200, "vector-only 要明确报不可用，不是 200 + 空集");
    assert_eq!(
        body["error"]["code"],
        serde_json::json!("vector_unavailable"),
        "{body}"
    );
}

/// 没有配置 `[memory.embedding]` 却问 hybrid：**配置错误**，不能静默变成长期关键词模式
/// （§9.4）。
#[tokio::test]
async fn hybrid_without_an_embedding_section_is_reported_as_a_configuration_error() {
    let gateway = GatewayBuilder::new(&memory_config("keyword", false))
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

    // 配置里写的是 keyword，所以默认那一问照常。
    let (status, body) = gateway.get("/v1/memories?query=%E7%A9%BA%E8%B0%83").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memories"].as_array().unwrap().len(), 1);

    // 明确要 hybrid 就是配置错误。
    let (status, body) = gateway
        .get("/v1/memories?query=%E7%A9%BA%E8%B0%83&mode=hybrid")
        .await;
    assert_ne!(status, 200, "{body}");
    assert_eq!(
        body["error"]["code"],
        serde_json::json!("config_invalid"),
        "{body}"
    );
}

/// 不带查询词 = **列库存**，带查询词 = 检索。两种问法答法不同（§9.4：没有足够相关内容
/// 时返回空——而 `komo memory list` 要的显然不是空）。
#[tokio::test]
async fn listing_without_a_query_is_inventory_not_an_empty_search() {
    let (gateway, _embeddings) = with_memories(
        "hybrid",
        vec![
            seeded(
                "m-1",
                "客厅空调设 26 度",
                Provenance::UserStatement,
                MemoryState::Active,
            ),
            seeded(
                "m-2",
                "也许更喜欢浅色",
                Provenance::ModelInference,
                MemoryState::Candidate,
            ),
        ],
    )
    .await;

    let (status, body) = gateway.get("/v1/memories").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memories"].as_array().unwrap().len(), 2, "库存两条");
    assert_eq!(body["degraded"], serde_json::json!(false), "列库存不碰向量");

    // 按状态筛。
    let (status, body) = gateway.get("/v1/memories?state=candidate").await;
    assert_eq!(status, 200, "{body}");
    let only = body["memories"].as_array().unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0]["id"], serde_json::json!("m-2"));

    // 按作用域筛。
    let (status, body) = gateway.get("/v1/memories?scope=project%3Akomo").await;
    assert_eq!(status, 200, "{body}");
    assert!(body["memories"].as_array().unwrap().is_empty(), "{body}");
}

/// `contested` 暂停自动召回，但**明确写出状态**时查得到（§9.6）。
#[tokio::test]
async fn a_contested_memory_is_out_of_recall_but_findable_on_purpose() {
    let (gateway, _embeddings) = with_memories(
        "keyword",
        vec![seeded(
            "m-1",
            "客厅空调设 26 度",
            Provenance::UserStatement,
            MemoryState::Contested,
        )],
    )
    .await;

    let (status, body) = gateway.get("/v1/memories?query=%E7%A9%BA%E8%B0%83").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["memories"].as_array().unwrap().is_empty(),
        "自动召回够不着：{body}"
    );

    let (status, body) = gateway
        .get("/v1/memories?query=%E7%A9%BA%E8%B0%83&state=contested")
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["memories"].as_array().unwrap().len(),
        1,
        "明确 search 要查得到，不然没人帮得上把冲突定下来：{body}"
    );
}

/// 一次 Run 用到了哪些记忆，写进 `TurnRequest::memories`——**审计证据**（§9.7）。
#[tokio::test]
async fn a_turn_records_which_memories_reached_it() {
    let embeddings = ConceptEmbeddings::new();
    let llm = komo_kernel::test_support::ScriptedLlm::new(vec![vec![crate::text_round(
        1,
        "知道了，26 度。",
    )]]);
    let gateway = GatewayBuilder::new(&memory_config("hybrid", true))
        .embeddings(Arc::clone(&embeddings) as Arc<dyn komo_kernel::traits::EmbeddingClient>)
        .llm(Arc::new(llm.clone()) as Arc<dyn komo_kernel::traits::LlmClient>)
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

    let (_session, _run) = crate::a_run(&gateway, "rk-1", "空调现在几度来着").await;

    let requests = llm.requests.clone();
    eventually("这一轮开跑了", || {
        !requests.lock().expect("请求表").is_empty()
    })
    .await;

    let seen = requests.lock().expect("请求表").clone();
    let used = &seen[0].memories;
    assert_eq!(used.len(), 1, "注入了一条：{used:?}");
    assert_eq!(used[0].memory, MemoryId::from_raw("m-1"));
    assert_eq!(used[0].revision, 1, "记的是**哪个版本**（§9.7）");

    // 使用计数记上了，但真实性一点没变（§9.2）。
    let after = gateway
        .state()
        .memory
        .get(&MemoryId::from_raw("m-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.usage.count, 1);
    assert_eq!(
        after.confirmation,
        komo_kernel::types::memory::Confirmation::Unconfirmed
    );
}
