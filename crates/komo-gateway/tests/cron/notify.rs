//! `notify`（§10）：过滤**结果**的投递，**从不**过滤一次等待。

use std::sync::Arc;

use komo_kernel::traits::LlmClient;
use komo_kernel::types::status::RunStatus;

use crate::harness::*;

fn daily(notify: &str) -> serde_json::Value {
    serde_json::json!({
        "name": "morning-summary",
        "schedule": "0 9 * * *",
        "timezone": "Asia/Shanghai",
        "prompt": "整理今天的技术动态",
        "notify": notify,
    })
}

/// `always`：跑成了也投。
#[tokio::test]
async fn always_delivers_a_good_run() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "整理好了")]) as Arc<dyn LlmClient>)
        .await;
    let job = gw.add_job(daily("always")).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_status(&run, |s| s.is_terminal(), "终态").await;

    let sender = Arc::clone(&gw.home);
    eventually("home chat 收到结果", move || {
        sender.texts().iter().any(|t| t.contains("整理好了"))
    })
    .await;
    gw.stop().await;
}

/// `on_error`：跑成了**不投**，但一次等待照样投——那是任务在**问**，不是在报告。
#[tokio::test]
async fn on_error_keeps_quiet_about_a_good_run_but_never_about_a_wait() {
    let home = Home::new();
    let counter = home.workspace().join("ran.log");

    // ── 前半：一个顺利跑完的 Job，`on_error` 下不投。
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "整理好了")]) as Arc<dyn LlmClient>)
        .await;
    let quiet = gw.add_job(daily("on_error")).await;
    gw.make_due(&quiet.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_status(&run, |s| s.is_terminal(), "终态").await;

    // 等触发状态记成 ok，说明盯梢已经处理完这个 Run——这时还没投才说明问题。
    let mut settled = false;
    for _ in 0..100 {
        if gw
            .firings(&quiet.id)
            .await
            .first()
            .map(|f| f.status)
            .is_some_and(|s| s == komo_kernel::cron::FiringStatus::Ok)
        {
            settled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(settled, "这一次触发应当已经记成 ok");
    assert!(
        !gw.home.texts().iter().any(|t| t.contains("整理好了")),
        "on_error 不该报告一次顺利的运行：{:?}",
        gw.home.texts()
    );
    gw.stop().await;

    // ── 后半：同一份配置，一个停在等待上的 Job。**一律投**。
    let llm = FakeLlm::always(vec![shell_append(1, "pc-1", &counter, "one")]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let asking = gw.add_job(daily("on_error")).await;
    gw.make_due(&asking.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_status(&run, |s| s == RunStatus::WaitingApproval, "等待审批")
        .await;

    let sender = Arc::clone(&gw.home);
    eventually("等待照样投到 home chat", move || {
        !sender.approvals().is_empty()
    })
    .await;
    gw.stop().await;
}

/// `never`：结果一概不投——**但等待还是投**，否则这个 Job 会从此停在那里。
#[tokio::test]
async fn never_still_lets_a_wait_through() {
    let home = Home::new();
    let counter = home.workspace().join("ran.log");
    let llm = FakeLlm::always(vec![shell_append(1, "pc-1", &counter, "one")]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let job = gw.add_job(daily("never")).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_status(&run, |s| s == RunStatus::WaitingApproval, "等待审批")
        .await;

    let sender = Arc::clone(&gw.home);
    eventually("never 也要投一次等待", move || {
        !sender.approvals().is_empty()
    })
    .await;
    gw.stop().await;
}

/// `notify` 写错**当场拒绝**，不静默当默认，而且说得出接受哪几个词。
///
/// 它是一个枚举而不是一个字符串，所以拒绝发生在反序列化那一层（422）——形状不同，
/// 但两件要紧的事都在：**没有创建任何 Job**，以及消息里列出了接受的写法。
#[tokio::test]
async fn a_misspelled_notify_is_refused() {
    let home = Home::new();
    let gw = home.start(FakeLlm::new(vec![]) as Arc<dyn LlmClient>).await;
    let (status, body) = gw.post("/v1/cron", daily("nerver")).await;
    assert!(status == 400 || status == 422, "{status} {body}");
    assert!(body.contains("nerver"), "说出是哪个词：{body}");
    assert!(body.contains("on_error"), "列出接受的写法：{body}");
    assert!(gw.cron_list().await.jobs.is_empty(), "一个 Job 都没留下");
    gw.stop().await;
}
