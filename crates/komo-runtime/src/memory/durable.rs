//! 记忆在新存储（Durable）上的两个接缝（D5 W3c，`docs/storage-durable-d5.md` §4 / §8）。
//!
//! - [`DurableSessionEvents`]：[`SessionEvents`] 的新存储实现。一个 Session 的 entry 与 submission 经
//!   kernel 的 `legacy_events` 投影成旧事件词汇，提取（`extract::Transcript`）一个字不改地读它。
//!   投影之后做两处适配，让旧提取读得懂：
//!   1. `RunAccepted` 在投影里没有正文（正文在放置输入的 entry 上，成了 run 的第一条 `MessageUser`）；
//!      旧提取把 `RunAccepted.text` 当用户陈述，所以把那条 `MessageUser` 的正文并回 `RunAccepted`
//!      （并且去掉那条 `MessageUser`，否则同一句话进两次）；
//!   2. 注入的记忆（`memory.injected`）：每个 Run 的召回块 entry（`komo.memory.recall`，`docs/memory.md`
//!      §9.4）由投影直接出成挂在它 run 上的 `MemoryInjected`，正文不进提取窗口。升级前的会话只在
//!      conversation 级 document `komo.memory`（[`MemoryDoc`]）里记了最后一块，合成一条 `MemoryInjected`
//!      挂在触发那次召回的 run 上（见 [`inject_memory_events`]）。
//! - [`DurableMemoryWork`]：[`MemoryWorkLog`] 的新存储实现。工作来源是**已结算的输入 submission**
//!   （经投影得到终态的 run）；处理游标记在 conversation 级 document `komo.memory_cursor`
//!   （[`CursorDoc`]），`finish` 走 engine 提交；领取只在进程内（[`HashSet`] 记着手里的 run），
//!   进程退出即放回——所以启动时的 `requeue_stuck` 只是清空这张表。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::durable::legacy_events::legacy_events;
use komo_kernel::durable::storage::{ConversationQuery, EntryQuery, SessionQuery, SubmissionQuery};
use komo_kernel::durable::{
    ConversationId, ConversationRecord, Cursor, DocumentAddress, DocumentScope, EntryRecord,
    InputState, ScanOrder, SessionKind as DurableSessionKind, SessionLifecycle, Storage,
    StorageError, SubmissionKind, SubmissionRecord, legacy_ids,
};
use komo_kernel::events::{EVENT_FORMAT_VERSION, Event, EventPayload, MemoryInjected};
use komo_kernel::traits::{Clock, StoreError};
use komo_kernel::types::agent::MemoryMode;
use komo_kernel::types::ids::{CronJobId, EventId, RunId, Seq, SessionId};
use komo_kernel::types::memory::{MemoryWork, RecallDiagnostics};
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::MemoryUse;
use komo_store::models::SessionKind;
use serde::{Deserialize, Serialize};

use super::work::{MemoryBatch, MemoryWorkItem, MemoryWorkLog};
use super::{MemoryError, SessionEvents};
use crate::durable::Engine;

const SCAN_PAGE: usize = 512;

// ------------------------------------------------------------------ komo.memory

