//! `monitor` / `monitor_stop` 在新存储上的**契约**（`docs/monitor.md`）。
//!
//! # 边界：谁做什么
//!
//! | 事 | 归谁 |
//! |---|---|
//! | 起脚本（bash、只读挂载、环境）、进程组、stdout 逐行泵、合批与背压、时限计时、刷屏 / 事件数上限、崩溃遗留进程的核对与终止 | **gateway**（D5）：`OrchestrationHooks::start_monitor` / `stop_monitor` 背后 |
//! | 事件与结束消息变成输入交回会话 | gateway 经 `Harness::submit`（事件，[`event_draft`]）与 [`Harness::finish_monitor`](crate::durable::harness::Harness::finish_monitor)（结束消息，与登记状态同一次提交） |
//! | **登记**：谁挂的、命令、时限、截止时刻、`resume_after_restart`、是否已结束 | **runtime**：Session 级 document `komo.monitors`，和一切别的状态在同一个库、同一条提交线 |
//! | 工具任务的"登记 → 起进程 → 落结果"三明治，崩溃后的恢复 | **runtime**：`monitor` / `monitor_stop` 的工具任务（本文件） |
//! | 重启后对账：声明了 `resume_after_restart` 的重挂，其余交回"已中断" | **runtime** 决定、gateway 动手：[`Harness::reconcile_monitors`](crate::durable::harness::Harness::reconcile_monitors) |
//!
//! 这样 gateway 在 D5 要做的只剩"进程这一半"：实现两个钩子、把泵的行变成 `event_draft`、进程结束
//! 时调 `finish_monitor`、启动时调一次 `reconcile_monitors`。**监听的状态不再有第二份真相**
//! （旧的 `runtime/monitors/<id>.json` 与 `runtime/monitors/children/` 里的"登记"里属于
//! 状态的那部分都进了 `komo.monitors`；进程身份——pid / 起来的时刻——仍是 gateway 自己的事，
//! runtime 不碰子进程，因为它不起它们）。
//!
//! # 登记
//!
//! `komo.monitors`（Session 级，一份）：`{ items: [MonitorRecord] }`。`id` 是挂它的工具任务的
//! ID，句柄 `#id`；`state` 是 `starting`（登记了、进程还没确认）→ `running` → `ended{reason}`。
//! 结束的登记留着最近 64 条（"停一个已经结束的"要答得出怎么结束的，旧实现只在内存里记 64 个且
//! 重启就忘，现在跨重启）。
//!
//! # `monitor` 工具任务
//!
//! ```text
//! prepare → [登记 starting + checkpoint orchestrate{monitorStart}]
//!        → hook.start_monitor(Fresh)
//!        → [登记 running] → 落结果 entry（"已开始监听 #id：标题"）
//! ```
//!
//! 起进程是真副作用，崩溃发生在登记之后、结果之前时，恢复从 `orchestrate{monitorStart}`：先让
//! gateway **认回**同一个 id 下还在跑的进程（[`LaunchMode::Recover`]，进程活得比引擎的重启久
//! 的情况，例如同一进程里重开引擎）；认不到，且登记声明了 `resume_after_restart`、剩余时限
//! 至少 1 分钟，就**重挂**（[`LaunchMode::Resume`]，同一个 id，时限只给剩下的）；否则登记为
//! `ended{interrupted}`，结果是"上一次挂的监听随重启中断了，没有自动重挂"——任意命令不自动
//! 重跑（§8.6）。
//!
//! # `monitor_stop`
//!
//! 只在挂它的那个会话里找。`starting` / `running` 的：先登记 `ended{stopped}`（意图），再
//! `stop_monitor`（幂等），最后落结果；停掉的监听**不交回**结束消息（模型自己停的，它知道）。
//! 已经结束的不算失败：答"不用停了：…已结束…"。找不到时列出这个会话还在跑的。

use std::time::Duration;

