//! 提交订阅 → SSE（D5 W2b，`docs/storage-durable-d5.md` §5）。
//!
//! 协议冻结，客户端读的还是旧事件。这里把 `Line` 上每次已提交的批次（[`Publication`]）投影成旧事件词汇，
//! 经 [`EventHub::publish_event`] 推给订阅这个 Session 的 SSE 连接；`komo.live` 里节流提交的部分回复
//! 产出只在 SSE 上的 `AssistantDelta`。
//!
//! # 结构
//!
//! - `subscribe_commits` 的回调在提交线上**同步**执行，所以只把批次里关心的几种写入克隆进一条
//!   无界 mpsc 就返回；其余全在 feed 任务里做；
//! - feed 任务维护三张缓存：conversation → Session、`komo.live` 的 document ID → conversation、
//!   每个 Session 一个 [`Projection`]（懒建：第一次碰到这个 Session 时从存储补读到当前，再接增量）；
//! - 补读与直播是同一个纯函数（W1a 的 [`Projection`]）：补读 = 从会话开头把所有记录喂一遍，直播 =
//!   每个批次里的 entry 与 submission 喂一遍。
//!
//! # 懒建时哪些帧要推
//!
//! 懒建那一刻读到的存储状态**不早于**触发它的那个批次，可能还含着之后几个批次的记录（它们的批次
//! 还在队列里）。所以懒建产出的帧里：
//!
//! - 记录 ID 不大于 `floor`（feed 之前就有的、或前面批次里已经见过的记录）**不推**——订阅者连上来时
//!   自己补读过它们，再推一遍会让派生的 `RunStatus` 帧倒回旧状态；
//! - 其余**都推**，包括那些还在队列里的后续记录：它们的批次到达时 entry 被投影当作重放丢掉、
//!   submission 早已见过，所以不会重复。
//!
//! `floor` 靠"ID 全局单调、都是在提交线上铸的"：feed 起来时铸一个新 ID 当初值，之后每个批次的所有记录
//! ID（entry / submission / task / conversation / document）都抬高它。新记录的 ID 一定大于见过的一切。
//!
//! # 并发读
//!
//! 补读不进提交线：读已提交状态（`Storage` 的读口），所以补读再多也不占提交线。
//! `TursoStorage` 的读口目前只有一条连接（`docs/storage-durable-d5.md` §9 的风险），
//! 并发补读的开销由 `concurrent_backfills_do_not_stall_commits` 量。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use komo_kernel::durable::legacy_events::{Projection, Record, legacy_events};
use komo_kernel::durable::legacy_ids::{record_of, run_id};
use komo_kernel::durable::storage::SubmissionQuery;
use komo_kernel::durable::{
    AssistantMessage, ContentBlock, ConversationId, Cursor, DocumentId, DocumentScope, EntityId,
    EntryRecord, ScanOrder, Storage, StorageWrite, SubmissionRecord,
};
use komo_kernel::events::Event;
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::traits::Shutdown;
use komo_kernel::types::ids::{Seq, SessionId};
use komo_runtime::durable::Publication;
use komo_runtime::durable::harness::live::{LIVE_KIND, LiveState};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

use super::DurableCore;
use super::sessions::SessionError;
use super::tree::load_tree;
use crate::sse::SharedHub;

/// 内存里同时留多少个 Session 的投影；超出就丢最久没动的（下次碰到时重新补读，不丢帧）。
const MAX_SESSIONS: usize = 128;
/// 补读时每页读多少条 submission。
const SCAN_PAGE: usize = 512;
/// 同时进行的补读读存储的个数上限。`TursoStorage` 的读口只有一条连接（一把互斥锁），提交线上每次提交
/// 也要读好几次；补读不设限的话，20 路并发补读会让每次提交的每次读都排在 20 个补读的查询后面
/// （冒烟里提交慢了近 30 倍）。限成 2 路，提交的读最多排在两条查询后面；补读本身只是排队等，
/// 不丢也不乱序。换了连接池（W5 之前，见 §9 的风险）就可以放宽。
const READ_PERMITS: usize = 2;
/// 轮询停机旗子的间隔（`Shutdown` 只是个标志，不能 await）。
const SHUTDOWN_POLL: Duration = Duration::from_millis(250);

