//! 向量索引与代次走 HTTP 的那一面（§9.5、§14 ⑤）。

use std::sync::Arc;

use komo_kernel::traits::EmbeddingClient;
use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::memory::{MemoryState, Provenance, RetrievalMode};

use crate::harness::*;
use crate::seeded;

/// `POST /v1/memory-index/rebuild` **真的起一个构建**，进度由 `GET /v1/memory-index` 查。
#[tokio::test]
async fn a_rebuild_really_runs_and_its_progress_is_queryable() {
    let embeddings = ConceptEmbeddings::new();
    let gateway = GatewayBuilder::new(&memory_config("hybrid", true))
        .embeddings(Arc::clone(&embeddings) as Arc<dyn EmbeddingClient>)
        .start()
        .await;
    for index in 0..5 {
        gateway
            .state()
            .memory
            .put(
                seeded(
                    &format!("m-{index}"),
                    &format!("客厅空调设 {} 度", 20 + index),
                    Provenance::UserStatement,
                    MemoryState::Active,
                ),
                None,
            )
            .await
            .unwrap();
    }

    // 还没建：有 embedding 配置，但一个代次都没有——那是"还没建"，不是"没配置"。
    let (status, body) = gateway.get("/v1/memory-index").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], serde_json::json!("building"), "{body}");
    assert_eq!(body["indexed"], serde_json::json!(0));
    assert_eq!(body["total"], serde_json::json!(5));
    assert!(
        body["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("rebuild")),
        "要说清下一步做什么：{body}"
    );

    let (status, body) = gateway
        .post("/v1/memory-index/rebuild", serde_json::json!({}))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], serde_json::json!(true), "{body}");
    let generation = body["generation"].as_str().expect("代次").to_string();
    assert!(generation.starts_with("gen-"), "{generation}");

    // 后台任务跑完之后覆盖率到 1，代次生效。
    let state = Arc::clone(gateway.state());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let status = state.memories.index_status().await.unwrap();
        if status.coverage >= 1.0 && status.indexed == 5 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等了 10 秒还没建完：{status:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let (status, body) = gateway.get("/v1/memory-index").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], serde_json::json!("ready"), "{body}");
    assert_eq!(body["indexed"], serde_json::json!(5));
    assert_eq!(body["coverage"], serde_json::json!(1.0));
    assert_eq!(body["generation"], serde_json::json!(generation));
    // 空间指纹报出来了，**凭证不在里面**（§9.5）。
    let space = &body["space"];
    assert_eq!(space["model"], serde_json::json!("concept-v1"));
    assert_eq!(space["dimensions"], serde_json::json!(4));
    assert!(
        !body.to_string().contains("test-embedding-key"),
        "凭证值不进指纹，也不进这个响应"
    );
    assert!(
        body["errors"].as_array().unwrap().is_empty(),
        "指纹对得上就没有漂移提示：{body}"
    );
}

/// 没有配置向量模型时 `GET /v1/memory-index` 是一个**状态**，重建则是配置错误。
#[tokio::test]
async fn an_index_without_an_embedding_model_reads_as_unconfigured() {
    let gateway = GatewayBuilder::new(&memory_config("keyword", false))
        .start()
        .await;

    let (status, body) = gateway.get("/v1/memory-index").await;
    assert_eq!(status, 200, "没配置是一个状态，不是一次失败：{body}");
    assert_eq!(body["state"], serde_json::json!("unconfigured"));

    let (status, body) = gateway
        .post("/v1/memory-index/rebuild", serde_json::json!({}))
        .await;
    assert_ne!(status, 200, "{body}");
    assert_eq!(
        body["error"]["code"],
        serde_json::json!("config_invalid"),
        "{body}"
    );
}

/// §14 ⑤：**修改记忆时旧 revision 不召回**——旧向量连 join 都 join 不上。
#[tokio::test]
async fn editing_a_memory_retires_its_vector() {
    let (gateway, _embeddings) = crate::with_memories(
        "hybrid",
        vec![seeded(
            "m-1",
            "喜欢深色主题",
            Provenance::UserStatement,
            MemoryState::Active,
        )],
    )
    .await;

    // 换一种说法查得到。
    let found = gateway
        .state()
        .memories
        .search(gateway.state().memories.query("界面用暗色", None))
        .await
        .unwrap();
    assert_eq!(found.items.len(), 1, "{found:?}");

    // 改正文 → revision 2。旧向量还在表里，但它说的是上一版的意思。
    let mut edited = gateway
        .state()
        .memory
        .get(&MemoryId::from_raw("m-1"))
        .await
        .unwrap()
        .unwrap();
    edited.revision = 2;
    edited.content = "改成浅色主题".into();
    gateway.state().memory.put(edited, Some(1)).await.unwrap();

    let after = gateway
        .state()
        .memories
        .search(
            gateway
                .state()
                .memories
                .query("界面用暗色", Some(RetrievalMode::Vector)),
        )
        .await;
    match after {
        Ok(result) => assert!(
            result.items.is_empty(),
            "旧 revision 的向量不能再召回它：{result:?}"
        ),
        Err(error) => panic!("向量臂应当照常可用，只是没有命中：{error}"),
    }
    // 覆盖率如实掉下去——这一条现在欠着向量。
    let status = gateway.state().memories.index_status().await.unwrap();
    assert_eq!(status.indexed, 0, "{status:?}");
}