use komo_kernel::durable::task::NextTaskState;
use komo_kernel::durable::{Content, ConversationId, DocumentAddress, DocumentScope, Storage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Cx, MonitorError, Reply, Step, task_doc_address};
use crate::durable::error::{DurableError, TaskError};
use crate::durable::harness::generation::GenerationTask;
use crate::durable::harness::inbox::{self, InputEntry, QueueModes, SubmissionDraft, WhenBusy};
use crate::durable::runtime::TaskRuntime;
use crate::durable::tx::Tx;

pub const MONITORS_KIND: &str = "komo.monitors";
pub const MONITORS_VERSION: u32 = 1;
/// 交回结束消息的输入的 entry kind。
pub const MONITOR_END_ENTRY: &str = "monitor.end";
/// 交回事件的输入的 entry kind。
pub const MONITOR_EVENT_ENTRY: &str = "monitor.event";
/// 登记里最多留多少条已结束的。
const KEEP_ENDED: usize = 64;
/// 截止前至少还剩这么久才值得重挂（`docs/monitor.md` §5）。
pub const MIN_RESUME: Duration = Duration::from_secs(60);

pub fn monitors_address() -> DocumentAddress {
    DocumentAddress {
        kind: MONITORS_KIND.to_owned(),
        scope: DocumentScope::Session,
        key: None,
    }
}

/// 一个监听走到哪了。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum MonitorState {
    /// 登记了，进程还没确认起来。重启后由挂它的那个工具任务接着判，对账不碰。
    Starting,
    Running,
    Ended {
        /// `exit` / `timeout` / `flood` / `limit` / `stopped` / `interrupted` / `start_failed` /
        /// `cancelled`……由结束它的一方定。
        reason: String,
        /// 给模型看的一句话："已结束：脚本退出（exit=0）……"。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        at: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorRecord {
    /// 挂它的工具任务的 ID；句柄是 `#id`。
    pub id: u64,
    /// 事件与结束消息交回哪个 conversation。
    pub conversation: ConversationId,
    pub title: String,
    pub command: String,
    pub cwd: std::path::PathBuf,
    pub timeout_minutes: u64,
    /// 模型声明脚本只读、重跑无副作用：重启后按原句柄重挂。
    #[serde(default)]
    pub resume_after_restart: bool,
    /// 登记时刻与截止时刻（`Line` 时钟，毫秒）。
    pub started_at: i64,
    pub deadline: i64,
    pub state: MonitorState,
}

impl MonitorRecord {
    pub fn handle(&self) -> String {
        self.id.to_string()
    }

    pub fn is_live(&self) -> bool {
        !matches!(self.state, MonitorState::Ended { .. })
    }
}

/// `start_monitor` 怎么起。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// 第一次：起进程。
    Fresh,
    /// 恢复里的"认回"：只认这个 id 下还在跑的进程，认不到答 `Interrupted`，**绝不起新进程**。
    Recover,
    /// 重挂：同一个 id、同一份命令，时限只给 `remaining`；第一条事件末尾注明中间断过。
    Resume,
}

/// 交给 gateway 的启动说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorLaunch {
    pub record: MonitorRecord,
    pub mode: LaunchMode,
    /// 离截止还剩多久（毫秒，不为负）。
    pub remaining_ms: i64,
}

// ---------------------------------------------------------------- 登记 document

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Registry {
    items: Vec<MonitorRecord>,
}

impl Registry {
    fn find(&self, id: u64) -> Option<&MonitorRecord> {
        self.items.iter().find(|record| record.id == id)
    }

    fn find_mut(&mut self, id: u64) -> Option<&mut MonitorRecord> {
        self.items.iter_mut().find(|record| record.id == id)
    }

