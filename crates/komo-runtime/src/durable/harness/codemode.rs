//! codemode 外层（`docs/storage-durable.md` §4.7、§5；`docs/codemode.md` §3、§5、§7；pi-durable
//! spec §7.3、§7.6）：`komo.tool` 任务执行 `Operation::Codemode` 时，在 worker 里跑脚本，脚本里的
//! 每次工具调用是名下的一个嵌套工具任务（`nested.rs`）。
//!
//! 跟旧 journal（`executor/codemode.rs`）的对应：
//!
//! | 旧 | 新 |
//! |---|---|
//! | `start` 指纹 | 外层任务的 memo `codemode.start`（先写者赢，对不上 = 回放分歧） |
//! | 子调用 `intent` / `started` / `delivered` | 嵌套任务自己的 `call → execute → 结果`；结果写进 `komo.nested` 的成员 |
//! | `nondet` 批次（时钟 / 熵） | 外层任务的 memo `codemode.nondet.<n>`（先写者赢），下一次新子调用建任务之前落 |
//! | `finished` | 没有：外层结果已发布就按 §5 的表复用；没发布就整段重放（已交付的从槽位回放，不重做） |
//!
//! 外层任务重启后按 §5 的表重跑（`CodemodeTool::replay` 是 Safe）：同一段脚本从头跑，第 `n` 个
//! 调用在 `komo.nested` 里找 `c<n>`——名字与参数都对得上就等它的任务（已终结的立刻拿结果，没终结
//! 的由引擎照 §5 恢复），**不再另建**；对不上是分歧，整段 uncertain。记录用完之后的新调用才建
//! 新的嵌套任务。已记录的时钟与随机数读数按序原样还给脚本。
//!
//! 脚本结束（任何原因）之前，外层先把名下还活着的嵌套任务 abort 掉并等它们落下结果；外层自己被
//! abort 时由引擎自底向上处理，嵌套任务各自的 abort handler 写结果。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use komo_kernel::durable::task::NextTaskState;
use komo_kernel::durable::{TaskId, TaskOutcome, TaskState};
use komo_kernel::protocol::nested::bounded_args;
use komo_kernel::traits::OutputWriter;
use komo_kernel::types::refs::{ImageRef, ToolResultStatus};
use komo_kernel::types::tool::{ToolContext, ToolError, ToolImage, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::hooks::ToolSet;
use super::live::{self as progress, NestedToolSlot, ToolSlot};
use super::nested::{NESTED_VERSION, NestedDoc, NestedResult, nested_address, nested_key};
use super::tool::{Call, ToolTask};
use crate::codemode::{Nondet, ScriptCatalog, ScriptTool, live};
use crate::durable::error::DurableError;
use crate::durable::runtime::TaskRuntime;
use crate::durable::tx::{Ownership, TaskOptions};
use crate::executor::ToolExecutor;
use crate::executor::codemode::{
    ARGS_PREVIEW, NestedCall, failure_prefix, join_outputs, summary, truncate,
};
use crate::tools::codemode::NOT_IN_SCRIPTS;

/// 指纹 memo 的名字。
const START_MEMO: &str = "codemode.start";
/// 读数批次 memo 的名字前缀（`codemode.nondet.1`、`.2`……）。
const NONDET_MEMO: &str = "codemode.nondet";
/// 一段脚本最多读这么多次时钟 / 随机数（含恢复时已录下的）：memo 不能随脚本里的紧循环涨下去。
const MAX_NONDET: usize = 10_000;
/// 一段脚本最多调这么多次工具。
const MAX_CALLS: usize = 256;
/// 提交被拒时等 invocation 的取消信号多久：被拒几乎总是因为取消或关闭，别把它当成脚本的结果发布。
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// 读过的一次时钟或熵：请求与宿主给的值。恢复时按序原样还回去。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct NondetEntry {
    request: Nondet,
    value: Value,
}

