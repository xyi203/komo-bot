//! 触发面：唯一键、重启、重叠、留痕、清单（§10、§14 阶段 7 第一句）。

use std::sync::Arc;

use komo_kernel::cron::FiringStatus;
use komo_kernel::traits::LlmClient;

use crate::harness::*;

fn daily() -> serde_json::Value {
    serde_json::json!({
        "name": "morning-summary",
        "schedule": "0 9 * * *",
        "timezone": "Asia/Shanghai",
        "prompt": "整理今天的技术动态",
    })
}

/// **§14 阶段 7 第一句：重启不重复创建同次触发。**
///
/// 做法就是 §10 的那条唯一键 `job_id + scheduled_at_utc`。这里把它放在**真的一次
/// 重启**上验：第一台 Gateway 投出了这一槽，停机，同一个数据目录再起一台，把槽位
/// 按回原处（模拟"推进没落库就崩了"）再扫一轮——第二次 claim 是 `false`，触发记录
/// 还是一条，Run 还是一个。
#[tokio::test]
async fn a_restart_does_not_create_the_same_firing_twice() {
    let home = Home::new();
    let counter = home.workspace().join("ran.log");

    // 第一台：投出这一槽。模型让它跑一条 shell——但 shell 要审批，所以它会停在等待上，
    // 这正好让这一次触发**一直没结束**，重启之后还在。
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "做完了")]) as Arc<dyn LlmClient>)
        .await;
    let job = gw.add_job(daily()).await;
    gw.make_due(&job.id).await;
    let slot = gw.job(&job.id).await.next_run_at.expect("有槽位");

    let first = gw.tick().await;
    assert_eq!(first.fired.len(), 1, "{first:?}");
    let run = first.fired[0].run.clone();
    gw.wait_status(&run, |s| s.is_terminal(), "终态").await;
    gw.stop().await;

    let firings_before = {
        use komo_kernel::traits::CronRepo;
        let db = komo_store::Db::connect(home.path().join("state.db"))
            .await
            .expect("打开库");
        let repo = komo_store::TursoCronRepo::new(db);
        repo.firings(&job.id, 10).await.expect("读得出")
    };
    assert_eq!(firings_before.len(), 1);

    // ── 重启。同一个数据目录。
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "做完了")]) as Arc<dyn LlmClient>)
        .await;

    // 把槽位按回原处：这就是"推进已经算出来了、但落库前进程没了"。
    {
        let stored = gw.job(&job.id).await;
        gw.state()
            .cron
            .advance(&job.id, Some(slot), stored.status, None)
            .await
            .expect("拨得动");
    }

    let second = gw.tick().await;
    assert!(second.fired.is_empty(), "同一计划时间不再投：{second:?}");
    assert_eq!(
        second.skipped[0].reason,
        komo_runtime::scheduler::SkipReason::AlreadyFired
    );

    let firings = gw.firings(&job.id).await;
    assert_eq!(firings.len(), 1, "还是一条触发记录：{firings:?}");
    assert_eq!(firings[0].run, Some(run.clone()), "还是原来那个 Run");

    // 槽位照样往前推——跳过的是这一槽，不是这个 Job。
    assert!(gw.job(&job.id).await.next_run_at.unwrap() > slot);

    // 副作用文件从来没被写过两次（这里模型没调工具，所以是 0 —— 断言的是**次数**）。
    assert_eq!(lines(&counter), 0);
    gw.stop().await;
}

