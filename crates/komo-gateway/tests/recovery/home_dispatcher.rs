//! `docs/home-dispatcher.md` §8 Fix 1：聊天来源的交互 Run 跨重启还能收到最终回复。
//!
//! 看客只活在内存里（`service::run_watch` 的 `watch`），重启后没有代码重新订阅——
//! `runs.peer` 一直都在，但没人拿它去挂看客，于是这条 Run 跑完之后再没有人把回复投回
//! 发消息的那个渠道。修复是启动时对所有未终态、`peer` 非空的交互 Run 重新
//! `watch_interactive_run`（`service::run_watch::reattach_unfinished`）。
//!
//! 这里钉住修复之后的行为：一条 Run 在重启前还没到终态（停在一次 shell 调用的审批上），
//! 重启、批准、跑完之后，回复原样投到发消息的那个 Telegram 对端——**只投一次**，而且
//! 不是投给重启前那个发送口（完成发生在重启之后）。

use std::sync::Arc;

use komo_kernel::traits::{Inbound, LlmClient};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, Outbound};
use komo_kernel::types::status::RunState;

use crate::harness::*;

#[tokio::test]
async fn a_chat_run_in_flight_across_a_restart_still_gets_its_reply_delivered_once() {
    let home = Home::new();
    let first_sender = MemSender::new(ChannelPlatform::Telegram);

    // 第一段脚本：要跑一次 shell → 停在审批上。重启前这条 Run 必须还没到终态，否则测不出
    // "看客只活在内存里"这条修复——已经终态的 Run 由 §11.4 的补发（`deliveries` 表）兜底，
    // 那是另一条已经有的路。
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-1",
            "shell",
            serde_json::json!({"command": "echo hi"}),
        ),
        text_round(2, "跑完了。"),
    ]]);
    let gw = home
        .start_with(
            Arc::clone(&llm) as Arc<dyn LlmClient>,
            Arc::clone(&first_sender),
        )
        .await;

    // **走真的聊天入口**（Dispatcher），不是 `Gw::submit` 的 HTTP 路——只有聊天入口会把
    // `peer` 写进 `runs.peer`（§11.2），HTTP/TUI 提交的 Run 测不出这条修复。
    let ack = gw
        .dispatcher()
        .handle(inbound(
            ChannelPlatform::Telegram,
            "111",
            "111",
            "跑一下 echo",
            "restart-1",
            true,
        ))
        .await
        .expect("Dispatcher 处理");
    let komo_kernel::protocol::InboundAck::Queued { run, .. } = ack else {
        panic!("这条消息该排进一个 Run：{ack:?}");
    };

    let pending = gw.wait_approval().await;
    assert_eq!(
        pending.run.as_ref(),
        Some(&run),
        "停的是这条 Run 里那次 shell 调用"
    );
    assert_eq!(
        gw.db_state(&run).await,
        RunState::Waiting,
        "重启前这条 Run 还没到终态——这正是要测的那个窗口"
    );

    // "重启"：停机，同一个数据目录再起一台。看客只活在上一台的内存里，这一下就是丢掉它。
    gw.stop().await;
    let second_sender = MemSender::new(ChannelPlatform::Telegram);
    let gw = home
        .start_with(FakeLlm::finisher("跑完了。"), Arc::clone(&second_sender))
        .await;

    // 重启后这条审批还在等——同一条、同一个短 ID，账本没有因为重启换一副面孔。
    let still = gw.approvals().await;
    assert_eq!(still.len(), 1, "{still:?}");
    assert_eq!(still[0].approval, pending.approval, "还是原来那一条审批");

    // 批准它，让 Run 继续跑到完成。
    gw.decide(&pending.approval, true).await;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    // 核心断言：重启后新起的这台 Gateway 从没让任何看客订阅过这条 Run（`submit` 只在受理
    // 那一刻挂一次），它照样把最终回复投到了发消息的那个 Telegram 对端——靠的正是启动时
    // 补挂的看客。
    let watching = Arc::clone(&second_sender);
    eventually(
        "重启后补挂的看客把最终回复投回了来源渠道",
        move || watching.texts().contains(&"跑完了。".to_string()),
    )
    .await;

    let delivered: Vec<_> = second_sender
        .sent()
        .into_iter()
        .filter(
            |message| matches!(&message.outbound, Outbound::Text { text } if text == "跑完了。"),
        )
        .collect();
    assert_eq!(delivered.len(), 1, "最终回复只投一次：{delivered:?}");
    assert_eq!(
        delivered[0].peer,
        ChannelPeer::new(ChannelPlatform::Telegram, "111"),
        "投到发消息的那个来源渠道，不是别的对端"
    );

    // 第一台的发送口完全没收到这条完成回复：完成发生在重启之后，不该记在它头上，也顺带
    // 说明这条回复确实只被投过一次（不是两台各投了一份）。
    assert!(
        !first_sender.texts().contains(&"跑完了。".to_string()),
        "完成发生在重启之后：{:?}",
        first_sender.texts()
    );
}
