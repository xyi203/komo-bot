//! §14 的两段散文验收：「首个端到端验收」与「目录与引用验收」。

use std::sync::Arc;

use komo_kernel::types::status::RunStatus;

use crate::harness::*;

/// **首个端到端验收**（§14）：
///
/// > 启动 komo 自动拉起 Gateway，通过模型调用 write 生成工作文件，在任务的后续步骤前重启
/// > Gateway；**不打开 CLI 也应自动接续原 Run**，随后 resume 原会话查看结果，再读出原
/// > 文件。随后验证一次待审批 shell 在重启后仍等待，在飞书或 Telegram 里批准后**只执行
/// > 一次**已确认的调用；若模拟执行结果丢失，则进入未知状态核对而不是自动重跑。
///
/// 这里的"CLI"是 `komo-client` 那条 HTTP 路（`crates/komo-gateway` 不依赖它，所以这个
/// 测试自己发同样的请求）。关键是**顺序**：先什么客户端都不开地让它自己接续，再连上去看。
#[tokio::test]
async fn the_first_end_to_end_acceptance() {
    let home = Home::new();
    let made = home.workspace().join("report.md");

    // ── 1. 模型调用 write 生成工作文件，然后在"后续步骤"（下一轮模型）之前中断。
    let fault = home.inject(Fault::BeforeRecordRound(2));
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-write",
            "write",
            serde_json::json!({
                "path": made.display().to_string(),
                "content": "# 报告\n第一步做完了。\n"
            }),
        ),
        text_round(2, "报告写好了。"),
    ]]);
    let gw = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "e2e-1", "写一份报告").await.run;
    fault.wait_tripped().await;
    gw.stop().await;

    assert!(made.exists(), "工作文件已经生成");
    let events = home.events(&session);
    assert_eq!(tool_started(&events).len(), 1);
    assert_eq!(tool_results(&events).len(), 1, "这一步收尾了");
    assert!(
        !events.iter().any(|e| e.type_name() == "run.completed"),
        "任务还没做完：{:?}",
        home.event_types(&session)
    );

    // ── 2. 重启。**不打开任何客户端**，Gateway 自己把原 Run 接着跑完。
    home.clear_injection();
    let llm = FakeLlm::finisher("报告写好了。");
    let gw = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    // 只看账本，不发任何 HTTP：这就是"不打开 CLI"。
    let status = gw.wait_db_status(&run, |s| s.is_terminal(), "终态").await;
    assert_eq!(status, RunStatus::Completed, "自动接续并跑完了原 Run");
    assert_eq!(
        tool_started(&home.events(&session)).len(),
        1,
        "接续不重放已经完成的动作"
    );

    // ── 3. 现在才 resume 原会话看结果。
    let (code, body) = gw
        .post(
            &format!("/v1/sessions/{session}/resume"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    let detail = gw.run_detail(&run).await;
    assert_eq!(detail.summary.status, RunStatus::Completed);
    assert_eq!(detail.final_message.as_deref(), Some("报告写好了。"));

    // ── 4. 再让模型 `read` 读出原文件——文件真的在，内容真的是那一份。
    let read_llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-read",
            "read",
            serde_json::json!({"path": made.display().to_string()}),
        ),
        text_round(2, "读到了。"),
    ]]);
    gw.stop().await;
    let gw = home
        .start(Arc::clone(&read_llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let read_run = gw.submit(&session, "e2e-2", "把那份报告读出来").await.run;
    let detail = gw.wait_terminal(&read_run).await;
    assert_eq!(detail.summary.status, RunStatus::Completed, "{detail:?}");
    // 结果正文在 tool-output 里，读回来核对。
    let events = home.events(&session);
    let last = (*tool_results(&events).last().expect("有结果")).clone();
    let output = home.session_dir(&session).join(
        last.output_ref
            .path()
            .replace('/', std::path::MAIN_SEPARATOR_STR),
    );
    let body: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&output).expect("读 output.json")).unwrap();
    assert!(
        serde_json::to_string(&body)
            .unwrap()
            .contains("第一步做完了"),
        "read 读回来的就是原文件：{body}"
    );

    // ── 5. 一次待审批 shell 在重启后仍等待，批准后只执行一次。
    let counter = Counter::new(&home, "e2e.count");
    gw.stop().await;
    let gw = home
        .start(FakeLlm::new(vec![vec![
            call_round(
                1,
                "pc-shell",
                "shell",
                serde_json::json!({"command": counter.append_command()}),
            ),
            text_round(2, "跑过了。"),
        ]]))
        .await;
    let shell_run = gw.submit(&session, "e2e-3", "跑一条命令").await.run;
    let pending = gw.wait_approval().await;
    assert_eq!(pending.run.as_ref(), Some(&shell_run));
    gw.stop().await;
    assert_eq!(counter.count(), 0, "没批准就不执行");

    let gw = home.start(FakeLlm::finisher("批准之后跑过了。")).await;
    // 重启后仍然等待——同一条审批，同一个短 ID，没有第二条。
    let still = gw.approvals().await;
    assert_eq!(still.len(), 1, "重启后仍等待：{still:?}");
    assert_eq!(still[0].approval, pending.approval, "还是原来那一条");
    assert_eq!(still[0].short_id, pending.short_id, "短 ID 也没变");
    assert_eq!(
        gw.db_status(&shell_run).await,
        RunStatus::WaitingApproval,
        "Run 还停在审批上"
    );

    // 在"聊天里"批准（HTTP 与聊天走的是同一段代码，§13.1）。
    gw.decide(&pending.approval, true).await;
    let detail = gw.wait_terminal(&shell_run).await;
    assert_eq!(detail.summary.status, RunStatus::Completed, "{detail:?}");
    assert_eq!(counter.count(), 1, "**只执行一次**已确认的调用");

    // 授权消费：一次性授权用掉了，重复批准不再放行第二次执行。
    let consumed = gw
        .running
        .state
        .approval_repo
        .get(&pending.approval)
        .await
        .expect("读得到")
        .expect("有这条");
    let decision = consumed.decision.as_ref().expect("有结论");
    assert!(decision.approved);
    assert!(decision.consumed, "一次性授权被消费掉了");
    gw.decide(&pending.approval, true).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(counter.count(), 1, "重复批准不重复执行");

    // ── 6. 模拟执行结果丢失 → 进入未知状态核对，而不是自动重跑。
    let counter2 = Counter::new(&home, "e2e-lost.count");
    gw.stop().await;
    let fault = home.inject(Fault::BeforeFinishCall);
    let gw = home
        .start(FakeLlm::new(vec![vec![
            call_round(
                1,
                "pc-lost",
                "shell",
                serde_json::json!({"command": counter2.append_command()}),
            ),
            text_round(2, "跑过了。"),
        ]]))
        .await;
    let lost_run = gw.submit(&session, "e2e-4", "再跑一条命令").await.run;
    let record = gw.wait_approval().await;
    gw.decide(&record.approval, true).await;
    fault.wait_tripped().await;
    gw.stop().await;
    assert_eq!(counter2.count(), 1, "动作发出去了，结果丢了");

    home.clear_injection();
    let gw = home.start(FakeLlm::finisher("不该走到这里。")).await;
    let status = gw
        .wait_db_status(
            &lost_run,
            |s| s == RunStatus::NeedsAttention || s.is_terminal(),
            "needs_attention",
        )
        .await;
    assert_eq!(
        status,
        RunStatus::NeedsAttention,
        "结果丢了要进未知状态核对，而不是自动重跑"
    );
    assert_eq!(counter2.count(), 1, "**没有自动重跑**");
    gw.stop().await;
}

