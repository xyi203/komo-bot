//! 访客模式：群里非操作者 @机器人，只拿到一段无工具、不碰记忆、不读项目文件的对话。
//!
//! 入站同 `feishu.rs`：假开放平台给出的原始事件负载经 `parse` + `from_message`，再交给
//! 真 Dispatcher。模型是脚本化替身，记下每一次 `TurnRequest`，"没有工具 / 没有记忆段"
//! 断言的就是它收到的请求。

use std::sync::Arc;

use komo_gateway::channels::ChannelSender;
use komo_gateway::channels::feishu::fake::{
    BOT_OPEN_ID, Behavior, FakeOpenApi, text_event, text_event_replying,
};
use komo_gateway::channels::feishu::{FeishuSender, inbound as feishu_inbound};
use komo_gateway::service::test_support::Unsure;
use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::types::ids::{MemoryId, RunId, SessionId};
use komo_kernel::types::memory::{
    Confirmation, ExtractionMetadata, MemoryItem, MemoryKind, MemoryScope, MemoryState,
    MemoryUsage, MemoryWork, Provenance,
};
use komo_store::models::SessionKind;

use crate::harness::{FakeLlm, FixedFactory, GatewayBuilder, TestGateway, call_round, text_round};

const MEMORY_FACT: &str = "客厅空调设 26 度";
const PROJECT_RULE: &str = "项目规范暗号：蓝鲸";

struct Wired {
    gateway: TestGateway,
    llm: Arc<FakeLlm>,
    _fake: Arc<FakeOpenApi>,
}

