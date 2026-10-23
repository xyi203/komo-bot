//! `komo.tool`（`docs/storage-durable.md` §4.5、§5、§12.5；pi-durable spec §8.4，
//! `harness/tool.ts`）：一个模型调用。
//!
//! 输入 `{ "kind": "model", "assistant": 13, "callId": "call_3" }`——调用本身从 assistant entry
//! 读，任务记录保持很小。checkpoint：
//!
//! ```json
//! { "phase": "call" }
//! { "phase": "execute", "arguments": {…}, "plan": {…ExecutionPlan…}, "replay": "safe", "attempt": 1 }
//! ```
//!
//! # effect sandwich
//!
//! ```text
//! call:     找工具 → 能力面 → Tool::prepare → [提交意图：slot running + checkpoint execute]
//!           → 执行（stdout / stderr 流进 .partial，尾巴节流写槽位）
//!           → 完整输出落盘（同步 → 改名 → 同步目录）→ [提交结果 entry + 清槽位 + terminal]
//! execute:  只由恢复进入（spec §8.4），照设计 §5 的表：
//!             盘上有完整 output.json     → 补提交结果，复用原输出，不重做
//!             存下的与当前 replay 都 Safe → 新的一次尝试重跑（同一个 task、attempt + 1）
//!             否则                       → 中断结果（uncertain）：§8.6 的正文 + .partial 末尾各 2 KiB
//!             工具名已经没了             → 同样按中断处理（不问工具，不看重放策略）
//! ```
//!
//! 意图之前（checkpoint 还是 `call`）崩溃的确定没执行过，`call` 照常再来一遍。**大正文总是
//! 先于引用它的 entry**：`ToolOutputStore::publish` 同步完文件才返回，之后才提交结果 entry。
//!
//! 结果 entry 的 `model` 是 `projection.rs` 的 full 视图，**收尾时算一次、存进去**——"刚跑完"与
//! "重启后回放"逐字节相同，因为存下的就是那份。decayed 视图（若有）放在 `data.decayed`，D4c 的
//! 衰减只需要一条 `replace` 的 edit。entry 的 `data`：
//!
//! ```json
//! { "tool": "bash", "status": "completed", "attempt": 1, "elapsedMs": 12,
//!   "output": { "path": "tool-output/1/9/1/output.json", "size": 310, "hash": "…" },
//!   "stdout": {…ContentRef…}, "stderr": {…ContentRef…}, "decayed": "…" }
//! ```

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::durable::task::{NextTaskState, RunningTask};
use komo_kernel::durable::{
    ContentBlock, ConversationId, EntryId, Message, TaskId, ToolResultMessage,
};
use komo_kernel::projection::{ProjectionContext, ToolResultFacts, project_views, select};
use komo_kernel::traits::{OutputWriter, StoreError, Tool};
use komo_kernel::types::ids::{AttemptId, RunId, ToolCallId};
use komo_kernel::types::plan::{ExecutionPlan, Replay};
use komo_kernel::types::refs::{
    AttemptRef, InterruptedOutput, PublishedOutput, ToolResultBody, ToolResultStatus,
};
use komo_kernel::types::status::ToolCallState;
use komo_kernel::types::tool::{CancelToken, InterjectSignal, ResumedCall, ToolContext, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::TOOL_KIND;
use super::entries::{self, TOOL_RESULT};
use super::hooks::{HarnessDeps, HookError, ToolEnv, ToolSet};
use super::live::{self, SlotStatus};
use super::outcome::{aborted, completed, failed, running};
use crate::durable::error::TaskError;
use crate::durable::kind::TaskKind;
use crate::durable::runtime::TaskRuntime;
use crate::durable::tx::EntryDraft;
use crate::executor::cancel::race;
use crate::executor::{
    INTERRUPTED_TAIL_BYTES, REAP_GRACE, interrupted_message, result_body, store_images,
    timeout_error,
};

/// 输入（只认 model 发起的调用；`kind` 为以后的嵌套调用留着）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    assistant: EntryId,
    call_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "phase",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Checkpoint {
    Call,
    /// 意图：提交之后才允许产生真实副作用（§8.5）。
    Execute {
        arguments: Value,
        plan: Box<ExecutionPlan>,
        replay: Replay,
        /// 当前这次尝试（从 1 数）；输出落在 `tool-output/{conversation}/{task}/{attempt}/`。
        attempt: u32,
    },
}

fn encode(checkpoint: &Checkpoint) -> Value {
    serde_json::to_value(checkpoint).expect("Checkpoint 可序列化")
}

/// 模型这次要的调用。
#[derive(Debug, Clone)]
struct Call {
    id: String,
    name: String,
    arguments: Value,
}

/// 一次结果怎么收尾任务。
#[derive(Debug, Clone)]
enum Ending {
    /// 工具跑完了（含 `isError`：失败的结果照样让任务 completed）。
    Completed,
    /// 执行被打断或抛错：任务 `failed`，记录取消意图（spec §8.4）。
    Failed(String),
    /// abort handler 写的结果。
    Aborted,
}

/// 收尾前算好的结果：模型视图、decayed 视图、图片引用与 entry 的 `data`。
struct Rendered {
    view: String,
    is_error: bool,
    images: Vec<ContentBlock>,
    data: Value,
}

fn render(
    tool: &str,
    published: &PublishedOutput,
    attempt: u32,
    projection: &ProjectionContext,
) -> Result<Rendered, TaskError> {
    let body = published
        .body
        .as_deref()
        .ok_or_else(|| TaskError::new("the published output carries no body"))?;
    let facts = ToolResultFacts {
        tool,
        status: published.status,
        elapsed_ms: published.elapsed_ms,
        text: body.preview.as_deref(),
        output: &published.output,
        stdout: published.stdout.as_ref(),
        stderr: published.stderr.as_ref(),
        artifacts: &body.artifacts,
        images: &body.images,
    };
    let projected = project_views(&facts, projection);
    let decayed = projected.decayed.clone();
    // 刚跑完的结果从不处于已衰减状态；之后要不要换短视图是 D4c 的缓存账。
    let view = select(projected, false);
    let images = body
        .images
        .iter()
        .map(|image| {
            let mut extra = serde_json::Map::new();
            extra.insert("mime".into(), json!(image.mime));
            extra.insert("path".into(), json!(image.content.path));
            extra.insert("hash".into(), json!(image.content.hash.as_str()));
            ContentBlock::Image { extra }
        })
        .collect();
    let mut data = json!({
        "tool": tool,
        "status": published.status,
        "attempt": attempt,
        "elapsedMs": published.elapsed_ms,
        "output": published.output,
    });
    if let Some(stdout) = &published.stdout {
        data["stdout"] = json!(stdout);
    }
    if let Some(stderr) = &published.stderr {
        data["stderr"] = json!(stderr);
    }
    if let Some(decayed) = decayed {
        data["decayed"] = json!(decayed);
    }
    Ok(Rendered {
        view,
        is_error: published.status != ToolResultStatus::Completed,
        images,
        data,
    })
}

fn store_error(error: StoreError) -> TaskError {
    TaskError::new(error.to_string())
}

fn failed_body(message: String) -> ToolResultBody {
    ToolResultBody {
        status: ToolResultStatus::Failed,
        result: Value::Null,
        error: Some(message.clone()),
        exit_code: None,
        artifacts: vec![],
        images: vec![],
        preview: Some(message),
    }
}

/// 一次尝试的身份：落盘目录是 `tool-output/{conversation}/{tool_task}/{attempt}/`。
fn attempt_ref(
    env: &ToolEnv,
    conversation: ConversationId,
    task: TaskId,
    attempt: u32,
) -> AttemptRef {
    AttemptRef {
        session: env.session.clone(),
        run: RunId::from_raw(conversation.to_string()),
        call: ToolCallId::from_raw(task.to_string()),
        attempt: AttemptId::from_raw(attempt.to_string()),
    }
}

/// 工具任务。克隆便宜。
#[derive(Clone)]
pub struct ToolTask {
    deps: Arc<HarnessDeps>,
}

impl ToolTask {
    pub fn new(deps: Arc<HarnessDeps>) -> Self {
        Self { deps }
    }

    async fn tools(&self, conversation: ConversationId) -> Result<ToolSet, TaskError> {
        self.deps
            .hooks
            .tools(conversation)
            .await
            .map_err(|error| match error {
                HookError::NoModel(message) | HookError::Failed(message) => TaskError::new(message),
            })
    }

    /// 调用本身：从 assistant entry 读。
    async fn read_call(&self, rt: &TaskRuntime, input: &Input) -> Result<Call, TaskError> {
        let entry = rt
            .entry(input.assistant)
            .await?
            .ok_or_else(|| TaskError::new(format!("entry {} does not exist", input.assistant)))?;
        entry
            .model
            .iter()
            .flatten()
            .find_map(|message| match message {
                Message::Assistant(message) => {
                    message.content.iter().find_map(|block| match block {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } if *id == input.call_id => Some(Call {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        }),
                        _ => None,
                    })
                }
                _ => None,
            })
            .ok_or_else(|| {
                TaskError::new(format!(
                    "entry {} has no tool call {}",
                    input.assistant, input.call_id
                ))
            })
    }

    fn context(
        &self,
        env: &ToolEnv,
        conversation: ConversationId,
        task: TaskId,
        attempt: u32,
        resumed: Option<ResumedCall>,
        cancel: CancelToken,
    ) -> ToolContext {
        let attempt_ref = attempt_ref(env, conversation, task, attempt);
        ToolContext {
            session: env.session.clone(),
            run: attempt_ref.run,
            call: attempt_ref.call,
            attempt: attempt_ref.attempt,
            source: env.source.clone(),
            cwd: env.cwd.clone(),
            roots: env.roots.clone(),
            mounts: env.mounts.clone(),
            resumed,
            cancel,
            call_timeout: env.call_timeout,
            detach_after: env.detach_after,
            interjected: InterjectSignal::new(),
            conversation: env.conversation,
        }
    }

    // ------------------------------------------------------------------ call

    async fn call(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        let input = parse_input(task)?;
        let call = self.read_call(rt, &input).await?;
        let conversation = rt.conversation();
        let set = self.tools(conversation).await?;
        // 能力边界先于"有没有这个工具"（§4 末）：不在能力面里就等于不认识这个名字。
        let Some(tool) = set.get(&call.name).cloned() else {
            let names = set.surface().names();
            let message = if names.is_empty() {
                format!("这次运行没有可用的工具，{} 调不了", call.name)
            } else {
                format!(
                    "这次运行的工具集里没有 {}；可用的是：{}",
                    call.name,
                    names.join("、")
                )
            };
            return self.settle_unrun(rt, &set, &call, message).await;
        };
        let cancel = CancelToken::new();
        let ctx = self.context(&set.env, conversation, rt.task_id(), 1, None, cancel);
        // `prepare` 不产生副作用，但可能慢（MCP）：取消赢了就停，意图还没提交，重来一遍无妨。
        let cancelled = rt.cancelled();
        let prepared = tokio::select! {
            biased;
            _ = cancelled.cancelled() => return Err(TaskError::new("the tool call was cancelled")),
            prepared = tool.prepare(call.arguments.clone(), &ctx) => prepared,
        };
        let plan = match prepared {
            Ok(plan) => plan,
            Err(error) => return self.settle_unrun(rt, &set, &call, error.to_string()).await,
        };
        // 组合调用的第二步是一条 shell 命令：这次运行没有 bash 就不放行（§4 末）。
        if plan.then_run.is_some() && !set.can_run_commands() {
            let message = format!(
                "{} 的 then_run 要跑一条命令，但这次运行的工具集里没有 bash；\
                 去掉 then_run 再调一次",
                call.name
            );
            return self.settle_unrun(rt, &set, &call, message).await;
        }
        let replay = plan.recovery.replay();
        let checkpoint = Checkpoint::Execute {
            arguments: call.arguments.clone(),
            plan: Box::new(plan.clone()),
            replay,
            attempt: 1,
        };
        // 意图：槽位 running + checkpoint execute，同一次提交。提交返回之后才允许有副作用。
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                if let Some(slot) = live.slot_mut(task.id()) {
                    slot.status = SlotStatus::Running;
                    slot.replay = Some(replay);
                    slot.attempt = Some(1);
                    slot.clear_progress();
                }
                live::store(tx, conversation, &live).await?;
                Ok(Some(running(encode(&checkpoint))))
            })
        })
        .await?;
        self.run(rt, &set, &call, tool, plan, 1, None).await
    }

    /// 没有执行的调用的结论（工具不认识、参数准备不出来、被拒）：落盘的是失败结果，不是
    /// 悬着——任何从 transcript 重建的请求都不能带着一个没有输出的调用。
    async fn settle_unrun(
        &self,
        rt: &TaskRuntime,
        set: &ToolSet,
        call: &Call,
        message: String,
    ) -> Result<(), TaskError> {
        let attempt = attempt_ref(&set.env, rt.conversation(), rt.task_id(), 1);
        let published = self
            .publish(&set.env, &attempt, failed_body(message))
            .await?;
        self.commit_result(rt, set, call, &published, 1, Ending::Completed)
            .await
    }

    async fn publish(
        &self,
        env: &ToolEnv,
        attempt: &AttemptRef,
        body: ToolResultBody,
    ) -> Result<PublishedOutput, TaskError> {
        // `begin` 会接管这次尝试遗留的 `.partial`：中断前的输出随结果发布成 stdout / stderr。
        let writer = env.outputs.begin(attempt).await.map_err(store_error)?;
        env.outputs.publish(writer, body).await.map_err(store_error)
    }

    // ------------------------------------------------------------------ 执行

    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        rt: &TaskRuntime,
        set: &ToolSet,
        call: &Call,
        tool: Arc<dyn Tool>,
        plan: ExecutionPlan,
        attempt: u32,
        resumed: Option<ResumedCall>,
    ) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        let env = &set.env;
        let attempt_ref = attempt_ref(env, conversation, rt.task_id(), attempt);
        let mut writer = env.outputs.begin(&attempt_ref).await.map_err(store_error)?;
        let settings = self.deps.settings();

        let tail = Arc::new(Mutex::new(Tail::new(settings.tail_bytes)));
        let stop = CancellationToken::new();
        let progress = tokio::spawn(commit_output(
            rt.clone(),
            tail.clone(),
            stop.clone(),
            settings.output_interval,
        ));
        // kernel 的取消旗只有一个 flag：把引擎的取消信号接到它上面。
        let cancel = CancelToken::new();
        let bridge = {
            let (cancel, signal, done) = (cancel.clone(), rt.cancelled(), stop.clone());
            tokio::spawn(async move {
                tokio::select! {
                    _ = signal.cancelled() => cancel.cancel(),
                    _ = done.cancelled() => {}
                }
            })
        };
        let ctx = self.context(
            env,
            conversation,
            rt.task_id(),
            attempt,
            resumed,
            cancel.clone(),
        );

        let started = std::time::Instant::now();
        let executed = {
            let mut sink = Tee {
                inner: writer.as_mut(),
                tail: tail.clone(),
            };
            let execution = async {
                match plan.then_run {
                    // 组合调用：改动 → 核对 → 命令，一条结果（`tools::fusion`）。
                    Some(_) => match set.get("bash") {
                        Some(shell) => {
                            crate::tools::fusion::execute_fused(
                                &*tool,
                                plan.clone(),
                                &**shell,
                                &ctx,
                                &mut sink,
                            )
                            .await
                        }
                        None => Err(ToolError::Failed {
                            message: "then_run 要跑命令，但这次运行的工具集里没有 bash".into(),
                        }),
                    },
                    None => tool.execute(plan.clone(), &ctx, &mut sink).await,
                }
            };
            race(
                &cancel,
                tokio::time::timeout(env.call_timeout + REAP_GRACE, execution),
            )
            .await
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        stop.cancel();
        let _ = progress.await;
        let _ = bridge.await;

        // 取消赢了（abort 标记或关闭）：不发布、不写结果。`.partial` 留在盘上，abort handler
        // 或重启后的 `execute` 阶段接管它。
        let Some(executed) = executed else {
            return Err(TaskError::new("the tool call was cancelled"));
        };
        let outcome = match executed {
            Err(_elapsed) => Err(timeout_error(&plan, env.call_timeout)),
            Ok(result) => result,
        };
        let mut body = result_body(&outcome);
        if let Ok(output) = &outcome {
            let _ = store_images(writer.as_mut(), &mut body, &output.images).await;
        }
        // 完整输出先落盘（同步 → 改名 → 同步目录），之后才提交引用它的 entry。
        let mut published = env
            .outputs
            .publish(writer, body)
            .await
            .map_err(store_error)?;
        published.elapsed_ms = elapsed_ms;
        self.commit_result(rt, set, call, &published, attempt, Ending::Completed)
            .await
    }

    /// 提交结果 entry、清槽位、让任务终结。
    async fn commit_result(
        &self,
        rt: &TaskRuntime,
        set: &ToolSet,
        call: &Call,
        published: &PublishedOutput,
        attempt: u32,
        ending: Ending,
    ) -> Result<(), TaskError> {
        let rendered = render(&call.name, published, attempt, &set.env.projection)?;
        let (call_id, name) = (call.id.clone(), call.name.clone());
        let conversation = rt.conversation();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut content = vec![ContentBlock::text(rendered.view)];
                content.extend(rendered.images);
                let message = ToolResultMessage {
                    tool_call_id: call_id,
                    tool_name: name,
                    content,
                    is_error: rendered.is_error,
                    details: None,
                    timestamp: tx.now(),
                    extra: Default::default(),
                };
                let draft = EntryDraft::new(TOOL_RESULT)
                    .with_model(vec![Message::ToolResult(message)])
                    .with_data(rendered.data);
                let entry = tx.append_entry(conversation, draft).await?;
                let mut live = live::load(tx, conversation).await?;
                if let Some(slot) = live.slot_mut(task.id()) {
                    slot.finish();
                    slot.entry = Some(entry.id);
                }
                live::store(tx, conversation, &live).await?;
                let result = json!({ "kind": "model", "entryId": entry.id });
                Ok(Some(match ending {
                    Ending::Completed => completed(result),
                    Ending::Failed(message) => failed(message, "interrupted", Some(result)),
                    Ending::Aborted => aborted(Some(result)),
                }))
            })
        })
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------------ execute（恢复）

    /// 只由恢复进入。设计 §5 的表，一行一个分支。
    async fn execute(
        &self,
        task: &RunningTask,
        rt: &TaskRuntime,
        arguments: Value,
        plan: ExecutionPlan,
        stored: Replay,
        attempt: u32,
    ) -> Result<(), TaskError> {
        let input = parse_input(task)?;
        let call = self.read_call(rt, &input).await?;
        let conversation = rt.conversation();
        let set = self.tools(conversation).await?;
        let this = attempt_ref(&set.env, conversation, rt.task_id(), attempt);

        // 盘上有身份齐全、哈希对得上的 output.json：补提交结果，复用原输出，不重做。
        if let Some(published) = set
            .env
            .outputs
            .find_published(&this)
            .await
            .map_err(store_error)?
        {
            return self
                .commit_result(rt, &set, &call, &published, attempt, Ending::Completed)
                .await;
        }

        // 存下的与工具现在声明的都是 Safe 才重跑；现在的 unsafe 能否决存下的 safe，反过来不行。
        // 工具名已经没了（或移出了能力面）：没有工具可问，直接按中断处理。
        if let Some(tool) = set.get(&call.name).cloned()
            && stored.and(tool.replay()) == Replay::Safe
        {
            let next = attempt + 1;
            let checkpoint = Checkpoint::Execute {
                arguments,
                plan: Box::new(plan.clone()),
                replay: stored,
                attempt: next,
            };
            rt.commit(move |tx, task| {
                Box::pin(async move {
                    let mut live = live::load(tx, conversation).await?;
                    if let Some(slot) = live.slot_mut(task.id()) {
                        // 重跑从头报：清掉被打断的那次发布的东西。
                        slot.attempt = Some(next);
                        slot.clear_progress();
                    }
                    live::store(tx, conversation, &live).await?;
                    Ok(Some(running(encode(&checkpoint))))
                })
            })
            .await?;
            let resumed = ResumedCall {
                previous_attempt: Some(AttemptId::from_raw(attempt.to_string())),
                previous_state: ToolCallState::Started,
                attempts_so_far: attempt,
            };
            return self
                .run(rt, &set, &call, tool, plan, next, Some(resumed))
                .await;
        }

        let left = set
            .env
            .outputs
            .interrupted(&this, INTERRUPTED_TAIL_BYTES)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(task = %rt.task_id(), %error, "读不出中断前的部分输出");
                InterruptedOutput::default()
            });
        let message = interrupted_message(&call.name, &left);
        let body = ToolResultBody {
            status: ToolResultStatus::Uncertain,
            result: json!({
                "kind": "interrupted",
                "stdout_bytes": left.stdout_bytes,
                "stderr_bytes": left.stderr_bytes,
            }),
            error: Some(message.clone()),
            exit_code: None,
            artifacts: vec![],
            images: vec![],
            preview: Some(message),
        };
        let published = self.publish(&set.env, &this, body).await?;
        let ending = Ending::Failed(format!("工具 {} 被重启打断，可能已部分执行", call.name));
        self.commit_result(rt, &set, &call, &published, attempt, ending)
            .await
    }
}

