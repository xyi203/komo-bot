//! §14「首个端到端验收」（Durable 版）。

use komo_gateway::service::test_support::Gate;
use komo_gateway::service::test_support::driver_stream::{open_session, submit};
use komo_gateway::service::test_support::harness::{
    FakeLlm, call_round, multi_call_round, text_round,
};
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::status::RunState;

use crate::{entries_json, sender, start, until};

/// 启动 komo 自动拉起 Gateway，模型调用 `write` 生成工作文件，在任务的后续步骤前重启 Gateway；
/// **不打开 CLI 也自动接续原 run**，随后 resume 原会话查看结果，再读出原文件。
///
/// "后续步骤前"：一轮里同时调 `write` 与 `gate`，`write` 做完、`gate` 停着时重启。没有结果的那次 `gate`
/// 调用重启后落成 `uncertain`（不重跑），模型接着答完。"要人核对的调用"在新存储里没有停等（`WaitReason`
/// 不再产生）——那一半由 `a_missing_blob_faults_the_tool_task_and_lists_it` 钉住；"结果丢失 → 不自动重跑"
/// 由 `a_killed_bash_is_handed_to_the_model_as_interrupted` 钉住。
#[tokio::test]
async fn the_first_end_to_end_acceptance() {
    let gate = Gate::new();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let made = home.join("workspaces").join("report.md");

    // ── 1. 模型调用 write 生成工作文件，gate 把这一轮停在"后续步骤"之前，然后重启。
    let llm = FakeLlm::new(vec![vec![
        multi_call_round(
            1,
            &[
                (
                    "pc-write",
                    "write",
                    serde_json::json!({
                        "path": made.display().to_string(),
                        "content": "# 报告\n第一步做完了。\n"
                    }),
                ),
                ("pc-gate", "gate", serde_json::json!({})),
            ],
        ),
        text_round(2, "报告写好了。"),
    ]]);
    std::fs::create_dir_all(home.join("workspaces")).unwrap();
    let mut gateway = start(Some(&home), llm, vec![gate.tool()], &sender()).await;
    let session = open_session(&gateway).await;
    let run = submit(&gateway, &session, "e2e-1", "写一份报告").await.run;
    gate.wait_entered(1).await;
    let wrote = made.clone();
    until("write 落下了文件", move || {
        let wrote = wrote.clone();
        async move { wrote.exists() }
    })
    .await;
    gateway.stop().await;
    assert!(made.exists(), "工作文件已经生成");

    // ── 2. 重启。**不打开任何客户端**，Gateway 自己把原 run 接着跑完（没有 gate 工具了：那次调用没有结果，
    //    落成 uncertain）。
    let llm = FakeLlm::new(vec![
        vec![text_round(1, "报告写好了。")],
        vec![
            call_round(
                1,
                "pc-read",
                "read",
                serde_json::json!({"path": made.display().to_string()}),
            ),
            text_round(2, "读到了。"),
        ],
    ]);
    let gateway = start(Some(&home), llm, vec![], &sender()).await;
    // 只看存储，不发任何 HTTP：这就是"不打开 CLI"。
    let record = gateway.wait_run(&run).await;
    assert_eq!(
        record.status(),
        komo_kernel::durable::SubmissionStatus::Done,
        "自动接续并跑完了原 run：{record:?}"
    );
    let entries = entries_json(&gateway, &session).await;
    assert_eq!(
        entries.matches("\"tool\":\"write\"").count(),
        1,
        "接续不重放已经完成的 write"
    );
    assert_eq!(
        std::fs::read_to_string(&made).unwrap(),
        "# 报告\n第一步做完了。\n",
        "文件还是原来那一份"
    );

    // ── 3. 现在才 resume 原会话看结果——走 CLI 那条路（`komo resume` 用的就是 `KomoClient`）。
    let base = gateway.base_url().to_owned();
    let client =
        komo_client::KomoClient::new(&base, Some(gateway.token().to_owned())).expect("客户端");
    let session_id = SessionId::from_raw(session.clone());
    let resumed = client
        .resume(
            &session_id,
            &komo_kernel::protocol::http::ResumeRequest::default(),
        )
        .await
        .expect("resume 得动");
    assert_eq!(resumed.session, session_id);
    assert!(resumed.resumed.is_empty(), "没有没结束的 run：{resumed:?}");
    assert!(resumed.pending.is_empty(), "没有待处理的：{resumed:?}");
    let detail = client.run(&run).await.expect("读得到运行详情");
    assert_eq!(detail.summary.state, RunState::Completed);
    assert_eq!(detail.final_message.as_deref(), Some("报告写好了。"));

    // ── 4. 再让模型 `read` 读出原文件——文件真的在，内容真的是那一份。
    let read_run: RunId = submit(&gateway, &session, "e2e-2", "把那份报告读出来")
        .await
        .run;
    let record = gateway.wait_run(&read_run).await;
    assert_eq!(
        record.status(),
        komo_kernel::durable::SubmissionStatus::Done
    );
    assert!(
        entries_json(&gateway, &session)
            .await
            .contains("第一步做完了"),
        "read 读回来的就是原文件"
    );
}
