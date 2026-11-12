//! 主会话消息的分发（`docs/tasks.md` §4.11、§4.12、§4.14 F2）：主会话（`kind = main`）收到的普通
//! 文本不再起主会话的模型轮次，由程序分发；干活和回话都在任务会话里，结果由程序原文投回渠道。
//!
//! 分发本身是引擎里的一种任务（[`DispatchTask`]，种类名 [`DISPATCH_KIND`]），不是提交之外的一段
//! 程序，所以恢复照 `docs/storage-durable.md` §5 的表走：
//!
//! ```text
//! 受理      一次提交：主线消息的展示 write（`main.user`，请求键 = 平台消息键）+ 一个分发任务
//!   │       （请求键重投 → 原消息、不再建任务，所以一条消息至多一个分发任务）
//!   ▼
//! judge     第一个检查点。拼输入 → `router::judge` → 把去向写进检查点（[`Decision`]）。
//!   │       没有副作用，重启后重做无妨；同一主会话里更早的分发任务没结束时先等它（次序）。
//!   ▼
//! execute   第二个检查点。**一次提交**里落效果并把分发任务标成完成：
//!             new       建任务会话（`Harness::spawn_task_in_tx`，与 `task` 工具同一段）
//!             continue  把原文提交给目标任务（请求键 `dispatch:{主线消息的 entry}`）；已结束就接着做
//!             stop      `DurableCore::stop_task`（标已停、撤输入、停监听、取消执行），再落一句"好，先停了。"
//!             list      在主线上落一条清单消息（不经模型）
//! ```
//!
//! 位置的路由归属（[`owner`]，`docs/tasks.md` §4.14 F5）：消息的位置（带话题的 `ChannelPeer`）有归属任务、
//! 任务没被停、会话还活着时，`judge` 不调 Router，直接把去向定成 `continue` 给那个任务（[`Decision::direct`]），
//! 之后与 Router 判出的 `continue` 走同一段执行——展示消息、请求键幂等、回话去处都不用另做一套。
//! `new` 派出的任务带上这个位置的归属（`komo.task.thread`），位置原来的归属在同一次提交里清掉。
//!
//! 群的"线"（`kind = main`、`source = line:…`，[`GROUP_LINE_PREFIX`]）走同一套：展示消息与分发任务落在那个群
//! 自己的会话里，Router 输入只看这条线（[`input`]），范围是 `RouteScope::Group`。
//!
//! 结果不在这里：任务一轮结束时 runtime 的 `report` 在主会话里落一条 `main.reply` 展示 entry
//! （`komo.task.reply` 写定了去处），[`deliver`] 订阅提交把它原文投到渠道。

pub mod deliver;
mod input;
mod owner;

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use komo_agent::router::{Fallback, RecentMessage, Route, title_from};
use komo_kernel::durable::TaskStatus;
use komo_kernel::durable::storage::TaskQuery;
use komo_kernel::durable::task::RunningTask;
use komo_kernel::durable::{
    Content, ConversationId, EntryId, SessionLifecycle, SubmissionId, SubmissionKind, TaskId,
    WriteState,
};
use komo_kernel::types::chat::ChannelPeer;
use komo_runtime::durable::harness::inbox::Admitted;
use komo_runtime::durable::harness::{
    NewTask, ReplyTo, SubmissionDraft, WhenBusy, WriteDraft, load_task_doc, outcome, store_task_doc,
};
use komo_runtime::durable::{
    DurableError, Ownership, TaskError, TaskKind, TaskOptions, TaskRuntime, Tx,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::DurableCore;
use super::sessions::SessionError;
use super::submit::{Accepted, Landing};
use crate::service::state::GatewayState;

/// 主线上用户发的消息（只用于展示的 write，没有 `model`；`data {text, peer?, principal?}`）。
pub const MAIN_USER_ENTRY: &str = "main.user";
pub use komo_runtime::durable::harness::MAIN_REPLY_ENTRY;
/// 分发任务的种类名。
pub const DISPATCH_KIND: &str = "komo.dispatch";

/// 主线消息的来源。
#[derive(Debug, Clone)]
pub struct MainMessage {
    /// 展示在主线上的正文，也是 Router 判断的原文。
    pub text: String,
    /// 请求键（平台消息键 / HTTP 的请求键）：持久去重。
    pub request_id: String,
    /// 来自哪个渠道对端；HTTP 来的没有（回话只落主线，不投渠道）。
    pub peer: Option<ChannelPeer>,
    pub principal: Option<Value>,
}

/// 分发任务的输入（建任务时写定，之后不变）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchInput {
    /// 主会话。
    pub session: String,
    /// 主线消息落在哪个 conversation。
    pub conversation: ConversationId,
    /// 受理这条消息的 write submission：消息的 entry 由它找（排队时先等它落下）。
    pub submission: SubmissionId,
    /// 回话的去处（`ChannelPeer` 的字符串形式）；没有就只落主线的展示 entry。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
}

