//! D1 的 [`Storage`] 在 Turso 上的实现：写经 [`Line`]，读走同库的一条只读连接。
//!
//! # 写
//!
//! `StorageWrite` 先转成线的 [`Write`]（记录 serde 成 JSON 文本，可过滤的字段单独成列）。
//! `commit` 是结构性提交（FULL）；`commit_progress` 只收 `DocumentChange`，走 OFF 档。
//! 校验失败（ID 被别的实体占用、不可变实体重写、目标不存在、request_id 被占用、
//! document 已退役或地址上已有活的……）是 [`StorageError::Rejected`]，线保持健康；
//! 引擎或 I/O 的错误是 [`StorageError::Backend`]，同时让线失败（之后所有提交返回同一个错误）。
//! 界线的细节见 [`super::write`]。
//!
//! # 读
//!
//! 读都在一条只读连接上，WAL 下不阻塞也不被写阻塞。每条语句是自己的快照；fork 扫描先取祖先链
//! 的 conversation 记录（不可变），再逐段查 entry（只追加），所以跨语句也不会读到自相矛盾的历史。
//!
//! fork 的可见性规则**不在 SQL 里重写**：祖先链与每段的上限用 kernel 的
//! [`fork_chain`] / [`is_visible`] 算，这里只把"某段的 `[min, upper]`"翻成一条
//! `conversation_id = ? AND id BETWEEN ? AND ?` 查询。cursor 沿用 D1 的 `{after, order}`：
//! 降序续扫把上界收到 `after - 1`、升序把下界收到 `after + 1`，再整条走同一套链计算。
//!
//! document 的 `key` 在库里编码成 `key_value`：`None` → 空串，`Some(k)` → `"=" + k`，
//! 所以缺省与空串是两个不同的地址（conformance 的 `document_identities_stay_lossless`）。

use std::collections::HashMap;
use std::path::Path;

use komo_kernel::durable::fork::{ForkError, ForkSegment, fork_chain, is_visible};
use komo_kernel::durable::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use komo_kernel::durable::records::{
    ConversationRecord, DocumentAddress, DocumentRecord, DocumentScope, EntryRecord, SessionRecord,
    SubmissionRecord, TaskRecord,
};
use komo_kernel::durable::storage::{
    ConversationQuery, Cursor, DocumentQuery, EntryQuery, MAX_ID, Page, ScanOrder, ScanStart,
    SessionCursor, SessionPage, SessionQuery, Storage, StorageError, StorageResult, StorageWrite,
    StoredEntry, SubmissionQuery, TaskQuery, check_limit, check_progress, page_of, scan_start,
    session_page_of, session_scan_start,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;
use turso::{Connection, Value};

use super::line::{Line, LineError};
use super::write::{CommitKind, Write};

/// Turso 上的 `Storage`。
pub struct TursoStorage {
    line: Line,
    readers: ReadPool,
}

/// 读连接的条数上限。WAL 下读不挡写、写不挡读，所以读可以并发；小池就够——提交线上每次提交
/// 要读好几次，补读 / HTTP 查询再多也只占这几条，不会把提交的读挤到队尾（D5 W5，原先只有一条）。
const READ_CONNECTIONS: usize = 4;

/// 读连接池：信号量限并发，空闲连接放在栈里，用完放回；栈空了现开一条（连接在一次查询中途被
/// 取消丢掉时就是这样补上的）。关闭后信号量关闭、空闲连接清空，在途查询的连接用完直接丢。
struct ReadPool {
    permits: Semaphore,
    free: std::sync::Mutex<Vec<Connection>>,
}

impl ReadPool {
    fn new(first: Connection) -> ReadPool {
        ReadPool {
            permits: Semaphore::new(READ_CONNECTIONS),
            free: std::sync::Mutex::new(vec![first]),
        }
    }

    fn free(&self) -> std::sync::MutexGuard<'_, Vec<Connection>> {
        self.free
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn line_error(error: LineError) -> StorageError {
    match error {
        LineError::Rejected(message) => StorageError::Rejected(message),
        LineError::Closed => StorageError::Closed,
        other => StorageError::Backend(other.to_string()),
    }
}

fn backend(what: &str, error: impl std::fmt::Display) -> StorageError {
    StorageError::Backend(format!("{what}：{error}"))
}

fn fork_error(error: ForkError) -> StorageError {
    match error {
        ForkError::UnknownConversation(id) => {
            StorageError::request(format!("unknown conversation: {id}"))
        }
        ForkError::Cycle(id) => StorageError::Backend(format!("fork cycle at conversation {id}")),
    }
}

/// 写路径上的 ID：超出库的有符号 64 位就是被拒。
fn write_id(id: u64) -> StorageResult<i64> {
    if id > MAX_ID {
        return Err(StorageError::rejected(format!("ID {id} is out of range")));
    }
    Ok(id as i64)
}

/// 读路径上的 ID：超出范围的不可能存在。
fn read_id(id: u64) -> Option<i64> {
    i64::try_from(id).ok()
}

fn json<T: Serialize>(value: &T) -> StorageResult<String> {
    serde_json::to_string(value).map_err(|e| backend("serialize", e))
}

/// 枚举的 serde 名字（`pending`、`active`……）：库里的状态列与查询条件用同一个写法。
fn tag<T: Serialize>(value: &T) -> StorageResult<String> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => Ok(name),
        Ok(other) => Err(backend("tag", format!("not a string: {other}"))),
        Err(error) => Err(backend("tag", error)),
    }
}

