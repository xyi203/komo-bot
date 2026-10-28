//! 编排工具 `task` / `monitor` / `monitor_stop` 在新 harness 上的执行路径（D4c；
//! `docs/storage-durable.md` §4.6、§12.4；pi-durable spec §5.4–§5.5、§7.3 的 subagent 两节）。
//!
//! 旧路径里这几个工具的 `prepare` 之后由 executor 的"编排操作"分流到 gateway 的
//! `TaskSpawner` / `Monitors`；新路径里它们和别的工具一样是 `komo.tool` 任务，只是 `call` 阶段
//! 在 `Tool::prepare` 之后**不进入通用的 intent → execute**，而是转到这里（`tool.rs` 里就一个
//! 分支）：`Tool::execute` 对这几个工具恒为错误，执行本来就不在工具里。
//!
//! # `task`：owned、background 的子 conversation
//!
//! ```text
//! 工具任务（父 conversation 里的 komo.tool）
//!   └─ 一次提交：
//!        ├─ 锚任务 A（komo.background）   conversation 所有、background，立刻 completed
//!        │     └─ 子 conversation C        所有者是 A；context:"fork" 时 fork(at = 父此刻的最新 entry)
//!        │           ├─ komo.task document  父、标题、model / effort、结果契约
//!        │           ├─ fork 切口处没有结果的调用补一条结果 entry（只在 C 里，不进父）
//!        │           └─ 第一条输入 placed + 起 run（komo.generation）
//!        └─ checkpoint → `orchestrate { reply: "已派出 #C：标题" }`
//!   之后另起一次提交落结果 entry，工具任务 completed
//! ```
//!
//! - **子会话不挡父的 idle，父被取消也不连带它**：锚任务是 background，普通遍历（`wait_for_idle`、
//!   `abort_conversation(.., background: false)`、所有者级联）都停在它前面——与旧规则一致
//!   （`docs/background-tasks.md` 里任务是独立的会话，取消 home 的 Run 不动任务）。A 在子会话
//!   还有活的时候停在 `completing`，子会话安静了才 terminal（spec §5.5）；之后在 C 里继续
//!   （`task_id` 接着做）是 C 自己的普通工作。
//! - **显式取消子任务**走引擎：`Harness::abort_conversation(engine, C)`——直接作用在 C 里，不看
//!   所有者；C 的 run 以 `aborted` 收尾，照常交回一条"没有完成"。
//! - **句柄**是子 conversation 的 ID（十进制，`#12`）：`task_id` 直接解析成 conversation，再核对
//!   它的 `komo.task.parent` 就是调用方，不需要登记表。旧实现的四位 UUID 短号属于 gateway 的
//!   "任务会话"，D5 若要沿用，经 [`OrchestrationHooks`] 之外的映射层处理。
//! - **`task_id` 接着做** = 往 C 里 submit 新输入（不带契约的追问是 steer，在 C 正忙时并进当前
//!   run；带契约的 followUp 排在后面）。`model` / `effort` 写进 `komo.task`，下一次生成的 `prepare`
//!   由 gateway 的 `AgentHooks::setup` 按名字解析。
//! - **任务会话里不能再派任务**：调用方 conversation 自己有 `komo.task` 就拒绝。
//! - **查进度**只读 C 的 `komo.live`、entry 与 `komo.task`，不提交任何东西。
//!
//! 交回结果在 [`report`]：子 run 收尾的**同一次提交**里往父 conversation submit
//! `task-result:{C}:{首个输入}`。
//!
//! # `monitor`
//!
//! 进程、stdout 泵、合批与渠道投递属于 gateway（D5）；这里只有它们需要的**持久契约**：
//! [`monitor`] 里的登记 document、工具任务的"登记 → 起进程 → 落结果"三明治、结束 / 中断消息的
//! 原子幂等交回，以及重启后的对账（`resume_after_restart`）。边界写在 `monitor.rs` 的模块文档里。

