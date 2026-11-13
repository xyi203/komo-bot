use std::path::Path;
use std::sync::Arc;

use komo_kernel::durable::{
    AssistantMessage, Content, ContentBlock, Message, StopReason, SystemMessage, ToolResultMessage,
    UserMessage,
};
use komo_kernel::types::turn::{Role, RoundInput};
use komo_llm::{ModelRequest, ModelStream};

use crate::service::state::{codemode_unreadable_paths, protected_paths};
use crate::service::test_support::harness::{FakeLlm, GatewayBuilder, call_round, text_round};
use crate::service::test_support::{TestGateway, config_toml, sample_snapshot};

fn files_starting_with(home: &Path, prefix: &str) -> Vec<String> {
    std::fs::read_dir(home)
        .expect("数据目录")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(prefix))
        .collect()
}

#[tokio::test]
async fn opens_and_closes_durable_db_beside_aux_db() {
    let config = config_toml("");
    let mut first = GatewayBuilder::new(&config)
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;
    assert!(first.home.join("durable.db").exists(), "durable.db 打开了");
    assert!(first.home.join("aux.db").exists(), "aux.db 打开了");
    let (status, body) = first.get("/healthz").await;
    assert_eq!(status, 200, "{body}");

    // 关掉之后同一个数据目录能再起来：Turso 对库文件持进程独占锁，没关干净这里就起不来。
    first.stop().await;
    let mut second = GatewayBuilder::new(&config)
        .fake_llm(FakeLlm::finisher("好"))
        .at(&first.home)
        .start()
        .await;
    second.stop().await;
}

#[tokio::test]
async fn durable_mode_never_opens_state_db() {
    let mut gateway = GatewayBuilder::new(&config_toml(""))
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;
    assert_eq!(
        files_starting_with(&gateway.home, "state.db"),
        Vec::<String>::new(),
        "Durable 不碰 state.db"
    );
    gateway.stop().await;
    assert_eq!(
        files_starting_with(&gateway.home, "state.db"),
        Vec::<String>::new()
    );
}

/// 路由：共用的（models）在，会话路由是 v2 的。
#[tokio::test]
async fn durable_mode_serves_shared_routes_and_leaves_session_routes_to_v2() {
    let gateway = TestGateway::with(&config_toml(""), FakeLlm::finisher("好")).await;
    let (status, body) = gateway.get("/v1/models").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = gateway.get("/v1/sessions").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = gateway.get("/v1/interventions").await;
    assert_eq!(status, 200, "v2 的 interventions 路由（W4a）：{body}");
}

async fn get_json(gateway: &TestGateway, path: &str) -> (u16, serde_json::Value) {
    let (status, body) = gateway.get(path).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body)),
    )
}

async fn post_json(
    gateway: &TestGateway,
    path: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let (status, body) = gateway.post(path, body).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body)),
    )
}

/// 读完 `/v1/sessions` 的全部页（`query` 不含 `limit` / `cursor`），返回会话 ID 与每页条数。
async fn session_pages(gateway: &TestGateway, query: &str, limit: u32) -> (Vec<String>, usize) {
    let mut ids = Vec::new();
    let mut pages = 0;
    let mut cursor: Option<String> = None;
    loop {
        let mut path = format!("/v1/sessions?limit={limit}{query}");
        if let Some(cursor) = &cursor {
            path.push_str(&format!("&cursor={cursor}"));
        }
        let (status, body) = get_json(gateway, &path).await;
        assert_eq!(status, 200, "{body}");
        let page = body["sessions"].as_array().expect("sessions");
        assert!(page.len() <= limit as usize, "{body}");
        pages += 1;
        ids.extend(
            page.iter()
                .map(|s| s["session"].as_str().unwrap().to_owned()),
        );
        match body.get("next_cursor").and_then(|c| c.as_str()) {
            Some(next) => cursor = Some(next.to_owned()),
            None => return (ids, pages),
        }
    }
}

