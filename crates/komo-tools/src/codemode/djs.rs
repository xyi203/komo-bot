//! djs 引擎（[`ScriptEngine`] 实现）：纯 Rust JavaScript / TypeScript 引擎（`djs-core` + `djs-prelude`）。
//!
//! 特点（`docs/codemode.md` §2、§4）：
//! - 零第三方依赖、零 C FFI、引擎内无文件 / 网络 / 子进程 / 环境变量；
//! - `prelude.js` 在进程内首次调用时预编译并固化成快照（`OnceLock<Vec<u8>>`），每次执行直接拷贝镜像起步；
//! - 用户脚本以 `Goal::AsyncBody` + `typescript: true` 单遍编译，顶层支持 `await` / `return`，行号 1:1 零偏移；
//! - `ExecOptions::fatal_out_of_memory = true` 提供脚本无法 `catch` 的硬堆上限；
//! - 按 Fuel 切片推进，切片间检查计算耗时上限（不含等待宿主 I/O 的时间）；
//! - 确定性随机种子与虚拟时钟经 [`EngineHost::nondet`] 注入，兼容 `komo.nested` 崩溃重放。

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use djs_core::compiler::CompileOptions;
use djs_core::heap::{Handle, Heap};
use djs_core::value::Value as JsValue;
use djs_core::vm::{ErrorKind, ExecOptions, Status, Vm, VmOptions, WeakRefs};
use djs_core::wire::{self, Sink, Writer};
use serde_json::{Map, Number, Value};

use super::Nondet;
use super::engine::{EngineHost, RunEnd, ScriptEngine, ToolCall};
use super::worker::{OUTPUT_CAP, Request};

pub(super) const PRELUDE: &str = include_str!("prelude.js");

/// 宿主函数 ID（与 `prelude.js` 约定）。
const FN_ACALL: u32 = 1;
const FN_TEXT: u32 = 2;
const FN_CONSOLE: u32 = 3;

/// 每次 `vm.run_metered` 的 Fuel 切片（约 1ms 紧密循环），切片间检查时间上限。
const FUEL_SLICE: u64 = 200_000;
/// `prelude.js` 与 `__initRuntime` 初始化的 Fuel 预算。
const SETUP_FUEL: u64 = 50_000_000;
/// 未指定内存上限时的默认堆上限（64 MiB；宿主默认传 1 GiB）。
const DEFAULT_HEAP_LIMIT: usize = 64 << 20;
/// 单个工具参数从堆里读出时的最大估计字节数。
const MAX_ARG_BYTES: usize = 16 << 20;

/// 进程级预编译快照：含冻结内建对象、宿主函数绑定与已求值的 `prelude.js`。
static PRELUDE_SNAPSHOT: OnceLock<Result<Vec<u8>, String>> = OnceLock::new();

/// djs 引擎构建标识（随 `djs-core` 源码与特性变化），写入握手与 `codemode.start` 指纹。
pub fn build_id() -> String {
    format!("djs-{:08x}", djs_core::vm::BUILD_HASH)
}

/// djs（JavaScript / TypeScript）引擎。
pub struct DjsEngine;

impl ScriptEngine for DjsEngine {
    fn compile_check(&self, code: &str) -> Result<(), String> {
        let mut vm = djs_prelude::new_vm(VmOptions::default())
            .map_err(|e| format!("初始化 djs 失败：{e:?}"))?;
        let opts = CompileOptions {
            typescript: true,
            ..CompileOptions::async_body("codemode.ts", &[])
        };
        vm.compile(code.as_bytes(), &opts)
            .map(|_| ())
            .map_err(|e| format_compile_error(code, &e))
    }

    fn run(&self, request: &Request, host: &mut dyn EngineHost) -> RunEnd {
        match catch_unwind(AssertUnwindSafe(|| run_inner(request, host))) {
            Ok(end) => end,
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_owned());
                RunEnd {
                    console: String::new(),
                    error: Some(format!("djs 引擎内部错误：{msg}")),
                }
            }
        }
    }
}

