//! §8.3 的「工具结果正文」与 `docs/komo_observation.md` 的 M1–M3，端到端两个测试：
//!
//! - **投影**：超预算时头尾都留着、写明省了多少，并给出**可直接 `read` 的资源入口**
//!   （`artifact://<run>/<call>/<attempt>/result`，§4.7）；
//! - **恢复**：那条入口真的读得进去——Session 自己的输出是只读根，**不用审批**；
//! - **预算**：`[execution] model_result_bytes` 说了算（§6），不是写死的 1 KiB；
//! - **一份事实**：同一个 Run 的下一个执行段回放时，渲染出的字节与刚跑完那次**完全相同**
//!   ——跑过的调用如此，没跑起来的（工具名不认识）也如此。
//! - **计划**：`update_plan` 的计划只在日志里，重启之后折出同一份在线状态，回放的结果
//!   与刚跑完那次逐字节相同。
//! - **压缩**：账本里一条 `context.compacted` 之后，回放是任务 → 摘要 → 切点起的原样轮次，
//!   外置的摘要按引用读回来，两次重启之间那一段逐字节不变。
//!
//! 共用件在 `komo_gateway::service::test_support::harness`（真数据目录、真 `service::start`、
//! 脚本化模型）。这里断言的是**模型收到了什么**，所以读的是 `FakeLlm` 记下的轮输入与请求。

#![allow(unused_imports)]

use std::sync::Arc;

use komo_gateway::service::test_support::harness::{
    FakeLlm, Home, call_round, config_toml, text_round,
};
use komo_kernel::events::{Event, EventPayload, ToolResult};
use komo_kernel::traits::LlmClient;
use komo_kernel::types::resource::{ResourceUri, output_file_path};
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::RoundInput;

/// 这个 Session 里所有 `tool.result`，按顺序。
fn tool_results(events: &[Event]) -> Vec<&ToolResult> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolResult(body) => Some(body),
            _ => None,
        })
        .collect()
}

/// 模型被喂回来的工具结果正文（工具结果是从轮输入回去的）：开头是 `[工具名` 的那一条。
fn fed_back(llm: &FakeLlm, tool: &str) -> String {
    let prefix = format!("[{tool} · ");
    llm.inputs
        .lock()
        .expect("轮输入")
        .iter()
        .find_map(|input| match input {
            RoundInput::ToolResults { results, .. } => results
                .iter()
                .find(|result| result.content.starts_with(&prefix))
                .map(|result| result.content.clone()),
            RoundInput::First => None,
        })
        .unwrap_or_else(|| panic!("模型没拿到 {tool} 的结果"))
}

/// 回放窗口里那条工具结果的正文：某个执行段开始装配上下文时，它先从账本+`output.json`
/// 把事实读回来，再投影一次。
fn replayed(llm: &FakeLlm, tool: &str) -> String {
    let prefix = format!("[{tool} · ");
    llm.requests
        .lock()
        .expect("请求")
        .iter()
        .rev()
        .find_map(|request| {
            request
                .messages
                .iter()
                .flat_map(|message| message.tool_results.iter())
                .find(|result| result.content.starts_with(&prefix))
                .map(|result| result.content.clone())
        })
        .unwrap_or_else(|| panic!("回放窗口里没有 {tool} 的结果"))
}

/// 一个 5000 行的文件 + 一条 `read`：正文远超 2 KiB 的预算。
fn home_with_a_big_file() -> (Home, std::path::PathBuf) {
    let home = Home::with_config(&config_toml(
        // 上下文预算调到 2 KiB：这条测试要看的就是"超了预算会怎样"（§6）。
        "[execution]\nmodel_result_bytes = 2048\n",
    ));
    let lines: String = (1..=5000).map(|n| format!("行 {n}\n")).collect();
    let file = home.workspace().join("大文件.txt");
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    std::fs::write(&file, &lines).expect("写文件");
    (home, file)
}