/// 升级前的会话钉住的那一块记忆注入的 uses（conversation 级 document）。按 Run 召回之后不再写，
/// 旧会话里的那份只给提取与 run 详情补旧 Run 的注入（`docs/memory.md` §9.4）。
pub const MEMORY_DOC_KIND: &str = "komo.memory";
pub const MEMORY_DOC_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryDoc {
    pub uses: Vec<MemoryUse>,
    /// 召回诊断（W3c 加，旧文档没有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall: Option<RecallDiagnostics>,
    /// 召回发生的时刻（毫秒；W3c 加，旧文档 = 0）。run 详情与提取靠它认出"是哪条 run 触发了这次召回"：
    /// 开始时刻不晚于它的最后一条 run。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub at: i64,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

pub fn memory_address(conversation: ConversationId) -> DocumentAddress {
    DocumentAddress {
        kind: MEMORY_DOC_KIND.to_owned(),
        scope: DocumentScope::Conversation { conversation },
        key: None,
    }
}

/// 已提交的 `komo.memory`；读不出或形状不对都当没有（它只是审计与核对用的）。
pub async fn read_memory_doc(
    storage: &dyn Storage,
    conversation: ConversationId,
) -> Option<MemoryDoc> {
    let record = storage
        .find_document(&memory_address(conversation))
        .await
        .ok()??;
    serde_json::from_value(record.value).ok()
}

/// 把 `komo.memory` 合成的 `MemoryInjected` 挂进这个 conversation 的事件里。
///
/// 触发召回的 run = 开始时刻（`RunStarted.ts`）不晚于 `doc.at` 的最后一条（`at == 0` 的旧文档取第一条）。
/// 事件紧跟在它的 `RunAccepted` 后面、同 `seq`：之后的 run 沿用它（[`komo_kernel::events::injection_at`]
/// 取"结束前最近的一条"），之前的 run 没有。同一个 conversation 的 document 只有最后一份——换段重召回之后，
/// 更早的 run 看不到当时注入的那一块（新存储不留历史，最佳努力）。
pub fn inject_memory_events(events: &mut Vec<Event>, doc: &MemoryDoc) {
    let at = doc.at;
    let mut chosen: Option<RunId> = None;
    for event in events.iter() {
        if let (EventPayload::RunStarted(_), Some(run)) = (&event.payload, &event.run) {
            let millis = (event.ts.unix_timestamp_nanos() / 1_000_000) as i64;
            if at == 0 {
                chosen = Some(run.clone());
                break;
            }
            if millis > at {
                break;
            }
            chosen = Some(run.clone());
        }
    }
    let Some(run) = chosen else { return };
    let Some(position) = events.iter().position(|event| {
        matches!(event.payload, EventPayload::RunAccepted(_)) && event.run.as_ref() == Some(&run)
    }) else {
        return;
    };
    let anchor = &events[position];
    let injected = Event {
        v: EVENT_FORMAT_VERSION,
        seq: anchor.seq,
        event_id: EventId::from_raw(format!("memory-injected-{run}")),
        session: anchor.session.clone(),
        run: Some(run),
        ts: anchor.ts,
        payload: EventPayload::MemoryInjected(MemoryInjected {
            text: None,
            text_ref: None,
            uses: doc.uses.clone(),
            recall: doc.recall.clone(),
        }),
        entry: None,
    };
    events.insert(position + 1, injected);
}

/// 投影之后的适配（见模块文档第 1 条）：run 第一条 `MessageUser` 的正文并回 `RunAccepted`。
pub fn adapt_for_extraction(events: &mut Vec<Event>) {
    let mut starters: HashMap<RunId, String> = HashMap::new();
    let mut started: HashSet<RunId> = HashSet::new();
    let mut remove: HashSet<usize> = HashSet::new();
    for (index, event) in events.iter().enumerate() {
        match (&event.payload, &event.run) {
            (EventPayload::RunStarted(_), Some(run)) => {
                started.insert(run.clone());
            }
            (EventPayload::MessageUser(body), Some(run))
                if started.remove(run) && body.text.is_some() =>
            {
                starters.insert(run.clone(), body.text.clone().unwrap_or_default());
                remove.insert(index);
            }
            _ => {}
        }
    }
    if starters.is_empty() {
        return;
    }
    for event in events.iter_mut() {
        if let (EventPayload::RunAccepted(body), Some(run)) = (&mut event.payload, &event.run)
            && let Some(text) = starters.get(run)
        {
            body.text = Some(text.clone());
        }
    }
    let mut index = 0;
    events.retain(|_| {
        let keep = !remove.contains(&index);
        index += 1;
        keep
    });
}

// ------------------------------------------------------------------ 读 Session

fn io(error: StorageError) -> StoreError {
    StoreError::Io(error.to_string())
}

struct ConversationLog {
    conversation: ConversationRecord,
    entries: Vec<EntryRecord>,
    submissions: Vec<SubmissionRecord>,
}

async fn session_conversations(
    storage: &dyn Storage,
    session: &str,
) -> Result<Vec<ConversationRecord>, StoreError> {
    let query = ConversationQuery {
        session: Some(session.to_owned()),
        order: Some(ScanOrder::Ascending),
        ..ConversationQuery::default()
    };
    let mut all = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = storage
            .scan_conversations(&query, SCAN_PAGE, cursor.as_ref())
            .await
            .map_err(io)?;
        all.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(all),
        }
    }
}

