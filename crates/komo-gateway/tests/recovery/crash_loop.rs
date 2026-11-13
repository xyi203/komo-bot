//! 崩溃循环保护（`docs/storage-durable-perf.md` §6）：连续被崩溃打断到上限的任务停住，列进
//! intervention（`blocked`，`resolve` 放行 / `abandon` 放弃）。
//!
//! 测试进程里杀不掉 Gateway 又在同一个目录重开（Turso 的文件锁还在旧实例手里），所以"前几次崩溃"是
//! 停机之后直接改库造出来的：在飞工具任务的 `restarts` 记成上限减一、引擎标记改成"没有干净关闭"——
//! 正是又一次崩溃之后库里的样子。

use std::path::Path;
use std::sync::Arc;

use komo_gateway::service::test_support::driver_stream::{core, open_session, submit};
use komo_gateway::service::test_support::harness::{FakeLlm, TestGateway, call_round, text_round};
use komo_kernel::durable::storage::TaskQuery;
use komo_kernel::durable::task::RESTART_LIMIT;
use komo_kernel::durable::{
    Storage, StorageWrite, SubmissionStatus, TaskId, TaskState, TaskStatus,
};
use komo_kernel::protocol::http::{
    InterventionAnswerResponse, InterventionKind, InterventionSummary, InterventionVerdict,
};
use komo_kernel::types::ids::RunId;
use komo_runtime::durable::engine::{engine_address, engine_state};

use crate::{start_bash, until};

/// 起一台、让一条 `bash` 停在 `execute`、停机，再把库改成"它已经被崩溃打断了上限减一次、上一代又崩了"，
/// 然后重启。返回重启后的 Gateway、那条 run、那个工具任务与会话。
async fn restart_after_crashes(home: &Path) -> (TestGateway, RunId, TaskId, String) {
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-bash",
            "bash",
            serde_json::json!({ "command": "echo hi; sleep 9" }),
        ),
        text_round(2, "不该走到这里"),
    ]]);
    let mut gateway = start_bash(Some(home), llm).await;
    let session = open_session(&gateway).await;
    let run = submit(&gateway, &session, "loop-1", "跑一条命令").await.run;
    let core1 = core(&gateway);
    let found = Arc::new(std::sync::Mutex::new(None));
    let seen = Arc::clone(&found);
    until("工具任务进了 execute", move || {
        let core = core1.clone();
        let seen = Arc::clone(&seen);
        async move {
            let page = core
                .storage
                .scan_tasks(
                    &TaskQuery {
                        kind: Some("komo.tool".into()),
                        status: Some(TaskStatus::Running),
                        ..TaskQuery::default()
                    },
                    10,
                    None,
                )
                .await
                .unwrap();
            for task in page.items {
                if let TaskState::Running { checkpoint } = &task.state
                    && checkpoint["phase"] == "execute"
                {
                    *seen.lock().unwrap() = Some(task.id);
                    return true;
                }
            }
            false
        }
    })
    .await;
    let task = found.lock().unwrap().take().unwrap();
    gateway.stop().await;

    let storage = komo_store::durable::TursoStorage::open(home).await.unwrap();
    let mut record = storage.task(task).await.unwrap().unwrap();
    assert!(
        matches!(record.state, TaskState::Running { .. }),
        "干净停机留下的是 running：{record:?}"
    );
    assert_eq!(record.restarts, 0, "干净停机不算崩溃");
    record.restarts = RESTART_LIMIT - 1;
    let marker = storage
        .find_document(&engine_address())
        .await
        .unwrap()
        .expect("开过一次库就有引擎标记");
    assert_eq!(marker.value, engine_state(true), "干净停机写了标记");
    storage
        .commit(&[
            StorageWrite::Task(record),
            StorageWrite::DocumentChange {
                id: marker.id,
                version: marker.version,
                value: engine_state(false),
            },
        ])
        .await
        .unwrap();
    storage.close().await.unwrap();

    let gateway = start_bash(Some(home), FakeLlm::finisher("命令被打断了，我知道了。")).await;
    (gateway, run, task, session)
}