fn parse_input(task: &RunningTask) -> Result<Input, TaskError> {
    serde_json::from_value(task.input().clone())
        .map_err(|e| TaskError::new(format!("tool task input is malformed: {e}")))
}

/// 一次被取消的调用的正文：取消说明，加上中断前已落盘的部分输出（若有）。
fn aborted_message(tool: &str, left: &InterruptedOutput) -> String {
    let mut out = format!("工具 {tool} 被取消");
    if left.is_empty() {
        return out;
    }
    out.push_str("\n取消前已落盘的部分输出（不是完成结果）：");
    for (label, bytes, tail) in [
        ("stdout", left.stdout_bytes, &left.stdout_tail),
        ("stderr", left.stderr_bytes, &left.stderr_tail),
    ] {
        if bytes == 0 {
            continue;
        }
        let shown = tail.len() as u64;
        if shown < bytes {
            out.push_str(&format!("\n[{label} 共 {bytes} 字节，末尾 {shown} 字节]\n"));
        } else {
            out.push_str(&format!("\n[{label} 共 {bytes} 字节]\n"));
        }
        out.push_str(tail);
    }
    out
}

#[async_trait]
impl TaskKind for ToolTask {
    fn name(&self) -> &str {
        TOOL_KIND
    }

    fn initial(&self, _input: &Value) -> Value {
        encode(&Checkpoint::Call)
    }