async fn conversation_submissions(
    storage: &dyn Storage,
    conversation: ConversationId,
) -> Result<Vec<SubmissionRecord>, StoreError> {
    let query = SubmissionQuery {
        conversation: Some(conversation),
        status: None,
        order: Some(ScanOrder::Ascending),
    };
    let mut submissions = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = storage
            .scan_submissions(&query, SCAN_PAGE, cursor.as_ref())
            .await
            .map_err(io)?;
        submissions.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(submissions),
        }
    }
}

async fn load_conversation(
    storage: &dyn Storage,
    conversation: ConversationRecord,
) -> Result<ConversationLog, StoreError> {
    let mut query = EntryQuery::new(conversation.id);
    query.order = Some(ScanOrder::Ascending);
    // fork 出来的子 conversation 的可见历史带着祖先那一段；run 都在自己名下的 entry 里。
    query.min = conversation
        .parent
        .map(|parent| komo_kernel::durable::EntryId::new(parent.at.get() + 1));
    let mut entries = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = storage
            .scan_entries(&query, SCAN_PAGE, cursor.as_ref())
            .await
            .map_err(io)?;
        entries.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    let submissions = conversation_submissions(storage, conversation.id).await?;
    Ok(ConversationLog {
        conversation,
        entries,
        submissions,
    })
}

/// 一个 Session 的旧事件（按 `seq` 升序）与各 conversation 的原始记录。
async fn load_session(
    storage: &dyn Storage,
    session: &str,
) -> Result<(Vec<Event>, Vec<ConversationLog>), StoreError> {
    let mut events = Vec::new();
    let mut logs = Vec::new();
    for conversation in session_conversations(storage, session).await? {
        let log = load_conversation(storage, conversation).await?;
        let mut mine = legacy_events(session, &log.entries, &log.submissions);
        adapt_for_extraction(&mut mine);
        if let Some(doc) = read_memory_doc(storage, log.conversation.id).await {
            inject_memory_events(&mut mine, &doc);
        }
        events.extend(mine);
        logs.push(log);
    }
    // 稳定排序：同 seq 的合成事件留在它挂靠的 `RunAccepted` 后面。
    events.sort_by_key(|event| event.seq);
    Ok((events, logs))
}

// ------------------------------------------------------------------ SessionEvents

/// 新存储上的 [`SessionEvents`]。
pub struct DurableSessionEvents {
    storage: Arc<dyn Storage>,
}

impl DurableSessionEvents {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        DurableSessionEvents { storage }
    }
}

impl std::fmt::Debug for DurableSessionEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableSessionEvents")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SessionEvents for DurableSessionEvents {
    async fn events_of(&self, session: &SessionId) -> Result<Vec<Event>, MemoryError> {
        let (events, _) = load_session(&*self.storage, session.as_str()).await?;
        Ok(events)
    }
}

// ------------------------------------------------------------------ komo.memory_cursor

pub const CURSOR_DOC_KIND: &str = "komo.memory_cursor";
pub const CURSOR_DOC_VERSION: u32 = 1;

/// 一条 run 的处理进度。只记**不是"全新"**的：没有这一项 = pending + 游标 0。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCursor {
    pub work: MemoryWork,
    /// 处理到哪条 `Seq`（只进不退）。
    pub cursor: u64,
}