/// 这次执行里脚本的状态（宿主一侧）。
#[derive(Default)]
struct State {
    /// 已发出的调用数（= 当前调用的序号）。
    cursor: usize,
    /// 开始时 `komo.nested` 里已有的调用数：脚本得把它们都走一遍才算没分歧。
    recorded: usize,
    /// 宿主的致命错误锁住不放：Python 接住 `ToolError` 或自己伪造 done 都解不开。
    fatal: Option<ToolError>,
    /// 已落账的读数（展开后）与消费到哪儿。
    replay: Vec<NondetEntry>,
    replay_cursor: usize,
    /// 已落账的批次数。
    batches: usize,
    /// 本次读到、还没落账的读数：下一次新子调用建任务之前（或结束之前）才落。之间崩了也没关系——
    /// 这些值没被任何已提交的外部效果用过，恢复时重新读就是。
    pending: Vec<NondetEntry>,
    /// 按序号记：并发的调用交付先后不定，汇总时再排。
    calls: Vec<(usize, NestedCall)>,
    images: Vec<(usize, ImageRef)>,
}

fn uncertain(message: impl Into<String>) -> ToolError {
    ToolError::Uncertain {
        message: message.into(),
    }
}

/// 一次执行里的全部东西。
struct Script<'a> {
    task: &'a ToolTask,
    rt: &'a TaskRuntime,
    outer: &'a Call,
    state: Arc<Mutex<State>>,
    /// 开始时已有的嵌套调用，按序号。
    docs: Vec<NestedDoc>,
    /// 已经过了准入（对上记录或建好任务）的最大序号。`atools` 的调用并发跑，但准入一个接
    /// 一个按序号来：`komo.nested` 的 `c<n>` 必须连续（恢复时缺一个就到头），读数也要在用到它们的
    /// 那次建任务之前落账。
    admitted: tokio::sync::watch::Sender<usize>,
}

/// 执行 codemode 一次：脚本跑完交回整段结果。任何出口之前，名下还活着的嵌套任务都先收掉
/// （spec §7.3：caller 终结之前总是先 abort 自己的嵌套调用）；被取消时这个 future 直接被丢掉，
/// 由引擎自底向上收。
pub(super) async fn execute(
    task: &ToolTask,
    rt: &TaskRuntime,
    set: &ToolSet,
    outer: &Call,
    code: &str,
    ctx: &ToolContext,
    sink: &mut dyn OutputWriter,
) -> Result<ToolOutput, ToolError> {
    let result = run_script(task, rt, set, outer, code, ctx, sink).await;
    if result.is_err() {
        abort_nested(rt).await;
    }
    result
}

