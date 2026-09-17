//! 无人值守的审批（§10、§7.4、§11.2 最后一条、§14 阶段 7 第二、三句）。

use std::sync::Arc;

use komo_kernel::traits::LlmClient;
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::status::RunStatus;

use crate::harness::*;

fn daily() -> serde_json::Value {
    serde_json::json!({
        "name": "morning-summary",
        "schedule": "0 9 * * *",
        "timezone": "Asia/Shanghai",
        "prompt": "整理今天的技术动态",
    })
}

/// **§14 阶段 7 第二句：新危险操作等待审批。**
///
/// 「不能因无人值守而自动放行」（§10）——所以一个 Cron Run 里的任意 shell 停在
/// `waiting_approval` 上，而且**那条请求要到得了人手里**：Cron 没有来源会话，
/// §7.4 的"投递到 Run 的来源会话与 home chat"在这一侧只剩 home chat。
#[tokio::test]
async fn a_dangerous_action_waits_and_the_request_reaches_home_chat() {
    let home = Home::new();
    let counter = home.workspace().join("ran.log");
    let llm = FakeLlm::always(vec![shell_append(1, "pc-1", &counter, "one")]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let job = gw.add_job(daily()).await;
    gw.make_due(&job.id).await;
    let tick = gw.tick().await;
    let run = tick.fired[0].run.clone();

    // ① 停下来等人，**不自动放行**。
    gw.wait_status(&run, |s| s == RunStatus::WaitingApproval, "等待审批")
        .await;
    assert_eq!(lines(&counter), 0, "没批准就一次都没跑");

    // ② 请求到了 home chat。
    let sender = Arc::clone(&gw.home);
    eventually("home chat 收到审批请求", move || {
        !sender.approvals().is_empty()
    })
    .await;
    let presented = gw.home.approvals();
    assert_eq!(presented.len(), 1, "{presented:?}");
    assert_eq!(presented[0].plan.tool, "shell");
    assert!(
        matches!(
            presented[0].plan.source,
            komo_kernel::types::plan::PlanSource::Cron { .. }
        ),
        "界面上显示的就是那份 Cron 来源的计划"
    );

    // ③ 触发记录记成 `waiting`——既不是跑成也不是失败（§10）。
    let mut waiting = false;
    for _ in 0..100 {
        if gw
            .firings(&job.id)
            .await
            .first()
            .map(|f| f.status)
            .is_some_and(|s| s == komo_kernel::cron::FiringStatus::Waiting)
        {
            waiting = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(waiting, "触发状态应当是 waiting");

    // ④ 这条 Ask 给出的范围里有 Cron Job——§7.2 的第三种（Once 永远在）。
    let record = gw.wait_approval().await;
    assert!(record.scopes.contains(&ApprovalScope::Once));
    assert!(
        record.scopes.contains(&ApprovalScope::CronJob),
        "Cron 来源的计划可以批到 Job 范围：{:?}",
        record.scopes
    );

    gw.stop().await;
}

/// **§14 阶段 7 第三句：`/approve` 之后按 Cron 权限继续，不升权。**
///
/// 在聊天里批准一次，是**这一次调用**的批准（§7.2 第一种）。它不会让这个 Job 从此
/// 拥有"操作者在场"的权限：下一次同类动作照样要问。
#[tokio::test]
async fn approving_once_does_not_hand_the_job_the_operators_powers() {
    let home = Home::new();
    let counter = home.workspace().join("ran.log");
    // 每一段脚本都是"跑一条 shell 然后收尾"。续跑的那一段会再要一次同类动作——
    // 这正是"后续同类动作还要不要问"的那一次。
    let llm = FakeLlm::new(vec![
        vec![
            shell_append(1, "pc-1", &counter, "one"),
            shell_append(2, "pc-2", &counter, "two"),
            text_round(3, "都做完了"),
        ],
        vec![
            shell_append(1, "pc-2", &counter, "two"),
            text_round(2, "都做完了"),
        ],
    ]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let job = gw.add_job(daily()).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_status(&run, |s| s == RunStatus::WaitingApproval, "第一次等待")
        .await;

    let first = gw.wait_approval().await;
    gw.approve_in_chat(first.short_id.as_str(), "").await;

    // 第一条跑了；**第二条同类动作又停下来问**——一次批准只是一次批准。
    gw.wait_status(&run, |s| s == RunStatus::WaitingApproval, "第二次等待")
        .await;
    let second = gw.wait_approval().await;
    assert_ne!(second.approval, first.approval, "是新的一条请求");
    assert_eq!(lines(&counter), 1, "只跑了被批准的那一条");

    // 而这个 Job 名下**一条范围授权都没有**：`Once` 从不产生授权。
    let grants = gw
        .state()
        .approval_repo
        .grants_for_job(&job.id, 1, time::OffsetDateTime::now_utc())
        .await
        .expect("读得出");
    assert!(grants.is_empty(), "没升权：{grants:?}");

    gw.stop().await;
}

/// `/approve <id> cron` 才给 Job 范围授权——而它**绑这个 Job 的这一版加这条命令**
/// （§7.2「Cron Job 授权：绑定 Job 版本、工具或模块版本、参数范围和权限」）。
///
/// TODO(decide: 文档 §11.3 的命令表只写了 `run`。`cron` 这个词按 `run` 的形状实现，
/// 三个渠道的渲染里已经这么写了，等文档收口。)
#[tokio::test]
async fn approving_with_cron_scope_binds_the_job_and_its_version() {
    let home = Home::new();
    let counter = home.workspace().join("ran.log");
    // 与 `approving_once_…` **同一份脚本**，只有批准的范围不同——两条测试的差别因此
    // 只剩那一个词。同一条命令要两次：第二次应当**不再问**（授权覆盖得到它）。
    let llm = FakeLlm::new(vec![
        vec![
            shell_append(1, "pc-1", &counter, "one"),
            shell_append(2, "pc-2", &counter, "one"),
            text_round(3, "都做完了"),
        ],
        vec![
            shell_append(1, "pc-2", &counter, "one"),
            text_round(2, "都做完了"),
        ],
    ]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let job = gw.add_job(daily()).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_status(&run, |s| s == RunStatus::WaitingApproval, "等待审批")
        .await;

    let record = gw.wait_approval().await;
    gw.approve_in_chat(record.short_id.as_str(), "cron").await;

    // 授权真的写下来了，而且是 Cron Job 范围。
    let grants = gw
        .state()
        .approval_repo
        .grants_for_job(&job.id, job.version, time::OffsetDateTime::now_utc())
        .await
        .expect("读得出");
    assert_eq!(grants.len(), 1, "{grants:?}");
    assert!(matches!(
        grants[0].scope,
        komo_kernel::policy::GrantScope::CronJob { .. }
    ));

    // 同一条命令的第二次不再问：这个 Run 跑到终态，两次副作用都发生了，而**待处理
    // 审批一条都没有**——没有第二次提问。
    gw.wait_status(&run, |s| s.is_terminal(), "终态").await;
    assert_eq!(lines(&counter), 2, "同一条命令的第二次不再问");
    assert!(
        gw.approvals().await.is_empty(),
        "不该有第二条待处理审批：{:?}",
        gw.approvals().await
    );

    // **改定义之后旧授权失效**（§10）：版本 +1，`grants_for_job` 按新版本查不到它。
    let (status, body) = gw
        .patch(
            &format!("/v1/cron/{}", job.id),
            serde_json::json!({"prompt": "换一句话"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let edited: komo_kernel::cron::CronJob = serde_json::from_str(&body).unwrap();
    assert_eq!(edited.version, job.version + 1);
    let after = gw
        .state()
        .approval_repo
        .grants_for_job(&edited.id, edited.version, time::OffsetDateTime::now_utc())
        .await
        .expect("读得出");
    assert!(after.is_empty(), "Job 改了，旧授权失效：{after:?}");

    gw.stop().await;
}

/// 一条 Cron 的授权**不漏进交互 Run**，反过来也一样（§11.2 最后一条）。
#[tokio::test]
async fn a_cron_grant_never_reaches_an_interactive_run() {
    let home = Home::new();
    let counter = home.workspace().join("ran.log");
    let llm = FakeLlm::always(vec![shell_append(1, "pc-1", &counter, "one")]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    // Cron 那一侧：批到 Job 范围。
    let job = gw.add_job(daily()).await;
    gw.make_due(&job.id).await;
    let cron_run = gw.tick().await.fired[0].run.clone();
    gw.wait_status(&cron_run, |s| s == RunStatus::WaitingApproval, "等待审批")
        .await;
    let record = gw.wait_approval().await;
    gw.approve_in_chat(record.short_id.as_str(), "cron").await;
    gw.wait_status(&cron_run, |s| s.is_terminal(), "终态").await;
    let after_cron = lines(&counter);

    // 交互那一侧：同一条命令，照样要问。
    let (status, body) = gw.post("/v1/sessions", serde_json::json!({})).await;
    assert_eq!(status, 200, "{body}");
    let summary: komo_kernel::protocol::http::SessionSummary = serde_json::from_str(&body).unwrap();
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{}/runs", summary.session),
            serde_json::json!({"request_key": "chat-1", "text": "跑一下"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let submitted: komo_kernel::protocol::http::SubmitRunResponse =
        serde_json::from_str(&body).unwrap();

    gw.wait_status(
        &submitted.run,
        |s| s == RunStatus::WaitingApproval,
        "交互 Run 照样要问",
    )
    .await;
    assert_eq!(
        lines(&counter),
        after_cron,
        "Cron 的授权没有替交互 Run 放行"
    );

    gw.stop().await;
}