#[tokio::test]
async fn a_big_tool_output_keeps_both_ends_and_the_uri_it_points_at_really_reads() {
    let (home, _file) = home_with_a_big_file();
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-read",
            "read",
            serde_json::json!({ "path": "大文件.txt" }),
        ),
        text_round(2, "读完了。"),
    ]]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "obs-1", "把那份大文件读一遍")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let live = fed_back(&llm, "read");
    // 抬头、头尾两段、省了多少——不是"砍掉一半"。
    assert!(live.starts_with("[read · 完成 · "), "{live}");
    assert!(live.contains("中间省略"), "超了预算要说省了多少：\n{live}");
    assert!(live.contains("行 1"), "头部要留着：\n{live}");
    // 中间那一段（行 300 左右）正好落在被省略的区间里。
    assert!(
        !live.contains("行 300"),
        "被省略的部分不该声称给了：\n{live}"
    );

    // 完整输出的引用是一条**资源入口**（`artifact://<run>/<call>/<attempt>/result`），它指的
    // 正是这次落盘的那一份 `output.json`——拼法只有一处（`output_file_path`），两边共用。
    let events = home.events(&session);
    let result = tool_results(&events).pop().expect("有结果");
    let output = home.session_dir(&session).join(result.output_ref.path());
    let readable = std::fs::canonicalize(&output).expect("output.json 真的在");
    let entry = live
        .lines()
        .find_map(|line| line.strip_prefix("完整输出："))
        .expect("正文里要给出去哪儿读")
        .split('（')
        .next()
        .expect("入口")
        .to_string();
    let uri = ResourceUri::parse(&entry).expect("印出来的入口是合法的资源 URI");
    assert!(
        matches!(uri, ResourceUri::Output { .. }),
        "印出来的该是工具输出那条入口：{entry}"
    );
    let pointed = output_file_path(&home.session_dir(&session), &uri).expect("那条入口有路径");
    assert_eq!(
        std::fs::canonicalize(&pointed).expect("入口指向的文件真的在"),
        readable,
        "印出来的入口要正好指向落盘的那一份"
    );

    // ── 再用**同一条入口**发起一次 `read`：模型原样交回来就真读得进去，而且**不用审批**
    // （Session 自己的输出是只读根，§8.3）。
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-again", "read", serde_json::json!({ "path": entry })),
        text_round(2, "打开了。"),
    ]]);
    gateway.stop().await;
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let run = gateway
        .submit(&session, "obs-2", "把完整输出读出来")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    assert!(
        gateway.interventions().await.is_empty(),
        "读 Session 自己的输出停下来等人了"
    );

    // 省略不等于丢失：模型没看见的那一行，就在那条入口指向的文件里。
    let on_disk = std::fs::read_to_string(&readable).expect("读 output.json");
    assert!(on_disk.contains("行 300"), "被省略的那一段就在完整输出里");

    // 而模型确实能从那条入口读回来——**而且不用审批**（Session 自己的输出是只读根）。
    let read_back = fed_back(&llm, "read");
    assert!(
        read_back.contains("\"v\":1"),
        "读到的是那份 output.json：\n{read_back}"
    );
}

/// 同一个 Run 的第二段（第一次停在审批上）回放时，工具结果的正文必须与刚跑完那次一致。
///
/// 回放窗口里**正跑着的那条 Run 是完整协议**（§8.3），所以要看回放，就得让同一个 Run 再起
/// 一段——这里用"半轮停在审批上、批准之后接着跑"这条最普通的路。换成新 Run 就不成了：历史
/// Run 只发布正文，工具结果那一轮不再进新请求。
#[tokio::test]
async fn the_replayed_window_renders_the_same_bytes_as_the_live_round() {
    let (home, _file) = home_with_a_big_file();
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-read",
            "read",
            serde_json::json!({ "path": "大文件.txt" }),
        ),
        // 第二个调用要过审批：这一轮就停在这儿，Run 让出名额。
        call_round(
            2,
            "pc-shell",
            "shell",
            serde_json::json!({ "command": "echo 收尾" }),
        ),
        text_round(3, "都做完了。"),
    ]]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "obs-3", "读一遍再跑一条命令")
        .await
        .run;

    let pending = gateway.wait_approval().await;
    assert_eq!(pending.run.as_ref(), Some(&run), "停的是这条 Run");
    let live = fed_back(&llm, "read");

    // 批准 → 这条 Run 重新入队、起**第二个执行段**；它装配上下文时要回放那一轮。
    gateway.decide(&pending.approval, true).await;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    assert_eq!(
        replayed(&llm, "read"),
        live,
        "同一份事实在回放时渲染出的字节必须与刚跑完那次一样"
    );
}

