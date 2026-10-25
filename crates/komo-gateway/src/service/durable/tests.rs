use std::path::Path;
use std::sync::Arc;

use komo_kernel::durable::{
    AssistantMessage, Content, ContentBlock, Message, StopReason, SystemMessage, ToolResultMessage,
    UserMessage,
};
use komo_kernel::types::turn::{Role, RoundInput};
use komo_runtime::llm::{ModelRequest, ModelStream};

use crate::service::state::{codemode_unreadable_paths, protected_paths};
use crate::service::test_support::harness::{
    FakeLlm, GatewayBuilder, call_round, telegram_config, text_round,
};
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
    let config = telegram_config("111");
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
    let mut gateway = GatewayBuilder::new(&telegram_config("111"))
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

/// 路由按存储模式挑：共用的（models）在，会话路由是 v2 的（W2a 起有了），还没填的 Legacy 独有
/// 路由（interventions，W4a）在 Durable 下还是空桩。
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

#[test]
fn write_refuses_the_durable_databases() {
    use komo_runtime::tools::protect::Protected;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
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