pub mod monitor;
pub mod report;
mod task;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::durable::task::{NextTaskState, RunningTask};
use komo_kernel::durable::{ConversationId, DocumentAddress, DocumentScope, EntryId, Storage};
use komo_kernel::types::delegate::DelegateContract;
use komo_kernel::types::model::Effort;
use komo_kernel::types::plan::{ExecutionPlan, Operation};
use komo_kernel::types::refs::{ToolResultBody, ToolResultStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use komo_kernel::traits::MonitorError;
pub use monitor::{LaunchMode, MonitorLaunch, MonitorRecord, MonitorState};

use super::hooks::ToolSet;
use super::tool::{Call, Ending, ToolTask, attempt_ref};
use crate::durable::error::{DurableError, TaskError};
use crate::durable::kind::TaskKind;
use crate::durable::line::TxFuture;
use crate::durable::runtime::TaskRuntime;
use crate::durable::tx::Tx;

/// 锚任务的种类名：background、conversation 所有，持有子 conversation（spec §7.3 的 subagent）。
pub const ANCHOR_KIND: &str = "komo.background";
/// 标记"这是一个后台任务的 conversation"的 document（子 conversation 的）。
pub const TASK_DOC_KIND: &str = "komo.task";
pub const TASK_DOC_VERSION: u32 = 1;
/// 交回到父 conversation 的输入的 entry kind。
pub const TASK_RESULT_ENTRY: &str = "task.result";

/// fork 任务第一条输入的开头（`docs/background-tasks.md` §2b）：紧跟在父的对话与补的结果之后。
/// 放在输入里而不是系统提示里——系统提示是父那份，改一个字缓存就不中。
pub const FORK_NOTICE: &str = "（以上是派你的那个会话到此刻为止的对话。你是从那里分出来的后台任务：\
只做下面这件事。派你的会话同时还在跑，同一个工作目录里的文件可能正在变——不要接着做它没做完的事，\
不要改它正在改的东西，也不要再派任务。中途没有人能回答你：几种做法代价不同时自己选更保守的那种接着做，\
在最后的回复里写明；只在做完、或者真的做不下去（缺权限、缺信息又查不到、任务的前提不成立）时才停，\
你最后的回复会作为那次 task 调用的结果交回给它。）";

/// 派出这个 fork 的那次调用在 fork 里的结果。
pub(crate) const FORK_CALL_RESULT: &str = "这次调用派出的就是你（fork）。派你的会话在继续它自己的事，你最后的回复会作为这次调用的结果交回给它。";

/// 同一轮里排在派出那次之后、还没执行的调用在 fork 里的结果。
pub(crate) const FORK_LATER_CALL_RESULT: &str =
    "派你的会话在派出你之后才执行这次调用，你看不到它的结果。";

// ---------------------------------------------------------------- 钩子

/// 编排需要 agent / gateway 侧才有的东西。runtime 不依赖 komo-agent，D5 在 gateway 里实现；
/// 没装配的默认值（[`NoOrchestration`]）让 `task` 照常工作（校验放过、契约用一份朴素的说明），
/// `monitor` 干净地失败。
#[async_trait]
pub trait OrchestrationHooks: Send + Sync {
    /// 只给 `effort`（或给了 `effort` 又沿用上一次的模型）时，选定的模型只有 gateway 知道
    /// （Agent 的默认，或那个任务上一条 Run 的）：按它的档位校验。`conversation` 是要跑的那个
    /// conversation（新派是调用方，接着做是子 conversation），`model` 是已知的 alias。
    /// `Err` 的文字给模型看（前面会加"effort 立不住："）。
    async fn check_effort(
        &self,
        _conversation: ConversationId,
        _model: Option<&str>,
        _effort: &Effort,
    ) -> Result<(), String> {
        Ok(())
    }

    /// `task` 带契约时补在任务后面的那一段。默认与 komo-agent 的 `task_contract_block` 同一套
    /// 说法；gateway 应该覆盖成直接调它，免得两处文字漂移。
    fn contract_block(&self, contract: &DelegateContract) -> String {
        format!(
            "（结果契约）最后一条回复只放结果，不调工具、不寒暄：\n一个满足下面 schema 的 JSON 对象\
             （交回之前用同一份校验复验，复验结论随结果一起交回）：\n{}",
            serde_json::to_string_pretty(&contract.schema).unwrap_or_else(|_| "{}".into())
        )
    }

    /// 起（或认回、重挂）一个监听的进程。语义见 [`LaunchMode`]。成功后事件由 gateway 经
    /// `Harness::submit` 交回，结束时调 `Harness::finish_monitor`。
    async fn start_monitor(&self, _launch: &MonitorLaunch) -> Result<(), MonitorError> {
        Err(MonitorError::Failed(
            "这台 Gateway 没有接监听（OrchestrationHooks 没有装配监听）".into(),
        ))
    }

    /// 终止监听的进程组。幂等：已经没有的也算成功。
    async fn stop_monitor(&self, _id: u64) -> Result<(), MonitorError> {
        Ok(())
    }

    /// 接手 `bash` 转后台的活子进程的一方（`docs/monitor.md` §8；`BashTool` 的 `DetachSlot` 里装它）。
    /// 接手之后进程和监听同一套：gateway 用 [`Harness::register_monitor`](super::Harness::register_monitor)
    /// 登记、结束时 `finish_monitor`。默认没有——`bash` 不转后台。
    fn detacher(&self) -> Option<Arc<dyn crate::tools::detach::Detacher>> {
        None
    }

    /// Gateway 起来、引擎开始调度之后、调 [`Harness::reconcile_monitors`](super::Harness::reconcile_monitors)
    /// **之前**调一次：上一代留下的子进程（还活着的监听进程组）要在任何新进程起来之前核对并终止。
    /// 默认什么都不做。
    async fn boot(&self) {}

    /// Gateway 停机、引擎关闭之前调一次：终止自己起的全部进程组，**不**登记结束（登记留着，下一代启动时
    /// 由 `reconcile_monitors` 判重挂还是中断）。默认什么都不做。
    async fn shutdown(&self) {}
}

/// 什么都没装配。
pub struct NoOrchestration;

#[async_trait]
impl OrchestrationHooks for NoOrchestration {}

// ---------------------------------------------------------------- komo.task document

pub fn task_doc_address(conversation: ConversationId) -> DocumentAddress {
    DocumentAddress {
        kind: TASK_DOC_KIND.to_owned(),
        scope: DocumentScope::Conversation { conversation },
        key: None,
    }
}

/// 一次 run 的收尾记录（查进度读它）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastRun {
    /// `completed` / `failed` / `cancelled`。
    pub state: String,
    pub ended_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// 子 conversation 的 `komo.task`：它是谁派的、叫什么、用哪个模型、哪些输入带结果契约。
///
/// gateway 的 `AgentHooks::setup` 读它：`model` / `effort` 是 alias 名字，按当前配置解析；
/// `fork` 的任务沿用 `parent` 的模型与 effort 并用 `parent` 当提示缓存的路由键；有这份
/// document 的 conversation 能力面里不该有 `monitor`、fresh 任务也不该有 `task`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDoc {
    pub parent: ConversationId,
    /// 持有这个 conversation 的锚任务。
    pub anchor: komo_kernel::durable::TaskId,
    pub title: String,
    #[serde(default)]
    pub fork: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// 带结果契约的输入：key 是 submission ID。run 收尾时按它的首个输入取出复验。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub contracts: BTreeMap<String, DelegateContract>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<LastRun>,
}

