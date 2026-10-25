//! 在飞的 `bash` 跨重启：不重跑，`uncertain` 交给模型。

use komo_gateway::service::test_support::driver_stream::{open_session, submit};
use komo_gateway::service::test_support::harness::{FakeLlm, call_round, text_round};
use komo_kernel::durable::SubmissionStatus;

use crate::{entries_json, sender, start_bash, until};

/// 一条 `bash` 命令跑到一半 Gateway 重启：**执行器不把命令再跑一遍**（计数文件只有一行），那次调用落成
/// `uncertain`——正文说明"被 Gateway 重启打断、副作用是否已经发生未知"，并带上中断前已落盘的部分输出——
/// 交给模型判断，run 照常收尾，不停下来等人。
#[tokio::test]
async fn a_killed_bash_is_handed_to_the_model_as_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    std::fs::create_dir_all(home.join("workspaces")).unwrap();
    let counter = home.join("workspaces").join("counter.txt");
    let command = format!(
        "echo ran >> {}; echo partial-out; sleep 9",
        counter.display()
    );
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-bash",
            "bash",
            serde_json::json!({ "command": command }),
        ),
        text_round(2, "不该走到这里"),
    ]]);
    let mut gateway = start_bash(Some(&home), llm, &sender()).await;
    let session = open_session(&gateway).await;
    let run = submit(&gateway, &session, "kill-1", "跑一条慢命令")
        .await
        .run;
    let started = counter.clone();
    until("命令起来了", move || {
        let started = started.clone();
        async move { started.exists() }
    })
    .await;
    // 让 stdout 的头一行落盘（尾巴节流写）。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    gateway.stop().await;

    let llm = FakeLlm::finisher("核对过了，不重做。");
    let gateway = start_bash(Some(&home), llm, &sender()).await;
    let record = gateway.wait_run(&run).await;
    assert_eq!(
        record.status(),
        SubmissionStatus::Done,
        "模型接着答完，run 正常收尾，不停下来等人：{record:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&counter).unwrap().lines().count(),
        1,
        "**执行器没有自动重跑**"
    );
    let entries = entries_json(&gateway, &session).await;
    assert!(
        entries.contains("\"status\":\"uncertain\""),
        "那次调用记成 uncertain：{entries}"
    );
    assert!(
        entries.contains("重启打断"),
        "正文说清楚是被重启打断的：{entries}"
    );
    assert!(
        entries.contains("partial-out"),
        "中断前落盘的部分输出带给模型：{entries}"
    );
}
