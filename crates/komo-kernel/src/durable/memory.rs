//! `Storage` 的内存参考实现（跨 crate 测试替身，取代今天的 `MemLedger`；设计 §3.7）。
//!
//! 一把 `std::sync::Mutex` 护住整个状态，所有 future 都立即就绪，不依赖 tokio。写入时先
//! 整批校验、再整批应用，所以失败的提交什么都没改；读出的都是独立拷贝。

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::fork::{ForkError, fork_chain, is_visible, visible_entries};
use super::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use super::records::{
    ConversationRecord, DocumentAddress, DocumentRecord, EntryRecord, SessionRecord,
    SubmissionRecord, TaskRecord,
};
use super::storage::{
    ConversationQuery, Cursor, DocumentQuery, EntryQuery, MAX_ID, Page, ScanOrder, ScanStart,
    SessionCursor, SessionPage, SessionQuery, Storage, StorageError, StorageResult, StorageWrite,
    StoredEntry, SubmissionQuery, TaskQuery, check_limit, page_of, scan_start, session_page_of,
    session_scan_start,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Conversation,
    Entry,
    Task,
    Submission,
    Document,
}

impl Table {
    fn name(self) -> &'static str {
        match self {
            Table::Conversation => "conversation",
            Table::Entry => "entry",
            Table::Task => "task",
            Table::Submission => "submission",
            Table::Document => "document",
        }
    }
}

/// 一批里对一个 document 的动作。
#[derive(Default)]
struct DocumentAction {
    create: Option<DocumentAddress>,
    content: Option<(u32, serde_json::Value)>,
    retire: bool,
}

struct State {
    closed: bool,
    next_id: u64,
    next_seq: u64,
    tables: HashMap<u64, Table>,
    conversations: BTreeMap<ConversationId, ConversationRecord>,
    /// 每个 conversation 自己的 entry，按 ID 升序。
    entries: HashMap<ConversationId, Vec<EntryRecord>>,
    entry_index: HashMap<EntryId, (ConversationId, Seq)>,
    tasks: BTreeMap<TaskId, TaskRecord>,
    submissions: BTreeMap<SubmissionId, SubmissionRecord>,
    requests: HashMap<(ConversationId, String), SubmissionId>,
    documents: BTreeMap<DocumentId, DocumentRecord>,
    sessions: BTreeMap<String, SessionRecord>,
}

/// 内存里的 `Storage`。
pub struct MemoryStorage {
    state: Mutex<State>,
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                closed: false,
                // 1 留给根 conversation。
                next_id: 2,
                next_seq: 1,
                tables: HashMap::new(),
                conversations: BTreeMap::new(),
                entries: HashMap::new(),
                entry_index: HashMap::new(),
                tasks: BTreeMap::new(),
                submissions: BTreeMap::new(),
                requests: HashMap::new(),
                documents: BTreeMap::new(),
                sessions: BTreeMap::new(),
            }),
        }
    }

    fn open(&self) -> StorageResult<MutexGuard<'_, State>> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return Err(StorageError::Closed);
        }
        Ok(state)
    }
}

fn fork_error(error: ForkError) -> StorageError {
    match error {
        ForkError::UnknownConversation(id) => {
            StorageError::request(format!("unknown conversation: {id}"))
        }
        ForkError::Cycle(id) => StorageError::Backend(format!("fork cycle at conversation {id}")),
    }
}

/// 按扫描方向从有序 map 里取最多 `take` 条满足条件的值。
fn scan_map<K: Ord + Copy, V: Clone>(
    map: &BTreeMap<K, V>,
    start: ScanStart,
    key: impl Fn(u64) -> K,
    take: usize,
    keep: impl Fn(&V) -> bool,
) -> Vec<V> {
    let after = start.after.map(key);
    match start.order {
        ScanOrder::Ascending => {
            let lower = after.map_or(Bound::Unbounded, Bound::Excluded);
            map.range((lower, Bound::Unbounded))
                .map(|(_, value)| value)
                .filter(|value| keep(value))
                .take(take)
                .cloned()
                .collect()
        }
        ScanOrder::Descending => {
            let upper = after.map_or(Bound::Unbounded, Bound::Excluded);
            map.range((Bound::Unbounded, upper))
                .rev()
                .map(|(_, value)| value)
                .filter(|value| keep(value))
                .take(take)
                .cloned()
                .collect()
        }
    }
}

