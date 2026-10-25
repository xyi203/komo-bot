//! MCP 在 Durable 存储上（`docs/storage-durable-d5.md` W4b）：与 `../main.rs` 同名的测试，Gateway 跑在新存储
//! 上。真 stdio MCP 服务器（`server.py`）与 OAuth 服务器（`oauth_server.py`）照旧。
//!
//! 共同的改动：
//!
//! - 起 Gateway 用 `Home::start_durable`（`FakeLlm` 同时是 `ModelStream`）；
//! - `gw.interventions().is_empty()`（证"不该停下来问人"）一句删掉：新存储没有 `waiting + intervention`
//!   状态，Run 要么收尾要么 faulted，`wait_terminal` + `Completed` 已经说明没有停住；
//! - "冻结能力面"改成"每阶段（每次请求）按当前目录解析"：Legacy 的 Run 受理时冻结能力面与 MCP 目录，
//!   Durable 的钩子每次请求前按当前执行器目录重算（`hooks/tools.rs`），下面各测试里按这个语义改写，
//!   理由写在各自的注释里。
//!
//! 没迁的（理由）：
//!
//! - `codemode_restart_delivers_completed_unsafe_mcp_result_without_repeating_write`、
//!   `codemode_restart_does_not_repeat_unsafe_mcp_in_uncommitted_delivery_window`：靠 `tests/recovery`
//!   的旧故障注入（`AfterCodemodeDelivery` / `BeforeCodemodeDelivery`，卡在旧账本的 journal 事件上）。
//!   新存储里这两个窗口由 runtime 的 `harness/tests/codemode.rs` 钉住（`a_restart_during_an_unsafe_
//!   sub_call_ends_the_script_uncertain_and_never_redoes_it`、`a_restart_during_the_second_sub_call_
//!   replays_the_first_and_reruns_the_second_when_safe`，含 Turso 版）；
//! - `a_resumed_run_waits_for_its_mcp_server` / `a_resumed_run_is_told_why_its_mcp_server_is_missing`：
//!   靠 `BeforeStartRun` 故障注入造"受理过、没开跑、重启接续"的 Run。新存储没有受理快照与接续这一步，
//!   重启后任何 Run 都是"下一次请求"，所以改写成 `a_run_right_after_start_waits_for_its_mcp_server` /
//!   `a_run_is_told_why_its_mcp_server_is_missing`（见下）。

use std::sync::Arc;

use komo_gateway::service::test_support::harness::{
    DEFAULT_ENV, FakeLlm, call_round, config_toml, text_round,
};
use komo_kernel::types::status::RunState;

use super::{
    OAuthServer, demo_config, demo_config_with, has_demo, home_with_demo, mounted, tool_results,
};

/// 声明为只读的工具直接跑；子进程拿到了声明的 `DEMO_TOKEN`，没有继承 Gateway 的环境。
#[tokio::test]
async fn a_read_only_mcp_tool_runs_and_sees_only_declared_env() {
    assert!(std::env::var("CARGO_MANIFEST_DIR").is_ok());
    let (home, _) = home_with_demo("");
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-1",
            "mcp__demo__echo",
            serde_json::json!({ "text": "hi" }),
        ),
        text_round(2, "好了"),
    ]]);
    let gw = home.start_durable(Arc::clone(&llm)).await;
    let names = mounted(&gw).await;
    assert!(
        names.iter().any(|name| name == "mcp__demo__bump"),
        "{names:?}"
    );

    let session = gw.open_session().await;
    let run = gw.submit(&session, "mcp-1", "回显一下").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let results = tool_results(&llm);
    assert!(
        results
            .iter()
            .any(|text| text.contains("hi token=s3cret inherited=False")),
        "{results:?}"
    );
}

/// 没声明只读的工具同样直接执行（§7 没有审批），而且只执行一次。
#[tokio::test]
async fn a_side_effecting_mcp_tool_runs_directly_and_once() {
    let (home, counter) = home_with_demo("");
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "mcp__demo__bump", serde_json::json!({})),
        text_round(2, "加好了"),
    ]]);
    let gw = home.start_durable(Arc::clone(&llm)).await;

    let session = gw.open_session().await;
    let run = gw.submit(&session, "mcp-2", "加一").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    assert_eq!(std::fs::read_to_string(&counter).unwrap(), "bump\n");
}

