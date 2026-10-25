//! Intervention 在新存储上（`docs/storage-durable-d5.md` §8、决定 13；W4a）。
//!
//! 旧 komo 的 Intervention 是"一条 Run 停在只有人能回答的问题上"（`waiting + intervention`）；新存储
//! 没有这种等待（`WaitReason` 不再产生），人要看的只剩一类：**faulted 任务**——任务代码本身出了错
//! （phase 抛错、迁移失败、reconcile 发现它引用的 blob 读不到），调度器写了 `faulted` 结局。协议冻结，
//! 所以它们映射成旧词汇：
//!
//! | 字段 | 值 |
//! |---|---|
//! | `handle` | faulted 任务的十进制 ID |
//! | `kind` | `Blocked` |
//! | `question` | fault 的原因（`TaskOutcomeError.message`） |
//! | `verdicts` | 只有 `abandon`（决定 13；`resolve` 在新存储里没有意义——没有"等前提处理好"的 Run） |
//! | `run` / `call` | 尽力而为：工具任务按它的 assistant entry 在投影里找所属 run；找不到就是 `None` |
//!
//! # 清单是派生视图
//!
//! 清单从 `durable.db` 的 faulted 任务算出来，没有另一张表：[`FaultIndex`] 开库后第一次用时全扫一遍
//! terminal 任务（TODO：任务表没有按结局的索引，会话多了以后加），之后靠提交订阅增量维护（回调只做
//! 同步的 `Mutex` 插入）。`abandon` 只是**确认**——记在 Session 级 document `komo.interventions.ack`
//! （key = komo Session ID，值 `{ abandoned: [任务 ID] }`），确认过的不再列出、不再投递；任务本身已经
//! terminal，没有别的可改。purge 时随 Session 一起退役。
//!
//! # NeedsAttention 投递
//!
//! [`spawn_attention`] 订阅 faulted 任务的出现，对每个未确认的任务投一条 `NeedsAttention`：**来源
//! 对端 + home chat**（同一个只投一次），逻辑键 `intervention:blocked:{任务}:…` 幂等，所以
//! 启动时回放（上一代崩在投递前）与帧路径对同一个任务各投一次也只有一条。cron 会话没有来源对端，
//! 照投 home，**不看 `notify: never`**——旧语义"faulted 不能静默"（W3b 的 `cron_watch` 只管 run 的终态）。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use komo_kernel::durable::storage::TaskQuery;
use komo_kernel::durable::{
    ConversationId, Cursor, DocumentAddress, DocumentScope, EntryId, ScanOrder, SessionKind,
    SessionLifecycle, SessionRecord, Storage, StorageWrite, TaskId, TaskOutcome, TaskRecord,
    TaskState, TaskStatus, legacy_ids,
};
use komo_kernel::protocol::http::{
    InterventionAnswerResponse, InterventionDetail, InterventionKind, InterventionListQuery,
    InterventionSummary, InterventionVerdict,
};
use komo_kernel::traits::{GatewayError, Shutdown};
use komo_kernel::types::chat::{ChannelPeer, DeliveryTarget, Outbound};
use komo_kernel::types::ids::{RunId, SessionId, ToolCallId};
use komo_runtime::durable::harness::{COMPACTION_KIND, GENERATION_KIND, TOOL_KIND};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::DurableCore;
use super::sessions::{SessionError, time_of};
use super::tree::load_tree;
use super::watch::{Source, source_of_entry};
use crate::service::state::GatewayState;

/// Session 级 document 的种类：key 是 komo Session ID，值 `{ abandoned: [任务 ID] }`。
pub const ACK_KIND: &str = "komo.interventions.ack";
const ACK_VERSION: u32 = 1;

const SCAN_PAGE: usize = 512;
/// 轮询停机旗子的间隔（`Shutdown` 只是个标志，不能 await）。
const SHUTDOWN_POLL: Duration = Duration::from_millis(250);