impl State {
    /// 校验整批并算出 document 动作；不改状态。
    fn check(&self, writes: &[StorageWrite]) -> StorageResult<HashMap<DocumentId, DocumentAction>> {
        let mut claimed: HashMap<u64, Table> = HashMap::new();
        let mut requests: HashMap<(ConversationId, &str), SubmissionId> = HashMap::new();
        let mut actions: HashMap<DocumentId, DocumentAction> = HashMap::new();
        for write in writes {
            let (table, id) = match write {
                StorageWrite::Conversation(record) => (Table::Conversation, record.id.get()),
                StorageWrite::Entry(record) => (Table::Entry, record.id.get()),
                StorageWrite::Task(record) => (Table::Task, record.id.get()),
                StorageWrite::Submission(record) => (Table::Submission, record.id.get()),
                StorageWrite::DocumentCreate(create) => (Table::Document, create.id.get()),
                StorageWrite::DocumentChange { .. } | StorageWrite::DocumentRetire { .. } => {
                    self.collect_document_action(write, &mut actions)?;
                    continue;
                }
                StorageWrite::Session(_) => continue,
            };
            if id > MAX_ID {
                return Err(StorageError::rejected(format!("ID {id} is out of range")));
            }
            let existing = self.tables.get(&id).copied();
            let earlier = claimed.get(&id).copied();
            // conversation / entry / document 创建后不可变：ID 已在库里或同批已写过都不行。
            // task / submission 整条替换，同一张表里重写是正常的。
            let immutable = matches!(table, Table::Conversation | Table::Entry | Table::Document);
            if let Some(existing) = existing
                && (immutable || existing != table)
            {
                return Err(StorageError::rejected(format!(
                    "ID {id} already belongs to {}",
                    existing.name()
                )));
            }
            if let Some(earlier) = earlier
                && (immutable || earlier != table)
            {
                return Err(StorageError::rejected(format!(
                    "ID {id} is written more than once ({} and {})",
                    earlier.name(),
                    table.name()
                )));
            }
            claimed.insert(id, table);
            if matches!(write, StorageWrite::DocumentCreate(_)) {
                self.collect_document_action(write, &mut actions)?;
            }
            if let StorageWrite::Submission(record) = write
                && let Some(request) = record.request_id.as_deref()
            {
                let key = (record.conversation, request);
                let holder = requests.get(&key).copied().or_else(|| {
                    self.requests
                        .get(&(record.conversation, request.to_owned()))
                        .copied()
                });
                if holder.is_some_and(|holder| holder != record.id) {
                    return Err(StorageError::rejected(format!(
                        "request ID {request:?} is already used in conversation {}",
                        record.conversation
                    )));
                }
                requests.insert(key, record.id);
            }
        }
        self.check_document_addresses(&actions)?;
        Ok(actions)
    }

    fn collect_document_action(
        &self,
        write: &StorageWrite,
        actions: &mut HashMap<DocumentId, DocumentAction>,
    ) -> StorageResult<()> {
        let id = match write {
            StorageWrite::DocumentCreate(create) => create.id,
            StorageWrite::DocumentChange { id, .. } | StorageWrite::DocumentRetire { id } => *id,
            _ => return Ok(()),
        };
        let action = actions.entry(id).or_default();
        match write {
            StorageWrite::DocumentCreate(create) => {
                if action.create.is_some() || action.content.is_some() {
                    return Err(StorageError::rejected(format!(
                        "document {id} has more than one content command"
                    )));
                }
                action.create = Some(create.address.clone());
                action.content = Some((create.version, create.value.clone()));
            }
            StorageWrite::DocumentChange { version, value, .. } => {
                if action.content.is_some() {
                    return Err(StorageError::rejected(format!(
                        "document {id} has more than one content command"
                    )));
                }
                action.content = Some((*version, value.clone()));
            }
            StorageWrite::DocumentRetire { .. } => {
                if action.retire {
                    return Err(StorageError::rejected(format!(
                        "document {id} is retired more than once"
                    )));
                }
                action.retire = true;
            }
            _ => {}
        }
        Ok(())
    }

