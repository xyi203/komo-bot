//! 看客（`watch.rs`）：内部输入的来源对端（决定 6）与重启补挂。

use std::sync::Arc;

use komo_kernel::protocol::InboundAck;
use komo_kernel::types::chat::{ChannelPlatform, Outbound};
use komo_runtime::durable::harness::{InputEntry, WhenBusy};

use super::sessions::NewSession;
use super::submit::InputRequest;
use super::watch;
use crate::channels::ChannelSender;
use crate::service::test_support::Gate;
use crate::service::test_support::harness::{
    FakeLlm, FixedFactory, GatewayBuilder, MemSender, TestGateway, call_round, config_toml,
    eventually, inbound, text_round,
};

/// home chat 是 999，操作者 111：来源对端与 home chat 不是同一个，"各回各的"才说得清。
const CONFIG: &str = r#"
[channels.telegram]
enabled = true
allow_from = [111]
home_chat = 999
"#;

async fn start(llm: Arc<FakeLlm>, gate: Option<&Arc<Gate>>) -> (TestGateway, Arc<MemSender>) {
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let mut builder = GatewayBuilder::new(&config_toml(CONFIG))
        .durable()
        .fake_llm(llm)
        .factory(FixedFactory::sender_only(
            Arc::clone(&sender) as Arc<dyn ChannelSender>
        ));
    if let Some(gate) = gate {
        builder = builder.tool(gate.tool());
    }
    (builder.start().await, sender)
}

fn internal(key: &str) -> InputRequest {
    InputRequest::new("[后台任务 x]\n\n结果")
        .request_id(key)
        .entry(InputEntry::new("task.result", None))
        .when_busy(WhenBusy::Steer)
}

fn finished_to(sender: &MemSender, chat: &str, run: &komo_kernel::types::ids::RunId) -> usize {
    sender
        .to_chat(chat)
        .iter()
        .filter(
            |message| matches!(&message.outbound, Outbound::RunFinished { run: r, .. } if r == run),
        )
        .count()
}

/// 决定 6：内部输入（任务结果、监听事件）开的 run，结果回给当前路径上最近一条带 peer 的用户输入的
/// peer——不是 home chat。
#[tokio::test]
async fn an_internal_input_is_answered_to_the_latest_peer_on_the_path() {
    let (gateway, sender) = start(FakeLlm::finisher("好"), None).await;
    let ack = gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "111",
            "111",
            "在",
            "t:1",
            true,
        ))
        .await;
    let InboundAck::Queued { session, run } = ack else {
        panic!("{ack:?}");
    };
    gateway.wait_run(&run).await;

    let core = gateway.state().core().unwrap().clone();
    let accepted = core
        .submit_input(session.as_str(), internal("task-result:t:1"))
        .await
        .unwrap();
    gateway.wait_run(&accepted.run).await;

    let watching = Arc::clone(&sender);
    let run = accepted.run.clone();
    eventually("回给了说话的那个 peer", move || {
        finished_to(&watching, "111", &run) == 1
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        finished_to(&sender, "999", &accepted.run),
        0,
        "不投 home chat"
    );
    assert_eq!(finished_to(&sender, "111", &accepted.run), 1, "只一条");
}

/// 路径上没有任何带 peer 的输入：主会话退回 home chat；别的会话（TUI / HTTP 开的）不投——它自己
/// 看着事件流。
#[tokio::test]
async fn an_internal_input_without_a_peer_falls_back_to_home_chat_for_the_main_session_only() {
    let (gateway, sender) = start(FakeLlm::finisher("好"), None).await;
    let core = gateway.state().core().unwrap().clone();

    let (main, _) = core.main_session("assistant").await.unwrap();
    watch::ensure(gateway.state(), &main.id);
    let to_main = core
        .submit_input(&main.id, internal("task-result:m:1"))
        .await
        .unwrap();
    gateway.wait_run(&to_main.run).await;

    let normal = core
        .create_session(NewSession::new("assistant"))
        .await
        .unwrap();
    watch::ensure(gateway.state(), &normal.id);
    let to_normal = core
        .submit_input(&normal.id, internal("task-result:n:1"))
        .await
        .unwrap();
    gateway.wait_run(&to_normal.run).await;

    let watching = Arc::clone(&sender);
    let run = to_main.run.clone();
    eventually("主会话的结果回了 home chat", move || {
        finished_to(&watching, "999", &run) == 1
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(finished_to(&sender, "999", &to_normal.run), 0);
    assert_eq!(finished_to(&sender, "111", &to_normal.run), 0);
}

/// 重启之后：run 在重启前开了头、重启后才收尾，回复照样投回说话的 peer，且只投一次。
#[tokio::test]
async fn a_chat_run_in_flight_across_a_restart_still_gets_its_reply_once() {
    let gate = Gate::new();
    let (mut gateway, first_sender) = start(
        FakeLlm::new(vec![vec![
            call_round(1, "pc-1", "gate", serde_json::json!({})),
            text_round(2, "甲好了。"),
        ]]),
        Some(&gate),
    )
    .await;
    let ack = gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "111",
            "111",
            "等着",
            "t:1",
            true,
        ))
        .await;
    let InboundAck::Queued { run, .. } = ack else {
        panic!("{ack:?}");
    };
    gate.wait_entered(1).await;
    assert_eq!(finished_to(&first_sender, "111", &run), 0);

    // 重启：同一个数据目录，新的发送口。
    let second = MemSender::new(ChannelPlatform::Telegram);
    gateway
        .restart_with(
            &config_toml(CONFIG),
            vec![FixedFactory::sender_only(
                Arc::clone(&second) as Arc<dyn ChannelSender>
            )],
        )
        .await;
    gateway.wait_run(&run).await;

    let watching = Arc::clone(&second);
    let waited = run.clone();
    eventually("重启后回复投回了说话的 peer", move || {
        finished_to(&watching, "111", &waited) >= 1
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(finished_to(&second, "111", &run), 1, "只投一次");
    assert_eq!(finished_to(&first_sender, "111", &run), 0);
}
