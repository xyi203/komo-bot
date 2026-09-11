//! These run a real Python interpreter against the real embedded host —
//! the protocol has two sides and testing one against a mock of the other
//! proves only that the mock agrees with itself.

use super::*;

fn python() -> Option<String> {
    for candidate in ["python3", "python"] {
        if std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok()
        {
            return Some(candidate.to_string());
        }
    }
    None
}

/// A temp dir holding a `plugins/` directory, cleaned up on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("komo-pyhost-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("plugins")).unwrap();
        Self(path)
    }

    fn home(&self) -> &Path {
        &self.0
    }

    fn plugins(&self) -> PathBuf {
        self.0.join("plugins")
    }

    fn write(&self, name: &str, source: &str) {
        std::fs::write(self.plugins().join(name), source).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A broker no plugin under test reaches for. `call` takes one because a
/// plugin function may compose komo's tools; most of these do not.
fn no_calls(
    name: String,
    _args: serde_json::Value,
) -> std::future::Ready<Result<ToolAnswer, String>> {
    std::future::ready(Err(format!("`{name}` is not available in this test")))
}

const GREETER: &str = r#"
from komo_plugin import tool

@tool("Greet someone by name.")
def greet(name: str, excited: bool = False) -> str:
    return f"hello {name}" + ("!" if excited else "")
"#;

#[tokio::test]
async fn a_plugin_is_loaded_described_and_callable() {
    let Some(python) = python() else {
        eprintln!("no python interpreter; skipping");
        return;
    };
    let scratch = Scratch::new("callable");
    scratch.write("greeter.py", GREETER);

    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();

    let tools = host.manifest().await.unwrap().tools;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "greet");
    assert_eq!(tools[0].description, "Greet someone by name.");
    // The signature is the contract: annotated types and which arguments
    // are required both come from it.
    assert_eq!(tools[0].parameters["properties"]["name"]["type"], "string");
    assert_eq!(
        tools[0].parameters["properties"]["excited"]["type"],
        "boolean"
    );
    assert_eq!(tools[0].parameters["required"], serde_json::json!(["name"]));

    let out = host
        .call("greet", serde_json::json!({ "name": "komo" }), no_calls)
        .await
        .unwrap();
    assert_eq!(out, "hello komo");
    let out = host
        .call(
            "greet",
            serde_json::json!({ "name": "komo", "excited": true }),
            no_calls,
        )
        .await
        .unwrap();
    assert_eq!(out, "hello komo!");

    host.shutdown().await;
}

/// A plugin raising is a result the model can work with, not a dead host:
/// the error comes back as `Plugin` (never retried, since the code already
/// rejected the call) and the next call still works.
#[tokio::test]
async fn a_raising_plugin_answers_with_an_error_and_the_host_survives() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("raising");
    scratch.write(
        "boom.py",
        r#"
from komo_plugin import tool

@tool("Always fails.")
def boom() -> str:
    raise ValueError("kaboom")

@tool("Always works.")
def fine() -> str:
    return "ok"
"#,
    );

    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();
    host.manifest().await.unwrap();

    let error = host
        .call("boom", serde_json::json!({}), no_calls)
        .await
        .expect_err("a raising tool is an error");
    assert!(format!("{error}").contains("kaboom"), "{error}");
    assert!(!error.retryable(), "the plugin already rejected this call");

    assert_eq!(
        host.call("fine", serde_json::json!({}), no_calls)
            .await
            .unwrap(),
        "ok"
    );
    host.shutdown().await;
}

/// One broken file must not cost the working ones — the agent writing a
/// plugin with a syntax error should lose that plugin and nothing else.
#[tokio::test]
async fn a_plugin_that_fails_to_import_does_not_take_the_others_down() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("broken");
    scratch.write("good.py", GREETER);
    scratch.write("bad.py", "this is not python(");

    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();
    let tools = host.manifest().await.unwrap().tools;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "greet");
    host.shutdown().await;
}

/// The point of the whole thing: write a file, and the tool shows up
/// without a restart. The host pushes the new manifest unasked.
#[tokio::test]
async fn writing_a_plugin_file_pushes_a_new_manifest() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("hotreload");

    let (host, mut events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();
    assert!(host.manifest().await.unwrap().tools.is_empty());

    scratch.write("late.py", GREETER);

    let event = tokio::time::timeout(std::time::Duration::from_secs(15), events.recv())
        .await
        .expect("the host should notice a new plugin file")
        .expect("the event stream should stay open");
    match event {
        HostEvent::ManifestChanged(manifest) => {
            assert_eq!(manifest.tools.len(), 1);
            assert_eq!(manifest.tools[0].name, "greet");
        }
        other => panic!("expected a manifest change, got {other:?}"),
    }

    // And it is callable straight away, without asking for the manifest.
    assert_eq!(
        host.call("greet", serde_json::json!({ "name": "you" }), no_calls)
            .await
            .unwrap(),
        "hello you"
    );
    host.shutdown().await;
}