    fn check_document_addresses(
        &self,
        actions: &HashMap<DocumentId, DocumentAction>,
    ) -> StorageResult<()> {
        let mut live: HashMap<&DocumentAddress, i32> = HashMap::new();
        for (id, action) in actions {
            let existing = self.documents.get(id);
            match (&action.create, existing) {
                (None, None) => {
                    return Err(StorageError::rejected(format!("unknown document: {id}")));
                }
                (Some(_), Some(_)) => {
                    return Err(StorageError::rejected(format!(
                        "document {id} already exists"
                    )));
                }
                (None, Some(existing)) if !existing.is_alive() => {
                    return Err(StorageError::rejected(format!("document {id} is retired")));
                }
                _ => {}
            }
            let address = action
                .create
                .as_ref()
                .or_else(|| existing.map(|existing| &existing.address))
                .expect("a document action has a create or an existing record");
            let count = live.entry(address).or_insert_with(|| {
                i32::from(
                    self.documents
                        .values()
                        .any(|doc| doc.is_alive() && &doc.address == address),
                )
            });
            if action.retire && existing.is_some() {
                *count -= 1;
            }
            if action.create.is_some() && !action.retire {
                *count += 1;
            }
        }
        if live.values().any(|count| *count > 1) {
            return Err(StorageError::rejected(
                "document address already has a current incarnation",
            ));
        }
        Ok(())
    }

    fn apply(
        &mut self,
        writes: &[StorageWrite],
        actions: HashMap<DocumentId, DocumentAction>,
    ) -> Seq {
        let seq = Seq::new(self.next_seq);
        self.next_seq += 1;
        for write in writes {
            match write {
                StorageWrite::Conversation(record) => {
                    self.claim(record.id.get(), Table::Conversation);
                    self.conversations.insert(record.id, record.clone());
                }
                StorageWrite::Entry(record) => {
                    self.claim(record.id.get(), Table::Entry);
                    let entries = self.entries.entry(record.conversation).or_default();
                    let at = entries.partition_point(|entry| entry.id < record.id);
                    entries.insert(at, record.clone());
                    self.entry_index
                        .insert(record.id, (record.conversation, seq));
                }
                StorageWrite::Task(record) => {
                    self.claim(record.id.get(), Table::Task);
                    self.tasks.insert(record.id, record.clone());
                }
                StorageWrite::Submission(record) => {
                    self.claim(record.id.get(), Table::Submission);
                    if let Some(previous) = self.submissions.get(&record.id)
                        && let Some(request) = &previous.request_id
                    {
                        self.requests
                            .remove(&(previous.conversation, request.clone()));
                    }
                    if let Some(request) = &record.request_id {
                        self.requests
                            .insert((record.conversation, request.clone()), record.id);
                    }
                    self.submissions.insert(record.id, record.clone());
                }
                StorageWrite::Session(record) => {
                    self.sessions.insert(record.id.clone(), record.clone());
                }
                StorageWrite::DocumentCreate(_)
                | StorageWrite::DocumentChange { .. }
                | StorageWrite::DocumentRetire { .. } => {}
            }
        }
        for (id, action) in actions {
            if let Some(address) = action.create {
                let (version, value) = action.content.expect("a create carries content");
                self.claim(id.get(), Table::Document);
                self.documents.insert(
                    id,
                    DocumentRecord {
                        id,
                        address,
                        version,
                        value,
                        created_at: seq,
                        changed_at: seq,
                        retired_at: None,
                    },
                );
            } else if let Some((version, value)) = action.content {
                let record = self.documents.get_mut(&id).expect("checked to exist");
                record.version = version;
                record.value = value;
                record.changed_at = seq;
            }
            if action.retire {
                self.documents.get_mut(&id).expect("exists").retired_at = Some(seq);
            }
        }
        seq
    }

    fn claim(&mut self, id: u64, table: Table) {
        self.tables.insert(id, table);
        self.next_id = self.next_id.max(id.saturating_add(1));
    }

    fn chain(
        &self,
        conversation: ConversationId,
        min: Option<EntryId>,
        max: Option<EntryId>,
    ) -> StorageResult<Vec<super::fork::ForkSegment>> {
        fork_chain(conversation, |id| self.conversations.get(&id), min, max).map_err(fork_error)
    }

