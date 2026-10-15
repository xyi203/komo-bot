//! 进程内 worker 上的协议与语言限制；要杀进程的（死循环、内存看门狗）在 `crates/komo/tests`。

use komo_kernel::types::tool::ToolNamespace;
use serde_json::json;

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
async fn a_script_talks_to_the_host_and_returns_only_its_text() {
    let outcome = Worker::in_process()
        .run(
            "r = tools.read(path='a.txt')\ntext(r['text'].upper())\nprint('done')",
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
    assert_eq!(outcome.console, "done\n");
}

#[tokio::test]
async fn hyphens_in_tool_names_are_written_as_underscores_and_a_dict_merges_into_the_keywords() {
    let outcome = Worker::in_process()
        .run(
            "text(tools.mcp__dev_radius__search({'q': 'x'}, limit=2))",
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
    let echoed: serde_json::Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(echoed, json!({ "args": { "q": "x", "limit": 2 } }));
}

#[tokio::test]
async fn a_refused_call_raises_a_tool_error_the_script_can_catch() {
    let outcome = Worker::in_process()
        .run(
            "try:\n    tools.write(path='x')\nexcept ToolError as e:\n    text('拒了：' + str(e))",
            &["write".to_string()],
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Err("会改东西".to_string()) },
        )
        .await;
    assert_eq!(outcome.outputs, vec!["拒了：会改东西".to_string()]);
}

#[tokio::test]
async fn an_unknown_tool_name_is_an_attribute_error() {
    let outcome = run("tools.nope()", &["read", "rg"]).await;
    let error = outcome.error.unwrap();
    assert!(error.contains("AttributeError"), "{error}");
    assert!(error.contains("nope"), "{error}");
}

/// `tools.<name>` 是真的属性：绑到变量、放进列表、`getattr` 动态取，都能调。
#[tokio::test]
async fn tools_can_be_bound_stored_and_fetched_by_name() {
    let outcome = Worker::in_process()
        .run(
            r#"
read = tools.read
fns = {"rg": tools.rg}
text([read(path="a")["n"], fns["rg"](pattern="b")["n"], getattr(tools, "read")(path="c")["n"], hasattr(tools, "nope")])
"#,
            &["read".to_string(), "rg".to_string()],
            &ScriptCatalog::default(),
            TIMEOUT,
            |name, _| async move { Ok(json!({ "n": name })) },
        )
        .await;
    assert_eq!(outcome.error, None);
    let seen: serde_json::Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(seen, json!(["read", "rg", "read", false]));
}

#[tokio::test]
async fn an_exception_keeps_the_partial_output_and_reports_the_script_line() {
    let outcome = run("text('前半')\nx = 1\nraise ValueError('坏了')", &[]).await;
    assert_eq!(outcome.outputs, vec!["前半".to_string()]);
    let error = outcome.error.unwrap();
    assert!(error.contains("ValueError: 坏了"), "{error}");
    assert!(error.contains("line 3"), "{error}");
}

#[tokio::test]
async fn a_script_makes_at_most_256_tool_calls() {
    let tools = ["echo".to_string()];
    let outcome = Worker::in_process()
        .run(
            "n = 0\nfor i in range(300):\n    tools.echo()\n    n += 1\ntext(n)",
            &tools,
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Ok(json!({})) },
        )
        .await;
    let error = outcome.error.unwrap();
    assert!(error.contains("一段脚本最多调 256 次工具"), "{error}");
    assert!(outcome.outputs.is_empty());
}

#[tokio::test]
async fn the_language_has_no_files_environment_network_or_subprocess() {
    let outcome = run(
        r#"
def blocked(f):
    try:
        f()
        return False
    except Exception:
        return True

import os
text({
    "read": blocked(lambda: open("/etc/passwd").read()),
    "write": blocked(lambda: open("/tmp/komo-codemode-probe", "w").write("x")),
    "path": blocked(lambda: __import__("pathlib").Path("/etc/passwd").read_text()),
    "subprocess": blocked(lambda: __import__("subprocess")),
    "socket": blocked(lambda: __import__("socket")),
    "env": os.getenv("HOME"),
    "environ": dict(os.environ),
})
"#,
        &[],
    )
    .await;
    assert_eq!(outcome.error, None);
    let verdict: serde_json::Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(
        verdict,
        json!({
            "read": true, "write": true, "path": true, "subprocess": true, "socket": true,
            "env": null, "environ": {},
        })
    );
    assert!(!std::path::Path::new("/tmp/komo-codemode-probe").exists());
}

fn discovery_catalog() -> ScriptCatalog {
    let tool =
        |name: &str, description: &str, parameters: serde_json::Value, namespace: Option<&str>| {
            ScriptTool {
                name: name.into(),
                description: description.into(),
                parameters,
                namespace: namespace.map(str::to_string),
            }
        };
    let string = |description: &str| json!({ "type": "string", "description": description });
    let object =
        |properties: serde_json::Value| json!({ "type": "object", "properties": properties });
    ScriptCatalog {
        tools: vec![
            tool(
                "read",
                "Read a file from the workspace",
                object(json!({ "path": string("file path") })),
                None,
            ),
            tool(
                "rg",
                "Search file contents with a regex",
                object(json!({ "pattern": string("regex to search") })),
                None,
            ),
            tool(
                "mcp__dev-radius__search",
                "搜索代码仓库里的符号与文件",
                object(json!({ "query": string("搜索关键词") })),
                Some("mcp__dev-radius"),
            ),
            tool(
                "mcp__dev-radius__open_issue",
                "打开一个 issue 并返回详情",
                object(json!({ "id": { "type": "integer", "description": "issue 编号" } })),
                Some("mcp__dev-radius"),
            ),
            tool(
                "mcp__notes__find_notes",
                "Find notes by title or tag, searches the notebook",
                object(json!({ "tag": { "type": "string" } })),
                Some("mcp__notes"),
            ),
        ],
        namespaces: vec![
            ToolNamespace {
                name: "mcp__dev-radius".into(),
                server: "dev-radius".into(),
                description: Some("代码仓库平台".into()),
                instructions: Some("先搜索再打开".into()),
            },
            ToolNamespace {
                name: "mcp__notes".into(),
                server: "notes".into(),
                description: Some("Notebook".into()),
                instructions: None,
            },
        ],
    }
}

