//! 进程内 worker 上的协议与语言限制；要杀进程的（死循环、内存看门狗）在 `crates/komo/tests`。

use komo_kernel::types::tool::ToolNamespace;
use serde_json::json;
use std::sync::Arc;

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
async fn the_language_has_no_network_subprocess_or_environment() {
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
        json!({ "subprocess": true, "socket": true, "env": null, "environ": {} })
    );
}

/// 一个像数据目录的临时目录：`sessions/` `runtime/` `state.db*` `.env`，外加一个允许读的工作区。
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for path in ["sessions", "runtime", "workspace/sub"] {
            std::fs::create_dir_all(root.join(path)).unwrap();
        }
        for (path, body) in [
            ("sessions/x", "session"),
            ("runtime/x", "runtime"),
            ("state.db", "db"),
            ("state.db-wal", "wal"),
            (".env", "SECRET=1"),
            ("workspace/a.txt", "alpha"),
            ("workspace/sub/b.txt", "beta"),
        ] {
            std::fs::write(root.join(path), body).unwrap();
        }
        std::os::unix::fs::symlink(root.join(".env"), root.join("workspace/link")).unwrap();
        std::os::unix::fs::symlink(root.join("workspace/a.txt"), root.join("workspace/alias"))
            .unwrap();
        Self { dir }
    }

    fn root(&self) -> std::path::PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn worker(&self) -> Worker {
        let root = self.root();
        Worker::in_process()
            .in_dir(root.join("workspace"))
            .protecting(vec![
                root.join("sessions"),
                root.join("state.db"),
                root.join("runtime"),
                root.join(".env"),
            ])
    }

    async fn run(&self, code: &str) -> ScriptOutcome {
        self.worker()
            .run(
                &code.replace("{root}", &self.root().to_string_lossy()),
                &[],
                &ScriptCatalog::default(),
                TIMEOUT,
                |_, _| async { Err("本测试不调工具".to_string()) },
            )
            .await
    }
}

#[tokio::test]
async fn files_are_readable_through_open_and_pathlib() {
    let home = Home::new();
    let outcome = home
        .run(
            r#"
import os
from pathlib import Path
text(open("/etc/hosts").read() != "")
text(Path("{root}/workspace/a.txt").read_text())
text(open("a.txt").read())
text(open("alias").read())
text(sorted(p.name for p in Path("{root}/workspace").iterdir()))
text([Path("a.txt").exists(), Path("nope").exists(), Path("sub").is_dir(), Path("a.txt").stat().st_size])
text(os.getcwd() == "{root}/workspace")
"#,
        )
        .await;
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.outputs[0], "true");
    assert_eq!(outcome.outputs[1], "alpha");
    assert_eq!(outcome.outputs[2], "alpha");
    assert_eq!(outcome.outputs[3], "alpha");
    let names: Vec<String> = serde_json::from_str(&outcome.outputs[4]).unwrap();
    assert!(names.contains(&"a.txt".to_string()) && names.contains(&"sub".to_string()));
    assert_eq!(outcome.outputs[5], "[\n  true,\n  false,\n  true,\n  5\n]");
    assert_eq!(outcome.outputs[6], "true");
}

#[tokio::test]
async fn komo_own_state_is_unreadable_even_through_dotdot_and_symlinks() {
    let home = Home::new();
    let outcome = home
        .run(
            r#"
from pathlib import Path
def verdict(path):
    try:
        Path(path).read_text()
        return "read"
    except PermissionError:
        return "denied"
    except Exception as e:
        return "other " + type(e).__name__

paths = [
    "{root}/.env", "{root}/state.db", "{root}/state.db-wal", "{root}/sessions/x", "{root}/runtime/x",
    "../.env", "sub/../../state.db", "{root}/workspace/../sessions/x", "link",
]
text({p: verdict(p) for p in paths})
text(verdict("a.txt"))
try:
    open("../.env").read()
    text("open read")
except PermissionError:
    text("open denied")
"#,
        )
        .await;
    assert_eq!(outcome.error, None);
    let verdicts: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&outcome.outputs[0]).unwrap();
    for (path, verdict) in &verdicts {
        assert_eq!(verdict, "denied", "{path}");
    }
    assert_eq!(verdicts.len(), 9);
    assert_eq!(outcome.outputs[1], "read");
    assert_eq!(outcome.outputs[2], "open denied");
}

#[tokio::test]
async fn every_write_form_is_refused_and_leaves_the_disk_alone() {
    let home = Home::new();
    let outcome = home
        .run(
            r#"
from pathlib import Path
attempts = {
    "write_text": lambda: Path("new.txt").write_text("x"),
    "write_bytes": lambda: Path("new.bin").write_bytes(b"x"),
    "overwrite": lambda: Path("a.txt").write_text("x"),
    "open_w": lambda: open("new2.txt", "w").write("x"),
    "open_a": lambda: open("a.txt", "a").write("x"),
    "open_rplus": lambda: open("a.txt", "r+").write("x"),
    "mkdir": lambda: Path("d").mkdir(),
    "unlink": lambda: Path("a.txt").unlink(),
    "rmdir": lambda: Path("sub").rmdir(),
    "rename": lambda: Path("a.txt").rename("c.txt"),
}
result = {}
for name, attempt in attempts.items():
    try:
        attempt()
        result[name] = "done"
    except PermissionError as e:
        result[name] = "write / edit" in str(e)
    except Exception as e:
        result[name] = "other " + type(e).__name__
text(result)
"#,
        )
        .await;
    assert_eq!(outcome.error, None);
    let result: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&outcome.outputs[0]).unwrap();
    for (name, verdict) in &result {
        assert!(
            *verdict == json!(true) || name == "open_rplus",
            "{name}: {verdict}"
        );
    }
    let workspace = home.root().join("workspace");
    let mut names: Vec<_> = std::fs::read_dir(&workspace)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["a.txt", "alias", "link", "sub"]);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "alpha"
    );
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

#[tokio::test]
async fn the_clock_and_entropy_come_from_the_host() {
    let seen: Arc<std::sync::Mutex<Vec<Nondet>>> = Arc::default();
    let log = Arc::clone(&seen);
    let outcome = Worker::in_process()
        .with_nondet(Arc::new(move |request| {
            log.lock().unwrap().push(request.clone());
            match request {
                Nondet::Clock => Ok(json!(86_400.5)),
                Nondet::Urandom { size } => Ok(json!("ab".repeat(*size))),
            }
        }))
        .run(
            "import time, os\nfrom datetime import datetime, date\ntext([time.time(), str(datetime.now()), str(date.today()), os.urandom(2).hex()])\ntime.sleep(0.01)",
            &[],
            &ScriptCatalog::default(),
            TIMEOUT,
            |_, _| async { Err(String::new()) },
        )
        .await;
    assert_eq!(outcome.error, None);
    let read: serde_json::Value = serde_json::from_str(&outcome.outputs[0]).unwrap();
    assert_eq!(
        read,
        json!([86_400.5, "1970-01-02 00:00:00.500000", "1970-01-02", "abab"])
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [
            Nondet::Clock,
            Nondet::Clock,
            Nondet::Clock,
            Nondet::Urandom { size: 2 }
        ]
    );
}

#[tokio::test]
async fn a_worker_that_is_another_build_is_refused_before_the_script_runs() {
    let stale = Hello {
        komo: "0.0.0".into(),
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