    fn entries_of(&self, id: ConversationId) -> &[EntryRecord] {
        self.entries.get(&id).map_or(&[], Vec::as_slice)
    }

    fn stored_entry(&self, id: EntryId) -> Option<StoredEntry> {
        let (conversation, commit_seq) = *self.entry_index.get(&id)?;
        let entries = self.entries_of(conversation);
        let at = entries.partition_point(|entry| entry.id < id);
        Some(StoredEntry {
            entry: entries[at].clone(),
            commit_seq,
        })
    }
}

#[async_trait::async_trait]
impl Storage for MemoryStorage {
    async fn commit(&self, writes: &[StorageWrite]) -> StorageResult<Seq> {
        let mut state = self.open()?;
        let actions = state.check(writes)?;
        Ok(state.apply(writes, actions))
    }

    async fn mint_id(&self) -> StorageResult<u64> {
        let mut state = self.open()?;
        if state.next_id > MAX_ID {
            return Err(StorageError::IdSpaceExhausted);
        }
        let id = state.next_id;
        state.next_id += 1;
        Ok(id)
    }

    async fn conversation(&self, id: ConversationId) -> StorageResult<Option<ConversationRecord>> {
        Ok(self.open()?.conversations.get(&id).cloned())
    }

    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<ConversationRecord>> {
        check_limit(limit)?;
        let state = self.open()?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let values = scan_map(
            &state.conversations,
            start,
            ConversationId::new,
            limit + 1,
            |record| {
                query.owner_conversation.is_none_or(|wanted| {
                    record
                        .owner
                        .is_some_and(|owner| owner.conversation == wanted)
                }) && query
                    .owner_task
                    .is_none_or(|wanted| record.owner.is_some_and(|owner| owner.task == wanted))
            },
        );
        Ok(page_of(values, limit, start.order, |record| {
            record.id.get()
        }))
    }

    async fn entry(&self, id: EntryId) -> StorageResult<Option<StoredEntry>> {
        Ok(self.open()?.stored_entry(id))
    }

    async fn visible_entry(
        &self,
        conversation: ConversationId,
        id: EntryId,
    ) -> StorageResult<Option<StoredEntry>> {
        let state = self.open()?;
        let chain = state.chain(conversation, None, None)?;
        let Some((owner, _)) = state.entry_index.get(&id) else {
            return Ok(None);
        };
        if !is_visible(&chain, *owner, id) {
            return Ok(None);
        }
        Ok(state.stored_entry(id))
    }