fn prelude_snapshot() -> Result<&'static [u8], String> {
    PRELUDE_SNAPSHOT
        .get_or_init(build_prelude_snapshot)
        .as_deref()
        .map_err(Clone::clone)
}

fn build_prelude_snapshot() -> Result<Vec<u8>, String> {
    let oom = |_| "初始化 djs 前置快照时内存不足".to_owned();
    let opts = VmOptions {
        memory_limit: 64 << 20,
        weak_refs: WeakRefs::Strong,
        frozen_builtins: true,
        exec: ExecOptions {
            fatal_out_of_memory: true,
            ..ExecOptions::default()
        },
        ..VmOptions::default()
    };
    let mut vm = djs_prelude::new_vm(opts).map_err(oom)?;

    let acall = vm
        .new_async_host_function(FN_ACALL, "__acall")
        .map_err(oom)?;
    vm.define_global("__acall", acall).map_err(oom)?;
    let text_fn = vm.new_host_function(FN_TEXT, "__text").map_err(oom)?;
    vm.define_global("__text", text_fn).map_err(oom)?;
    let console_fn = vm.new_host_function(FN_CONSOLE, "__console").map_err(oom)?;
    vm.define_global("__console", console_fn).map_err(oom)?;

    let prelude_fn = vm
        .compile(PRELUDE.as_bytes(), &CompileOptions::script("<prelude>"))
        .map_err(|e| format!("djs 前置脚本编译失败：{e}"))?;
    let this = JsValue::object(vm.global());
    vm.call(prelude_fn, this, &[])
        .map_err(|e| format!("启动前置脚本失败：{e:?}"))?;
    match vm.run(SETUP_FUEL) {
        Status::Done(_) => {}
        Status::Threw(e) => return Err(format!("前置脚本抛错：{}", format_exception(&mut vm, e))),
        other => return Err(format!("前置脚本未正常结束：{other:?}")),
    }
    vm.seal().map_err(oom)?;
    Ok(vm.image().to_vec())
}