fn config(guest_agent: bool) -> String {
    let guest_line = if guest_agent {
        "guest_agent = \"guest\"\n"
    } else {
        ""
    };
    format!(
        r#"
default_agent = "assistant"

[agents.assistant]

[agents.guest]
tools = []
memory = "private"
project_files = false
instructions = "你是群里的访客助手。"

[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[model.memory]
type = "completion"
api_backend = "responses"
base_url = "https://memory-llm.example.com/v1"
model = "memory-test"
api_key_env = "KOMO_MEMORY_API_KEY"
effort = "low"

[models]
default = "main"

[memory]
enabled = true
model = "memory"
extract_idle = "1s"

[memory.retrieval]
mode = "keyword"
candidate_limit = 40
top_k = 8
max_tokens = 1500

[channels.feishu]
enabled = true
allow_from = ["ou_op"]
home_chat = "oc_home"
{guest_line}"#
    )
}

async fn wire_with(config: &str, llm: Arc<FakeLlm>) -> Wired {
    crate::harness::install_crypto();
    let fake = Arc::new(FakeOpenApi::start(Behavior::default()).await);
    let sender = Arc::new(FeishuSender::with_api(fake.api())) as Arc<dyn ChannelSender>;
    let gateway = GatewayBuilder::new(config)
        .env(
            "KOMO_LLM_API_KEY=test-key\nKOMO_MEMORY_API_KEY=test-memory-key\n\
             FEISHU_APP_ID=cli_test\nFEISHU_APP_SECRET=test-app-secret\n",
        )
        .llm(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .tool(Unsure::new().tool())
        .factory(FixedFactory::sender_only(sender))
        .start()
        .await;
    Wired {
        gateway,
        llm,
        _fake: fake,
    }
}

async fn wire(guest_agent: bool) -> Wired {
    wire_with(&config(guest_agent), FakeLlm::new(vec![])).await
}

fn decode(payload: &[u8]) -> InboundMessage {
    let parsed = feishu_inbound::parse(payload).expect("认得出这条事件");
    let key = feishu_inbound::request_key(&parsed.event_id);
    let feishu_inbound::EventKind::Message(event) = parsed.kind;
    feishu_inbound::from_message(&event, BOT_OPEN_ID, key).expect("这条要进 Dispatcher")
}

/// 群里 @机器人。`root` / `thread` 都空 = 主流里开新话题。
fn group(event: &str, sender: &str, text: &str, root: &str, thread: &str) -> InboundMessage {
    decode(&text_event_replying(
        event,
        "oc_group",
        "group",
        sender,
        &format!("@_user_1 {text}"),
        &[("@_user_1", BOT_OPEN_ID)],
        root,
        thread,
    ))
}

fn dm(event: &str, sender: &str, text: &str) -> InboundMessage {
    decode(&text_event(event, "oc_dm", "p2p", sender, text, &[]))
}

async fn queued(gateway: &TestGateway, message: InboundMessage) -> (SessionId, RunId) {
    match gateway.handle(message).await {
        InboundAck::Queued { session, run } => (session, run),
        other => panic!("应该排队：{other:?}"),
    }
}

async fn record_of(
    gateway: &TestGateway,
    session: &SessionId,
) -> komo_store::repos::session::SessionRecord {
    komo_store::repos::session::get(&gateway.state().db, session)
        .await
        .expect("读得出")
        .expect("会话在")
}

async fn wait_terminal(gateway: &TestGateway, session: &SessionId) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let runs = gateway.runs_of(session).await;
        if !runs.is_empty() && runs.iter().all(|run| run.state.is_terminal()) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("会话 {session} 的 Run 没在 10 秒内收尾");
}

fn memory_item() -> MemoryItem {
    let now = time::OffsetDateTime::now_utc();
    MemoryItem {
        id: MemoryId::from_raw("m-1"),
        revision: 1,
        content: MEMORY_FACT.into(),
        kind: MemoryKind::Preference,
        scope: MemoryScope::Personal,
        provenance: Provenance::UserStatement,
        confirmation: Confirmation::Unconfirmed,
        state: MemoryState::Active,
        evidence: vec![],
        observed_at: now,
        valid_until: None,
        created_at: now,
        updated_at: now,
        extraction: ExtractionMetadata::new("memory-test", None, "v1"),
        usage: MemoryUsage::default(),
        supersedes: None,
        superseded_by: None,
    }
}

#[tokio::test]
async fn a_stranger_mention_in_a_group_gets_an_isolated_guest_session() {
    let wired = wire(true).await;
    let gateway = &wired.gateway;

    let (guest, _) = queued(gateway, group("G1", "ou_a", "你好", "", "")).await;
    let record = record_of(gateway, &guest).await;
    assert_eq!(record.kind, SessionKind::Guest);
    assert_eq!(record.agent_id, "guest");
    assert_eq!(record.origin, "guest:feishu:oc_group#om_G1");

    // 另一个陌生人在同一个话题里：同一个访客会话。
    let (again, _) = queued(gateway, group("G2", "ou_b", "我也问一句", "om_G1", "omt_g")).await;
    assert_eq!(again, guest);

    // 操作者在同一个话题里：另一个会话，走他自己的 `chat:` 与默认 Agent。
    let (operator, _) = queued(gateway, group("G3", "ou_op", "看日志", "om_G1", "omt_g")).await;
    assert_ne!(operator, guest, "访客碰不到操作者的会话");
    let record = record_of(gateway, &operator).await;
    assert_eq!(record.kind, SessionKind::Normal);
    assert_eq!(record.agent_id, "assistant");
    assert_eq!(record.origin, "chat:feishu:oc_group#om_G1");

    // 另一个话题的陌生人：另一个访客会话。
    let (other, _) = queued(gateway, group("G4", "ou_a", "新话题", "", "")).await;
    assert_ne!(other, guest);
    assert_eq!(gateway.sessions().await.len(), 3);
}

#[tokio::test]
async fn a_stranger_is_refused_when_no_guest_agent_is_configured() {
    let wired = wire(false).await;

    let ack = wired
        .gateway
        .handle(group("N1", "ou_a", "你好", "", ""))
        .await;
    assert!(matches!(ack, InboundAck::Rejected { .. }), "{ack:?}");
    assert!(wired.gateway.sessions().await.is_empty(), "不留会话");
}

#[tokio::test]
async fn a_stranger_direct_message_is_refused_even_with_a_guest_agent() {
    let wired = wire(true).await;

    let ack = wired.gateway.handle(dm("P1", "ou_a", "在吗")).await;
    assert!(matches!(ack, InboundAck::Rejected { .. }), "{ack:?}");
    assert!(wired.gateway.sessions().await.is_empty(), "不留会话");
}

#[tokio::test]
async fn a_stranger_cannot_answer_an_intervention_or_use_commands() {
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "unsure", serde_json::json!({})),
        text_round(2, "好了。"),
    ]]);
    let wired = wire_with(&config(true), llm).await;
    let gateway = &wired.gateway;

    let (_, run) = queued(gateway, group("A1", "ou_op", "试一下", "", "")).await;
    let pending = gateway.wait_intervention(&run).await;
    let sessions_before = gateway.sessions().await.len();

    for (event, text) in [
        ("A2", format!("/answer {} satisfied", pending.handle)),
        ("A3", "/status".to_string()),
        ("A4", "/pending".to_string()),
        ("A5", "/cancel".to_string()),
        ("A6", "/new".to_string()),
        ("A7", "/approve abc".to_string()),
    ] {
        let ack = gateway.handle(group(event, "ou_a", &text, "", "")).await;
        assert!(
            matches!(ack, InboundAck::Rejected { .. }),
            "{text}: {ack:?}"
        );
    }

    let still: Vec<String> = gateway
        .interventions()
        .await
        .into_iter()
        .map(|one| one.handle)
        .collect();
    assert_eq!(still, vec![pending.handle.clone()], "介入没被动过");
    assert_eq!(gateway.sessions().await.len(), sessions_before, "不开会话");

    gateway.answer(&pending.handle, "satisfied").await;
}

