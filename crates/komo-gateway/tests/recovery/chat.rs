//! 看客补挂：聊天来源的 run 跨重启，回复仍投回来源对端，只投一次。

use komo_gateway::service::test_support::Gate;
use komo_gateway::service::test_support::harness::{FakeLlm, call_round, inbound, text_round};
use komo_kernel::protocol::InboundAck;
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, Outbound};

use crate::{sender, start, until};

/// 一条聊天消息起的 run 在重启前还没结算（停在 `gate` 上），重启、续跑、跑完之后，回复原样投到发消息的那个
/// Telegram 对端——**只投一次**，而且是投给重启之后那个发送口（完成发生在重启之后）。
///
/// 重启前 run 必须还没到终态，否则测不出"看客只活在内存里"：已经结算的 run 由启动时的补投兜底，那是
/// `watch::replay_recent` 的事。这里的要点是 W4a 的订阅次序——`feed` 与看客在 `core.resume()` 之前就位，
/// 所以恢复之后才产生的终态帧一定被看客收到。
#[tokio::test]
async fn a_chat_run_in_flight_across_a_restart_still_gets_its_reply_delivered_once() {
    let gate = Gate::new();
    let first_sender = sender();
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "gate", serde_json::json!({})),
        text_round(2, "跑完了。"),
    ]]);
    let mut gateway = start(None, llm, vec![gate.tool()], &first_sender).await;

    // **走真的聊天入口**（Dispatcher）：只有它会把 `peer` 写进输入 entry（`chat.input`），
    // HTTP / TUI 提交的 run 测不出看客补挂。
    let ack = gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "111",
            "111",
            "跑一下 echo",
            "restart-1",
            true,
        ))
        .await;
    let InboundAck::Queued { run, .. } = ack else {
        panic!("这条消息该排进一个 run：{ack:?}");
    };
    gate.wait_entered(1).await;
    gateway.stop().await;

    // 重启：看客只活在上一台的内存里，这一下就是丢掉它。没有 gate 工具了。
    let second_sender = sender();
    let home = gateway.home.clone();
    let gateway = start(
        Some(&home),
        FakeLlm::finisher("跑完了。"),
        vec![],
        &second_sender,
    )
    .await;
    let record = gateway.wait_run(&run).await;
    assert_eq!(
        record.status(),
        komo_kernel::durable::SubmissionStatus::Done
    );

    let watching = std::sync::Arc::clone(&second_sender);
    until(
        "重启后补挂的看客把最终回复投回了来源渠道",
        || {
            let watching = std::sync::Arc::clone(&watching);
            async move { watching.texts().contains(&"跑完了。".to_string()) }
        },
    )
    .await;
    // 多等一拍，让启动补投（`replay_recent`）与帧路径都有机会再投一次——逻辑键保证只有一条。
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let delivered: Vec<_> = second_sender
        .sent()
        .into_iter()
        .filter(|message| {
            matches!(&message.outbound, Outbound::RunFinished { summary, .. } if summary == "跑完了。")
        })
        .collect();
    assert_eq!(delivered.len(), 1, "最终回复只投一次：{delivered:?}");
    assert_eq!(
        delivered[0].peer,
        ChannelPeer::new(ChannelPlatform::Telegram, "111"),
        "投到发消息的那个来源渠道"
    );
    assert!(
        !first_sender.texts().contains(&"跑完了。".to_string()),
        "完成发生在重启之后：{:?}",
        first_sender.texts()
    );
}
