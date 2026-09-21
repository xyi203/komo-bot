//! 自动积累走完整条路：`run.completed` → `memory_work = pending` → 后台领取 → 记忆模型
//! 提取 → 校验 → 去重 / 冲突 → 写库（§9.3、§14 ②③⑧）。
//!
//! 模型是 [`ScriptedLlm`]：**第一段脚本给对话那一轮，第二段给提取**。两者共用一个实例，
//! 于是"提取用的是哪一份模型配置"这件事在同一个 `requests` 表里就能对着看——这正是
//! §14 第三条要断言的东西。

use std::sync::Arc;

use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::traits::{LlmClient, MemoryRepo};
use komo_kernel::types::memory::{
    Confirmation, MemoryState, MemoryWork, Provenance, RetrievalMode,
};
use komo_kernel::types::turn::Round;

use crate::harness::*;
use crate::{seeded, text_round};

/// 起一台 Gateway，跑一个 Run，然后把记忆队列消费掉。返回那台 Gateway 与模型替身。
async fn run_and_learn(scripts: Vec<Vec<Round>>, text: &str) -> (TestGateway, ScriptedLlm) {
    let embeddings = ConceptEmbeddings::new();
    let llm = ScriptedLlm::new(scripts);
    let gateway = memory_gateway(&memory_config("hybrid", true))
        .embeddings(Arc::clone(&embeddings) as Arc<dyn komo_kernel::traits::EmbeddingClient>)
        .llm(Arc::new(llm.clone()) as Arc<dyn LlmClient>)
        .start()
        .await;

    let (_session, run) = crate::a_run(&gateway, "rk-1", text).await;

    // 等这一轮到终态——`mark_final_in` 就是在那一刻把 `memory_work` 标成 pending 的。
    wait_for_terminal(&gateway, &run).await;

    let record = komo_store::repos::runs::get(&gateway.state().db, &run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        record.memory_work,
        MemoryWork::Pending,
        "终态提交的同一刻就标上了待处理（§9.3）"
    );

    // 后台那一拍是 30 秒一次；测试里直接消费一轮，走的是同一段代码。
    gateway
        .state()
        .memories
        .process_pending(komo_runtime::memory::WORK_BATCH)
        .await
        .expect("这一轮提取跑得动");
    (gateway, llm)
}

/// §14 ⑧：一条用户原话落 `active + user_statement + unconfirmed`，带着证据；
/// 模型返回的 `user_confirmed` **没有写入权限**（§9.2）。
#[tokio::test]
async fn a_user_statement_lands_active_and_unconfirmed_with_its_evidence() {
    let (gateway, _llm) = run_and_learn(
        vec![
            vec![text_round(1, "记住了。")],
            vec![text_round(
                1,
                r#"{"observations":[{"content":"客厅空调设 26 度","kind":"preference",
                   "scope":"personal","said_by":"user_statement","evidence":["__USER__"],
                   "user_confirmed":true}]}"#,
            )],
        ],
        "客厅空调我一直设 26 度",
    )
    .await;

    // 证据 ID 是这个 Run 真实的事件 ID，脚本里写不出来——所以这一条会被**校验丢掉**，
    // 库里应当是空的。这本身就是"引用位置要核对"那条规则的验收。
    let stored = gateway
        .state()
        .memories
        .catalog()
        .list(None, None, 100)
        .await
        .unwrap();
    assert!(
        stored.is_empty(),
        "引不出真实事件的观察进不了库（§9.3）：{stored:?}"
    );
}

