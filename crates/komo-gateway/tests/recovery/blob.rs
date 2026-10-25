//! blob 引用读不到 → 工具任务 `faulted`，列进 intervention。

use komo_gateway::service::test_support::driver_stream::{core, open_session, submit};
use komo_gateway::service::test_support::harness::{FakeLlm, call_round, text_round};
use komo_kernel::durable::SubmissionStatus;
use komo_kernel::durable::storage::TaskQuery;
use komo_kernel::durable::{TaskState, TaskStatus};
use komo_kernel::protocol::http::{
    InterventionAnswerResponse, InterventionKind, InterventionSummary,
};
use komo_kernel::types::chat::Outbound;

use crate::{sender, start_bash, until};

/// 一个在 `execute` 阶段的工具任务（重启时被打断的 `bash`），它尝试目录里的 `output.json` 被写坏了——
/// 这份"已发布的输出"不可信，而恢复会把它当成"没有结果"重判（重跑或记成 uncertain），悄悄换掉一次
/// 已经完成的尝试。Gateway 启动时（**引擎恢复之前**）的 reconcile 只查 blob 引用，发现读不到就让那个
/// 工具任务 `faulted`：
///
/// - 出现在 `/v1/interventions`（`blocked`、问题 = fault 原因、可答只有 `abandon`），也在会话详情的 `pending` 里；
/// - 投一条 NeedsAttention 到 home chat，重启之后不重复投；
/// - `resolve` 等别的结论是 422；`abandon` 记在 `komo.interventions.ack`，之后不再列出，重复答复回原结论；
/// - run 本身不被挡住，照常收尾。
#[tokio::test]
async fn a_missing_blob_faults_the_tool_task_and_lists_it() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    std::fs::create_dir_all(home.join("workspaces")).unwrap();
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-bash",
            "bash",
            serde_json::json!({ "command": "echo hi; sleep 9" }),
        ),
        text_round(2, "不该走到这里"),
    ]]);
    let mut gateway = start_bash(Some(&home), llm, &sender()).await;
    let session = open_session(&gateway).await;
    let run = submit(&gateway, &session, "blob-1", "跑一条命令").await.run;

    // 等工具任务进了 `execute`，记下它的尝试目录。
    let core1 = core(&gateway);
    let found = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen = std::sync::Arc::clone(&found);
    until("工具任务进了 execute", move || {
        let core = core1.clone();
        let seen = std::sync::Arc::clone(&seen);
        async move {
            for status in [TaskStatus::Pending, TaskStatus::Running] {
                let page = core
                    .storage
                    .scan_tasks(
                        &TaskQuery {
                            kind: Some("komo.tool".into()),
                            status: Some(status),
                            ..TaskQuery::default()
                        },
                        10,
                        None,
                    )
                    .await
                    .unwrap();
                for task in page.items {
                    if let TaskState::Pending { checkpoint } | TaskState::Running { checkpoint } =
                        &task.state
                        && checkpoint["phase"] == "execute"
                    {
                        *seen.lock().unwrap() = Some((
                            task.conversation,
                            task.id,
                            checkpoint["attempt"].as_u64().unwrap(),
                        ));
                        return true;
                    }
                }
            }
            false
        }
    })
    .await;
    let (conversation, task, attempt) = found.lock().unwrap().take().unwrap();
    let session_of = core(&gateway)
        .storage
        .conversation(conversation)
        .await
        .unwrap()
        .unwrap()
        .session
        .unwrap();
    gateway.stop().await;

    // 停机之后把那次尝试的 output.json 写坏。
    let attempt_dir = home
        .join("sessions")
        .join(&session_of)
        .join(format!("tool-output/{conversation}/{task}/{attempt}"));
    std::fs::create_dir_all(&attempt_dir).unwrap();
    std::fs::write(attempt_dir.join("output.json"), b"{ this is not json").unwrap();

    // 重启：启动时的 reconcile 在恢复之前让它 faulted；run 照常收尾。
    let second_sender = sender();
    let gateway = start_bash(
        Some(&home),
        FakeLlm::finisher("命令没跑成，我知道了。"),
        &second_sender,
    )
    .await;
    let record = gateway.wait_run(&run).await;
    assert_eq!(record.status(), SubmissionStatus::Done, "{record:?}");

    let (code, body) = gateway.get_json("/v1/interventions").await;
    assert_eq!(code, 200, "{body}");
    let listed: Vec<InterventionSummary> =
        serde_json::from_value(body["interventions"].clone()).unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    let one = &listed[0];
    assert_eq!(one.handle, task.to_string(), "句柄是 faulted 任务的 ID");
    assert_eq!(one.kind, InterventionKind::Blocked);
    assert!(one.question.contains("blob"), "问题 = fault 原因：{one:?}");
    assert!(one.question.contains("output.json"), "{one:?}");
    assert_eq!(
        one.verdicts,
        vec![komo_kernel::protocol::http::InterventionVerdict::Abandon],
        "只能答 abandon"
    );
    let (code, detail) = gateway.get_json(&format!("/v1/sessions/{session}")).await;
    assert_eq!(code, 200, "{detail}");
    assert_eq!(
        detail["pending"].as_array().map(Vec::len),
        Some(1),
        "{detail}"
    );

    // NeedsAttention 投到 home chat。
    let watching = std::sync::Arc::clone(&second_sender);
    until("NeedsAttention 投到了 home", || {
        let watching = std::sync::Arc::clone(&watching);
        async move {
            watching
                .sent()
                .iter()
                .any(|m| matches!(&m.outbound, Outbound::NeedsAttention { reason, .. } if reason.contains("blob")))
        }
    })
    .await;

    // 答复：别的结论是 422；abandon 之后不再列出，重复答复回原结论。
    let (code, body) = gateway.answer_raw(&task.to_string(), "resolve").await;
    assert_eq!(code, 422, "{body}");
    let answered: InterventionAnswerResponse =
        serde_json::from_str(&gateway.answer(&task.to_string(), "abandon").await).unwrap();
    assert!(!answered.already_answered, "{answered:?}");
    let (_, body) = gateway.get_json("/v1/interventions").await;
    assert_eq!(
        body["interventions"].as_array().map(Vec::len),
        Some(0),
        "{body}"
    );
    let again: InterventionAnswerResponse =
        serde_json::from_str(&gateway.answer(&task.to_string(), "abandon").await).unwrap();
    assert!(again.already_answered, "{again:?}");
    let (code, _) = gateway.answer_raw("999999999", "abandon").await;
    assert_eq!(code, 404, "不存在的句柄");
    let (_, detail) = gateway.get_json(&format!("/v1/sessions/{session}")).await;
    assert_eq!(
        detail["pending"].as_array().map(Vec::len),
        Some(0),
        "{detail}"
    );

    // 再重启一次：已确认的不再列出，也不再重复提醒。
    let mut gateway = gateway;
    gateway.stop().await;
    let third_sender = sender();
    let gateway = start_bash(Some(&home), FakeLlm::finisher("好"), &third_sender).await;
    let (_, body) = gateway.get_json("/v1/interventions").await;
    assert_eq!(
        body["interventions"].as_array().map(Vec::len),
        Some(0),
        "{body}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        third_sender
            .sent()
            .iter()
            .all(|m| !matches!(&m.outbound, Outbound::NeedsAttention { .. })),
        "{:?}",
        third_sender.sent()
    );
}
