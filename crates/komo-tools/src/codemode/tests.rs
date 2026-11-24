//! 进程内 worker 上的 djs 引擎、流式并发协议与安全边界测试；真子进程的验收在 `crates/komo/tests`。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use komo_kernel::types::tool::ToolNamespace;
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::djs::DjsEngine;
use super::engine::{EngineHost, ScriptEngine, ToolCall};
use super::worker::{Limits, Request};
use super::*;

const TIMEOUT: Duration = Duration::from_secs(30);

async fn run(code: &str, tools: &[&str]) -> ScriptOutcome {
    let tools: Vec<String> = tools.iter().map(|name| (*name).to_string()).collect();
    Worker::in_process()
        .run(
            code,
            &tools,
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Err("本测试不调工具".to_string()) },
        )
        .await
}

#[tokio::test]
async fn typescript_script_talks_to_the_host_and_returns_only_its_text() {
    let outcome = Worker::in_process()
        .run(
            r#"
interface ReadRes { status: string; text: string }
const r = (await tools.read({ path: "a.txt" })) as ReadRes;
text(r.text.toUpperCase());
console.log("done", 42);
"#,
            &["read".to_string()],
            &ScriptCatalog::default(),
            TIMEOUT,
            |name, args| async move {
                assert_eq!(name, "read");
                assert_eq!(args["path"], "a.txt");
                Ok(json!({ "status": "completed", "text": "hello" }))
            },
        )
        .await;
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.outputs, vec!["HELLO".to_string()]);
    assert_eq!(outcome.console, "done 42\n");
}

#[tokio::test]
async fn top_level_return_works_without_calling_text() {
    let outcome = run(
        "const nums: number[] = [1, 2, 3, 4]; return { sum: nums.reduce((a, b) => a + b, 0) };",
        &[],
    )
    .await;
    assert_eq!(outcome.error, None);
    let parsed: Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(parsed, json!({ "sum": 10 }));
}

#[tokio::test]
async fn hyphens_and_underscores_alias_the_same_tool_and_undefined_args_are_stripped() {
    let outcome = Worker::in_process()
        .run(
            r#"
const a = await tools.mcp__dev_radius__search({ q: "x", limit: 2, empty: undefined });
const b = await tools["mcp__dev-radius__search"]({ q: "y" });
text([a.args, b.args, tools["mcp__dev-radius__search"] === tools.mcp__dev_radius__search]);
"#,
            &["mcp__dev-radius__search".to_string()],
            &ScriptCatalog::default(),
            TIMEOUT,
            |name, args| async move {
                assert_eq!(name, "mcp__dev-radius__search");
                Ok(json!({ "args": args }))
            },
        )
        .await;
    assert_eq!(outcome.error, None);
    let echoed: Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(
        echoed,
        json!([{ "q": "x", "limit": 2 }, { "q": "y" }, true])
    );
}

#[tokio::test]
async fn a_refused_call_rejects_with_a_real_tool_error_distinct_from_type_error() {
    let outcome = Worker::in_process()
        .run(
            r#"
let caughtTool = false;
let msg = "";
try {
  await tools.write({ path: "x" });
} catch (e) {
  caughtTool = e instanceof ToolError;
  msg = (e as Error).message;
}
let ownIsTool = false;
try {
  throw new TypeError("plain");
} catch (e) {
  ownIsTool = e instanceof ToolError;
}
text({ caughtTool, msg, ownIsTool });
"#,
            &["write".to_string()],
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Err("会改东西".to_string()) },
        )
        .await;
    assert_eq!(outcome.error, None);
    let parsed: Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(
        parsed,
        json!({ "caughtTool": true, "msg": "会改东西", "ownIsTool": false })
    );
}

#[tokio::test]
async fn an_unknown_tool_name_throws_tool_error_with_closest_suggestions() {
    let outcome = run(
        "await tools.mcp_memos_search_memos({})",
        &["read", "rg", "mcp__memos__search_memos"],
    )
    .await;
    let error = outcome.error.unwrap();
    assert!(error.contains("ToolError"), "{error}");
    assert!(
        error.contains("最接近的是 tools.mcp__memos__search_memos"),
        "{error}"
    );
}

#[tokio::test]
async fn tools_can_be_bound_stored_and_checked_with_in() {
    let outcome = Worker::in_process()
        .run(
            r#"
const read = tools.read;
const fns = { rg: tools.rg };
const [a, b, c] = await Promise.all([
  read({ path: "a" }),
  fns.rg({ pattern: "b" }),
  tools["read"]({ path: "c" }),
]);
text([a.n, b.n, c.n, "nope" in tools, "read" in tools]);
"#,
            &["read".to_string(), "rg".to_string()],
            &ScriptCatalog::default(),
            TIMEOUT,
            |name, _| async move { Ok(json!({ "n": name })) },
        )
        .await;
    assert_eq!(outcome.error, None);
    let seen: Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(seen, json!(["read", "rg", "read", false, true]));
}