/// 提交里读 `komo.task`（document 读，不是表读：排在表写之后也行）。
pub async fn load_task_doc(
    tx: &mut Tx,
    conversation: ConversationId,
) -> Result<Option<TaskDoc>, DurableError> {
    match tx.document(&task_doc_address(conversation)).await? {
        None => Ok(None),
        Some((_, value)) => serde_json::from_value(value)
            .map(Some)
            .map_err(|e| DurableError::rejected(format!("komo.task is malformed: {e}"))),
    }
}

pub async fn store_task_doc(
    tx: &mut Tx,
    conversation: ConversationId,
    doc: &TaskDoc,
) -> Result<(), DurableError> {
    let value = serde_json::to_value(doc).expect("TaskDoc 可序列化");
    tx.put_document(task_doc_address(conversation), TASK_DOC_VERSION, value)
        .await
}

/// 已提交的 `komo.task`（读不经过线）。gateway 的钩子、测试的替身用。
pub async fn task_doc(
    storage: &dyn Storage,
    conversation: ConversationId,
) -> Result<Option<TaskDoc>, DurableError> {
    match storage
        .find_document(&task_doc_address(conversation))
        .await?
    {
        None => Ok(None),
        Some(record) => serde_json::from_value(record.value)
            .map(Some)
            .map_err(|e| DurableError::rejected(format!("komo.task is malformed: {e}"))),
    }
}