fn session_ids(body: &serde_json::Value) -> Vec<String> {
    body["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|s| s["session"].as_str().unwrap().to_owned())
        .collect()
}

/// 会话列表分页只做加法（`docs/session-optimization.md` E）：不带 `limit` 与之前一样给全部、没有
/// `next_cursor`；带了就按页给，各页拼起来等于全量（同样的次序、不丢不重）；翻页期间新建的会话
/// 不会挤进后面的页；坏参数 400。
#[tokio::test]
async fn session_list_pages_add_up_to_the_unpaged_list() {
    let gateway = TestGateway::with(&config_toml(""), FakeLlm::finisher("好")).await;
    let mut created = Vec::new();
    for _ in 0..7 {
        let (status, body) = post_json(&gateway, "/v1/sessions", serde_json::json!({})).await;
        assert_eq!(status, 200, "{body}");
        created.push(body["session"].as_str().unwrap().to_owned());
    }
    let (status, body) = post_json(
        &gateway,
        &format!("/v1/sessions/{}/delete", created[2]),
        serde_json::json!({ "now": true }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // 老客户端：不带 limit，全部，字段只有 `sessions`。
    let (status, unpaged) = get_json(&gateway, "/v1/sessions").await;
    assert_eq!(status, 200, "{unpaged}");
    assert_eq!(
        unpaged.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["sessions"],
        "不带 limit 的响应形状不变：{unpaged}"
    );
    let everything = session_ids(&unpaged);
    let mut newest_first = created.clone();
    newest_first.remove(2);
    newest_first.reverse();
    assert_eq!(everything, newest_first, "最新的在前，逻辑删除的默认不列");
    // `cursor` 不带 `limit` 时被忽略：还是全部。
    let (_, ignored) = get_json(&gateway, "/v1/sessions?cursor=zz").await;
    assert_eq!(session_ids(&ignored), everything);

    for limit in [1, 2, 3, 6, 7, 50] {
        let (ids, pages) = session_pages(&gateway, "", limit).await;
        assert_eq!(ids, everything, "limit {limit}");
        assert_eq!(pages, everything.len().div_ceil(limit as usize).max(1));
    }
    let (_, all) = get_json(&gateway, "/v1/sessions?all=1").await;
    assert_eq!(
        session_pages(&gateway, "&all=1", 2).await.0,
        session_ids(&all)
    );
    assert!(session_ids(&all).contains(&created[2]));

    // 翻页期间新建：第一页之后的页不重复、不漏掉原来的。
    let (_, first) = get_json(&gateway, "/v1/sessions?limit=2").await;
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let (_, body) = post_json(&gateway, "/v1/sessions", serde_json::json!({})).await;
    let fresh = body["session"].as_str().unwrap().to_owned();
    let (_, rest) = get_json(&gateway, &format!("/v1/sessions?limit=50&cursor={cursor}")).await;
    let mut stitched = session_ids(&first);
    stitched.extend(session_ids(&rest));
    assert_eq!(stitched, everything);
    assert!(!stitched.contains(&fresh));
    assert!(rest.get("next_cursor").is_none(), "{rest}");

    for bad in [
        "/v1/sessions?limit=0",
        "/v1/sessions?limit=2&cursor=zz",
        "/v1/sessions?limit=2&cursor=abc",
        "/v1/sessions?limit=many",
    ] {
        let (status, body) = gateway.get(bad).await;
        assert_eq!(status, 400, "{bad}: {body}");
    }
}

#[test]
fn write_refuses_the_durable_databases() {
    use komo_tools::tools::protect::Protected;
    let dir = tempfile::tempdir().unwrap();
    // `Protected::denies` 接收真实路径；macOS 的 /var 指向 /private/var。
    let canonical_home = dir.path().canonicalize().unwrap();
    let home = canonical_home.as_path();
    let snapshot = sample_snapshot();
    let protected = Protected::new(protected_paths(&snapshot, home));
    for name in [
        "durable.db",
        "durable.db-wal",
        "durable.db-log",
        "aux.db",
        "aux.db-shm",
        "aux.db-wal",
    ] {
        assert!(protected.denies(&home.join(name)), "{name} 受保护");
    }
    assert!(!protected.denies(&home.join("notes.txt")));

    // 脚本里连读也不行（`sessions/` 以外同一份名单）。
    let unreadable = codemode_unreadable_paths(&snapshot, home);
    assert!(unreadable.contains(&home.join("durable.db")));
    assert!(unreadable.contains(&home.join("aux.db")));
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        content: Content::Text(text.into()),
        timestamp: 0,
        extra: Default::default(),
    })
}

fn system(prompt: &str, tools: Vec<serde_json::Value>) -> Message {
    Message::System(SystemMessage {
        content: Content::Text(prompt.into()),
        sections: None,
        tools_added: Some(tools),
        tools_removed: None,
        timestamp: 0,
        extra: Default::default(),
    })
}

async fn ask(llm: &FakeLlm, messages: &[Message]) -> AssistantMessage {
    let model = sample_snapshot().model;
    let request = ModelRequest::new(&model, messages);
    llm.stream(&request, &mut |_| {})
        .await
        .expect("脚本里没有错误")
}

#[tokio::test]
async fn fake_llm_as_model_stream_keeps_turn_request_observables() {
    let llm = FakeLlm::new(vec![
        vec![
            call_round(1, "c1", "read", serde_json::json!({ "path": "a.txt" })),
            text_round(2, "读完了"),
        ],
        vec![text_round(1, "第二段")],
    ]);
    let tool = serde_json::json!({
        "name": "read", "description": "读文件", "parameters": { "type": "object" }
    });
    let head = vec![system("你是助手", vec![tool.clone()]), user("读一下 a.txt")];

    // 回合 1 的第一轮：最后一条是用户输入 → 新脚本、记一份请求。
    let first = ask(&llm, &head).await;
    assert_eq!(first.stop_reason, StopReason::ToolUse);
    assert_eq!(llm.turns(), 1);
    {
        let requests = llm.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].system_prompt, "你是助手");
        assert_eq!(requests[0].tools.len(), 1);
        assert_eq!(requests[0].tools[0].name, "read");
        assert_eq!(requests[0].messages.len(), 1);
        assert_eq!(requests[0].messages[0].role, Role::User);
        assert_eq!(
            requests[0].messages[0].text.as_deref(),
            Some("读一下 a.txt")
        );
        assert_eq!(requests[0].model.model, sample_snapshot().model.model);
    }

    // 同一回合的续轮：最后一条是工具结果 → 取下一轮，不开新回合、不记新请求。
    let mut again = head.clone();
    again.push(Message::Assistant(first));
    again.push(Message::ToolResult(ToolResultMessage {
        tool_call_id: "c1".into(),
        tool_name: "read".into(),
        content: vec![ContentBlock::text("内容")],
        is_error: false,
        details: None,
        timestamp: 0,
        extra: Default::default(),
    }));
    let second = ask(&llm, &again).await;
    assert_eq!(second.stop_reason, StopReason::Stop);
    assert_eq!(llm.turns(), 1, "续轮不算新回合");
    assert_eq!(llm.requests.lock().unwrap().len(), 1);
    {
        let inputs = llm.inputs.lock().unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0], RoundInput::First);
        let RoundInput::ToolResults { results, revised } = &inputs[1] else {
            panic!("{:?}", inputs[1]);
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].provider_call_id, "c1");
        assert_eq!(results[0].content, "内容");
        assert!(revised.is_empty());
    }

    // 下一条用户输入：新回合、取第二段脚本；请求里带着前面的历史。
    let mut next = again.clone();
    next.push(Message::Assistant(second));
    next.push(user("再来"));
    let third = ask(&llm, &next).await;
    assert_eq!(llm.turns(), 2);
    assert_eq!(
        third.content,
        vec![ContentBlock::text("第二段")],
        "第二段脚本"
    );
    let requests = llm.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let roles: Vec<Role> = requests[1].messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Assistant,
            Role::User
        ]
    );
    assert_eq!(requests[1].messages[1].tool_calls[0].name, "read");
    assert_eq!(requests[1].messages[2].tool_results[0].content, "内容");
    let _: Arc<dyn ModelStream> = llm.clone();
}