/// 分发任务的检查点（`phase` 是引擎要求的字符串字段）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum Checkpoint {
    /// 第一个检查点：还没判断（重启后从这里重做，判断没有副作用）。
    Judge,
    /// 第二个检查点：去向已定，下一次提交落效果并完成。
    Execute { decision: Box<Decision> },
}

/// 判断的结果：判断那一刻的快照，执行时照它做，不再重读。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub route: Route,
    /// `continue` / `stop` 指的任务（候选里的短号已解析成 conversation）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ConversationId>,
    /// `Some` = 这个去向是兜底得来的，不是模型的判断：原因与调了几次模型（以后查误判用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Fallback>,
    /// 判断时用的主线最近消息：`new` 的第一条输入带的是**同一份**（`docs/tasks.md` §4.12 U4），
    /// 重启后执行不会因为主线又长了几条而不同。
    #[serde(default)]
    pub recent: Vec<RecentMessage>,
    /// `list` 的清单正文（判断时渲染好）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<String>,
    /// 直通：位置的归属任务收下这条消息，没有调 Router（`route` 是占位的 `continue`）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub direct: bool,
}

/// 直通时 `Route::Continue` 里占位的任务标识（没有 Router 给的短号）。
const DIRECT_TASK: &str = "direct";

/// 引擎里的分发任务。`state` 是弱引用：任务定义装在注册表里，不让它替 `GatewayState` 续命。
pub struct DispatchTask {
    state: Weak<GatewayState>,
}

impl DispatchTask {
    pub fn new(state: &Arc<GatewayState>) -> Self {
        DispatchTask {
            state: Arc::downgrade(state),
        }
    }

    /// 只用来建任务（名字、版本、起始检查点）的定义：建任务发生在受理的提交里，那时不需要跑它。
    fn definition() -> Self {
        DispatchTask { state: Weak::new() }
    }

    fn state(&self) -> Result<Arc<GatewayState>, TaskError> {
        self.state
            .upgrade()
            .ok_or_else(|| TaskError::new("Gateway 已经停了，分发任务等下一代接着做"))
    }
}

#[async_trait]
impl TaskKind for DispatchTask {
    fn name(&self) -> &str {
        DISPATCH_KIND
    }

    fn initial(&self, _input: &Value) -> Value {
        serde_json::to_value(Checkpoint::Judge).expect("检查点可序列化")
    }

    async fn run_phase(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        let checkpoint: Checkpoint = serde_json::from_value(task.checkpoint.clone())
            .map_err(|error| TaskError::new(format!("分发任务的检查点读不出来：{error}")))?;
        let state = self.state()?;
        let input: DispatchInput = serde_json::from_value(task.input().clone())
            .map_err(|error| TaskError::new(format!("分发任务的输入读不出来：{error}")))?;
        match checkpoint {
            Checkpoint::Judge => judge(&state, task, rt, &input).await,
            Checkpoint::Execute { decision } => execute(&state, rt, &input, *decision).await,
        }
    }

