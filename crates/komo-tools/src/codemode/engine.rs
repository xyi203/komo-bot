//! codemode 的脚本引擎接缝：worker 里真正跑脚本的那一层。换引擎（Monty → 别的语言或解释器）只换
//! [`ScriptEngine`] 的实现，宿主、恢复与重放都不动（`docs/codemode.md` §3）。
//!
//! 分工：引擎管编译、运行、把脚本对外的每一次需要翻译成 [`EngineHost`] 的调用，以及它自己语言里的
//! 值怎么与 JSON 互转、前置代码、文件边界。行协议、工具调用次数上限、延后调用的成批等待、`text()`
//! 的总量上限都在 [`EngineHost`] 的实现里（worker），所有引擎一样。
//!
//! 引擎必须是**确定性**的：同一段脚本、同一串答复，发出的调用（名字、参数、延后调用的 id）与顺序每次
//! 一样——崩溃恢复靠从头重跑、按序还回录下的答复。时钟与熵只能经 [`EngineHost::nondet`] 问宿主。

use serde_json::{Map, Value};

use super::Nondet;
use super::worker::Request;

/// 脚本发出的一次工具调用：工具名与参数。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub args: Map<String, Value>,
}

/// 一个脚本引擎。跑在 worker 专用的大栈线程里，同步执行，不经 tokio。
pub trait ScriptEngine {
    /// 只编译不运行：脚本（连同引擎自己的前置代码）能不能过解析与编译。
    fn compile_check(&self, code: &str) -> Result<(), String>;

    /// 跑一段脚本到结束。脚本自己的失败（编译错误、未捕获的异常、超限）进 [`RunEnd::error`]。
    fn run(&self, request: &Request, host: &mut dyn EngineHost) -> RunEnd;
}

/// 引擎看到的宿主。工具调用失败、被拒、宿主断开都是 `Err(说明)`，引擎在脚本里把它表现成
/// `ToolError`（可捕获）。
pub trait EngineHost {
    /// 同步工具调用（`tools.<name>(..)`）：等到答复。`call` 是引擎解析出的调用；解析不出时给
    /// `Err(说明)`，它照样算一次调用。
    fn call(&mut self, call: Result<ToolCall, String>) -> Result<Value, String>;

    /// 延后工具调用（`atools.<name>(..)`）：发出去不等。`id` 由引擎给，一段脚本里不重复。被拒的
    /// （超次数、解析不出）也记下，在 [`EngineHost::wait`] 里作为失败交回。
    fn defer(&mut self, id: u32, call: Result<ToolCall, String>);

    /// 脚本卡在 `await` 上：宿主先等**所有**发出未答的延后调用都有结果，再交回 `ids` 里有结果的
    /// 那些。成批交回、不按完成先后，所以结果的顺序不依赖工具跑得快慢。
    fn wait(&mut self, ids: &[u32]) -> Vec<(u32, Result<Value, String>)>;

    /// 时钟与熵：问宿主，宿主记账，恢复时原样还回。
    fn nondet(&mut self, ask: &Nondet) -> Result<Value, String>;

    /// `text()` 的一段输出；合计超过上限时 `Err`。
    fn text(&mut self, text: String) -> Result<(), String>;
}

/// 一段脚本跑完之后引擎交回的：控制台输出与失败说明（`None` = 正常跑完）。`text()` 的输出在宿主那边。
#[derive(Debug, Default)]
pub struct RunEnd {
    pub console: String,
    pub error: Option<String>,
}