/// conversation 级 document：这个 conversation 里各 run 的记忆处理进度，键是 `RunId`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorDoc {
    #[serde(default)]
    pub runs: BTreeMap<String, RunCursor>,
}

pub fn cursor_address(conversation: ConversationId) -> DocumentAddress {
    DocumentAddress {
        kind: CURSOR_DOC_KIND.to_owned(),
        scope: DocumentScope::Conversation { conversation },
        key: None,
    }
}

/// 已提交的游标 document（没有或读不出 = 空）。
pub async fn read_cursor_doc(storage: &dyn Storage, conversation: ConversationId) -> CursorDoc {
    match storage.find_document(&cursor_address(conversation)).await {
        Ok(Some(record)) => serde_json::from_value(record.value).unwrap_or_default(),
        _ => CursorDoc::default(),
    }
}

// ------------------------------------------------------------------ MemoryWorkLog

/// 记忆模式的来源：会话的 Agent → 它此刻配置的 `(记忆模式, agent id)`。
///
/// 旧 Run 在受理时冻结 `RunSnapshot::memory_mode`；新存储没有快照，所以按**提取那一刻**的配置取
/// （偏离，留给 W4 / D7 决定要不要把它冻进 `komo.agent`）。
pub type MemoryModes = Arc<dyn Fn(Option<&str>) -> (MemoryMode, String) + Send + Sync>;

/// 新存储上的 [`MemoryWorkLog`]。
pub struct DurableMemoryWork {
    storage: Arc<dyn Storage>,
    engine: Arc<Engine>,
    clock: Arc<dyn Clock>,
    modes: MemoryModes,
    /// 本进程手里正在处理的 run（"领取"只在进程内）。
    claimed: Mutex<HashSet<RunId>>,
    /// Session → 上次评估时它已结算输入的最大 submission ID：评估后没有任何待处理的 run，
    /// 之后这个数不变就不必再投影它。
    quiet: Mutex<HashMap<String, u64>>,
}

impl std::fmt::Debug for DurableMemoryWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableMemoryWork").finish_non_exhaustive()
    }
}

impl DurableMemoryWork {
    pub fn new(
        storage: Arc<dyn Storage>,
        engine: Arc<Engine>,
        clock: Arc<dyn Clock>,
        modes: MemoryModes,
    ) -> Self {
        DurableMemoryWork {
            storage,
            engine,
            clock,
            modes,
            claimed: Mutex::new(HashSet::new()),
            quiet: Mutex::new(HashMap::new()),
        }
    }

    /// 一条 run 此刻的处理进度（给测试与诊断读；没有记录 = pending、游标 0）。
    pub async fn progress(&self, run: &RunId) -> Result<RunCursor, StoreError> {
        let submission = self.submission_of(run).await?;
        let doc = read_cursor_doc(&*self.storage, submission.conversation).await;
        Ok(doc.runs.get(run.as_str()).copied().unwrap_or(RunCursor {
            work: MemoryWork::Pending,
            cursor: 0,
        }))
    }

    async fn submission_of(&self, run: &RunId) -> Result<SubmissionRecord, StoreError> {
        let not_found = || StoreError::NotFound {
            what: format!("run {run}"),
        };
        let id = legacy_ids::submission_of(run).ok_or_else(not_found)?;
        self.storage
            .submission(id)
            .await
            .map_err(io)?
            .ok_or_else(not_found)
    }
}

/// 终态事件 → 这条 run 的状态。
fn terminal_state(payload: &EventPayload) -> Option<RunState> {
    match payload {
        EventPayload::RunCompleted(_) => Some(RunState::Completed),
        EventPayload::RunFailed(_) => Some(RunState::Failed),
        EventPayload::RunCancelled(_) => Some(RunState::Cancelled),
        _ => None,
    }
}

