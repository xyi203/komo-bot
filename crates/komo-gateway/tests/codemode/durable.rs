//! codemode 在 Durable 存储上（`docs/storage-durable-d5.md` W4b）：真 Gateway + djs（JS/TS）。

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

/// 脚本并发读两个文件、只把过滤后的那几行交回来，不停下来问人。
#[tokio::test]
async fn a_script_reads_and_filters_and_only_the_filtered_text_comes_back() {
    let home = Home::new();
    std::fs::create_dir_all(home.workspace()).unwrap();
    std::fs::write(home.workspace().join("a.log"), "ok 1\nERROR disk\nok 2\n").unwrap();
    std::fs::write(home.workspace().join("b.log"), "ok 3\nERROR net\n").unwrap();
    let code = r#"
const names = ["a.log", "b.log"];
const rs = await Promise.all(names.map((path) => tools.read({ path })));
const hits: string[] = [];
for (let i = 0; i < names.length; i++) {
  for (const line of rs[i].text.split("\n")) {
    if (line.includes("ERROR")) hits.push(`${names[i]}: ${line}`);
  }
}
text(hits.join("\n"));
"#;
    let (_gw, llm, result) = run_script(&home, code).await;
    assert!(result.contains("a.log: ERROR disk"), "{result}");
    assert!(result.contains("b.log: ERROR net"), "{result}");
    assert!(!result.contains("ok 1"), "过滤掉的不该回来：{result}");
    assert!(result.contains("调用了 2 次工具：read×2"), "{result}");
    assert_eq!(tool_results(&llm).len(), 1, "脚本里的两次读不进模型的消息");
}

/// 照 pi：`write` / `edit` 在脚本里能调，两个 `write` 也能并发发出；改动不因脚本后面的步骤回滚。
#[tokio::test]
async fn writes_and_edits_inside_a_script_land_including_two_in_parallel() {
    let home = Home::new();
    std::fs::create_dir_all(home.workspace()).unwrap();
    let code = r#"
const rs = await Promise.allSettled([
  tools.write({ path: "a.txt", content: "alpha\n" }),
  tools.write({ path: "b.txt", content: "beta\n" }),
]);
text(rs.map((r) => r.status === "fulfilled"));
const e = await tools.edit({ path: "a.txt", edits: [{ match_text: "alpha", replace_text: "ALPHA" }] });
text(e.status);
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
/// 绕过字面写法（`tools["task"]`）也调不到，`task` 不在脚本的工具表里。
#[tokio::test]
async fn orchestration_inside_a_script_is_refused_literally_and_through_bracket_lookup() {
    let home = Home::new();
    let (_, _, result) = run_script(
        &home,
        "text('started');\nawait tools.task({ prompt: 'x' });\n",
    )
    .await;
    assert!(result.contains("脚本里调不了 task"), "{result}");
    assert!(!result.contains("started"), "脚本不该跑过：{result}");

    let code = r#"
try {
  await tools["task"]({ prompt: "x" });
} catch (error) {
  text("refused: " + (error as Error).message);
}
text([TOOLS.includes("task"), TOOLS.includes("write")]);
"#;
    let (_, _, result) = run_script(&home, code).await;
    assert!(
        result.contains("refused: ") && result.contains("没有叫 task 的工具"),
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
        "const body = (await tools.read({{ path: {:?} }})).text;\ntext('read: ' + body.trim());",
        outside.display().to_string()
    );
    let (_gw, _, result) = run_script(&home, &code).await;
    assert!(result.contains("read: 外面那一行"), "{result}");
}

/// 死循环到 `call_timeout_secs` 被杀，Run 照常收尾。
#[tokio::test]
async fn a_runaway_script_is_killed_at_the_call_timeout() {
    let home = Home::with_config(&config_toml("[execution]\ncall_timeout_secs = 2\n"));
    let (_, _, result) = run_script(&home, "while (true) {}").await;
    assert!(result.contains("脚本超时"), "{result}");
}

#[tokio::test]
async fn a_bash_preview_can_be_followed_by_reading_its_complete_artifact_inside_the_script() {
    let home = Home::with_config(&config_toml("[execution]\nmodel_result_bytes = 1024\n"));
    let code = r#"
const r = await tools.bash({
  command: "printf 'FIRST_OUT\\n'; for ((i=0; i<1000; i++)); do printf 'line %04d data\\n' \"$i\"; done; printf 'LAST_OUT\\n'; printf 'STDERR_OUT\\n' >&2; exit 7",
});
const ref = r.text.split("完整输出：")[1].split("（")[0].trim();
const full = (await tools.read({ path: ref })).text;
text({
  status: r.status,
  exit: r.result.exit_code,
  head: r.text.includes("FIRST_OUT"),
  tail_in_head: r.text.includes("LAST_OUT"),
  notice: r.text.includes("超过单条结果上限"),
  full_head: full.includes("FIRST_OUT"),
  full_tail: full.includes("LAST_OUT"),
  stderr: full.includes("STDERR_OUT"),
});
"#;
    let (_gw, _, result) = run_script(&home, code).await;
    for expected in [
        "\"status\": \"failed\"",
        "\"exit\": 7",
        "\"head\": true",
        "\"tail_in_head\": false",
        "\"notice\": true",
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
const rs = await Promise.allSettled([
  tools.edit({ path: "f.txt", edits: [{ match_text: "one", replace_text: "ONE" }] }),
  tools.edit({ path: "f.txt", edits: [{ match_text: "two", replace_text: "TWO" }] }),
]);
text(rs.map((r) => r.status === "fulfilled" ? r.value.status : "rejected"));
text(rs.map((r) => r.status === "fulfilled" ? r.value.text : r.reason.message));
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