/// 没跑起来的调用（工具名不认识）同样只有一份投影：它的结论落了盘，同一个 Run 的下一段
/// 回放时渲染出的字节与刚交回模型的那次一样。
#[tokio::test]
async fn a_refused_call_renders_the_same_bytes_live_and_replayed() {
    let home = Home::with_config(&config_toml(""));
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-nope", "nope", serde_json::json!({})),
        // 第二个调用要过审批：Run 停在这儿，批准之后起第二个执行段。
        call_round(
            2,
            "pc-shell",
            "shell",
            serde_json::json!({ "command": "echo 收尾" }),
        ),
        text_round(3, "都做完了。"),
    ]]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "obs-4", "调一个不存在的工具再跑一条命令")
        .await
        .run;

    let pending = gateway.wait_approval().await;
    assert_eq!(pending.run.as_ref(), Some(&run), "停的是这条 Run");
    let live = fed_back(&llm, "nope");
    assert!(live.starts_with("[nope · 失败 · "), "{live}");
    assert!(live.contains("没有 nope"), "{live}");

    gateway.decide(&pending.approval, true).await;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    assert_eq!(
        replayed(&llm, "nope"),
        live,
        "没跑起来的调用，回放时渲染出的字节也必须与刚交回模型的那次一样"
    );
}

/// 投影设置冻结在 Run 受理时的快照里（§6.2）：Run 停在审批上时改配置、热重载，批准之后
/// 第二段回放出来的字节仍与刚跑完那次一样——热重载只影响之后受理的 Run。
#[tokio::test]
async fn a_hot_reload_does_not_change_how_a_running_run_projects() {
    let (home, _file) = home_with_a_big_file();
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-read",
            "read",
            serde_json::json!({ "path": "大文件.txt" }),
        ),
        call_round(
            2,
            "pc-shell",
            "shell",
            serde_json::json!({ "command": "echo 收尾" }),
        ),
        text_round(3, "都做完了。"),
    ]]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "obs-5", "读一遍再跑一条命令")
        .await
        .run;

    let pending = gateway.wait_approval().await;
    assert_eq!(pending.run.as_ref(), Some(&run), "停的是这条 Run");
    let live = fed_back(&llm, "read");
    assert!(live.len() > 1500, "受理时的预算是 2 KiB：{}", live.len());

    // Run 停着的时候把预算改小、衰减关掉，然后热重载。
    std::fs::write(
        home.path().join("config.toml"),
        config_toml("[execution]\nmodel_result_bytes = 512\ndecay_threshold_bytes = 0\n"),
    )
    .expect("改 config.toml");
    gateway.state().config.reload().expect("新配置校验得过");
    assert_eq!(
        gateway
            .state()
            .config
            .current()
            .execution
            .model_result_bytes,
        512
    );

    gateway.decide(&pending.approval, true).await;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    assert_eq!(
        replayed(&llm, "read"),
        live,
        "这条 Run 按受理时冻结的投影设置回放，不跟着热重载变"
    );
}