fn store_kind(kind: DurableSessionKind) -> SessionKind {
    match kind {
        DurableSessionKind::Main => SessionKind::Main,
        DurableSessionKind::Task => SessionKind::Task,
        // cron 会话在记忆这一层与普通会话一样；它的 run 靠来源（`PlanSource::Cron`）跳过。
        DurableSessionKind::Normal | DurableSessionKind::Cron => SessionKind::Normal,
    }
}

#[async_trait]
impl MemoryWorkLog for DurableMemoryWork {
    async fn claim(
        &self,
        idle: time::Duration,
        limit: usize,
    ) -> Result<Vec<MemoryBatch>, StoreError> {
        let now = self.clock.now();
        let mut out: Vec<MemoryBatch> = Vec::new();
        let mut cursor = None;
        // TODO：每拍扫全部 Session（`scan_sessions` 全表）；`quiet` 让没有新结算输入的 Session 只花
        // 一次 submission 扫描。长期要把"待处理"记成索引。
        'sessions: loop {
            let page = self
                .storage
                .scan_sessions(
                    &SessionQuery {
                        lifecycle: Some(SessionLifecycle::Active),
                        order: Some(ScanOrder::Ascending),
                    },
                    SCAN_PAGE,
                    cursor.as_ref(),
                )
                .await
                .map_err(io)?;
            for session in page.items {
                if out.len() >= limit {
                    break 'sessions;
                }
                if let Some(batch) = self.evaluate(&session, idle, now).await? {
                    out.push(batch);
                }
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(out)
    }

    async fn finish(&self, run: &RunId, work: MemoryWork, cursor: Seq) -> Result<(), StoreError> {
        let submission = self.submission_of(run).await?;
        let conversation = submission.conversation;
        let key = run.as_str().to_owned();
        let result = self
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    let address = cursor_address(conversation);
                    let mut doc: CursorDoc = match tx.document(&address).await? {
                        Some((_, value)) => serde_json::from_value(value).unwrap_or_default(),
                        None => CursorDoc::default(),
                    };
                    // 游标只进不退。
                    let before = doc.runs.get(&key).map_or(0, |entry| entry.cursor);
                    doc.runs.insert(
                        key,
                        RunCursor {
                            work,
                            cursor: before.max(cursor.0),
                        },
                    );
                    let value = serde_json::to_value(&doc).expect("CursorDoc 可序列化");
                    tx.put_document(address, CURSOR_DOC_VERSION, value).await
                })
            })
            .await;
        // 不管写没写成都放开：写失败 = 游标没推进，下一拍重新领取再处理（提取按证据幂等）。
        self.claimed.lock().expect("领取表").remove(run);
        result.map_err(|error| StoreError::Other(format!("记忆游标没写下去：{error}")))
    }

    async fn requeue_stuck(&self) -> Result<u64, StoreError> {
        // 领取只在进程内：新进程里它本来就是空的；热重启同进程时清掉上一代的。
        let mut claimed = self.claimed.lock().expect("领取表");
        let count = claimed.len() as u64;
        claimed.clear();
        self.quiet.lock().expect("静默表").clear();
        Ok(count)
    }
}