#[tokio::test]
async fn removing_the_guest_agent_from_the_config_refuses_the_next_stranger() {
    let wired = wire(true).await;
    queued(&wired.gateway, group("H1", "ou_a", "你好", "", "")).await;

    wired.gateway.write_config(&config(false));
    wired.gateway.reload().await;

    let ack = wired
        .gateway
        .handle(group("H2", "ou_a", "还在吗", "om_H1", "omt_h"))
        .await;
    assert!(matches!(ack, InboundAck::Rejected { .. }), "{ack:?}");
}

#[tokio::test]
async fn a_guest_run_sees_no_tools_no_memory_and_no_project_files() {
    let wired = wire(true).await;
    let gateway = &wired.gateway;
    gateway
        .state()
        .memory
        .put(memory_item(), None)
        .await
        .expect("写得下");
    std::fs::write(
        gateway.home.join("workspaces").join("AGENTS.md"),
        PROJECT_RULE,
    )
    .expect("写项目规范");

    // 对照：操作者同样问空调，模型收到工具、记忆与项目规范。
    let (operator, _) = queued(gateway, group("R1", "ou_op", "空调设多少度来着", "", "")).await;
    wait_terminal(gateway, &operator).await;
    let (guest, _) = queued(gateway, group("R2", "ou_a", "空调设多少度来着", "", "")).await;
    wait_terminal(gateway, &guest).await;

    let requests = wired.llm.requests.lock().expect("请求表").clone();
    let by = |session: &SessionId| {
        requests
            .iter()
            .find(|request| &request.session == session)
            .expect("这个会话的请求")
            .clone()
    };
    let operator = by(&operator);
    assert!(!operator.tools.is_empty(), "操作者有工具");
    assert!(operator.system_prompt.contains(MEMORY_FACT));
    assert!(operator.system_prompt.contains(PROJECT_RULE));

    let guest = by(&guest);
    assert!(guest.tools.is_empty(), "访客没有工具：{:?}", guest.tools);
    assert!(guest.system_prompt.contains("你是群里的访客助手。"));
    assert!(!guest.system_prompt.contains(MEMORY_FACT));
    assert!(!guest.system_prompt.contains("[已知记忆]"));
    assert!(!guest.system_prompt.contains(PROJECT_RULE));
    assert!(!guest.system_prompt.contains("<project_instructions"));
    assert!(guest.memories.is_empty());
}

#[tokio::test]
async fn a_guest_run_is_never_handed_to_memory_extraction() {
    let wired = wire(true).await;
    let gateway = &wired.gateway;

    let (guest, _) = queued(gateway, group("E1", "ou_a", "我喜欢深色主题", "", "")).await;
    wait_terminal(gateway, &guest).await;
    // `extract_idle = 1s`：等够了，再手动推一轮（后台自己的节拍更慢）。
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let report = gateway.state().memories.process_pending(8).await.unwrap();
    assert_eq!(report.skipped, 1, "{report:?}");
    let runs = gateway.runs_of(&guest).await;
    assert!(
        runs.iter().all(|run| run.memory_work == MemoryWork::Done),
        "{runs:?}"
    );
    assert!(
        !wired.llm.models().contains(&"memory-test".to_string()),
        "访客的话没有交给记忆模型：{:?}",
        wired.llm.models()
    );

    // 对照：操作者的 Run 照常交给记忆模型。
    let (operator, _) = queued(gateway, group("E2", "ou_op", "我喜欢深色主题", "", "")).await;
    wait_terminal(gateway, &operator).await;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    gateway.state().memories.process_pending(8).await.unwrap();
    assert!(
        wired.llm.models().contains(&"memory-test".to_string()),
        "操作者的 Run 该被提取：{:?}",
        wired.llm.models()
    );
}