/// **目录与引用验收**（§14）：
///
/// > 工具事件、大参数、完整输出及 stdout / stderr 都位于同一 Session 目录；**改变
/// > Gateway 当前工作目录不影响引用读取**；不同 Session 或不同 attempt 不能互相覆盖。
#[tokio::test]
async fn directories_and_references() {
    let home = Home::new();
    let counter_a = Counter::new(&home, "dir-a.count");
    let counter_b = Counter::new(&home, "dir-b.count");

    let gw = home
        .start(FakeLlm::new(vec![
            vec![
                call_round(
                    1,
                    "pc-a",
                    "shell",
                    serde_json::json!({"command": format!("{}; echo 到标准错误 1>&2", counter_a.append_command())}),
                ),
                text_round(2, "A 跑过了。"),
            ],
            // 第 2 段：批准之后的**续跑**也是一次 `begin_turn`，它不该再要求任何调用。
            vec![text_round(2, "A 跑过了。")],
            // 第 3 段：B 会话。
            vec![
                call_round(
                    1,
                    "pc-b",
                    "shell",
                    serde_json::json!({"command": counter_b.append_command()}),
                ),
                text_round(2, "B 跑过了。"),
            ],
        ]))
        .await;

    let session_a = gw.open_session().await;
    let run_a = gw.submit(&session_a, "dir-a", "跑 A").await.run;
    let approval = gw.wait_approval().await;
    gw.decide(&approval.approval, true).await;
    gw.wait_terminal(&run_a).await;

    let session_b = gw.open_session().await;
    let run_b = gw.submit(&session_b, "dir-b", "跑 B").await.run;
    let approval = gw.wait_approval().await;
    gw.decide(&approval.approval, true).await;
    gw.wait_terminal(&run_b).await;

    // ① 事件、输出与 stdout / stderr 都在**同一个 Session 目录**下，引用是相对路径。
    for (session, run) in [(&session_a, &run_a), (&session_b, &run_b)] {
        let events = home.events(session);
        let result = (*tool_results(&events).last().expect("有结果")).clone();
        assert!(
            !result.output_ref.path().starts_with('/'),
            "引用是受控相对路径：{}",
            result.output_ref.path()
        );
        assert!(
            result
                .output_ref
                .path()
                .starts_with(&format!("tool-output/{run}/")),
            "输出落在这个 Run 自己的目录下：{}",
            result.output_ref.path()
        );
        let resolved = home.session_dir(session).join(
            result
                .output_ref
                .path()
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        );
        assert!(resolved.exists(), "{}", resolved.display());
        for stream in [result.stdout.as_ref(), result.stderr.as_ref()]
            .into_iter()
            .flatten()
        {
            assert!(!stream.path.starts_with('/'), "{}", stream.path);
            assert!(
                home.session_dir(session)
                    .join(stream.path.replace('/', std::path::MAIN_SEPARATOR_STR))
                    .exists(),
                "stdout / stderr 也在同一个 Session 目录里：{}",
                stream.path
            );
        }
    }

    // ② 不同 Session 不互相覆盖：两个会话各有一棵自己的 tool-output 树。
    let a_outputs = home.session_dir(&session_a).join("tool-output");
    let b_outputs = home.session_dir(&session_b).join("tool-output");
    assert!(a_outputs.exists() && b_outputs.exists());
    assert_ne!(a_outputs, b_outputs);
    assert_eq!(counter_a.count(), 1);
    assert_eq!(counter_b.count(), 1);

    // ③ 改变 Gateway 的当前工作目录，引用照样读得出来（§8.3：不依赖进程工作目录）。
    let elsewhere = tempfile::tempdir().expect("另一个目录");
    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(elsewhere.path()).expect("换 cwd");
    let read_back = gw
        .get(&format!("/v1/sessions/{session_a}/events?from=0"))
        .await;
    std::env::set_current_dir(previous).expect("换回来");
    assert_eq!(read_back.0, 200, "{}", read_back.1);
    assert!(read_back.1.contains("tool.result"), "{}", read_back.1);

    gw.stop().await;

    // ④ 不同 attempt 不互相覆盖：同一个调用被中断之后重做一次，两次尝试各写各的目录。
    //
    // 造法：`finish_call` 之前跳闸——第一次尝试已经把 output.json 发布出去了，结果事件没
    // 写；停机之后把目标文件删掉，于是恢复时 `write` 的核对给出"确定未执行"，第二次尝试
    // 真的跑起来并发布自己那一份输出。
    let home2 = Home::new();
    let target = home2.workspace().join("attempts.txt");
    let fault = home2.inject(Fault::BeforeFinishCall);
    let gw = home2
        .start(FakeLlm::new(vec![vec![
            call_round(
                1,
                "pc-1",
                "write",
                serde_json::json!({"path": target.display().to_string(), "content": "两次尝试"}),
            ),
            text_round(2, "写好了。"),
        ]]))
        .await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "attempts", "写个文件").await.run;
    fault.wait_tripped().await;
    gw.stop().await;
    std::fs::remove_file(&target).expect("把写好的文件删掉");

    home2.clear_injection();
    let gw = home2.start(FakeLlm::finisher("核对之后重做了一次。")).await;
    gw.wait_terminal(&run).await;

    let events = home2.events(&session);
    let started = tool_started(&events);
    assert_eq!(
        started.len(),
        2,
        "两次尝试：{:?}",
        home2.event_types(&session)
    );
    assert_eq!(started[0].call_id, started[1].call_id, "同一个 ToolCall");
    assert_ne!(started[0].attempt_id, started[1].attempt_id);
    let call_dir = home2
        .session_dir(&session)
        .join("tool-output")
        .join(run.as_str())
        .join(started[0].call_id.as_str());
    let mut dirs: Vec<String> = std::fs::read_dir(&call_dir)
        .expect("调用目录在")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    assert!(
        dirs.contains(&started[0].attempt_id.to_string())
            && dirs.contains(&started[1].attempt_id.to_string()),
        "两次尝试各有一个目录，谁也没盖掉谁：{dirs:?}"
    );
    for attempt in [&started[0].attempt_id, &started[1].attempt_id] {
        assert!(
            call_dir.join(attempt.as_str()).join("output.json").exists(),
            "每次尝试的完整输出都在自己那一份里：{attempt}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default(),
        "两次尝试",
        "核对出「确定未执行」之后重做的那一次生效了"
    );
    gw.stop().await;
}