pub fn ack_address(session: &str) -> DocumentAddress {
    DocumentAddress {
        kind: ACK_KIND.to_owned(),
        scope: DocumentScope::Session,
        key: Some(session.to_owned()),
    }
}

/// 一个 terminal 任务的 fault 原因；不是 `faulted` 就是 `None`。
pub fn fault_message(record: &TaskRecord) -> Option<&str> {
    match &record.state {
        TaskState::Terminal {
            outcome: TaskOutcome::Faulted { error },
        } => Some(error.message.as_str()),
        _ => None,
    }
}

// ------------------------------------------------------------------- 索引

/// 全部 faulted 任务的内存索引（见模块文档）。
#[derive(Default)]
pub struct FaultIndex {
    state: Mutex<IndexState>,
    /// 全扫只做一次；并发的第一次调用排队等它。
    loading: tokio::sync::Mutex<()>,
    /// 投递任务的接收口（有人订阅才有）。
    announce: Mutex<Option<mpsc::UnboundedSender<TaskRecord>>>,
}

#[derive(Default)]
struct IndexState {
    loaded: bool,
    tasks: BTreeMap<u64, TaskRecord>,
}

impl FaultIndex {
    /// 提交线上的订阅回调：同步、要轻。
    pub fn observe(&self, writes: &[StorageWrite]) {
        for write in writes {
            let StorageWrite::Task(record) = write else {
                continue;
            };
            if fault_message(record).is_none() {
                continue;
            }
            let fresh = self
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .tasks
                .insert(record.id.get(), record.clone())
                .is_none();
            if fresh
                && let Some(sender) = self
                    .announce
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
            {
                let _ = sender.send(record.clone());
            }
        }
    }

    /// 订阅"新出现的 faulted 任务"（给 [`spawn_attention`]；只有最后一个订阅者收得到）。
    fn subscribe(&self) -> mpsc::UnboundedReceiver<TaskRecord> {
        let (sender, receiver) = mpsc::unbounded_channel();
        *self.announce.lock().unwrap_or_else(PoisonError::into_inner) = Some(sender);
        receiver
    }

    async fn ensure_loaded(&self, storage: &dyn Storage) -> Result<(), SessionError> {
        if self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .loaded
        {
            return Ok(());
        }
        let _loading = self.loading.lock().await;
        if self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .loaded
        {
            return Ok(());
        }
        let query = TaskQuery {
            status: Some(TaskStatus::Terminal),
            order: Some(ScanOrder::Ascending),
            ..TaskQuery::default()
        };
        let mut found = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = storage
                .scan_tasks(&query, SCAN_PAGE, cursor.as_ref())
                .await?;
            found.extend(
                page.items
                    .into_iter()
                    .filter(|record| fault_message(record).is_some()),
            );
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for record in found {
            state.tasks.entry(record.id.get()).or_insert(record);
        }
        state.loaded = true;
        Ok(())
    }

    fn all(&self) -> Vec<TaskRecord> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tasks
            .values()
            .cloned()
            .collect()
    }

    fn get(&self, id: u64) -> Option<TaskRecord> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tasks
            .get(&id)
            .cloned()
    }

    /// 这些 conversation 被 purge 了：它们的任务不再列。
    pub fn forget(&self, conversations: &[ConversationId]) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tasks
            .retain(|_, record| !conversations.contains(&record.conversation));
    }
}

// ------------------------------------------------------------------- 清单

/// 一个未确认的 faulted 任务，连同它所在的 Session。
#[derive(Debug, Clone)]
pub struct Faulted {
    pub task: TaskRecord,
    pub session: SessionRecord,
    pub message: String,
}

impl Faulted {
    pub fn handle(&self) -> String {
        self.task.id.to_string()
    }

    fn what(&self) -> &'static str {
        match self.task.kind.as_str() {
            TOOL_KIND => "工具调用",
            GENERATION_KIND => "模型请求",
            COMPACTION_KIND => "压缩",
            _ => "任务",
        }
    }
}