#[tokio::test]
async fn an_exception_keeps_the_partial_output_and_reports_the_exact_user_line() {
    let outcome = run("text('前半');\nconst x = 1;\nthrow new Error('坏了');", &[]).await;
    assert_eq!(outcome.outputs, vec!["前半".to_string()]);
    let error = outcome.error.unwrap();
    assert!(error.contains("Error: 坏了"), "{error}");
    assert!(error.contains("codemode.ts:3"), "{error}");
}

#[tokio::test]
async fn a_script_makes_at_most_256_tool_calls() {
    let outcome = Worker::in_process()
        .run(
            r#"
const ps = [];
for (let i = 0; i < 300; i++) ps.push(tools.echo({ i }));
const rs = await Promise.allSettled(ps);
text(rs.filter((r) => r.status === "rejected").length);
"#,
            &["echo".to_string()],
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Ok(json!({})) },
        )
        .await;
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.outputs, vec!["44".to_string()]);
}

#[tokio::test]
async fn the_sandbox_has_no_io_or_env_and_freezes_builtins() {
    let outcome = run(
        r#"
let frozen = false;
try { (Array.prototype as any).hack = 1; } catch { frozen = true; }
text({
  frozen,
  fetch: typeof (globalThis as any).fetch,
  process: typeof (globalThis as any).process,
  require: typeof (globalThis as any).require,
  Deno: typeof (globalThis as any).Deno,
  Bun: typeof (globalThis as any).Bun,
});
"#,
        &[],
    )
    .await;
    assert_eq!(outcome.error, None);
    let verdict: Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(
        verdict,
        json!({
            "frozen": true,
            "fetch": "undefined",
            "process": "undefined",
            "require": "undefined",
            "Deno": "undefined",
            "Bun": "undefined",
        })
    );
}

#[tokio::test]
async fn text_formats_objects_sets_maps_bigints_and_utf8() {
    let outcome = run(
        r#"
text({ z: 1, a: [1, 2.5, true, null], 中: "文\n", s: new Set([1, 2]), b: 9007199254740993n });
text(7);
"#,
        &[],
    )
    .await;
    assert_eq!(outcome.error, None);
    let parsed: Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(
        parsed,
        json!({
            "z": 1,
            "a": [1, 2.5, true, null],
            "中": "文\n",
            "s": [1, 2],
            "b": "9007199254740993",
        })
    );
    assert_eq!(outcome.outputs[1], "7");
}

#[tokio::test]
async fn discovery_ranks_cjk_and_english_queries_and_describes_namespaces() {
    let catalog = ScriptCatalog {
        tools: vec![
            ScriptTool {
                name: "read".into(),
                description: "读取本地文件内容".into(),
                parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
                namespace: None,
            },
            ScriptTool {
                name: "mcp__dev-radius__search_issues".into(),
                description: "按关键词搜索线上工单与缺陷".into(),
                parameters: json!({ "type": "object", "properties": { "query": { "type": "string" } } }),
                namespace: Some("mcp__dev-radius".into()),
            },
            ScriptTool {
                name: "mcp__notes__find_notes".into(),
                description: "Find personal notes by query".into(),
                parameters: json!({ "type": "object" }),
                namespace: Some("mcp__notes".into()),
            },
        ],
        namespaces: vec![
            ToolNamespace {
                name: "mcp__dev-radius".into(),
                server: "dev-radius".into(),
                description: Some("研发工单系统".into()),
                instructions: Some("查询工单时先限定项目".into()),
            },
            ToolNamespace {
                name: "mcp__notes".into(),
                server: "notes".into(),
                description: Some("Notes".into()),
                instructions: None,
            },
        ],
    };
    let outcome = Worker::in_process()
        .run(
            r#"
const cjk = searchTools("搜索工单");
const en = search_tools("personal note", 5, "notes");
const tool = describeTool("mcp__dev_radius__search_issues");
const ns = describeNamespace("dev_radius");
text({ cjk, en, toolName: tool?.name, nsTools: ns?.tools, all: ALL_TOOLS.map((t) => t.name) });
"#,
            &[
                "read".into(),
                "mcp__dev-radius__search_issues".into(),
                "mcp__notes__find_notes".into(),
            ],
            &catalog,
            TIMEOUT,
            |_, _| async { Err(String::new()) },
        )
        .await;
    assert_eq!(outcome.error, None);
    let parsed: Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(parsed["cjk"][0]["name"], "mcp__dev_radius__search_issues");
    assert_eq!(parsed["en"][0]["name"], "mcp__notes__find_notes");
    assert_eq!(parsed["toolName"], "mcp__dev_radius__search_issues");
    assert_eq!(parsed["nsTools"], json!(["mcp__dev_radius__search_issues"]));
    assert_eq!(
        parsed["all"],
        json!([
            "read",
            "mcp__dev_radius__search_issues",
            "mcp__notes__find_notes"
        ])
    );
}

