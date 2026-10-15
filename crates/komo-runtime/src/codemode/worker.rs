//! codemode 的 worker：在进程里跑 Monty，把脚本每一次对宿主的暂停翻译成 stdio 上的行 JSON
//! （`docs/codemode.md` §3、§4）。
//!
//! 协议（每行一个 JSON）：
//!   宿主 → worker  第一行 [`Request`]；之后每行是一次工具调用的答复 `{"ok": ..}` / `{"error": ".."}`
//!   worker → 宿主  `{"call": name, "args": {..}}`，最后一行 `{"done": {..}}`
//!
//! worker 里没有文件、环境变量、网络：Monty 的语言本身就没有 socket / subprocess，文件与环境变量
//! 这类要宿主放行的调用（OsCall）一律答 `PermissionError`（环境变量答空）。工具调用与输出
//! （`text()`）以外，脚本碰不到宿主。

use std::io::{BufRead, Write};
use std::time::Duration;

use monty::{MontyRun, RunProgress};
use monty_types::{
    CollectedStreams, CompileOptions, ExcType, ExtFunctionResult, MontyException, MontyObject,
    NameLookupResult, OsFunctionCall, PrintWriter, ResourceLimits, ResourceTracker,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::convert::{json_to_monty, monty_to_json, to_text};

const PRELUDE: &str = include_str!("prelude.py");
/// 脚本里 `print` 与 `text()` 各自最多收这么多字节；超了脚本里抛错，不拖垮 worker。
const OUTPUT_CAP: usize = 1 << 20;
/// 工具调用以外的暂停（`text()`、睡眠、OsCall）也算 suspension；Monty 默认的 1000 太小。
const MAX_SUSPENSIONS: usize = 1_000_000;
/// 跑 Monty 的线程栈：递归深度由 Monty 自己的上限管，栈要给够（`docs/codemode.md` §8）。
pub(super) const STACK_SIZE: usize = 64 << 20;
const INPUT_NAMES: [&str; 4] = ["_call", "text", "TOOLS", "_CATALOG"];

/// 宿主交进来的一次运行：脚本、能调的工具名、发现工具用的说明与限制。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Request {
    pub code: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub catalog: Value,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Limits {
    /// Monty 自己的内存软限：预检单次大分配；缓慢增长挡不住，那由宿主的 RSS 看门狗管。
    pub max_memory: Option<usize>,
    /// Monty 自己数的"脚本时间"（不含等宿主答复的时间）。墙钟上限在宿主；这条只给不能被杀的
    /// 进程内 worker（测试）兜底。
    pub max_turn_ms: Option<u64>,
    pub max_calls: Option<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Done {
    pub outputs: Vec<String>,
    pub console: String,
    pub error: Option<String>,
}

/// 子命令入口（`komo __codemode-worker`）：stdin 读请求，stdout 写协议。返回退出码。
pub fn run() -> i32 {
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(|| serve(std::io::stdin().lock(), std::io::stdout().lock()))
        .map_or(101, |thread| thread.join().unwrap_or(101))
}

/// 在一对读写端上跑一次请求。进程内的测试 worker 与子命令共用它。
pub(super) fn serve(mut input: impl BufRead, mut output: impl Write) -> i32 {
    let mut line = String::new();
    if !matches!(input.read_line(&mut line), Ok(n) if n > 0) {
        return 2;
    }
    let request: Request = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(error) => {
            send(
                &mut output,
                &json!({ "done": { "error": format!("请求读不懂：{error}") } }),
            );
            return 2;
        }
    };
    let done = execute(request, &mut input, &mut output);
    send(&mut output, &json!({ "done": done }));
    0
}

fn send(output: &mut impl Write, message: &Value) {
    let _ = writeln!(output, "{message}");
    let _ = output.flush();
}

/// 问宿主一次工具调用。宿主断开也是一个 `Err`，脚本里表现成 `ToolError`。
fn call_host(
    input: &mut impl BufRead,
    output: &mut impl Write,
    name: &str,
    args: &Map<String, Value>,
) -> Result<Value, String> {
    send(output, &json!({ "call": name, "args": args }));
    let mut line = String::new();
    if !matches!(input.read_line(&mut line), Ok(n) if n > 0) {
        return Err("komo 断开了".into());
    }
    let reply: Value =
        serde_json::from_str(&line).map_err(|error| format!("答复读不懂：{error}"))?;
    match reply.get("error") {
        Some(error) => Err(error.as_str().unwrap_or("error").to_owned()),
        None => Ok(reply.get("ok").cloned().unwrap_or(Value::Null)),
    }
}

fn tool_error(message: impl Into<String>) -> ExtFunctionResult {
    ExtFunctionResult::Error(MontyException::new(
        ExcType::RuntimeError,
        Some(message.into()),
    ))
}

fn limits(request: &Limits) -> ResourceLimits {
    let mut limits = ResourceLimits::default().max_suspensions(MAX_SUSPENSIONS);
    if let Some(bytes) = request.max_memory {
        limits = limits.max_memory(bytes);
    }
    if let Some(ms) = request.max_turn_ms {
        limits = limits.max_turn_duration(Duration::from_millis(ms));
    }
    limits
}

fn execute(request: Request, input: &mut impl BufRead, output: &mut impl Write) -> Done {
    let mut done = Done::default();
    let max_calls = request.limits.max_calls.unwrap_or(256);

    let source = format!("{PRELUDE}{}", request.code);
    let runner = match MontyRun::new(
        source,
        "codemode.py",
        INPUT_NAMES.iter().map(|name| (*name).to_owned()).collect(),
        CompileOptions::default(),
    ) {
        Ok(runner) => runner,
        Err(error) => {
            done.error = Some(user_lines(&error.to_string()));
            return done;
        }
    };
    let inputs = vec![
        MontyObject::function("_call", None),
        MontyObject::function("text", None),
        MontyObject::list(
            request
                .tools
                .iter()
                .map(|name| MontyObject::string(name.clone())),
        ),
        json_to_monty(&request.catalog),
    ];

    let mut streams = CollectedStreams::default();
    let mut progress = runner.start(
        inputs,
        ResourceTracker::new(limits(&request.limits)),
        print(&mut streams),
    );

    let (mut calls, mut text_bytes) = (0u32, 0usize);
    loop {
        let paused = match progress {
            Ok(paused) => paused,
            Err(error) => {
                done.error = Some(user_lines(&error.to_string()));
                break;
            }
        };
        progress = match paused {
            RunProgress::Complete(_) => break,
            RunProgress::FunctionCall(call) if call.function_name == "_call" => {
                let result = if calls >= max_calls {
                    tool_error(format!("一段脚本最多调 {max_calls} 次工具"))
                } else {
                    calls += 1;
                    match tool_call(&call.args) {
                        Some((name, args)) => match call_host(input, output, &name, &args) {
                            Ok(value) => ExtFunctionResult::Return(json_to_monty(&value)),
                            Err(message) => tool_error(message),
                        },
                        None => tool_error("前置代码调 _call 的参数不对"),
                    }
                };
                call.resume(result, print(&mut streams))
            }
            RunProgress::FunctionCall(call) if call.function_name == "text" => {
                let rendered = match call.args.arg(0) {
                    Some(value) => match value.as_str() {
                        Some(text) => text.to_owned(),
                        None => to_text(value),
                    },
                    None => "null".to_owned(),
                };
                text_bytes += rendered.len();
                let result = if text_bytes > OUTPUT_CAP {
                    tool_error("text() 的输出合计超过 1 MiB")
                } else {
                    done.outputs.push(rendered);
                    ExtFunctionResult::Return(MontyObject::none())
                };
                call.resume(result, print(&mut streams))
            }
            RunProgress::FunctionCall(call) => {
                let name = call.function_name.clone();
                call.resume(ExtFunctionResult::NotFound(name), print(&mut streams))
            }
            RunProgress::OsCall(call) => {
                call.resume_with(print(&mut streams), |call| os_call(call))
            }
            RunProgress::NameLookup(lookup) => {
                lookup.resume(NameLookupResult::Undefined, print(&mut streams))
            }
            RunProgress::ResolveFutures(futures) => futures.abort(
                MontyException::new(ExcType::RuntimeError, Some("codemode 里没有 async".into())),
                print(&mut streams),
            ),
        };
    }
    done.console = console(&streams);
    done
}

/// 文件、环境这类要宿主放行的调用：这个版本一个都不放行。
fn os_call(call: OsFunctionCall) -> ExtFunctionResult {
    match &call {
        OsFunctionCall::Getenv(args) => ExtFunctionResult::Return(args.default.clone()),
        OsFunctionCall::GetEnviron => ExtFunctionResult::Return(MontyObject::dict([])),
        OsFunctionCall::SystemSleep(duration) => {
            std::thread::sleep(*duration);
            ExtFunctionResult::Return(MontyObject::none())
        }
        _ => match call.fs_primary_path() {
            Some(path) => ExtFunctionResult::Error(MontyException::new(
                ExcType::PermissionError,
                Some(format!("Permission denied: '{path}'")),
            )),
            None => ExtFunctionResult::Error(call.on_no_handler()),
        },
    }
}

/// 前置代码里 `tools.<name>(...)` 汇成的 `_call(name, args)`。
fn tool_call(args: &monty_types::CallArgs) -> Option<(String, Map<String, Value>)> {
    let name = args.arg(0)?.as_str()?.to_owned();
    match monty_to_json(args.arg(1)?) {
        Value::Object(args) => Some((name, args)),
        _ => None,
    }
}

fn print(streams: &mut CollectedStreams) -> PrintWriter<'_> {
    PrintWriter::CollectStreams(streams, Some(OUTPUT_CAP))
}

fn console(streams: &CollectedStreams) -> String {
    streams
        .entries()
        .iter()
        .map(|(_, text)| text.as_str())
        .collect()
}

/// 回溯里的 `line N` 是拼了 prelude 之后的行号；减回成脚本自己的。
fn user_lines(traceback: &str) -> String {
    let prelude_lines = PRELUDE.lines().count();
    let mut out = String::new();
    for line in traceback.lines() {
        match line.find(", line ") {
            Some(at) => {
                let rest = &line[at + ", line ".len()..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                match digits.parse::<usize>() {
                    Ok(number) => {
                        out.push_str(&line[..at + ", line ".len()]);
                        out.push_str(&number.saturating_sub(prelude_lines).to_string());
                        out.push_str(&rest[digits.len()..]);
                    }
                    Err(_) => out.push_str(line),
                }
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out.trim_end().to_owned()
}