// ------------------------------------------------------------------- 补读

/// 一个 Session 的全部 entry 与 submission（补读与懒建的输入）。
#[derive(Debug, Clone, Default)]
pub struct SessionRecords {
    pub entries: Vec<EntryRecord>,
    pub submissions: Vec<SubmissionRecord>,
}

/// 读一个 Session 全部 conversation 自己的 entry 与全部 submission（已提交状态，不进提交线）。
///
/// **先读 entry 再读 submission**：两次读之间可能插进新的提交，让 submission 比 entry 新——
/// 这没关系（缺的 entry 会随它自己的批次到达）；反过来（entry 比 submission 新）会让输入 entry
/// 找不到放置它的 submission，被当成普通 entry 投影，所以不能先读 submission。
pub async fn read_session_records(
    storage: &dyn Storage,
    session: &str,
) -> Result<SessionRecords, SessionError> {
    static GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(READ_PERMITS);
    let _permit = GATE.acquire().await.expect("补读闸门不会关");
    let tree = load_tree(storage, session).await?;
    let mut records = SessionRecords::default();
    let mut conversations = Vec::with_capacity(tree.branches.len());
    for branch in tree.branches {
        conversations.push(branch.conversation.id);
        records.entries.extend(branch.entries);
    }
    for conversation in conversations {
        let query = SubmissionQuery {
            conversation: Some(conversation),
            status: None,
            order: Some(ScanOrder::Ascending),
        };
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = storage
                .scan_submissions(&query, SCAN_PAGE, cursor.as_ref())
                .await?;
            records.submissions.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
    }
    Ok(records)
}

/// 补读：这个 Session 里 `seq > after` 的全部事件（升序）。
///
/// 补读要从会话开头投影（run 的归属靠顺序折出来），游标只用来丢掉已经给过的帧。
pub async fn backfill(
    storage: &dyn Storage,
    session: &str,
    after: Seq,
) -> Result<Vec<Event>, SessionError> {
    let records = read_session_records(storage, session).await?;
    let mut events = legacy_events(session, &records.entries, &records.submissions);
    events.retain(|event| event.seq > after);
    Ok(events)
}

// ------------------------------------------------------------------- 启动

/// 已经起了 feed 的 `DurableCore`（弱引用）。重复 `spawn` 同一个 core 不再起第二份，否则每一帧推两遍。
static RUNNING: Mutex<Vec<Weak<DurableCore>>> = Mutex::new(Vec::new());

/// 起 feed：订阅提交线、开一个任务把批次投影成 SSE 帧。返回 `false` 表示这个 core 已经有一份在跑。
///
/// 在 `service::start` 里 `durable::background::spawn` 之后 `.await` 它（见 W2b 报告）。返回时订阅已经
/// 生效，之后的提交一定会被推出去。任务在线封口（关闭或失败）或 `shutdown` 举起时退出。
pub async fn spawn(core: &Arc<DurableCore>, hub: &SharedHub, shutdown: &Shutdown) -> bool {
    {
        let mut running = RUNNING.lock().unwrap_or_else(PoisonError::into_inner);
        running.retain(|weak| weak.strong_count() > 0);
        if running
            .iter()
            .any(|weak| std::ptr::eq(weak.as_ptr(), Arc::as_ptr(core)))
        {
            return false;
        }
        running.push(Arc::downgrade(core));
    }
    let storage = Arc::clone(&core.storage);
    // 先铸初值再订阅：铸出来的 ID 比此前的一切都大。夹在两步之间的提交（实际不存在：feed 在
    // Gateway 接受流量之前起）不会被投递，但它们的记录 ID 大于初值，下一次碰到同一个 Session
    // 懒建时会补推。
    let floor = match storage.mint_id().await {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(%error, "SSE feed 起不来：铸不出初始 ID");
            return false;
        }
    };
    let (sender, receiver) = mpsc::unbounded_channel::<Publication>();
    let line = core.engine.line().clone();
    let subscription = line.subscribe_commits(move |publication| {
        // 回调在提交线上同步执行：只克隆关心的写入，别的（task、session 行）不用。
        let writes: Vec<StorageWrite> = publication
            .writes
            .iter()
            .filter(|write| !matches!(write, StorageWrite::Task(_) | StorageWrite::Session(_)))
            .cloned()
            .collect();
        if !writes.is_empty() {
            let _ = sender.send(Publication {
                seq: publication.seq,
                writes,
            });
        }
    });
    let feed = Feed::new(storage, Arc::clone(hub), floor);
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        feed.run(receiver, line, shutdown).await;
        subscription.dispose();
    });
    true
}