#[tokio::test]
async fn the_clock_and_entropy_come_from_the_host_and_are_deterministic() {
    let make_nondet = || -> NondetHandler {
        Arc::new(|request| match request {
            Nondet::Clock => Ok(json!(86_400.5)),
            Nondet::Urandom { size } => Ok(json!("ab".repeat(*size))),
            Nondet::Deliver { .. } => Err("unexpected".into()),
        })
    };
    let code = "text([Date.now(), new Date().toISOString(), Math.random(), Math.random()]);";
    let a = Worker::in_process()
        .with_nondet(make_nondet())
        .run(
            code,
            &[],
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Err(String::new()) },
        )
        .await;
    let b = Worker::in_process()
        .with_nondet(make_nondet())
        .run(
            code,
            &[],
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Err(String::new()) },
        )
        .await;
    assert_eq!(a.error, None);
    assert_eq!(a.outputs, b.outputs);
    let parsed: Value = serde_json::from_str(&a.outputs[0]).unwrap();
    assert_eq!(parsed[0], 86_400_500i64);
    assert_eq!(parsed[1], "1970-01-02T00:00:00.500Z");
}

#[tokio::test]
async fn a_worker_that_speaks_another_protocol_is_refused_before_the_script_runs() {
    let stale = Hello {
        protocol: WORKER_PROTOCOL - 1,
        ..Hello::current()
    };
    let outcome = Worker::in_process_as(stale)
        .run(
            "text('ran')",
            &[],
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Err(String::new()) },
        )
        .await;
    assert!(outcome.incompatible.unwrap().contains("重启 Gateway"));
    assert!(outcome.outputs.is_empty());
}

#[tokio::test]
async fn a_worker_from_another_engine_build_is_refused_before_the_script_runs() {
    let stale = Hello {
        engine: "djs-00000000".into(),
        ..Hello::current()
    };
    let outcome = Worker::in_process_as(stale)
        .run(
            "text('ran')",
            &[],
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Err(String::new()) },
        )
        .await;
    assert!(outcome.incompatible.unwrap().contains("重启 Gateway"));
    assert!(outcome.outputs.is_empty());
}

/// `komo update` 换了磁盘上的二进制、Gateway 还没重启：协议与引擎构建号没变就照常跑。
#[tokio::test]
async fn a_worker_from_another_komo_build_on_the_same_protocol_still_runs() {
    let newer = Hello {
        komo: "99.0.0".into(),
        ..Hello::current()
    };
    let outcome = Worker::in_process_as(newer)
        .run(
            "text('ran')",
            &[],
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Err(String::new()) },
        )
        .await;
    assert!(outcome.incompatible.is_none(), "{:?}", outcome.incompatible);
    assert_eq!(outcome.outputs, vec!["ran".to_string()]);
}

fn rendezvous()
-> impl FnMut(String, Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>> {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    move |name, args| {
        let barrier = barrier.clone();
        Box::pin(async move {
            barrier.wait().await;
            Ok(json!({
                "status": "completed",
                "text": format!("{name}:{}", args["path"].as_str().unwrap_or_default())
            }))
        })
    }
}

#[tokio::test]
async fn promise_all_runs_calls_concurrently_and_returns_in_argument_order() {
    let outcome = Worker::in_process()
        .run(
            r#"
const [a, b] = await Promise.all([
  tools.read({ path: "a" }),
  tools.read({ path: "b" }),
]);
text([a.text, b.text]);
"#,
            &["read".to_string()],
            &ScriptCatalog::default(),
            Duration::from_secs(5),
            rendezvous(),
        )
        .await;
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.outputs, vec!["[\n  \"read:a\",\n  \"read:b\"\n]"]);
}

#[tokio::test]
async fn promise_all_settled_keeps_every_result_when_one_call_fails() {
    let outcome = Worker::in_process()
        .run(
            r#"
const rs = await Promise.allSettled([
  tools.read({ path: "a" }),
  tools.read({ path: "missing" }),
  tools.read({ path: "c" }),
]);
text(rs.map((r) => r.status === "fulfilled" ? r.value.text : r.reason.message));
"#,
            &["read".to_string()],
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, args| async move {
                match args["path"].as_str() {
                    Some("missing") => Err("没有这个文件".to_string()),
                    path => Ok(json!({ "text": path })),
                }
            },
        )
        .await;
    assert_eq!(outcome.error, None);
    let seen: Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(seen, json!(["a", "没有这个文件", "c"]));
}