fn encode_key(key: &Option<String>) -> String {
    key.as_ref()
        .map_or_else(String::new, |key| format!("={key}"))
}

fn decode_key(stored: &str) -> Option<String> {
    stored.strip_prefix('=').map(str::to_owned)
}

fn scope_columns(scope: &DocumentScope) -> StorageResult<(&'static str, i64)> {
    Ok(match scope {
        DocumentScope::Session => ("session", 0),
        DocumentScope::Conversation { conversation } => {
            ("conversation", write_id(conversation.get())?)
        }
        DocumentScope::Task { task } => ("task", write_id(task.get())?),
    })
}

fn to_write(write: &StorageWrite) -> StorageResult<Write> {
    Ok(match write {
        StorageWrite::Conversation(record) => Write::InsertConversation {
            id: write_id(record.id.get())?,
            owner_conversation: record
                .owner
                .map(|owner| write_id(owner.conversation.get()))
                .transpose()?,
            owner_task: record
                .owner
                .map(|owner| write_id(owner.task.get()))
                .transpose()?,
            parent_conversation: record
                .parent
                .map(|parent| write_id(parent.conversation.get()))
                .transpose()?,
            parent_at: record
                .parent
                .map(|parent| write_id(parent.at.get()))
                .transpose()?,
            session_id: record.session.clone(),
            record: json(record)?,
        },
        StorageWrite::Entry(record) => Write::InsertEntry {
            id: write_id(record.id.get())?,
            conversation_id: write_id(record.conversation.get())?,
            head: record.head.map(|head| write_id(head.get())).transpose()?,
            record: json(record)?,
        },
        StorageWrite::Task(record) => Write::UpsertTask {
            id: write_id(record.id.get())?,
            conversation_id: write_id(record.conversation.get())?,
            kind: record.kind.clone(),
            status: tag(&record.state.status())?,
            abort_requested: record.abort_requested,
            background: record.background,
            record: json(record)?,
        },
        StorageWrite::Submission(record) => Write::UpsertSubmission {
            id: write_id(record.id.get())?,
            conversation_id: write_id(record.conversation.get())?,
            request_id: record.request_id.clone(),
            status: tag(&record.status())?,
            record: json(record)?,
        },
        StorageWrite::DocumentCreate(create) => {
            let (scope_kind, owner_id) = scope_columns(&create.address.scope)?;
            Write::CreateDocument {
                id: write_id(create.id.get())?,
                kind: create.address.kind.clone(),
                scope_kind: scope_kind.to_owned(),
                owner_id,
                key_value: encode_key(&create.address.key),
                version: i64::from(create.version),
                value: json(&create.value)?,
            }
        }
        StorageWrite::DocumentChange { id, version, value } => Write::ReplaceDocument {
            id: write_id(id.get())?,
            version: i64::from(*version),
            value: json(value)?,
        },
        StorageWrite::DocumentRetire { id } => Write::RetireDocument {
            id: write_id(id.get())?,
        },
        StorageWrite::PurgeConversations { conversations } => Write::PurgeConversations {
            conversations: conversations
                .iter()
                .map(|conversation| write_id(conversation.get()))
                .collect::<StorageResult<Vec<_>>>()?,
        },
        StorageWrite::Session(record) => Write::UpsertSession {
            id: record.id.clone(),
            lifecycle: tag(&record.lifecycle)?,
            current_branch: Some(write_id(record.current_branch.get())?),
            root_conversation: write_id(record.root_conversation.get())?,
            record: json(record)?,
        },
    })
}

fn int(v: i64) -> Value {
    Value::Integer(v)
}

fn text(v: impl Into<String>) -> Value {
    Value::Text(v.into())
}

fn as_int(value: &Value) -> StorageResult<i64> {
    match value {
        Value::Integer(n) => Ok(*n),
        other => Err(backend(
            "column",
            format!("expected an integer, got {other:?}"),
        )),
    }
}

