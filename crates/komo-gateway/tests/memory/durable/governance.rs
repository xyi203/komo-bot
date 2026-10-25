//! confirm / forget 与它们对 resume 的影响（§9.6、§9.7、§14 ⑦）。

use std::sync::Arc;

use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::types::memory::{MemoryState, Provenance};

use super::{Scripted, a_run, run_in, wait_for_terminal};
use crate::harness::*;
use crate::{seeded, text_round};

/// §14 ⑦：confirm / forget **带着预期 revision**，重复请求幂等。
#[tokio::test]
async fn confirm_and_forget_are_idempotent_over_http() {
    let (gateway, _embeddings) = super::with_memories(
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
        .post_json(
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
        .post_json(
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
        .post_json(
            "/v1/memories/m-1/confirm",
            serde_json::json!({"expected_revision": 1, "request_key": "cli-confirm-1"}),
        )
        .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again, body, "重复请求幂等（§9.6）");

    // forget 同理。
    let (status, body) = gateway
        .post_json(
            "/v1/memories/m-1/forget",
            serde_json::json!({"expected_revision": 1, "request_key": "cli-forget-1"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memory"]["state"], serde_json::json!("forgotten"));

    let (status, again) = gateway
        .post_json(
            "/v1/memories/m-1/forget",
            serde_json::json!({"expected_revision": 1, "request_key": "cli-forget-1"}),
        )
        .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again, body);

    // **遗忘不等于删除原文**（§9.6）：正文还读得到，界面要说清遗忘范围。
    let (status, detail) = gateway.get_json("/v1/memories/m-1").await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(
        detail["memory"]["content"],
        serde_json::json!("客厅空调设 26 度")
    );
    assert_eq!(detail["memory"]["state"], serde_json::json!("forgotten"));

    // 但召回不到了，关键词与向量的引用都失效了（§9.6）。
    let (status, body) = gateway
        .get_json("/v1/memories?query=%E7%A9%BA%E8%B0%83")
        .await;
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

    // 第一轮：注入了它。
    let (session, run) = a_run(&gateway, "rk-1", "空调几度来着").await;
    wait_for_terminal(&gateway, &run).await;
    {
        // 新 harness 的请求没有 `memories` 字段：看系统提示里的记忆段与 `komo.memory` 里记的 uses。
        let seen = llm.requests.lock().expect("请求表").clone();
        assert!(
            seen[0].system_prompt.contains("客厅空调设 26 度"),
            "第一轮注入了：{}",
            seen[0].system_prompt
        );
        let detail = detail_of(&gateway, &run).await;
        assert_eq!(
            detail["memories"].as_array().map(Vec::len),
            Some(1),
            "{detail}"
        );
        assert_eq!(detail["memories"][0]["memory"], serde_json::json!("m-1"));
    }

    // 操作者遗忘它。
    let (status, body) = gateway
        .post_json(
            "/v1/memories/m-1/forget",
            serde_json::json!({"expected_revision": 1}),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    // 第二轮：同一个会话，同一个问题。
    let second = run_in(&gateway, &session, "rk-2", "空调几度来着").await;
    wait_for_terminal(&gateway, &second).await;

    let seen = llm.requests.lock().expect("请求表").clone();
    assert_eq!(seen.len(), 2, "跑了两轮");
    // 钉住的那一块里有条目被遗忘：作废重召回（没有别的可召回），第二轮的系统提示里不再有它。
    assert!(
        !seen[1].system_prompt.contains("客厅空调设 26 度"),
        "遗忘的内容不能回到上下文（§9.7）：{}",
        seen[1].system_prompt
    );
}

async fn detail_of(
    gateway: &TestGateway,
    run: &komo_kernel::types::ids::RunId,
) -> serde_json::Value {
    let (status, body) = gateway.get_json(&format!("/v1/runs/{run}")).await;
    assert_eq!(status, 200, "{body}");
    body
}

/// 一条**不存在**的记忆：404，而不是一个空对象。
#[tokio::test]
async fn an_unknown_memory_is_a_not_found_not_an_empty_answer() {
    let (gateway, _embeddings) = super::with_memories("keyword", vec![]).await;
    let (status, body) = gateway.get_json("/v1/memories/nope").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(
        body["error"]["code"],
        serde_json::json!("not_found"),
        "{body}"
    );
}

/// sweep 先预演、再应用；已确认的不动；遗忘的能 restore 回来，并且重新召回得到。
#[tokio::test]
async fn sweep_forgets_by_rule_and_restore_brings_it_back() {
    let long_ago = time::OffsetDateTime::now_utc() - time::Duration::days(100);
    let aged = |id: &str, content: &str| {
        let mut item = seeded(
            id,
            content,
            Provenance::ToolObservation,
            MemoryState::Active,
        );
        item.created_at = long_ago;
        item.observed_at = long_ago;
        item
    };
    let mut confirmed = aged("m-confirmed", "书房空调设 25 度");
    confirmed.confirmation = komo_kernel::types::memory::Confirmation::UserConfirmed;
    let mut expired = seeded(
        "m-expired",
        "今天门锁没电",
        Provenance::ToolObservation,
        MemoryState::Active,
    );
    expired.valid_until = Some(long_ago);
    let (gateway, _embeddings) = super::with_memories(
        "keyword",
        vec![
            aged("m-old", "卧室空调设 24 度"),
            seeded(
                "m-new",
                "客厅空调设 26 度",
                Provenance::ToolObservation,
                MemoryState::Active,
            ),
            confirmed,
            expired,
        ],
    )
    .await;
    let sweep = serde_json::json!({"never_used_days": 60, "idle_days": 90});

    // 预演：列出来，但什么都不改。
    let (status, body) = gateway.post_json("/v1/memory-sweep", sweep.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["applied"], serde_json::json!(false));
    let picked: Vec<_> = body["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["memory"].as_str().unwrap(), a["rule"].as_str().unwrap()))
        .collect();
    assert_eq!(
        picked,
        vec![("m-expired", "expired"), ("m-old", "never_used")],
        "{body}"
    );
    let (_, detail) = gateway.get_json("/v1/memories/m-old").await;
    assert_eq!(detail["memory"]["state"], serde_json::json!("active"));

    // 应用。
    let mut apply = sweep.clone();
    apply["apply"] = serde_json::json!(true);
    let (status, body) = gateway.post_json("/v1/memory-sweep", apply.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["actions"].as_array().unwrap().len(), 2, "{body}");
    for (id, state) in [
        ("m-old", "forgotten"),
        ("m-expired", "forgotten"),
        ("m-new", "active"),
        ("m-confirmed", "active"),
    ] {
        let (_, detail) = gateway.get_json(&format!("/v1/memories/{id}")).await;
        assert_eq!(detail["memory"]["state"], serde_json::json!(state), "{id}");
    }
    // 再跑一次：没有可做的了。
    let (_, again) = gateway.post_json("/v1/memory-sweep", apply).await;
    assert!(again["actions"].as_array().unwrap().is_empty(), "{again}");

    // 生效的不能 restore。
    let (status, body) = gateway
        .post_json(
            "/v1/memories/m-new/restore",
            serde_json::json!({"expected_revision": 1}),
        )
        .await;
    assert_eq!(status, 400, "{body}");

    // 遗忘的能回来，关键词索引跟着回来。
    let (status, body) = gateway
        .post_json(
            "/v1/memories/m-old/restore",
            serde_json::json!({"expected_revision": 1}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memory"]["state"], serde_json::json!("active"));
    assert_eq!(body["memory"]["revision"], serde_json::json!(1));
    let (_, found) = gateway
        .get_json("/v1/memories?query=%E5%8D%A7%E5%AE%A4&mode=keyword")
        .await;
    let ids: Vec<_> = found["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["m-old"], "{found}");
}
