//! codemode 在 Durable 存储上（`docs/storage-durable-d5.md` W4b）：与 `../main.rs` 同名同断言，Gateway
//! 跑在新存储上，只有起 Gateway 与"停下来问人"那几处不同：
//!
//! - 模型替身是 `FakeLlm`（同时是 `ModelStream`），起 Gateway 用 `Home::start`；
//! - Legacy 里 `gw.interventions().is_empty()` 证"脚本里不停下来问人"。新存储没有 `waiting + intervention`
//!   这个状态（intervention 只列 faulted 任务，路由由 W4a 接），这一句删掉：`run_script` 已断言 Run 以
//!   `Completed` 收尾，"停下来等人"在新存储里不存在，所以没有可断言的东西。

use std::sync::Arc;

use komo_gateway::service::test_support::harness::{
    FakeLlm, Gw, Home, call_round, config_toml, text_round,
};
use komo_kernel::types::status::RunState;

use super::tool_results;

/// 跑一段脚本，交回模型看到的那条工具结果。
async fn run_script(home: &Home, code: &str) -> (Gw, Arc<FakeLlm>, String) {
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "codemode", serde_json::json!({ "code": code })),
        text_round(2, "好了"),
    ]]);
    let gw = home.start(Arc::clone(&llm)).await;
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
    let (_gw, llm, result) = run_script(&home, code).await;
    assert!(result.contains("a.log: ERROR disk"), "{result}");
    assert!(result.contains("b.log: ERROR net"), "{result}");
    assert!(!result.contains("ok 1"), "过滤掉的不该回来：{result}");
    assert!(result.contains("调用了 2 次工具：read×2"), "{result}");
    assert_eq!(tool_results(&llm).len(), 1, "脚本里的两次读不进模型的消息");
}

/// 照 pi：`write` / `edit` 在脚本里能调，两个 `write` 也能用 `atools` 并行发出；改动不因脚本
/// 后面的步骤回滚。
#[tokio::test]
async fn writes_and_edits_inside_a_script_land_including_two_in_parallel() {
    let home = Home::new();
    std::fs::create_dir_all(home.workspace()).unwrap();
    let code = r#"
rs = await settle(
    atools.write(path="a.txt", content="alpha\n"),
    atools.write(path="b.txt", content="beta\n"),
)
text([r["ok"] for r in rs])
e = tools.edit(path="a.txt", edits=[{"match_text": "alpha", "replace_text": "ALPHA"}])
text(e["status"])
"#;
    let (_, _, result) = run_script(&home, code).await;
    assert!(result.contains("true,\n  true"), "{result}");
    assert!(result.contains("completed"), "{result}");
    assert!(
        result.contains("调用了 3 次工具：write×2、edit×1"),
        "{result}"
    );
    let read = |name: &str| std::fs::read_to_string(home.workspace().join(name)).unwrap();
    assert_eq!(read("a.txt"), "ALPHA\n");
    assert_eq!(read("b.txt"), "beta\n");
}

/// 编排工具在脚本里仍调不了：字面写着 `tools.task(` 的脚本在运行前就被拒，前面那句 `text` 也没机会跑；
/// 绕过字面写法（`getattr`）也调不到，`task` 不在脚本的工具表里。
#[tokio::test]
async fn orchestration_inside_a_script_is_refused_literally_and_through_getattr() {
    let home = Home::new();
    let (_, _, result) = run_script(&home, "text(\"started\")\ntools.task(prompt=\"x\")\n").await;
    assert!(result.contains("脚本里调不了 task"), "{result}");
    assert!(!result.contains("started"), "脚本不该跑过：{result}");

    let code = r#"
try:
    getattr(tools, "task")(prompt="x")
except AttributeError as error:
    text("refused: " + str(error))
text(["task" in TOOLS, "write" in TOOLS])
"#;
    let (_, _, result) = run_script(&home, code).await;
    assert!(
        result.contains("refused: ") && result.contains("no attribute 'task'"),
        "{result}"
    );
    assert!(result.contains("false,\n  true"), "{result}");
}

/// 工作区外的读取在脚本里照样放行（D8：内层只看计划是不是只读，与 auto 一致），不会停下来等人。
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
    let (_gw, _, result) = run_script(&home, &code).await;
    assert!(result.contains("read: 外面那一行"), "{result}");
}

/// 死循环到 `call_timeout_secs` 被杀，Run 照常收尾。
#[tokio::test]
async fn a_runaway_script_is_killed_at_the_call_timeout() {
    let home = Home::with_config(&config_toml("[execution]\ncall_timeout_secs = 2\n"));
    let (_, _, result) = run_script(&home, "while True:\n    pass\n").await;
    assert!(result.contains("脚本超时"), "{result}");
}

#[tokio::test]
async fn a_bash_tail_can_be_followed_by_reading_its_complete_artifact_inside_the_script() {
    let home = Home::with_config(&config_toml("[execution]\nmodel_result_bytes = 1024\n"));
    let code = r#"
r = tools.bash(command="printf 'FIRST_OUT\\n'; for ((i=0; i<1000; i++)); do printf 'line %04d data\\n' \"$i\"; done; printf 'LAST_OUT\\n'; printf 'STDERR_OUT\\n' >&2; exit 7")
ref = r["text"].split("完整输出：", 1)[1].split("（", 1)[0].strip()
full = tools.read(path=ref)["text"]
text({"status": r["status"], "exit": r["result"]["exit_code"],
      "tail": "LAST_OUT" in r["text"], "head_in_tail": "FIRST_OUT" in r["text"],
      "full_head": "FIRST_OUT" in full, "full_tail": "LAST_OUT" in full,
      "stderr": "STDERR_OUT" in full})
"#;
    let (_gw, _, result) = run_script(&home, code).await;
    for expected in [
        "\"status\": \"failed\"",
        "\"exit\": 7",
        "\"tail\": true",
        "\"head_in_tail\": false",
        "\"full_head\": true",
        "\"full_tail\": true",
        "\"stderr\": true",
    ] {
        assert!(result.contains(expected), "{expected}: {result}");
    }
}

/// 并行的两个 `edit` 改同一个文件的两处：不能有一处被悄悄覆盖掉——要么都改上，要么后一个得到冲突失败。
#[tokio::test]
async fn two_parallel_edits_of_one_file_never_lose_an_update_silently() {
    let home = Home::new();
    std::fs::create_dir_all(home.workspace()).unwrap();
    std::fs::write(home.workspace().join("f.txt"), "one\ntwo\n").unwrap();
    let code = r#"
rs = await settle(
    atools.edit(path="f.txt", edits=[{"match_text": "one", "replace_text": "ONE"}]),
    atools.edit(path="f.txt", edits=[{"match_text": "two", "replace_text": "TWO"}]),
)
text([r["value"]["status"] if r["ok"] else "rejected" for r in rs])
text([r["value"]["text"] if r["ok"] else r["error"] for r in rs])
"#;
    let (_, _, result) = run_script(&home, code).await;
    let body = std::fs::read_to_string(home.workspace().join("f.txt")).unwrap();
    let applied = ["ONE", "TWO"].iter().filter(|s| body.contains(*s)).count();
    let failed = result.matches("\"failed\"").count() + result.matches("\"rejected\"").count();
    assert_eq!(applied + failed, 2, "{result}\n{body}");
    if failed > 0 {
        assert!(result.contains("版本冲突"), "{result}");
    }
}
