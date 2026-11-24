//! codemode 的真 worker：起 `komo __codemode-worker` 子进程（`docs/codemode.md` §10）。
//! 进程内 worker 管不了的——死循环被杀、内存看门狗、隐藏子命令——都在这里。

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use komo_tools::codemode::{ScriptCatalog, ScriptOutcome, ScriptTool, Worker};
use serde_json::json;

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_komo"))
}

fn names(tools: &[&str]) -> Vec<String> {
    tools.iter().map(|name| (*name).to_string()).collect()
}

async fn run(worker: &Worker, code: &str, timeout: Duration) -> ScriptOutcome {
    worker
        .run(
            code,
            &names(&["read", "fail"]),
            &ScriptCatalog::default(),
            timeout,
            |name, args| async move {
                match name.as_str() {
                    "read" => Ok(json!({
                        "status": "completed",
                        "text": format!("ok 1\nERROR {}\nok 2", args["path"].as_str().unwrap_or_default()),
                    })),
                    _ => Err("boom".to_string()),
                }
            },
        )
        .await
}

#[tokio::test]
async fn a_script_reads_twice_and_only_the_filtered_text_comes_back() {
    let code = r#"
const rs = await Promise.all(["a.log", "b.log"].map((path) => tools.read({ path })));
const hits = rs.flatMap((r) => r.text.split("\n").filter((line: string) => line.includes("ERROR")));
text(hits.join("\n"));
"#;
    let outcome = run(&Worker::exe(exe()), code, Duration::from_secs(30)).await;
    assert_eq!(outcome.error, None);
    assert_eq!(
        outcome.outputs,
        vec!["ERROR a.log\nERROR b.log".to_string()]
    );
    assert_eq!(outcome.console, "");
}

#[tokio::test]
async fn a_failed_sub_call_is_a_tool_error_the_script_can_catch() {
    let code = r#"
try {
  await tools.fail();
} catch (e) {
  if (e instanceof ToolError) text("caught " + e.message);
}
"#;
    let outcome = run(&Worker::exe(exe()), code, Duration::from_secs(30)).await;
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.outputs, vec!["caught boom".to_string()]);
}

#[tokio::test]
async fn date_formatting_and_typescript_syntax_work_in_the_real_worker() {
    let code = r#"
const fmt = (sec: number, offsetHours = 0): string => {
  const d = new Date((sec + offsetHours * 3600) * 1000);
  return d.toISOString().slice(0, 19).replace("T", " ");
};
text([fmt(0), fmt(1729324800.25), fmt(1729324800.25, 8), fmt(-1)]);
"#;
    let outcome = run(&Worker::exe(exe()), code, Duration::from_secs(30)).await;
    assert_eq!(outcome.error, None);
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&outcome.outputs[0]).unwrap(),
        [
            "1970-01-01 00:00:00",
            "2024-10-19 08:00:00",
            "2024-10-19 16:00:00",
            "1969-12-31 23:59:59",
        ]
    );
}

#[tokio::test]
async fn the_worker_process_has_no_fs_network_or_environment() {
    let code = r#"
text({
  fetch: typeof (globalThis as any).fetch,
  process: typeof (globalThis as any).process,
  require: typeof (globalThis as any).require,
  open: typeof (globalThis as any).open,
});
"#;
    let outcome = run(&Worker::exe(exe()), code, Duration::from_secs(30)).await;
    assert_eq!(outcome.error, None);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&outcome.outputs[0]).unwrap(),
        json!({
            "fetch": "undefined",
            "process": "undefined",
            "require": "undefined",
            "open": "undefined",
        })
    );
}

#[tokio::test]
async fn a_worker_from_another_build_is_refused_before_the_script_runs() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("komo");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho '{\"hello\":{\"protocol\":999,\"engine\":\"djs-00000000\",\"komo\":\"0.0.0\"}}'\ncat > /dev/null\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    komo_tools::spawn::retry_text_file_busy(|| {
        std::process::Command::new(&fake)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
    })
    .unwrap();
    let outcome = run(&Worker::exe(&fake), "text('ran')", Duration::from_secs(30)).await;
    let message = outcome.incompatible.expect("握手对不上");
    assert!(message.contains("重启 Gateway"), "{message}");
    assert!(message.contains("协议 999"), "{message}");
    assert!(outcome.outputs.is_empty());
    assert_eq!(outcome.error, None);
}

#[tokio::test]
async fn discovery_works_in_the_real_worker() {
    let catalog = ScriptCatalog {
        tools: vec![
            ScriptTool {
                name: "read".into(),
                description: "读一个文件".into(),
                parameters: json!({}),
                namespace: None,
            },
            ScriptTool {
                name: "mcp__dev__search".into(),
                description: "搜索代码仓库".into(),
                parameters: json!({}),
                namespace: None,
            },
        ],
        namespaces: vec![],
    };
    let outcome = Worker::exe(exe())
        .run(
            "text(searchTools('搜索').map((t) => t.name));",
            &names(&["read", "mcp__dev__search"]),
            &catalog,
            Duration::from_secs(30),
            |_, _| async { Err(String::new()) },
        )
        .await;
    assert_eq!(outcome.error, None);
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&outcome.outputs[0]).unwrap(),
        ["mcp__dev__search"]
    );
}

#[tokio::test]
async fn an_infinite_loop_is_killed_at_the_deadline_and_the_parent_carries_on() {
    let worker = Worker::exe(exe());
    let started = Instant::now();
    let outcome = run(&worker, "while (true) {}", Duration::from_secs(1)).await;
    assert!(outcome.error.unwrap().contains("超时"));
    assert!(started.elapsed() < Duration::from_secs(10));

    let outcome = run(&worker, "text('still fine')", Duration::from_secs(30)).await;
    assert_eq!(outcome.outputs, vec!["still fine".to_string()]);
}

#[tokio::test]
async fn memory_growth_is_stopped_cleanly_at_the_hard_heap_cap() {
    let worker = Worker::exe(exe()).with_memory_limit(64 << 20);
    let started = Instant::now();
    let outcome = run(
        &worker,
        "const chunks = []; while (true) chunks.push('x'.repeat(1000));",
        Duration::from_secs(60),
    )
    .await;
    let error = outcome.error.unwrap();
    assert!(error.contains("脚本内存超限"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[test]
fn the_worker_subcommand_is_hidden_from_help() {
    let help = Command::new(exe()).arg("--help").output().unwrap();
    assert!(!String::from_utf8_lossy(&help.stdout).contains("__codemode-worker"));
}