/// §8.3 的衰减：大结果完整给过两次之后，活着的 loop 在第三次请求前把它换成短视图；
/// 同一个 Run 的下一段回放时，从日志数出同一个次数，直接给出**逐字节相同**的那份短视图。
///
/// 读一个大文件 → 两轮小调用 → 一条要审批的 `shell`（这一轮的请求里已经换过了）→ 批准，
/// 第二段回放。
#[tokio::test]
async fn the_decayed_view_is_the_same_live_and_replayed() {
    let home = Home::with_config(&config_toml(""));
    let lines: String = (1..=5000).map(|n| format!("行 {n}\n")).collect();
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    std::fs::write(home.workspace().join("大文件.txt"), &lines).expect("写文件");
    std::fs::write(home.workspace().join("小.txt"), "一行\n").expect("写文件");
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-big",
            "read",
            serde_json::json!({ "path": "大文件.txt" }),
        ),
        call_round(2, "pc-s1", "read", serde_json::json!({ "path": "小.txt" })),
        call_round(3, "pc-s2", "read", serde_json::json!({ "path": "小.txt" })),
        call_round(
            4,
            "pc-shell",
            "shell",
            serde_json::json!({ "command": "echo 收尾" }),
        ),
        text_round(5, "都做完了。"),
    ]]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(
            &session,
            "obs-6",
            "读一遍大文件，再看两眼小文件，最后跑一条命令",
        )
        .await
        .run;

    let pending = gateway.wait_approval().await;
    assert_eq!(pending.run.as_ref(), Some(&run), "停的是这条 Run");

    let inputs = llm.inputs.lock().expect("轮输入").clone();
    assert_eq!(inputs.len(), 4, "停在审批上之前请求了四次：{inputs:?}");
    let revised = |index: usize| match &inputs[index] {
        RoundInput::ToolResults { revised, .. } => revised.clone(),
        RoundInput::First => Vec::new(),
    };
    let RoundInput::ToolResults { results, .. } = &inputs[1] else {
        panic!("第二次请求带着 read 的结果：{:?}", inputs[1]);
    };
    let full = results
        .iter()
        .find(|result| result.provider_call_id == "pc-big")
        .expect("read 的结果")
        .content
        .clone();
    assert!(full.len() > 4096, "完整视图要超过默认阈值：{}", full.len());
    assert!(
        revised(1).is_empty() && revised(2).is_empty(),
        "前两次完整给"
    );
    let live = revised(3);
    assert_eq!(live.len(), 1, "第三次请求前换掉：{:?}", inputs[3]);
    assert_eq!(live[0].provider_call_id, "pc-big");
    let live = live[0].content.clone();
    assert!(live.contains("这份结果已完整给过 2 次"), "{live}");
    assert!(
        live.contains("行 1\n") && live.contains("…（中间省略 "),
        "{live}"
    );
    assert!(live.contains("完整输出：artifact://"), "{live}");
    assert!(live.len() + 1024 <= full.len());

    gateway.decide(&pending.approval, true).await;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let replayed = llm
        .requests
        .lock()
        .expect("请求")
        .last()
        .expect("第二段的请求")
        .messages
        .iter()
        .flat_map(|message| message.tool_results.iter())
        .find(|result| result.provider_call_id == "pc-big")
        .cloned()
        .expect("第二段回放里有那条 read 结果");
    assert_eq!(
        replayed.content, live,
        "活着时换上的短视图与下一段回放给出的必须逐字节相同"
    );
    assert!(replayed.decay.is_none(), "回放时已经过了完整期");
}