/// 起不来的服务器记 warn 跳过，Gateway 照常起来，别的服务器的工具照常挂。
#[tokio::test]
async fn a_broken_server_does_not_take_the_others_down() {
    let (home, _) = home_with_demo(
        r#"
[mcp.servers.broken]
command = "/nonexistent/mcp-server"
"#,
    );
    let gw = home.start_durable(FakeLlm::finisher("好")).await;
    let names = &mounted(&gw).await;
    assert!(
        names.iter().any(|name| name == "mcp__demo__echo"),
        "{names:?}"
    );
    assert!(
        !names.iter().any(|name| name.starts_with("mcp__broken__")),
        "{names:?}"
    );
}

/// 默认 `exposure = "codemode"`（`docs/mcp.md` §3）：MCP 工具不进交给模型的 Schema，系统提示列出
/// 服务器与 instructions 的第一行；脚本里 `describe_namespace` 拿得到全文，`search_tools` 用中文
/// 也搜得到，找到的工具照常调用、只执行一次。
#[tokio::test]
async fn codemode_exposure_keeps_mcp_out_of_the_schema_and_reachable_from_scripts() {
    let (home, counter) = home_with_demo("");
    let code = r#"
ns = describe_namespace("demo")
text(ns["instructions"])
text(ns["tools"])
hits = search_tools("计数文件")
text(hits[0]["name"])
text(describe_tool(hits[0]["name"])["parameters"])
tools.mcp__demo__bump()
"#;
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "codemode", serde_json::json!({ "code": code })),
        text_round(2, "好了"),
    ]]);
    let gw = home.start_durable(Arc::clone(&llm)).await;
    assert!(
        gw.state()
            .tool_names()
            .iter()
            .any(|name| name == "codemode")
    );
    let session = gw.open_session().await;
    let run = gw.submit(&session, "mcp-cm", "加一").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let request = llm.requests.lock().unwrap()[0].clone();
    let declared: Vec<&str> = request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(declared.contains(&"codemode"), "{declared:?}");
    assert!(
        !declared.iter().any(|name| name.starts_with("mcp__")),
        "{declared:?}"
    );
    assert!(
        request
            .system_prompt
            .contains("- mcp__demo：演示服务器：先 echo 再 bump。"),
        "{}",
        request.system_prompt
    );
    assert!(
        !request.system_prompt.contains("往计数文件里追加一行"),
        "instructions 只进第一行：{}",
        request.system_prompt
    );

    let result = tool_results(&llm).pop().expect("有一条工具结果");
    assert!(result.contains("bump 往计数文件里追加一行。"), "{result}");
    assert!(result.contains("mcp__demo__echo"), "{result}");
    assert!(result.contains("mcp__demo__bump"), "{result}");
    assert_eq!(std::fs::read_to_string(&counter).unwrap(), "bump\n");
}

/// `exposure = "direct"` 与 `tool_exposure`：直出的进 Schema，`hidden` 的不挂；服务器上只剩直出的
/// 工具时，系统提示不出服务器段。
#[tokio::test]
async fn direct_and_hidden_exposure_follow_the_config() {
    let (home, _) = home_with_demo(
        r#"exposure = "direct"
tool_exposure = { "b*" = "hidden" }"#,
    );
    let llm = FakeLlm::finisher("好");
    let gw = home.start_durable(Arc::clone(&llm)).await;
    let names = &mounted(&gw).await;
    assert!(
        names.iter().any(|name| name == "mcp__demo__echo"),
        "{names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "mcp__demo__bump"),
        "{names:?}"
    );

    let session = gw.open_session().await;
    let run = gw.submit(&session, "mcp-direct", "看看").await.run;
    gw.wait_terminal(&run).await;
    let request = llm.requests.lock().unwrap()[0].clone();
    assert!(
        request
            .tools
            .iter()
            .any(|tool| tool.name == "mcp__demo__echo"),
        "{:?}",
        request.tools
    );
    assert!(
        !request.system_prompt.contains("mcp__demo"),
        "{}",
        request.system_prompt
    );
}

