//! Monty 引擎（[`ScriptEngine`] 的第一个实现）：在进程里跑 Python 子集，把脚本每一次暂停翻译成
//! [`EngineHost`] 的调用（`docs/codemode.md` §3、§4）。
//!
//! 前置代码（`prelude.py`）把 `tools.<name>(..)` 汇成 `_call(name, args)`、`atools.<name>(..)` 汇成
//! `_acall(name, args)`；`text()` 是外部函数。延后调用的 id 用 Monty 自己的 `call_id`。
//!
//! 这里没有网络与子进程：Monty 的语言本身就没有 socket / subprocess。文件是**只读**的：
//! 每个 OsCall 先在这里拦（规范化路径、解符号链接，落在 komo 自己的状态里就拒，读也一样；
//! 写类一律拒），再交给只读挂载的 `/`。环境变量答空。时钟与熵问宿主（[`Nondet`]）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use monty::{MontyRun, RunProgress};
use monty_fs::{MountCallOutcome, MountMode, MountTable};
use monty_types::{
    CollectedStreams, CompileOptions, DateTimeSource, ExcType, ExtFunctionResult, MontyException,
    MontyObject, NameLookupResult, OsFunctionCall, OsPolicy, PrintWriter, RandomStart,
    ResourceLimits, ResourceTracker,
};
use serde_json::Value;

use super::convert::{json_to_monty, monty_to_json, to_text};
use super::engine::{EngineHost, RunEnd, ScriptEngine, ToolCall};
use super::worker::{Limits, OUTPUT_CAP, Request};
use super::{MAX_URANDOM, Nondet};
use crate::tools::paths::real_path;
use crate::tools::protect::Protected;

pub(super) const PRELUDE: &str = include_str!("prelude.py");
/// 工具调用以外的暂停（`text()`、睡眠、OsCall）也算 suspension；Monty 默认的 1000 太小。
const MAX_SUSPENSIONS: usize = 1_000_000;
const INPUT_NAMES: [&str; 5] = ["_call", "_acall", "text", "TOOLS", "_CATALOG"];

/// Monty（Python 子集）引擎。
pub struct MontyEngine;

impl ScriptEngine for MontyEngine {
    fn compile_check(&self, code: &str) -> Result<(), String> {
        compile(code).map(|_| ())
    }

