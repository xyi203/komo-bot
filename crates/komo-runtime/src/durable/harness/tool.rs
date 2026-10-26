//! `komo.tool`（`docs/storage-durable.md` §4.5、§5、§12.5；pi-durable spec §8.4，
//! `harness/tool.ts`）：一个模型调用。
//!
//! 输入 `{ "kind": "model", "assistant": 13, "callId": "call_3" }`——调用本身从 assistant entry
//! 读，任务记录保持很小。codemode 脚本里的子调用是 `{ "kind": "nested", "parent": 9, "key": "c1",
//! "callId": "call_3/c1", "name": "read", "arguments": {…} }`，由外层 codemode 的工具任务拥有，
//! 走同一条路径，槽位在 `nestedTools`，结果写进外层名下的 `komo.nested` document 而不是 transcript
//! （`nested.rs`；脚本这一侧在 `codemode.rs`）。checkpoint：
//!
//! ```json
//! { "phase": "call" }
//! { "phase": "execute", "arguments": {…}, "plan": {…ExecutionPlan…}, "replay": "safe", "attempt": 1 }
//! { "phase": "probe", …同 execute…, "child": { "pid": 4242, "pgid": 4242, "startedAt": 1761206400000 },
//!   "until": 1761206405000 }
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
//! probe:    槽位里记录的子进程（`komo.live.tools[].pid`，工具起进程时经节流进度那条路径写入）
//!           重启后还活着 → `execute` 不重判，进 `probe`：`sleep(until)` 后用 `ProcessProbe` 探一次，
//!           还在就把 `until` 推后一个间隔再来；结束了（含 pid 被复用、起来的时刻对不上）回到
//!           `execute` 那张表重新判。取消时 abort handler 照常写 `aborted` 结果（不终止那个进程，
//!           结果正文里说明它可能还在）。
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
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::durable::task::{NextTaskState, RunningTask};
use komo_kernel::durable::{
    ContentBlock, ConversationId, EntryId, Message, TaskId, ToolResultMessage,
};
use komo_kernel::projection::{ProjectionContext, ToolResultFacts, project_views, select};
use komo_kernel::traits::{ChildStarted, OutputWriter, StoreError, Tool};
use komo_kernel::types::ids::{AttemptId, RunId, ToolCallId};
use komo_kernel::types::plan::{ExecutionPlan, Operation, Replay};
use komo_kernel::types::refs::{
    AttemptRef, InterruptedOutput, PublishedOutput, ToolResultBody, ToolResultStatus,
};
use komo_kernel::types::status::ToolCallState;
use komo_kernel::types::tool::{CancelToken, InterjectSignal, ResumedCall, ToolContext, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::TOOL_KIND;
use super::codemode as script;
use super::entries::{self, TOOL_RESULT};
use super::hooks::{HarnessDeps, HookError, ToolEnv, ToolSet};
use super::live::{self, SlotStatus};
use super::nested::{self, NestedMeta, NestedResult};
use super::orchestration;
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
use crate::recovery::{ChildProcess, Liveness};
use crate::tools::codemode::NOT_IN_SCRIPTS;

/// 输入：模型发起的调用，或 codemode 脚本里的嵌套调用。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum Input {
    Model {
        assistant: EntryId,
        call_id: String,
    },
    /// 嵌套调用自带工具名与参数（没有 assistant entry 可读）；`call_id` 是 `<外层 callId>/<key>`。
    Nested {
        parent: TaskId,
        key: String,
        call_id: String,
        name: String,
        arguments: Value,
    },
}

impl Input {
    /// 模型发的调用所在的 assistant entry。编排（`task` / `monitor`）只认这一种：脚本里不放行
    /// 编排（见 `call` 里的嵌套检查），所以嵌套调用走到这里就是输入坏了。
    fn assistant(&self) -> Result<EntryId, TaskError> {
        match self {
            Input::Model { assistant, .. } => Ok(*assistant),
            Input::Nested { .. } => Err(TaskError::new(
                "orchestration needs a model tool call, not a nested one",
            )),
        }
    }
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
    /// 恢复时发现上一次执行起的子进程还活着：等它结束（§5 的 `probe` 阶段）。
    Probe {
        arguments: Value,
        plan: Box<ExecutionPlan>,
        replay: Replay,
        attempt: u32,
        child: ChildRef,
        /// 下一次探测的时刻（`Line` 时钟，毫秒）。
        until: i64,
    },
    /// 编排调用（`task` / `monitor` / `monitor_stop`，见 [`orchestration`]）的效果已经提交，
    /// 只差落结果（或接着起 / 停进程）。`step` 的形状归 `orchestration::Step`。
    Orchestrate {
        step: Value,
    },
}

