//! §8.3 的「工具结果正文」与 `docs/komo_observation.md` 的 M1–M3，端到端两个测试：
//!
//! - **投影**：超预算时头尾都留着、写明省了多少，并给出**可直接 `read` 的绝对路径**；
//! - **恢复**：那条路径真的读得进去——Session 自己的输出是只读根，**不用审批**；
//! - **预算**：`[execution] model_result_bytes` 说了算（§6），不是写死的 1 KiB；
//! - **一份事实**：同一个 Run 的下一个执行段回放时，渲染出的字节与刚跑完那次**完全相同**。
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
            RoundInput::ToolResults { results } => results
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
async fn a_big_tool_output_keeps_both_ends_and_the_path_it_points_at_really_reads() {
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

    // 完整输出的引用是**能直接读的绝对路径**：Session 目录 + 引用里的相对路径。
    let events = home.events(&session);
    let result = tool_results(&events).pop().expect("有结果");
    let output = home.session_dir(&session).join(result.output_ref.path());
    let readable = std::fs::canonicalize(&output).expect("output.json 真的在");
    assert!(
        live.contains(&format!("完整输出：{}", readable.display())),
        "正文里要给出可以直接 read 的路径：\n{live}"
    );

    // ── 再用同一条路径发起一次 `read`：那条路径真的读得进去，而且**不用审批**（§8.3）。
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-again",
            "read",
            serde_json::json!({ "path": readable.display().to_string() }),
        ),
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

    // 省略不等于丢失：模型没看见的那一行，就在那条路径指向的文件里。
    let on_disk = std::fs::read_to_string(&readable).expect("读 output.json");
    assert!(on_disk.contains("行 300"), "被省略的那一段就在完整输出里");

    // 而模型确实能从那条路径读回来——**而且不用审批**（Session 自己的输出是只读根）。
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