fn as_opt_int(value: &Value) -> StorageResult<Option<i64>> {
    match value {
        Value::Null => Ok(None),
        other => as_int(other).map(Some),
    }
}

fn as_text(value: &Value) -> StorageResult<&str> {
    match value {
        Value::Text(t) => Ok(t),
        other => Err(backend("column", format!("expected text, got {other:?}"))),
    }
}

fn decode<T: DeserializeOwned>(value: &Value) -> StorageResult<T> {
    serde_json::from_str(as_text(value)?).map_err(|e| backend("decode", e))
}

/// 一次扫描的 WHERE 条件 + 参数（`?N` 的编号自动排）。
#[derive(Default)]
struct Filter {
    clauses: Vec<String>,
    params: Vec<Value>,
}

impl Filter {
    fn eq(&mut self, column: &str, value: Value) {
        self.params.push(value);
        self.clauses
            .push(format!("{column} = ?{}", self.params.len()));
    }

    fn raw(&mut self, clause: &str) {
        self.clauses.push(clause.to_owned());
    }

    fn cmp(&mut self, column: &str, op: &str, value: Value) {
        self.params.push(value);
        self.clauses
            .push(format!("{column} {op} ?{}", self.params.len()));
    }

    fn where_sql(&self) -> String {
        if self.clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", self.clauses.join(" AND "))
        }
    }
}

fn direction(order: ScanOrder) -> &'static str {
    match order {
        ScanOrder::Ascending => "ASC",
        ScanOrder::Descending => "DESC",
    }
}

/// 多取一条用来判断有没有下一页。
fn take(limit: usize) -> i64 {
    i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)
}

impl TursoStorage {
    /// 打开 `<data_dir>/durable.db`。
    pub async fn open(data_dir: &Path) -> StorageResult<TursoStorage> {
        TursoStorage::from_line(Line::open(data_dir).await.map_err(line_error)?)
    }

    pub fn from_line(line: Line) -> StorageResult<TursoStorage> {
        let reader = line.read_connection().map_err(line_error)?;
        Ok(TursoStorage {
            line,
            readers: ReadPool::new(reader),
        })
    }

    /// 底下的线（blob 顺序、基准、测试用）。
    pub fn line(&self) -> &Line {
        &self.line
    }

    async fn query(&self, sql: &str, params: Vec<Value>) -> StorageResult<Vec<Vec<Value>>> {
        let _permit = self
            .readers
            .permits
            .acquire()
            .await
            .map_err(|_| StorageError::Closed)?;
        let pooled = self.readers.free().pop();
        let conn = match pooled {
            Some(conn) => conn,
            None => self.line.read_connection().map_err(line_error)?,
        };
        let result = Self::run(&conn, sql, params).await;
        if !self.readers.permits.is_closed() {
            self.readers.free().push(conn);
        }
        result
    }

    async fn run(
        conn: &Connection,
        sql: &str,
        params: Vec<Value>,
    ) -> StorageResult<Vec<Vec<Value>>> {
        let mut cursor = conn.query(sql, params).await.map_err(|e| backend(sql, e))?;
        let mut out = Vec::new();
        while let Some(row) = cursor.next().await.map_err(|e| backend(sql, e))? {
            let mut cols = Vec::with_capacity(row.column_count());
            for i in 0..row.column_count() {
                cols.push(row.get_value(i).map_err(|e| backend(sql, e))?);
            }
            out.push(cols);
        }
        Ok(out)
    }

    async fn one(&self, sql: &str, params: Vec<Value>) -> StorageResult<Option<Vec<Value>>> {
        Ok(self.query(sql, params).await?.into_iter().next())
    }

    /// 按主键取一条 `record` JSON。
    async fn record_by_id<T: DeserializeOwned>(
        &self,
        table: &str,
        id: u64,
    ) -> StorageResult<Option<T>> {
        let Some(id) = read_id(id) else {
            self.check_open().await?;
            return Ok(None);
        };
        match self
            .one(
                &format!("SELECT record FROM {table} WHERE id = ?1"),
                vec![int(id)],
            )
            .await?
        {
            Some(row) => Ok(Some(decode(&row[0])?)),
            None => Ok(None),
        }
    }

    async fn check_open(&self) -> StorageResult<()> {
        if self.readers.permits.is_closed() {
            return Err(StorageError::Closed);
        }
        Ok(())
    }

