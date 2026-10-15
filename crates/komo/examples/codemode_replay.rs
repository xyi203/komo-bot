//! 升级 Monty 或改前置代码（`prelude.py`）之后的回归：拿真实会话里的 codemode 脚本与录像
//! （`docs/codemode.md` §10）回放一遍，核对脚本能编译、调用序列与录下来的一致。
//!
//! 脚本与录像是从本机会话（只读）里抽出来的数据，不进仓库：每个脚本一个 `codemode/<name>.py`，
//! 有完整子调用录像的另有 `replay/<name>.py.json`
//! （`{"tools": [..], "cwd": ".."|null, "calls": [{"name", "args", "value", "error"}]}`）。
//!
//! cargo run --release -p komo --example codemode_replay -- <komo 可执行文件> <目录>
//! 每个脚本一行 JSON：{"file", "status", "recorded_calls", "actual_calls", "detail"}；
//! 没有录像的只做编译检查（status 为 `compiled` 或 `compile_error`）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use komo_runtime::codemode::{ScriptCatalog, Worker, worker::compile_check};
use serde_json::{Value, json};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (exe, root) = (PathBuf::from(&args[0]), PathBuf::from(&args[1]));
    let mut scripts: Vec<_> = std::fs::read_dir(root.join("codemode"))
        .expect("目录下要有 codemode/")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    scripts.sort();
    for script in scripts {
        let name = script.file_name().unwrap().to_string_lossy().into_owned();
        let code = std::fs::read_to_string(&script).unwrap();
        let line = match std::fs::read_to_string(root.join("replay").join(format!("{name}.json"))) {
            Ok(replay) => {
                replay_one(&name, &exe, &code, &serde_json::from_str(&replay).unwrap()).await
            }
            Err(_) => match compile_check(&code) {
                Ok(()) => report(&name, "compiled", 0, 0, ""),
                Err(error) => report(&name, "compile_error", 0, 0, &error),
            },
        };
        println!("{line}");
    }
}

async fn replay_one(name: &str, exe: &Path, code: &str, replay: &Value) -> Value {
    let recorded: Vec<Value> = replay["calls"].as_array().cloned().unwrap_or_default();
    let tools: Vec<String> = replay["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut worker = Worker::exe(exe);
    if let Some(cwd) = replay["cwd"].as_str() {
        worker = worker.in_dir(cwd);
    }
    let state = Arc::new(Mutex::new((0usize, None::<String>)));
    let recorded_calls = Arc::new(recorded.clone());
    let outcome = worker
        .run(
            code,
            &tools,
            &ScriptCatalog::default(),
            Duration::from_secs(30),
            {
                let state = Arc::clone(&state);
                move |tool, args| {
                    let state = Arc::clone(&state);
                    let recorded = Arc::clone(&recorded_calls);
                    async move {
                        let mut state = state.lock().unwrap();
                        let at = state.0;
                        state.0 += 1;
                        let Some(call) = recorded.get(at) else {
                            state.1.get_or_insert(format!(
                                "多调了一次 {tool}（录像只有 {} 次）",
                                recorded.len()
                            ));
                            return Err("录像用完了".to_string());
                        };
                        if call["name"] != tool || call["args"] != args {
                            state.1.get_or_insert(format!(
                                "第 {} 次调用不一致：录像 {}({}) 实际 {tool}({args})",
                                at + 1,
                                call["name"],
                                call["args"]
                            ));
                        }
                        match &call["error"] {
                            Value::Null => Ok(call["value"].clone()),
                            error => Err(error
                                .as_str()
                                .map_or_else(|| error.to_string(), str::to_owned)),
                        }
                    }
                }
            },
        )
        .await;
    let (calls, diverged) = state.lock().unwrap().clone();
    let (status, detail) = match (&outcome.incompatible, &outcome.error, &diverged) {
        (Some(message), _, _) => ("incompatible", message.clone()),
        (None, Some(error), _) => (
            "script_error",
            error.lines().last().unwrap_or("").to_owned(),
        ),
        (None, None, Some(detail)) => ("diverged", detail.clone()),
        _ if calls != recorded.len() => (
            "fewer_calls",
            format!("录像 {} 次，实际 {calls} 次", recorded.len()),
        ),
        _ => ("ok", String::new()),
    };
    report(name, status, recorded.len(), calls, &detail)
}

fn report(name: &str, status: &str, recorded: usize, actual: usize, detail: &str) -> Value {
    json!({
        "file": name, "status": status, "recorded_calls": recorded, "actual_calls": actual,
        "detail": detail.chars().take(160).collect::<String>(),
    })
}
