//! codemode 的 worker：在进程里跑 Monty，把脚本每一次对宿主的暂停翻译成 stdio 上的行 JSON
//! （`docs/codemode.md` §3、§4）。
//!
//! 协议（每行一个 JSON）：
//!   宿主 → worker  第一行 [`Request`]；之后每行是一次工具调用的答复 `{"ok": ..}` / `{"error": ".."}`
//!   worker → 宿主  `{"call": name, "args": {..}}`，最后一行 `{"done": {..}}`
//!
//! 第一行输出是握手 `{"hello": {..}}`（版本，见 [`Hello`]）；时钟与熵走 `{"os": {..}}`，由宿主
//! 回答并记账（[`Nondet`]）。
//!
//! worker 里没有网络与子进程：Monty 的语言本身就没有 socket / subprocess。文件是**只读**的：
//! 每个 OsCall 先在这里拦（规范化路径、解符号链接，落在 komo 自己的状态里就拒，读也一样；
//! 写类一律拒），再交给只读挂载的 `/`。环境变量答空。

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use monty::{MontyRun, RunProgress};
use monty_fs::{MountCallOutcome, MountMode, MountTable};
use monty_types::{
    CollectedStreams, CompileOptions, DateTimeSource, ExcType, ExtFunctionResult, MontyException,
    MontyObject, NameLookupResult, OsFunctionCall, OsPolicy, PrintWriter, RandomStart,
    ResourceLimits, ResourceTracker,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::convert::{json_to_monty, monty_to_json, to_text};
use super::{Hello, MAX_URANDOM, Nondet};
use crate::tools::paths::real_path;
use crate::tools::protect::Protected;

pub(super) const PRELUDE: &str = include_str!("prelude.py");
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
    /// 脚本的工作目录；没有就是 `/`。
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// 读写都拒的路径（komo 自己的状态）。
    #[serde(default)]
    pub protected: Vec<PathBuf>,
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
        .spawn(|| {
            serve(
                std::io::stdin().lock(),
                std::io::stdout().lock(),
                Hello::current(),
            )
        })
        .map_or(101, |thread| thread.join().unwrap_or(101))
}