    fn insert(&mut self, record: MonitorRecord) {
        self.items.retain(|existing| existing.id != record.id);
        self.items.push(record);
        let ended = self.items.iter().filter(|r| !r.is_live()).count();
        let mut drop = ended.saturating_sub(KEEP_ENDED);
        self.items.retain(|record| {
            if drop > 0 && !record.is_live() {
                drop -= 1;
                false
            } else {
                true
            }
        });
    }
}

fn parse(value: Value) -> Result<Registry, DurableError> {
    serde_json::from_value(value)
        .map_err(|e| DurableError::rejected(format!("komo.monitors is malformed: {e}")))
}

async fn load(tx: &mut Tx) -> Result<Registry, DurableError> {
    match tx.document(&monitors_address()).await? {
        None => Ok(Registry::default()),
        Some((_, value)) => parse(value),
    }
}

async fn store(tx: &mut Tx, registry: &Registry) -> Result<(), DurableError> {
    let value = serde_json::to_value(registry).expect("Registry 可序列化");
    tx.put_document(monitors_address(), MONITORS_VERSION, value)
        .await
}

/// 已提交的全部登记（含已结束的最近 64 条）。gateway 启动与测试用。
pub async fn list(storage: &dyn Storage) -> Result<Vec<MonitorRecord>, DurableError> {
    match storage.find_document(&monitors_address()).await? {
        None => Ok(Vec::new()),
        Some(record) => Ok(parse(record.value)?.items),
    }
}

async fn read(rt: &TaskRuntime) -> Result<Registry, TaskError> {
    match rt.find_document(&monitors_address()).await? {
        None => Ok(Registry::default()),
        Some(record) => Ok(parse(record.value)?),
    }
}

/// 只改登记、不动任务状态的一次提交。
async fn mutate<F>(rt: &TaskRuntime, change: F) -> Result<(), TaskError>
where
    F: FnOnce(&mut Registry, i64) + Send + 'static,
{
    rt.commit(move |tx, _| {
        Box::pin(async move {
            let mut registry = load(tx).await?;
            change(&mut registry, tx.now());
            store(tx, &registry).await?;
            Ok(None::<NextTaskState>)
        })
    })
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- 给 gateway 的交回

/// 一条监听事件的输入：steer，请求键 `monitor:{id}:{suffix}`（`suffix` 是 gateway 自己的"代 + 序号"，
/// 重挂后用新的一代，免得撞上重启前交过的键被吞掉）。
pub fn event_draft(id: u64, title: &str, suffix: &str, text: impl Into<String>) -> SubmissionDraft {
    SubmissionDraft::Input {
        content: Content::Text(text.into()),
        entry: Some(InputEntry::new(
            MONITOR_EVENT_ENTRY,
            Some(json!({ "monitor": { "id": id, "handle": id.to_string(), "title": title } })),
        )),
        when_busy: WhenBusy::Steer,
        request_id: Some(format!("monitor:{id}:{suffix}")),
        // 内部输入：父被取消时不撤（见 `inbox` 模块文档）。
        internal: true,
    }
}

/// 结束一个监听并把消息交回挂它的会话，**同一次提交**：登记 `ended`（只有第一个结束它的人
/// 赢，已经结束的返回 `false`、什么都不写）与输入 `monitor-end:{id}`（steer）。挂它的会话不在了
/// 就只登记。
pub(crate) async fn finish_in_tx(
    tx: &mut Tx,
    generation: &GenerationTask,
    modes: QueueModes,
    id: u64,
    reason: &str,
    detail: Option<&str>,
    message: String,
) -> Result<bool, DurableError> {
    let mut registry = load(tx).await?;
    let Some(record) = registry.find_mut(id) else {
        return Ok(false);
    };
    if !record.is_live() {
        return Ok(false);
    }
    let (conversation, title) = (record.conversation, record.title.clone());
    // 表读（会话在不在、请求键、head）先于这次提交的第一次表写；登记是 document，不算表写。
    let present = tx.conversation(conversation).await?.is_some();
    record.state = MonitorState::Ended {
        reason: reason.to_owned(),
        detail: detail.map(str::to_owned),
        at: tx.now(),
    };
    store(tx, &registry).await?;
    if present {
        let draft = SubmissionDraft::Input {
            content: Content::Text(message),
            entry: Some(InputEntry::new(
                MONITOR_END_ENTRY,
                Some(json!({
                    "monitor": { "id": id, "handle": id.to_string(), "title": title, "reason": reason }
                })),
            )),
            when_busy: WhenBusy::Steer,
            request_id: Some(format!("monitor-end:{id}")),
            internal: true,
        };
        inbox::admit(tx, generation, conversation, &draft, modes).await?;
    }
    Ok(true)
}

/// 对账一条监听的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciled {
    /// 声明了 `resume_after_restart` 且还有足够的时限：已让 gateway 重挂，登记仍是 `running`。
    Resumed(u64),
    /// 已登记为结束并交回了消息（`reason`：`interrupted` / `timeout`）。
    Ended(u64, String),
}