/// 期望值是 2026-10-15 用迁移前的 CPython `driver.py` 对同一份目录跑出来的：BM25 排序、
/// 中文分词、`describe_*` 的形状都不许因为换了引擎而变。（`parameters` 里的键序是排过的：目录经
/// `serde_json::Value` 交进来，那边对象按键排序，迁移前后一样。）
#[tokio::test]
async fn discovery_ranks_and_describes_exactly_as_the_cpython_driver_did() {
    let catalog = discovery_catalog();
    let names: Vec<String> = catalog.tools.iter().map(|tool| tool.name.clone()).collect();
    let outcome = Worker::in_process()
        .run(
            r#"
text([
    search_tools("搜索"),
    search_tools("search files", 2),
    search_tools("issue 编号"),
    search_tools("notes tag"),
    search_tools("仓库", namespace="dev-radius"),
    search_tools("nothing-matches-zzz"),
])
text(describe_tool("mcp__dev_radius__open_issue"))
text(describe_namespace("dev_radius"))
text(describe_tool("nope"))
text([tool["name"] for tool in ALL_TOOLS])
"#,
            &names,
            &catalog,
            TIMEOUT,
            |_, _| async { Err(String::new()) },
        )
        .await;
    assert_eq!(outcome.error, None);
    let hit = |name: &str, description: &str| json!({ "name": name, "description": description });
    let search = hit("mcp__dev_radius__search", "搜索代码仓库里的符号与文件");
    let issue = hit("mcp__dev_radius__open_issue", "打开一个 issue 并返回详情");
    let notes = hit(
        "mcp__notes__find_notes",
        "Find notes by title or tag, searches the notebook",
    );
    let rankings: serde_json::Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(
        rankings,
        json!([
            [search, issue],
            [
                hit("rg", "Search file contents with a regex"),
                hit("read", "Read a file from the workspace")
            ],
            [issue, search],
            [notes],
            [search, issue],
            [],
        ])
    );
    assert_eq!(
        outcome.outputs[1],
        r#"{
  "name": "mcp__dev_radius__open_issue",
  "description": "打开一个 issue 并返回详情",
  "parameters": {
    "properties": {
      "id": {
        "description": "issue 编号",
        "type": "integer"
      }
    },
    "type": "object"
  },
  "namespace": "mcp__dev-radius"
}"#
    );
    assert_eq!(
        outcome.outputs[2],
        r#"{
  "name": "mcp__dev-radius",
  "description": "代码仓库平台",
  "instructions": "先搜索再打开",
  "tools": [
    "mcp__dev_radius__search",
    "mcp__dev_radius__open_issue"
  ]
}"#
    );
    assert_eq!(outcome.outputs[3], "null");
    let all: Vec<String> = serde_json::from_str(&outcome.outputs[4]).unwrap();
    assert_eq!(
        all,
        [
            "read",
            "rg",
            "mcp__dev_radius__search",
            "mcp__dev_radius__open_issue",
            "mcp__notes__find_notes"
        ]
    );
}

#[test]
fn the_engine_version_in_the_fingerprint_is_the_pinned_one() {
    let manifest = include_str!("../../../../Cargo.toml");
    assert!(
        manifest.contains(&format!("monty = \"={MONTY_VERSION}\"")),
        "Cargo.toml 里 monty 的钉版本与 MONTY_VERSION 不一致"
    );
}

/// `text()` 的非字符串输出逐字节同 Python 的 `json.dumps(indent=2, ensure_ascii=False, default=str)`：
/// dict 保留插入序、空容器不换行、非 ASCII 原样、tuple 是数组。（期望值由 CPython 3 跑出。）
#[tokio::test]
async fn text_renders_non_strings_like_python_json_dumps() {
    let outcome = run(
        r#"
d = {"z": 1, "a": [1, 2.5, True, None], "中": "文\n", "e": {}, "l": [], "t": (1, 2), 3: "n"}
text(d)
text(7)
text([{"k": 1}])
"#,
        &[],
    )
    .await;
    assert_eq!(outcome.error, None);
    assert_eq!(
        outcome.outputs[0],
        "{\n  \"z\": 1,\n  \"a\": [\n    1,\n    2.5,\n    true,\n    null\n  ],\n  \"中\": \"文\\n\",\n  \"e\": {},\n  \"l\": [],\n  \"t\": [\n    1,\n    2\n  ],\n  \"3\": \"n\"\n}"
    );
    assert_eq!(outcome.outputs[1], "7");
    assert_eq!(outcome.outputs[2], "[\n  {\n    \"k\": 1\n  }\n]");
}
