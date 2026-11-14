//! 设置页的两个持久开关（`docs/redesign.md` §2.3）在真 Gateway + 真 stdio MCP 服务器上：
//! `PUT /v1/mcp/{name}`（整台）与 `PUT /v1/mcp/{name}/tools/{tool}`（逐个工具）。开关写回 config.toml
//! （保留注释与别的键、开 = 删键）并热重载：关着的服务器不连、状态 `disabled`；关着的工具不进目录。

use std::sync::Arc;

use komo_gateway::service::test_support::harness::{FakeLlm, call_round, text_round};
use serde_json::{Value, json};

use super::{home_with_demo, mounted};

async fn put(
    gw: &komo_gateway::service::test_support::harness::Gw,
    path: &str,
    enabled: bool,
) -> (u16, Value) {
    let (status, body) = gw
        .request(
            reqwest::Method::PUT,
            path,
            Some(json!({ "enabled": enabled })),
        )
        .await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

async fn server_view(gw: &komo_gateway::service::test_support::harness::Gw) -> Value {
    let (status, body) = gw.get_json("/v1/mcp").await;
    assert_eq!(status, 200, "{body}");
    body["servers"][0].clone()
}

/// 等服务器连上、工具挂进目录。
async fn wait_mounted(gw: &komo_gateway::service::test_support::harness::Gw, tool: &str) {
    for _ in 0..200 {
        if mounted(gw).await.iter().any(|name| name == tool) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("等了 10 秒，{tool} 也没挂上");
}

#[tokio::test]
async fn switching_a_server_off_and_on_rewrites_the_config_and_hot_reloads() {
    let (home, counter) = home_with_demo(r#"exposure = "direct""#);
    let before = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    // 配置里留一条注释，证明改写不吃掉它。
    std::fs::write(
        home.path().join("config.toml"),
        before.replace(
            "[mcp.servers.demo]",
            "# 演示服务器，别删\n[mcp.servers.demo]",
        ),
    )
    .unwrap();
    let before = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "mcp__demo__bump", json!({})),
        text_round(2, "好了"),
    ]]);
    let gw = home.start(Arc::clone(&llm)).await;
    wait_mounted(&gw, "mcp__demo__bump").await;
    assert_eq!(server_view(&gw).await["enabled"], true);

    // 关：状态 disabled，工具撤出目录，config 里写 `enabled = false` 且注释还在。
    let (status, body) = put(&gw, "/v1/mcp/demo", false).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["servers"][0]["state"], "disabled", "{body}");
    assert_eq!(body["servers"][0]["enabled"], false);
    let text = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(
        text.contains("enabled = false") && text.contains("# 演示服务器，别删"),
        "{text}"
    );
    assert!(
        !mounted(&gw)
            .await
            .iter()
            .any(|name| name.starts_with("mcp__demo__"))
    );
    assert_eq!(server_view(&gw).await["state"], "disabled");
    // 重连 / 断开改不了「关着」。
    let (status, body) = gw
        .post_json("/v1/mcp/reconnect", json!({ "servers": ["demo"] }))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(server_view(&gw).await["state"], "disabled");
    // 模型再要这个工具，跑不出副作用。
    let session = gw.open_session().await;
    let run = gw.submit(&session, "off-1", "加一").await.run;
    gw.wait_terminal(&run).await;
    assert!(!counter.exists(), "关着的服务器没有执行任何调用");

    // 重启：仍然关着（持久），从没起过进程。
    let gw = {
        gw.stop().await;
        home.start(FakeLlm::finisher("好")).await
    };
    assert_eq!(server_view(&gw).await["state"], "disabled");
    assert!(
        !mounted(&gw)
            .await
            .iter()
            .any(|name| name.starts_with("mcp__demo__"))
    );

    // 开：删键（文件回到改之前的样子），后台连上，工具回来。
    let (status, body) = put(&gw, "/v1/mcp/demo", true).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
        before
    );
    wait_mounted(&gw, "mcp__demo__bump").await;
    assert_eq!(server_view(&gw).await["state"], "connected");

    // 不存在的服务器：404，config 不变。
    let (status, body) = put(&gw, "/v1/mcp/ghost", false).await;
    assert_eq!(status, 404, "{body:?}");
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
        before
    );
    gw.stop().await;
}

#[tokio::test]
async fn switching_a_tool_off_hides_it_without_reconnecting_and_on_brings_it_back() {
    let (home, counter) = home_with_demo(r#"exposure = "direct""#);
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "mcp__demo__bump", json!({})),
        text_round(2, "没法加"),
    ]]);
    let gw = home.start(Arc::clone(&llm)).await;
    wait_mounted(&gw, "mcp__demo__bump").await;
    let tools = server_view(&gw).await["tools"].clone();
    assert!(
        tools
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["enabled"] == true),
        "{tools}"
    );

    // 关一个：响应里那一项 enabled=false，另一个不动；目录里只剩 echo；config 写 disabled_tools。
    let (status, body) = put(&gw, "/v1/mcp/demo/tools/bump", false).await;
    assert_eq!(status, 200, "{body}");
    let flags: Vec<(String, bool)> = body["servers"][0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().unwrap().to_string(),
                tool["enabled"].as_bool().unwrap(),
            )
        })
        .collect();
    assert!(
        flags.contains(&("bump".to_string(), false)) && flags.contains(&("echo".to_string(), true)),
        "{flags:?}"
    );
    let text = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(text.contains(r#"disabled_tools = ["bump"]"#), "{text}");
    let names = mounted(&gw).await;
    assert!(
        names.iter().any(|n| n == "mcp__demo__echo")
            && !names.iter().any(|n| n == "mcp__demo__bump"),
        "{names:?}"
    );
    assert_eq!(server_view(&gw).await["state"], "connected", "没有重连");

    // 模型还想调它：没有这个工具，计数文件没有动。
    let session = gw.open_session().await;
    let run = gw.submit(&session, "tool-off", "加一").await.run;
    gw.wait_terminal(&run).await;
    assert!(!counter.exists(), "关着的工具没有执行");

    // 服务器没列出过的名字不让关（400）；服务器不存在 404。
    let (status, body) = put(&gw, "/v1/mcp/demo/tools/nope", false).await;
    assert_eq!(status, 400, "{body:?}");
    let (status, _) = put(&gw, "/v1/mcp/ghost/tools/bump", false).await;
    assert_eq!(status, 404);

    // 开回来：键连同数组一起消失，工具重新挂上。
    let (status, body) = put(&gw, "/v1/mcp/demo/tools/bump", true).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        !std::fs::read_to_string(home.path().join("config.toml"))
            .unwrap()
            .contains("disabled_tools")
    );
    wait_mounted(&gw, "mcp__demo__bump").await;
    // 本来就开着的再开一次：幂等。
    let (status, _) = put(&gw, "/v1/mcp/demo/tools/bump", true).await;
    assert_eq!(status, 200);
    gw.stop().await;
}