    /// 按 ID 扫一张表的 `record`；多取一条给 `page_of` 判断下一页。
    async fn scan_table<T: DeserializeOwned>(
        &self,
        table: &str,
        mut filter: Filter,
        start: ScanStart,
        limit: usize,
    ) -> StorageResult<Vec<T>> {
        if let Some(after) = start.after {
            // 超出范围的 cursor：升序之后没有东西，降序等于不设界。
            match (read_id(after), start.order) {
                (Some(after), ScanOrder::Ascending) => filter.cmp("id", ">", int(after)),
                (Some(after), ScanOrder::Descending) => filter.cmp("id", "<", int(after)),
                (None, ScanOrder::Ascending) => {
                    self.check_open().await?;
                    return Ok(Vec::new());
                }
                (None, ScanOrder::Descending) => {}
            }
        }
        let sql = format!(
            "SELECT record FROM {table}{} ORDER BY id {} LIMIT {}",
            filter.where_sql(),
            direction(start.order),
            take(limit)
        );
        let rows = self.query(&sql, filter.params).await?;
        rows.iter().map(|row| decode(&row[0])).collect()
    }

    /// 祖先链：先把沿 `parent` 往上的 conversation 记录取齐（不可变），再交给 kernel 的
    /// [`fork_chain`] 算每段的上限。
    async fn chain(
        &self,
        start: ConversationId,
        min: Option<EntryId>,
        max: Option<EntryId>,
    ) -> StorageResult<Vec<ForkSegment>> {
        let mut records: HashMap<ConversationId, ConversationRecord> = HashMap::new();
        let mut current = Some(start);
        while let Some(id) = current {
            if records.contains_key(&id) {
                break; // 环：交给 fork_chain 报告
            }
            match self.conversation(id).await? {
                Some(record) => {
                    current = record.parent.map(|parent| parent.conversation);
                    records.insert(id, record);
                }
                None => break,
            }
        }
        fork_chain(start, |id| records.get(&id), min, max).map_err(fork_error)
    }

    fn stored_entry(row: &[Value]) -> StorageResult<StoredEntry> {
        Ok(StoredEntry {
            entry: decode(&row[0])?,
            commit_seq: Seq::new(as_int(&row[1])? as u64),
        })
    }

    fn document_record(row: &[Value]) -> StorageResult<DocumentRecord> {
        // 列序见 `DOCUMENT_COLUMNS`。
        let scope = match (as_text(&row[2])?, as_int(&row[3])? as u64) {
            ("session", _) => DocumentScope::Session,
            ("conversation", id) => DocumentScope::Conversation {
                conversation: ConversationId::new(id),
            },
            ("task", id) => DocumentScope::Task {
                task: TaskId::new(id),
            },
            (other, _) => return Err(backend("document", format!("unknown scope {other:?}"))),
        };
        Ok(DocumentRecord {
            id: DocumentId::new(as_int(&row[0])? as u64),
            address: DocumentAddress {
                kind: as_text(&row[1])?.to_owned(),
                scope,
                key: decode_key(as_text(&row[4])?),
            },
            version: u32::try_from(as_int(&row[5])?).map_err(|e| backend("document version", e))?,
            value: serde_json::from_str(as_text(&row[6])?).map_err(|e| backend("decode", e))?,
            created_at: Seq::new(as_int(&row[7])? as u64),
            changed_at: Seq::new(as_int(&row[8])? as u64),
            retired_at: as_opt_int(&row[9])?.map(|seq| Seq::new(seq as u64)),
        })
    }
}

const DOCUMENT_COLUMNS: &str = "id, kind, scope_kind, owner_id, key_value, version, value, created_seq, changed_seq, retired_seq";

#[async_trait::async_trait]
impl Storage for TursoStorage {
    async fn commit(&self, writes: &[StorageWrite]) -> StorageResult<Seq> {
        self.check_open().await?;
        let writes = writes
            .iter()
            .map(to_write)
            .collect::<StorageResult<Vec<_>>>()?;
        let seq = self
            .line
            .commit(CommitKind::Structural, writes)
            .await
            .map_err(line_error)?;
        Ok(Seq::new(seq))
    }

    async fn commit_progress(&self, writes: &[StorageWrite]) -> StorageResult<Seq> {
        self.check_open().await?;
        check_progress(writes)?;
        let writes = writes
            .iter()
            .map(to_write)
            .collect::<StorageResult<Vec<_>>>()?;
        let seq = self
            .line
            .commit(CommitKind::Progress, writes)
            .await
            .map_err(line_error)?;
        Ok(Seq::new(seq))
    }

    async fn mint_id(&self) -> StorageResult<u64> {
        self.check_open().await?;
        self.line
            .try_mint_id()
            .map(|id| id as u64)
            .ok_or(StorageError::IdSpaceExhausted)
    }