    async fn find_latest_head_marker(
        &self,
        conversation: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> StorageResult<Option<EntryRecord>> {
        let state = self.open()?;
        let chain = state.chain(conversation, None, at_or_before)?;
        Ok(
            super::fork::find_latest_head_marker(&chain, |id| state.entries_of(id), at_or_before)
                .cloned(),
        )
    }

    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<EntryRecord>> {
        check_limit(limit)?;
        let state = self.open()?;
        let start = scan_start(query.order, cursor, ScanOrder::Descending)?;
        // cursor 收紧扫描远离的那一侧的边界。
        let (mut min, mut max) = (query.min, query.max);
        if let Some(after) = start.after {
            match start.order {
                ScanOrder::Descending => {
                    let Some(below) = after.checked_sub(1) else {
                        return Ok(Page {
                            items: Vec::new(),
                            next: None,
                        });
                    };
                    max = Some(max.map_or(EntryId::new(below), |max| max.min(EntryId::new(below))));
                }
                ScanOrder::Ascending => {
                    let above = EntryId::new(after.saturating_add(1));
                    min = Some(min.map_or(above, |min| min.max(above)));
                }
            }
        }
        let chain = state.chain(query.conversation, min, max)?;
        let values: Vec<EntryRecord> =
            visible_entries(&chain, |id| state.entries_of(id), min, max, start.order)
                .into_iter()
                .take(limit + 1)
                .cloned()
                .collect();
        Ok(page_of(values, limit, start.order, |entry| entry.id.get()))
    }

    async fn task(&self, id: TaskId) -> StorageResult<Option<TaskRecord>> {
        Ok(self.open()?.tasks.get(&id).cloned())
    }

    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<TaskRecord>> {
        check_limit(limit)?;
        let state = self.open()?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let values = scan_map(&state.tasks, start, TaskId::new, limit + 1, |task| {
            query
                .conversation
                .is_none_or(|wanted| task.conversation == wanted)
                && query
                    .kind
                    .as_ref()
                    .is_none_or(|wanted| &task.kind == wanted)
                && query
                    .status
                    .is_none_or(|wanted| task.state.status() == wanted)
                && query
                    .abort_requested
                    .is_none_or(|wanted| task.abort_requested == wanted)
                && query
                    .background
                    .is_none_or(|wanted| task.background == wanted)
        });
        Ok(page_of(values, limit, start.order, |task| task.id.get()))
    }

    async fn submission(&self, id: SubmissionId) -> StorageResult<Option<SubmissionRecord>> {
        Ok(self.open()?.submissions.get(&id).cloned())
    }

    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<SubmissionRecord>> {
        check_limit(limit)?;
        let state = self.open()?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let values = scan_map(
            &state.submissions,
            start,
            SubmissionId::new,
            limit + 1,
            |record| {
                query
                    .conversation
                    .is_none_or(|wanted| record.conversation == wanted)
                    && query.status.is_none_or(|wanted| record.status() == wanted)
            },
        );
        Ok(page_of(values, limit, start.order, |record| {
            record.id.get()
        }))
    }

    async fn submission_by_request(
        &self,
        conversation: ConversationId,
        request_id: &str,
    ) -> StorageResult<Option<SubmissionRecord>> {
        let state = self.open()?;
        Ok(state
            .requests
            .get(&(conversation, request_id.to_owned()))
            .and_then(|id| state.submissions.get(id))
            .cloned())
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
    ) -> StorageResult<Option<DocumentRecord>> {
        let state = self.open()?;
        Ok(state
            .documents
            .values()
            .find(|doc| doc.is_alive() && &doc.address == address)
            .cloned())
    }

    async fn document(&self, id: DocumentId) -> StorageResult<Option<DocumentRecord>> {
        let state = self.open()?;
        Ok(state
            .documents
            .get(&id)
            .filter(|doc| doc.is_alive())
            .cloned())
    }

    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<DocumentRecord>> {
        check_limit(limit)?;
        let state = self.open()?;
        let start = scan_start(Some(ScanOrder::Ascending), cursor, ScanOrder::Ascending)?;
        let values = scan_map(&state.documents, start, DocumentId::new, limit + 1, |doc| {
            doc.is_alive()
                && doc.address.scope == query.scope
                && query
                    .kind
                    .as_ref()
                    .is_none_or(|wanted| &doc.address.kind == wanted)
        });
        Ok(page_of(values, limit, start.order, |doc| doc.id.get()))
    }

    async fn session(&self, id: &str) -> StorageResult<Option<SessionRecord>> {
        Ok(self.open()?.sessions.get(id).cloned())
    }

    async fn scan_sessions(
        &self,
        query: &SessionQuery,
        limit: usize,
        cursor: Option<&SessionCursor>,
    ) -> StorageResult<SessionPage> {
        check_limit(limit)?;
        let state = self.open()?;
        let (order, after) = session_scan_start(query.order, cursor)?;
        let keep = |record: &&SessionRecord| {
            query
                .lifecycle
                .is_none_or(|wanted| record.lifecycle == wanted)
        };
        let values: Vec<SessionRecord> = match order {
            ScanOrder::Ascending => {
                let lower = after.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
                state
                    .sessions
                    .range::<str, _>((lower, Bound::Unbounded))
                    .map(|(_, record)| record)
                    .filter(keep)
                    .take(limit + 1)
                    .cloned()
                    .collect()
            }
            ScanOrder::Descending => {
                let upper = after.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
                state
                    .sessions
                    .range::<str, _>((Bound::Unbounded, upper))
                    .rev()
                    .map(|(_, record)| record)
                    .filter(keep)
                    .take(limit + 1)
                    .cloned()
                    .collect()
            }
        };
        Ok(session_page_of(values, limit, order))
    }

    async fn close(&self) -> StorageResult<()> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::durable::conformance;
    use crate::test_support::block_on;

    #[test]
    fn passes_the_storage_conformance_suite() {
        block_on(conformance::run_all(|| async {
            Arc::new(MemoryStorage::new()) as Arc<dyn Storage>
        }));
    }
}