// ------------------------------------------------------------------- feed 任务

/// 一个 Session 的投影与它推到哪了。
struct SessionFeed {
    projection: Projection,
    /// 这个 Session 推过（或补读见过）的最大 seq，`AssistantDelta` 用它当 SSE 的 `id:`。
    last_seq: u64,
    touched: u64,
}

struct Feed {
    storage: Arc<dyn Storage>,
    hub: SharedHub,
    /// conversation → Session（`None`：不属于任何 komo Session，没有事件可推）。
    conversations: HashMap<ConversationId, Option<String>>,
    /// document ID → 它所属的 conversation（`None`：不是 `komo.live`）。
    documents: HashMap<DocumentId, Option<ConversationId>>,
    sessions: HashMap<String, SessionFeed>,
    /// 每个 conversation 上次发出的部分回复文字。
    partials: HashMap<ConversationId, String>,
    /// 见过的最大记录 ID（见模块文档）。
    floor: u64,
    tick: u64,
}

impl Feed {
    fn new(storage: Arc<dyn Storage>, hub: SharedHub, floor: u64) -> Self {
        Feed {
            storage,
            hub,
            conversations: HashMap::new(),
            documents: HashMap::new(),
            sessions: HashMap::new(),
            partials: HashMap::new(),
            floor,
            tick: 0,
        }
    }

    async fn run(
        mut self,
        mut receiver: mpsc::UnboundedReceiver<Publication>,
        line: komo_runtime::durable::Line,
        shutdown: Shutdown,
    ) {
        let sealed = line.sealed();
        tokio::pin!(sealed);
        let mut poll = tokio::time::interval(SHUTDOWN_POLL);
        loop {
            tokio::select! {
                biased;
                publication = receiver.recv() => match publication {
                    Some(publication) => self.handle(publication).await,
                    None => return,
                },
                () = &mut sealed => {
                    // 关闭开始：把已经排进队列的批次处理完就走（已受理的提交仍会结算并发布）。
                    while let Ok(publication) = receiver.try_recv() {
                        self.handle(publication).await;
                    }
                    return;
                }
                _ = poll.tick() => {
                    if shutdown.is_cancelled() {
                        return;
                    }
                }
            }
        }
    }