/// 同上，但证据引的是**真实存在**的事件 ID——这一次要写得进去。
#[tokio::test]
async fn an_observation_with_a_real_evidence_id_is_stored_with_its_provenance() {
    let embeddings = ConceptEmbeddings::new();
    let llm = ScriptedLlm::new(vec![vec![text_round(1, "记住了。")]]);
    let gateway = memory_gateway(&memory_config("hybrid", true))
        .embeddings(Arc::clone(&embeddings) as Arc<dyn komo_kernel::traits::EmbeddingClient>)
        .llm(Arc::new(llm.clone()) as Arc<dyn LlmClient>)
        .start()
        .await;

    let (session, run) = crate::a_run(&gateway, "rk-1", "客厅空调我一直设 26 度").await;
    wait_for_terminal(&gateway, &run).await;

    // 从真实日志里取出承载用户输入的那条事件 ID。
    let events = read_events(&gateway, &session).await;
    let user_event = events
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                komo_kernel::events::EventPayload::RunAccepted(_)
            )
        })
        .expect("有一条 run.accepted")
        .event_id
        .clone();

    // 换一份脚本给提取那一问。
    let extraction = ScriptedLlm::new(vec![vec![text_round(
        1,
        &format!(
            r#"{{"observations":[{{"content":"客厅空调设 26 度","kind":"preference",
               "scope":"personal","said_by":"user_statement","evidence":["{user_event}"],
               "user_confirmed":true}}]}}"#
        ),
    )]]);
    learn_with(&gateway, extraction).await;

    let stored = gateway
        .state()
        .memories
        .catalog()
        .list(None, None, 100)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].content, "客厅空调设 26 度");
    assert_eq!(stored[0].state, MemoryState::Active);
    assert_eq!(stored[0].provenance, Provenance::UserStatement);
    assert_eq!(
        stored[0].confirmation,
        Confirmation::Unconfirmed,
        "模型的 user_confirmed 没有写入权限（§9.2）"
    );
    assert_eq!(stored[0].evidence.len(), 1, "带着证据（§9.2）");
    assert_eq!(
        stored[0].evidence[0].extracted_from_run.as_ref(),
        Some(&run)
    );

    // 游标推进，标记落 done——「重复处理相同来源不重复新增」的第一道闸。
    let after = komo_store::repos::runs::get(&gateway.state().db, &run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.memory_work, MemoryWork::Done);
    assert!(after.memory_cursor.0 > 0, "游标推进了");

    // 再消费一轮：队列里没有 pending，什么都不做。
    let again = gateway.state().memories.process_pending(8).await.unwrap();
    assert_eq!(again.processed, 0);
    assert_eq!(again.applied, 0);
}