/// `update_plan` 的计划只在日志里：两次更新（登记 → 完成一步）之后停在审批上，Gateway
/// 重启，从日志折出的在线状态还是同一份计划、同样一个边界；第二段回放的计划结果与刚跑
/// 完那次逐字节相同。
#[tokio::test]
async fn the_plan_survives_a_restart_and_replays_the_same_bytes() {
    use komo_kernel::compaction::online_state;

    let home = Home::with_config(&config_toml(""));
    let step = |id: &str, status: &str| serde_json::json!({ "id": id, "goal": format!("做 {id}"), "status": status });
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-p1",
            "update_plan",
            serde_json::json!({ "steps": [step("a", "in_progress"), step("b", "pending")] }),
        ),
        call_round(
            2,
            "pc-p2",
            "update_plan",
            serde_json::json!({
                "steps": [step("a", "completed"), step("b", "in_progress")],
                "progress": {
                    "files_changed": ["a.txt"],
                    "verification": ["看过了"],
                    "decisions": [],
                },
            }),
        ),
        call_round(
            3,
            "pc-shell",
            "shell",
            serde_json::json!({ "command": "echo 收尾" }),
        ),
    ]]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "plan-1", "分两步做，最后跑一条命令")
        .await
        .run;

    let pending = gateway.wait_approval().await;
    assert_eq!(pending.run.as_ref(), Some(&run), "停的是这条 Run");
    assert_eq!(pending.plan.tool, "shell", "update_plan 不该停下来等审批");
    let live = |provider_call_id: &str| {
        llm.inputs
            .lock()
            .expect("轮输入")
            .iter()
            .find_map(|input| match input {
                RoundInput::ToolResults { results, .. } => results
                    .iter()
                    .find(|result| result.provider_call_id == provider_call_id)
                    .map(|result| result.content.clone()),
                RoundInput::First => None,
            })
            .unwrap_or_else(|| panic!("模型没拿到 {provider_call_id} 的结果"))
    };
    let (live_p1, live_p2) = (live("pc-p1"), live("pc-p2"));
    assert!(live_p1.contains("<komo-plan"), "{live_p1}");
    assert!(
        live_p2.contains(r#""id":"a","goal":"做 a","status":"completed""#),
        "{live_p2}"
    );

    // 计划整份内联在 `tool.planned` 里——外置了，折在线状态那一步读不到它。
    let events = home.events(&session);
    let planned: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolPlanned(body) if body.plan_ref.is_none() => body
                .plan
                .as_ref()
                .filter(|plan| plan.tool == "update_plan")
                .map(|_| body.call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(planned.len(), 2, "两次更新的计划都内联：{events:?}");
    let before = online_state(&events, &run);
    assert_eq!(before.plan.len(), 2);
    assert_eq!(
        before.completed_boundary_request_counts.len(),
        1,
        "完成 a 是一个边界"
    );

    // 重启：在线状态只从日志来。
    gateway.stop().await;
    let llm = FakeLlm::finisher("都做完了。");
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    assert_eq!(
        online_state(&home.events(&session), &run),
        before,
        "重启之后从日志折出的是同一份在线状态"
    );

    gateway.decide(&pending.approval, true).await;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let after = online_state(&home.events(&session), &run);
    assert_eq!(after.plan, before.plan);
    assert_eq!(
        after.completed_boundary_request_counts.len(),
        1,
        "第二段没有新的计划更新"
    );

    let replayed = |provider_call_id: &str| {
        llm.requests
            .lock()
            .expect("请求")
            .last()
            .expect("第二段的请求")
            .messages
            .iter()
            .flat_map(|message| message.tool_results.iter())
            .find(|result| result.provider_call_id == provider_call_id)
            .map(|result| result.content.clone())
            .unwrap_or_else(|| panic!("回放窗口里没有 {provider_call_id}"))
    };
    assert_eq!(replayed("pc-p1"), live_p1, "登记那次回放要逐字节相同");
    assert_eq!(replayed("pc-p2"), live_p2, "完成那次回放要逐字节相同");
}

/// 在线压缩的回放（§8.3）：三轮 `read` 之后停在审批上，账本里补一条 `context.compacted`
/// （切在第三轮开头，摘要长到要外置），重启再批：第二段的请求是**任务 → 摘要 → 第三轮起
/// 的原样轮次**；再停一次、再重启，第三段回放的同一段前缀逐字节不变。
#[tokio::test]
async fn a_compacted_run_replays_the_same_summary_and_tail_across_restarts() {
    use komo_agent::context::COMPACTION_PREFIX;
    use komo_kernel::test_support::compacted;
    use komo_kernel::types::turn::Role;

    let home = Home::with_config(&config_toml(""));
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    std::fs::write(home.workspace().join("小.txt"), "一行\n").expect("写文件");
    let read = || serde_json::json!({ "path": "小.txt" });
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-r1", "read", read()),
        call_round(2, "pc-r2", "read", read()),
        call_round(3, "pc-r3", "read", read()),
        call_round(
            4,
            "pc-s1",
            "shell",
            serde_json::json!({ "command": "echo 一" }),
        ),
    ]]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "compact-1", "读三遍小文件，再跑两条命令")
        .await
        .run;
    let first = gateway.wait_approval().await;
    assert_eq!(first.run.as_ref(), Some(&run));

    let rounds: Vec<_> = home
        .events(&session)
        .into_iter()
        .filter(|event| {
            event.run.as_ref() == Some(&run)
                && matches!(event.payload, EventPayload::MessageAssistant(_))
        })
        .map(|event| event.seq)
        .collect();
    assert_eq!(rounds.len(), 4, "四轮都落盘了");
    let summary = "读过两遍小文件，内容都是「一行」。".repeat(200);
    assert!(summary.len() > 4096, "要真的过内联上限");
    gateway
        .state()
        .turn_ledger
        .record_compaction(&run, compacted(rounds[2], summary.clone()))
        .await
        .expect("压缩事件落盘");
    let line = home
        .events(&session)
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::ContextCompacted(body) => Some(body),
            _ => None,
        })
        .expect("日志里有 context.compacted");
    assert!(
        matches!(
            line,
            komo_kernel::events::ContextCompacted::Compacted {
                summary: None,
                summary_ref: Some(_),
                ..
            }
        ),
        "长摘要外置，行里只留引用：{line:?}"
    );

    // 重启 → 批 → 第二段。
    gateway.stop().await;
    let llm = FakeLlm::new(vec![vec![call_round(
        5,
        "pc-s2",
        "shell",
        serde_json::json!({ "command": "echo 二" }),
    )]]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    gateway.decide(&first.approval, true).await;
    let second = loop {
        let pending = gateway.wait_approval().await;
        if pending.approval != first.approval {
            break pending;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    let segment_two = llm
        .requests
        .lock()
        .expect("请求")
        .last()
        .expect("第二段的请求")
        .messages
        .clone();

    let shape: Vec<(Role, Option<String>, Vec<String>)> = segment_two
        .iter()
        .map(|message| {
            let ids = message
                .tool_calls
                .iter()
                .map(|call| call.provider_call_id.clone())
                .chain(
                    message
                        .tool_results
                        .iter()
                        .map(|result| result.provider_call_id.clone()),
                )
                .collect();
            // 摘要正文太长，形状里只记它是不是那一句。
            let text = match &message.text {
                Some(text) if text == &format!("{COMPACTION_PREFIX}\n\n{summary}") => {
                    Some("<摘要>".to_string())
                }
                other => other.clone(),
            };
            (message.role, text, ids)
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (
                Role::User,
                Some("读三遍小文件，再跑两条命令".into()),
                vec![]
            ),
            (Role::User, Some("<摘要>".into()), vec![]),
            (Role::Assistant, None, vec!["pc-r3".into()]),
            (Role::Tool, None, vec!["pc-r3".into()]),
            // 批准的那次调用这一段才跑，结果经轮输入回去，不在开段的回放里。
            (Role::Assistant, None, vec!["pc-s1".into()]),
        ],
        "任务 → 摘要（外置的正文按引用读回来）→ 第三轮起原样"
    );

    // 再重启 → 批 → 第三段：同一段前缀逐字节不变。
    gateway.stop().await;
    let llm = FakeLlm::finisher("都做完了。");
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    gateway.decide(&second.approval, true).await;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let segment_three = llm
        .requests
        .lock()
        .expect("请求")
        .last()
        .expect("第三段的请求")
        .messages
        .clone();
    assert_eq!(
        segment_three.len(),
        segment_two.len() + 2,
        "多了 s1 的结果与 s2 那一轮"
    );
    assert_eq!(
        serde_json::to_string(&segment_three[..segment_two.len()]).unwrap(),
        serde_json::to_string(&segment_two).unwrap(),
        "重启之后回放的那一段逐字节相同"
    );
}
