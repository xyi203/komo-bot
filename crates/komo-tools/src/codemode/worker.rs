//! codemode 的 worker：脚本引擎（[`ScriptEngine`]，现为 [`DjsEngine`]）跑在这个进程里，worker
//! 把引擎对宿主的每一次需要翻译成 stdio 上的行 JSON（`docs/codemode.md` §3、§4）。这一层与引擎无关。
//!
//! 协议（每行一个 JSON）：
//!   宿主 → worker  第一行 [`Request`]；之后每行是一次调用答复 `{"ok": ..}` / `{"error": ".."}`
//!                  或 `{"results": [{"id", "ok"} | {"id", "error"}..]}`（答 `wait`）
//!   worker → 宿主  `{"call": name, "args": {..}[, "id", "defer": true]}` / `{"wait": [id..]}`
//!                  / `{"os": {..}}` / 最后一行 `{"done": {..}}`
//!
//! `wait` 支持流式返回：宿主只需交付已完成的一批（至少 1 个）id，未完成的继续在宿主侧并发执行，
//! 这样多跳链式并发不被慢调用拖住；宿主侧同时记录交付批次，重启回放时按同序交付。

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::djs::DjsEngine;
use super::engine::{EngineHost, ScriptEngine, ToolCall};
use super::{Hello, Nondet};

/// 脚本里 `console` 与 `text()` 各自最多收这么多字节；超了脚本里抛错，不拖垮 worker。
pub(super) const OUTPUT_CAP: usize = 1 << 20;
/// 跑引擎的线程栈：djs 的 JS 栈帧在堆里，8 MiB 足够编译器解析深层嵌套。
pub(super) const STACK_SIZE: usize = 8 << 20;
/// worker 用的引擎。
const ENGINE: DjsEngine = DjsEngine;

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
    /// 脚本的工作目录（保留用于上下文透传与指纹）。
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Limits {
    /// 引擎自己的堆内存硬上限（djs 不可捕获 OOM，外加宿主的 RSS 看门狗双保险）。
    pub max_memory: Option<usize>,
    /// 引擎自己数的“脚本计算时间”（不含等宿主答复的时间）。墙钟上限在宿主；这条同时给
    /// 进程内测试 worker 兜底。
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
pub(super) fn serve(input: impl BufRead, output: impl Write, hello: Hello) -> i32 {
    serve_with(&ENGINE, input, output, hello)
}

pub(super) fn serve_with(
    engine: &dyn ScriptEngine,
    mut input: impl BufRead,
    mut output: impl Write,
    hello: Hello,
) -> i32 {
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
    let done = execute(engine, request, &mut input, &mut output);
    send(&mut output, &json!({ "done": done }));
    0
}

/// 只编译不运行：脚本能不能过 djs（TypeScript AsyncBody）解析与编译。
pub fn compile_check(code: &str) -> Result<(), String> {
    ENGINE.compile_check(code)
}

fn execute(
    engine: &dyn ScriptEngine,
    request: Request,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Done {
    let mut host = WorkerHost {
        input,
        output,
        max_calls: request.limits.max_calls.unwrap_or(256),
        calls: 0,
        text_bytes: 0,
        outputs: Vec::new(),
        outstanding: Vec::new(),
        settled: HashMap::new(),
    };
    let end = engine.run(&request, &mut host);
    Done {
        outputs: host.outputs,
        console: end.console,
        error: end.error,
    }
}

/// [`EngineHost`] 的 worker 实现：调用次数上限、`text()` 总量上限、流式延后调用等待，以及
/// 它们在行协议上的样子。
struct WorkerHost<'a, I, O> {
    input: &'a mut I,
    output: &'a mut O,
    max_calls: u32,
    calls: u32,
    text_bytes: usize,
    outputs: Vec<String>,
    /// 发出未答的异步调用（流式返回后，未答的继续留在这里）。
    outstanding: Vec<u32>,
    /// 已经有结果、还没交给引擎的异步调用（含本地被拒的）。
    settled: HashMap<u32, Result<Value, String>>,
}

impl<I: BufRead, O: Write> WorkerHost<'_, I, O> {
    /// 记一次调用；超上限时给拒绝说明，不计数。
    fn count(&mut self) -> Result<(), String> {
        if self.calls >= self.max_calls {
            return Err(format!("一段脚本最多调 {} 次工具", self.max_calls));
        }
        self.calls += 1;
        Ok(())
    }
}

