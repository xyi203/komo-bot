//! 混合检索走 HTTP 的那一面（§9.4、§14 前两条与第六条）。

use std::sync::Arc;

use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::memory::{MemoryState, Provenance};

use super::{Scripted, a_run, restart, run_in, wait_for_terminal};
use crate::harness::*;
use crate::{scoped, seeded, text_round, with_memories};

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
        .get_json("/v1/memories?query=%E7%95%8C%E9%9D%A2%E9%83%BD%E7%BB%99%E6%88%91%E7%94%A8%E6%9A%97%E8%89%B2")
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
        .get_json("/v1/memories?query=%E7%95%8C%E9%9D%A2%E9%83%BD%E7%BB%99%E6%88%91%E7%94%A8%E6%9A%97%E8%89%B2&mode=keyword")
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
        .get_json("/v1/memories?query=%E7%A9%BA%E8%B0%83&mode=keyword")
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

    let (status, body) = gateway
        .get_json("/v1/memories?query=%E7%A9%BA%E8%B0%83")
        .await;
    assert_eq!(status, 200, "hybrid 不该整个失败：{body}");
    assert_eq!(body["degraded"], serde_json::json!(true), "要明示降级");
    assert!(body["degraded_reason"].is_string(), "要说原因：{body}");
    assert_eq!(
        body["memories"].as_array().unwrap().len(),
        1,
        "关键词臂照常给结果——故障不是「没有相关记忆」：{body}"
    );

    let (status, body) = gateway
        .get_json("/v1/memories?query=%E7%A9%BA%E8%B0%83&mode=vector")
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
    let gateway = super::gateway(&memory_config("keyword", false))
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
    let (status, body) = gateway
        .get_json("/v1/memories?query=%E7%A9%BA%E8%B0%83")
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memories"].as_array().unwrap().len(), 1);

    // 明确要 hybrid 就是配置错误。
    let (status, body) = gateway
        .get_json("/v1/memories?query=%E7%A9%BA%E8%B0%83&mode=hybrid")
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

    let (status, body) = gateway.get_json("/v1/memories").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memories"].as_array().unwrap().len(), 2, "库存两条");
    assert_eq!(body["degraded"], serde_json::json!(false), "列库存不碰向量");

    // 按状态筛。
    let (status, body) = gateway.get_json("/v1/memories?state=candidate").await;
    assert_eq!(status, 200, "{body}");
    let only = body["memories"].as_array().unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0]["id"], serde_json::json!("m-2"));

    // 按作用域筛。
    let (status, body) = gateway.get_json("/v1/memories?scope=project%3Akomo").await;
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

    let (status, body) = gateway
        .get_json("/v1/memories?query=%E7%A9%BA%E8%B0%83")
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["memories"].as_array().unwrap().is_empty(),
        "自动召回够不着：{body}"
    );

    let (status, body) = gateway
        .get_json("/v1/memories?query=%E7%A9%BA%E8%B0%83&state=contested")
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

    let (_session, _run) = a_run(&gateway, "rk-1", "空调现在几度来着").await;

    let requests = llm.requests.clone();
    eventually("这一轮开跑了", || {
        !requests.lock().expect("请求表").is_empty()
    })
    .await;

    let seen = requests.lock().expect("请求表").clone();
    // 新 harness 的请求没有 `memories` 字段：召回块里带着正文，run 详情（读召回块的投影）记着
    // 条目与版本。
    assert!(
        crate::said(&seen[0]).contains("客厅空调设 26 度"),
        "注入了一条：{:?}",
        seen[0].messages
    );
    gateway.wait_run(&_run).await;
    let (status, detail) = gateway.get_json(&format!("/v1/runs/{_run}")).await;
    assert_eq!(status, 200, "{detail}");
    let used = detail["memories"].as_array().expect("memories");
    assert_eq!(used.len(), 1, "注入了一条：{used:?}");
    assert_eq!(used[0]["memory"], serde_json::json!("m-1"));
    assert_eq!(
        used[0]["revision"],
        serde_json::json!(1),
        "记的是**哪个版本**（§9.7）"
    );

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