impl DurableMemoryWork {
    /// 评估一个 Session：它此刻有没有"空闲够久且还没处理完"的已结算 run。
    async fn evaluate(
        &self,
        session: &komo_kernel::durable::SessionRecord,
        idle: time::Duration,
        now: time::OffsetDateTime,
    ) -> Result<Option<MemoryBatch>, StoreError> {
        let conversations = session_conversations(&*self.storage, &session.id).await?;
        // 先只看 submission：有没有没结算的输入、已结算输入的最大 ID。
        let mut settled_max = 0u64;
        for conversation in &conversations {
            for submission in conversation_submissions(&*self.storage, conversation.id).await? {
                let SubmissionKind::Input { state } = &submission.kind else {
                    continue;
                };
                match state {
                    InputState::Queued | InputState::Placed { .. } => return Ok(None),
                    InputState::Done { .. } | InputState::Unanswered { .. } => {
                        settled_max = settled_max.max(submission.id.get());
                    }
                }
            }
        }
        if settled_max == 0 {
            return Ok(None);
        }
        if self
            .quiet
            .lock()
            .expect("静默表")
            .get(&session.id)
            .is_some_and(|seen| *seen >= settled_max)
        {
            return Ok(None);
        }

        let (events, logs) = load_session(&*self.storage, &session.id).await?;
        let mut docs: HashMap<ConversationId, CursorDoc> = HashMap::new();
        for log in &logs {
            docs.insert(
                log.conversation.id,
                read_cursor_doc(&*self.storage, log.conversation.id).await,
            );
        }
        // run → 所在 conversation 与它的第一个输入 submission。
        let mut owner: HashMap<u64, (ConversationId, &SubmissionRecord, &ConversationLog)> =
            HashMap::new();
        for log in &logs {
            for submission in &log.submissions {
                owner.insert(submission.id.get(), (log.conversation.id, submission, log));
            }
        }

        let source_agent = session.agent.as_deref();
        let (mode, agent_id) = (self.modes)(source_agent);
        let mut items: Vec<MemoryWorkItem> = Vec::new();
        let mut last_end: Option<time::OffsetDateTime> = None;
        let mut pending = false;
        for event in &events {
            let (Some(status), Some(run)) = (terminal_state(&event.payload), &event.run) else {
                continue;
            };
            last_end = Some(last_end.map_or(event.ts, |seen| seen.max(event.ts)));
            let Some(id) = legacy_ids::submission_of(run) else {
                continue;
            };
            let Some((conversation, submission, log)) = owner.get(&id.get()) else {
                continue;
            };
            let progress = docs
                .get(conversation)
                .and_then(|doc| doc.runs.get(run.as_str()))
                .copied();
            if matches!(
                progress,
                Some(RunCursor {
                    work: MemoryWork::Done | MemoryWork::Error,
                    ..
                })
            ) {
                continue;
            }
            pending = true;
            let from_cron = session.kind == DurableSessionKind::Cron
                || input_entry_kind(log, submission).is_some_and(|kind| kind == "cron.input");
            items.push(MemoryWorkItem {
                run: run.clone(),
                session: SessionId::from_raw(session.id.clone()),
                status,
                source: if from_cron {
                    PlanSource::Cron {
                        job: CronJobId::from_raw("cron"),
                        job_version: 0,
                    }
                } else {
                    PlanSource::Interactive {
                        session: SessionId::from_raw(session.id.clone()),
                    }
                },
                cursor: Seq(progress.map_or(0, |entry| entry.cursor)),
                frozen: Some((mode, agent_id.clone())),
            });
        }
        if !pending {
            self.quiet
                .lock()
                .expect("静默表")
                .insert(session.id.clone(), settled_max);
            return Ok(None);
        }
        // 会话空闲够久才领（§9.10）：最后一条 run 结束后过了 `idle`。
        if let Some(end) = last_end
            && now - end < idle
        {
            return Ok(None);
        }
        let mut claimed = self.claimed.lock().expect("领取表");
        items.retain(|item| !claimed.contains(&item.run));
        if items.is_empty() {
            return Ok(None);
        }
        for item in &items {
            claimed.insert(item.run.clone());
        }
        Ok(Some(MemoryBatch {
            session: SessionId::from_raw(session.id.clone()),
            kind: store_kind(session.kind),
            items,
        }))
    }
}

/// 一条输入 submission 放置出的 entry 的 kind（还没放置 = `None`）。
fn input_entry_kind<'a>(
    log: &'a ConversationLog,
    submission: &SubmissionRecord,
) -> Option<&'a str> {
    let SubmissionKind::Input { state } = &submission.kind else {
        return None;
    };
    let entry = match state {
        InputState::Placed { entry } | InputState::Done { entry, .. } => Some(*entry),
        InputState::Unanswered { entry, .. } => *entry,
        InputState::Queued => None,
    }?;
    log.entries
        .iter()
        .find(|candidate| candidate.id == entry)
        .map(|entry| entry.kind.as_str())
}
