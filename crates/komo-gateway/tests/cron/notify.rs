//! `notify`（§10）：过滤**结果**往主对话里的投递（`docs/redesign.md` §2.7）。结果作为 Komo 在主对话里说的
//! 一句话落下来：`always` 每次都投，`on_error` 只在出错时投，`never` 不投；静默回复（`NO_REPLY`）不投。
//! 触发状态不受它影响，写错的值当场被拒。一次**等待**（停住的运行在问你）不看 `notify`，见 `confirm.rs`。

use std::sync::Arc;

use komo_kernel::events::EventPayload;

use crate::harness::*;

/// 主对话里 Komo 说过的话（`/v1/home-session` 没有就建；旧的在前）。
pub(crate) async fn main_lines(gw: &Gw) -> Vec<String> {
    let (code, body) = gw.get("/v1/home-session").await;
    assert_eq!(code, 200, "{body}");
    let session = serde_json::from_str::<komo_kernel::protocol::http::SessionSummary>(&body)
        .expect("主会话")
        .session;
    gw.events(&session)
        .await
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::LineReply(line) => Some(line.text),
            _ => None,
        })
        .collect()
}

/// 等主对话里出现一句满足条件的话。
pub(crate) async fn wait_line(gw: &Gw, what: &str, wanted: impl Fn(&str) -> bool) -> String {
    for _ in 0..200 {
        if let Some(line) = main_lines(gw).await.into_iter().find(|line| wanted(line)) {
            return line;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!(
        "等了 10 秒，主对话里也没有：{what}；现有 {:?}",
        main_lines(gw).await
    );
}

/// 等触发状态落成 `want`（盯梢是异步的）。
async fn wait_status(
    gw: &Gw,
    job: &komo_kernel::types::ids::CronJobId,
    want: komo_kernel::cron::FiringStatus,
) {
    for _ in 0..100 {
        if gw.firings(job).await.first().map(|f| f.status) == Some(want) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("触发状态没有记成 {want:?}");
}

fn daily(notify: &str) -> serde_json::Value {
    serde_json::json!({
        "name": "morning-summary",
        "schedule": "0 9 * * *",
        "timezone": "Asia/Shanghai",
        "prompt": "整理今天的技术动态",
        "notify": notify,
    })
}

/// `always`：跑成了也投——前面带上定时任务的名字，正文是这一次 Run 的最终回复。
#[tokio::test]
async fn always_posts_a_good_runs_reply_to_the_main_chat() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "整理好了")]))
        .await;
    let job = gw.add_job(daily("always")).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;

    let line = wait_line(&gw, "定时结果", |line| line.contains("整理好了")).await;
    assert_eq!(line, "定时任务「morning-summary」：整理好了");
    wait_status(&gw, &job.id, komo_kernel::cron::FiringStatus::Ok).await;
    // 主对话列表的最后一句话就是它。
    let (_, chats) = gw.get_json("/v1/chats").await;
    assert!(
        chats["chats"][0]["lastMessage"]
            .as_str()
            .unwrap()
            .contains("整理好了"),
        "{chats}"
    );
    gw.stop().await;
}

/// 不说话（S1）：prompt Job 的回答整段是 `NO_REPLY` 时触发照记 `ok`，`always` 下也不投——
/// 同命令型 Job 的空 stdout。
#[tokio::test]
async fn a_silent_reply_settles_ok_but_posts_nothing() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "NO_REPLY")]))
        .await;
    let job = gw.add_job(daily("always")).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;

    wait_status(&gw, &job.id, komo_kernel::cron::FiringStatus::Ok).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let lines = main_lines(&gw).await;
    assert!(lines.is_empty(), "记号不该多出一句话：{lines:?}");
    gw.stop().await;
}

/// `on_error`：跑成了**不投**；出错照投。
#[tokio::test]
async fn on_error_keeps_quiet_about_a_good_run_but_posts_a_failure() {
    let home = Home::new();
    // ── 前半：一个顺利跑完的 Job，`on_error` 下不投。
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "整理好了")]))
        .await;
    let quiet = gw.add_job(daily("on_error")).await;
    gw.make_due(&quiet.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
    // 等触发状态记成 ok，说明盯梢已经处理完这个 Run——这时还没有才说明问题。
    wait_status(&gw, &quiet.id, komo_kernel::cron::FiringStatus::Ok).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let lines = main_lines(&gw).await;
    assert!(
        lines.is_empty(),
        "on_error 不该报告一次顺利的运行：{lines:?}"
    );
    gw.stop().await;

    // ── 后半：同一份配置，一个**失败**的 Job。照投。
    let llm = FakeLlm::always(vec![Err(komo_kernel::types::turn::LlmError::Rejected {
        status: 400,
        message: "模型后端坏了".into(),
    })]);
    let gw = home.start(Arc::clone(&llm)).await;
    let failing = gw.add_job(daily("on_error")).await;
    gw.make_due(&failing.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
    let line = wait_line(&gw, "失败的结果", |line| line.contains("任务失败")).await;
    assert!(
        line.starts_with("定时任务「morning-summary」：任务失败："),
        "{line}"
    );
    wait_status(&gw, &failing.id, komo_kernel::cron::FiringStatus::Error).await;
    gw.stop().await;
}

/// `never`：结果一概不投，触发状态照样记成 `error`——结果去原 Session 看（§10 最后一条）。
#[tokio::test]
async fn never_keeps_quiet_even_about_a_failure_but_the_firing_still_settles_as_error() {
    let home = Home::new();
    let llm = FakeLlm::always(vec![Err(komo_kernel::types::turn::LlmError::Rejected {
        status: 400,
        message: "模型后端坏了".into(),
    })]);
    let gw = home.start(Arc::clone(&llm)).await;

    let job = gw.add_job(daily("never")).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;

    wait_status(&gw, &job.id, komo_kernel::cron::FiringStatus::Error).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let lines = main_lines(&gw).await;
    assert!(lines.is_empty(), "never 一概不投：{lines:?}");
    gw.stop().await;
}

/// `notify` 写错**当场拒绝**，不静默当默认，而且说得出接受哪几个词。
///
/// 它是一个枚举而不是一个字符串，所以拒绝发生在反序列化那一层（422）——形状不同，
/// 但两件要紧的事都在：**没有创建任何 Job**，以及消息里列出了接受的写法。
#[tokio::test]
async fn a_misspelled_notify_is_refused() {
    let home = Home::new();
    let gw = home.start(FakeLlm::new(vec![])).await;
    let (status, body) = gw.post_cron(daily("nerver")).await;
    assert!(status == 400 || status == 422, "{status} {body}");
    assert!(body.contains("nerver"), "说出是哪个词：{body}");
    assert!(body.contains("on_error"), "列出接受的写法：{body}");
    assert!(gw.cron_list().await.jobs.is_empty(), "一个 Job 都没留下");
    gw.stop().await;
}