/// 编排调用提交效果时把 checkpoint 推进到 `orchestrate`。
pub(super) fn orchestrating(step: Value) -> NextTaskState {
    running(encode(&Checkpoint::Orchestrate { step }))
}

/// 槽位里记下的子进程身份（`ToolSlot::{pid, pgid, started_at}`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildRef {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgid: Option<u32>,
    /// 起来的时刻（Unix 毫秒）；判断 pid 有没有被复用靠它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
}

impl ChildRef {
    fn process(&self, what: String) -> ChildProcess {
        ChildProcess {
            pid: self.pid,
            pgid: self.pgid,
            started_at: self.started_at.and_then(|ms| {
                time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
            }),
            what,
        }
    }
}

fn encode(checkpoint: &Checkpoint) -> Value {
    serde_json::to_value(checkpoint).expect("Checkpoint 可序列化")
}

/// 这次要的调用（模型发的，或脚本里的嵌套调用）。
#[derive(Debug, Clone)]
pub(super) struct Call {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: Value,
    /// 嵌套调用：指回外层任务。
    pub(super) nested: Option<NestedMeta>,
}

/// 一次结果怎么收尾任务。
#[derive(Debug, Clone)]
pub(super) enum Ending {
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

/// 模型视图与嵌套 bash 的脚本视图共用落盘事实与 kernel 投影。
pub(super) fn result_facts<'a>(
    tool: &'a str,
    published: &'a PublishedOutput,
) -> Result<ToolResultFacts<'a>, TaskError> {
    let body = published
        .body
        .as_deref()
        .ok_or_else(|| TaskError::new("the published output carries no body"))?;
    Ok(ToolResultFacts {
        tool,
        status: published.status,
        elapsed_ms: published.elapsed_ms,
        text: body.preview.as_deref(),
        output: &published.output,
        stdout: published.stdout.as_ref(),
        stderr: published.stderr.as_ref(),
        artifacts: &body.artifacts,
        images: &body.images,
    })
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
    let projected = project_views(&result_facts(tool, published)?, projection);
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
pub(super) fn attempt_ref(
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
    pub(super) deps: Arc<HarnessDeps>,
}

impl ToolTask {
    pub fn new(deps: Arc<HarnessDeps>) -> Self {
        Self { deps }
    }

