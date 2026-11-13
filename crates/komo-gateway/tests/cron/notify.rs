//! 触发状态（§10）：不论 `notify` 怎么写，结果都照常回写进 `cron_firings`。聊天渠道删除后（`docs/redesign.md`
//! P17）结果不再往外投，`notify` 只剩配置里的一个值（后续随目标的定时重做），所以这里只验：
//! 触发状态不受它影响，以及写错的值当场被拒。

use std::sync::Arc;

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

/// 不说话（S1）：prompt Job 的回答整段是 `NO_REPLY` 时触发照记 `ok`——同命令型 Job 的空 stdout。
#[tokio::test]
async fn a_silent_reply_settles_ok() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "NO_REPLY")]))
        .await;
    let job = gw.add_job(daily("always")).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;

    let mut settled = false;
    for _ in 0..100 {
        if gw
            .firings(&job.id)
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
    gw.stop().await;
}

/// 失败的触发在任何 `notify` 下都照记 `error`——结果去原 Session 看（§10 最后一条）。
#[tokio::test]
async fn a_failed_run_settles_the_firing_as_error_whatever_notify_says() {
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

    let mut settled = false;
    for _ in 0..100 {
        if gw.firings(&job.id).await.first().map(|f| f.status)
            == Some(komo_kernel::cron::FiringStatus::Error)
        {
            settled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(settled, "触发状态应当记成 error");
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
    let (status, body) = gw.post("/v1/cron", daily("nerver")).await;
    assert!(status == 400 || status == 422, "{status} {body}");
    assert!(body.contains("nerver"), "说出是哪个词：{body}");
    assert!(body.contains("on_error"), "列出接受的写法：{body}");
    assert!(gw.cron_list().await.jobs.is_empty(), "一个 Job 都没留下");
    gw.stop().await;
}