impl<I: BufRead, O: Write> EngineHost for WorkerHost<'_, I, O> {
    fn call(&mut self, call: Result<ToolCall, String>) -> Result<Value, String> {
        self.count()?;
        let call = call?;
        answer(&ask_host(
            self.input,
            self.output,
            &json!({ "call": call.name, "args": call.args }),
        )?)
    }

    fn defer(&mut self, id: u32, call: Result<ToolCall, String>) {
        match self.count().and(call) {
            Ok(call) => {
                send(
                    self.output,
                    &json!({ "call": call.name, "args": call.args, "id": id, "defer": true }),
                );
                self.outstanding.push(id);
            }
            Err(refused) => {
                self.settled.insert(id, Err(refused));
            }
        }
    }

    fn wait(&mut self, ids: &[u32]) -> Vec<(u32, Result<Value, String>)> {
        // 先把已经在本地结算（比如超 256 次被拒）的直接交回，避免多余往返。
        let already: Vec<(u32, Result<Value, String>)> = ids
            .iter()
            .filter_map(|id| self.settled.remove(id).map(|result| (*id, result)))
            .collect();
        if !already.is_empty() {
            return already;
        }

        let waiting: Vec<u32> = ids
            .iter()
            .copied()
            .filter(|id| self.outstanding.contains(id))
            .collect();
        if waiting.is_empty() {
            return Vec::new();
        }
        match ask_host(self.input, self.output, &json!({ "wait": waiting })) {
            Ok(reply) => {
                let mut returned = Vec::new();
                for item in reply
                    .get("results")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(id) = item.get("id").and_then(Value::as_u64) {
                        let id = id as u32;
                        self.outstanding.retain(|&p| p != id);
                        returned.push((id, answer(item)));
                    }
                }
                if returned.is_empty() {
                    // 宿主答复为空，标记本次等待的全部失败，避免空转。
                    for id in waiting {
                        self.outstanding.retain(|&p| p != id);
                        returned.push((id, Err("宿主没有答这次调用".to_owned())));
                    }
                }
                returned
            }
            Err(message) => {
                let mut failed = Vec::with_capacity(waiting.len());
                for id in waiting {
                    self.outstanding.retain(|&p| p != id);
                    failed.push((id, Err(message.clone())));
                }
                failed
            }
        }
    }

    fn nondet(&mut self, ask: &Nondet) -> Result<Value, String> {
        answer(&ask_host(self.input, self.output, &json!({ "os": ask }))?)
    }

    fn text(&mut self, text: String) -> Result<(), String> {
        self.text_bytes += text.len();
        if self.text_bytes > OUTPUT_CAP {
            return Err("text() 的输出合计超过 1 MiB".to_owned());
        }
        self.outputs.push(text);
        Ok(())
    }
}

fn send(output: &mut impl Write, message: &Value) {
    let _ = writeln!(output, "{message}");
    let _ = output.flush();
}

/// 发一行、读一行答复。宿主断开也是一个 `Err`，脚本里表现成 `ToolError`。
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
    Ok(reply)
}

/// 一条 `{"ok": v}` / `{"error": ".."}` 答复。
fn answer(reply: &Value) -> Result<Value, String> {
    match reply.get("error") {
        Some(error) => Err(error.as_str().unwrap_or("error").to_owned()),
        None => Ok(reply.get("ok").cloned().unwrap_or(Value::Null)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;
    use crate::codemode::engine::RunEnd;

    /// 非 djs 的脚本引擎桩：验证 worker 协议对引擎透明、支持流式分批 wait。
    struct Scripted;

    impl ScriptEngine for Scripted {
        fn compile_check(&self, _code: &str) -> Result<(), String> {
            Ok(())
        }

        fn run(&self, _request: &Request, host: &mut dyn EngineHost) -> RunEnd {
            let call = |name: &str| {
                Ok(ToolCall {
                    name: name.to_owned(),
                    args: Map::new(),
                })
            };
            host.defer(7, call("slow"));
            host.defer(8, Err("参数不对".to_owned()));
            host.defer(9, call("fast"));
            let local_err = host.wait(&[7, 8, 9]);
            let first = host.wait(&[7, 9]);
            let second = host.wait(&[7]);
            let _ = host.text(format!("{local_err:?} | {first:?} | {second:?}"));
            RunEnd {
                console: String::new(),
                error: None,
            }
        }
    }

    #[test]
    fn the_worker_speaks_the_same_streaming_protocol_for_any_engine() {
        let mut input = std::io::Cursor::new(
            "{\"results\": [{\"id\": 9, \"ok\": \"F\"}]}\n{\"results\": [{\"id\": 7, \"ok\": \"S\"}]}\n".as_bytes(),
        );
        let mut output = Vec::new();
        let done = execute(&Scripted, Request::default(), &mut input, &mut output);

        let sent: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            sent,
            [
                json!({ "call": "slow", "args": {}, "id": 7, "defer": true }),
                json!({ "call": "fast", "args": {}, "id": 9, "defer": true }),
                json!({ "wait": [7, 9] }),
                json!({ "wait": [7] }),
            ],
            "本地被拒的 8 直接先回不上线；第一次 wait 只回了 9，7 留在 outstanding 供下次 wait"
        );
        assert_eq!(
            done.outputs,
            [r#"[(8, Err("参数不对"))] | [(9, Ok(String("F")))] | [(7, Ok(String("S")))]"#]
        );
    }
}