async fn run_script(
    task: &ToolTask,
    rt: &TaskRuntime,
    set: &ToolSet,
    outer: &Call,
    code: &str,
    ctx: &ToolContext,
    sink: &mut dyn OutputWriter,
) -> Result<ToolOutput, ToolError> {
    let Some(worker) = task.deps.codemode() else {
        return Err(ToolError::Failed {
            message: "这台 Gateway 没有接 codemode".into(),
        });
    };
    let mut names: Vec<String> = set
        .surface()
        .names()
        .iter()
        .filter(|name| !NOT_IN_SCRIPTS.contains(&name.as_str()))
        .cloned()
        .collect();
    names.sort();

    // 指纹：脚本、引擎与 worker 协议版本、能力表、cwd 任一变了，恢复就是分歧。先写者赢。
    let fingerprint = ToolExecutor::codemode_fingerprint(code, &names, ctx);
    let winner = match rt.put_memo(START_MEMO, fingerprint.clone()).await {
        Ok(winner) => winner,
        Err(error) => return Err(host_error(rt, error).await),
    };
    if winner != fingerprint {
        return Err(uncertain("codemode 回放分歧：脚本、协议或能力面改变"));
    }

    // 已落账的读数与已有的嵌套调用。
    let mut state = State::default();
    loop {
        let name = format!("{NONDET_MEMO}.{}", state.batches + 1);
        let memo = match rt.memo(&name).await {
            Ok(memo) => memo,
            Err(error) => return Err(host_error(rt, error).await),
        };
        let Some(memo) = memo else { break };
        let batch: Vec<NondetEntry> = serde_json::from_value(memo)
            .map_err(|e| uncertain(format!("codemode 记录的读数损坏：{e}")))?;
        state.replay.extend(batch);
        state.batches += 1;
    }
    let docs = match load_docs(rt).await {
        Ok(docs) => docs,
        Err(error) => return Err(host_error(rt, error).await),
    };
    state.recorded = docs.len();
    let script = Script {
        task,
        rt,
        outer,
        state: Arc::new(Mutex::new(state)),
        docs,
        admitted: tokio::sync::watch::Sender::new(0),
    };

    // 被接续的 Run 里 MCP 工具可能还没挂上：等它们的服务器是钩子（`AgentHooks::tools`）的事，
    // 这里按工具表现在有的算。
    let catalog = script_catalog(set, &names);
    let reads = script.state.clone();
    let script = &script;
    let outcome = worker
        .in_dir(ctx.cwd.clone())
        .with_nondet(Arc::new(move |request| read(&reads, request)))
        .run(code, &names, &catalog, ctx.call_timeout, |name, args| {
            // 序号按 worker 发来的顺序定，不等 future 被轮询。
            let index = script.reserve();
            script.nested_call(index, name, args)
        })
        .await;

    if let Some(message) = outcome.incompatible {
        // 脚本一行没跑；但恢复时前几次尝试可能已经做过子调用，那就不能当普通失败。
        let any = script.docs.len() + script.state().cursor > 0;
        return Err(if any {
            uncertain(message)
        } else {
            ToolError::Failed { message }
        });
    }

    // 不论脚本怎么结束（超时、被杀、崩了），名下还活着的嵌套任务都先收掉，等它们落下结果。
    abort_nested(rt).await;
    script.check_end(rt).await?;

    let (calls, images) = {
        let mut state = script.state();
        let mut calls = std::mem::take(&mut state.calls);
        calls.sort_by_key(|(index, _)| *index);
        let mut images = state.images.clone();
        images.sort_by_key(|(index, _)| *index);
        (
            calls.into_iter().map(|(_, call)| call).collect::<Vec<_>>(),
            images
                .into_iter()
                .map(|(_, image)| image)
                .collect::<Vec<_>>(),
        )
    };
    // 非 bash 结果的模型视图保留头部：失败原因与已交付调用必须先于大段脚本输出。
    let child_failed = calls
        .iter()
        .any(|call| !call.ok || call.status == Some(ToolResultStatus::Failed));
    let mut text = match &outcome.error {
        Some(error) => failure_prefix(error, &calls),
        None if child_failed => format!("（{}；脚本已正常结束）\n", summary(&calls)),
        None => String::new(),
    };
    text.push_str(&join_outputs(&outcome.outputs));
    if !outcome.console.is_empty() {
        text.push_str(&format!("\n<console>\n{}</console>", outcome.console));
    }
    if let Some(error) = &outcome.error {
        text.push_str(&format!("\n<traceback>\n{error}\n</traceback>"));
    }
    if outcome.error.is_none() && !child_failed && !calls.is_empty() {
        text.push_str(&format!("\n（{}）", summary(&calls)));
    }
    let text = text.trim_start();
    let mut tool_images = Vec::new();
    for image in &images {
        match set.env.outputs.open_image(image).await {
            Ok(bytes) => tool_images.push(ToolImage {
                mime: image.mime.clone(),
                bytes,
            }),
            Err(error) => {
                tracing::warn!(task = %rt.task_id(), %error, "读不回嵌套调用的图片");
            }
        }
    }
    let output = ToolOutput {
        status: if outcome.error.is_some() {
            ToolResultStatus::Failed
        } else {
            ToolResultStatus::Completed
        },
        result: json!({ "calls": calls, "error": outcome.error }),
        exit_code: None,
        artifacts: vec![],
        // 子调用交出来的图：脚本只拿得到文字，图随外层结果交给模型。
        images: tool_images,
        preview: Some(text.to_owned()),
    };
    // 读数先落账再交结果：结果里可能带着用了这些值的东西。
    script.flush_nondet().await?;
    sink.write_stdout(text.as_bytes())
        .await
        .map_err(|error| ToolError::Failed {
            message: error.to_string(),
        })?;
    Ok(output)
}