/// 一个 Session 里已确认放弃的任务 ID。
async fn acked(storage: &dyn Storage, session: &str) -> Result<BTreeSet<u64>, SessionError> {
    let Some(record) = storage.find_document(&ack_address(session)).await? else {
        return Ok(BTreeSet::new());
    };
    Ok(decode_ack(&record.value))
}

fn decode_ack(value: &Value) -> BTreeSet<u64> {
    value
        .get("abandoned")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

impl DurableCore {
    /// 新出现的 faulted 任务的索引（订阅在 `DurableCore::open` 里接上）。
    pub fn faults(&self) -> &Arc<FaultIndex> {
        &self.faults
    }

    /// 未确认的 faulted 任务（升序）。`only` 给了就只取那个 Session 的。`deleted` / `purged` 的会话
    /// 不列：没有人能再对它们做什么。
    pub async fn faulted(&self, only: Option<&str>) -> Result<Vec<Faulted>, SessionError> {
        self.faults.ensure_loaded(&*self.storage).await?;
        let mut sessions: HashMap<ConversationId, Option<SessionRecord>> = HashMap::new();
        let mut acks: HashMap<String, BTreeSet<u64>> = HashMap::new();
        let mut out = Vec::new();
        for task in self.faults.all() {
            let session = match sessions.get(&task.conversation) {
                Some(found) => found.clone(),
                None => {
                    let found = self.session_of_conversation(task.conversation).await?;
                    sessions.insert(task.conversation, found.clone());
                    found
                }
            };
            let Some(session) = session else { continue };
            if only.is_some_and(|only| only != session.id)
                || !matches!(
                    session.lifecycle,
                    SessionLifecycle::Active | SessionLifecycle::Closing
                )
            {
                continue;
            }
            if !acks.contains_key(&session.id) {
                let set = acked(&*self.storage, &session.id).await?;
                acks.insert(session.id.clone(), set);
            }
            if acks[&session.id].contains(&task.id.get()) {
                continue;
            }
            let Some(message) = fault_message(&task).map(str::to_owned) else {
                continue;
            };
            out.push(Faulted {
                task,
                session,
                message,
            });
        }
        Ok(out)
    }

    /// conversation 属于哪个 Session（行读得出才算）。
    async fn session_of_conversation(
        &self,
        conversation: ConversationId,
    ) -> Result<Option<SessionRecord>, SessionError> {
        let Some(record) = self.storage.conversation(conversation).await? else {
            return Ok(None);
        };
        let Some(session) = record.session else {
            return Ok(None);
        };
        Ok(self.storage.session(&session).await?)
    }

    /// 待处理清单（协议里的 [`InterventionSummary`]）。`kind = verify` 恒空：新存储没有"结果不明"
    /// 的停等，那类走模型（uncertain 结果）。
    pub async fn interventions(
        &self,
        query: &InterventionListQuery,
    ) -> Result<Vec<InterventionSummary>, SessionError> {
        if query.kind == Some(InterventionKind::Verify) {
            return Ok(Vec::new());
        }
        let only = query.session.as_ref().map(SessionId::as_str);
        let mut runs = RunLookup::default();
        let mut out = Vec::new();
        for faulted in self.faulted(only).await? {
            let summary = self.summary_of(&faulted, &mut runs).await;
            if query
                .run
                .as_ref()
                .is_some_and(|run| summary.run.as_ref() != Some(run))
            {
                continue;
            }
            out.push(summary);
        }
        Ok(out)
    }

    /// 一个 Session 的待处理清单（会话详情与 resume 的 `pending`）。
    pub async fn pending_of(
        &self,
        session: &str,
    ) -> Result<Vec<InterventionSummary>, SessionError> {
        self.interventions(&InterventionListQuery {
            session: Some(SessionId::from_raw(session)),
            ..InterventionListQuery::default()
        })
        .await
    }

    /// 按句柄取详情；不在清单里（没有这个任务、不是 faulted、已确认）是 `None`。
    pub async fn intervention(
        &self,
        handle: &str,
    ) -> Result<Option<InterventionDetail>, SessionError> {
        let Some(faulted) = self.find_faulted(handle).await? else {
            return Ok(None);
        };
        let acks = acked(&*self.storage, &faulted.session.id).await?;
        if acks.contains(&faulted.task.id.get()) {
            return Ok(None);
        }
        let summary = self.summary_of(&faulted, &mut RunLookup::default()).await;
        Ok(Some(InterventionDetail::Blocked {
            reason: faulted.message.clone(),
            summary,
        }))
    }

    /// 句柄 → faulted 任务（含已确认的）。
    async fn find_faulted(&self, handle: &str) -> Result<Option<Faulted>, SessionError> {
        let Ok(id) = handle.trim().parse::<u64>() else {
            return Ok(None);
        };
        self.faults.ensure_loaded(&*self.storage).await?;
        let task = match self.faults.get(id) {
            Some(task) => task,
            None => match self.storage.task(TaskId::new(id)).await? {
                Some(task) if fault_message(&task).is_some() => task,
                _ => return Ok(None),
            },
        };
        let Some(message) = fault_message(&task).map(str::to_owned) else {
            return Ok(None);
        };
        let Some(session) = self.session_of_conversation(task.conversation).await? else {
            return Ok(None);
        };
        Ok(Some(Faulted {
            task,
            session,
            message,
        }))
    }

    async fn summary_of(&self, faulted: &Faulted, runs: &mut RunLookup) -> InterventionSummary {
        let task = &faulted.task;
        let call = (task.kind == TOOL_KIND)
            .then(|| task.input.get("callId").and_then(Value::as_str))
            .flatten()
            .map(ToolCallId::from_raw);
        let run = match task_entry(task) {
            Some(entry) => runs.run_of(self, &faulted.session.id, entry).await,
            None => None,
        };
        InterventionSummary {
            handle: faulted.handle(),
            kind: InterventionKind::Blocked,
            session: SessionId::from_raw(faulted.session.id.clone()),
            run,
            call,
            question: faulted.message.clone(),
            verdicts: vec![InterventionVerdict::Abandon],
            created_at: time_of(task.ended_at.unwrap_or(0)),
        }
    }

    /// 答复。**只有 `abandon`**（决定 13）：记进 `komo.interventions.ack`，之后不再列出、不再投递。
    /// 同一条再答一次返回原结论（`already_answered`）；别的结论是 `InvalidRequest`（HTTP 映射成 422）。
    pub async fn answer_intervention(
        &self,
        handle: &str,
        verdict: InterventionVerdict,
    ) -> Result<InterventionAnswerResponse, GatewayError> {
        let gateway = |error: SessionError| GatewayError::Internal(error.to_string());
        let Some(faulted) = self.find_faulted(handle).await.map_err(gateway)? else {
            return Err(GatewayError::NotFound {
                what: format!("Intervention {handle}"),
            });
        };
        let id = faulted.task.id.get();
        let already = acked(&*self.storage, &faulted.session.id)
            .await
            .map_err(gateway)?
            .contains(&id);
        if already {
            return Ok(InterventionAnswerResponse {
                handle: handle.to_owned(),
                kind: InterventionKind::Blocked,
                verdict: InterventionVerdict::Abandon,
                run_state: None,
                note: format!("{handle} 已经确认放弃过了，这里是原结论。"),
                already_answered: true,
            });
        }
        if verdict != InterventionVerdict::Abandon {
            return Err(GatewayError::InvalidRequest(format!(
                "这条是 blocked（任务出错：{}），答不了 `{}`；它此刻能答的是：`abandon`",
                faulted.message,
                verdict.as_str()
            )));
        }
        let session = faulted.session.id.clone();
        self.engine
            .commit(move |tx| {
                Box::pin(async move {
                    let address = ack_address(&session);
                    let mut set = match tx.document(&address).await? {
                        Some((_, value)) => decode_ack(&value),
                        None => BTreeSet::new(),
                    };
                    set.insert(id);
                    let ids: Vec<u64> = set.into_iter().collect();
                    tx.put_document(address, ACK_VERSION, json!({ "abandoned": ids }))
                        .await
                })
            })
            .await
            .map_err(|error| GatewayError::Internal(error.to_string()))?;
        Ok(InterventionAnswerResponse {
            handle: handle.to_owned(),
            kind: InterventionKind::Blocked,
            verdict: InterventionVerdict::Abandon,
            run_state: None,
            note: format!(
                "{handle}（{}出错：{}）已确认放弃，不再列在待处理里。",
                faulted.what(),
                faulted.message
            ),
            already_answered: false,
        })
    }
}

/// 工具任务输入里的 assistant entry（`{"kind":"model","assistant":13,…}`）；嵌套调用与别的任务没有。
fn task_entry(task: &TaskRecord) -> Option<EntryId> {
    if task.kind != TOOL_KIND {
        return None;
    }
    task.input
        .get("assistant")
        .and_then(Value::as_u64)
        .map(EntryId::new)
}

/// 一个 Session 的投影事件，按需读、一次调用内复用：用来把 assistant entry 对到它所属的 run。
#[derive(Default)]
struct RunLookup {
    events: HashMap<String, Vec<komo_kernel::events::Event>>,
}

impl RunLookup {
    async fn run_of(&mut self, core: &DurableCore, session: &str, entry: EntryId) -> Option<RunId> {
        if !self.events.contains_key(session) {
            let events =
                super::feed::backfill(&*core.storage, session, komo_kernel::types::ids::Seq::ZERO)
                    .await
                    .unwrap_or_default();
            self.events.insert(session.to_owned(), events);
        }
        let wanted = legacy_ids::seq(entry, 0);
        self.events[session]
            .iter()
            .find(|event| event.seq == wanted)
            .and_then(|event| event.run.clone())
    }
}

// ------------------------------------------------------------------- NeedsAttention 投递

/// 起 NeedsAttention 的投递（`service::start` 调，**在 `core.resume()` 之前**）：订阅是同步建好的，
/// 之后出现的 faulted 任务一个不漏；任务里先回放已有的（上一代崩在投递前），再等新的。
pub fn spawn_attention(state: &Arc<GatewayState>, shutdown: &Shutdown) {
    let Ok(core) = state.core() else { return };
    let mut receiver = core.faults.subscribe();
    let weak = Arc::downgrade(state);
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        // 回放：全部未确认的 faulted 任务（`deliver_once` 幂等，投过的原样返回）。
        if let Some(state) = weak.upgrade()
            && let Ok(core) = state.core()
        {
            match core.faulted(None).await {
                Ok(all) => {
                    for faulted in all {
                        announce(&state, &faulted).await;
                    }
                }
                Err(error) => tracing::warn!(%error, "读不出 faulted 任务，启动时的提醒补投没做成"),
            }
        }
        loop {
            let received = tokio::time::timeout(SHUTDOWN_POLL, receiver.recv()).await;
            if shutdown.is_cancelled() {
                return;
            }
            let record = match received {
                Err(_) => continue,
                Ok(Some(record)) => record,
                Ok(None) => return,
            };
            let Some(state) = weak.upgrade() else { return };
            let Ok(core) = state.core() else { return };
            match core.find_faulted(&record.id.to_string()).await {
                Ok(Some(faulted)) => {
                    let acks = acked(&*core.storage, &faulted.session.id)
                        .await
                        .unwrap_or_default();
                    // 先确认再投的竞争不会发生（刚出现的任务还没人答），但 purge 后的会话要跳过。
                    if matches!(
                        faulted.session.lifecycle,
                        SessionLifecycle::Active | SessionLifecycle::Closing
                    ) && !acks.contains(&faulted.task.id.get())
                    {
                        announce(&state, &faulted).await;
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "faulted 任务的提醒没投出去"),
            }
        }
    });
}