/// 脚本里调有副作用的 MCP 工具：副作用只发生一次，**只有外层 `codemode` 调用的结果进模型消息**。
///
/// 改了断言：Legacy 数 `codemode.journal` 事件的阶段（`start / intent / started / delivered /
/// finished`）。新存储没有 journal 事件：脚本里的每次子调用是外层任务名下的一个嵌套工具任务（runtime
/// `a_script_with_two_sub_calls_returns_only_its_text_and_leaves_one_nested_task_per_call`），所以这里
/// 看"模型消息里只有一条工具结果"与"账本里外层 + 嵌套各一个工具任务"。
#[tokio::test]
async fn codemode_side_effecting_mcp_is_journaled_and_only_parent_enters_model_messages() {
    let (home, counter) = home_with_demo("");
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-cm",
            "codemode",
            serde_json::json!({
                "code": "text(tools.mcp__demo__bump()['text'])"
            }),
        ),
        text_round(2, "好了"),
    ]]);
    let gw = home.start_durable(Arc::clone(&llm)).await;
    assert!(
        gw.state()
            .tool_names()
            .iter()
            .any(|name| name == "codemode")
    );
    let session = gw.open_session().await;
    let run = gw.submit(&session, "cm-write", "加一").await.run;
    assert_eq!(
        gw.wait_terminal(&run).await.summary.state,
        RunState::Completed
    );
    assert_eq!(std::fs::read_to_string(counter).unwrap(), "bump\n");
    let results = tool_results(&llm);
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("bumped"));
    let events = gw.durable_events(&session).await;
    let model_results = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                komo_kernel::events::EventPayload::ToolResult(_)
            )
        })
        .count();
    assert_eq!(model_results, 1, "嵌套调用的结果不进模型消息");

    // 账本里：外层 codemode 一个工具任务 + 嵌套的 bump 一个。
    use komo_kernel::durable::storage::{ConversationQuery, TaskQuery};
    let core = Arc::clone(gw.state().core().expect("Durable 模式"));
    let conversations = core
        .storage
        .scan_conversations(
            &ConversationQuery {
                session: Some(session.to_string()),
                ..ConversationQuery::default()
            },
            100,
            None,
        )
        .await
        .unwrap()
        .items;
    let mut tool_tasks = 0;
    for conversation in conversations {
        tool_tasks += core
            .storage
            .scan_tasks(
                &TaskQuery {
                    conversation: Some(conversation.id),
                    kind: Some("komo.tool".into()),
                    ..TaskQuery::default()
                },
                100,
                None,
            )
            .await
            .unwrap()
            .items
            .len();
    }
    assert_eq!(tool_tasks, 2, "外层 codemode 与嵌套的 bump 各一个工具任务");
    gw.stop().await;
}

/// 启动不等 MCP 握手（`docs/mcp.md` §2.2）：一台握手永远不回的服务器不拖住 Gateway 就绪；第一条
/// Run 受理时等到点就放手，正常服务器的工具照样在它的 Schema 里。
#[tokio::test]
async fn a_stuck_server_does_not_delay_startup() {
    let (home, _) = home_with_demo("");
    std::fs::write(
        home.path().join("config.toml"),
        demo_config_with(
            &home,
            "python3",
            &[],
            r#"exposure = "direct"

[mcp.servers.stuck]
command = "python3"
args = ["-c", "import sys; sys.stdin.read()"]
"#,
        ),
    )
    .unwrap();
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-1",
            "mcp__demo__echo",
            serde_json::json!({ "text": "hi" }),
        ),
        text_round(2, "好了"),
    ]]);
    let begun = std::time::Instant::now();
    let gw = home.start_durable(Arc::clone(&llm)).await;
    let ready = begun.elapsed();
    assert!(
        ready < std::time::Duration::from_secs(5),
        "Gateway 等了 MCP 握手：{ready:?}"
    );
    let stuck = gw
        .state()
        .mcp
        .statuses()
        .into_iter()
        .find(|status| status.name == "stuck")
        .unwrap();
    assert_eq!(
        stuck.state,
        komo_kernel::protocol::http::McpServerState::Connecting
    );

    let session = gw.open_session().await;
    let run = gw.submit(&session, "first", "回显一下").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    // 受理时等卡住那台至多 `STARTUP_WAIT`（10 秒），不是它的握手超时（30 秒）。
    let done = begun.elapsed();
    assert!(
        done < std::time::Duration::from_secs(20),
        "第一条 Run 被卡住的服务器拖住了：{done:?}"
    );
    let request = llm.requests.lock().unwrap()[0].clone();
    assert!(
        request
            .tools
            .iter()
            .any(|tool| tool.name == "mcp__demo__echo"),
        "{:?}",
        request.tools
    );
    let results = tool_results(&llm);
    assert!(
        results.iter().any(|text| text.contains("hi token=s3cret")),
        "{results:?}"
    );
    gw.stop().await;
}

