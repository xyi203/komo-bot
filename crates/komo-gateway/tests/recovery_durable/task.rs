//! 后台任务跨重启，结果仍交回 home。

use std::time::Duration;

use komo_gateway::service::test_support::Gate;
use komo_gateway::service::test_support::driver_stream::{core, submit, wait_keyed};
use komo_gateway::service::test_support::harness::{FakeLlm, call_round, text_round};
use komo_kernel::durable::SubmissionStatus;

use crate::{sender, start};

/// home 会话派出一个后台任务；任务停在 `gate` 上时重启。重启后任务接着跑完（没结果的那次 `gate` 调用落成
/// `uncertain`），结果由 runtime 在子 run 收尾的同一次提交里交回 home（请求键 `task-result:…` 的内部输入），
/// home 再起一个 run 转告——不靠任何客户端、任何看客，重启后没人订阅也照样交回。
#[tokio::test]
async fn a_background_task_in_flight_across_a_restart_still_reports_back() {
    let gate = Gate::new();
    let llm = FakeLlm::new(vec![
        // home：派任务后收尾。
        vec![
            call_round(
                1,
                "pc-task",
                "task",
                serde_json::json!({"prompt": "跑一下 echo", "title": "echo"}),
            ),
            text_round(2, "已派出。"),
        ],
        // 任务：调 gate，停住。
        vec![
            call_round(1, "pc-gate", "gate", serde_json::json!({})),
            text_round(2, "任务跑完了。"),
        ],
    ]);
    let mut gateway = start(None, llm, vec![gate.tool()], &sender()).await;
    let agent = gateway.state().snapshot().agent.default_agent.clone();
    let (home_session, _) = core(&gateway)
        .main_session(&agent)
        .await
        .expect("home 会话");
    let accepted = submit(&gateway, &home_session.id, "tui-1", "派个任务跑 echo").await;
    let record = gateway.wait_run(&accepted.run).await;
    assert_eq!(record.status(), SubmissionStatus::Done, "home 这一轮收尾了");
    gate.wait_entered(1).await;
    gateway.stop().await;

    let home = gateway.home.clone();
    let gateway = start(
        Some(&home),
        FakeLlm::finisher("转告：任务跑完了。"),
        vec![],
        &sender(),
    )
    .await;
    let report = wait_keyed(&gateway, "task-result:", Duration::from_secs(30)).await;
    assert_eq!(
        report.status(),
        SubmissionStatus::Done,
        "任务的结果交回 home，home 转告完了：{report:?}"
    );
}
