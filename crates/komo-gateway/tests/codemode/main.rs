//! codemode（`docs/codemode.md` §10）：真 Gateway + 真沙箱。沙箱在 macOS 与开了 Landlock 的
//! Linux 上有，别处 `codemode` 不注册，这组测试跳过。

use std::sync::Arc;

use komo_gateway::service::test_support::harness::{
    FakeLlm, Gw, Home, call_round, config_toml, text_round,
};
use komo_kernel::traits::LlmClient;
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::RoundInput;

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

/// macOS 总有 Seatbelt；Linux 看内核的 LSM 列表里有没有 landlock。
fn sandbox_expected() -> bool {
    cfg!(target_os = "macos")
        || (cfg!(target_os = "linux")
            && std::fs::read_to_string("/sys/kernel/security/lsm")
                .is_ok_and(|lsm| lsm.split(',').any(|name| name.trim() == "landlock")))
}

/// 跑一段脚本，交回模型看到的那条工具结果。没有沙箱时是 `None`。
async fn run_script(home: &Home, code: &str) -> Option<(Gw, Arc<FakeLlm>, String)> {
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "codemode", serde_json::json!({ "code": code })),
        text_round(2, "好了"),
    ]]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    if !gw.state().tool_names.iter().any(|name| name == "codemode") {
        if sandbox_expected() {
            panic!("这台机器上 codemode 该挂上");
        }
        return None;
    }
    let session = gw.open_session().await;
    let run = gw.submit(&session, "cm-1", "跑脚本").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let result = tool_results(&llm).pop().expect("有一条工具结果");
    Some((gw, llm, result))
}

/// 脚本读两个文件、只把过滤后的那几行交回来，不停下来问人。
#[tokio::test]
async fn a_script_reads_and_filters_and_only_the_filtered_text_comes_back() {
    let home = Home::new();
    std::fs::create_dir_all(home.workspace()).unwrap();
    std::fs::write(home.workspace().join("a.log"), "ok 1\nERROR disk\nok 2\n").unwrap();
    std::fs::write(home.workspace().join("b.log"), "ok 3\nERROR net\n").unwrap();
    let code = r#"
hits = []
for name in ["a.log", "b.log"]:
    body = tools.read(path=name)["text"]
    hits += [name + ": " + line for line in body.splitlines() if "ERROR" in line]
text("\n".join(hits))
"#;
    let Some((gw, _, result)) = run_script(&home, code).await else {
        return;
    };
    assert!(gw.interventions().await.is_empty());
    assert!(result.contains("a.log: ERROR disk"), "{result}");
    assert!(result.contains("b.log: ERROR net"), "{result}");
    assert!(!result.contains("ok 1"), "过滤掉的不该回来：{result}");
    assert!(result.contains("调用了 2 次工具：read×2"), "{result}");
}

/// 会改东西的工具在脚本里调不了：抛 ToolError，文件没被写。
#[tokio::test]
async fn a_write_inside_a_script_is_refused() {
    let home = Home::new();
    let code = r#"
try:
    tools.write(path="x.txt", content="y")
except ToolError as error:
    text("refused: " + str(error))
"#;
    let Some((_, _, result)) = run_script(&home, code).await else {
        return;
    };
    assert!(
        result.contains("refused: ") && result.contains("write 不是只读工具或 MCP"),
        "{result}"
    );
    assert!(!home.workspace().join("x.txt").exists());
}

/// 工作区外的读取在脚本里照样放行（D8：内层只看计划是不是只读，与 auto 一致），不会
/// 停下来等人。
#[tokio::test]
async fn a_read_outside_the_workspace_is_allowed_inside_a_script() {
    let home = Home::new();
    let outside = home.path().join("outside.txt");
    std::fs::write(&outside, "外面那一行\n").unwrap();
    let code = format!(
        r#"
body = tools.read(path="{}")["text"]
text("read: " + body.strip())
"#,
        outside.display()
    );
    let Some((gw, _, result)) = run_script(&home, &code).await else {
        return;
    };
    assert!(result.contains("read: 外面那一行"), "{result}");
    assert!(gw.interventions().await.is_empty(), "脚本里不停下来问人");
}

/// 死循环到 `call_timeout_secs` 被杀，Run 照常收尾。
#[tokio::test]
async fn a_runaway_script_is_killed_at_the_call_timeout() {
    let home = Home::with_config(&config_toml("[execution]\ncall_timeout_secs = 2\n"));
    let Some((_, _, result)) = run_script(&home, "while True:\n    pass\n").await else {
        return;
    };
    assert!(result.contains("脚本超时"), "{result}");
}