/// 被"接续"的 Run 调 MCP 工具：重启后服务器还在连（握手前睡 2 秒），顶层调用与脚本里的调用都等它连上
/// 再调通，而不是报"没有这个工具"。
///
/// 改写：Legacy 用 `BeforeStartRun` 故障造"受理过、没开跑、重启接续"的 Run，能力面在受理时冻结。新存储
/// 没有这一步，重启后第一条 Run 的每次请求都按当前目录重算工具并有超时地等 MCP（`hooks/tools.rs`），
/// 所以直接用慢握手的配置起 Gateway、马上提交。
#[tokio::test]
async fn a_run_right_after_start_waits_for_its_mcp_server() {
    let (home, _) = home_with_demo("");
    std::fs::write(
        home.path().join("config.toml"),
        demo_config_with(&home, "python3", &["2"], r#"exposure = "direct""#),
    )
    .unwrap();
    let llm = echo_twice();
    let gw = home.start_durable(Arc::clone(&llm)).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "resumed", "调一下").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let results = tool_results(&llm);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(results[0].contains("top token=s3cret"), "{results:?}");
    assert!(results[1].contains("nested token=s3cret"), "{results:?}");
    gw.stop().await;
}

/// 被"接续"的 Run 调 MCP 工具，服务器连不上：顶层调用与脚本里的调用都报"MCP 服务器 demo 连接失败"，
/// 不是"没有这个工具"；Run 照常收尾。
///
/// 改写同上：能力面按当前配置重算——Profile 点名了 `mcp__demo__echo`（`tools = [...]`）才会在服务器没连上
/// 时留在能力面里，由桩说原因；Legacy 靠受理时冻结的能力面，这里改成 Profile 显式列出。
#[tokio::test]
async fn a_run_is_told_why_its_mcp_server_is_missing() {
    let (home, _) = home_with_demo("");
    let missing = home.path().join("no-such-python");
    let config = demo_config_with(
        &home,
        &missing.display().to_string(),
        &[],
        r#"exposure = "direct""#,
    )
    .replace(
        "[agents.assistant]\n",
        "[agents.assistant]\ntools = [\"read\", \"codemode\", \"mcp__demo__echo\"]\n",
    );
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    let llm = echo_twice();
    let gw = home.start_durable(Arc::clone(&llm)).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "resumed", "调一下").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let results = tool_results(&llm);
    assert_eq!(results.len(), 2, "{results:?}");
    for result in &results {
        assert!(result.contains("MCP 服务器 demo 连接失败"), "{results:?}");
        assert!(!result.contains("没有名为"), "{results:?}");
        assert!(!result.contains("脚本里不能调"), "{results:?}");
    }
    gw.stop().await;
}

/// 被"接续"的 Run 先直接调 `mcp__demo__echo`，再在脚本里调一次。
fn echo_twice() -> Arc<FakeLlm> {
    FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-1",
            "mcp__demo__echo",
            serde_json::json!({ "text": "top" }),
        ),
        call_round(
            2,
            "pc-2",
            "codemode",
            serde_json::json!({
                "code": "text(tools.mcp__demo__echo(text='nested')['text'])"
            }),
        ),
        text_round(3, "好了"),
    ]])
}

