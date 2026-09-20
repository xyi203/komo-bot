//! §14 的两段散文验收：「首个端到端验收」与「目录与引用验收」。

use std::sync::Arc;

use komo_kernel::types::status::{RunState, WaitReason};

use crate::harness::*;

/// **首个端到端验收**（§14）：
///
/// > 启动 komo 自动拉起 Gateway，通过模型调用 write 生成工作文件，在任务的后续步骤前重启
/// > Gateway；**不打开 CLI 也应自动接续原 Run**，随后 resume 原会话查看结果，再读出原
/// > 文件。随后验证一次待审批 shell 在重启后仍等待，在飞书或 Telegram 里批准后**只执行
/// > 一次**已确认的调用；若模拟执行结果丢失，则进入未知状态核对而不是自动重跑。
///
/// 这里的"CLI"就是 `komo-client` 的那条 HTTP 路——`resume` 这一步用**真的 `KomoClient`**
/// 发（它是 gateway 的 dev-dependency，不成环）。关键是**顺序**：先什么客户端都不开地让
/// 它自己接续，再连上去看。
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
    let status = gw.wait_db_state(&run, |s| s.is_terminal(), "终态").await;
    assert_eq!(status, RunState::Completed, "自动接续并跑完了原 Run");
    assert_eq!(
        tool_started(&home.events(&session)).len(),
        1,
        "接续不重放已经完成的动作"
    );

    // ── 3. 现在才 resume 原会话看结果——走 CLI 那条路（`komo resume` 用的就是它）。
    let (base, token) = gw.address();
    let client = komo_client::KomoClient::new(&base, Some(token)).expect("客户端");
    let resumed = client
        .resume(
            &session,
            &komo_kernel::protocol::http::ResumeRequest::default(),
        )
        .await
        .expect("resume 得动");
    assert_eq!(resumed.session, session);
    let detail = client.run(&run).await.expect("读得到运行详情");
    assert_eq!(detail.summary.state, RunState::Completed);
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
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
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
    // §8.4 把"停"拆成两维：状态是 `Waiting`，**在等什么**另说。只断言状态等于弱化——
    // 退避、依赖那些"不是在等人"的等待也是 `Waiting`，所以第二维必须一起断言。
    assert_eq!(
        gw.db_state(&shell_run).await,
        RunState::Waiting,
        "Run 还停在等待上"
    );
    assert_eq!(
        gw.wait_waiting(&shell_run).await,
        WaitReason::Approval {
            approval: pending.approval.clone()
        },
        "等的是原来那一条审批，不是别的理由"
    );

    // 在"聊天里"批准（HTTP 与聊天走的是同一段代码，§13.1）。
    gw.decide(&pending.approval, true).await;
    let detail = gw.wait_terminal(&shell_run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
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

    // ── 6. 模拟执行结果丢失。§8.6 的规矩是「**先判断是否发生**，再决定是否重试」，所以
    //    "丢了结果"有两种结局，取决于这个工具核对不核对得出来。两种各断言一次。
    //
    //    6a. `write`：目标身份与内容哈希都在计划里，核对**答得出来**。丢了输出之后恢复
    //        读到"目标已是预期内容"，于是照实报告、**不重跑**，Run 正常收尾。
    let written = home.workspace().join("e2e-verifiable.txt");
    gw.stop().await;
    let fault = home.inject(Fault::BeforeFinishCall);
    let gw = home
        .start(FakeLlm::new(vec![vec![
            call_round(
                1,
                "pc-verifiable",
                "write",
                serde_json::json!({
                    "path": written.display().to_string(),
                    "content": "核对得出来"
                }),
            ),
            text_round(2, "写好了。"),
        ]]))
        .await;
    let verifiable_run = gw.submit(&session, "e2e-4", "写一个能核对的文件").await.run;
    fault.wait_tripped().await;
    gw.stop().await;
    let events = home.events(&session);
    let written_started = tool_started(&events)
        .last()
        .map(|s| (*s).clone())
        .expect("有这次尝试");
    assert_eq!(
        std::fs::read_to_string(&written).unwrap_or_default(),
        "核对得出来",
        "动作发出去了"
    );
    // 结果丢了：输出没落盘，`tool.result` 也没写。
    home.drop_attempt_output(&session, &verifiable_run, &written_started);

    home.clear_injection();
    let gw = home.start(FakeLlm::finisher("核对之后收尾。")).await;
    let status = gw
        .wait_db_state(
            &verifiable_run,
            |s| s.is_terminal() || s == RunState::Waiting,
            "收场",
        )
        .await;
    assert_eq!(
        status,
        RunState::Completed,
        "核对答得出「目标已满足」，就照实报告并收尾（§8.6），不是停在等人（`waiting`）"
    );
    assert_eq!(
        std::fs::read_to_string(&written).unwrap_or_default(),
        "核对得出来",
        "**没有重跑**：内容还是原来那一份"
    );
    let events = home.events(&session);
    let attempts_of_write: Vec<_> = tool_started(&events)
        .into_iter()
        .filter(|started| started.call_id == written_started.call_id)
        .collect();
    assert_eq!(attempts_of_write.len(), 1, "只有那一次尝试，没有第二次");
    assert!(
        unpaired_attempts(&events).is_empty(),
        "核对之后那条 started 也配上了结果：{:?}",
        unpaired_attempts(&events)
    );

    //    6b. `shell`：默认 `verify` 是 `Unavailable`，**核对不出来**。同样丢了结果，这一
    //        次只能进未知状态等人，且不能把命令再跑一遍。
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
    let lost_run = gw.submit(&session, "e2e-5", "再跑一条命令").await.run;
    let record = gw.wait_approval().await;
    gw.decide(&record.approval, true).await;
    fault.wait_tripped().await;
    gw.stop().await;
    assert_eq!(counter2.count(), 1, "动作发出去了，结果丢了");
    let events = home.events(&session);
    let lost_started = tool_started(&events)
        .last()
        .map(|s| (*s).clone())
        .expect("有这次尝试");
    home.drop_attempt_output(&session, &lost_run, &lost_started);

    home.clear_injection();
    let gw = home.start(FakeLlm::finisher("不该走到这里。")).await;
    let status = gw
        .wait_db_state(
            &lost_run,
            |s| s == RunState::Waiting || s.is_terminal(),
            "等人",
        )
        .await;
    assert_eq!(
        status,
        RunState::Waiting,
        "核对不出结论就进未知状态等人，而不是自动重跑"
    );
    // 第二维：这个 `waiting` 停在**要人判断**的那一类上，不是退避到点或等同会话前一条
    // Run（§8.4、§8.6）——只断言状态名等于弱化。
    let why = gw.wait_waiting(&lost_run).await;
    assert!(
        matches!(why, WaitReason::Intervention { .. }),
        "等的是「要人判断」那一条 Intervention：{why:?}"
    );
    assert_eq!(counter2.count(), 1, "**没有自动重跑**");
    let events = home.events(&session);
    assert!(
        unpaired_attempts(&events).is_empty(),
        "那条 started 要配一条明确的 uncertain：{:?}",
        unpaired_attempts(&events)
    );
    let settled = tool_results(&events)
        .into_iter()
        .find(|result| result.attempt_id == lost_started.attempt_id)
        .expect("配上了");
    assert_eq!(
        settled.status,
        komo_kernel::types::refs::ToolResultStatus::Uncertain,
        "副作用发生没发生不知道"
    );
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

    // ④ 不同 attempt 不互相覆盖：同一个调用被中断之后**真的重做一次**，两次尝试各写各的
    //    目录，第二次不许碰第一次那一份。
    //
    // 造法三步：① `finish_call` 之前跳闸——第一次尝试已经把 output.json 发布出去了，结果
    // 事件没写；② 停机之后把第一次那份 output.json **改名**存到旁边（`orphan::find` 于是
    // 读不到"这次动作确实发生过"的证据，恢复退回工具自己的核对；改名而不是删除，是为了在
    // 那个目录里留下一份第二次尝试**不许动**的真文件）；③ 把写好的目标文件删掉，于是
    // `write` 的核对给出"确定未执行"，第二次尝试真的跑起来并发布自己那一份输出。
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

    let first = tool_started(&home2.events(&session))
        .first()
        .map(|s| (*s).clone())
        .expect("第一次尝试");
    let first_dir = home2.attempt_dir(&session, &run, &first);
    let kept = first_dir.join("output.json.kept");
    std::fs::rename(first_dir.join("output.json"), &kept).expect("把第一次那份挪到旁边");
    let kept_bytes = std::fs::read(&kept).expect("读得到");
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
    assert_eq!(started[0].attempt_id, first.attempt_id);

    let call_dir = first_dir.parent().expect("调用目录").to_path_buf();
    let mut dirs: Vec<String> = std::fs::read_dir(&call_dir)
        .expect("调用目录在")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    assert_eq!(dirs.len(), 2, "两次尝试各有一个目录：{dirs:?}");
    assert!(
        dirs.contains(&started[0].attempt_id.to_string())
            && dirs.contains(&started[1].attempt_id.to_string()),
        "目录名就是 attempt ID：{dirs:?}"
    );

    // 第二次尝试把自己的输出写进**自己**那个目录，一个字节都没碰第一次那一份。
    let second_output = call_dir
        .join(started[1].attempt_id.as_str())
        .join("output.json");
    assert!(
        second_output.exists(),
        "第二次尝试的完整输出：{}",
        second_output.display()
    );
    assert_eq!(
        std::fs::read(&kept).expect("第一次那份还在"),
        kept_bytes,
        "谁也没盖掉谁"
    );
    let last = tool_results(&events)
        .last()
        .map(|r| (*r).clone())
        .expect("有结果");
    assert_eq!(
        last.output_ref.path(),
        format!(
            "tool-output/{run}/{}/{}/output.json",
            started[1].call_id, started[1].attempt_id
        ),
        "结果引用指的是第二次那一份"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default(),
        "两次尝试",
        "核对出「确定未执行」之后重做的那一次生效了"
    );
    gw.stop().await;
}