impl Script<'_> {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 锁住宿主的致命错误：之后任何子调用都不再执行。
    fn lock_fatal(&self, error: ToolError) {
        self.state().fatal.get_or_insert(error);
    }

    /// 给下一次调用分序号。在 worker 发来调用的那一刻做：这时的读数正好是脚本在它之前读的。
    fn reserve(&self) -> usize {
        let mut state = self.state();
        state.cursor += 1;
        let index = state.cursor;
        // 回放到已记录的调用时，它之前的读数应当全是录下来的；读多了说明脚本走了别的路。
        if index <= state.recorded && !state.pending.is_empty() {
            let name = self.docs.get(index - 1).map_or("", |doc| doc.name.as_str());
            state.fatal.get_or_insert(uncertain(format!(
                "codemode 回放分歧：子调用 {index} ({name}) 之前多读了时钟 / 随机数"
            )));
        }
        index
    }

    /// 脚本里的第 `index` 次工具调用（`docs/codemode.md` §5）。错误就是交给脚本的 `ToolError` 正文；
    /// 结果不明或宿主自己出错时锁住 fatal，之后的调用一律不再执行，脚本接住异常也解不开。
    async fn nested_call(&self, index: usize, name: String, args: Value) -> Result<Value, String> {
        let started = Instant::now();
        let preview = truncate(&args.to_string(), ARGS_PREVIEW);
        let arguments = bounded_args(&args);
        let shown = (preview, arguments);
        let result = self.admit_and_wait_at(index, &name, args).await;
        let result = match result {
            Ok(result) => {
                let images = result.images.iter().cloned().map(|image| (index, image));
                self.state().images.extend(images);
                result
            }
            Err(error) => {
                let message = error.to_string();
                let unknown = error.is_uncertain();
                self.lock_fatal(error);
                let reply = Err(message.clone());
                self.record_call(index, name, shown, started, &reply, None, unknown);
                return Err(message);
            }
        };
        let reply = if result.uncertain {
            let message = result
                .error
                .clone()
                .unwrap_or_else(|| "子调用结果不明；停止脚本后续调用".into());
            self.lock_fatal(uncertain(message));
            Err("子调用结果不明；停止脚本后续调用".to_owned())
        } else {
            result.script_result()
        };
        let unknown = result.uncertain;
        self.record_call(index, name, shown, started, &reply, Some(&result), unknown);
        reply
    }

    /// `shown` 是参数的两种样子：截断的文字（给模型摘要）与收窄的值（给界面）。
    #[allow(clippy::too_many_arguments)]
    fn record_call(
        &self,
        index: usize,
        tool: String,
        shown: (String, Value),
        started: Instant,
        reply: &Result<Value, String>,
        result: Option<&NestedResult>,
        uncertain: bool,
    ) {
        let (args, arguments) = shown;
        let call = NestedCall {
            id: nested_key(index),
            tool,
            args,
            arguments,
            uncertain,
            ok: reply.is_ok(),
            status: result
                .filter(|_| reply.is_ok())
                .and_then(NestedResult::status),
            output: result.and_then(|result| result.output.clone()),
            elapsed_ms: started.elapsed().as_millis() as u64,
            error: reply.as_ref().err().cloned(),
        };
        self.state().calls.push((index, call));
    }

    /// 第 `index` 个调用：记录里有就等它的任务，没有就建一个再等。准入按序号一个接一个，
    /// 等结果可以并发。
    async fn admit_and_wait_at(
        &self,
        index: usize,
        name: &str,
        args: Value,
    ) -> Result<NestedResult, ToolError> {
        let mut turn = self.admitted.subscribe();
        // Sender 活得比这里久，`wait_for` 不会因为通道关闭而失败。
        let _ = turn.wait_for(|admitted| *admitted + 1 >= index).await;
        let admitted = self.admit_at(index, name, args).await;
        // 不论成败都轮到下一个；失败的已经锁了 fatal（或只是这一次被拒），后面的自己会看到。
        self.admitted
            .send_modify(|admitted| *admitted = (*admitted).max(index));
        let (key, id) = admitted?;
        self.wait_nested(&key, id).await
    }

    /// 准入第 `index` 个调用：对上记录，或者先把读数落账再建任务。返回它的 key 与任务。
    async fn admit_at(
        &self,
        index: usize,
        name: &str,
        args: Value,
    ) -> Result<(String, TaskId), ToolError> {
        {
            let state = self.state();
            if let Some(error) = &state.fatal {
                return Err(error.clone());
            }
            if index > MAX_CALLS {
                return Err(uncertain(format!("codemode 子调用超过 {MAX_CALLS} 次")));
            }
        }
        let key = nested_key(index);
        let id = match self.docs.get(index - 1) {
            Some(doc) => {
                if doc.name != name || doc.arguments != args {
                    return Err(uncertain(format!(
                        "codemode 回放分歧：子调用 {key} 的工具名或参数改变"
                    )));
                }
                doc.task_id
            }
            None => {
                // 这次新调用的参数可能用到了脚本刚读到的时钟 / 随机数：先把它们落账。
                self.flush_nondet().await?;
                match self.admit(&key, name, args).await {
                    Ok(id) => id,
                    Err(error) => return Err(host_error(self.rt, error).await),
                }
            }
        };
        Ok((key, id))
    }

    /// 等嵌套任务终结，读它交付的结果。
    async fn wait_nested(&self, key: &str, id: TaskId) -> Result<NestedResult, ToolError> {
        let record = match self.rt.wait_for_task(id).await {
            Ok(record) => record,
            Err(error) => return Err(host_error(self.rt, error).await),
        };
        let doc = match read_doc(self.rt, key).await {
            Ok(doc) => doc,
            Err(error) => return Err(host_error(self.rt, error).await),
        };
        if let Some(result) = doc.and_then(|doc| doc.result) {
            return Ok(result);
        }
        // 任务终结了，却没有结果：调度器自己写的 `faulted` / `orphaned`（没来得及写结果），做没做过
        // 不知道。
        Ok(NestedResult::from_scheduler(&match record.state {
            TaskState::Terminal {
                outcome: TaskOutcome::Faulted { error },
            } => error.message,
            TaskState::Terminal {
                outcome: TaskOutcome::Orphaned { reason },
            } => reason,
            other => format!("{:?}", other.status()),
        }))
    }

    /// 同一次提交里建嵌套任务与它的 document 成员。
    async fn admit(&self, key: &str, name: &str, args: Value) -> Result<TaskId, DurableError> {
        let kind = self.task.clone();
        let created = Arc::new(Mutex::new(None));
        let out = created.clone();
        let key = key.to_owned();
        let call_id = format!("{}/{key}", self.outer.id);
        let name = name.to_owned();
        let conversation = self.rt.conversation();
        self.rt
            .commit(move |tx, task| {
                Box::pin(async move {
                    let mut live = progress::load(tx, conversation).await?;
                    let input = json!({
                        "kind": "nested",
                        "parent": task.id(),
                        "key": key,
                        "callId": call_id,
                        "name": name,
                        "arguments": args,
                    });
                    let id = tx
                        .create_task(
                            &kind,
                            input,
                            Ownership::Task(task.id()),
                            TaskOptions::default(),
                        )
                        .await?;
                    let mut slot = ToolSlot::pending(call_id, name.clone());
                    slot.task_id = Some(id);
                    live.nested_tools
                        .get_or_insert_with(Vec::new)
                        .push(NestedToolSlot {
                            parent_task_id: task.id(),
                            slot,
                        });
                    progress::store(tx, conversation, &live).await?;
                    let doc = NestedDoc {
                        task_id: id,
                        name,
                        arguments: args,
                        result: None,
                    };
                    tx.put_document(
                        nested_address(task.id(), &key),
                        NESTED_VERSION,
                        serde_json::to_value(&doc).expect("NestedDoc 可序列化"),
                    )
                    .await?;
                    *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
                    Ok(None::<NextTaskState>)
                })
            })
            .await?;
        let id = created
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("提交成功就建了任务");
        Ok(id)
    }

    /// 把本次读到、还没落账的读数落成一条 memo。必须在下一次新子调用建任务之前（以及交出结果
    /// 之前）做：之后的外部效果才可能用到这些值，恢复时它们得原样回来。
    async fn flush_nondet(&self) -> Result<(), ToolError> {
        let (batch, index) = {
            let state = self.state();
            if state.pending.is_empty() {
                return Ok(());
            }
            (state.pending.clone(), state.batches + 1)
        };
        let candidate = serde_json::to_value(&batch).expect("读数可序列化");
        let winner = match self
            .rt
            .put_memo(&format!("{NONDET_MEMO}.{index}"), candidate.clone())
            .await
        {
            Ok(winner) => winner,
            Err(error) => return Err(host_error(self.rt, error).await),
        };
        if winner != candidate {
            return Err(uncertain(format!(
                "codemode 回放分歧：第 {index} 批时钟 / 随机数记录与已落账的不同"
            )));
        }
        let mut state = self.state();
        state.pending.drain(..batch.len());
        state.batches += 1;
        Ok(())
    }

    /// 脚本结束后的核对：宿主的致命错误、没走完的记录、没交付的调用、结果不明的调用。
    async fn check_end(&self, rt: &TaskRuntime) -> Result<(), ToolError> {
        let fail = |message: &str| uncertain(message);
        {
            let state = self.state();
            if let Some(error) = &state.fatal {
                return Err(error.clone());
            }
            if state.cursor < state.recorded {
                return Err(fail("codemode 回放分歧：脚本提前结束，未消费全部子调用"));
            }
            if state.replay_cursor != state.replay.len() {
                return Err(fail(
                    "codemode 回放分歧：脚本提前结束，未消费全部时钟 / 随机数记录",
                ));
            }
        }
        let docs = match load_docs(rt).await {
            Ok(docs) => docs,
            Err(error) => return Err(host_error(rt, error).await),
        };
        for (position, doc) in docs.iter().enumerate() {
            let key = nested_key(position + 1);
            match &doc.result {
                None => {
                    return Err(uncertain(format!(
                        "codemode 子调用 {key} ({}) 被中断且没有交付结果；停止脚本，禁止自动续写",
                        doc.name
                    )));
                }
                Some(result) if result.uncertain => {
                    return Err(uncertain(format!(
                        "codemode 已中断，子调用 {key} ({}) 的结果不明；不会自动重跑：{}",
                        doc.name,
                        result.error.as_deref().unwrap_or("uncertain")
                    )));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

/// 回答脚本的一次时钟 / 熵读取：录下来的先按序还；录完了读真值，记在 `pending`。
/// 请求对不上录下来的（脚本走了别的路）是恢复分歧，锁住 fatal。
fn read(state: &Mutex<State>, request: &Nondet) -> Result<Value, String> {
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(error) = &state.fatal {
        return Err(error.to_string());
    }
    if let Some(entry) = state.replay.get(state.replay_cursor) {
        if entry.request != *request {
            let error = uncertain(format!(
                "codemode 回放分歧：第 {} 次时钟 / 随机数读取与记录不符（记录 {:?}，现在 {:?}）",
                state.replay_cursor + 1,
                entry.request,
                request
            ));
            let message = error.to_string();
            state.fatal = Some(error);
            return Err(message);
        }
        let value = entry.value.clone();
        state.replay_cursor += 1;
        return Ok(value);
    }
    if state.replay.len() + state.pending.len() >= MAX_NONDET {
        return Err(format!("一段脚本最多读 {MAX_NONDET} 次时钟 / 随机数"));
    }
    let value = live(request)?;
    state.pending.push(NondetEntry {
        request: request.clone(),
        value: value.clone(),
    });
    Ok(value)
}

/// 提交被拒：几乎总是因为 invocation 结束了（取消、关闭）。那就等外层的取消信号到，让
/// `race` 把这次执行丢掉——不能把"提交被拒"当成脚本的结果发布出去（重启后会被当成完整输出复用）。
/// 等不到取消的才是真的存储错误，按结果不明处理。
async fn host_error(rt: &TaskRuntime, error: DurableError) -> ToolError {
    let cancelled = rt.cancelled();
    let _ = tokio::time::timeout(CANCEL_GRACE, cancelled.cancelled()).await;
    if cancelled.is_cancelled() {
        std::future::pending::<()>().await;
    }
    uncertain(format!("codemode 的状态提交失败：{error}；停止脚本"))
}

/// 外层名下所有嵌套调用的 document，按序号（`c1`、`c2`……连续，缺一个就到头）。
async fn load_docs(rt: &TaskRuntime) -> Result<Vec<NestedDoc>, DurableError> {
    let found = Arc::new(Mutex::new(Vec::new()));
    let out = found.clone();
    rt.commit(move |tx, task| {
        Box::pin(async move {
            let mut docs = Vec::new();
            for index in 1..=MAX_CALLS {
                let address = nested_address(task.id(), &nested_key(index));
                let Some((_, value)) = tx.document(&address).await? else {
                    break;
                };
                let doc: NestedDoc = serde_json::from_value(value).map_err(|e| {
                    DurableError::rejected(format!("komo.nested is malformed: {e}"))
                })?;
                docs.push(doc);
            }
            *out.lock().unwrap_or_else(|e| e.into_inner()) = docs;
            Ok(None::<NextTaskState>)
        })
    })
    .await?;
    let docs = std::mem::take(&mut *found.lock().unwrap_or_else(|e| e.into_inner()));
    Ok(docs)
}

async fn read_doc(rt: &TaskRuntime, key: &str) -> Result<Option<NestedDoc>, DurableError> {
    let found = Arc::new(Mutex::new(None));
    let out = found.clone();
    let key = key.to_owned();
    rt.commit(move |tx, task| {
        Box::pin(async move {
            let doc = match tx.document(&nested_address(task.id(), &key)).await? {
                Some((_, value)) => {
                    Some(serde_json::from_value::<NestedDoc>(value).map_err(|e| {
                        DurableError::rejected(format!("komo.nested is malformed: {e}"))
                    })?)
                }
                None => None,
            };
            *out.lock().unwrap_or_else(|e| e.into_inner()) = doc;
            Ok(None::<NextTaskState>)
        })
    })
    .await?;
    let doc = found.lock().unwrap_or_else(|e| e.into_inner()).take();
    Ok(doc)
}

/// abort 名下所有还活着的嵌套任务并等它们终结（自底向上由引擎做；这里是外层还活着、脚本
/// 已经结束时的收拾）。失败不挡路：外层自己被取消时引擎会接手。
async fn abort_nested(rt: &TaskRuntime) {
    let Ok(owned) = rt.owned_tasks().await else {
        return;
    };
    for record in owned {
        if let Err(error) = rt.abort_owned(record.id).await {
            tracing::debug!(task = %record.id, %error, "abort 嵌套任务没成功");
        }
    }
}

/// 脚本里发现工具用的说明（`docs/codemode.md` §6）：`names` 里每个工具的说明与 schema，
/// 以及它们所属的组。不进指纹：发现只影响脚本往哪里调，调用本身已由工具名与参数核对。
fn script_catalog(set: &ToolSet, names: &[String]) -> ScriptCatalog {
    let mut catalog = ScriptCatalog::default();
    for name in names {
        let Some(tool) = set.get(name) else {
            continue;
        };
        let definition = tool.definition();
        let namespace = tool.namespace();
        if let Some(namespace) = &namespace
            && !catalog
                .namespaces
                .iter()
                .any(|known| known.name == namespace.name)
        {
            catalog.namespaces.push(namespace.clone());
        }
        catalog.tools.push(ScriptTool {
            name: definition.name,
            description: definition.description,
            parameters: definition.parameters,
            namespace: namespace.map(|namespace| namespace.name),
        });
    }
    catalog
}
