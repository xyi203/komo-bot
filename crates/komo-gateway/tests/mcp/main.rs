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
    let home = Home::with_config(&config_toml(""));
    let counter = home.path().join("bump.count");
    std::fs::write(
        home.path().join("config.toml"),
        demo_config(&home, extra_servers),
    )
    .unwrap();
    std::fs::write(
        home.path().join(".env"),
        format!("{DEFAULT_ENV}DEMO_TOKEN=s3cret\n"),
    )
    .unwrap();
    (home, counter)
}

/// 配着 `demo` 服务器的整份 config.toml。
fn demo_config(home: &Home, extra_servers: &str) -> String {
    let server = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp/server.py");
    let counter = home.path().join("bump.count");
    config_toml(&format!(
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
    ))
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
            .tool_names()
            .iter()
            .any(|name| name == "mcp__demo__bump"),
        "{:?}",
        gw.state().tool_names()
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
    let names = &gw.state().tool_names();
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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let names = &gw.state().tool_names();
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
    assert!(
        gw.state()
            .tool_names()
            .iter()
            .any(|name| name == "codemode")
    );
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

/// 一台要 OAuth 的 HTTP MCP 服务器（`oauth_server.py`）。服务器此刻认哪个 token 写在一个文件里，
/// 改它就是"token 作废"；`calls` 每真的执行一次 `whoami` 记一行。
struct OAuthServer {
    child: std::process::Child,
    port: u16,
    valid: std::path::PathBuf,
    calls: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl OAuthServer {
    fn start(token: &str) -> OAuthServer {
        use std::io::BufRead as _;
        let dir = tempfile::tempdir().unwrap();
        let valid = dir.path().join("valid");
        let calls = dir.path().join("calls");
        std::fs::write(&valid, token).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp/oauth_server.py");
        let mut child = std::process::Command::new("python3")
            .arg(script)
            .arg(&valid)
            .arg(&calls)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        OAuthServer {
            child,
            port: line.trim().parse().unwrap(),
            valid,
            calls,
            _dir: dir,
        }
    }

    fn home(&self) -> Home {
        Home::with_config(&config_toml(&format!(
            r#"
[mcp.servers.secure]
url = "http://127.0.0.1:{port}/mcp"
oauth = true
exposure = "direct"
"#,
            port = self.port
        )))
    }

    /// 服务器换了一个 token：手里的那个作废。
    fn rotate(&self, token: &str) {
        std::fs::write(&self.valid, token).unwrap();
    }

    /// `komo auth mcp login secure` 落盘的样子：一份带 access token、没有 refresh token 的凭证。
    fn log_in(&self, home: &Home, token: &str) {
        let path = komo_runtime::mcp_auth::credentials_path(home.path(), "secure");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = serde_json::json!({
            "client_id": "komo-test",
            "token_response": { "access_token": token, "token_type": "Bearer" },
            "granted_scopes": [],
            "token_received_at": null,
        });
        std::fs::write(path, serde_json::to_vec(&body).unwrap()).unwrap();
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.calls).unwrap_or_default()
    }
}

impl Drop for OAuthServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    assert!(
        gw.state()
            .tool_names()
            .iter()
            .any(|name| name == "mcp__secure__whoami")
    );

    secure.rotate("fresh");
    let session = gw.open_session().await;
    let run = gw.submit(&session, "expired", "调一下").await.run;
    gw.wait_terminal(&run).await;
    let events = home.events(&session);
    let rejected = *recovery::tool_results(&events).last().unwrap();
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

/// 有没有 `mcp__demo__` 开头的工具。
fn has_demo(names: &[String]) -> bool {
    names.iter().any(|name| name.starts_with("mcp__demo__"))
}

/// 热重载加一台服务器（`docs/mcp.md` §2.2）：不用重启，下一条 Run 就看得到它的工具。重载不等
/// 握手；直出服务器在受理 Run 时等一下（照 pi 的 `waitForDirectServers`）。
#[tokio::test]
async fn a_server_added_by_reload_is_seen_by_the_next_run() {
    let home = Home::with_config(&config_toml(""));
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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    assert!(has_demo(&gw.state().tool_names()));

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

/// 在跑的 Run 能力面受理时冻结：服务器删掉之后它再调那个工具，拿到一句"已从配置移除"，
/// Run 照常往下走，不 panic、不停下来问人。
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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
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
    assert!(gw.interventions().await.is_empty(), "不该停下来问人");
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
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let before = gw.state().tool_names();
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