fn run_inner(request: &Request, host: &mut dyn EngineHost) -> RunEnd {
    let mut end = RunEnd::default();

    // 1. 从宿主读取一次初始随机种子与时钟，保证崩溃重放确定性。
    let seed = read_seed(host);
    let initial_now_ms = read_clock_ms(host);

    let memory_limit = request
        .limits
        .max_memory
        .unwrap_or(DEFAULT_HEAP_LIMIT)
        .max(4 << 20);
    let snapshot = match prelude_snapshot() {
        Ok(bytes) => bytes,
        Err(error) => {
            end.error = Some(error);
            return end;
        }
    };
    let opts = VmOptions {
        memory_limit,
        weak_refs: WeakRefs::Strong,
        frozen_builtins: true,
        exec: ExecOptions {
            fatal_out_of_memory: true,
            ..ExecOptions::default()
        },
        ..VmOptions::default()
    };
    let mut buf = Vec::with_capacity(Heap::image_capacity(snapshot.len(), memory_limit));
    buf.extend_from_slice(snapshot);
    let mut vm = match Vm::from_bytes_trusted(buf, opts) {
        Ok(vm) => vm,
        Err(e) => {
            end.error = Some(format!("加载 djs 快照失败：{e:?}"));
            return end;
        }
    };
    vm.set_math_random_seed(seed);
    vm.set_now(initial_now_ms);

    // 2. 绑定本次能力面的工具表与发现目录。
    if let Err(error) = init_session(&mut vm, request) {
        end.error = Some(error);
        return end;
    }

    // 3. 编译用户脚本为 TypeScript AsyncBody（行号 1:1 对应用户源码）。
    let compile_opts = CompileOptions {
        typescript: true,
        ..CompileOptions::async_body("codemode.ts", &[])
    };
    let main_fn = match vm.compile(request.code.as_bytes(), &compile_opts) {
        Ok(f) => f,
        Err(error) => {
            end.error = Some(format_compile_error(&request.code, &error));
            return end;
        }
    };
    let this = JsValue::object(vm.global());
    if let Err(error) = vm.call(main_fn, this, &[]) {
        end.error = Some(format!("启动脚本失败：{error:?}"));
        return end;
    }

    // 4. 切片运行主循环：处理同步宿主调用（text/console）、流式异步工具调用（__acall）、超限与结束。
    let turn_limit = request.limits.max_turn_ms.map(Duration::from_millis);
    let mut compute_elapsed = Duration::ZERO;
    let mut entry_handle: Option<Handle> = None;
    let mut pending_ids: Vec<u32> = Vec::new();
    let mut wrote_text = false;

    loop {
        if let Some(limit) = turn_limit
            && compute_elapsed >= limit
        {
            vm.abort();
            end.error = Some(format!("脚本超时（{}ms），已终止", limit.as_millis()));
            break;
        }

        let slice_start = Instant::now();
        let (status, _used) = vm.run_metered(FUEL_SLICE);
        compute_elapsed += slice_start.elapsed();

        match status {
            Status::OutOfFuel => continue,
            Status::HostOutOfMemory => {
                end.error = Some(format!(
                    "脚本内存超限（{} MiB），已终止",
                    memory_limit >> 20
                ));
                break;
            }
            Status::Threw(err) => {
                end.error = Some(format_exception(&mut vm, err));
                break;
            }
            Status::HostCall { call_id, fn_id } => {
                let reply = serve_sync_call(
                    &mut vm,
                    call_id,
                    fn_id,
                    host,
                    &mut end.console,
                    &mut wrote_text,
                );
                let resolved = match reply {
                    Ok(v) => vm.resolve_host_call(call_id, Ok(v)),
                    Err(msg) => match vm.new_error(ErrorKind::Error, &msg) {
                        Ok(err_val) => vm.resolve_host_call(call_id, Err(err_val)),
                        Err(_) => {
                            end.error = Some(format!(
                                "脚本内存超限（{} MiB），已终止",
                                memory_limit >> 20
                            ));
                            break;
                        }
                    },
                };
                if let Err(error) = resolved {
                    end.error = Some(format!("回传宿主调用结果失败：{error:?}"));
                    break;
                }
            }
            Status::Done(_) | Status::Idle => {
                // 先把本轮新发出的异步工具调用立即登记给宿主（即使脚本随即 return，未 await 的也发出）。
                dispatch_new_calls(&mut vm, host, &mut pending_ids);

                if entry_handle.is_none() {
                    match vm.outcome() {
                        Some(Ok(v)) => {
                            if vm.promise_state(v).is_some() {
                                vm.mark_promise_handled(v);
                            }
                            match vm.heap_mut().new_handle(v) {
                                Ok(h) => entry_handle = Some(h),
                                Err(_) => {
                                    end.error = Some(format!(
                                        "脚本内存超限（{} MiB），已终止",
                                        memory_limit >> 20
                                    ));
                                    break;
                                }
                            }
                        }
                        Some(Err(err)) => {
                            end.error = Some(format_exception(&mut vm, err));
                            break;
                        }
                        None => {
                            end.error = Some("脚本异常中止：没有返回结果".to_owned());
                            break;
                        }
                    }
                }

                let entry = vm.heap().get_handle(entry_handle.expect("entry handle"));
                match vm.promise_state(entry) {
                    None => break,
                    Some(Some(Ok(value))) => {
                        // 如果脚本没有调 `text()` 但顶层 `return` 了非 undefined 值，自动作为输出交回。
                        if !wrote_text && !value.is_undefined() {
                            match format_return_value(&mut vm, value) {
                                Ok(rendered) => {
                                    if let Err(msg) = host.text(rendered) {
                                        end.error = Some(msg);
                                    }
                                }
                                Err(msg) => {
                                    end.error = Some(msg);
                                }
                            }
                        }
                        break;
                    }
                    Some(Some(Err(err))) => {
                        end.error = Some(format_exception(&mut vm, err));
                        break;
                    }
                    Some(None) => {
                        if pending_ids.is_empty() {
                            end.error = Some("脚本在等一个永远不会完成的 Promise".to_owned());
                            break;
                        }
                        let replies = host.wait(&pending_ids);
                        if replies.is_empty() {
                            end.error = Some("宿主没有答复挂起的工具调用".to_owned());
                            break;
                        }
                        // 工具结果回来时推进一次虚拟时钟（确定性重放友好）。
                        let now_ms = read_clock_ms(host);
                        let mut oom = false;
                        for (id, reply) in replies {
                            pending_ids.retain(|&p| p != id);
                            let res = match reply {
                                Ok(value) => match json_to_js(&mut vm, &value) {
                                    Ok(v) => vm.resolve_host_call_at(id, Ok(v), now_ms),
                                    Err(_) => {
                                        oom = true;
                                        break;
                                    }
                                },
                                Err(msg) => match vm.new_error(ErrorKind::Error, &msg) {
                                    Ok(e) => vm.resolve_host_call_at(id, Err(e), now_ms),
                                    Err(_) => {
                                        oom = true;
                                        break;
                                    }
                                },
                            };
                            if let Err(error) = res {
                                end.error = Some(format!("回传异步工具结果失败：{error:?}"));
                                oom = true;
                                break;
                            }
                        }
                        if oom {
                            if end.error.is_none() {
                                end.error = Some(format!(
                                    "脚本内存超限（{} MiB），已终止",
                                    memory_limit >> 20
                                ));
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    if let Some(h) = entry_handle {
        vm.heap_mut().drop_handle(h);
    }
    end
}

fn init_session(vm: &mut Vm, request: &Request) -> Result<(), String> {
    let init_fn = vm
        .global_path("__initRuntime")
        .ok_or_else(|| "djs 快照缺少 __initRuntime".to_owned())?;
    let tools_json = Value::Array(
        request
            .tools
            .iter()
            .map(|name| Value::String(name.clone()))
            .collect(),
    );
    let tools_val = json_to_js(vm, &tools_json)?;
    let catalog_val = json_to_js(vm, &request.catalog)?;
    let this = JsValue::object(vm.global());
    vm.call(init_fn, this, &[tools_val, catalog_val])
        .map_err(|e| format!("初始化脚本运行时失败：{e:?}"))?;
    match vm.run(SETUP_FUEL) {
        Status::Done(_) => Ok(()),
        Status::Threw(e) => Err(format!("初始化运行时抛错：{}", format_exception(vm, e))),
        Status::HostOutOfMemory => Err("脚本内存超限，已终止".to_owned()),
        other => Err(format!("初始化运行时未正常结束：{other:?}")),
    }
}

fn serve_sync_call(
    vm: &mut Vm,
    call_id: u32,
    fn_id: u32,
    host: &mut dyn EngineHost,
    console: &mut String,
    wrote_text: &mut bool,
) -> Result<JsValue, String> {
    let info = vm
        .host_call(call_id)
        .ok_or_else(|| format!("找不到同步宿主调用 {call_id}"))?;
    let first = info.args.first().copied().unwrap_or(JsValue::UNDEFINED);
    match fn_id {
        FN_TEXT => {
            let rendered = js_string_or_display(vm, first);
            host.text(rendered)?;
            *wrote_text = true;
            Ok(JsValue::UNDEFINED)
        }
        FN_CONSOLE => {
            let line = js_string_or_display(vm, first);
            if console.len() + line.len() <= OUTPUT_CAP {
                console.push_str(&line);
            } else {
                return Err("console 输出合计超过 1 MiB".to_owned());
            }
            Ok(JsValue::UNDEFINED)
        }
        other => Err(format!("未知的同步宿主函数 id={other}")),
    }
}

fn dispatch_new_calls(vm: &mut Vm, host: &mut dyn EngineHost, pending_ids: &mut Vec<u32>) {
    for call_id in vm.new_host_calls() {
        let Some(info) = vm.host_call(call_id) else {
            continue;
        };
        if info.fn_id != FN_ACALL {
            continue;
        }
        let parsed = parse_tool_call(vm.heap(), &info.args);
        host.defer(call_id, parsed);
        pending_ids.push(call_id);
    }
}

fn parse_tool_call(heap: &Heap, args: &[JsValue]) -> Result<ToolCall, String> {
    let name = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| heap.str_to_string(s))
        .ok_or_else(|| "前置代码调 __acall 缺少工具名".to_owned())?;
    let raw_args = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    if raw_args.is_undefined() || raw_args.is_null() {
        return Ok(ToolCall {
            name,
            args: Map::new(),
        });
    }
    let json = js_to_json(heap, raw_args)?;
    match json {
        Value::Object(map) => Ok(ToolCall { name, args: map }),
        _ => Err("工具参数必须是对象".to_owned()),
    }
}

fn format_return_value(vm: &mut Vm, value: JsValue) -> Result<String, String> {
    if let Some(s) = value.as_string() {
        return Ok(vm.heap().str_to_string(s));
    }
    match js_to_json(vm.heap(), value) {
        Ok(json) => Ok(serde_json::to_string_pretty(&json).unwrap_or_else(|_| json.to_string())),
        Err(_) => Ok(vm.display(value)),
    }
}

fn js_string_or_display(vm: &mut Vm, v: JsValue) -> String {
    if let Some(s) = v.as_string() {
        vm.heap().str_to_string(s)
    } else {
        vm.display(v)
    }
}

fn format_compile_error(code: &str, err: &djs_core::compiler::CompileError) -> String {
    let base = format!(
        "SyntaxError: {} at codemode.ts:{}:{}",
        err.message, err.line, err.col
    );
    let looks_python = code.contains("def ")
        || code.contains("except ")
        || code.contains("import json")
        || code.contains("import re")
        || code.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("for ") && t.ends_with(':')
        });
    if looks_python {
        format!(
            "{base}\n提示：codemode 使用 TypeScript / JavaScript（非 Python），请改用 `await tools.<name>({{ ... }})`。"
        )
    } else {
        base
    }
}

fn format_exception(vm: &mut Vm, err: JsValue) -> String {
    let msg = vm.display(err);
    match vm.error_stack(err) {
        Some(stack) if !stack.trim().is_empty() => format!("{msg}\n{stack}"),
        _ => msg,
    }
}

fn read_seed(host: &mut dyn EngineHost) -> u64 {
    let Ok(Value::String(hex)) = host.nondet(&Nondet::Urandom { size: 8 }) else {
        return 1;
    };
    u64::from_str_radix(hex.trim(), 16).unwrap_or(1).max(1)
}

fn read_clock_ms(host: &mut dyn EngineHost) -> f64 {
    match host.nondet(&Nondet::Clock) {
        Ok(v) => v.as_f64().map_or(0.0, |secs| (secs * 1000.0).floor()),
        Err(_) => 0.0,
    }
}

// ---------------------------------------------------------------- JSON ↔ djs Heap 桥接

fn json_to_js(vm: &mut Vm, value: &Value) -> Result<JsValue, String> {
    let mut writer = Writer::new();
    write_json(&mut writer, value);
    vm.decode_wire(&writer.finish())
        .map_err(|e| format!("写入 JS 值失败：{}", e.text()))
}

fn write_json(w: &mut Writer, value: &Value) {
    match value {
        Value::Null => w.null(),
        Value::Bool(b) => w.bool(*b),
        Value::Number(n) => w.number(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => w.str(s),
        Value::Array(items) => {
            w.array(items.len());
            for item in items {
                write_json(w, item);
            }
        }
        Value::Object(map) => {
            w.object(map.len());
            for (k, v) in map {
                w.key(k);
                write_json(w, v);
            }
        }
    }
}

fn js_to_json(heap: &Heap, value: JsValue) -> Result<Value, String> {
    let mut sink = JsonSink::default();
    wire::encode_to(heap, value, MAX_ARG_BYTES, &mut sink).map_err(|e| e.text())?;
    sink.finish()
}

/// 直接将 djs 堆内值流式转成 `serde_json::Value`，零中间字节缓冲，不执行任何 JS。
#[derive(Default)]
struct JsonSink {
    written: usize,
    stack: Vec<OpenContainer>,
    root: Option<Value>,
}

enum OpenContainer {
    Array {
        items: Vec<Value>,
        remaining: usize,
    },
    Object {
        map: Map<String, Value>,
        remaining: usize,
        next_key: Option<String>,
    },
}

impl JsonSink {
    fn push_value(&mut self, value: Value) {
        let mut current = Some(value);
        while let Some(v) = current.take() {
            match self.stack.last_mut() {
                None => {
                    self.root = Some(v);
                }
                Some(OpenContainer::Array { items, remaining }) => {
                    items.push(v);
                    *remaining -= 1;
                    if *remaining == 0 {
                        if let Some(OpenContainer::Array { items, .. }) = self.stack.pop() {
                            current = Some(Value::Array(items));
                        }
                    }
                }
                Some(OpenContainer::Object {
                    map,
                    remaining,
                    next_key,
                }) => {
                    if let Some(key) = next_key.take() {
                        map.insert(key, v);
                    }
                    *remaining -= 1;
                    if *remaining == 0 {
                        if let Some(OpenContainer::Object { map, .. }) = self.stack.pop() {
                            current = Some(Value::Object(map));
                        }
                    }
                }
            }
        }
    }

    fn finish(self) -> Result<Value, String> {
        self.root
            .ok_or_else(|| "无法将 JS 值转换为 JSON".to_owned())
    }
}

impl Sink for JsonSink {
    fn written(&self) -> usize {
        self.written
    }

    fn undefined(&mut self) {
        self.written += 1;
        self.push_value(Value::Null);
    }

    fn null(&mut self) {
        self.written += 1;
        self.push_value(Value::Null);
    }

    fn bool(&mut self, b: bool) {
        self.written += 1;
        self.push_value(Value::Bool(b));
    }

    fn number(&mut self, n: f64) {
        self.written += 8;
        let val = if n.is_finite() {
            if n.fract() == 0.0 && n >= (i64::MIN as f64) && n <= (i64::MAX as f64) && n != -0.0 {
                let i = n as i64;
                if (i as f64) == n {
                    Value::Number(Number::from(i))
                } else {
                    Number::from_f64(n).map_or(Value::Null, Value::Number)
                }
            } else {
                Number::from_f64(n).map_or(Value::Null, Value::Number)
            }
        } else {
            Value::Null
        };
        self.push_value(val);
    }

    fn hole(&mut self) {
        self.written += 1;
        self.push_value(Value::Null);
    }

    fn str_latin1(&mut self, units: &[u8]) {
        self.written += units.len() + 1;
        let s: String = units.iter().map(|&b| b as char).collect();
        self.push_value(Value::String(s));
    }

    fn str(&mut self, s: &str) {
        self.written += s.len() + 1;
        self.push_value(Value::String(s.to_owned()));
    }

    fn str_utf16(&mut self, units: &[u16]) {
        self.written += units.len() * 2 + 1;
        let s = String::from_utf16_lossy(units);
        self.push_value(Value::String(s));
    }

    fn bytes(&mut self, b: &[u8]) {
        self.written += b.len() + 1;
        let arr = b.iter().map(|&x| Value::Number(Number::from(x))).collect();
        self.push_value(Value::Array(arr));
    }

    fn array(&mut self, count: usize) {
        self.written += 2;
        if count == 0 {
            self.push_value(Value::Array(Vec::new()));
        } else {
            self.stack.push(OpenContainer::Array {
                items: Vec::with_capacity(count.min(1024)),
                remaining: count,
            });
        }
    }

    fn object(&mut self, count: usize) {
        self.written += 2;
        if count == 0 {
            self.push_value(Value::Object(Map::new()));
        } else {
            self.stack.push(OpenContainer::Object {
                map: Map::with_capacity(count.min(256)),
                remaining: count,
                next_key: None,
            });
        }
    }

    fn key(&mut self, k: &str) {
        self.written += k.len() + 1;
        if let Some(OpenContainer::Object { next_key, .. }) = self.stack.last_mut() {
            *next_key = Some(k.to_owned());
        }
    }

    fn key_utf16(&mut self, units: &[u16]) {
        self.written += units.len() * 2 + 1;
        if let Some(OpenContainer::Object { next_key, .. }) = self.stack.last_mut() {
            *next_key = Some(String::from_utf16_lossy(units));
        }
    }
}
