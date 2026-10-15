//! codemode 的真 worker：起 `komo __codemode-worker` 子进程（`docs/codemode.md` §10）。
//! 进程内 worker 管不了的——死循环被杀、内存看门狗、隐藏子命令——都在这里。

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use komo_runtime::codemode::{ScriptCatalog, ScriptOutcome, ScriptTool, Worker};
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
hits = []
for name in ["a.log", "b.log"]:
    body = tools.read(path=name)["text"]
    hits += [line for line in body.splitlines() if "ERROR" in line]
text("\n".join(hits))
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
    let code = "try:\n    tools.fail()\nexcept ToolError as e:\n    text('caught ' + str(e))";
    let outcome = run(&Worker::exe(exe()), code, Duration::from_secs(30)).await;
    assert_eq!(outcome.outputs, vec!["caught boom".to_string()]);
}

#[tokio::test]
async fn the_worker_process_has_no_files_and_no_environment() {
    let code = r#"
import os
try:
    open("/etc/passwd").read()
    text("read")
except Exception as e:
    text("denied")
text(os.getenv("HOME"))
"#;
    let outcome = run(&Worker::exe(exe()), code, Duration::from_secs(30)).await;
    assert_eq!(outcome.error, None);
    assert_eq!(
        outcome.outputs,
        vec!["denied".to_string(), "null".to_string()]
    );
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
            "text([t['name'] for t in search_tools('搜索')])",
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
    let outcome = run(&worker, "while True:\n    pass", Duration::from_secs(1)).await;
    assert!(outcome.error.unwrap().contains("超时"));
    assert!(started.elapsed() < Duration::from_secs(10));

    let outcome = run(&worker, "text('still fine')", Duration::from_secs(30)).await;
    assert_eq!(outcome.outputs, vec!["still fine".to_string()]);
}

#[tokio::test]
async fn slow_memory_growth_is_stopped_before_it_takes_the_machine() {
    let worker = Worker::exe(exe()).with_memory_limit(256 << 20);
    let started = Instant::now();
    let outcome = run(
        &worker,
        "chunks = []\nwhile True:\n    chunks.append('x' * 1000)",
        Duration::from_secs(60),
    )
    .await;
    let error = outcome.error.unwrap();
    assert!(error.contains("脚本内存超限"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(30));
}

#[test]
fn the_worker_subcommand_is_hidden_from_help() {
    let help = Command::new(exe()).arg("--help").output().unwrap();
    assert!(!String::from_utf8_lossy(&help.stdout).contains("__codemode-worker"));
}