    fn run(&self, request: &Request, host: &mut dyn EngineHost) -> RunEnd {
        let mut end = RunEnd::default();
        let runner = match compile(&request.code) {
            Ok(runner) => runner.with_os_policy(OsPolicy {
                datetime: DateTimeSource::CallHost,
                random_start: RandomStart::CallHost,
                ..OsPolicy::default()
            }),
            Err(error) => {
                end.error = Some(error);
                return end;
            }
        };
        let mut runner = runner;
        let cwd = request.cwd.clone().unwrap_or_else(|| PathBuf::from("/"));
        runner.set_cwd(&cwd.to_string_lossy());
        let mut fs = match Fs::new(&request.protected, cwd) {
            Ok(fs) => fs,
            Err(error) => {
                end.error = Some(format!("文件挂载失败：{error}"));
                return end;
            }
        };
        let inputs = vec![
            MontyObject::function("_call", None),
            MontyObject::function("_acall", None),
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

        loop {
            let paused = match progress {
                Ok(paused) => paused,
                Err(error) => {
                    end.error = Some(user_lines(&error.to_string()));
                    break;
                }
            };
            progress = match paused {
                RunProgress::Complete(_) => break,
                RunProgress::FunctionCall(call) if call.function_name == "_call" => {
                    let parsed = tool_call(&call.args)
                        .ok_or_else(|| "前置代码调 _call 的参数不对".to_owned());
                    let result = ext_result(host.call(parsed));
                    call.resume(result, print(&mut streams))
                }
                // 延后调用：发出去不等答复，脚本拿到 awaitable 接着跑；被拒的在 `await` 的地方抛出，
                // 与真正失败的调用一样。
                RunProgress::FunctionCall(call) if call.function_name == "_acall" => {
                    let parsed = tool_call(&call.args)
                        .ok_or_else(|| "前置代码调 _acall 的参数不对".to_owned());
                    host.defer(call.call_id, parsed);
                    call.resume_pending(print(&mut streams))
                }
                RunProgress::FunctionCall(call) if call.function_name == "text" => {
                    let rendered = match call.args.arg(0) {
                        Some(value) => match value.as_str() {
                            Some(text) => text.to_owned(),
                            None => to_text(value),
                        },
                        None => "null".to_owned(),
                    };
                    let result = match host.text(rendered) {
                        Ok(()) => ExtFunctionResult::Return(MontyObject::none()),
                        Err(message) => tool_error(message),
                    };
                    call.resume(result, print(&mut streams))
                }
                RunProgress::FunctionCall(call) => {
                    let name = call.function_name.clone();
                    call.resume(ExtFunctionResult::NotFound(name), print(&mut streams))
                }
                RunProgress::OsCall(call) => call.resume_with(print(&mut streams), |call| {
                    os_call(call, &mut fs, &mut *host)
                }),
                RunProgress::NameLookup(lookup) => {
                    lookup.resume(NameLookupResult::Undefined, print(&mut streams))
                }
                RunProgress::ResolveFutures(futures) => {
                    let results: Vec<(u32, ExtFunctionResult)> = host
                        .wait(futures.pending_call_ids())
                        .into_iter()
                        .map(|(id, reply)| (id, ext_result(reply)))
                        .collect();
                    if results.is_empty() {
                        // 在等的不是工具调用（比如 asyncio.sleep 之外的东西），再等也等不来。
                        futures.abort(
                            MontyException::new(
                                ExcType::RuntimeError,
                                Some("脚本在等一个不会完成的 awaitable".into()),
                            ),
                            print(&mut streams),
                        )
                    } else {
                        futures.resume(results, print(&mut streams))
                    }
                }
            };
        }
        end.console = console(&streams);
        end
    }
}

/// 前置代码拼上脚本，过 Monty 的解析与编译；失败时行号已减回成脚本自己的。
fn compile(code: &str) -> Result<MontyRun, String> {
    MontyRun::new(
        format!("{PRELUDE}{code}"),
        "codemode.py",
        INPUT_NAMES.iter().map(|name| (*name).to_owned()).collect(),
        CompileOptions::default(),
    )
    .map_err(|error| user_lines(&error.to_string()))
}

fn ext_result(reply: Result<Value, String>) -> ExtFunctionResult {
    match reply {
        Ok(value) => ExtFunctionResult::Return(json_to_monty(&value)),
        Err(message) => tool_error(message),
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
fn os_call(call: OsFunctionCall, fs: &mut Fs, host: &mut dyn EngineHost) -> ExtFunctionResult {
    match &call {
        OsFunctionCall::Getenv(args) => ExtFunctionResult::Return(args.default.clone()),
        OsFunctionCall::GetEnviron => ExtFunctionResult::Return(MontyObject::dict([])),
        OsFunctionCall::SystemSleep(duration) => {
            std::thread::sleep(*duration);
            ExtFunctionResult::Return(MontyObject::none())
        }
        OsFunctionCall::Time(_) => match clock(host) {
            Ok(seconds) => ExtFunctionResult::Return(MontyObject::float(seconds)),
            Err(message) => tool_error(message),
        },
        OsFunctionCall::DateToday | OsFunctionCall::DateTimeNow(_) => {
            match clock(host).and_then(|seconds| civil_clock(&call, seconds)) {
                Ok(value) => ExtFunctionResult::Return(value),
                Err(message) => tool_error(message),
            }
        }
        OsFunctionCall::Urandom(args) => {
            let size = usize::try_from(args.size).unwrap_or(usize::MAX);
            if size > MAX_URANDOM {
                return tool_error(format!("一次最多读 {MAX_URANDOM} 字节随机数"));
            }
            match host.nondet(&Nondet::Urandom { size }) {
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

fn clock(host: &mut dyn EngineHost) -> Result<f64, String> {
    host.nondet(&Nondet::Clock)?
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
    let wall = jiff::tz::Offset::UTC.to_datetime(timestamp);
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

/// 前置代码里 `tools.<name>(...)` 汇成的 `_call(name, args)`（`atools` 是 `_acall`）。
fn tool_call(args: &monty_types::CallArgs) -> Option<ToolCall> {
    let name = args.arg(0)?.as_str()?.to_owned();
    match monty_to_json(args.arg(1)?) {
        Value::Object(args) => Some(ToolCall { name, args }),
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