// ---------------------------------------------------------------- 锚任务

/// 锚任务：立刻 completed；子 conversation 有活的普通工作时引擎让它停在 `completing`。
pub struct BackgroundTask;

#[async_trait]
impl TaskKind for BackgroundTask {
    fn name(&self) -> &str {
        ANCHOR_KIND
    }

    fn initial(&self, _input: &Value) -> Value {
        json!({ "phase": "anchor" })
    }

    async fn run_phase(&self, _task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        rt.commit_next(super::outcome::completed(json!({}))).await?;
        Ok(())
    }

    async fn abort(&self, _task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        rt.commit_next(super::outcome::aborted(None)).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------- 工具任务里的分派

/// 一次编排调用的结论：给模型的一句话。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    pub text: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub result: Value,
}

impl Reply {
    pub fn ok(text: impl Into<String>, result: Value) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            result,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            result: json!({ "error": text }),
            text,
            is_error: true,
        }
    }
}

/// 副作用已经提交之后工具任务的 checkpoint（`orchestrate` 阶段）里记的步骤。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "camelCase")]
pub enum Step {
    /// 副作用都提交了，只差把结果落盘、提交结果 entry。
    Reply { reply: Reply },
    /// 监听的登记已提交（`starting`），要起 / 认回进程（见 [`monitor`]）。
    MonitorStart,
    /// 监听已登记为停掉，要让进程停下来再回话。
    MonitorStop,
}

impl Step {
    pub(super) fn state(&self) -> NextTaskState {
        super::tool::orchestrating(serde_json::to_value(self).expect("Step 可序列化"))
    }
}

/// 这份计划是不是编排调用。
pub(super) fn handles(operation: &Operation) -> bool {
    matches!(
        operation,
        Operation::Task { .. }
            | Operation::TaskStatus { .. }
            | Operation::Monitor { .. }
            | Operation::MonitorStop { .. }
    )
}

/// 一次分派手里的东西。
pub(super) struct Cx<'a> {
    pub tool: &'a ToolTask,
    pub rt: &'a TaskRuntime,
    pub set: &'a ToolSet,
    pub call: &'a Call,
    /// 发起这次调用的 assistant entry。
    pub assistant: EntryId,
}

impl Cx<'_> {
    pub fn conversation(&self) -> ConversationId {
        self.rt.conversation()
    }

    pub fn hooks(&self) -> &dyn OrchestrationHooks {
        &*self.tool.deps().orchestration
    }

    /// 落盘结果、提交结果 entry、让任务终结。
    pub async fn finish(&self, reply: &Reply, ending: Ending) -> Result<(), TaskError> {
        let attempt = attempt_ref(&self.set.env, self.conversation(), self.rt.task_id(), 1);
        let body = ToolResultBody {
            status: if reply.is_error {
                ToolResultStatus::Failed
            } else {
                ToolResultStatus::Completed
            },
            result: reply.result.clone(),
            error: reply.is_error.then(|| reply.text.clone()),
            exit_code: None,
            artifacts: vec![],
            images: vec![],
            preview: Some(reply.text.clone()),
        };
        let published = self.tool.publish(&self.set.env, &attempt, body).await?;
        self.tool
            .commit_result(self.rt, self.set, self.call, &published, 1, ending)
            .await
    }

    /// 钩子可能阻塞（起进程、等 gateway）：取消（abort 标记、引擎关闭）赢了就放弃等待，重启后
    /// 从 checkpoint 接着做。
    pub async fn guarded<T>(
        &self,
        work: impl std::future::Future<Output = T>,
    ) -> Result<T, TaskError> {
        let cancelled = self.rt.cancelled();
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => Err(TaskError::new("the orchestration call was cancelled")),
            value = work => Ok(value),
        }
    }

    pub async fn reply(&self, reply: &Reply) -> Result<(), TaskError> {
        self.finish(reply, Ending::Completed).await
    }

    /// 效果三明治里"效果"那一半：提交 `change`（建会话、放输入、登记……全在这一次提交里，
    /// 返回给模型的结论），同一次提交把 checkpoint 推进到 `orchestrate { reply }`，然后当场
    /// 落结果。两次提交之间崩溃，恢复从 `orchestrate` 接着落结果，**不会再产生一遍效果**。
    pub async fn commit_effect<F>(&self, change: F) -> Result<(), TaskError>
    where
        F: for<'a> FnOnce(&'a mut Tx, &'a RunningTask) -> TxFuture<'a, Reply> + Send + 'static,
    {
        let slot = Arc::new(Mutex::new(None));
        let out = slot.clone();
        self.rt
            .commit(move |tx, task| {
                Box::pin(async move {
                    let reply = change(tx, task).await?;
                    *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(reply.clone());
                    Ok(Some(Step::Reply { reply }.state()))
                })
            })
            .await?;
        let reply = slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| TaskError::new("the orchestration commit produced no reply"))?;
        self.reply(&reply).await
    }
}