/// **上一次仍未结束时跳过本次并记录原因**（§10）。
///
/// "记录"落在 `cron_firings` 上，不是只落在日志里：`last_error` 那一列是当前状态，
/// 下一次成功触发就会把它清掉，而"昨天那一槽被跳过了"是一条历史。
#[tokio::test]
async fn an_overlapping_slot_is_skipped_and_leaves_a_trace() {
    let home = Home::new();
    let counter = home.workspace().join("ran.log");

    // 模型每一段都要跑一条 shell：shell 要审批，于是第一次触发**停在等待上**——
    // §10 说的"含等待审批"就是这一种"还没结束"。
    let llm = FakeLlm::always(vec![shell_append(1, "pc-1", &counter, "one")]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let job = gw.add_job(daily()).await;
    gw.make_due(&job.id).await;
    let first = gw.tick().await;
    assert_eq!(first.fired.len(), 1);
    gw.wait_status(
        &first.fired[0].run,
        |s| s == komo_kernel::types::status::RunStatus::WaitingApproval,
        "等待审批",
    )
    .await;

    // 下一槽到了，而上一次还在等人。
    gw.make_due(&job.id).await;
    let second = gw.tick().await;
    assert!(second.fired.is_empty(), "{second:?}");
    assert_eq!(
        second.skipped[0].reason,
        komo_runtime::scheduler::SkipReason::Overlap
    );

    let firings = gw.firings(&job.id).await;
    assert_eq!(firings.len(), 2, "跳过的那一槽也有记录：{firings:?}");
    let skipped = firings
        .iter()
        .find(|f| f.status == FiringStatus::Skipped)
        .expect("有一条 skipped");
    assert!(
        skipped.error.as_deref().unwrap().contains("还没结束"),
        "说得出为什么：{skipped:?}"
    );

    // 副作用只发生了……零次：shell 还卡在审批上（这正是"未结束"）。
    assert_eq!(lines(&counter), 0, "没批准就没跑");
    gw.stop().await;
}

/// `overlap = allow`：上一次还没结束也照样再起一个。
#[tokio::test]
async fn an_allow_policy_fires_even_when_the_last_one_is_still_waiting() {
    let home = Home::new();
    let counter = home.workspace().join("ran.log");
    let llm = FakeLlm::always(vec![shell_append(1, "pc-1", &counter, "one")]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let mut body = daily();
    body["overlap"] = serde_json::json!("allow");
    let job = gw.add_job(body).await;

    gw.make_due(&job.id).await;
    let first = gw.tick().await;
    gw.wait_status(
        &first.fired[0].run,
        |s| s == komo_kernel::types::status::RunStatus::WaitingApproval,
        "等待审批",
    )
    .await;

    gw.make_due(&job.id).await;
    let second = gw.tick().await;
    assert_eq!(second.fired.len(), 1, "allow 照样投：{second:?}");
    assert_eq!(gw.firings(&job.id).await.len(), 2);

    gw.stop().await;
}

/// **清单上看得见"上一次怎么样"与"下一次什么时候"**（§10：结果去原 Session 查看）。
#[tokio::test]
async fn the_list_shows_the_last_firing_and_the_next_slot() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "整理好了")]) as Arc<dyn LlmClient>)
        .await;
    let job = gw.add_job(daily()).await;

    // 还没响过：有下一次，没有上一次。
    let list = gw.cron_list().await;
    let status = list
        .status
        .iter()
        .find(|s| s.job == job.id)
        .expect("清单里有它");
    assert!(status.next_run_at.is_some());
    assert!(status.last.is_none(), "还没响过");

    gw.make_due(&job.id).await;
    let tick = gw.tick().await;
    let run = tick.fired[0].run.clone();
    let session = tick.fired[0].session.clone();
    gw.wait_status(&run, |s| s.is_terminal(), "终态").await;

    // 「更新本次触发状态」：盯梢把它记成了 ok。
    eventually("触发状态记成 ok", || true).await;
    let mut ok = false;
    for _ in 0..100 {
        let list = gw.cron_list().await;
        let last = list
            .status
            .iter()
            .find(|s| s.job == job.id)
            .and_then(|s| s.last.clone());
        if last.as_ref().map(|f| f.status) == Some(FiringStatus::Ok) {
            let last = last.unwrap();
            // 「Cron 的结果去原 Session 查看」——会话号就在这一行上。
            assert_eq!(last.session, Some(session.clone()));
            assert_eq!(last.run, Some(run.clone()));
            ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(ok, "触发状态最终应当是 ok");

    gw.stop().await;
}
