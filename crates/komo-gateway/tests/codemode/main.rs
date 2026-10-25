//! codemode（`docs/codemode.md` §10）：真 Gateway，worker 是进程内的（测试进程自己不是 `komo`；
//! 真 worker 进程的验收在 `crates/komo/tests/codemode_worker.rs`）。

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

/// 跑一段脚本，交回模型看到的那条工具结果。
async fn run_script(home: &Home, code: &str) -> (Gw, Arc<FakeLlm>, String) {
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "codemode", serde_json::json!({ "code": code })),
        text_round(2, "好了"),
    ]]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    assert!(
        gw.state()
            .tool_names()
            .iter()
            .any(|name| name == "codemode"),
        "codemode 该总是挂上"
    );
    let session = gw.open_session().await;
    let run = gw.submit(&session, "cm-1", "跑脚本").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let result = tool_results(&llm).pop().expect("有一条工具结果");
    (gw, llm, result)
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
    let (gw, _, result) = run_script(&home, code).await;
    assert!(gw.interventions().await.is_empty());
    assert!(result.contains("a.log: ERROR disk"), "{result}");
    assert!(result.contains("b.log: ERROR net"), "{result}");
    assert!(!result.contains("ok 1"), "过滤掉的不该回来：{result}");
    assert!(result.contains("调用了 2 次工具：read×2"), "{result}");
}

/// 会改东西的工具在脚本里调不了：字面写着 `tools.write(` 的脚本在运行前就被拒，前面那句
/// `text` 也没机会跑；文件没被写。
#[tokio::test]
async fn a_write_inside_a_script_is_refused_before_the_script_runs() {
    let home = Home::new();
    let code = r#"
text("started")
tools.write(path="x.txt", content="y")
"#;
    let (_, _, result) = run_script(&home, code).await;
    assert!(result.contains("脚本里调不了 write"), "{result}");
    assert!(!result.contains("started"), "脚本不该跑过：{result}");
    assert!(!home.workspace().join("x.txt").exists());
}

/// 绕过字面写法（`getattr`）也调不到：`write` 不在脚本的工具表里，根本没有这个属性。
#[tokio::test]
async fn a_write_reached_through_getattr_is_still_refused() {
    let home = Home::new();
    let code = r#"
try:
    getattr(tools, "write")(path="x.txt", content="y")
except AttributeError as error:
    text("refused: " + str(error))
text("write" in TOOLS)
"#;
    let (_, _, result) = run_script(&home, code).await;
    assert!(
        result.contains("refused: ") && result.contains("no attribute 'write'"),
        "{result}"
    );
    assert!(result.contains("false"), "TOOLS 里不该有 write：{result}");
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
    let (gw, _, result) = run_script(&home, &code).await;
    assert!(result.contains("read: 外面那一行"), "{result}");
    assert!(gw.interventions().await.is_empty(), "脚本里不停下来问人");
}

/// 死循环到 `call_timeout_secs` 被杀，Run 照常收尾。
#[tokio::test]
async fn a_runaway_script_is_killed_at_the_call_timeout() {
    let home = Home::with_config(&config_toml("[execution]\ncall_timeout_secs = 2\n"));
    let (_, _, result) = run_script(&home, "while True:\n    pass\n").await;
    assert!(result.contains("脚本超时"), "{result}");
}

mod durable;