    async fn run_phase(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        let checkpoint: Checkpoint = serde_json::from_value(task.checkpoint.clone())
            .map_err(|e| TaskError::new(format!("tool checkpoint is malformed: {e}")))?;
        match checkpoint {
            Checkpoint::Call => self.call(task, rt).await,
            Checkpoint::Execute {
                arguments,
                plan,
                replay,
                attempt,
            } => {
                self.execute(task, rt, arguments, *plan, replay, attempt)
                    .await
            }
        }
    }

    /// 写一条 `aborted` 的错误结果，带上槽位里持久的部分输出（`.partial` 末尾），以 `aborted`
    /// 收场。
    async fn abort(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        let input = parse_input(task)?;
        let call = self.read_call(rt, &input).await?;
        let conversation = rt.conversation();
        let attempt = match serde_json::from_value::<Checkpoint>(task.checkpoint.clone()) {
            Ok(Checkpoint::Execute { attempt, .. }) => Some(attempt),
            _ => None,
        };
        let set = match self.tools(conversation).await {
            Ok(set) => set,
            Err(error) => {
                // 拿不到输出存储：没法落盘，但调用不能悬着。
                let name = call.name.clone();
                let call_id = call.id.clone();
                let text = format!("工具 {name} 被取消（{error}）");
                rt.commit(move |tx, task| {
                    Box::pin(async move {
                        let entry = entries::append_harness_error(
                            tx,
                            conversation,
                            &call_id,
                            &name,
                            "aborted",
                            &text,
                        )
                        .await?;
                        let mut live = live::load(tx, conversation).await?;
                        if let Some(slot) = live.slot_mut(task.id()) {
                            slot.finish();
                            slot.entry = Some(entry.id);
                        }
                        live::store(tx, conversation, &live).await?;
                        Ok(Some(aborted(Some(
                            json!({ "kind": "model", "entryId": entry.id }),
                        ))))
                    })
                })
                .await?;
                return Ok(());
            }
        };
        let this = attempt_ref(&set.env, conversation, rt.task_id(), attempt.unwrap_or(1));
        let left = if attempt.is_some() {
            set.env
                .outputs
                .interrupted(&this, INTERRUPTED_TAIL_BYTES)
                .await
                .unwrap_or_default()
        } else {
            InterruptedOutput::default()
        };
        let mut body = result_body(&Err(ToolError::Cancelled));
        let text = aborted_message(&call.name, &left);
        body.preview = Some(text);
        let published = self.publish(&set.env, &this, body).await?;
        self.commit_result(
            rt,
            &set,
            &call,
            &published,
            attempt.unwrap_or(1),
            Ending::Aborted,
        )
        .await
    }
}