    async fn conversation(&self, id: ConversationId) -> StorageResult<Option<ConversationRecord>> {
        self.record_by_id("conversations", id.get()).await
    }

    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<ConversationRecord>> {
        check_limit(limit)?;
        self.check_open().await?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let mut filter = Filter::default();
        if let Some(owner) = query.owner_conversation {
            filter.eq("owner_conversation", int(write_id(owner.get())?));
        }
        if let Some(task) = query.owner_task {
            filter.eq("owner_task", int(write_id(task.get())?));
        }
        if let Some(session) = &query.session {
            filter.eq("session_id", text(session.clone()));
        }
        if let Some(parent) = query.parent_conversation {
            filter.eq("parent_conversation", int(write_id(parent.get())?));
        }
        let values = self
            .scan_table("conversations", filter, start, limit)
            .await?;
        Ok(page_of(
            values,
            limit,
            start.order,
            |r: &ConversationRecord| r.id.get(),
        ))
    }

    async fn entry(&self, id: EntryId) -> StorageResult<Option<StoredEntry>> {
        let Some(raw) = read_id(id.get()) else {
            self.check_open().await?;
            return Ok(None);
        };
        match self
            .one(
                "SELECT record, commit_seq FROM entries WHERE id = ?1",
                vec![int(raw)],
            )
            .await?
        {
            Some(row) => Ok(Some(Self::stored_entry(&row)?)),
            None => Ok(None),
        }
    }

    async fn visible_entry(
        &self,
        conversation: ConversationId,
        id: EntryId,
    ) -> StorageResult<Option<StoredEntry>> {
        let chain = self.chain(conversation, None, None).await?;
        let Some(raw) = read_id(id.get()) else {
            return Ok(None);
        };
        let Some(row) = self
            .one(
                "SELECT record, commit_seq, conversation_id FROM entries WHERE id = ?1",
                vec![int(raw)],
            )
            .await?
        else {
            return Ok(None);
        };
        let owner = ConversationId::new(as_int(&row[2])? as u64);
        if !is_visible(&chain, owner, id) {
            return Ok(None);
        }
        Ok(Some(Self::stored_entry(&row)?))
    }