/// 在一对读写端上跑一次请求。进程内的测试 worker 与子命令共用它。
pub(super) fn serve(mut input: impl BufRead, mut output: impl Write, hello: Hello) -> i32 {
    send(&mut output, &json!({ "hello": hello }));
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

/// 只编译不运行：脚本拼上前置代码之后能不能过 Monty 的解析与编译。升级 Monty 或改前置代码时，
/// 拿真实脚本集跑一遍（`crates/komo/examples/codemode_replay.rs`）。
pub fn compile_check(code: &str) -> Result<(), String> {
    MontyRun::new(
        format!("{PRELUDE}{code}"),
        "codemode.py",
        INPUT_NAMES.iter().map(|name| (*name).to_owned()).collect(),
        CompileOptions::default(),
    )
    .map(|_| ())
    .map_err(|error| user_lines(&error.to_string()))
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
    ask_host(input, output, &json!({ "call": name, "args": args }))
}

/// 问宿主一次时钟或熵（[`Nondet`]）。
fn os_host(
    input: &mut impl BufRead,
    output: &mut impl Write,
    request: &Nondet,
) -> Result<Value, String> {
    ask_host(input, output, &json!({ "os": request }))
}

fn ask_host(
    input: &mut impl BufRead,
    output: &mut impl Write,
    message: &Value,
) -> Result<Value, String> {
    send(output, message);
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
        Ok(runner) => runner.with_os_policy(OsPolicy {
            datetime: DateTimeSource::CallHost,
            random_start: RandomStart::CallHost,
            ..OsPolicy::default()
        }),
        Err(error) => {
            done.error = Some(user_lines(&error.to_string()));
            return done;
        }
    };
    let mut runner = runner;
    let cwd = request.cwd.clone().unwrap_or_else(|| PathBuf::from("/"));
    runner.set_cwd(&cwd.to_string_lossy());
    let mut fs = match Fs::new(&request.protected, cwd) {
        Ok(fs) => fs,
        Err(error) => {
            done.error = Some(format!("文件挂载失败：{error}"));
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
            RunProgress::OsCall(call) => call.resume_with(print(&mut streams), |call| {
                os_call(call, &mut fs, input, output)
            }),
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

/// 只读的文件系统：宿主的 `/` 只读挂载，外加 komo 自己状态的名单。
struct Fs {
    mounts: MountTable,
    protected: Protected,
    cwd: PathBuf,
}

impl Fs {
    fn new(protected: &[PathBuf], cwd: PathBuf) -> Result<Self, String> {
        let mut mounts = MountTable::new();
        mounts
            .mount("/", "/", MountMode::ReadOnly, None)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            mounts,
            protected: Protected::new(protected.to_vec()),
            cwd,
        })
    }

    /// 读写类 OsCall：先拦，再交给挂载。
    fn serve(&mut self, mut call: OsFunctionCall) -> ExtFunctionResult {
        let Some(path) = call.fs_primary_path().map(str::to_owned) else {
            return ExtFunctionResult::Error(call.on_no_handler());
        };
        if call.is_write() {
            return permission(
                &path,
                "codemode 里的文件是只读的；写文件请用 write / edit 工具，要跑完整 Python 用 bash 的 python3",
            );
        }
        // Monty 已按 cwd 把相对路径解成绝对的；这里再补一道，再解掉 `..` 与符号链接，
        // 名单比对的是真实路径。
        let absolute = if Path::new(&path).is_absolute() {
            PathBuf::from(&path)
        } else {
            self.cwd.join(&path)
        };
        let real = real_path(&absolute).unwrap_or(absolute);
        if self.protected.denies(&real) {
            return permission(&path, "这是 komo 自己的状态，脚本不能读");
        }
        // 挂载（cap-std）不跟绝对路径的符号链接，而它们很常见；名单比对用的真实路径本来就
        // 没有链接了，直接拿它去读——比对的与读到的是同一个文件。`is_symlink` 要看链接本身，
        // 不改写。
        if !matches!(call, OsFunctionCall::IsSymlink(_))
            && let Some(primary) = call.fs_paths_mut().next()
        {
            *primary = monty_types::MontyPath::new(real.to_string_lossy().into_owned());
        }
        match self.mounts.handle_os_call(call) {
            MountCallOutcome::Handled(Ok(value)) => ExtFunctionResult::Return(value),
            MountCallOutcome::Handled(Err(error)) => {
                ExtFunctionResult::Error(error.into_exception())
            }
            MountCallOutcome::NotHandled(call) => ExtFunctionResult::Error(call.on_no_handler()),
        }
    }
}

fn permission(path: &str, why: &str) -> ExtFunctionResult {
    ExtFunctionResult::Error(MontyException::new(
        ExcType::PermissionError,
        Some(format!("Permission denied: '{path}'（{why}）")),
    ))
}

/// OsCall：环境变量答空；时钟与熵问宿主（宿主记账、恢复时原样还）；睡眠在这里睡
/// （墙钟由宿主管，到点整个 worker 被杀）；其余是文件系统（[`Fs`]）。
fn os_call(
    call: OsFunctionCall,
    fs: &mut Fs,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> ExtFunctionResult {
    match &call {
        OsFunctionCall::Getenv(args) => ExtFunctionResult::Return(args.default.clone()),
        OsFunctionCall::GetEnviron => ExtFunctionResult::Return(MontyObject::dict([])),
        OsFunctionCall::SystemSleep(duration) => {
            std::thread::sleep(*duration);
            ExtFunctionResult::Return(MontyObject::none())
        }
        OsFunctionCall::Time(_) => match clock(input, output) {
            Ok(seconds) => ExtFunctionResult::Return(MontyObject::float(seconds)),
            Err(message) => tool_error(message),
        },
        OsFunctionCall::DateToday | OsFunctionCall::DateTimeNow(_) => {
            match clock(input, output).and_then(|seconds| civil_clock(&call, seconds)) {
                Ok(value) => ExtFunctionResult::Return(value),
                Err(message) => tool_error(message),
            }
        }
        OsFunctionCall::Urandom(args) => {
            let size = usize::try_from(args.size).unwrap_or(usize::MAX);
            if size > MAX_URANDOM {
                return tool_error(format!("一次最多读 {MAX_URANDOM} 字节随机数"));
            }
            match os_host(input, output, &Nondet::Urandom { size }) {
                Ok(Value::String(hex)) => match decode_hex(&hex) {
                    Some(bytes) => ExtFunctionResult::Return(MontyObject::bytes(bytes)),
                    None => tool_error("宿主给的随机数不是十六进制"),
                },
                Ok(_) => tool_error("宿主给的随机数不是字符串"),
                Err(message) => tool_error(message),
            }
        }
        _ => fs.serve(call),
    }
}

fn clock(input: &mut impl BufRead, output: &mut impl Write) -> Result<f64, String> {
    os_host(input, output, &Nondet::Clock)?
        .as_f64()
        .ok_or_else(|| "宿主给的时钟不是数字".to_string())
}

/// `date.today()` / `datetime.now(tz)`：由宿主给的 Unix 秒算出。没有 tz 时按 UTC 的墙上时间
/// （沙箱的本地时区默认 UTC）。
fn civil_clock(call: &OsFunctionCall, seconds: f64) -> Result<MontyObject, String> {
    let timestamp = jiff::Timestamp::from_microsecond((seconds * 1e6).round() as i64)
        .map_err(|error| format!("时钟超出范围：{error}"))?;
    let (offset, name) = match call {
        OsFunctionCall::DateTimeNow(Some(tz)) => (tz.offset_seconds, tz.name.clone()),
        _ => (0, None),
    };
    let wall = timestamp.to_zoned(jiff::tz::TimeZone::UTC).datetime();
    let wall = wall
        .checked_add(jiff::SignedDuration::from_secs(i64::from(offset)))
        .unwrap_or(wall);
    Ok(match call {
        OsFunctionCall::DateTimeNow(zone) => MontyObject::datetime(monty_types::MontyDateTime {
            year: i32::from(wall.year()),
            month: wall.month() as u8,
            day: wall.day() as u8,
            hour: wall.hour() as u8,
            minute: wall.minute() as u8,
            second: wall.second() as u8,
            microsecond: (wall.subsec_nanosecond() / 1000) as u32,
            offset_seconds: zone.as_ref().map(|_| offset),
            timezone_name: if zone.is_some() { name } else { None },
        }),
        _ => MontyObject::date(monty_types::MontyDate {
            year: i32::from(wall.year()),
            month: wall.month() as u8,
            day: wall.day() as u8,
        }),
    })
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len() / 2)
        .map(|at| u8::from_str_radix(hex.get(2 * at..2 * at + 2)?, 16).ok())
        .collect()
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