/// §14 ⑤：**重建中崩溃可以接续**——停机再起来，剩下的接着算，代次不变。
#[tokio::test]
async fn a_rebuild_survives_a_restart() {
    let embeddings = ConceptEmbeddings::new();
    let mut gateway = GatewayBuilder::new(&memory_config("hybrid", true))
        .embeddings(Arc::clone(&embeddings) as Arc<dyn EmbeddingClient>)
        .start()
        .await;
    for index in 0..6 {
        gateway
            .state()
            .memory
            .put(
                seeded(
                    &format!("m-{index}"),
                    &format!("客厅空调设 {} 度", 20 + index),
                    Provenance::UserStatement,
                    MemoryState::Active,
                ),
                None,
            )
            .await
            .unwrap();
    }

    // 第一轮：端点关着，一条向量都算不出来——代次留在 building，**记忆正文一条不少**。
    embeddings.set_down(true);
    let stalled = gateway.state().memories.build_index().await.unwrap();
    let generation = stalled.generation.clone();
    assert_eq!(stalled.indexed, 0);
    assert!(!stalled.activated, "没追平就不切换（§9.5 第 4 步）");
    assert!(!stalled.errors.is_empty(), "失败要说出来，不伪装成已完成");
    assert_eq!(
        gateway
            .state()
            .memories
            .catalog()
            .list(None, None, 100)
            .await
            .unwrap()
            .len(),
        6,
        "失败不删除记忆正文（§9.5）"
    );

    // 重启：同一个数据目录、同一个空间。
    let revived = ConceptEmbeddings::new();
    gateway
        .restart(
            &memory_config("hybrid", true),
            Arc::clone(&revived) as Arc<dyn EmbeddingClient>,
        )
        .await;

    let resumed = gateway.state().memories.build_index().await.unwrap();
    assert_eq!(
        resumed.generation, generation,
        "同一个空间就是同一个代次——重建天然接得上"
    );
    assert_eq!(resumed.indexed, 6);
    assert!(resumed.activated);
    assert_eq!(
        gateway
            .state()
            .memory
            .index_status()
            .await
            .unwrap()
            .coverage,
        1.0
    );
}

/// 同维度但**不同代次**的向量不混用（§9.5「同维度不代表同一空间」）。
#[tokio::test]
async fn an_old_generation_is_never_mixed_in() {
    let (gateway, _embeddings) = crate::with_memories(
        "hybrid",
        vec![seeded(
            "m-1",
            "喜欢深色主题",
            Provenance::UserStatement,
            MemoryState::Active,
        )],
    )
    .await;
    let mine = gateway.state().memories.index_status().await.unwrap();
    let generation = mine.generation.clone().expect("有代次");

    // 另一个空间抢了生效位（"换了模型"）。同样是 4 维。
    let repo = komo_store::TursoMemoryRepo::new(gateway.state().db.clone());
    let mut other = gateway.state().memories.space().expect("有空间");
    other.model = "someone-else".into();
    let other_generation = komo_runtime::memory::IndexBuilder::generation_id(&other);
    repo.put_generation(
        &other_generation,
        Some(&other),
        komo_kernel::protocol::http::IndexState::Ready,
        false,
    )
    .await
    .unwrap();
    repo.activate_generation(&other_generation).await.unwrap();
    assert_ne!(other_generation, generation);

    // 向量臂在另一代里找不到东西——**不拿旧向量去比**，而不是"排得靠后"。
    let result = gateway
        .state()
        .memories
        .search(
            gateway
                .state()
                .memories
                .query("界面用暗色", Some(RetrievalMode::Vector)),
        )
        .await
        .unwrap();
    assert!(result.items.is_empty(), "{result:?}");

    // 而且这件事要被**说出来**：配置的空间与生效代次对不上，需要一次重建（§9.5、§13.3）。
    let status = gateway.state().memories.index_status().await.unwrap();
    assert!(
        status.errors.iter().any(|note| note.contains("rebuild")),
        "{status:?}"
    );
}