    async fn find_latest_head_marker(
        &self,
        conversation: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> StorageResult<Option<EntryRecord>> {
        let chain = self.chain(conversation, None, at_or_before).await?;
        for segment in &chain {
            let upper = match (segment.upper, at_or_before) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let mut filter = Filter::default();
            filter.eq(
                "conversation_id",
                int(write_id(segment.conversation.get())?),
            );
            filter.raw("head IS NOT NULL");
            if let Some(upper) = upper {
                filter.cmp("id", "<=", int(write_id(upper.get())?));
            }
            let sql = format!(
                "SELECT record FROM entries{} ORDER BY id DESC LIMIT 1",
                filter.where_sql()
            );
            if let Some(row) = self.one(&sql, filter.params).await? {
                return Ok(Some(decode(&row[0])?));
            }
        }
        Ok(None)
    }

    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<EntryRecord>> {
        check_limit(limit)?;
        self.check_open().await?;
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
        let chain = self.chain(query.conversation, min, max).await?;
        // 降序先读最子的一段再往祖先走；升序先读根一段再读各个 fork。
        let segments: Vec<&ForkSegment> = match start.order {
            ScanOrder::Descending => chain.iter().collect(),
            ScanOrder::Ascending => chain.iter().rev().collect(),
        };
        let mut values: Vec<EntryRecord> = Vec::new();
        for segment in segments {
            let wanted = limit + 1 - values.len();
            let upper = match (segment.upper, max) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let mut filter = Filter::default();
            filter.eq(
                "conversation_id",
                int(write_id(segment.conversation.get())?),
            );
            // 上下界超出库的整数范围时，等于没有这一侧的限制或者什么都没有：min 超界 → 空。
            if let Some(min) = min {
                match read_id(min.get()) {
                    Some(min) => filter.cmp("id", ">=", int(min)),
                    None => continue,
                }
            }
            if let Some(upper) = upper
                && let Some(upper) = read_id(upper.get())
            {
                filter.cmp("id", "<=", int(upper));
            }
            let sql = format!(
                "SELECT record FROM entries{} ORDER BY id {} LIMIT {}",
                filter.where_sql(),
                direction(start.order),
                wanted
            );
            for row in self.query(&sql, filter.params).await? {
                values.push(decode(&row[0])?);
            }
            if values.len() > limit {
                break;
            }
        }
        Ok(page_of(
            values,
            limit,
            start.order,
            |entry: &EntryRecord| entry.id.get(),
        ))
    }

    async fn task(&self, id: TaskId) -> StorageResult<Option<TaskRecord>> {
        self.record_by_id("tasks", id.get()).await
    }

    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<TaskRecord>> {
        check_limit(limit)?;
        self.check_open().await?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let mut filter = Filter::default();
        if let Some(conversation) = query.conversation {
            filter.eq("conversation_id", int(write_id(conversation.get())?));
        }
        if let Some(kind) = &query.kind {
            filter.eq("kind", text(kind.as_str()));
        }
        if let Some(status) = query.status {
            filter.eq("status", text(tag(&status)?));
        }
        if let Some(flag) = query.abort_requested {
            filter.eq("abort_requested", int(flag as i64));
        }
        if let Some(flag) = query.background {
            filter.eq("background", int(flag as i64));
        }
        let values = self.scan_table("tasks", filter, start, limit).await?;
        Ok(page_of(values, limit, start.order, |t: &TaskRecord| {
            t.id.get()
        }))
    }

    async fn submission(&self, id: SubmissionId) -> StorageResult<Option<SubmissionRecord>> {
        self.record_by_id("submissions", id.get()).await
    }

    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<SubmissionRecord>> {
        check_limit(limit)?;
        self.check_open().await?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let mut filter = Filter::default();
        if let Some(conversation) = query.conversation {
            filter.eq("conversation_id", int(write_id(conversation.get())?));
        }
        if let Some(status) = query.status {
            filter.eq("status", text(tag(&status)?));
        }
        let values = self.scan_table("submissions", filter, start, limit).await?;
        Ok(page_of(
            values,
            limit,
            start.order,
            |s: &SubmissionRecord| s.id.get(),
        ))
    }

    async fn submission_by_request(
        &self,
        conversation: ConversationId,
        request_id: &str,
    ) -> StorageResult<Option<SubmissionRecord>> {
        let Some(raw) = read_id(conversation.get()) else {
            self.check_open().await?;
            return Ok(None);
        };
        match self
            .one(
                "SELECT record FROM submissions WHERE conversation_id = ?1 AND request_id = ?2",
                vec![int(raw), text(request_id)],
            )
            .await?
        {
            Some(row) => Ok(Some(decode(&row[0])?)),
            None => Ok(None),
        }
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
    ) -> StorageResult<Option<DocumentRecord>> {
        self.check_open().await?;
        let Ok((scope_kind, owner_id)) = scope_columns(&address.scope) else {
            return Ok(None); // 超界的 ID 不可能存在
        };
        let sql = format!(
            "SELECT {DOCUMENT_COLUMNS} FROM documents WHERE kind = ?1 AND scope_kind = ?2 \
             AND owner_id = ?3 AND key_value = ?4 AND retired_seq IS NULL"
        );
        match self
            .one(
                &sql,
                vec![
                    text(address.kind.as_str()),
                    text(scope_kind),
                    int(owner_id),
                    text(encode_key(&address.key)),
                ],
            )
            .await?
        {
            Some(row) => Ok(Some(Self::document_record(&row)?)),
            None => Ok(None),
        }
    }

    async fn document(&self, id: DocumentId) -> StorageResult<Option<DocumentRecord>> {
        let Some(raw) = read_id(id.get()) else {
            self.check_open().await?;
            return Ok(None);
        };
        let sql = format!(
            "SELECT {DOCUMENT_COLUMNS} FROM documents WHERE id = ?1 AND retired_seq IS NULL"
        );
        match self.one(&sql, vec![int(raw)]).await? {
            Some(row) => Ok(Some(Self::document_record(&row)?)),
            None => Ok(None),
        }
    }

    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<DocumentRecord>> {
        check_limit(limit)?;
        self.check_open().await?;
        let start = scan_start(Some(ScanOrder::Ascending), cursor, ScanOrder::Ascending)?;
        let Ok((scope_kind, owner_id)) = scope_columns(&query.scope) else {
            return Ok(Page {
                items: Vec::new(),
                next: None,
            });
        };
        let mut filter = Filter::default();
        filter.eq("scope_kind", text(scope_kind));
        filter.eq("owner_id", int(owner_id));
        filter.raw("retired_seq IS NULL");
        if let Some(kind) = &query.kind {
            filter.eq("kind", text(kind.as_str()));
        }
        if let Some(after) = start.after {
            match read_id(after) {
                Some(after) => filter.cmp("id", ">", int(after)),
                None => {
                    return Ok(Page {
                        items: Vec::new(),
                        next: None,
                    });
                }
            }
        }
        let sql = format!(
            "SELECT {DOCUMENT_COLUMNS} FROM documents{} ORDER BY id ASC LIMIT {}",
            filter.where_sql(),
            take(limit)
        );
        let values = self
            .query(&sql, filter.params)
            .await?
            .iter()
            .map(|row| Self::document_record(row))
            .collect::<StorageResult<Vec<_>>>()?;
        Ok(page_of(values, limit, start.order, |d: &DocumentRecord| {
            d.id.get()
        }))
    }

    async fn session(&self, id: &str) -> StorageResult<Option<SessionRecord>> {
        match self
            .one("SELECT record FROM sessions WHERE id = ?1", vec![text(id)])
            .await?
        {
            Some(row) => Ok(Some(decode(&row[0])?)),
            None => Ok(None),
        }
    }

    async fn session_by_request(&self, key: &str) -> StorageResult<Option<SessionRecord>> {
        match self
            .one(
                "SELECT record FROM sessions WHERE request_key = ?1",
                vec![text(key)],
            )
            .await?
        {
            Some(row) => Ok(Some(decode(&row[0])?)),
            None => Ok(None),
        }
    }

    async fn scan_sessions(
        &self,
        query: &SessionQuery,
        limit: usize,
        cursor: Option<&SessionCursor>,
    ) -> StorageResult<SessionPage> {
        check_limit(limit)?;
        self.check_open().await?;
        let (order, after) = session_scan_start(query.order, cursor)?;
        let mut filter = Filter::default();
        if let Some(lifecycle) = query.lifecycle {
            filter.eq("lifecycle", text(tag(&lifecycle)?));
        }
        if let Some(after) = after {
            let op = match order {
                ScanOrder::Ascending => ">",
                ScanOrder::Descending => "<",
            };
            filter.cmp("id", op, text(after));
        }
        let sql = format!(
            "SELECT record FROM sessions{} ORDER BY id {} LIMIT {}",
            filter.where_sql(),
            direction(order),
            take(limit)
        );
        let values = self
            .query(&sql, filter.params)
            .await?
            .iter()
            .map(|row| decode(&row[0]))
            .collect::<StorageResult<Vec<SessionRecord>>>()?;
        Ok(session_page_of(values, limit, order))
    }

    async fn close(&self) -> StorageResult<()> {
        self.readers.permits.close();
        self.readers.free().clear();
        self.line.close().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use komo_kernel::durable::conformance;
    use serde_json::json;

    use super::*;

    /// D1 的 conformance 原样跑在 Turso 上：每个用例一个全新的库目录。
    #[tokio::test]
    async fn passes_the_storage_conformance_suite() {
        let root = std::env::temp_dir().join(format!("komo-conformance-{}", std::process::id()));
        let counter = std::sync::atomic::AtomicUsize::new(0);
        conformance::run_all(|| {
            let dir: PathBuf = root.join(
                counter
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    .to_string(),
            );
            async move {
                std::fs::create_dir_all(&dir).unwrap();
                Arc::new(TursoStorage::open(&dir).await.unwrap()) as Arc<dyn Storage>
            }
        })
        .await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn minting_and_stored_state_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let first = TursoStorage::open(dir.path()).await.unwrap();
        assert_eq!(first.mint_id().await.unwrap(), 2);
        first
            .commit(&[
                StorageWrite::Conversation(ConversationRecord::root(ConversationId::new(1))),
                StorageWrite::Entry(EntryRecord::new(
                    EntryId::new(40),
                    ConversationId::new(1),
                    "message",
                )),
            ])
            .await
            .unwrap();
        // 铸过但没提交的号不要求保留；已存的最大 ID 必须被越过。
        first.close().await.unwrap();
        drop(first);

        let second = TursoStorage::open(dir.path()).await.unwrap();
        assert_eq!(second.mint_id().await.unwrap(), 41);
        assert_eq!(second.mint_id().await.unwrap(), 42);
        assert!(
            second
                .entry(EntryId::new(40))
                .await
                .unwrap()
                .is_some_and(|stored| stored.commit_seq == Seq::new(1))
        );
    }

    /// purge 删掉末尾的行后重开：取号器从持久化的高水位起，不从剩余行的最大 ID 起。
    #[tokio::test]
    async fn purged_ids_are_never_reissued_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let first = TursoStorage::open(dir.path()).await.unwrap();
        let conversation = ConversationId::new(first.mint_id().await.unwrap());
        let entry = EntryId::new(first.mint_id().await.unwrap());
        first
            .commit(&[
                StorageWrite::Conversation(ConversationRecord::root(conversation)),
                StorageWrite::Entry(EntryRecord::new(entry, conversation, "message")),
            ])
            .await
            .unwrap();
        first
            .commit(&[StorageWrite::PurgeConversations {
                conversations: vec![conversation],
            }])
            .await
            .unwrap();
        first.close().await.unwrap();
        drop(first);

        let second = TursoStorage::open(dir.path()).await.unwrap();
        let next = second.mint_id().await.unwrap();
        assert!(next > entry.get(), "{next} 复用了已 purge 的 {entry:?}");
    }

    /// 旧库没有 `max_id` 这一行：开库时按表里的最大值补上，之后照常推进。
    /// 读连接池：并发读不互相阻塞也不出错；关闭后读是 `Closed`；同一目录紧接着重开不报
    /// `database is locked`（连一次都没写过的新文件也一样，W0b 记过的 Turso 问题）。
    #[tokio::test]
    async fn the_read_pool_serves_concurrent_reads_and_reopens_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        for round in 0..3 {
            let storage = Arc::new(TursoStorage::open(dir.path()).await.unwrap());
            let mut joins = Vec::new();
            for _ in 0..(READ_CONNECTIONS * 3) {
                let storage = storage.clone();
                joins.push(tokio::spawn(async move {
                    for _ in 0..20 {
                        storage.conversation(ConversationId::new(1)).await.unwrap();
                    }
                }));
            }
            for join in joins {
                join.await.unwrap();
            }
            if round == 1 {
                let id = storage.mint_id().await.unwrap();
                storage
                    .commit(&[StorageWrite::Conversation(ConversationRecord::root(
                        ConversationId::new(id),
                    ))])
                    .await
                    .unwrap();
            }
            storage.close().await.unwrap();
            assert!(matches!(
                storage.conversation(ConversationId::new(1)).await,
                Err(StorageError::Closed)
            ));
        }
    }