/// `tool.rs` 的 `call` 阶段在 `Tool::prepare` 之后转进来。
pub(super) async fn call(
    tool: &ToolTask,
    rt: &TaskRuntime,
    set: &ToolSet,
    call: &Call,
    assistant: EntryId,
    plan: ExecutionPlan,
) -> Result<(), TaskError> {
    let cx = Cx {
        tool,
        rt,
        set,
        call,
        assistant,
    };
    match plan.operation {
        Operation::Task {
            prompt,
            title,
            task_id,
            model,
            effort,
            contract,
            fork,
        } => {
            let request = task::Request {
                prompt,
                title,
                task_id,
                model,
                effort,
                contract,
                fork,
            };
            task::run(&cx, request).await
        }
        Operation::TaskStatus { task_id } => task::status(&cx, &task_id).await,
        Operation::Monitor {
            command,
            title,
            timeout_minutes,
            resume_after_restart,
        } => {
            let cwd = plan.cwd.unwrap_or_else(|| PathBuf::from("."));
            monitor::start(
                &cx,
                monitor::Request {
                    command,
                    title,
                    cwd,
                    timeout_minutes,
                    resume_after_restart,
                },
            )
            .await
        }
        Operation::MonitorStop { monitor_id } => monitor::stop(&cx, &monitor_id).await,
        other => Err(TaskError::new(format!(
            "operation {other:?} is not an orchestration call"
        ))),
    }
}

/// `orchestrate` 阶段（只由恢复进入）：副作用已经提交，接着做完。
pub(super) async fn resume(
    tool: &ToolTask,
    rt: &TaskRuntime,
    set: &ToolSet,
    call: &Call,
    assistant: EntryId,
    step: Value,
) -> Result<(), TaskError> {
    let step: Step = serde_json::from_value(step)
        .map_err(|e| TaskError::new(format!("orchestrate step is malformed: {e}")))?;
    let cx = Cx {
        tool,
        rt,
        set,
        call,
        assistant,
    };
    match step {
        Step::Reply { reply } => cx.reply(&reply).await,
        Step::MonitorStart => monitor::resume_start(&cx).await,
        Step::MonitorStop => monitor::resume_stop(&cx).await,
    }
}

/// 在 `orchestrate` 阶段被取消：副作用已经生效，结果照实写（带上"已取消"）。
pub(super) async fn aborted(
    tool: &ToolTask,
    rt: &TaskRuntime,
    set: &ToolSet,
    call: &Call,
    assistant: EntryId,
    step: Value,
) -> Result<(), TaskError> {
    let step: Step = serde_json::from_value(step)
        .map_err(|e| TaskError::new(format!("orchestrate step is malformed: {e}")))?;
    let cx = Cx {
        tool,
        rt,
        set,
        call,
        assistant,
    };
    let reply = match step {
        Step::Reply { mut reply } => {
            reply.text = format!("{}\n（这次调用被取消时它已经生效。）", reply.text);
            reply
        }
        Step::MonitorStart => monitor::abort_start(&cx).await?,
        Step::MonitorStop => monitor::abort_stop(&cx).await?,
    };
    cx.finish(&reply, Ending::Aborted).await
}