    /// 处理一个已提交批次。
    async fn handle(&mut self, publication: Publication) {
        self.tick += 1;
        let mut highest = self.floor;
        let mut bump = |id: u64| highest = highest.max(id);

        // 1. 先登记 conversation 与 document（同一批里后面的写入要用）。
        let mut live_values: Vec<(DocumentId, &Value)> = Vec::new();
        let mut purged: Vec<ConversationId> = Vec::new();
        for write in &publication.writes {
            match write {
                StorageWrite::Conversation(record) => {
                    bump(record.id.raw());
                    self.conversations.insert(record.id, record.session.clone());
                }
                StorageWrite::DocumentCreate(create) => {
                    bump(create.id.raw());
                    let live = live_conversation(&create.address.kind, &create.address.scope);
                    self.documents.insert(create.id, live);
                    if live.is_some() {
                        live_values.push((create.id, &create.value));
                    }
                }
                StorageWrite::DocumentChange { id, value, .. } => live_values.push((*id, value)),
                StorageWrite::DocumentRetire { id } => {
                    if let Some(Some(conversation)) = self.documents.remove(id) {
                        self.partials.remove(&conversation);
                    }
                }
                StorageWrite::Entry(entry) => bump(entry.id.raw()),
                StorageWrite::Submission(submission) => bump(submission.id.raw()),
                StorageWrite::Task(task) => bump(task.id.raw()),
                StorageWrite::PurgeConversations { conversations } => {
                    purged.extend(conversations.iter().copied());
                }
                StorageWrite::Session(_) => {}
            }
        }

        // 2. entry 与 submission 按 Session 归组。
        let mut by_session: BTreeMap<String, (Vec<&EntryRecord>, Vec<&SubmissionRecord>)> =
            BTreeMap::new();
        for write in &publication.writes {
            match write {
                StorageWrite::Entry(entry) => {
                    if let Some(session) = self.session_of(entry.conversation).await {
                        by_session.entry(session).or_default().0.push(entry);
                    }
                }
                StorageWrite::Submission(submission) => {
                    if let Some(session) = self.session_of(submission.conversation).await {
                        by_session.entry(session).or_default().1.push(submission);
                    }
                }
                _ => {}
            }
        }
        let floor_before = self.floor;
        for (session, (entries, submissions)) in by_session {
            if self.sessions.contains_key(&session) {
                let records: Vec<Record<'_>> = entries
                    .into_iter()
                    .map(Record::Entry)
                    .chain(submissions.into_iter().map(Record::Submission))
                    .collect();
                let hub = Arc::clone(&self.hub);
                if let Some(feed) = self.sessions.get_mut(&session) {
                    feed.touched = self.tick;
                    for event in feed.projection.project(&records) {
                        feed.last_seq = feed.last_seq.max(event.seq.0);
                        hub.publish_event(&event);
                    }
                }
            } else {
                // 懒建：存储里已经有这批记录（甚至更多），投影整个 Session，只推 `floor` 之后的帧。
                self.seed(&session, floor_before).await;
            }
        }

        // 3. `komo.live` 的部分回复 → AssistantDelta。
        for (id, value) in live_values {
            self.live_changed(id, value).await;
        }

        // 4. 清理被 purge 的 conversation。
        for conversation in purged {
            if let Some(Some(session)) = self.conversations.remove(&conversation) {
                self.sessions.remove(&session);
            }
            self.partials.remove(&conversation);
            self.documents
                .retain(|_, owner| *owner != Some(conversation));
        }

        self.floor = highest;
        self.evict();
    }

