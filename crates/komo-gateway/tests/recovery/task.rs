//! 后台任务跨重启，结果仍交回 home；任务被停下时丢弃排队与晚到的监听事件。

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

/// 任务被停下（`DurableCore::stop_task`）：当前一轮取消、排队的监听事件撤回、`komo.task` 标成已停，
/// 之后晚到的监听事件在受理处就被丢掉（`unanswered{stopped}`）、不再起轮次；任务清单里它是"没有完成"。
/// 不涉及重启，放在这里是因为它与任务交回共用同一套 `komo.task` 与收件箱。
#[tokio::test]
async fn a_stopped_task_drops_queued_and_late_monitor_events() {
    use komo_kernel::durable::storage::SessionQuery;
    use komo_kernel::durable::{InputState, SessionKind, SubmissionKind};
    use komo_kernel::protocol::http::TaskOutcome;
    use komo_runtime::durable::harness::orchestration::monitor;

    let gate = Gate::new();
    let llm = FakeLlm::new(vec![
        vec![
            call_round(
                1,
                "st-task",
                "task",
                serde_json::json!({"prompt": "跑个脚本", "title": "脚本"}),
            ),
            text_round(2, "已派出。"),
        ],
        vec![
            call_round(1, "st-gate", "gate", serde_json::json!({})),
            text_round(2, "跑完了。\n状态：完成"),
        ],
    ]);
    let gateway = start(None, llm, vec![gate.tool()], &sender()).await;
    let core = core(&gateway);
    let agent = gateway.state().snapshot().agent.default_agent.clone();
    let (home_session, _) = core.main_session(&agent).await.expect("home 会话");
    let accepted = submit(&gateway, &home_session.id, "st-1", "派个任务").await;
    gateway.wait_run(&accepted.run).await;
    gate.wait_entered(1).await;
    let task = core
        .storage
        .scan_sessions(
            &SessionQuery {
                kind: Some(SessionKind::Task),
                ..Default::default()
            },
            10,
            None,
        )
        .await
        .unwrap()
        .items
        .remove(0);
    let settled = |id| {
        let core = core.clone();
        async move {
            core.storage
                .submission(id)
                .await
                .unwrap()
                .expect("submission")
                .kind
        }
    };

    // 任务在忙：监听事件排进收件箱（内部输入，取消本来会保留它）。
    let queued = core
        .harness
        .submit(
            &core.engine,
            task.current_branch,
            monitor::event_draft(7, "盯", "1:1", "[监听 #x「盯」]\n一行"),
        )
        .await
        .unwrap();
    assert!(matches!(
        settled(queued).await,
        SubmissionKind::Input {
            state: InputState::Queued
        }
    ));

    core.stop_task(task.root_conversation).await.unwrap();

    assert!(
        matches!(
            settled(queued).await,
            SubmissionKind::Input {
                state: InputState::Unanswered { .. }
            }
        ),
        "排队的事件该被撤回"
    );
    let doc = komo_runtime::durable::harness::task_doc(&*core.storage, task.root_conversation)
        .await
        .unwrap()
        .expect("komo.task");
    assert!(doc.stopped);
    let late = core
        .harness
        .submit(
            &core.engine,
            task.current_branch,
            monitor::event_draft(7, "盯", "1:2", "[监听 #x「盯」]\n又一行"),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            settled(late).await,
            SubmissionKind::Input {
                state: InputState::Unanswered { ref reason, .. }
            } if reason == "stopped"
        ),
        "晚到的事件该被丢掉"
    );
    let row = core
        .task_page(false, None, None)
        .await
        .unwrap()
        .tasks
        .into_iter()
        .find(|row| row.session.as_str() == task.id)
        .expect("任务清单里有它");
    assert_eq!(row.outcome, Some(TaskOutcome::NotCompleted));
    gate.release();
}