// ---------------------------------------------------------------- 输出尾巴

/// 运行中输出的尾巴：stdout / stderr 按到达次序，至多 `cap` 字节，更早的丢掉并计数。
struct Tail {
    buffer: VecDeque<u8>,
    cap: usize,
    dropped: u64,
}

impl Tail {
    fn new(cap: usize) -> Self {
        Self {
            buffer: VecDeque::new(),
            cap,
            dropped: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend(chunk);
        while self.buffer.len() > self.cap {
            self.buffer.pop_front();
            self.dropped += 1;
        }
    }

    fn snapshot(&self) -> (String, u64) {
        let bytes: Vec<u8> = self.buffer.iter().copied().collect();
        (String::from_utf8_lossy(&bytes).into_owned(), self.dropped)
    }
}

/// 把写进 `.partial` 的每一块同时记进尾巴。
struct Tee<'a> {
    inner: &'a mut dyn OutputWriter,
    tail: Arc<Mutex<Tail>>,
}

#[async_trait]
impl OutputWriter for Tee<'_> {
    async fn write_stdout(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.inner.write_stdout(chunk).await?;
        self.tail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(chunk);
        Ok(())
    }

    async fn write_stderr(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.inner.write_stderr(chunk).await?;
        self.tail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(chunk);
        Ok(())
    }

    async fn write_image(
        &mut self,
        mime: &str,
        bytes: &[u8],
    ) -> Result<komo_kernel::types::refs::ImageRef, StoreError> {
        self.inner.write_image(mime, bytes).await
    }

    fn attempt(&self) -> &AttemptRef {
        self.inner.attempt()
    }

    fn bytes_written(&self) -> u64 {
        self.inner.bytes_written()
    }
}