    pub(super) fn deps(&self) -> &Arc<HarnessDeps> {
        &self.deps
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
        let (assistant, call_id) = match input {
            Input::Model { assistant, call_id } => (*assistant, call_id),
            Input::Nested {
                parent,
                key,
                call_id,
                name,
                arguments,
            } => {
                return Ok(Call {
                    id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    nested: Some(NestedMeta {
                        parent: *parent,
                        key: key.clone(),
                    }),
                });
            }
        };
        let entry = rt
            .entry(assistant)
            .await?
            .ok_or_else(|| TaskError::new(format!("entry {assistant} does not exist")))?;
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
                        } if id == call_id => Some(Call {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                            nested: None,
                        }),
                        _ => None,
                    })
                }
                _ => None,
            })
            .ok_or_else(|| TaskError::new(format!("entry {assistant} has no tool call {call_id}")))
    }

    #[allow(clippy::too_many_arguments)]
    async fn context(
        &self,
        env: &ToolEnv,
        rt: &TaskRuntime,
        attempt: u32,
        resumed: Option<ResumedCall>,
        cancel: CancelToken,
        nested: bool,
    ) -> Result<ToolContext, TaskError> {
        let conversation = rt.conversation();
        // 生成任务等待所有工具（包括脚本子调用）收尾后才结算 Run。恢复仍读这份持久状态；
        // 插话追加在 inputs 后面，不改变首个输入，也不能拿最新的 submission 代替它。
        let document = rt
            .find_document(&live::live_address(conversation))
            .await?
            .ok_or_else(|| TaskError::new("tool call has no live Run"))?;
        let live: live::LiveState = serde_json::from_value(document.value)
            .map_err(|error| TaskError::new(format!("invalid live Run: {error}")))?;
        let first = live
            .run
            .and_then(|run| run.inputs.first().copied())
            .ok_or_else(|| TaskError::new("tool call has no input submission"))?;
        let attempt_ref = attempt_ref(env, conversation, rt.task_id(), attempt);
        Ok(ToolContext {
            session: env.session.clone(),
            run: attempt_ref.run,
            input_run: Some(komo_kernel::durable::legacy_ids::run_id(first)),
            call: attempt_ref.call,
            attempt: attempt_ref.attempt,
            source: env.source.clone(),
            cwd: env.cwd.clone(),
            roots: env.roots.clone(),
            mounts: env.mounts.clone(),
            resumed,
            cancel,
            call_timeout: env.call_timeout,
            // 脚本等着嵌套调用的结果往下走：转后台只对模型直接发的调用有意义。
            detach_after: if nested { None } else { env.detach_after },
            // 嵌套调用等着脚本往下走，不看插话；顶层调用用 gateway 给的那面旗。
            interjected: if nested {
                InterjectSignal::new()
            } else {
                env.interjected.clone().unwrap_or_default()
            },
            conversation: env.conversation,
        })
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
            let message = if call.nested.is_some() {
                format!("脚本里不能调 {}", call.name)
            } else if names.is_empty() {
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
        // 脚本里调不了的（`write` / `codemode` 这些）：和能力面外的一样当作不认识。
        if call.nested.is_some() && NOT_IN_SCRIPTS.contains(&call.name.as_str()) {
            let message = format!("脚本里不能调 {}", call.name);
            return self.settle_unrun(rt, &set, &call, message).await;
        }
        let cancel = CancelToken::new();
        let ctx = self
            .context(&set.env, rt, 1, None, cancel, call.nested.is_some())
            .await?;
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
        // 嵌套调用逐个提交：文件改动与之后的命令分别记账。
        if call.nested.is_some() && plan.then_run.is_some() {
            let message = format!(
                "{} 的 then_run 不能在脚本里用；去掉 then_run，检查编辑结果后单独调用 tools.bash",
                call.name
            );
            return self.settle_unrun(rt, &set, &call, message).await;
        }
        // 文件工具仍执行自己的 prepare / 版本检查 / 保护路径；编排由模型直接调用。
        if call.nested.is_some()
            && !matches!(
                plan.operation,
                Operation::ReadFile
                    | Operation::WriteFile
                    | Operation::ShellCommand { .. }
                    | Operation::McpCall { .. }
            )
        {
            let message = format!(
                "{} 不是读写文件、bash 或 MCP；脚本里不支持此操作",
                call.name
            );
            return self.settle_unrun(rt, &set, &call, message).await;
        }
        // 编排调用没有"意图 → 执行"：效果在 `orchestration` 里一次提交完，崩溃后从 `orchestrate`
        // 接着做（同一次提交把 checkpoint 推进过去），不经过下面的重放判断。
        if orchestration::handles(&plan.operation) {
            return orchestration::call(self, rt, &set, &call, input.assistant()?, plan).await;
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
        if let Some(nested) = &call.nested {
            // 没执行过：给脚本的只是那句话，没有"原始输出"可指（摘要里不编造执行入口）。
            let mut result = NestedResult::from_published(&published, &attempt, false);
            result.output = None;
            return self
                .commit_nested(rt, nested, result, Ending::Completed)
                .await;
        }
        self.commit_result(rt, set, call, &published, 1, Ending::Completed)
            .await
    }

    pub(super) async fn publish(
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
        let nested = call.nested.is_some();
        let cancel = CancelToken::new();
        let ctx = self
            .context(env, rt, attempt, resumed, cancel.clone(), nested)
            .await?;
        let attempt_ref = attempt_ref(env, conversation, rt.task_id(), attempt);
        let mut writer = env.outputs.begin(&attempt_ref).await.map_err(store_error)?;
        let settings = self.deps.settings();

        let tail = Arc::new(Mutex::new(Tail::new(settings.tail_bytes)));
        let wake = Arc::new(Notify::new());
        let stop = CancellationToken::new();
        // bash 交给脚本的是落盘后的有限视图；其他嵌套工具仍保留完整 stdout。
        let captured: Option<Arc<Mutex<Vec<u8>>>> =
            (nested && call.name != "bash").then(Arc::default);
        let progress = tokio::spawn(commit_output(
            rt.clone(),
            tail.clone(),
            wake.clone(),
            stop.clone(),
            settings.output_interval,
        ));
        // kernel 的取消旗只有一个 flag：把引擎的取消信号接到它上面。
        let bridge = {
            let (cancel, signal, done) = (cancel.clone(), rt.cancelled(), stop.clone());
            tokio::spawn(async move {
                tokio::select! {
                    _ = signal.cancelled() => cancel.cancel(),
                    _ = done.cancelled() => {}
                }
            })
        };

        let started = std::time::Instant::now();
        let executed = {
            let mut sink = Tee {
                inner: writer.as_mut(),
                tail: tail.clone(),
                wake: wake.clone(),
                captured: captured.clone(),
            };
            let execution = async {
                // codemode：脚本在 worker 里跑，里面的每个调用是名下的嵌套任务（`codemode.rs`）。
                if let Operation::Codemode { code } = &plan.operation {
                    return script::execute(self, rt, set, call, code, &ctx, &mut sink).await;
                }
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
        // 或重启后的 `execute` 阶段接管它。工具自己看到同一个取消旗、抢在 `race` 的轮询之前
        // 以 `Cancelled` 返回，也是取消赢了。
        let executed = match executed {
            Some(Ok(Err(ToolError::Cancelled))) if cancel.is_cancelled() => None,
            executed => executed,
        };
        let Some(executed) = executed else {
            return Err(TaskError::new("the tool call was cancelled"));
        };
        let outcome = match executed {
            Err(_elapsed) => Err(timeout_error(&plan, env.call_timeout)),
            Ok(result) => result,
        };
        let mut body = result_body(&outcome);
        // 嵌套调用的宿主超时 / 取消会锁住脚本。把状态一起落盘，恢复重建也必须得到 uncertain。
        if nested
            && matches!(
                &outcome,
                Err(ToolError::Cancelled | ToolError::Timeout { .. })
            )
        {
            body.status = ToolResultStatus::Uncertain;
        }
        if let Ok(output) = &outcome {
            let _ = store_images(writer.as_mut(), &mut body, &output.images).await;
        }
        // 完整输出先落盘（同步 → 改名 → 同步目录），之后才提交引用它的 entry。
        let mut published = env
            .outputs
            .publish(writer, body)
            .await
            .map_err(store_error)?;
        if let Some(nested) = &call.nested {
            let result = if call.name == "bash" {
                // 用 publish 返回的持久化耗时，不能用执行结束后的另一次本地计时覆盖。
                NestedResult::from_bash(&published, &attempt_ref, &env.projection, false)?
            } else {
                let bytes = captured
                    .map(|bytes| {
                        std::mem::take(&mut *bytes.lock().unwrap_or_else(|e| e.into_inner()))
                    })
                    .unwrap_or_default();
                NestedResult::from_outcome(&outcome, &bytes, &published, &attempt_ref)
            };
            return self
                .commit_nested(rt, nested, result, Ending::Completed)
                .await;
        }
        published.elapsed_ms = elapsed_ms;
        self.commit_result(rt, set, call, &published, attempt, Ending::Completed)
            .await
    }

    /// 提交结果 entry、清槽位、让任务终结。
    pub(super) async fn commit_result(
        &self,
        rt: &TaskRuntime,
        set: &ToolSet,
        call: &Call,
        published: &PublishedOutput,
        attempt: u32,
        ending: Ending,
    ) -> Result<(), TaskError> {
        // 嵌套调用：结果写进外层名下的 document，不进 transcript。
        if let Some(nested) = &call.nested {
            let this = attempt_ref(&set.env, rt.conversation(), rt.task_id(), attempt);
            let aborted = matches!(ending, Ending::Aborted);
            let result = if call.name == "bash" {
                NestedResult::from_bash(published, &this, &set.env.projection, aborted)?
            } else {
                NestedResult::from_published(published, &this, aborted)
            };
            return self.commit_nested(rt, nested, result, ending).await;
        }
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
                live.remove_nested(task.id());
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
        self.resolve(task, rt, arguments, plan, stored, attempt, true)
            .await
    }

    /// `execute` 与 `probe`（子进程结束之后）共用的那张表。`check_child`：还没探过子进程，
    /// 槽位里记着一个就转进 `probe`。
    #[allow(clippy::too_many_arguments)]
    async fn resolve(
        &self,
        task: &RunningTask,
        rt: &TaskRuntime,
        arguments: Value,
        plan: ExecutionPlan,
        stored: Replay,
        attempt: u32,
        check_child: bool,
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
            && (call.nested.is_none() || nested::reusable(&published, &call.name))
        {
            return self
                .commit_result(rt, &set, &call, &published, attempt, Ending::Completed)
                .await;
        }

        // 上一次执行起的子进程还活着吗？没探过、槽位里记着一个：进 `probe`，由它按间隔探。
        if check_child && let Some(child) = recorded_child(rt, attempt).await? {
            let checkpoint = Checkpoint::Probe {
                arguments,
                plan: Box::new(plan),
                replay: stored,
                attempt,
                child,
                until: rt.now()?,
            };
            rt.commit_next(running(encode(&checkpoint))).await?;
            return Ok(());
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

    // ------------------------------------------------------------------ probe（恢复）

    /// `probe` 阶段：睡到 `until`，探一次。还在就把 `until` 推后一个间隔；结束了回到 `execute`
    /// 那张表。取消时 `sleep_until` 拒绝，由 abort handler 收尾。
    #[allow(clippy::too_many_arguments)]
    async fn probe(
        &self,
        task: &RunningTask,
        rt: &TaskRuntime,
        arguments: Value,
        plan: ExecutionPlan,
        replay: Replay,
        attempt: u32,
        child: ChildRef,
        until: i64,
    ) -> Result<(), TaskError> {
        rt.sleep_until(until).await?;
        let process = child.process(format!("tool task {}", rt.task_id()));
        match self.deps.probe.probe(&process) {
            Liveness::Gone => {
                self.resolve(task, rt, arguments, plan, replay, attempt, false)
                    .await
            }
            // 查不出来按"还在"处理：重复执行比多等一会儿危险（§8.7）。
            Liveness::Alive | Liveness::Unknown => {
                let interval = self.deps.settings().probe_interval;
                let next = rt.now()? + interval.as_millis() as i64;
                let checkpoint = Checkpoint::Probe {
                    arguments,
                    plan: Box::new(plan),
                    replay,
                    attempt,
                    child,
                    until: next,
                };
                rt.commit_next(running(encode(&checkpoint))).await?;
                Ok(())
            }
        }
    }
}

/// 槽位里记录的、属于这次尝试的子进程。
async fn recorded_child(rt: &TaskRuntime, attempt: u32) -> Result<Option<ChildRef>, TaskError> {
    let conversation = rt.conversation();
    let found = Arc::new(Mutex::new(None));
    let out = found.clone();
    // 只读：不返回新状态，也不写任何东西。
    rt.commit(move |tx, task| {
        Box::pin(async move {
            let live = live::load(tx, conversation).await?;
            let child = live.slot(task.id()).and_then(|slot| {
                if slot.attempt != Some(attempt) {
                    return None;
                }
                Some(ChildRef {
                    pid: slot.pid?,
                    pgid: slot.pgid,
                    started_at: slot.started_at,
                })
            });
            *out.lock().unwrap_or_else(|e| e.into_inner()) = child;
            Ok(None::<NextTaskState>)
        })
    })
    .await?;
    let child = *found.lock().unwrap_or_else(|e| e.into_inner());
    Ok(child)
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
            Checkpoint::Probe {
                arguments,
                plan,
                replay,
                attempt,
                child,
                until,
            } => {
                self.probe(task, rt, arguments, *plan, replay, attempt, child, until)
                    .await
            }
            Checkpoint::Orchestrate { step } => {
                let input = parse_input(task)?;
                let call = self.read_call(rt, &input).await?;
                let set = self.tools(rt.conversation()).await?;
                orchestration::resume(self, rt, &set, &call, input.assistant()?, step).await
            }
        }
    }

    /// 写一条 `aborted` 的错误结果，带上槽位里持久的部分输出（`.partial` 末尾），以 `aborted`
    /// 收场。
    async fn abort(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        let input = parse_input(task)?;
        let call = self.read_call(rt, &input).await?;
        let conversation = rt.conversation();
        let (attempt, probing) = match serde_json::from_value::<Checkpoint>(task.checkpoint.clone())
        {
            Ok(Checkpoint::Execute { attempt, .. }) => (Some(attempt), None),
            Ok(Checkpoint::Probe { attempt, child, .. }) => (Some(attempt), Some(child)),
            _ => (None, None),
        };
        let set = match self.tools(conversation).await {
            Ok(set) => set,
            Err(error) => {
                // 拿不到输出存储：没法落盘，但调用不能悬着。
                if let Some(nested) = &call.nested {
                    let result = NestedResult {
                        value: None,
                        error: Some(format!("工具 {} 被取消（{error}）", call.name)),
                        uncertain: true,
                        images: vec![],
                        output: None,
                    };
                    return self
                        .commit_nested(rt, nested, result, Ending::Aborted)
                        .await;
                }
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
        // 编排调用的效果已经提交：结果照实写，不当成"没跑过"。
        if let Ok(Checkpoint::Orchestrate { step }) =
            serde_json::from_value::<Checkpoint>(task.checkpoint.clone())
        {
            return orchestration::aborted(self, rt, &set, &call, input.assistant()?, step).await;
        }
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
        let mut text = aborted_message(&call.name, &left);
        // 在 probe 阶段被取消：重启前起的子进程我们不终止它，别让模型以为它没了。
        if let Some(child) = probing {
            let process = child.process(format!("tool task {}", rt.task_id()));
            if self.deps.probe.probe(&process) != Liveness::Gone {
                text.push_str(&format!(
                    "\n重启前这次调用起的子进程（pid {}）取消时还在运行，komo 没有终止它。",
                    child.pid
                ));
            }
        }
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
    /// 工具起的子进程（`OutputWriter::child_started`）。
    child: Option<ChildStarted>,
}

/// 某一刻要写进槽位的进度：输出尾巴、丢掉的字节数、子进程身份。
#[derive(Clone, PartialEq)]
struct Snapshot {
    text: String,
    dropped: u64,
    child: Option<ChildStarted>,
}

impl Tail {
    fn new(cap: usize) -> Self {
        Self {
            buffer: VecDeque::new(),
            cap,
            dropped: 0,
            child: None,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend(chunk);
        while self.buffer.len() > self.cap {
            self.buffer.pop_front();
            self.dropped += 1;
        }
    }

    fn snapshot(&self) -> Snapshot {
        let bytes: Vec<u8> = self.buffer.iter().copied().collect();
        Snapshot {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            dropped: self.dropped,
            child: self.child,
        }
    }
}

/// 把写进 `.partial` 的每一块同时记进尾巴。
struct Tee<'a> {
    inner: &'a mut dyn OutputWriter,
    tail: Arc<Mutex<Tail>>,
    /// 起了子进程：叫醒进度提交，不等下一个节流间隔（重启前越早写进槽位越好）。
    wake: Arc<Notify>,
    /// 需要完整正文的嵌套工具的 stdout；bash 使用落盘后的有限视图，不收集这一份。
    captured: Option<Arc<Mutex<Vec<u8>>>>,
}

#[async_trait]
impl OutputWriter for Tee<'_> {
    async fn write_stdout(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.inner.write_stdout(chunk).await?;
        if let Some(captured) = &self.captured {
            captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(chunk);
        }
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

    fn child_started(&mut self, child: ChildStarted) {
        self.inner.child_started(child);
        self.tail.lock().unwrap_or_else(|e| e.into_inner()).child = Some(child);
        self.wake.notify_one();
    }
}

/// 节流提交运行中的进度（§3.6）：输出尾巴与子进程身份，只替换 `komo.live` 里这个槽位的对应
/// 字段，经 `Line` 走 `commit_progress`。槽位不在了（任务已收尾）就什么都不写。
async fn commit_output(
    rt: TaskRuntime,
    tail: Arc<Mutex<Tail>>,
    wake: Arc<Notify>,
    stop: CancellationToken,
    interval: Duration,
) {
    let conversation = rt.conversation();
    let mut written = Snapshot {
        text: String::new(),
        dropped: 0,
        child: None,
    };
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            _ = wake.notified() => {}
            _ = tokio::time::sleep(interval) => {}
        }
        let snapshot = tail.lock().unwrap_or_else(|e| e.into_inner()).snapshot();
        if snapshot == written {
            continue;
        }
        let pending = snapshot.clone();
        let committed = rt
            .commit(move |tx, task| {
                Box::pin(async move {
                    tx.mark_progress();
                    let mut live = live::load(tx, conversation).await?;
                    let Some(slot) = live.slot_mut(task.id()) else {
                        return Ok(None);
                    };
                    slot.output = Some(pending.text);
                    slot.dropped_bytes = (pending.dropped > 0).then_some(pending.dropped);
                    if let Some(child) = pending.child {
                        slot.pid = Some(child.pid);
                        slot.pgid = Some(child.pgid);
                        slot.started_at = Some(child.started_at_ms);
                    }
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
