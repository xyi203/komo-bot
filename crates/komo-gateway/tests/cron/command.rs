//! 命令直跑模式（§10）：不经模型，触发时固定跑 Job 的 `command`。走真 Gateway——
//! 真数据目录、真 state.db，`LlmClient` 是脚本模型，用
//! [`komo_gateway::service::test_support::harness::FakeLlm::turns`] 断言它一次都
//! 没被问过。命令 Job 不再签授权（§7 删掉审批之后，触发时那次 `bash` 直接执行）。

use std::sync::Arc;

use crate::harness::*;

fn command_job(name: &str, command: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "schedule": "0 9 * * *",
        "timezone": "UTC",
        "prompt": "",
        "command": command,
    })
}

/// 退出 0、stdout 非空：`ok`，投递的正文是 trim 过的 stdout **原样**——没有
/// `CommandDriver` 收尾那份投影正文的头（`exit=... stdout=...`），也没经过模型。
#[tokio::test]
async fn a_command_job_never_asks_the_model_and_delivers_trimmed_stdout() {
    let home = Home::new();
    // 空脚本：`begin_turn` 一旦真的被调用，`FakeDriver` 会给一句收尾文本而不是报错
    // （见 `FakeLlm` 的注释），所以断言靠 `llm.turns()`，不是"没崩"。
    let llm = FakeLlm::new(vec![]);
    let gw = home.start(Arc::clone(&llm)).await;

    let job = gw.add_job(command_job("say-hi", "echo '  hi  '")).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;

    assert_eq!(llm.turns(), 0, "命令 Run 全程零模型请求（§10）");

    let sender = Arc::clone(&gw.home);
    eventually("home chat 收到 trim 过的 stdout", move || {
        sender
            .texts()
            .iter()
            .any(|t| t.contains("hi") && !t.contains("stdout="))
    })
    .await;

    let firings = gw.firings(&job.id).await;
    assert_eq!(
        firings.first().map(|f| f.status),
        Some(komo_kernel::cron::FiringStatus::Ok)
    );
    gw.stop().await;
}

/// 退出 0、stdout 为空：`ok`，但**不投递**——看门狗"没事不说话"（§10）。
#[tokio::test]
async fn a_command_job_with_empty_stdout_settles_ok_but_delivers_nothing() {
    let home = Home::new();
    let llm = FakeLlm::new(vec![]);
    let gw = home.start(Arc::clone(&llm)).await;

    let job = gw.add_job(command_job("silent", "true")).await;
    let before = gw.home.texts().len();
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;

    assert_eq!(llm.turns(), 0);

    // 触发状态照样回写为 ok——它确实跑成了，只是没有输出。
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

    assert_eq!(
        gw.home.texts().len(),
        before,
        "空输出不该多出一条投递：{:?}",
        gw.home.texts()
    );
    gw.stop().await;
}

/// 非 0 退出：`error`，投递退出码 + stderr 末尾若干行，受 `notify` 过滤（默认
/// `always`，投）。
#[tokio::test]
async fn a_non_zero_exit_delivers_an_error_summary() {
    let home = Home::new();
    let llm = FakeLlm::new(vec![]);
    let gw = home.start(Arc::clone(&llm)).await;

    let job = gw
        .add_job(command_job("boom", "echo went-wrong >&2; exit 3"))
        .await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;

    assert_eq!(llm.turns(), 0);

    let sender = Arc::clone(&gw.home);
    eventually("错误摘要带着退出码与 stderr", move || {
        sender
            .texts()
            .iter()
            .any(|t| t.contains("exit=3") && t.contains("went-wrong"))
    })
    .await;

    let firings = gw.firings(&job.id).await;
    assert_eq!(
        firings.first().map(|f| f.status),
        Some(komo_kernel::cron::FiringStatus::Error)
    );
    gw.stop().await;
}

/// 失败的触发：一个 `komo.cron.firing` span 带 job 与结果，失败计数按 job 记一次；
/// 指标里没有 Run / 会话 id。
#[tokio::test]
async fn a_failed_firing_emits_its_span_and_counts_a_failure() {
    let capture = komo_runtime::telemetry::capture::Capture::default();
    let _guard = capture.install();
    let home = Home::new();
    let gw = home.start(FakeLlm::new(vec![])).await;

    let job = gw.add_job(command_job("boom", "exit 3")).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
    eventually("失败的触发有了 span 结果", || {
        capture
            .spans("komo.cron.firing")
            .iter()
            .any(|span| span.fields.get("komo.cron.outcome").map(String::as_str) == Some("error"))
    })
    .await;

    let span = &capture.spans("komo.cron.firing")[0];
    assert_eq!(span.fields["komo.cron.job"], job.id.to_string());
    let failures = capture.metrics("monotonic_counter.komo.cron.failures");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].fields["komo.cron.job"], job.id.to_string());
    for event in capture.all_metrics() {
        // Durable 的 RunId 是十进制 submission ID，很短，按子串找会误撞 Job UUID 里的数字；
        // 改成按字段：值里不能有这个 Run，键里不能有 run / session。
        for (key, value) in &event.fields {
            assert!(
                !key.contains("run") && !key.contains("session") && value != run.as_str(),
                "指标里不该有 Run / 会话 id：{key}={value}"
            );
        }
    }
    gw.stop().await;
}

/// `komo cron add --command` 与 `--prompt` 二选一：都给或都不给，HTTP 层当场拒绝，
/// 不创建任何 Job（§10）。
#[tokio::test]
async fn prompt_and_command_are_mutually_exclusive() {
    let home = Home::new();
    let gw = home.start(FakeLlm::new(vec![])).await;

    let (status, body) = gw
        .post(
            "/v1/cron",
            serde_json::json!({
                "name": "both",
                "schedule": "0 9 * * *",
                "timezone": "UTC",
                "prompt": "整理今天的动态",
                "command": "echo hi",
            }),
        )
        .await;
    assert_eq!(status, 400, "{body}");

    let (status, body) = gw
        .post(
            "/v1/cron",
            serde_json::json!({
                "name": "neither",
                "schedule": "0 9 * * *",
                "timezone": "UTC",
                "prompt": "",
            }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(gw.cron_list().await.jobs.is_empty(), "一个 Job 都没留下");
    gw.stop().await;
}