pub(crate) fn interrupted_message(record: &MonitorRecord, why: &str) -> String {
    format!("[监听 #{}「{}」已结束]\n\n{why}", record.id, record.title)
}

// ---------------------------------------------------------------- 工具任务

pub(super) struct Request {
    pub command: String,
    pub title: String,
    pub cwd: std::path::PathBuf,
    pub timeout_minutes: u64,
    pub resume_after_restart: bool,
}

pub(super) async fn start(cx: &Cx<'_>, request: Request) -> Result<(), TaskError> {
    // 任务会话里别挂：任务这一轮一结束结果就交回去了（`docs/monitor.md` §3 的工具说明）。
    if cx
        .rt
        .find_document(&task_doc_address(cx.conversation()))
        .await?
        .is_some()
    {
        return cx
            .reply(&Reply::error(
                "挂不上监听：后台任务会话里不能挂监听：任务这一轮一结束，结果就交回去了",
            ))
            .await;
    }
    let id = cx.rt.task_id().get();
    let conversation = cx.conversation();
    // 效果一：登记（starting），checkpoint 推进到 `orchestrate { monitorStart }`。
    cx.rt
        .commit(move |tx, _| {
            Box::pin(async move {
                let now = tx.now();
                let mut registry = load(tx).await?;
                registry.insert(MonitorRecord {
                    id,
                    conversation,
                    title: request.title,
                    command: request.command,
                    cwd: request.cwd,
                    timeout_minutes: request.timeout_minutes,
                    resume_after_restart: request.resume_after_restart,
                    started_at: now,
                    deadline: now + request.timeout_minutes as i64 * 60_000,
                    state: MonitorState::Starting,
                });
                store(tx, &registry).await?;
                Ok(Some(Step::MonitorStart.state()))
            })
        })
        .await?;
    let record = own_record(cx).await?;
    launch(cx, record, LaunchMode::Fresh).await
}

/// 这次调用自己挂的那条登记（id 就是工具任务的 ID）。
async fn own_record(cx: &Cx<'_>) -> Result<MonitorRecord, TaskError> {
    let registry = read(cx.rt).await?;
    registry
        .find(cx.rt.task_id().get())
        .cloned()
        .ok_or_else(|| TaskError::new("the monitor registration disappeared"))
}

fn started_reply(record: &MonitorRecord, note: &str) -> Reply {
    Reply::ok(
        format!(
            "已开始监听 #{}：{}（最长 {} 分钟）。事件会以 [监听 #{}…] 开头的消息交回。{note}",
            record.id, record.title, record.timeout_minutes, record.id
        ),
        json!({
            "id": record.id,
            "handle": record.handle(),
            "title": record.title,
            "conversation": record.conversation,
            "timeout_minutes": record.timeout_minutes,
        }),
    )
}