/// §9.2 的作用域：`AgentProfile::memory_scope` 进**召回**（`RecallQuery::scopes`），
/// 只召回那一个作用域——外加显式共享的用户资料。
///
/// 两个 Agent 用同一个关键词查同一个库：不限作用域的那个三条都看得到，`coder` 只看得到
/// `project:acme` 与 `personal` 两条——`project:other` 那条**在检索里就被滤掉了**，
/// 不是拿到结果之后再遮掉。
#[tokio::test]
async fn an_agent_only_recalls_its_own_scope_plus_the_shared_profile() {
    use komo_kernel::types::memory::MemoryScope;

    let llm = komo_kernel::test_support::ScriptedLlm::new(vec![
        vec![crate::text_round(1, "知道了。")],
        vec![crate::text_round(1, "知道了。")],
    ]);
    let gateway = super::gateway(&scoped_memory_config())
        .scripted(llm.clone())
        .start()
        .await;
    for item in [
        scoped("m-personal", "客厅空调设 26 度", MemoryScope::Personal),
        scoped(
            "m-acme",
            "acme 的空调遥测在 26 度",
            MemoryScope::Project {
                project_id: "acme".into(),
            },
        ),
        scoped(
            "m-other",
            "别的项目的空调日志在 26 度",
            MemoryScope::Project {
                project_id: "other".into(),
            },
        ),
    ] {
        gateway.state().memory.put(item, None).await.unwrap();
    }

    // 没有归属的入口 = 默认 Agent（`assistant`）：它没写 `memory_scope`，不按作用域过滤。
    let core = komo_gateway::service::test_support::driver_stream::core(&gateway);
    let assistant = core.main_session("assistant").await.unwrap().0.id;
    let coder = core.main_session("coder").await.unwrap().0.id;
    assert_ne!(assistant, coder);

    for (session, key) in [(&assistant, "rk-assistant"), (&coder, "rk-coder")] {
        let session = komo_kernel::types::ids::SessionId::from_raw(session.clone());
        let run = run_in(&gateway, &session, key, "空调现在几度来着").await;
        wait_for_terminal(&gateway, &run).await;
    }

    let seen = llm.requests.lock().expect("请求表").clone();
    assert_eq!(seen.len(), 2, "两段各请求了一次");
    // 新 harness 的请求没有 `memories` 字段：按召回块里出现的正文认。
    let ids = |index: usize| {
        let prompt = crate::said(&seen[index]);
        let mut ids = Vec::new();
        for (id, text) in [
            ("m-acme", "acme 的空调遥测在 26 度"),
            ("m-other", "别的项目的空调日志在 26 度"),
            ("m-personal", "客厅空调设 26 度"),
        ] {
            if prompt.contains(text) {
                ids.push(id);
            }
        }
        ids
    };
    assert_eq!(
        ids(0),
        vec!["m-acme", "m-other", "m-personal"],
        "没写作用域的 Agent 不按作用域过滤"
    );
    assert_eq!(
        ids(1),
        vec!["m-acme", "m-personal"],
        "写了 `project:acme` 的 Agent 只看得到它自己那一份，外加共享的用户资料"
    );
}