/// 启动时还没登录：Gateway 照常起来、这台服务器的工具不在；登录之后（凭证文件写进来）下一条 Run
/// 受理时连上、工具挂进来，不用重启（`docs/mcp.md` §2.2）。
#[tokio::test]
async fn logging_in_after_start_mounts_the_tools_on_the_next_run() {
    let secure = OAuthServer::start("good");
    let home = secure.home();
    let llm = FakeLlm::new(vec![
        vec![text_round(1, "还没登录")],
        vec![
            call_round(1, "pc-1", "mcp__secure__whoami", serde_json::json!({})),
            text_round(2, "好了"),
        ],
    ]);
    let gw = home.start_durable(Arc::clone(&llm)).await;
    let names = gw.state().tool_names();
    assert!(
        !names.iter().any(|name| name.starts_with("mcp__secure__")),
        "{names:?}"
    );
    let session = gw.open_session().await;
    let run = gw.submit(&session, "before", "看看").await.run;
    gw.wait_terminal(&run).await;

    secure.log_in(&home, "good");
    let run = gw.submit(&session, "after", "调一下").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let results = tool_results(&llm);
    assert!(
        results.iter().any(|text| text.contains("you=good")),
        "{results:?}"
    );
    assert_eq!(secure.calls(), "whoami\n");
    gw.stop().await;
}

/// 运行中 token 作废：调用报"要登录"、请求没被执行（不是"结果不明"）；重新登录之后，同一台
/// Gateway 的下一次调用就连上了。
#[tokio::test]
async fn a_rejected_token_asks_for_login_and_the_next_login_reconnects() {
    let secure = OAuthServer::start("good");
    let home = secure.home();
    secure.log_in(&home, "good");
    let whoami = |pc: &str| {
        vec![
            call_round(1, pc, "mcp__secure__whoami", serde_json::json!({})),
            text_round(2, "好"),
        ]
    };
    let llm = FakeLlm::new(vec![whoami("pc-1"), whoami("pc-2")]);
    let gw = home.start_durable(Arc::clone(&llm)).await;
    assert!(
        mounted(&gw)
            .await
            .iter()
            .any(|name| name == "mcp__secure__whoami")
    );

    secure.rotate("fresh");
    let session = gw.open_session().await;
    let run = gw.submit(&session, "expired", "调一下").await.run;
    gw.wait_terminal(&run).await;
    let events = gw.durable_events(&session).await;
    let rejected = events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            komo_kernel::events::EventPayload::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("有一条工具结果");
    assert_eq!(
        rejected.status,
        komo_kernel::types::refs::ToolResultStatus::Failed,
        "凭证被拒 = 没执行，不是结果不明"
    );
    let results = tool_results(&llm);
    assert!(
        results
            .last()
            .is_some_and(|text| text.contains("komo auth mcp login secure")),
        "{results:?}"
    );
    assert_eq!(secure.calls(), "");

    secure.log_in(&home, "fresh");
    let run = gw.submit(&session, "again", "再调一下").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let results = tool_results(&llm);
    assert!(
        results
            .last()
            .is_some_and(|text| text.contains("you=fresh")),
        "{results:?}"
    );
    assert_eq!(secure.calls(), "whoami\n");
    gw.stop().await;
}

/// 热重载加一台服务器（`docs/mcp.md` §2.2）：不用重启，下一条 Run 就看得到它的工具。
#[tokio::test]
async fn a_server_added_by_reload_is_seen_by_the_next_run() {
    let home = komo_gateway::service::test_support::harness::Home::with_config(&config_toml(""));
    std::fs::write(
        home.path().join(".env"),
        format!("{DEFAULT_ENV}DEMO_TOKEN=s3cret\n"),
    )
    .unwrap();
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-1",
            "mcp__demo__echo",
            serde_json::json!({ "text": "hi" }),
        ),
        text_round(2, "好了"),
    ]]);
    let gw = home.start_durable(Arc::clone(&llm)).await;
    assert!(!has_demo(&gw.state().tool_names()));

    std::fs::write(
        home.path().join("config.toml"),
        demo_config(&home, r#"exposure = "direct""#),
    )
    .unwrap();
    let response = komo_gateway::reload::reload(gw.state())
        .await
        .expect("新配置装得上");
    assert!(response.start_only.is_empty(), "{response:?}");
    assert_eq!(
        response
            .mcp
            .iter()
            .map(|server| server.name.as_str())
            .collect::<Vec<_>>(),
        vec!["demo"]
    );

    let session = gw.open_session().await;
    let run = gw.submit(&session, "hot-add", "回显一下").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let request = llm.requests.lock().unwrap()[0].clone();
    assert!(
        request
            .tools
            .iter()
            .any(|tool| tool.name == "mcp__demo__echo"),
        "{:?}",
        request.tools
    );
    let results = tool_results(&llm);
    assert!(
        results.iter().any(|text| text.contains("hi token=s3cret")),
        "{results:?}"
    );
    gw.stop().await;
}