/// 一个 faulted 任务该提醒给谁：来源对端（有的话）+ home chat。
async fn announce(state: &Arc<GatewayState>, faulted: &Faulted) {
    let Ok(core) = state.core() else { return };
    let summary = core.summary_of(faulted, &mut RunLookup::default()).await;
    let key = crate::notifier::intervention_key(&summary);
    let label = match faulted.session.kind {
        SessionKind::Cron => faulted
            .session
            .title
            .as_deref()
            .map(|name| format!("定时任务「{name}」：")),
        SessionKind::Task => core
            .task_label(&faulted.session.id)
            .await
            .map(|label| format!("{label}：")),
        _ => None,
    }
    .unwrap_or_default();
    let reason = format!("{label}{}出错：{}", faulted.what(), faulted.message);
    let message = Outbound::NeedsAttention {
        session: SessionId::from_raw(faulted.session.id.clone()),
        run: summary
            .run
            .clone()
            .unwrap_or_else(|| RunId::from_raw(faulted.handle())),
        reason,
    };
    let peer = attention_peer(core, &faulted.session).await;
    let mut delivered_to_source = false;
    if let Some(peer) = peer {
        let is_home = state
            .notifier
            .home_targets()
            .iter()
            .any(|target| target.peer == peer);
        if !is_home
            && let Err(error) = state
                .notifier
                .log()
                .deliver_once(&DeliveryTarget::to_peer(peer), message.clone(), &key)
                .await
        {
            tracing::warn!(%error, "faulted 任务的提醒投不到来源会话");
        }
        delivered_to_source = !is_home;
    }
    if let Err(error) = state.notifier.deliver_home_once(message, &key).await {
        if delivered_to_source {
            tracing::debug!(%error, "没有 home chat，只提醒了来源会话");
        } else {
            tracing::warn!(%error, handle = %faulted.handle(), "没有 home chat 也没有来源会话，这条没人看得见");
        }
    }
}

