//! MCP（`docs/mcp.md`）：真 Gateway + 一个真的 stdio MCP 服务器（`server.py`）。
//!
//! 钉住的是"MCP 工具走与内置工具同一条路"：操作者声明为只读的直接放行，其余要审批、
//! 批了才真的执行；子进程只拿到声明过的 `.env` 变量；一个起不来的服务器不拖垮启动。

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
            RoundInput::ToolResults { results } => Some(
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

/// 声明为只读的工具不问人；子进程拿到了声明的 `DEMO_TOKEN`，没有继承 Gateway 的环境。
#[tokio::test]
async fn a_read_only_mcp_tool_runs_without_approval_and_sees_only_declared_env() {
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
    assert!(gw.approvals().await.is_empty(), "只读工具不该问人");

    let results = tool_results(&llm);
    assert!(
        results
            .iter()
            .any(|text| text.contains("hi token=s3cret inherited=False")),
        "{results:?}"
    );
}

/// 没声明只读的工具要审批；批了才真的执行一次。
#[tokio::test]
async fn a_side_effecting_mcp_tool_waits_for_approval_then_runs_once() {
    let (home, counter) = home_with_demo("");
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "mcp__demo__bump", serde_json::json!({})),
        text_round(2, "加好了"),
    ]]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let session = gw.open_session().await;
    let run = gw.submit(&session, "mcp-2", "加一").await.run;
    let approval = gw.wait_approval().await;
    assert_eq!(approval.plan.tool, "mcp__demo__bump");
    assert!(!counter.exists(), "批之前不该执行");

    gw.decide(&approval.approval, true).await;
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
