//! MCP（`docs/mcp.md`）：真 Gateway + 一个真的 stdio MCP 服务器（`server.py`）。
//!
//! 钉住的是"MCP 工具走与内置工具同一条路"：模型要的就执行（§7 没有 Policy，声明只读与否
//! 影响恢复时能不能重做），而且只执行一次；子进程只拿到声明过的 `.env` 变量；一个
//! 起不来的服务器不拖垮启动。

use std::path::Path;
use std::sync::Arc;

use komo_gateway::service::test_support::harness::{
    DEFAULT_ENV, FakeLlm, Home, call_round, config_toml, text_round,
};
use komo_kernel::traits::LlmClient;
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::RoundInput;

fn home_with_demo(extra_servers: &str) -> (Home, std::path::PathBuf) {
    let server = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp/server.py");
    let home = Home::with_config(&config_toml(""));
    let counter = home.path().join("bump.count");
    let config = config_toml(&format!(
        r#"
[mcp.servers.demo]
command = "python3"
args = ["{server}", "{counter}"]
env = ["DEMO_TOKEN"]
read_only = ["echo"]
{extra_servers}
"#,
        server = server.display(),
        counter = counter.display(),
    ));
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    std::fs::write(
        home.path().join(".env"),
        format!("{DEFAULT_ENV}DEMO_TOKEN=s3cret\n"),
    )
    .unwrap();
    (home, counter)
}

fn tool_results(llm: &FakeLlm) -> Vec<String> {
    llm.inputs
        .lock()
        .unwrap()
        .iter()
        .filter_map(|input| match input {
            RoundInput::ToolResults { results, .. } => Some(
                results
                    .iter()
                    .map(|result| result.content.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}

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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    assert!(
        gw.state()
            .tool_names
            .iter()
            .any(|name| name == "mcp__demo__bump"),
        "{:?}",
        gw.state().tool_names
    );

    let session = gw.open_session().await;
    let run = gw.submit(&session, "mcp-1", "回显一下").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    assert!(gw.interventions().await.is_empty(), "不该停下来问人");

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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let session = gw.open_session().await;
    let run = gw.submit(&session, "mcp-2", "加一").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    assert_eq!(std::fs::read_to_string(&counter).unwrap(), "bump\n");
    assert!(gw.interventions().await.is_empty(), "不该停下来问人");
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
    let gw = home.start(FakeLlm::finisher("好")).await;
    let names = &gw.state().tool_names;
    assert!(
        names.iter().any(|name| name == "mcp__demo__echo"),
        "{names:?}"
    );
    assert!(
        !names.iter().any(|name| name.starts_with("mcp__broken__")),
        "{names:?}"
    );
}

/// macOS 总有 Seatbelt；Linux 看内核的 LSM 列表里有没有 landlock（同 `tests/codemode`）。
fn sandbox_expected() -> bool {
    cfg!(target_os = "macos")
        || (cfg!(target_os = "linux")
            && std::fs::read_to_string("/sys/kernel/security/lsm")
                .is_ok_and(|lsm| lsm.split(',').any(|name| name.trim() == "landlock")))
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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    if !gw.state().tool_names.iter().any(|name| name == "codemode") {
        assert!(!sandbox_expected(), "这台机器上 codemode 该挂上");
        return;
    }
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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let names = &gw.state().tool_names;
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

#[path = "../recovery/harness.rs"]
mod recovery;

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
    let gw = home.start(llm.clone()).await;
    if !gw.state().tool_names.iter().any(|name| name == "codemode") {
        return;
    }
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
    let events = home.events(&session);
    assert_eq!(recovery::tool_results(&events).len(), 1);
    let phases: Vec<String> = events
        .iter()
        .filter_map(|event| match &event.payload {
            komo_kernel::events::EventPayload::CodemodeJournal(body) => {
                Some(body.record["phase"].as_str().unwrap().to_owned())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        phases,
        ["start", "intent", "started", "delivered", "finished"]
    );
    gw.stop().await;
}

async fn codemode_crash_window(delivery_committed: bool) {
    use recovery::{Fault, Injected};
    let (home, counter) = home_with_demo("");
    let fault = home.inject(if delivery_committed {
        Fault::AfterCodemodeDelivery(1)
    } else {
        Fault::BeforeCodemodeDelivery(1)
    });
    let gw = home
        .start(FakeLlm::new(vec![vec![
            call_round(
                1,
                "pc-cm",
                "codemode",
                serde_json::json!({
                    "code": "text(tools.mcp__demo__bump()['text'])"
                }),
            ),
            text_round(2, "好了"),
        ]]))
        .await;
    if !gw.state().tool_names.iter().any(|name| name == "codemode") {
        home.clear_injection();
        return;
    }
    let session = gw.open_session().await;
    let run = gw.submit(&session, "cm-crash", "加一").await.run;
    fault.wait_tripped().await;
    gw.stop().await;
    assert_eq!(std::fs::read_to_string(&counter).unwrap(), "bump\n");
    let events = home.events(&session);
    let started = (*recovery::tool_started(&events).last().unwrap()).clone();
    // 丢掉外层留下的孤儿输出，逼恢复走 journal 这条路，而不是直接收养它。
    home.drop_attempt_output(&session, &run, &started);
    home.clear_injection();
    let gw = home.start(FakeLlm::finisher("已核对")).await;
    assert_eq!(
        gw.wait_terminal(&run).await.summary.state,
        RunState::Completed
    );
    assert_eq!(
        std::fs::read_to_string(&counter).unwrap(),
        "bump\n",
        "写调用不能重复"
    );
    let events = home.events(&session);
    let result = *recovery::tool_results(&events).last().unwrap();
    assert_eq!(
        result.status,
        if delivery_committed {
            komo_kernel::types::refs::ToolResultStatus::Completed
        } else {
            komo_kernel::types::refs::ToolResultStatus::Uncertain
        }
    );
    assert!(gw.interventions().await.is_empty());
    gw.stop().await;
}

#[tokio::test]
async fn codemode_restart_delivers_completed_unsafe_mcp_result_without_repeating_write() {
    codemode_crash_window(true).await;
}

#[tokio::test]
async fn codemode_restart_does_not_repeat_unsafe_mcp_in_uncommitted_delivery_window() {
    codemode_crash_window(false).await;
}