/// 这个 Session 的"来源对端"：渠道会话（`source = chat:{peer}`）就是那个 peer；主会话取当前分支路径上
/// 最近一条带 peer 的用户输入；任务会话沿父会话找一层。TUI / HTTP / cron 的会话没有。
async fn attention_peer(core: &DurableCore, session: &SessionRecord) -> Option<ChannelPeer> {
    if let Some(peer) = session
        .source
        .as_deref()
        .and_then(|source| source.strip_prefix("chat:"))
        .and_then(ChannelPeer::parse)
    {
        return Some(peer);
    }
    if session.kind == SessionKind::Task {
        let parent = session.parent_session.as_deref()?;
        let parent = core.storage.session(parent).await.ok()??;
        if parent.kind == SessionKind::Task {
            return None;
        }
        return Box::pin(attention_peer(core, &parent)).await;
    }
    if session.kind != SessionKind::Main {
        return None;
    }
    let tail = core.tail_entry(session.current_branch).await.ok()??;
    let tree = load_tree(&*core.storage, &session.id).await.ok()?;
    tree.path(tail)
        .into_iter()
        .rev()
        .find_map(|record| match source_of_entry(record) {
            Source::Peer(peer) => Some(peer),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ack_document_round_trips_through_its_value() {
        let value = json!({ "abandoned": [3, 9, 4] });
        assert_eq!(
            decode_ack(&value).into_iter().collect::<Vec<_>>(),
            vec![3, 4, 9]
        );
        assert!(decode_ack(&json!({})).is_empty());
        assert!(decode_ack(&json!(null)).is_empty());
    }
}