/// A plugin's own `print` must not corrupt the protocol stream — it is a
/// debugging habit, not a bug, and the connection has to survive it.
#[tokio::test]
async fn plugin_stdout_does_not_corrupt_the_protocol() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("printing");
    scratch.write(
        "chatty.py",
        r#"
import sys
from komo_plugin import tool

print("noise at import time")

@tool("Prints, then answers.")
def chatty() -> str:
    print("noise during the call")
    print({"id": 1, "result": "a forged response"})
    return "answered"
"#,
    );

    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();
    assert_eq!(host.manifest().await.unwrap().tools.len(), 1);
    assert_eq!(
        host.call("chatty", serde_json::json!({}), no_calls)
            .await
            .unwrap(),
        "answered"
    );
    // Still healthy after all that noise.
    assert_eq!(host.manifest().await.unwrap().tools.len(), 1);
    host.shutdown().await;
}

/// The point of a plugin over a one-off program: it composes komo's own
/// tools, and keeps doing it. A `@tool` function reaches them through the
/// same `tools` object a program gets, brokered back over the same
/// connection.
#[tokio::test]
async fn a_plugin_tool_calls_komo_tools_through_the_same_broker() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("plugin-tools");
    scratch.write(
        "clock.py",
        r#"
from komo_plugin import tool, tools

@tool("Say what time komo thinks it is.")
def now() -> str:
    answer = tools.time()
    return f"komo says {answer} (zone {answer.structured['zone']})"
"#,
    );

    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();
    assert_eq!(host.manifest().await.unwrap().tools[0].name, "now");

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let out = host
        .call("now", serde_json::json!({}), move |name, args| {
            recorder.lock().unwrap().push((name, args));
            std::future::ready(Ok(ToolAnswer {
                content: "09:00".to_string(),
                structured: serde_json::json!({ "zone": "Asia/Shanghai" }),
            }))
        })
        .await
        .unwrap();

    assert_eq!(out, "komo says 09:00 (zone Asia/Shanghai)");
    // It reached komo as a real tool call, so it pays the same gating a
    // direct one does.
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "time");

    host.shutdown().await;
}

/// A plugin tool and a program over one host must not have their calls
/// crossed — the request id each call carries is what keeps them apart.
#[tokio::test]
async fn a_plugin_tool_and_a_program_keep_their_calls_apart() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("plugin-tools-concurrent");
    scratch.write(
        "echo.py",
        r#"
from komo_plugin import tool, tools

@tool("Echo what komo answers.")
def echo() -> str:
    return str(tools.whoami())
"#,
    );

    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();
    host.manifest().await.unwrap();

    let plugin = host.call("echo", serde_json::json!({}), |_n, _a| {
        std::future::ready(Ok(ToolAnswer::text("plugin")))
    });
    let program = host.run_code("return tools.whoami()", |_n, _a| {
        std::future::ready(Ok(ToolAnswer::text("program")))
    });
    let (plugin, program) = tokio::join!(plugin, program);
    assert_eq!(plugin.unwrap(), "plugin");
    assert_eq!(program.unwrap().result.unwrap(), "program");

    host.shutdown().await;
}

/// `tools` at import time has no call to dispatch into. It must fail the
/// import — the same treatment any other broken plugin gets — rather than
/// block the host waiting on an answer nobody is coming to give.
#[tokio::test]
async fn using_tools_at_import_time_fails_the_import_rather_than_hanging() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("plugin-tools-import");
    scratch.write("good.py", GREETER);
    scratch.write(
        "eager.py",
        r#"
from komo_plugin import tool, tools

tools.time()

@tool("Never reached — the module raises above.")
def unreachable() -> str:
    return "unreachable"
"#,
    );

    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();
    let tools = host.manifest().await.unwrap().tools;
    assert_eq!(
        tools.len(),
        1,
        "the broken file costs itself and nothing else: {tools:?}"
    );
    assert_eq!(tools[0].name, "greet");
    host.shutdown().await;
}

// ── Code mode ────────────────────────────────────────────────────────────

/// Dispatch that answers every call by echoing what it was asked for, and
/// records the calls in order.
fn recording_dispatch() -> (
    impl Fn(String, serde_json::Value) -> std::future::Ready<Result<ToolAnswer, String>>,
    Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let dispatch = move |name: String, args: serde_json::Value| {
        recorder.lock().unwrap().push((name.clone(), args.clone()));
        std::future::ready(Ok(ToolAnswer::text(format!("{name} says hi"))))
    };
    (dispatch, seen)
}

