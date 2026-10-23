//! 一次提交里的事务视图（spec §4）：表读只在第一次表写之前；document 读写随时可用且读到
//! 自己的写；创建类方法直接返回创建出的 ID / 记录。闭包结束后由 [`Tx::assemble`] 组成一批
//! `StorageWrite`。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::Value;

use komo_kernel::durable::Message;
use komo_kernel::durable::records::ContextEdit;
use komo_kernel::durable::storage::{DocumentCreate, DocumentQuery, TaskQuery};
use komo_kernel::durable::task::{Overlay, check_checkpoint};
use komo_kernel::durable::{
    ConversationId, ConversationRecord, Cursor, DocumentAddress, DocumentId, DocumentRecord,
    DocumentScope, EntryId, EntryRecord, ROOT_CONVERSATION_ID, Storage, StorageExt, StorageWrite,
    SubmissionId, SubmissionKind, SubmissionRecord, TaskId, TaskRecord, TaskState, TaskStatus,
};

use super::error::DurableError;
use super::kind::TaskKind;

const SCAN_PAGE: usize = 256;

/// 一次提交绑定的任务：`create_task` 默认落在它的 conversation，追加的 entry 记 `by_task`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskScope {
    pub conversation: ConversationId,
    pub task: TaskId,
}

/// 新任务归谁拥有：它的 conversation（顶层任务），或另一个任务（子任务）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    Conversation,
    Task(TaskId),
}

#[derive(Debug, Clone, Default)]
pub struct TaskOptions {
    /// 默认：所有者任务的 conversation，或事务绑定的 conversation。
    pub conversation: Option<ConversationId>,
    /// 仅 conversation 所有的任务：不受 conversation abort / idle / 级联影响。
    pub background: bool,
    /// 创建者只在内存里等它，重启后不恢复（spec §5.4）。
    pub abandon_on_restart: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationOwnership {
    Ownerless,
    Task(TaskId),
}

/// `head` 的写法：指自己（"自此重新开始"）或指某条已有 entry。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftHead {
    SelfHead,
    At(EntryId),
}

/// 追加 entry 时调用方给的字段；ID、conversation、`by_task` 由事务填。
#[derive(Debug, Clone, Default)]
pub struct EntryDraft {
    pub kind: String,
    pub model: Option<Vec<Message>>,
    pub data: Option<Value>,
    pub head: Option<DraftHead>,
    pub edits: Vec<ContextEdit>,
}

impl EntryDraft {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            ..Self::default()
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn with_model(mut self, model: Vec<Message>) -> Self {
        self.model = Some(model);
        self
    }

    pub fn with_head(mut self, head: DraftHead) -> Self {
        self.head = Some(head);
        self
    }
}

struct DocState {
    existing: Option<DocumentRecord>,
    current: Option<(u32, Value)>,
    retire_existing: bool,
}

pub struct Tx {
    storage: Arc<dyn Storage>,
    now: i64,
    scope: Option<TaskScope>,
    has_table_write: bool,
    conversations: Vec<ConversationRecord>,
    entries: Vec<EntryRecord>,
    tasks: BTreeMap<TaskId, (bool, TaskRecord)>,
    submissions: BTreeMap<SubmissionId, SubmissionRecord>,
    documents: HashMap<DocumentAddress, DocState>,
}

impl Tx {
    pub(super) fn new(storage: Arc<dyn Storage>, now: i64, scope: Option<TaskScope>) -> Self {
        Self {
            storage,
            now,
            scope,
            has_table_write: false,
            conversations: Vec::new(),
            entries: Vec::new(),
            tasks: BTreeMap::new(),
            submissions: BTreeMap::new(),
            documents: HashMap::new(),
        }
    }

    pub fn scope(&self) -> Option<TaskScope> {
        self.scope
    }

    /// 本次提交的时刻（毫秒），也是任务 `started_at` / `ended_at` 的戳。
    pub fn now(&self) -> i64 {
        self.now
    }

    // ------------------------------------------------------------- 表读