async fn launch(cx: &Cx<'_>, record: MonitorRecord, mode: LaunchMode) -> Result<(), TaskError> {
    let now = cx.rt.now()?;
    let launch = MonitorLaunch {
        remaining_ms: (record.deadline - now).max(0),
        record: record.clone(),
        mode,
    };
    let id = record.id;
    match cx.guarded(cx.hooks().start_monitor(&launch)).await? {
        Ok(()) => {
            mark_running(cx.rt, id).await?;
            let note = if mode == LaunchMode::Resume {
                "（Gateway 重启过，已按原句柄重挂，时限接着算。）"
            } else {
                ""
            };
            cx.reply(&started_reply(&record, note)).await
        }
        Err(error) => {
            end_record(cx.rt, id, "start_failed", format!("没有挂上：{error}")).await?;
            cx.reply(&Reply::error(format!("挂不上监听：{error}")))
                .await
        }
    }
}

async fn mark_running(rt: &TaskRuntime, id: u64) -> Result<(), TaskError> {
    mutate(rt, move |registry, _| {
        if let Some(record) = registry.find_mut(id)
            && record.state == MonitorState::Starting
        {
            record.state = MonitorState::Running;
        }
    })
    .await
}

async fn end_record(
    rt: &TaskRuntime,
    id: u64,
    reason: &'static str,
    detail: String,
) -> Result<(), TaskError> {
    mutate(rt, move |registry, now| {
        if let Some(record) = registry.find_mut(id)
            && record.is_live()
        {
            record.state = MonitorState::Ended {
                reason: reason.to_owned(),
                detail: Some(detail),
                at: now,
            };
        }
    })
    .await
}

/// `orchestrate { monitorStart }` 的恢复：认回、重挂或判中断（见模块文档）。
pub(super) async fn resume_start(cx: &Cx<'_>) -> Result<(), TaskError> {
    let record = own_record(cx).await?;
    let id = record.id;
    if !record.is_live() {
        return cx
            .reply(&Reply::error("挂不上监听：它在这次调用收尾之前已经结束了"))
            .await;
    }
    let now = cx.rt.now()?;
    let remaining = Duration::from_millis((record.deadline - now).max(0) as u64);
    let recover = MonitorLaunch {
        record: record.clone(),
        mode: LaunchMode::Recover,
        remaining_ms: remaining.as_millis() as i64,
    };
    let last = match cx.guarded(cx.hooks().start_monitor(&recover)).await? {
        Ok(()) => {
            mark_running(cx.rt, id).await?;
            return cx.reply(&started_reply(&record, "")).await;
        }
        Err(error) => error,
    };
    if record.resume_after_restart && remaining >= MIN_RESUME {
        return launch(cx, record, LaunchMode::Resume).await;
    }
    let why = match &last {
        MonitorError::Interrupted(text) if text.is_empty() => {
            "上一次挂的监听随 Gateway 重启中断了，没有自动重挂".to_owned()
        }
        other => format!("上一次挂的监听随重启中断了，没有自动重挂（{other}）"),
    };
    end_record(cx.rt, id, "interrupted", why.clone()).await?;
    cx.reply(&Reply::error(format!("挂不上监听：{why}"))).await
}

pub(super) async fn abort_start(cx: &Cx<'_>) -> Result<Reply, TaskError> {
    let id = cx.rt.task_id().get();
    end_record(cx.rt, id, "cancelled", "挂监听的调用被取消".into()).await?;
    // 进程可能已经起来了：尽力停掉，停不掉也只是多一个有时限的脚本。
    let _ = cx.hooks().stop_monitor(id).await;
    Ok(Reply::error("挂监听的调用被取消，没有在监听"))
}