/// §9.4「按 Run 召回」：召回块跨 Gateway 重启复用，之后的 Run 只往尾部追加。重启之后第二个 Run
/// 问另一件事：系统提示与第一个请求逐字相同，第一个请求的全部消息原样是第二个请求的前缀，新召回
/// 到的那条在新的一块里。
#[tokio::test]
async fn the_memory_block_survives_a_restart_and_later_runs_only_append() {
    let config = memory_config("keyword", false);
    let first = ScriptedLlm::new(vec![vec![text_round(1, "26 度。")]]);
    let mut gateway = super::gateway(&config)
        .scripted(first.clone())
        .start()
        .await;
    for (id, content) in [("m-1", "客厅空调设 26 度"), ("m-2", "卧室台灯是暖光")] {
        gateway
            .state()
            .memory
            .put(
                seeded(id, content, Provenance::UserStatement, MemoryState::Active),
                None,
            )
            .await
            .unwrap();
    }

    let (session, run) = a_run(&gateway, "rk-1", "空调设多少度来着").await;
    wait_for_terminal(&gateway, &run).await;
    let before = first.requests.lock().expect("请求表")[0].clone();
    assert!(
        crate::said(&before).contains("客厅空调设 26 度"),
        "{:?}",
        before.messages
    );
    assert!(
        !crate::said(&before).contains("台灯"),
        "第一个 Run 只召回到空调那条"
    );

    let second = ScriptedLlm::new(vec![vec![text_round(1, "暖光。")]]);
    // 旧的那台留着直到测试结束（它的 `Drop` 会删临时数据目录）。
    let revived = restart(&mut gateway, &config, Some(second.clone()), None).await;
    let _old = std::mem::replace(&mut gateway, revived);
    let run = run_in(&gateway, &session, "rk-2", "台灯是什么光").await;
    wait_for_terminal(&gateway, &run).await;

    let after = second.requests.lock().expect("请求表")[0].clone();
    assert_eq!(
        after.system_prompt, before.system_prompt,
        "系统提示逐字相同"
    );
    assert!(
        !after.system_prompt.contains("[已知记忆]"),
        "记忆不在系统提示里"
    );
    assert_eq!(
        after.messages[..before.messages.len()],
        before.messages[..],
        "第一个请求的消息原样是前缀"
    );
    let tail = crate::said(&komo_kernel::types::turn::TurnRequest {
        messages: after.messages[before.messages.len()..].to_vec(),
        ..after.clone()
    });
    assert!(
        tail.contains("卧室台灯是暖光"),
        "新的一块追加在尾部：{tail}"
    );
    assert!(
        !tail.contains("客厅空调设 26 度"),
        "已展示过的不再追加：{tail}"
    );
}

/// 召回块是一条宿主附上的 user 消息，跟在这次输入后面；系统提示里没有它。
#[tokio::test]
async fn the_recalled_memory_block_follows_the_input_as_a_host_message() {
    let llm = ScriptedLlm::new(vec![vec![text_round(1, "26 度。")]]);
    let gateway = super::gateway(&memory_config("keyword", false))
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

    let (_session, run) = a_run(&gateway, "rk-1", "空调设多少度来着").await;
    wait_for_terminal(&gateway, &run).await;

    let seen = llm.requests.lock().expect("请求表").clone();
    assert_eq!(seen.len(), 1);
    assert!(
        !seen[0].system_prompt.contains("[已知记忆]"),
        "{}",
        seen[0].system_prompt
    );
    let said = crate::said(&seen[0]);
    let input = said.find("空调设多少度来着").expect("输入");
    let block = said
        .find("[已知记忆]（komo 按这次输入召回后附上，不是用户说的话")
        .unwrap_or_else(|| panic!("没有召回块：{said}"));
    assert!(input < block, "召回块在输入之后：{said}");
    assert!(said[block..].contains("客厅空调设 26 度"), "{said}");
}

