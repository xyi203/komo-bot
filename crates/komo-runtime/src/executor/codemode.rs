//! codemode 在执行器里的那一半（`docs/codemode.md` §3、§5、§7）：跑脚本，回答脚本里的工具调用。
//!
//! 外层是一次普通调用（`start_call`、输出落盘都在 `execute_ready`）；这里只替换"执行"
//! 那一步。脚本里的每一次调用走 [`ToolExecutor::nested_call`]：能力面 → `prepare` →
//! 只读或 MCP 才放行 → 逐阶段记进 journal（`codemode_journal`）→ 执行。journal 不进模型
//! 消息，模型只看见外层结果；MCP 写操作因此可以放进脚本，中断后靠 journal 认回已交付的
//! 子结果，unsafe 的那一次绝不重做。范围外的读同样放行：脚本自己碰不到文件系统，
//! 边界来自语言，不来自名单（§7）。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, StoreError};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::plan::{Operation, Replay};
use komo_kernel::types::refs::{AttemptRef, ToolResultStatus};
use komo_kernel::types::tool::{ToolContext, ToolError, ToolOutput};
use serde::Serialize;

use super::codemode_journal::{Intent, Journal, Record, Reply, uncertain};
use super::{CallEnv, ToolExecutor};
use crate::codemode::{ScriptCatalog, ScriptTool};
use crate::tools::codemode::NOT_IN_SCRIPTS;

/// 摘要里每次调用的参数最多留这么长。
const ARGS_PREVIEW: usize = 512;