/// 节流提交输出尾巴（§3.6）：只替换 `komo.live` 里这个槽位的 `output`，经 `Line` 走
/// `commit_progress`。槽位不在了（任务已收尾）就什么都不写。
async fn commit_output(
    rt: TaskRuntime,
    tail: Arc<Mutex<Tail>>,
    stop: CancellationToken,
    interval: std::time::Duration,
) {
    let conversation = rt.conversation();
    let mut written = (String::new(), 0u64);
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
        let snapshot = tail.lock().unwrap_or_else(|e| e.into_inner()).snapshot();
        if snapshot == written {
            continue;
        }
        let (text, dropped) = snapshot.clone();
        let committed = rt
            .commit(move |tx, task| {
                Box::pin(async move {
                    let mut live = live::load(tx, conversation).await?;
                    let Some(slot) = live.slot_mut(task.id()) else {
                        return Ok(None);
                    };
                    slot.output = Some(text);
                    slot.dropped_bytes = (dropped > 0).then_some(dropped);
                    live::store(tx, conversation, &live).await?;
                    Ok(None::<NextTaskState>)
                })
            })
            .await;
        match committed {
            Ok(()) => written = snapshot,
            Err(_) if rt.cancelled().is_cancelled() => return,
            Err(error) => {
                let _ = rt.report(&format!("committing tool progress failed: {error}"));
            }
        }
    }
}