/// A program runs, prints, calls back into komo, and returns — the whole
/// point of code mode in one exchange.
#[tokio::test]
async fn a_program_calls_komo_tools_and_returns_a_value() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("codemode");
    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();

    let (dispatch, seen) = recording_dispatch();
    let result = host
        .run_code(
            r#"
print("starting")
first = tools.read(path="a.txt")
second = tools.shell(command="ls")
return {"first": first, "second": second}
"#,
            dispatch,
        )
        .await
        .unwrap();

    assert_eq!(result.logs.trim(), "starting");
    let value = result.result.expect("the program returned a value");
    assert_eq!(value["first"], "read says hi");
    assert_eq!(value["second"], "shell says hi");

    // The calls reached komo as the program wrote them, in order, with
    // their arguments intact.
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "read");
    assert_eq!(calls[0].1["path"], "a.txt");
    assert_eq!(calls[1].0, "shell");

    host.shutdown().await;
}

/// A result reaches the program on two channels: the text it would show a
/// reader, and the same result as data. The program computes on the second
/// — parsing the first is what got the early programs' answers wrong.
#[tokio::test]
async fn a_tool_result_carries_its_structured_view_into_the_program() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("codemode-structured");
    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();

    let result = host
        .run_code(
            r#"
out = tools.read(path="a.txt")
return {"text": str(out), "lines": out.structured["total_lines"]}
"#,
            |_name, _args| {
                std::future::ready(Ok(ToolAnswer {
                    // Laid out for a reader, exactly as `read` renders it.
                    content: "a.txt (lines 1-2 of 2)\n1│one\n2│two".to_string(),
                    structured: serde_json::json!({ "total_lines": 2 }),
                }))
            },
        )
        .await
        .unwrap();

    let value = result.result.expect("the program returned a value");
    // Still a `str`: a program that only wants the text is untouched by the
    // second channel existing.
    assert!(value["text"].as_str().unwrap().contains("1│one"));
    assert_eq!(value["lines"], 2);

    host.shutdown().await;
}

/// A tool with no structured view hands the program `None` rather than
/// something it has to tell apart from real data.
#[tokio::test]
async fn a_tool_without_a_structured_view_answers_none() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("codemode-nostructure");
    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();

    let result = host
        .run_code("return tools.time().structured is None", |_n, _a| {
            std::future::ready(Ok(ToolAnswer::text("9am")))
        })
        .await
        .unwrap();
    assert_eq!(result.result.unwrap(), true);

    host.shutdown().await;
}

/// A tool that failed raises inside the program, so it can catch and
/// recover — the same choice komo makes for the model, one level down.
#[tokio::test]
async fn a_failing_tool_raises_inside_the_program_and_can_be_caught() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("codemode-error");
    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();

    let result = host
        .run_code(
            r#"
try:
    tools.read(path="missing")
    return "no error"
except ToolError as error:
    return f"caught: {error.message}"
"#,
            |_name, _args| std::future::ready(Err("no such file".to_string())),
        )
        .await
        .unwrap();
    assert_eq!(result.result.unwrap(), "caught: no such file");

    host.shutdown().await;
}

/// The marker has two spellings — this constant and the one `host.py` matches
/// against — and a program that kept running is what a drift between them
/// would look like.
#[test]
fn the_host_speaks_the_same_suspension_marker() {
    assert!(
        HOST_SOURCE.contains(&format!("SUSPENDED_MARKER = \"{SUSPENDED_MARKER}\"")),
        "host.py must declare the same marker komo sends"
    );
}

/// A turn that stopped to wait takes the program with it, however carefully
/// the program catches. `ToolSuspended` derives `BaseException` for exactly
/// this: a swallowed suspension is a program still making effects while komo
/// is ending the turn.
#[tokio::test]
async fn a_suspension_unwinds_a_program_that_catches_everything() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("codemode-suspended");
    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();

    let outcome = host
        .run_code(
            r#"
try:
    tools.cron(action="add")
except BaseException as error:
    pass
return "kept going"
"#,
            |_name, _args| std::future::ready(Err(format!("{SUSPENDED_MARKER}: come back later"))),
        )
        .await;

    // Caught — `except BaseException` catches anything — but the program is
    // not what this guards: a bare `except Exception` is, and the program
    // above proves the exception is not one.
    assert_eq!(outcome.unwrap().result.unwrap(), "kept going");

    let outcome = host
        .run_code(
            r#"
try:
    tools.cron(action="add")
except Exception as error:
    return "swallowed"
return "kept going"
"#,
            |_name, _args| std::future::ready(Err(format!("{SUSPENDED_MARKER}: come back later"))),
        )
        .await;
    let message = match outcome {
        Err(PyHostError::Plugin(message)) => message,
        other => panic!("the program should unwind, not answer: {other:?}"),
    };
    assert!(message.contains(SUSPENDED_MARKER), "{message}");

    host.shutdown().await;
}