/// 热重载删掉一台服务器：它的工具从目录撤掉，之后受理的 Run 看不到。
#[tokio::test]
async fn a_server_removed_by_reload_is_gone_for_the_next_run() {
    let (home, _) = home_with_demo(r#"exposure = "direct""#);
    let llm = FakeLlm::finisher("好");
    let gw = home.start_durable(Arc::clone(&llm)).await;
    assert!(has_demo(&mounted(&gw).await));

    std::fs::write(home.path().join("config.toml"), config_toml("")).unwrap();
    let response = komo_gateway::reload::reload(gw.state())
        .await
        .expect("新配置装得上");
    assert!(response.start_only.is_empty(), "{response:?}");
    assert!(response.mcp.is_empty(), "{response:?}");
    assert!(!has_demo(&gw.state().tool_names()));

    let session = gw.open_session().await;
    let run = gw.submit(&session, "hot-remove", "看看").await.run;
    gw.wait_terminal(&run).await;
    let request = llm.requests.lock().unwrap()[0].clone();
    assert!(
        !request
            .tools
            .iter()
            .any(|tool| tool.name.starts_with("mcp__demo__")),
        "{:?}",
        request.tools
    );
    gw.stop().await;
}

/// 在跑的 Run 里服务器被删掉：它再调那个工具，拿到一句"已从配置移除"，Run 照常往下走。
///
/// 改写：Legacy 靠"能力面受理时冻结"——名字还在冻结的面里，调用落到执行器上才被告知"已移除"。新存储每次
/// 请求按当前目录重算：被撤的名字留在能力面里（`ToolExecutor::withdrawn`）、换成桩，桩的 `prepare` 报原因
/// （`hooks/tests.rs::a_withdrawn_mcp_tool_fails_with_its_reason` 是单元版，这里是端到端）；同一句话
/// 对模型可见，只是不再有"冻结的面"。
#[tokio::test]
async fn a_running_run_is_told_the_server_was_removed() {
    let (home, _) = home_with_demo(r#"exposure = "direct""#);
    let gate = komo_gateway::service::test_support::Gate::new();
    home.install_tools(vec![gate.tool()]);
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "gate", serde_json::json!({})),
        call_round(
            2,
            "pc-2",
            "mcp__demo__echo",
            serde_json::json!({ "text": "hi" }),
        ),
        text_round(3, "好"),
    ]]);
    let gw = home.start_durable(Arc::clone(&llm)).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "in-flight", "调一下").await.run;
    gate.wait_entered(1).await;

    std::fs::write(home.path().join("config.toml"), config_toml("")).unwrap();
    komo_gateway::reload::reload(gw.state())
        .await
        .expect("新配置装得上");
    gate.release();

    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let results = tool_results(&llm);
    assert!(
        results
            .last()
            .is_some_and(|text| text.contains("MCP 服务器 demo 已从配置移除")),
        "{results:?}"
    );
    gw.stop().await;
}

/// 校验不过的 `[mcp.servers]` 不装（§3 第 1 步）：服务器与工具原样留着。
#[tokio::test]
async fn an_invalid_mcp_reload_leaves_the_servers_alone() {
    let (home, _) = home_with_demo("");
    let llm = FakeLlm::finisher("好");
    let gw = home.start_durable(Arc::clone(&llm)).await;
    let before = mounted(&gw).await;
    assert!(has_demo(&before));

    std::fs::write(
        home.path().join("config.toml"),
        config_toml(
            r#"
[mcp.servers."my server"]
command = "python3"
"#,
        ),
    )
    .unwrap();
    let error = komo_gateway::reload::reload(gw.state())
        .await
        .expect_err("名字放不进工具名，装不上");
    assert_eq!(
        error.code(),
        komo_kernel::protocol::http::ErrorCode::ConfigInvalid
    );
    assert_eq!(gw.state().tool_names(), before);
    assert!(gw.state().snapshot().mcp.contains_key("demo"));
    gw.stop().await;
}