/// 流式 `wait` 核心验收：慢调用 `slow` 在闸门上卡住时，快链 `fast -> hop2` 不被批次屏障拖住，
/// `hop2` 到达宿主后主动放开 `slow` 的闸门——如果还是旧的全量批次屏障，这个测试必死锁超时！
#[tokio::test]
async fn streaming_wait_lets_a_fast_chain_start_its_second_hop_while_a_slow_call_is_blocked() {
    let slow_gate = Arc::new(Notify::new());
    let outcome = Worker::in_process()
        .run(
            r#"
const [slow, fastChain] = await Promise.all([
  tools.slow({}),
  tools.fast({}).then((r) => tools.hop2({ from: r.v })),
]);
text([slow.v, fastChain.v]);
"#,
            &["slow".to_string(), "fast".to_string(), "hop2".to_string()],
            &ScriptCatalog::default(),
            Duration::from_secs(5),
            move |name, args| {
                let gate = slow_gate.clone();
                async move {
                    match name.as_str() {
                        "slow" => {
                            gate.notified().await;
                            Ok(json!({ "v": "S" }))
                        }
                        "fast" => Ok(json!({ "v": "F1" })),
                        "hop2" => {
                            // 第二跳启动证明 fast 已经在 slow 未完成前被流式交付！放开 slow。
                            gate.notify_one();
                            Ok(json!({ "v": format!("{}+H2", args["from"].as_str().unwrap()) }))
                        }
                        other => Err(format!("unknown {other}")),
                    }
                }
            },
        )
        .await;
    assert_eq!(outcome.error, None);
    let seen: Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(seen, json!(["S", "F1+H2"]));
}

#[tokio::test]
async fn calls_reach_the_host_in_script_order_and_unawaited_ones_still_finish() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let finished = Arc::new(AtomicUsize::new(0));
    let (log, done) = (seen.clone(), finished.clone());
    let outcome = Worker::in_process()
        .run(
            r#"
const ps = [0, 1, 2, 3, 4].map((i) => tools.read({ path: String(i) }));
tools.read({ path: "forgotten" });
const rs = await Promise.all(ps);
text(rs.length);
"#,
            &["read".to_string()],
            &ScriptCatalog::default(),
            TIMEOUT,
            move |_, args| {
                log.lock()
                    .unwrap()
                    .push(args["path"].as_str().unwrap().to_string());
                let done = done.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    done.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({}))
                }
            },
        )
        .await;
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.outputs, vec!["5".to_string()]);
    assert_eq!(
        *seen.lock().unwrap(),
        ["0", "1", "2", "3", "4", "forgotten"]
    );
    assert_eq!(finished.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn memory_cap_is_hard_and_cannot_be_caught_by_script() {
    let outcome = Worker::in_process()
        .with_memory_limit(8 << 20)
        .run(
            r#"
const chunks: string[] = [];
try {
  while (true) chunks.push("x".repeat(1 << 20));
} catch {
  text("swallowed");
}
"#,
            &[],
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Err(String::new()) },
        )
        .await;
    assert!(outcome.outputs.is_empty(), "OOM 绝不能被脚本 catch 吞掉");
    let err = outcome.error.unwrap();
    assert!(err.contains("内存超限"), "{err}");
}

#[test]
fn infinite_loop_stops_cleanly_in_process_by_fuel_and_deadline() {
    struct DummyHost;
    impl EngineHost for DummyHost {
        fn call(&mut self, _: Result<ToolCall, String>) -> Result<Value, String> {
            Err("unused".into())
        }
        fn defer(&mut self, _: u32, _: Result<ToolCall, String>) {}
        fn wait(&mut self, _: &[u32]) -> Vec<(u32, Result<Value, String>)> {
            Vec::new()
        }
        fn nondet(&mut self, ask: &Nondet) -> Result<Value, String> {
            match ask {
                Nondet::Clock => Ok(json!(0.0)),
                Nondet::Urandom { size } => Ok(json!("01".repeat(*size))),
                Nondet::Deliver { .. } => Err("unused".into()),
            }
        }
        fn text(&mut self, _: String) -> Result<(), String> {
            Ok(())
        }
    }
    let req = Request {
        code: "while (true) {}".into(),
        limits: Limits {
            max_memory: Some(16 << 20),
            max_turn_ms: Some(100),
            max_calls: Some(256),
        },
        ..Request::default()
    };
    let started = Instant::now();
    let end = DjsEngine.run(&req, &mut DummyHost);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(end.error.unwrap().contains("超时"));
}

#[test]
fn compile_check_rejects_python_syntax_with_migration_hint() {
    let err =
        worker::compile_check("import json\nfor p in ['a']:\n    tools.read(path=p)").unwrap_err();
    assert!(err.contains("SyntaxError"), "{err}");
    assert!(err.contains("TypeScript / JavaScript"), "{err}");
    assert!(worker::compile_check("const r = await tools.read({ path: 'a' }); text(r);").is_ok());
}