    #[tokio::test]
    async fn a_database_without_the_high_water_row_gets_one_on_open() {
        let dir = tempfile::tempdir().unwrap();
        {
            let first = TursoStorage::open(dir.path()).await.unwrap();
            let id = first.mint_id().await.unwrap();
            first
                .commit(&[StorageWrite::Conversation(ConversationRecord::root(
                    ConversationId::new(id),
                ))])
                .await
                .unwrap();
            first
                .line()
                .read_connection()
                .unwrap()
                .execute("DELETE FROM durable_meta WHERE name = 'max_id'", ())
                .await
                .unwrap();
            first.close().await.unwrap();
        }
        let second = TursoStorage::open(dir.path()).await.unwrap();
        assert_eq!(second.mint_id().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn an_engine_failure_is_a_backend_error_and_every_later_commit_repeats_it() {
        let dir = tempfile::tempdir().unwrap();
        let storage = TursoStorage::open(dir.path()).await.unwrap();
        storage
            .line()
            .read_connection()
            .unwrap()
            .execute("DROP TABLE submissions", ())
            .await
            .unwrap();
        let write = |id| {
            StorageWrite::Submission(SubmissionRecord::queued_input(
                SubmissionId::new(id),
                ConversationId::new(1),
                None,
            ))
        };
        let first = storage.commit(&[write(5)]).await.unwrap_err();
        assert!(matches!(first, StorageError::Backend(_)), "{first:?}");
        assert_eq!(storage.commit(&[write(6)]).await.unwrap_err(), first);
        let progress = storage
            .commit_progress(&[StorageWrite::DocumentChange {
                id: DocumentId::new(9),
                version: 1,
                value: json!(null),
            }])
            .await
            .unwrap_err();
        assert_eq!(progress, first);
    }

    /// 进度提交在 OFF 档写下的值，被下一次结构性提交的 fsync 一起带上，重开后都在。
    #[tokio::test]
    async fn progress_values_are_there_after_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let storage = TursoStorage::open(dir.path()).await.unwrap();
        let address = DocumentAddress {
            kind: "komo.live".into(),
            scope: DocumentScope::Session,
            key: None,
        };
        storage
            .commit(&[StorageWrite::DocumentCreate(
                komo_kernel::durable::storage::DocumentCreate {
                    id: DocumentId::new(2),
                    address: address.clone(),
                    version: 1,
                    value: json!({"n": 0}),
                },
            )])
            .await
            .unwrap();
        for n in 1..=5 {
            storage
                .commit_progress(&[StorageWrite::DocumentChange {
                    id: DocumentId::new(2),
                    version: 1,
                    value: json!({"n": n}),
                }])
                .await
                .unwrap();
        }
        storage.close().await.unwrap();
        drop(storage);
        let storage = TursoStorage::open(dir.path()).await.unwrap();
        let doc = storage.find_document(&address).await.unwrap().unwrap();
        assert_eq!(doc.value, json!({"n": 5}));
        assert_eq!(doc.changed_at, Seq::new(6));
    }
}
