//! 审批投到哪：**有人在看就只弹给他，没人看着才投 chat**（§11.4 / §10）。
//!
//! 线上踩到的样子：TUI 里跑一条 Run，shell 调用按规则要问，于是 home chat（Telegram）里
//! 多一条审批卡片——而人正盯着 TUI 的弹窗。那条卡片是一次网络往返，实测几秒到几十秒
//! （渠道不通时更久），它只会**晚到**；答完之后那张卡片还会在原地回写一条"已批准 · 短ID"，
//! 而人刚刚在弹窗里答过。
//!
//! 规则改成：**TUI / HTTP 来源（没有 chat 对端）且这个会话此刻有 SSE 订阅者**时，审批
//! 只走直播帧（弹窗）；Cron、脚本、关掉 TUI 之后的那些照样投 home chat——§10 的底线是
//! 不能因为无人值守就没人知道它在等。人看着看着走掉的那种由周期兜底补投。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::traits::LlmClient;

use crate::harness::{
    FakeLlm, Gw, Home, MemSender, call_round, config_toml, eventually, telegram_block, text_round,
};

/// 一段"要一次 shell"的脚本：strict 下 shell 是 Ask。
fn shell_script() -> Vec<Result<komo_kernel::types::turn::Round, komo_kernel::types::turn::LlmError>>
{
    vec![
        call_round(
            1,
            "pc-1",
            "shell",
            serde_json::json!({"command": "echo hi"}),
        ),
        text_round(2, "跑完了。"),
    ]
}

fn config() -> String {
    config_toml(&telegram_block("111"))
}

/// 一条会问的规则表（§7.1 的 strict）。
const STRICT: &str = "mode = \"strict\"\n";

/// 一条都不问（`mode = "auto"`）。
const AUTO: &str = "mode = \"auto\"\n";

/// 起一台：TUI 会给 home chat 一个发送口。
async fn gateway(llm: Arc<FakeLlm>) -> (Gw, Arc<MemSender>) {
    let sender = MemSender::new(komo_kernel::types::chat::ChannelPlatform::Telegram);
    let home = Home::with_policy(&config(), STRICT);
    let gw = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;
    (gw, sender)
}

/// 有人在看着这个会话（TUI 的 SSE 订阅）：审批不投 chat，只走直播帧；人走了之后
/// 周期兜底把它补投出去。
#[tokio::test]
async fn a_watched_session_keeps_the_approval_off_the_chat() {
    let llm = FakeLlm::new(vec![shell_script()]);
    let (gw, sender) = gateway(Arc::clone(&llm)).await;
    let session = gw.open_session().await;

    // **先订阅再交任务**：这正是 TUI 在做的（它开着这个会话的事件流，并且记着
    // "有人在看"——`EventHub::watch` 的守卫就是 HTTP SSE 处理器拿的那一个）。
    let frames = gw.state().hub.subscribe(&session);
    let watching = gw.state().hub.watch(&session);
    let _run = gw.submit(&session, "watched-1", "跑一下 echo").await.run;

    // 屏幕上的人看得见它：权威清单（`GET /v1/approvals`）里有，弹窗就是拿它弹的。
    let record = gw.wait_approval().await;

    // 而 chat 上一条审批都没有——屏幕前的人已经看得见了。
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        sender.approvals().is_empty(),
        "有人在看着会话时不该往 chat 投审批：{:?}",
        sender.approvals()
    );

    // 人关掉界面走人：这条还挂在待处理上，周期兜底要把它投出去（§10）。
    drop(frames);
    drop(watching);
    gw.state().sweep_unseen_interventions().await;
    let watching = Arc::clone(&sender);
    eventually("没人看了，审批补投到 home chat", move || {
        !watching.approvals().is_empty()
    })
    .await;
    // 补投的那条正是挂在界面上的那条。
    assert_eq!(sender.approvals()[0].approval, record.approval);
}

/// 没人看着（Cron、`komo run` 脚本、TUI 关着）：照旧投 home chat——那是它唯一的出口。
#[tokio::test]
async fn an_unwatched_run_still_asks_in_the_chat() {
    let llm = FakeLlm::new(vec![shell_script()]);
    let (gw, sender) = gateway(Arc::clone(&llm)).await;
    let session = gw.open_session().await;

    let _run = gw.submit(&session, "unwatched-1", "跑一下 echo").await.run;

    let watching = Arc::clone(&sender);
    eventually("没人看着，审批投到 home chat", move || {
        !watching.approvals().is_empty()
    })
    .await;
    assert_eq!(sender.approvals()[0].plan.tool, "shell");
}

/// `mode = "auto"`：一个审批都不产生——没有事件、没有卡片，直接跑完。
///
/// 这条钉的是用户的那句预期：**自动放行的工具就直接跑**。
#[tokio::test]
async fn auto_mode_asks_nothing_at_all() {
    let sender = MemSender::new(komo_kernel::types::chat::ChannelPlatform::Telegram);
    let home = Home::with_policy(&config(), AUTO);
    let llm = FakeLlm::new(vec![shell_script()]);
    let gw = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;
    let session = gw.open_session().await;

    let run = gw.submit(&session, "auto-1", "跑一下 echo").await.run;
    gw.wait_terminal(&run).await;

    assert!(
        sender.approvals().is_empty(),
        "auto 下不该有审批卡片：{:?}",
        sender.approvals()
    );
    // 也确实跑了：这一次调用有结果。
    let log = std::fs::read_to_string(home.events_path(&session)).expect("读得到会话日志");
    assert!(
        !log.contains("\"approval.requested\""),
        "auto 下不该有审批事件"
    );
    assert!(log.contains("\"tool.result\""), "该跑的还是跑了");
}