    async fn abort(&self, _task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        rt.commit_next(outcome::aborted(None)).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------- 受理

/// 受理的提交里建分发任务（`DurableCore::land` 调，在主线消息的 write 受理之后）。`entry` 是消息落下的
/// entry（write 排队时还没有）。
pub(super) async fn create_task(
    tx: &mut Tx,
    session: &str,
    conversation: ConversationId,
    submission: SubmissionId,
    peer: Option<String>,
) -> Result<TaskId, DurableError> {
    let input = DispatchInput {
        session: session.to_owned(),
        conversation,
        submission,
        peer,
    };
    tx.create_task(
        &DispatchTask::definition(),
        serde_json::to_value(&input).expect("输入可序列化"),
        // 属于主会话的 conversation、不是 background：删会话时随它一起被取消。
        Ownership::Conversation,
        TaskOptions {
            conversation: Some(conversation),
            background: false,
            abandon_on_restart: false,
        },
    )
    .await
}

/// 受理一次（land 的参数）。
pub(super) struct Seed {
    pub peer: Option<String>,
}

impl DurableCore {
    /// 主会话收到一条普通文本：落成主线上的展示消息，同一次提交里建一个分发任务（模块文档）。
    /// 请求键重发返回原来那条（`deduplicated`），不再建任务。
    pub async fn dispatch_message(
        &self,
        session: &str,
        message: MainMessage,
    ) -> Result<Accepted, SessionError> {
        let hash = komo_kernel::types::digest::ContentHash::of_str(&message.text).to_string();
        let peer = message.peer.as_ref().map(ToString::to_string);
        let data = json!({
            "text": message.text,
            "peer": peer,
            "principal": message.principal,
            "inputHash": hash,
        });
        let draft = SubmissionDraft::Write {
            entry: WriteDraft::display(MAIN_USER_ENTRY, data),
            request_id: Some(message.request_id),
        };
        self.land(
            session,
            draft,
            Landing {
                hash,
                text: message.text,
                reject_when_busy: false,
                model: None,
                interject: false,
                dispatch: Some(Seed { peer }),
            },
        )
        .await
    }
}

/// 测试用：停在 `point`（0 = 判断之前，1 = 执行之前）直到旗子放下；引擎关闭时放弃。
#[cfg(test)]
async fn hold(core: &DurableCore, point: usize, rt: &TaskRuntime) -> Result<(), TaskError> {
    use std::sync::atomic::Ordering;
    let cancelled = rt.cancelled();
    while core.dispatch_hold[point].load(Ordering::SeqCst) {
        tokio::select! {
            _ = cancelled.cancelled() => return Err(TaskError::new("引擎关闭")),
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- 取主线消息

/// 主线消息的 entry 与正文。write 还没落下（排队）就等它：分发任务可能先于展示落下被调度。
async fn message_of(
    rt: &TaskRuntime,
    input: &DispatchInput,
) -> Result<Option<(EntryId, String)>, TaskError> {
    loop {
        let record = rt
            .submission(input.submission)
            .await?
            .ok_or_else(|| TaskError::new("主线消息的 submission 不见了"))?;
        let entry = match record.kind {
            SubmissionKind::Write {
                state: WriteState::Done { entry },
            } => entry,
            SubmissionKind::Write {
                state: WriteState::Queued,
            } => {
                rt.sleep_until(rt.now()? + 500).await?;
                continue;
            }
            // 消息没落下（被撤回）：没有可分发的东西。
            _ => return Ok(None),
        };
        let stored = rt
            .entry(entry)
            .await?
            .ok_or_else(|| TaskError::new("主线消息的 entry 不见了"))?;
        let text = stored
            .data
            .as_ref()
            .and_then(|data| data.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        return Ok(Some((entry, text)));
    }
}

// ---------------------------------------------------------------- judge

async fn judge(
    state: &Arc<GatewayState>,
    task: &RunningTask,
    rt: &TaskRuntime,
    dispatch: &DispatchInput,
) -> Result<(), TaskError> {
    let core = state
        .core()
        .map_err(|error| TaskError::new(error.to_string()))?;
    #[cfg(test)]
    hold(core, 0, rt).await?;
    // 次序：同一个主会话里更早的消息先分发完，后一条判断时才看得到前一条派出的任务。
    let earlier = earlier_open(core, dispatch.conversation, task.id()).await?;
    if !earlier.is_empty() {
        let waiting = serde_json::to_value(Checkpoint::Judge).expect("检查点可序列化");
        rt.commit_next(outcome::waiting(waiting, earlier)).await?;
        return Ok(());
    }
    let Some((entry, text)) = message_of(rt, dispatch).await? else {
        rt.commit_next(outcome::completed(json!({ "route": "none" })))
            .await?;
        return Ok(());
    };
    let record = core
        .session(&dispatch.session)
        .await
        .map_err(|error| TaskError::new(error.to_string()))?;
    let position = dispatch.peer.as_deref().and_then(ChannelPeer::parse);
    // 直通（F5）：位置有归属、归属的任务没被停、会话还活着 → 不调 Router。判断在这里、在"更早的消息都分发完"
    // 之后做，所以同一话题里紧挨着的两条消息，后一条看得到前一条刚开出来的归属。
    if let Some(position) = &position
        && let Some(holder) = owner::holder_of(core, &dispatch.session, position)
            .await
            .map_err(|error| TaskError::new(error.to_string()))?
        && !holder.stopped
    {
        let decision = Decision {
            route: Route::Continue {
                task: DIRECT_TASK.to_owned(),
            },
            target: Some(holder.conversation),
            fallback: None,
            recent: Vec::new(),
            list: None,
            direct: true,
        };
        let next = serde_json::to_value(Checkpoint::Execute {
            decision: Box::new(decision),
        })
        .expect("检查点可序列化");
        rt.commit_next(outcome::running(next)).await?;
        return Ok(());
    }
    let built = input::build(core, &record, entry, &text, position.as_ref())
        .await
        .map_err(|error| TaskError::new(error.to_string()))?;
    let agent = record
        .agent
        .clone()
        .unwrap_or_else(|| state.snapshot().agent.default_agent.clone());
    let routed = super::router::judge(
        state.llm.clone() as Arc<dyn komo_kernel::traits::LlmClient>,
        &state.snapshot(),
        &agent,
        &built.input,
    )
    .await;
    if let Some(fallback) = &routed.fallback {
        // 兜底得来的去向记进检查点（`Decision::fallback`）也记进日志：以后查误判靠它。
        tracing::warn!(
            reason = %fallback.reason,
            attempts = fallback.attempts,
            "分发判断没成，按兜底去向处理"
        );
    }
    let target = match &routed.route {
        Route::Continue { task } | Route::Stop { task } => {
            built.handles.get(task.as_str()).copied()
        }
        _ => None,
    };
    let list = matches!(routed.route, Route::List).then(|| input::render_list(&built.input.tasks));
    let decision = Decision {
        route: routed.route,
        target,
        fallback: routed.fallback,
        recent: built.input.recent.clone(),
        list,
        direct: false,
    };
    let next = serde_json::to_value(Checkpoint::Execute {
        decision: Box::new(decision),
    })
    .expect("检查点可序列化");
    rt.commit_next(outcome::running(next)).await?;
    Ok(())
}

/// 同一个 conversation 里比 `me` 更早、还没结束的分发任务。
async fn earlier_open(
    core: &DurableCore,
    conversation: ConversationId,
    me: TaskId,
) -> Result<Vec<TaskId>, TaskError> {
    let mut out = Vec::new();
    for status in [
        TaskStatus::Pending,
        TaskStatus::Running,
        TaskStatus::Waiting,
    ] {
        let query = TaskQuery {
            conversation: Some(conversation),
            kind: Some(DISPATCH_KIND.to_owned()),
            status: Some(status),
            ..TaskQuery::default()
        };
        let mut cursor = None;
        loop {
            let page = core
                .storage
                .scan_tasks(&query, 64, cursor.as_ref())
                .await
                .map_err(|error| TaskError::new(error.to_string()))?;
            out.extend(page.items.iter().map(|task| task.id).filter(|id| *id < me));
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- execute

async fn execute(
    state: &Arc<GatewayState>,
    rt: &TaskRuntime,
    dispatch: &DispatchInput,
    decision: Decision,
) -> Result<(), TaskError> {
    let core = state
        .core()
        .map_err(|error| TaskError::new(error.to_string()))?;
    #[cfg(test)]
    hold(core, 1, rt).await?;
    let Some((entry, text)) = message_of(rt, dispatch).await? else {
        rt.commit_next(outcome::completed(json!({ "route": "none" })))
            .await?;
        return Ok(());
    };
    let record = core
        .session(&dispatch.session)
        .await
        .map_err(|error| TaskError::new(error.to_string()))?;
    let group = input::group_of(&record);
    let position = dispatch.peer.as_deref().and_then(ChannelPeer::parse);
    // `continue` / `stop` 的目标必须是这条线派出的任务（群里还要回话去处在这个群）：候选里本来只有这些，
    // 这里是第二道。不属于就什么都不做——不停、不转交，也不退成新任务。
    let refused = match (&decision.route, decision.target) {
        (Route::Continue { .. } | Route::Stop { .. }, Some(target)) => {
            !owner::belongs(core, &dispatch.session, group.as_ref(), target)
                .await
                .map_err(|error| TaskError::new(error.to_string()))?
        }
        _ => false,
    };
    if refused {
        tracing::warn!(route = ?decision.route, target = ?decision.target, "分发的目标不属于这条线，没有执行");
        rt.commit_next(outcome::completed(
            json!({ "route": "refused", "task": decision.target }),
        ))
        .await?;
        return Ok(());
    }
    if let (Route::Stop { .. }, Some(target)) = (&decision.route, decision.target) {
        // 完整的"停下"（撤排队的输入、停监听、标已停、取消当前执行）见 `DurableCore::stop_task`。
        // 先于完成提交：每一步幂等，崩在两者之间重放只是再做一遍。任务已有的中间产出留在任务会话里，不外发。
        core.stop_task(target)
            .await
            .map_err(|error| TaskError::new(error.to_string()))?;
    }
    // 新任务拿位置的归属，位置原来的归属任务在同一次提交里清掉（每个位置至多一个）。同一条线上的分发
    // 一条一条做（`judge` 先等更早的），这里读到的归属不会在提交前被别的分发改掉。
    let thread = position
        .as_ref()
        .filter(|position| position.thread.is_some())
        .map(ToString::to_string);
    let old_holder = match (&position, &decision.route) {
        (Some(position), Route::New { .. } | Route::Continue { .. }) if thread.is_some() => {
            owner::holder_of(core, &dispatch.session, position)
                .await
                .map_err(|error| TaskError::new(error.to_string()))?
                .map(|holder| holder.conversation)
        }
        _ => None,
    };
    let harness = core.harness.clone();
    let dispatch = dispatch.clone();
    rt.commit(move |tx, _task| {
        Box::pin(async move {
            let reply = ReplyTo {
                peer: dispatch.peer.clone(),
                origin_entry: entry,
                origin_submission: dispatch.submission,
            };
            let key = format!("dispatch:{entry}");
            let done = match &decision.route {
                Route::New { description, title } => {
                    let task = new_task(
                        description,
                        title,
                        &text,
                        &decision.recent,
                        key,
                        reply,
                        thread.clone(),
                    );
                    let child = harness
                        .spawn_task_in_tx(tx, dispatch.conversation, task)
                        .await?;
                    release(tx, old_holder, &thread).await?;
                    json!({ "route": "new", "task": child })
                }
                Route::Continue { .. } => {
                    let target = decision
                        .target
                        .ok_or_else(|| DurableError::rejected("continue 的目标没有解析出来"))?;
                    match continue_task(tx, &harness, target, &text, &key, &reply).await? {
                        true => json!({ "route": "continue", "task": target }),
                        false => {
                            // 目标会话已经不收输入了（判断之后被删）：宁可多开一个任务，不丢消息。
                            let title = title_from(&text);
                            let task = new_task(
                                &text,
                                &title,
                                &text,
                                &decision.recent,
                                key,
                                reply,
                                thread.clone(),
                            );
                            let child = harness
                                .spawn_task_in_tx(tx, dispatch.conversation, task)
                                .await?;
                            release(tx, old_holder, &thread).await?;
                            json!({ "route": "new", "task": child, "from": "continue" })
                        }
                    }
                }
                Route::Stop { .. } => {
                    // 往来源对端说一句普通的话，同样落成主线上的 `main.reply`。
                    if decision.target.is_some() {
                        say(tx, &harness, &dispatch, &reply, key, STOPPED_REPLY).await?;
                    }
                    json!({ "route": "stop", "task": decision.target })
                }
                Route::List => {
                    let said = decision.list.clone().unwrap_or_default();
                    say(tx, &harness, &dispatch, &reply, key, &said).await?;
                    json!({ "route": "list" })
                }
                // 群里"不该由 Agent 接的"：什么都不发，消息只留在线上。
                Route::Ignore => json!({ "route": "ignore" }),
            };
            Ok(Some(outcome::completed(done)))
        })
    })
    .await?;
    core.resume();
    Ok(())
}

/// 位置换了新任务：原归属任务的 `thread` 清掉（它还是 `thread` 这个位置的才清；任务本身不动）。
async fn release(
    tx: &mut Tx,
    old: Option<ConversationId>,
    thread: &Option<String>,
) -> Result<(), DurableError> {
    let (Some(old), Some(thread)) = (old, thread) else {
        return Ok(());
    };
    if let Some(mut doc) = load_task_doc(tx, old).await?
        && doc.thread.as_ref() == Some(thread)
    {
        doc.thread = None;
        store_task_doc(tx, old, &doc).await?;
    }
    Ok(())
}

/// 停下之后往来源对端说的话。
const STOPPED_REPLY: &str = "好，先停了。";

/// 程序自己往主线说一句话（不经模型）：一条 `main.reply` 展示 entry，渠道投递由 [`deliver`] 照它做。
/// 请求键 `key` 让同一条消息的分发重放不重复落。
async fn say(
    tx: &mut Tx,
    harness: &komo_runtime::durable::harness::Harness,
    dispatch: &DispatchInput,
    reply: &ReplyTo,
    key: String,
    text: &str,
) -> Result<(), DurableError> {
    let data = json!({
        "text": text,
        "peer": reply.peer,
        "session": dispatch.session,
        "origin": {
            "entry": reply.origin_entry,
            "submission": reply.origin_submission,
        },
    });
    let draft = SubmissionDraft::Write {
        entry: WriteDraft::display(MAIN_REPLY_ENTRY, data),
        request_id: Some(key),
    };
    harness
        .admit_in_tx(tx, dispatch.conversation, &draft)
        .await?;
    Ok(())
}

/// `new` 的第一条输入：Router 写的任务说明、用户原话、主线最近几条消息（与 Router 输入用同一份）。
/// 这些放进输入、不放进系统提示：系统提示要跨任务命中缓存（`docs/tasks.md` §4.12 U4）。
fn new_task(
    description: &str,
    title: &str,
    message: &str,
    recent: &[RecentMessage],
    key: String,
    reply: ReplyTo,
    thread: Option<String>,
) -> NewTask {
    let mut text = description.trim().to_owned();
    if message.trim() != text {
        text.push_str(&format!("\n\n用户的原话：\n{}", message.trim()));
    }
    if !recent.is_empty() {
        text.push_str("\n\n这之前最近的几条消息（旧的在前，只用来接上前面的话）：");
        for one in recent {
            text.push_str(&format!(
                "\n- {}：{}",
                input::speaker(one.speaker),
                one.text
            ));
        }
    }
    NewTask {
        title: title.to_owned(),
        text,
        request_key: key,
        reply: Some(reply),
        thread,
    }
}

/// `continue`：把原文交给目标任务，并把回话的去处改成这条消息的来处。返回 `false` = 目标不能收了。
///
/// 目标已结束的话，提交一条输入就是"接着做"（`docs/background-tasks.md` §2）：它的 conversation 一直在，
/// 空闲时受理会直接起新一轮；忙着就并进当前一轮。
async fn continue_task(
    tx: &mut Tx,
    harness: &komo_runtime::durable::harness::Harness,
    target: ConversationId,
    text: &str,
    key: &str,
    reply: &ReplyTo,
) -> Result<bool, DurableError> {
    // 表读先于第一次表写。
    let Some(record) = tx.conversation(target).await? else {
        return Ok(false);
    };
    let Some(session) = record.session else {
        return Ok(false);
    };
    let Some(session) = tx.session(&session).await? else {
        return Ok(false);
    };
    if session.lifecycle != SessionLifecycle::Active {
        return Ok(false);
    }
    let Some(mut doc) = load_task_doc(tx, session.root_conversation).await? else {
        return Ok(false);
    };
    let draft = SubmissionDraft::Input {
        content: Content::Text(text.to_owned()),
        entry: None,
        when_busy: WhenBusy::Steer,
        request_id: Some(key.to_owned()),
        internal: false,
    };
    if matches!(
        harness
            .admit_in_tx(tx, session.current_branch, &draft)
            .await?,
        Admitted::Busy
    ) {
        return Err(DurableError::rejected("任务会话在受理时忙了"));
    }
    doc.reply = Some(reply.clone());
    // 用户的话让停下的任务重新做起来：清掉已停标记（之后的监听事件才又起得了轮次）。
    doc.stopped = false;
    store_task_doc(tx, session.root_conversation, &doc).await?;
    Ok(true)
}

/// 候选任务的短号 → conversation（`Decision::target` 用）。
pub(crate) type Handles = HashMap<String, ConversationId>;

#[cfg(test)]
mod tests;