pub(super) async fn stop(cx: &Cx<'_>, raw: &str) -> Result<(), TaskError> {
    let conversation = cx.conversation();
    let registry = read(cx.rt).await?;
    let wanted = raw.trim().trim_start_matches('#');
    let found = wanted
        .parse::<u64>()
        .ok()
        .and_then(|id| registry.find(id))
        .filter(|record| record.conversation == conversation)
        .cloned();
    let Some(record) = found else {
        let running: Vec<String> = registry
            .items
            .iter()
            .filter(|record| record.conversation == conversation && record.is_live())
            .map(|record| format!("#{}「{}」", record.id, record.title))
            .collect();
        let tail = if running.is_empty() {
            "这个会话没有在跑的监听".to_owned()
        } else {
            format!("这个会话在跑的：{}", running.join("、"))
        };
        return cx
            .reply(&Reply::error(format!(
                "停不了：#{wanted} 不是这个会话挂着的监听；{tail}"
            )))
            .await;
    };
    if let MonitorState::Ended { reason, detail, .. } = &record.state {
        let how = match (reason.as_str(), detail) {
            ("stopped", _) => "已经停掉了".to_owned(),
            (_, Some(detail)) => detail.clone(),
            (reason, None) => format!("已结束（{reason}）"),
        };
        return cx
            .reply(&Reply::ok(
                format!("不用停了：监听 #{}「{}」{how}", record.id, record.title),
                json!({
                    "id": record.id,
                    "handle": record.handle(),
                    "title": record.title,
                    "already_ended": how,
                }),
            ))
            .await;
    }
    // 意图：先登记为停掉（之后泵的 `finish_monitor` 就是空操作，不会再交回结束消息）。
    let id = record.id;
    cx.rt
        .commit(move |tx, _| {
            Box::pin(async move {
                let mut registry = load(tx).await?;
                if let Some(record) = registry.find_mut(id)
                    && record.is_live()
                {
                    record.state = MonitorState::Ended {
                        reason: "stopped".into(),
                        detail: Some("已经停掉了".into()),
                        at: tx.now(),
                    };
                    store(tx, &registry).await?;
                }
                Ok(Some(Step::MonitorStop.state()))
            })
        })
        .await?;
    finish_stop(cx, &record).await
}

async fn finish_stop(cx: &Cx<'_>, record: &MonitorRecord) -> Result<(), TaskError> {
    match cx.guarded(cx.hooks().stop_monitor(record.id)).await? {
        Ok(()) | Err(MonitorError::Unknown(_)) => {
            cx.reply(&Reply::ok(
                format!("已停止监听 #{}：{}", record.id, record.title),
                json!({ "id": record.id, "handle": record.handle(), "title": record.title, "already_ended": null }),
            ))
            .await
        }
        Err(error) => cx.reply(&Reply::error(format!("停不了：{error}"))).await,
    }
}

/// `orchestrate { monitorStop }` 的恢复：登记已经是停掉了，再让进程停一次（幂等）。
pub(super) async fn resume_stop(cx: &Cx<'_>) -> Result<(), TaskError> {
    let record = stopping_record(cx).await?;
    finish_stop(cx, &record).await
}

pub(super) async fn abort_stop(cx: &Cx<'_>) -> Result<Reply, TaskError> {
    let record = stopping_record(cx).await?;
    let _ = cx.hooks().stop_monitor(record.id).await;
    Ok(Reply::ok(
        format!("已停止监听 #{}：{}", record.id, record.title),
        json!({ "id": record.id, "handle": record.handle(), "title": record.title }),
    ))
}

/// 这次 `monitor_stop` 要停的那条登记：从任务输入里的 `monitor_id` 再解析一遍。
async fn stopping_record(cx: &Cx<'_>) -> Result<MonitorRecord, TaskError> {
    let call = &cx.call.arguments;
    let wanted = call
        .get("monitor_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .trim_start_matches('#')
        .to_owned();
    let registry = read(cx.rt).await?;
    wanted
        .parse::<u64>()
        .ok()
        .and_then(|id| registry.find(id))
        .cloned()
        .ok_or_else(|| TaskError::new(format!("monitor #{wanted} is not registered")))
}
