//! codemode 的 worker：脚本引擎（[`ScriptEngine`]，现在是 [`MontyEngine`]）跑在这个进程里，worker
//! 把引擎对宿主的每一次需要翻译成 stdio 上的行 JSON（`docs/codemode.md` §3、§4）。这一层与引擎无关。
//!
//! 协议（每行一个 JSON）：
//!   宿主 → worker  第一行 [`Request`]；之后每行是一次工具调用的答复 `{"ok": ..}` / `{"error": ".."}`
//!   worker → 宿主  `{"call": name, "args": {..}}`，最后一行 `{"done": {..}}`
//!
//! `atools.<name>(..)` 是延后调用：`{"call", "args", "id", "defer": true}` 不等答复，脚本拿到一个
//! awaitable 接着跑；脚本卡在 `await` 上时 worker 发 `{"wait": [id..]}`，宿主等**所有**发出未答的
//! 延后调用都有了结果，一次答 `{"results": [{"id", "ok"} | {"id", "error"}..]}`。结果总是成批
//! 交给引擎，不按完成先后，所以同一段脚本、同一批答复，跑出来的调用顺序每次一样（恢复靠它）。
//!
//! 第一行输出是握手 `{"hello": {..}}`（版本，见 [`Hello`]）；时钟与熵走 `{"os": {..}}`，由宿主
//! 回答并记账（[`Nondet`]）。

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::engine::{EngineHost, ScriptEngine, ToolCall};
use super::monty::MontyEngine;
use super::{Hello, Nondet};

/// 脚本里 `print` 与 `text()` 各自最多收这么多字节；超了脚本里抛错，不拖垮 worker。
pub(super) const OUTPUT_CAP: usize = 1 << 20;
/// 跑引擎的线程栈：递归深度由引擎自己的上限管，栈要给够（`docs/codemode.md` §8）。
pub(super) const STACK_SIZE: usize = 64 << 20;
/// worker 用的引擎。
const ENGINE: MontyEngine = MontyEngine;

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
    /// 引擎自己的内存上限（Monty：软限，预检单次大分配；缓慢增长挡不住，那由宿主的 RSS 看门狗管）。
    pub max_memory: Option<usize>,
    /// 引擎自己数的"脚本时间"（不含等宿主答复的时间）。墙钟上限在宿主；这条只给不能被杀的
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
    let done = execute(&ENGINE, request, &mut input, &mut output);
    send(&mut output, &json!({ "done": done }));
    0
}

/// 只编译不运行：脚本拼上引擎的前置代码之后能不能过解析与编译。升级引擎或改前置代码时，
/// 拿真实脚本集跑一遍（`crates/komo/examples/codemode_replay.rs`）。
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

/// [`EngineHost`] 的 worker 实现：调用次数上限、`text()` 总量上限、延后调用的成批等待，以及
/// 它们在行协议上的样子。
struct WorkerHost<'a, I, O> {
    input: &'a mut I,
    output: &'a mut O,
    max_calls: u32,
    calls: u32,
    text_bytes: usize,
    outputs: Vec<String>,
    /// 发出未答的延后调用。
    outstanding: Vec<u32>,
    /// 已经有结果、还没交给引擎的延后调用（含被拒的）。
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
        if !self.outstanding.is_empty() {
            let asked = std::mem::take(&mut self.outstanding);
            match ask_host(self.input, self.output, &json!({ "wait": asked })) {
                Ok(reply) => {
                    for item in reply
                        .get("results")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if let Some(id) = item.get("id").and_then(Value::as_u64) {
                            self.settled.insert(id as u32, answer(item));
                        }
                    }
                    for id in asked {
                        self.settled
                            .entry(id)
                            .or_insert_with(|| Err("宿主没有答这次调用".to_owned()));
                    }
                }
                Err(message) => {
                    for id in asked {
                        self.settled.insert(id, Err(message.clone()));
                    }
                }
            }
        }
        ids.iter()
            .filter_map(|id| self.settled.remove(id).map(|result| (*id, result)))
            .collect()
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

    /// 不是 Monty 的引擎：一次延后调用、一次被拒的延后调用、一次同步调用、等延后的、把结果写进 text。
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
            let quick = host.call(call("quick"));
            let waited = host.wait(&[7, 8]);
            let _ = host.text(format!("{quick:?} {waited:?}"));
            RunEnd {
                console: String::new(),
                error: None,
            }
        }
    }

    #[test]
    fn the_worker_speaks_the_same_protocol_for_any_engine() {
        let mut input = std::io::Cursor::new(
            "{\"ok\": \"Q\"}\n{\"results\": [{\"id\": 7, \"ok\": \"S\"}]}\n".as_bytes(),
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
                json!({ "call": "quick", "args": {} }),
                json!({ "wait": [7] }),
            ],
            "被拒的延后调用不上线，wait 只问发出去的"
        );
        assert_eq!(
            done.outputs,
            [r#"Ok(String("Q")) [(7, Ok(String("S"))), (8, Err("参数不对"))]"#]
        );
    }
}