    fn read_guard(&self, method: &'static str) -> Result<(), DurableError> {
        if self.has_table_write {
            return Err(DurableError::ReadAfterWrite(method));
        }
        Ok(())
    }

    pub async fn conversation(
        &mut self,
        id: ConversationId,
    ) -> Result<Option<ConversationRecord>, DurableError> {
        self.read_guard("conversation")?;
        Ok(self.storage.conversation(id).await?)
    }

    pub async fn task(&mut self, id: TaskId) -> Result<Option<TaskRecord>, DurableError> {
        self.read_guard("task")?;
        Ok(self.storage.task(id).await?)
    }

    pub async fn entry(&mut self, id: EntryId) -> Result<Option<EntryRecord>, DurableError> {
        self.read_guard("entry")?;
        Ok(self.storage.entry(id).await?.map(|stored| stored.entry))
    }

    pub async fn scan_tasks(
        &mut self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<komo_kernel::durable::Page<TaskRecord>, DurableError> {
        self.read_guard("scan_tasks")?;
        Ok(self.storage.scan_tasks(query, limit, cursor).await?)
    }

    /// 某状态下的全部任务（分页读完）。
    pub async fn all_tasks(&mut self, status: TaskStatus) -> Result<Vec<TaskRecord>, DurableError> {
        self.read_guard("scan_tasks")?;
        let query = TaskQuery {
            status: Some(status),
            ..TaskQuery::default()
        };
        let mut all = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = self
                .storage
                .scan_tasks(&query, SCAN_PAGE, cursor.as_ref())
                .await?;
            all.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(all),
            }
        }
    }

    // ------------------------------------------------------------- 表写

    fn mark_table_write(&mut self) {
        self.has_table_write = true;
    }

    async fn require_conversation(&self, id: ConversationId) -> Result<(), DurableError> {
        if self.conversations.iter().any(|record| record.id == id) {
            return Ok(());
        }
        if self.storage.conversation(id).await?.is_none() {
            return Err(DurableError::rejected(format!(
                "Conversation {id} does not exist"
            )));
        }
        Ok(())
    }

    /// 创建根 conversation（保留 ID 1）。
    pub async fn create_root_conversation(&mut self) -> Result<ConversationRecord, DurableError> {
        self.mark_table_write();
        let record = ConversationRecord::root(ROOT_CONVERSATION_ID);
        self.conversations.push(record.clone());
        Ok(record)
    }

    pub async fn create_conversation(
        &mut self,
        ownership: ConversationOwnership,
    ) -> Result<ConversationRecord, DurableError> {
        self.mark_table_write();
        let id: ConversationId = self.storage.mint().await?;
        let owner = self.conversation_owner(ownership).await?;
        let record = ConversationRecord {
            id,
            parent: None,
            owner,
        };
        self.conversations.push(record.clone());
        Ok(record)
    }

    pub async fn fork_conversation(
        &mut self,
        parent: ConversationId,
        at: EntryId,
        ownership: ConversationOwnership,
    ) -> Result<ConversationRecord, DurableError> {
        self.mark_table_write();
        let id: ConversationId = self.storage.mint().await?;
        self.require_conversation(parent).await?;
        let owner = self.conversation_owner(ownership).await?;
        let mut record = ConversationRecord::fork(id, parent, at);
        record.owner = owner;
        self.conversations.push(record.clone());
        Ok(record)
    }

    async fn conversation_owner(
        &self,
        ownership: ConversationOwnership,
    ) -> Result<Option<komo_kernel::durable::records::ConversationOwner>, DurableError> {
        let ConversationOwnership::Task(task) = ownership else {
            return Ok(None);
        };
        let owner = self.current_task(task).await?.ok_or_else(|| {
            DurableError::rejected(format!("Conversation owner task {task} does not exist"))
        })?;
        Ok(Some(komo_kernel::durable::records::ConversationOwner {
            conversation: owner.conversation,
            task,
        }))
    }

    /// 候选优先，其次已提交。
    pub(super) async fn current_task(
        &self,
        id: TaskId,
    ) -> Result<Option<TaskRecord>, DurableError> {
        if let Some((_, record)) = self.tasks.get(&id) {
            return Ok(Some(record.clone()));
        }
        Ok(self.storage.task(id).await?)
    }

    pub async fn append_entry(
        &mut self,
        conversation: ConversationId,
        draft: EntryDraft,
    ) -> Result<EntryRecord, DurableError> {
        self.mark_table_write();
        self.require_conversation(conversation).await?;
        let id: EntryId = self.storage.mint().await?;
        let mut record = EntryRecord::new(id, conversation, draft.kind);
        record.model = draft.model;
        record.data = draft.data;
        record.head = draft.head.map(|head| match head {
            DraftHead::SelfHead => id,
            DraftHead::At(at) => at,
        });
        record.edits = draft.edits;
        record.by_task = self.scope.map(|scope| scope.task);
        self.entries.push(record.clone());
        Ok(record)
    }

    /// 建一个 `pending` 任务，checkpoint 来自定义的 `initial`。
    pub async fn create_task(
        &mut self,
        kind: &dyn TaskKind,
        input: Value,
        ownership: Ownership,
        options: TaskOptions,
    ) -> Result<TaskId, DurableError> {
        self.mark_table_write();
        let owner = match ownership {
            Ownership::Task(id) => {
                let owner = self.current_task(id).await?.ok_or_else(|| {
                    DurableError::rejected(format!("Task owner {id} does not exist"))
                })?;
                if options.background {
                    return Err(DurableError::rejected("A child task cannot be background"));
                }
                if options
                    .conversation
                    .is_some_and(|c| c != owner.conversation)
                {
                    return Err(DurableError::rejected(format!(
                        "A child task lives in its owner's conversation {}",
                        owner.conversation
                    )));
                }
                Some(owner)
            }
            Ownership::Conversation => None,
        };
        let conversation = owner
            .as_ref()
            .map(|owner| owner.conversation)
            .or(options.conversation)
            .or(self.scope.map(|scope| scope.conversation))
            .ok_or_else(|| DurableError::rejected("create_task requires a conversation"))?;
        self.require_conversation(conversation).await?;
        let checkpoint = kind.initial(&input);
        check_checkpoint(&checkpoint)?;
        let id: TaskId = self.storage.mint().await?;
        let mut record = TaskRecord::pending(id, conversation, kind.name(), input, checkpoint);
        record.version = kind.version();
        record.owner = owner.map(|owner| owner.id);
        record.background = options.background;
        record.abandon_on_restart = options.abandon_on_restart;
        self.tasks.insert(id, (true, record));
        Ok(id)
    }

    /// 内部：整条替换一个任务记录，盖起止时刻。任务只经自己的 runtime 改自己的状态。
    pub(super) fn set_task(&mut self, value: TaskRecord) -> Result<(), DurableError> {
        self.mark_table_write();
        let candidate = self.tasks.get(&value.id);
        if let Some((_, existing)) = candidate {
            if matches!(existing.state, TaskState::Terminal { .. }) {
                return Err(DurableError::rejected(format!(
                    "Task {} already has a terminal candidate",
                    value.id
                )));
            }
            if existing.conversation != value.conversation {
                return Err(DurableError::rejected(format!(
                    "Task {} cannot change conversations",
                    value.id
                )));
            }
        }
        let created = candidate.is_some_and(|(created, _)| *created);
        let mut value = value;
        let earlier = candidate.map(|(_, record)| record);
        let status = value.state.status();
        if value.started_at.is_none() {
            value.started_at = earlier
                .and_then(|r| r.started_at)
                .or((status == TaskStatus::Running).then_some(self.now));
        }
        if value.ended_at.is_none() {
            value.ended_at = earlier
                .and_then(|r| r.ended_at)
                .or((status == TaskStatus::Terminal).then_some(self.now));
        }
        self.tasks.insert(value.id, (created, value));
        Ok(())
    }

    pub(super) fn is_staged(&self, id: TaskId) -> bool {
        self.tasks.contains_key(&id)
    }

    /// 内部：本次事务已暂存的任务候选与新建的 conversation，所有权遍历用。
    pub(super) fn overlay(&self) -> Overlay {
        Overlay {
            tasks: self
                .tasks
                .iter()
                .map(|(id, (_, record))| (*id, record.clone()))
                .collect(),
            edges: self
                .conversations
                .iter()
                .map(|record| (record.id, record.owner.map(|owner| owner.task)))
                .collect(),
        }
    }

    // ------------------------------------------------------------- submission

    /// 一条 submission 的当前记录：本次事务暂存的候选优先，其次已提交的。像 spec §6 的
    /// `tx.settleSubmission` 那样不算"表读"，所以第一次表写之后也能用。
    pub async fn submission(
        &mut self,
        id: SubmissionId,
    ) -> Result<Option<SubmissionRecord>, DurableError> {
        if let Some(record) = self.submissions.get(&id) {
            return Ok(Some(record.clone()));
        }
        Ok(self.storage.submission(id).await?)
    }

    /// conversation 内按 `request_id` 去重的那条（含本次事务里刚建的）。
    pub async fn submission_by_request(
        &mut self,
        conversation: ConversationId,
        request_id: &str,
    ) -> Result<Option<SubmissionRecord>, DurableError> {
        if let Some(record) = self.submissions.values().find(|record| {
            record.conversation == conversation && record.request_id.as_deref() == Some(request_id)
        }) {
            return Ok(Some(record.clone()));
        }
        self.read_guard("submission_by_request")?;
        Ok(self
            .storage
            .submission_by_request(conversation, request_id)
            .await?)
    }

    /// 建一条 submission（铸 ID）。
    pub async fn create_submission(
        &mut self,
        conversation: ConversationId,
        request_id: Option<&str>,
        kind: SubmissionKind,
    ) -> Result<SubmissionRecord, DurableError> {
        self.mark_table_write();
        self.require_conversation(conversation).await?;
        let id: SubmissionId = self.storage.mint().await?;
        let record = SubmissionRecord {
            id,
            conversation,
            request_id: request_id.map(str::to_owned),
            kind,
        };
        self.submissions.insert(id, record.clone());
        Ok(record)
    }

    /// 整条替换一条 submission（状态转换的合法性由调用方的规则保证）。
    pub fn put_submission(&mut self, record: SubmissionRecord) {
        self.mark_table_write();
        self.submissions.insert(record.id, record);
    }

    /// `at_or_before`（含）以内对 conversation 可见的最新一条带 `head` 的 entry。
    pub async fn latest_head_marker(
        &mut self,
        conversation: ConversationId,
    ) -> Result<Option<EntryRecord>, DurableError> {
        self.read_guard("latest_head_marker")?;
        Ok(self
            .storage
            .find_latest_head_marker(conversation, None)
            .await?)
    }

    // ------------------------------------------------------------- document

    async fn load_document(&mut self, address: &DocumentAddress) -> Result<(), DurableError> {
        if self.documents.contains_key(address) {
            return Ok(());
        }
        let existing = self.storage.find_document(address).await?;
        let current = existing
            .as_ref()
            .map(|record| (record.version, record.value.clone()));
        self.documents.insert(
            address.clone(),
            DocState {
                existing,
                current,
                retire_existing: false,
            },
        );
        Ok(())
    }

    /// 地址上当前活着的 document（读到本事务自己的写）。
    pub async fn document(
        &mut self,
        address: &DocumentAddress,
    ) -> Result<Option<(u32, Value)>, DurableError> {
        self.load_document(address).await?;
        Ok(self.documents[address].current.clone())
    }

    /// 地址上没有就创建，有就整值替换。
    pub async fn put_document(
        &mut self,
        address: DocumentAddress,
        version: u32,
        value: Value,
    ) -> Result<(), DurableError> {
        self.load_document(&address).await?;
        if let Some(state) = self.documents.get_mut(&address) {
            state.current = Some((version, value));
        }
        Ok(())
    }

    /// 退役地址上的 document；返回之前是否活着。
    pub async fn retire_document(
        &mut self,
        address: &DocumentAddress,
    ) -> Result<bool, DurableError> {
        self.load_document(address).await?;
        let state = self.documents.get_mut(address).expect("loaded above");
        let was_alive = state.current.is_some();
        state.current = None;
        if state.existing.is_some() {
            state.retire_existing = true;
        }
        Ok(was_alive)
    }

    /// 任务 terminal 时随它退役的、作用域为该任务的全部 document（spec §5.3）。
    pub(super) async fn retire_task_documents(&mut self, task: TaskId) -> Result<(), DurableError> {
        let query = DocumentQuery {
            scope: DocumentScope::Task { task },
            kind: None,
        };
        let mut addresses = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = self
                .storage
                .scan_documents(&query, SCAN_PAGE, cursor.as_ref())
                .await?;
            addresses.extend(page.items.into_iter().map(|record| record.address));
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        for (address, state) in &self.documents {
            if address.scope == query.scope && state.current.is_some() {
                addresses.push(address.clone());
            }
        }
        for address in addresses {
            self.retire_document(&address).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------- 装配

    /// 校验并组成这一批写入。新建的归属任务 / conversation 要有活的所有者，按所有者的最终
    /// 候选判断：不能是 completing、terminal 或带 abort 标记（所以任务不能在结束自己的那笔
    /// 提交里再建名下的工作，spec §5.5）。
    pub(super) async fn assemble(mut self) -> Result<Vec<StorageWrite>, DurableError> {
        let mut owners: Vec<(&'static str, TaskId)> = Vec::new();
        for record in &self.conversations {
            if let Some(owner) = record.owner {
                owners.push(("Conversation owner task", owner.task));
            }
        }
        for (created, record) in self.tasks.values() {
            if *created && let Some(owner) = record.owner {
                owners.push(("Task owner", owner));
            }
        }
        for (what, id) in owners {
            let owner = self
                .current_task(id)
                .await?
                .ok_or_else(|| DurableError::rejected(format!("{what} {id} does not exist")))?;
            match owner.state.status() {
                TaskStatus::Terminal | TaskStatus::Completing => {
                    return Err(DurableError::rejected(format!(
                        "{what} {id} is {:?}",
                        owner.state.status()
                    )));
                }
                _ => {}
            }
            if owner.abort_requested {
                return Err(DurableError::rejected(format!(
                    "{what} {id} is abort-marked"
                )));
            }
        }
        let mut writes = Vec::new();
        writes.extend(self.conversations.drain(..).map(StorageWrite::Conversation));
        writes.extend(self.entries.drain(..).map(StorageWrite::Entry));
        writes.extend(
            std::mem::take(&mut self.submissions)
                .into_values()
                .map(StorageWrite::Submission),
        );
        writes.extend(
            std::mem::take(&mut self.tasks)
                .into_values()
                .map(|(_, record)| StorageWrite::Task(record)),
        );
        let documents = std::mem::take(&mut self.documents);
        for (address, state) in documents {
            let DocState {
                existing,
                current,
                retire_existing,
            } = state;
            match (existing, current) {
                (None, None) => {}
                (None, Some((version, value))) => {
                    writes.push(self.document_create(address, version, value).await?);
                }
                (Some(existing), None) => {
                    if retire_existing {
                        writes.push(StorageWrite::DocumentRetire { id: existing.id });
                    }
                }
                (Some(existing), Some((version, value))) => {
                    if retire_existing {
                        writes.push(StorageWrite::DocumentRetire { id: existing.id });
                        writes.push(self.document_create(address, version, value).await?);
                    } else if existing.version != version || existing.value != value {
                        writes.push(StorageWrite::DocumentChange {
                            id: existing.id,
                            version,
                            value,
                        });
                    }
                }
            }
        }
        Ok(writes)
    }

    async fn document_create(
        &self,
        address: DocumentAddress,
        version: u32,
        value: Value,
    ) -> Result<StorageWrite, DurableError> {
        let id: DocumentId = self.storage.mint().await?;
        Ok(StorageWrite::DocumentCreate(DocumentCreate {
            id,
            address,
            version,
            value,
        }))
    }
}