/// 一次内层调用的摘要（外层 `output.json` 的 `result.calls`）。
#[derive(Debug, Clone, Serialize)]
struct NestedCall {
    tool: String,
    args: String,
    ok: bool,
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ToolExecutor {
    /// 取消或超时打断脚本之后重读一遍 journal：宿主内存里锁住的 fatal 随执行一起丢了。
    /// 已交付但结果不明的、started 之后没交付的，都把外层结果改成 uncertain——不自动重跑。
    pub(super) async fn codemode_check_interrupted(
        &self,
        ctx: &ToolContext,
    ) -> Result<(), ToolError> {
        let journal = self.codemode_load(ctx).await?;
        for child in journal.children.values() {
            if let Some(reference) = &child.delivered {
                let reply: Reply = self.codemode_open(reference).await?;
                if reply.value.is_some() == reply.error.is_some() {
                    return Err(uncertain("codemode 已交付结果格式损坏"));
                }
                if let Some(execution) = &reply.execution {
                    self.outputs
                        .open(execution)
                        .await
                        .map_err(|e| uncertain(e.to_string()))?;
                }
                if reply.uncertain {
                    return Err(uncertain(format!(
                        "codemode 已中断，已交付子调用结果不明；不会自动重跑：{}",
                        reply.error.as_deref().unwrap_or("uncertain")
                    )));
                }
            } else if child.started {
                let partial = self.codemode_partial(child).await;
                return Err(uncertain(format!(
                    "codemode 已中断，子调用 started 后未提交交付结果；不会自动重跑{partial}"
                )));
            }
        }
        Ok(())
    }

    /// 跑一段脚本。journal 里已有 `finished` 就直接交回那份完整结果，不再起 worker。
    pub(super) async fn run_codemode(
        &self,
        code: &str,
        env: &CallEnv,
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let Some(worker) = self.codemode.as_ref() else {
            return Err(ToolError::Failed {
                message: "这台 Gateway 没有接 codemode".into(),
            });
        };
        let mut names: Vec<String> = env
            .surface
            .names()
            .iter()
            .filter(|name| !NOT_IN_SCRIPTS.contains(&name.as_str()))
            .cloned()
            .collect();
        names.sort();
        let mut journal = self.codemode_load(ctx).await?;
        let fingerprint = Self::codemode_fingerprint(code, &names, ctx);
        match &journal.fingerprint {
            Some(saved) if saved != &fingerprint => {
                return Err(uncertain("codemode 回放分歧：脚本、协议或能力面改变"));
            }
            None => {
                self.codemode_record(
                    ctx,
                    "start",
                    Record::Start {
                        fingerprint: fingerprint.clone(),
                    },
                )
                .await?;
                journal.fingerprint = Some(fingerprint);
            }
            _ => {}
        }
        if journal.finished.is_none() {
            self.codemode_load_nondet(&mut journal).await?;
        }
        if let Some(output) = &journal.finished {
            let output: ToolOutput = self.codemode_open(output).await?;
            sink.write_stdout(output.preview.as_deref().unwrap_or_default().as_bytes())
                .await
                .map_err(|e| uncertain(e.to_string()))?;
            return Ok(output);
        }
        // 被接续的 Run：能力面里的 MCP 工具可能还没挂上，先等它们的服务器，脚本才发现得到它们
        // （[`ToolExecutor::await_mcp`]）。没连上的在脚本调到时报原因。
        self.await_mcp(&names).await;
        let catalog = self.script_catalog(&names);
        let journal = Arc::new(Mutex::new(journal));
        let calls: Arc<Mutex<Vec<NestedCall>>> = Arc::default();
        let reads = Arc::clone(&journal);
        let outcome = worker
            .clone()
            .in_dir(ctx.cwd.clone())
            .with_nondet(Arc::new(move |request| {
                reads.lock().expect("journal").read(request)
            }))
            .run(code, &names, &catalog, env.call_timeout, |name, args| {
                let calls = Arc::clone(&calls);
                let journal = Arc::clone(&journal);
                async move {
                    let started = Instant::now();
                    let preview = truncate(&args.to_string(), ARGS_PREVIEW);
                    let result = self.nested_call(&name, args, env, ctx, &journal).await;
                    calls.lock().expect("调用摘要").push(NestedCall {
                        tool: name,
                        args: preview,
                        ok: result.is_ok(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        error: result.as_ref().err().cloned(),
                    });
                    result
                }
            })
            .await;
        if let Some(message) = outcome.incompatible {
            // 脚本一行没跑；但恢复时前几次尝试可能已经做过子调用，那就不能当普通失败。
            return Err(if journal.lock().expect("journal").children.is_empty() {
                ToolError::Failed { message }
            } else {
                uncertain(message)
            });
        }
        let (end, pending) = {
            let journal = journal.lock().expect("journal");
            (
                journal.check_end(),
                journal
                    .children
                    .values()
                    .find(|c| c.started && c.delivered.is_none())
                    .cloned(),
            )
        };
        if let Err(error) = end {
            let partial = match pending {
                Some(child) => self.codemode_partial(&child).await,
                None => String::new(),
            };
            return Err(uncertain(format!("{error}{partial}")));
        }
        let calls = std::mem::take(&mut *calls.lock().expect("调用摘要"));

        let mut text = join_outputs(&outcome.outputs);
        if !outcome.console.is_empty() {
            text.push_str(&format!("\n<console>\n{}</console>", outcome.console));
        }
        if let Some(error) = &outcome.error {
            text.push_str(&format!("\n脚本出错：{error}"));
        }
        if !calls.is_empty() {
            text.push_str(&format!("\n（{}）", summary(&calls)));
        }
        let text = text.trim_start();
        let output = ToolOutput {
            status: if outcome.error.is_some() {
                ToolResultStatus::Failed
            } else {
                ToolResultStatus::Completed
            },
            result: serde_json::json!({ "calls": calls, "error": outcome.error }),
            exit_code: None,
            artifacts: vec![],
            preview: Some(text.to_owned()),
        };
        // 先把整段结果外置、提交 `finished`，再交给外层发布：两步之间崩了，恢复时直接
        // 交回这份结果，不再起 worker。
        self.codemode_flush_nondet(ctx, &journal).await?;
        let reference = self.codemode_save(ctx, "finished", &output).await?;
        self.codemode_record(ctx, "finished", Record::Finished { output: reference })
            .await?;
        sink.write_stdout(text.as_bytes())
            .await
            .map_err(|error| ToolError::Failed {
                message: error.to_string(),
            })?;
        Ok(output)
    }

    /// 脚本里发现工具用的说明（`docs/codemode.md` §6）：`names` 里每个工具的说明与 schema，
    /// 以及它们所属的组。不进指纹：发现只影响脚本往哪里调，调用本身已由工具名与参数核对。
    fn script_catalog(&self, names: &[String]) -> ScriptCatalog {
        let tools = self.tools();
        let mut catalog = ScriptCatalog::default();
        for name in names {
            let Some(tool) = tools.get(name) else {
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

    /// `start` 记下的指纹：脚本、引擎与 worker 协议版本、能力表、cwd 任一变了，恢复就是分歧。
    /// 升级前（CPython driver 时代）在途的调用指纹里是 `driver` / `env`，对不上，按分歧返回
    /// uncertain。
    pub(super) fn codemode_fingerprint(
        code: &str,
        names: &[String],
        ctx: &ToolContext,
    ) -> serde_json::Value {
        serde_json::json!({
            "version": 3,
            "code": ContentHash::of_str(code),
            "engine": format!("monty-{}", crate::codemode::MONTY_VERSION),
            "worker": crate::codemode::WORKER_PROTOCOL,
            "prelude": crate::codemode::prelude_hash(),
            "tools": names,
            "cwd": ctx.cwd,
        })
    }

    /// 脚本里的一次工具调用（`docs/codemode.md` §5）。错误就是交给脚本的 `ToolError` 正文；
    /// 结果不明或宿主自己出错时锁住 fatal，之后的调用一律不再执行，脚本接住异常也解不开。
    pub(super) async fn nested_call(
        &self,
        name: &str,
        args: serde_json::Value,
        env: &CallEnv,
        ctx: &ToolContext,
        journal: &Mutex<Journal>,
    ) -> Result<serde_json::Value, String> {
        let result = self
            .durable_nested_call(name, args, env, ctx, journal)
            .await;
        match result {
            Ok(reply) => {
                if reply.uncertain {
                    journal.lock().expect("journal").fatal =
                        Some(uncertain(reply.error.clone().unwrap_or_else(|| {
                            "MCP 子调用结果不明；停止脚本后续调用".into()
                        })));
                    return Err("MCP 子调用结果不明；停止脚本后续调用".into());
                }
                reply.result()
            }
            Err(error) => {
                journal.lock().expect("journal").fatal = Some(error.clone());
                Err(error.to_string())
            }
        }
    }

    /// 一次子调用的 journal 协议（`docs/codemode.md` §7 的表）：intent → started → 执行 →
    /// 发布输出 → delivered，每一步提交之后才走下一步；恢复时从已记录的阶段接着走。
    pub(super) async fn durable_nested_call(
        &self,
        name: &str,
        args: serde_json::Value,
        env: &CallEnv,
        ctx: &ToolContext,
        journal: &Mutex<Journal>,
    ) -> Result<Reply, ToolError> {
        let (index, child) = journal.lock().expect("journal").next(name)?;
        // 能力面里有、还没挂上的 MCP 工具先等它的服务器（[`ToolExecutor::await_mcp`]）。
        let mut pending_mcp = if env.surface.allows(name) && !NOT_IN_SCRIPTS.contains(&name) {
            self.await_mcp(&[name.to_string()]).await
        } else {
            Default::default()
        };
        // 已记下的结果也要过当前能力面；不回退到全局工具表。
        let tools = self.tools();
        let tool = tools
            .get(name)
            .filter(|_| env.surface.allows(name) && !NOT_IN_SCRIPTS.contains(&name));
        let mut child_ctx = ctx.clone();
        let attempt = Self::codemode_attempt(ctx, &format!("c{index}"));
        child_ctx.call = attempt.call.clone();
        child_ctx.attempt = attempt.attempt.clone();
        child_ctx.resumed = None;
        // 脚本等着内层调用的结果往下走：转后台只对模型直接发的调用有意义。
        child_ctx.detach_after = None;
        let intent: Intent = match child.as_ref().and_then(|c| c.intent.as_ref()) {
            Some(reference) => {
                let intent: Intent = self.codemode_open(reference).await?;
                if intent.name != name || intent.args != args {
                    return Err(uncertain(format!(
                        "codemode 回放分歧：子调用 c{index} 的工具名或参数改变"
                    )));
                }
                if tool.is_none() {
                    return Err(uncertain(format!("codemode 恢复能力面中没有 {name}")));
                }
                intent
            }
            None => {
                // 这次新调用的参数可能用到了脚本刚读到的时钟 / 随机数：先把它们落账。
                self.codemode_flush_nondet(ctx, journal).await?;
                let prepared = match tool {
                    Some(tool) => tool
                        .prepare(args.clone(), &child_ctx)
                        .await
                        .and_then(|plan| {
                            // 放行的是有副作用的 MCP，不是经 `Tool::execute` 的本地写入或编排。
                            if plan.then_run.is_some()
                                || !matches!(
                                    plan.operation,
                                    Operation::ReadFile | Operation::McpCall { .. }
                                )
                            {
                                Err(ToolError::Denied {
                                    reason: format!(
                                        "{name} 不是只读工具或 MCP；脚本里不支持此操作"
                                    ),
                                })
                            } else {
                                Ok(plan)
                            }
                        }),
                    // 热重载撤掉的、服务器还没连上的 MCP 工具说清楚为什么调不了；那台服务器不在
                    // 能力面里的照旧拒。
                    None => Err(ToolError::Denied {
                        reason: match self
                            .withdrawn_reason(name)
                            .or_else(|| pending_mcp.remove(name))
                        {
                            Some(reason) if env.surface.allows(name) => reason,
                            _ => format!("脚本里不能调 {name}"),
                        },
                    }),
                };
                let intent = match prepared {
                    Ok(plan) => Intent {
                        name: name.into(),
                        args,
                        replay: plan
                            .recovery
                            .replay()
                            .and(tool.expect("prepared tool").replay()),
                        plan: Some(plan),
                        error: None,
                    },
                    Err(error) => Intent {
                        name: name.into(),
                        args,
                        replay: Replay::Unsafe,
                        plan: None,
                        error: Some(error.to_string()),
                    },
                };
                let output = self
                    .codemode_save(ctx, &format!("intent-{index}"), &intent)
                    .await?;
                self.codemode_record(
                    ctx,
                    &format!("intent-{index}"),
                    Record::Intent {
                        index,
                        output: output.clone(),
                    },
                )
                .await?;
                journal.lock().expect("journal").children.insert(
                    index,
                    super::codemode_journal::Child {
                        intent: Some(output),
                        ..Default::default()
                    },
                );
                intent
            }
        };
        if let Some(reference) = child.as_ref().and_then(|c| c.delivered.as_ref()) {
            let reply: Reply = self.codemode_open(reference).await?;
            if reply.value.is_some() == reply.error.is_some() {
                return Err(uncertain("codemode 已交付结果格式损坏"));
            }
            if let Some(execution) = &reply.execution {
                self.outputs
                    .open(execution)
                    .await
                    .map_err(|e| uncertain(format!("子调用输出校验失败：{e}")))?;
            }
            return Ok(reply);
        }
        let reply = if let Some(plan) = intent.plan {
            let tool = tool.ok_or_else(|| uncertain("子调用工具已消失"))?;
            if child.as_ref().is_some_and(|c| c.started) {
                if intent.replay.and(tool.replay()) != Replay::Safe {
                    let partial = self
                        .codemode_partial(child.as_ref().expect("started child"))
                        .await;
                    return Err(uncertain(format!(
                        "codemode 子调用 c{index} ({name}) 已 started 但无交付结果；unsafe 不重跑{partial}"
                    )));
                }
            }
            if child.is_some() {
                // 提交过 intent、还没 started 的也要按当前约束再 prepare 一次；
                // 执行的仍是原来那份计划。
                let current = tool
                    .prepare(intent.args, &child_ctx)
                    .await
                    .map_err(|e| uncertain(format!("恢复 prepare 失败：{e}")))?;
                if current.then_run.is_some()
                    || plan.then_run.is_some()
                    || current.operation != plan.operation
                    || !matches!(
                        current.operation,
                        Operation::ReadFile | Operation::McpCall { .. }
                    )
                {
                    return Err(uncertain("恢复工具操作类型改变"));
                }
            }
            self.codemode_record(
                ctx,
                &format!("started-{index}-{}", ctx.attempt),
                Record::Started {
                    index,
                    attempt: attempt.clone(),
                },
            )
            .await?;
            {
                let mut journal = journal.lock().expect("journal");
                let child = journal.children.get_mut(&index).expect("intent");
                child.started = true;
                child.attempt = Some(attempt.clone());
            }
            let writer = self
                .outputs
                .begin(&attempt)
                .await
                .map_err(|e| uncertain(e.to_string()))?;
            let mut capture = Capture::persistent(writer);
            let result = tool.execute(plan, &child_ctx, &mut capture).await;
            // 工具可能把流写失败转成 Failed，甚至整个吞掉；宿主不管它怎么转，自己记着结果不明。
            if let Some(error) = &capture.write_error {
                return Err(uncertain(format!("子调用输出写入失败：{error}")));
            }
            let reply = match &result {
                Ok(output) => {
                    let text = match output.result.get("text").and_then(|text| text.as_str()) {
                        Some(text) => text.to_string(),
                        None if !capture.bytes.is_empty() => {
                            String::from_utf8_lossy(&capture.bytes).into()
                        }
                        None => output.preview.clone().unwrap_or_default(),
                    };
                    Reply {
                        value: Some(serde_json::json!({
                            "status": output.status,
                            "text": text,
                            "result": output.result,
                        })),
                        error: None,
                        uncertain: output.status == ToolResultStatus::Uncertain,
                        execution: None,
                    }
                }
                Err(error) => Reply {
                    value: None,
                    error: Some(error.to_string()),
                    uncertain: error.is_uncertain()
                        || matches!(error, ToolError::Cancelled | ToolError::Timeout { .. }),
                    execution: None,
                },
            };
            // 底层输出（流与元信息）照原样发布；脚本收到的那份精确回复另存，交付不依赖重新投影。
            let execution = self
                .outputs
                .publish(
                    capture.writer.take().expect("persistent capture"),
                    super::result_body(&result),
                )
                .await
                .map_err(|e| uncertain(format!("子调用输出发布失败：{e}")))?;
            Reply {
                execution: Some(execution.output),
                ..reply
            }
        } else {
            Reply {
                value: None,
                error: intent.error,
                uncertain: false,
                execution: None,
            }
        };
        let output = self
            .codemode_save(ctx, &format!("reply-{index}"), &reply)
            .await?;
        // 交付先提交，再回给 Python。这一笔没写进去就中断的，恢复时不拿没被引用的
        // output.json 去猜。
        self.codemode_record(
            ctx,
            &format!("delivered-{index}"),
            Record::Delivered {
                index,
                output: output.clone(),
            },
        )
        .await?;
        journal
            .lock()
            .expect("journal")
            .children
            .get_mut(&index)
            .expect("intent")
            .delivered = Some(output);
        Ok(reply)
    }
}

fn summary(calls: &[NestedCall]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for call in calls {
        match counts.iter_mut().find(|(tool, _)| *tool == call.tool) {
            Some((_, count)) => *count += 1,
            None => counts.push((&call.tool, 1)),
        }
    }
    let failed = calls.iter().filter(|call| !call.ok).count();
    let listed = counts
        .iter()
        .map(|(tool, count)| format!("{tool}×{count}"))
        .collect::<Vec<_>>()
        .join("、");
    match failed {
        0 => format!("调用了 {} 次工具：{listed}", calls.len()),
        n => format!("调用了 {} 次工具：{listed}；{n} 次失败", calls.len()),
    }
}

/// 多段 `text()` 拼起来：两段以上时每段前加 `==> text N/M <==`（学 pi 1.1.0）——直接用换行
/// 拼，模型分不清一段多行输出和两段输出的边界。只有一段时原样给，不多一个字。
fn join_outputs(outputs: &[String]) -> String {
    if outputs.len() <= 1 {
        return outputs.join("\n");
    }
    let total = outputs.len();
    outputs
        .iter()
        .enumerate()
        .map(|(index, output)| format!("==> text {}/{total} <==\n{output}", index + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…（截断）", &text[..end])
}

/// 内层调用的输出：流照样写进子调用自己的尝试目录，同时在内存里留一份给脚本的 `text`
/// 兜底；统一投影层负责限制给模型的视图。流写失败记在 `write_error`，不随工具怎么处理而消失。
struct Capture {
    attempt: AttemptRef,
    bytes: Vec<u8>,
    writer: Option<Box<dyn OutputWriter>>,
    write_error: Option<String>,
}

impl Capture {
    #[cfg(test)]
    fn new(attempt: AttemptRef) -> Self {
        Self {
            attempt,
            bytes: Vec::new(),
            writer: None,
            write_error: None,
        }
    }

    fn persistent(writer: Box<dyn OutputWriter>) -> Self {
        Self {
            attempt: writer.attempt().clone(),
            bytes: Vec::new(),
            writer: Some(writer),
            write_error: None,
        }
    }
}

#[async_trait]
impl OutputWriter for Capture {
    async fn write_stdout(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        if let Some(writer) = self.writer.as_mut() {
            if let Err(error) = writer.write_stdout(chunk).await {
                self.write_error.get_or_insert_with(|| error.to_string());
                return Err(error);
            }
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    async fn write_stderr(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        if let Some(writer) = self.writer.as_mut() {
            if let Err(error) = writer.write_stderr(chunk).await {
                self.write_error.get_or_insert_with(|| error.to_string());
                return Err(error);
            }
        }
        Ok(())
    }

    fn attempt(&self) -> &AttemptRef {
        &self.attempt
    }

    fn bytes_written(&self) -> u64 {
        self.writer
            .as_ref()
            .map_or(self.bytes.len() as u64, |w| w.bytes_written())
    }
}

#[cfg(test)]
mod tests {
    use super::join_outputs;

    #[tokio::test]
    async fn nested_output_above_one_mib_is_not_lost_before_projection() {
        use komo_kernel::traits::OutputWriter;
        use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
        let mut capture = super::Capture::new(super::AttemptRef {
            session: SessionId::from_raw("session"),
            run: RunId::from_raw("run"),
            call: ToolCallId::from_raw("call"),
            attempt: AttemptId::from_raw("attempt"),
        });
        let chunk = vec![b'x'; 700_000];
        capture.write_stdout(&chunk).await.unwrap();
        capture.write_stdout(&chunk).await.unwrap();
        capture.write_stdout(b"LAST").await.unwrap();
        assert_eq!(capture.bytes.len(), 1_400_004);
        assert!(capture.bytes.ends_with(b"LAST"));
    }

    #[test]
    fn a_single_output_is_left_as_is() {
        assert_eq!(join_outputs(&["a\nb".to_string()]), "a\nb");
        assert_eq!(join_outputs(&[]), "");
    }

    #[test]
    fn several_outputs_are_headed_with_their_position() {
        let joined = join_outputs(&["a\nb".to_string(), "c".to_string()]);
        assert_eq!(joined, "==> text 1/2 <==\na\nb\n==> text 2/2 <==\nc");
    }
}