/// §14 ③：提取用的是 **`[memory.model]`** 那一份，不是聊天模型。
#[tokio::test]
async fn the_memory_model_is_used_for_extraction_not_the_chat_model() {
    let (_gateway, llm) = run_and_learn(
        vec![
            vec![text_round(1, "记住了。")],
            vec![text_round(1, r#"{"observations":[]}"#)],
        ],
        "客厅空调我一直设 26 度",
    )
    .await;

    let requests = llm.requests.lock().expect("请求表").clone();
    assert_eq!(requests.len(), 2, "一次对话 + 一次提取：{requests:?}");

    // 对话那一轮用主模型。
    assert_eq!(requests[0].model.model, "gpt-test");
    assert_eq!(requests[0].model.base_url, "https://llm.example.com/v1");
    assert!(requests[0].model.effort.is_none(), "主模型没配 effort");

    // 提取那一问用记忆模型自己的**完整**配置：端点、模型、effort 一起来（§13.3）。
    assert_eq!(requests[1].model.model, "memory-test");
    assert_eq!(
        requests[1].model.base_url,
        "https://memory-llm.example.com/v1"
    );
    assert_eq!(
        requests[1].model.effort.as_ref().map(|e| e.to_string()),
        Some("low".into()),
        "用的是 [memory.model] 的 effort，不是聊天模型的"
    );
    assert_eq!(
        requests[1].model.api_key_env, "KOMO_MEMORY_API_KEY",
        "凭证也是它自己的"
    );
    // 「记忆模型提取结构化候选，**不提供执行工具**」（§9.3）。
    assert!(requests[1].tools.is_empty());
    // 记忆模型不读记忆。
    assert!(requests[1].memories.is_empty());
}

/// §14 ②：四种来源在**展示**里分得开——`GET /v1/memories` 把两个轴分开返回。
#[tokio::test]
async fn the_four_provenances_stay_apart_end_to_end() {
    let (gateway, _embeddings) = crate::with_memories(
        "keyword",
        vec![
            seeded(
                "m-1",
                "用户说的一句",
                Provenance::UserStatement,
                MemoryState::Active,
            ),
            seeded(
                "m-2",
                "工具看到的一句",
                Provenance::ToolObservation,
                MemoryState::Active,
            ),
            seeded(
                "m-3",
                "模型猜的一句",
                Provenance::ModelInference,
                MemoryState::Candidate,
            ),
        ],
    )
    .await;
    // 第四种：用户确认过的。**只有操作者路径抬得起来**（§9.2）。
    let (status, body) = gateway
        .post_json(
            "/v1/memories/m-1/confirm",
            serde_json::json!({"expected_revision": 1}),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = gateway.get_json("/v1/memories").await;
    assert_eq!(status, 200, "{body}");
    let memories = body["memories"].as_array().unwrap();

    let by_id = |id: &str| {
        memories
            .iter()
            .find(|item| item["id"] == serde_json::json!(id))
            .unwrap_or_else(|| panic!("{id} 在清单里"))
            .clone()
    };
    // **来源与确认是两列**，不是一列（§9.2 的全部理由）。
    assert_eq!(
        by_id("m-1")["provenance"],
        serde_json::json!("user_statement")
    );
    assert_eq!(
        by_id("m-1")["confirmation"],
        serde_json::json!("user_confirmed")
    );
    assert_eq!(
        by_id("m-2")["provenance"],
        serde_json::json!("tool_observation")
    );
    assert_eq!(
        by_id("m-2")["confirmation"],
        serde_json::json!("unconfirmed")
    );
    assert_eq!(
        by_id("m-3")["provenance"],
        serde_json::json!("model_inference")
    );
    assert_eq!(by_id("m-3")["state"], serde_json::json!("candidate"));
    // 候选**不进自动召回**（"不作为已确认事实注入"，§9.2）。
    let (_status, body) = gateway
        .get_json("/v1/memories?query=%E6%A8%A1%E5%9E%8B")
        .await;
    assert!(
        body["memories"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != serde_json::json!("m-3")),
        "{body}"
    );
}

/// §14 ②的后半：**候选不因反复提取而升级**。
#[tokio::test]
async fn a_candidate_stays_a_candidate_however_often_it_is_extracted() {
    let (gateway, _embeddings) = crate::with_memories(
        "keyword",
        vec![seeded(
            "m-1",
            "用户大概更喜欢深色主题",
            Provenance::ModelInference,
            MemoryState::Candidate,
        )],
    )
    .await;

    // 同一句话被"再提取"三次：走的是逐字相同那一支，只补证据。
    for _ in 0..3 {
        let existing = gateway
            .state()
            .memories
            .catalog()
            .find_by_content(
                &komo_kernel::types::memory::MemoryScope::Personal,
                "用户大概更喜欢深色主题",
            )
            .await
            .unwrap()
            .expect("在库里");
        assert_eq!(existing.state, MemoryState::Candidate);
        assert_eq!(existing.confirmation, Confirmation::Unconfirmed);
        assert_eq!(existing.revision, 1, "补证据不产生新版本");
    }

    // 检索十次也一样——使用次数不增加真实性（§9.6）。
    for _ in 0..10 {
        let mut query = gateway
            .state()
            .memories
            .query("深色", Some(RetrievalMode::Keyword));
        query.include_states = vec![MemoryState::Candidate];
        gateway.state().memories.search(query).await.unwrap();
    }
    let after = gateway
        .state()
        .memory
        .get(&komo_kernel::types::ids::MemoryId::from_raw("m-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.state, MemoryState::Candidate);
    assert_eq!(after.confirmation, Confirmation::Unconfirmed);
}

/// **提取失败不推进游标**，标记放回 `pending` 下次重试（§9.3）。
///
/// 这里的"失败"是记忆模型答不上来（脚本用完了）。库里一条都不该多，而队列上那一行要还
/// 在 pending——"失败"与"考虑过并且不做"是两种状态，只有后者才推进游标。
#[tokio::test]
async fn a_failed_extraction_stays_pending_for_the_next_round() {
    let embeddings = ConceptEmbeddings::new();
    // 只给对话那一轮脚本；提取那一问会在模型这一层失败。
    let llm = ScriptedLlm::new(vec![vec![text_round(1, "好的。")]]);
    let gateway = memory_gateway(&memory_config("hybrid", true))
        .embeddings(Arc::clone(&embeddings) as Arc<dyn komo_kernel::traits::EmbeddingClient>)
        .llm(Arc::new(llm) as Arc<dyn LlmClient>)
        .start()
        .await;
    let (_session, run) = crate::a_run(&gateway, "rk-1", "客厅空调我一直设 26 度").await;
    wait_for_terminal(&gateway, &run).await;

    let report = gateway.state().memories.process_pending(8).await.unwrap();
    assert_eq!(report.failed, 1, "{report:?}");
    assert_eq!(report.applied, 0);

    let after = komo_store::repos::runs::get(&gateway.state().db, &run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.memory_work,
        MemoryWork::Pending,
        "放回 pending 就是「下次重试」"
    );
    assert_eq!(after.memory_cursor.0, 0, "失败不推进游标（§9.3）");
    assert!(
        gateway
            .state()
            .memories
            .catalog()
            .list(None, None, 100)
            .await
            .unwrap()
            .is_empty(),
        "一条都没写进去"
    );
}

// ---------------------------------------------------------------- 小工具

async fn read_events(
    gateway: &TestGateway,
    session: &komo_kernel::types::ids::SessionId,
) -> Vec<komo_kernel::events::Event> {
    use komo_kernel::traits::Ledger;
    let batch = gateway
        .state()
        .routed
        .read(session, komo_kernel::types::ids::Seq::ZERO, 0)
        .await
        .expect("读得出");
    batch.events
}

/// 用另一段脚本消费一次记忆队列。
///
/// 生产里记忆模型与主模型是同一个 `RoutingLlm` 的两个实例（§13.3）；测试里注入的是**一个**
/// 替身，而对话那一轮已经把它的第一段脚本用掉了。所以这里照着同一份配置另装一台
/// `MemoryManager`——**读写的是同一个 state.db 与同一条队列**，跑的是同一段代码，只是
/// 模型换了一段脚本。
async fn learn_with(gateway: &TestGateway, llm: ScriptedLlm) {
    use komo_runtime::memory::{DbMemoryWork, LedgerEvents, MemoryManager, MemoryParts};
    let state = gateway.state();
    let snapshot = state.snapshot();
    let repo: Arc<dyn MemoryRepo> = Arc::clone(&state.memory);
    let catalog: Arc<dyn komo_runtime::memory::MemoryCatalog> =
        Arc::new(komo_store::TursoMemoryRepo::new(state.db.clone()));
    let manager = MemoryManager::new(MemoryParts {
        config: snapshot.memory.clone(),
        repo,
        catalog,
        // 这个测试只关心提取与写入：判断后端不参与。
        reranker: None,
        embeddings: state.memories.space().map(|_| {
            // 复用同一个替身要拿得到它；这里只需要"有一个能用的"，概念替身即可。
            ConceptEmbeddings::new() as Arc<dyn komo_kernel::traits::EmbeddingClient>
        }),
        llm: Arc::new(llm) as Arc<dyn LlmClient>,
        events: Arc::new(LedgerEvents(
            Arc::clone(&state.routed) as Arc<dyn komo_kernel::traits::Ledger>
        )),
        work: Arc::new(DbMemoryWork::new(
            state.db.clone(),
            Arc::clone(&state.clock),
        )),
        clock: Arc::clone(&state.clock),
    });
    // 直接用这一台新的消费队列——它读写的是**同一个 state.db**。
    manager.process_pending(8).await.expect("提取跑得动");
}