/// A positional call says what to do about it. komo dispatches by argument
/// name, so there is no order to bind a positional to — and the first
/// programs written against `tools` called them positionally, spending a
/// whole round on a host traceback to learn it.
#[tokio::test]
async fn a_positional_tool_call_is_refused_by_name() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("codemode-positional");
    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();

    let error = host
        .run_code("return tools.read(\"/etc/hosts\")", |_n, _a| {
            std::future::ready(Ok(ToolAnswer::text("")))
        })
        .await
        .expect_err("a positional call cannot be dispatched");
    let rendered = format!("{error}");
    assert!(rendered.contains("keyword arguments only"), "{rendered}");
    assert!(rendered.contains("tools.read()"), "{rendered}");

    host.shutdown().await;
}

/// A program that raises answers with its traceback rather than killing the
/// host: the traceback is what the model rewrites from.
#[tokio::test]
async fn a_raising_program_returns_its_traceback_and_the_host_survives() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("codemode-raise");
    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();

    let error = host
        .run_code("return 1 / 0", |_n, _a| {
            std::future::ready(Ok(ToolAnswer::text("")))
        })
        .await
        .expect_err("a raising program is an error");
    let rendered = format!("{error}");
    assert!(rendered.contains("ZeroDivisionError"), "{rendered}");
    assert!(!error.retryable(), "the program already failed");
    // The host's own dispatch frames are three of the four lines in a
    // typical failure and name a file the program's author never wrote.
    assert!(
        !rendered.contains("host.py"),
        "the traceback should start at the program's own frame: {rendered}"
    );

    // Still usable afterwards.
    let ok = host
        .run_code("return 2 + 2", |_n, _a| {
            std::future::ready(Ok(ToolAnswer::text("")))
        })
        .await
        .unwrap();
    assert_eq!(ok.result.unwrap(), 4);

    host.shutdown().await;
}

/// Two programs over one host must not have their tool calls crossed — the
/// run id each call carries is what keeps them apart.
#[tokio::test]
async fn concurrent_programs_keep_their_tool_calls_apart() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("codemode-concurrent");
    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();

    // Each program's dispatch answers with its own marker, so a crossed
    // call would come back with the other program's answer.
    let one = host.run_code("return tools.whoami()", |_n, _a| {
        std::future::ready(Ok(ToolAnswer::text("first")))
    });
    let two = host.run_code("return tools.whoami()", |_n, _a| {
        std::future::ready(Ok(ToolAnswer::text("second")))
    });
    let (one, two) = tokio::join!(one, two);
    assert_eq!(one.unwrap().result.unwrap(), "first");
    assert_eq!(two.unwrap().result.unwrap(), "second");

    host.shutdown().await;
}

/// A program that returns nothing says so, rather than the word `None` —
/// which the model would read as a value.
#[tokio::test]
async fn a_program_that_returns_nothing_reports_no_value() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("codemode-void");
    let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();

    let result = host
        .run_code("print('side effect only')", |_n, _a| {
            std::future::ready(Ok(ToolAnswer::text("")))
        })
        .await
        .unwrap();
    assert!(result.result.is_none());
    assert_eq!(result.logs.trim(), "side effect only");

    host.shutdown().await;
}

/// A host that dies fails everyone waiting on it immediately and reports
/// the exit, so the supervisor can restart rather than time out per call.
#[tokio::test]
async fn a_dead_host_fails_pending_calls_and_reports_the_exit() {
    let Some(python) = python() else { return };
    let scratch = Scratch::new("dying");
    scratch.write(
        "suicide.py",
        r#"
import os
from komo_plugin import tool

@tool("Kills the host process.")
def die() -> str:
    os._exit(1)
"#,
    );

    let (host, mut events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
        .await
        .unwrap();
    host.manifest().await.unwrap();

    let error = host
        .call("die", serde_json::json!({}), no_calls)
        .await
        .expect_err("the host died mid-call");
    assert!(error.retryable(), "the call never completed: {error}");

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
        .await
        .expect("the exit should be reported")
        .unwrap();
    assert!(matches!(event, HostEvent::Exited { .. }), "{event:?}");
}