    /// 这个 conversation 属于哪个 Session；缓存没有就读存储。
    async fn session_of(&mut self, conversation: ConversationId) -> Option<String> {
        if let Some(session) = self.conversations.get(&conversation) {
            return session.clone();
        }
        match self.storage.conversation(conversation).await {
            Ok(Some(record)) => {
                self.conversations
                    .insert(conversation, record.session.clone());
                record.session
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%conversation, %error, "SSE feed 读不到 conversation，这批不推");
                None
            }
        }
    }

    /// 懒建一个 Session 的投影并推 `floor` 之后的帧。读不到（会话没了、存储出错）就不建，
    /// 下一个触到它的批次再试。
    async fn seed(&mut self, session: &str, floor: u64) {
        let records = match read_session_records(&*self.storage, session).await {
            Ok(records) => records,
            Err(SessionError::NotFound(_)) => return,
            Err(error) => {
                tracing::warn!(session, %error, "SSE feed 补读会话失败，这批不推");
                return;
            }
        };
        let mut projection = Projection::new(session);
        let all: Vec<Record<'_>> = records
            .entries
            .iter()
            .map(Record::Entry)
            .chain(records.submissions.iter().map(Record::Submission))
            .collect();
        let frames = projection.project(&all);
        let last_seq = frames.last().map_or(0, |event| event.seq.0);
        for event in &frames {
            if record_of(event.seq) > floor {
                self.hub.publish_event(event);
            }
        }
        self.sessions.insert(
            session.to_owned(),
            SessionFeed {
                projection,
                last_seq,
                touched: self.tick,
            },
        );
    }

    /// 一个 document 的值变了：是 `komo.live` 就看有没有新的部分回复。
    async fn live_changed(&mut self, id: DocumentId, value: &Value) {
        let conversation = match self.documents.get(&id) {
            Some(owner) => *owner,
            None => {
                let owner = match self.storage.document(id).await {
                    Ok(Some(record)) => {
                        live_conversation(&record.address.kind, &record.address.scope)
                    }
                    Ok(None) => None,
                    Err(error) => {
                        tracing::warn!(document = %id, %error, "SSE feed 读不到 document");
                        return;
                    }
                };
                self.documents.insert(id, owner);
                owner
            }
        };
        let Some(conversation) = conversation else {
            return;
        };
        let partial = partial_of(value);
        let Some(partial) = partial else {
            // 没有进行中的部分回复（一轮结束、换了一轮、run 收尾）：下次从头算。
            self.partials.remove(&conversation);
            return;
        };
        let last = self.partials.get(&conversation).map_or("", String::as_str);
        let Some(delta) = delta_of(last, &partial.text) else {
            return;
        };
        let delta = delta.to_owned();
        self.partials.insert(conversation, partial.text);
        let Some(session) = self.session_of(conversation).await else {
            return;
        };
        if !self.sessions.contains_key(&session) {
            // 重启后一条在途 run 的第一个批次就是进度：先懒建，拿到 `last_seq`。
            let floor = self.floor;
            self.seed(&session, floor).await;
        }
        let id = self
            .sessions
            .get(&session)
            .map_or(Seq::ZERO, |feed| Seq(feed.last_seq));
        self.hub.publish(SseFrame {
            id,
            session: SessionId::from_raw(session),
            event: SseEvent::AssistantDelta {
                run: partial.run,
                round: partial.round,
                text: delta,
            },
        });
    }

    /// 投影留太多就丢最久没动的。
    fn evict(&mut self) {
        while self.sessions.len() > MAX_SESSIONS {
            let Some(oldest) = self
                .sessions
                .iter()
                .min_by_key(|(_, feed)| feed.touched)
                .map(|(session, _)| session.clone())
            else {
                return;
            };
            self.sessions.remove(&oldest);
        }
    }
}

/// `komo.live` 的地址 → 它所在的 conversation。
fn live_conversation(kind: &str, scope: &DocumentScope) -> Option<ConversationId> {
    match scope {
        DocumentScope::Conversation { conversation } if kind == LIVE_KIND => Some(*conversation),
        _ => None,
    }
}

/// `komo.live` 里正在节流提交的那一条部分回复。
struct Partial {
    run: komo_kernel::types::ids::RunId,
    /// 旧事件里的轮次：这条 run 已经完成的轮数 + 1（与 `MessageAssistant.round` 同一个数）。
    round: u32,
    text: String,
}

/// 从 `komo.live` 整值里取出部分回复的文字；没有 run、没有部分回复或还没有文字就是 `None`。
fn partial_of(value: &Value) -> Option<Partial> {
    let live = LiveState::deserialize(value).ok()?;
    let run = live.run?;
    let first = *run.inputs.first()?;
    let message = live.generation?.message?;
    let message: AssistantMessage = serde_json::from_value(message).ok()?;
    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if text.is_empty() {
        return None;
    }
    Some(Partial {
        run: run_id(first),
        round: run.rounds + 1,
        text,
    })
}

/// 与上次发出的部分回复比较：新文字以上次的为前缀就取后缀，前缀对不上从头发，没变化不发。
pub fn delta_of<'a>(last: &str, now: &'a str) -> Option<&'a str> {
    if now == last {
        return None;
    }
    Some(now.strip_prefix(last).unwrap_or(now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delta_is_the_suffix_when_the_prefix_matches_and_the_whole_text_otherwise() {
        assert_eq!(delta_of("", "你好"), Some("你好"));
        assert_eq!(delta_of("你好", "你好吗"), Some("吗"));
        assert_eq!(delta_of("你好吗", "你好吗"), None);
        assert_eq!(delta_of("你好吗", "我换了说法"), Some("我换了说法"));
        // 缩短（重试后从头来）也算对不上。
        assert_eq!(delta_of("你好吗", "你好"), Some("你好"));
    }
}