async fn listed(gateway: &TestGateway) -> Vec<InterventionSummary> {
    let (code, body) = gateway.get_json("/v1/interventions").await;
    assert_eq!(code, 200, "{body}");
    serde_json::from_value(body["interventions"].clone()).unwrap()
}

#[tokio::test]
async fn a_task_stopped_by_the_crash_loop_guard_is_listed_and_can_be_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    std::fs::create_dir_all(home.join("workspaces")).unwrap();
    let (gateway, run, task, session) = restart_after_crashes(&home).await;

    // 停住：列进清单（blocked、两种结论），会话详情的 pending 里也有；run 没有收尾。
    let all = listed(&gateway).await;
    assert_eq!(all.len(), 1, "{all:?}");
    let one = &all[0];
    assert_eq!(one.handle, task.to_string());
    assert_eq!(one.kind, InterventionKind::Blocked);
    assert!(
        one.question.contains(&format!("连续 {RESTART_LIMIT} 次")),
        "{one:?}"
    );
    assert_eq!(
        one.verdicts,
        vec![InterventionVerdict::Resolve, InterventionVerdict::Abandon]
    );
    let (_, detail) = gateway.get_json(&format!("/v1/sessions/{session}")).await;
    assert_eq!(
        detail["pending"].as_array().map(Vec::len),
        Some(1),
        "{detail}"
    );
    let (code, shown) = gateway.get_json(&format!("/v1/interventions/{task}")).await;
    assert_eq!(code, 200, "{shown}");
    let stored = core(&gateway).storage.task(task).await.unwrap().unwrap();
    assert_eq!(stored.state.status(), TaskStatus::Pending, "没被再起");

    // 别的结论 422；resolve 放行：计数清零，任务照常恢复（bash 不重跑，uncertain 交给模型），run 收尾。
    let (code, body) = gateway.answer_raw(&task.to_string(), "approve").await;
    assert_eq!(code, 422, "{body}");
    let answered: InterventionAnswerResponse =
        serde_json::from_str(&gateway.answer(&task.to_string(), "resolve").await).unwrap();
    assert_eq!(answered.verdict, InterventionVerdict::Resolve);
    let record = gateway.wait_run(&run).await;
    assert_eq!(record.status(), SubmissionStatus::Done, "{record:?}");
    let settled = core(&gateway).storage.task(task).await.unwrap().unwrap();
    assert_eq!(settled.state.status(), TaskStatus::Terminal);
    assert!(listed(&gateway).await.is_empty());
}

#[tokio::test]
async fn abandoning_a_stopped_task_faults_it_without_listing_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    std::fs::create_dir_all(home.join("workspaces")).unwrap();
    let (mut gateway, run, task, _session) = restart_after_crashes(&home).await;
    assert_eq!(listed(&gateway).await.len(), 1);

    let answered: InterventionAnswerResponse =
        serde_json::from_str(&gateway.answer(&task.to_string(), "abandon").await).unwrap();
    assert_eq!(answered.verdict, InterventionVerdict::Abandon);
    assert!(!answered.already_answered);
    let record = gateway.wait_run(&run).await;
    assert_eq!(record.status(), SubmissionStatus::Done, "{record:?}");
    let settled = core(&gateway).storage.task(task).await.unwrap().unwrap();
    assert!(
        matches!(&settled.state, TaskState::Terminal { outcome: komo_kernel::durable::TaskOutcome::Faulted { error } } if error.message.contains("放弃")),
        "{settled:?}"
    );
    // faulted 之前先记了 ack：不作为 faulted 再列一次；再答一次回原结论。
    assert!(listed(&gateway).await.is_empty());
    let again: InterventionAnswerResponse =
        serde_json::from_str(&gateway.answer(&task.to_string(), "abandon").await).unwrap();
    assert!(again.already_answered, "{again:?}");

    // 干净重启：什么都不再列。
    gateway.stop().await;
    let gateway = start_bash(Some(&home), FakeLlm::finisher("好")).await;
    assert!(listed(&gateway).await.is_empty());
}