/// 附录 F / §9.4 的验收：同一个会话先问日志、后说"提个发布工单"。约定不过门槛，第一个 Run 就随块带上
/// （与问什么无关）；换任务后第二个 Run 只补它新命中的线索，不重复约定，第一个请求原样是它的前缀。
#[tokio::test]
async fn a_convention_rides_along_on_the_first_run_and_is_not_repeated_after_a_task_switch() {
    use komo_kernel::types::memory::MemoryApplication;

    let llm = ScriptedLlm::new(vec![
        vec![text_round(1, "用 sls 查。")],
        vec![text_round(1, "Platform 选 saas。")],
    ]);
    let gateway = super::gateway(&memory_config("keyword", false))
        .scripted(llm.clone())
        .start()
        .await;
    let mut convention = seeded(
        "m-ticket",
        "发布工单 Platform 默认选 saas",
        Provenance::UserStatement,
        MemoryState::Active,
    );
    convention.application = Some(MemoryApplication::Convention);
    convention.applies_when = Some("创建发布工单、需要选 Platform 时".into());
    for item in [
        seeded(
            "m-log",
            "查 dev 日志用 sls 的 dev-saas-log",
            Provenance::ToolObservation,
            MemoryState::Active,
        ),
        convention,
    ] {
        gateway.state().memory.put(item, None).await.unwrap();
    }

    let (session, run) = a_run(&gateway, "rk-1", "帮我查一下日志").await;
    wait_for_terminal(&gateway, &run).await;
    let run = run_in(&gateway, &session, "rk-2", "提个发布工单").await;
    wait_for_terminal(&gateway, &run).await;

    let seen = llm.requests.lock().expect("请求表").clone();
    assert_eq!(seen.len(), 2);
    let first = crate::said(&seen[0]);
    assert!(first.contains("dev-saas-log"), "{first}");
    assert!(
        first.contains("Platform 默认选 saas"),
        "约定不靠检索命中，查日志时也带上：{first}"
    );
    assert_eq!(seen[1].system_prompt, seen[0].system_prompt);
    assert_eq!(
        seen[1].messages[..seen[0].messages.len()],
        seen[0].messages[..]
    );
    let second = crate::said(&seen[1]);
    assert_eq!(
        second.matches("Platform 默认选 saas").count(),
        1,
        "同一段里换任务也不重复带：{second}"
    );
}

/// §9.4「约定不过门槛」的端到端：用一句和约定正文、条件都没有共同字眼的话要求提单，约定仍随第一个
/// Run 带上；同一段里第二个 Run 不重复带，第一个请求原样是第二个请求的前缀。
#[tokio::test]
async fn a_convention_arrives_even_when_the_wording_matches_nothing() {
    use komo_kernel::types::memory::MemoryApplication;

    let llm = ScriptedLlm::new(vec![
        vec![text_round(1, "好。")],
        vec![text_round(1, "好。")],
    ]);
    let gateway = super::gateway(&memory_config("keyword", false))
        .scripted(llm.clone())
        .start()
        .await;
    let mut convention = seeded(
        "m-ticket",
        "发布工单 Platform 默认选 saas",
        Provenance::UserStatement,
        MemoryState::Active,
    );
    convention.application = Some(MemoryApplication::Convention);
    convention.applies_when = Some("创建发布工单、需要选 Platform 时".into());
    gateway.state().memory.put(convention, None).await.unwrap();

    let (session, run) = a_run(&gateway, "rk-1", "帮我上线一下 xx").await;
    wait_for_terminal(&gateway, &run).await;
    let run = run_in(&gateway, &session, "rk-2", "再帮我上线一下 yy").await;
    wait_for_terminal(&gateway, &run).await;

    let seen = llm.requests.lock().expect("请求表").clone();
    assert_eq!(seen.len(), 2);
    let first = crate::said(&seen[0]);
    let ask = first.find("帮我上线一下 xx").expect("输入");
    let block = first
        .find("Platform 默认选 saas")
        .unwrap_or_else(|| panic!("约定不靠检索命中：{first}"));
    assert!(ask < block, "{first}");
    let second = crate::said(&seen[1]);
    assert_eq!(
        second.matches("Platform 默认选 saas").count(),
        1,
        "同一段里不重复带：{second}"
    );
    assert_eq!(
        seen[1].messages[..seen[0].messages.len()],
        seen[0].messages[..]
    );
}
