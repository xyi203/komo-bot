//! `Storage` trait（pi-durable spec §10；设计 §3.7）。
//!
//! 一条提交线之下的存储契约：`commit` 原子，读到的都是已提交的状态，`mint_id` 在整个库
//! 里单调。与 pi 的差别：没有 `Context`（取消靠 future 被 drop）；document 只存整值，
//! 没有历史回看与 `document.copy`；cursor 是带方向的 `{after, order}`。
//!
//! 存储相信上层给的记录、引用和状态转移是语义上合法的，自己只保证：提交原子、全局 ID
//! 归属、conversation / entry / document 创建后不可变、读出的值是独立拷贝。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::{ConversationId, DocumentId, EntityId, EntryId, Seq, SubmissionId, TaskId};
use super::records::{
    ConversationRecord, DocumentAddress, DocumentRecord, DocumentScope, EntryRecord,
    SubmissionRecord, SubmissionStatus, TaskRecord, TaskStatus,
};

/// 存储错误。除 [`StorageError::Request`] 之外都对打开它的 Session 是致命的（spec §10.1）：
/// 调用方让整条线失败、重开，从已提交状态恢复。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// 读请求本身无效（未知 conversation、坏 cursor、方向不符、limit 为 0……），没有任何
    /// 持久副作用，只让这一次调用失败。
    #[error("invalid storage request: {0}")]
    Request(String),
    /// 提交被拒（ID 已被占用、document 地址冲突……）：整批不生效。
    #[error("commit rejected: {0}")]
    Rejected(String),
    #[error("ID space is exhausted")]
    IdSpaceExhausted,
    #[error("storage is closed")]
    Closed,
    /// 后端自己的故障（I/O、损坏）。
    #[error("storage backend failed: {0}")]
    Backend(String),
}

impl StorageError {
    pub fn request(message: impl Into<String>) -> Self {
        StorageError::Request(message.into())
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        StorageError::Rejected(message.into())
    }

    /// 是否只是"请求无效"：这一类不让 Session 失败。
    pub fn is_request(&self) -> bool {
        matches!(self, StorageError::Request(_))
    }
}

pub type StorageResult<T> = Result<T, StorageError>;

/// 扫描的 ID 次序：升序 = 最旧的在前。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScanOrder {
    Ascending,
    Descending,
}

/// 后端自有的续扫位置；调用方只把它原样交回同一个扫描。带着方向：拿 cursor 续扫时按它
/// 的方向走，查询里不写 `order` 也行，写了但不同则报 [`StorageError::Request`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    after: u64,
    order: ScanOrder,
}

impl Cursor {
    /// 上一页最后一条的 ID 与该次扫描的方向。
    pub fn new(after: u64, order: ScanOrder) -> Self {
        Self { after, order }
    }

    pub fn after(&self) -> u64 {
        self.after
    }

    pub fn order(&self) -> ScanOrder {
        self.order
    }
}

/// 一页结果；`next` 为空表示没有更多。
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<Cursor>,
}

/// 一次扫描从哪里、朝哪个方向走。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanStart {
    pub order: ScanOrder,
    pub after: Option<u64>,
}

/// 解出扫描的方向与位置：有 cursor 就沿它的方向续扫，`requested` 与它不同则报错。
pub fn scan_start(
    requested: Option<ScanOrder>,
    cursor: Option<&Cursor>,
    default: ScanOrder,
) -> StorageResult<ScanStart> {
    match cursor {
        None => Ok(ScanStart {
            order: requested.unwrap_or(default),
            after: None,
        }),
        Some(cursor) => match requested {
            Some(requested) if requested != cursor.order => Err(StorageError::request(format!(
                "the cursor continues a {:?} scan; the query asks for {:?}",
                cursor.order, requested
            ))),
            _ => Ok(ScanStart {
                order: cursor.order,
                after: Some(cursor.after),
            }),
        },
    }
}

/// 把最多 `limit + 1` 条已按扫描方向排好的结果切成一页。
pub fn page_of<T>(
    mut values: Vec<T>,
    limit: usize,
    order: ScanOrder,
    id: impl Fn(&T) -> u64,
) -> Page<T> {
    if values.len() <= limit {
        return Page {
            items: values,
            next: None,
        };
    }
    values.truncate(limit);
    let next = values.last().map(|last| Cursor::new(id(last), order));
    Page {
        items: values,
        next,
    }
}

/// `limit` 必须是正数（0 页什么也读不到，却会让 cursor 悬空）。
pub fn check_limit(limit: usize) -> StorageResult<()> {
    if limit == 0 {
        return Err(StorageError::request("limit must be positive"));
    }
    Ok(())
}

// ---------------------------------------------------------------------- queries

/// 默认升序。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationQuery {
    pub owner_conversation: Option<ConversationId>,
    pub owner_task: Option<TaskId>,
    pub order: Option<ScanOrder>,
}

/// 一个 conversation 沿 fork 祖先拼出来的可见历史里的 ID 范围（含两端）。默认**降序**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryQuery {
    pub conversation: ConversationId,
    pub min: Option<EntryId>,
    pub max: Option<EntryId>,
    pub order: Option<ScanOrder>,
}

impl EntryQuery {
    pub fn new(conversation: ConversationId) -> Self {
        Self {
            conversation,
            min: None,
            max: None,
            order: None,
        }
    }
}

/// 默认升序。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskQuery {
    pub conversation: Option<ConversationId>,
    pub kind: Option<String>,
    pub status: Option<TaskStatus>,
    pub abort_requested: Option<bool>,
    pub background: Option<bool>,
    pub order: Option<ScanOrder>,
}

/// 默认升序。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubmissionQuery {
    pub conversation: Option<ConversationId>,
    pub status: Option<SubmissionStatus>,
    pub order: Option<ScanOrder>,
}

/// 一个作用域里活着的 document，按 ID 升序（没有 `order`：cursor 必须是升序的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentQuery {
    pub scope: DocumentScope,
    pub kind: Option<String>,
}

/// entry 与创建它的提交序号。
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEntry {
    pub entry: EntryRecord,
    pub commit_seq: Seq,
}

// ----------------------------------------------------------------------- writes

/// 创建 document 时调用方给的字段；`created_at` / `changed_at` 由存储在提交时盖章。
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentCreate {
    pub id: DocumentId,
    pub address: DocumentAddress,
    pub version: u32,
    pub value: Value,
}

/// 一批提交里的一条写入。
#[derive(Debug, Clone, PartialEq)]
pub enum StorageWrite {
    /// 新建，之后不可变。
    Conversation(ConversationRecord),
    /// 新建，之后不可变。
    Entry(EntryRecord),
    /// task / submission 整条替换（ID 已存在则覆盖）。
    Task(TaskRecord),
    Submission(SubmissionRecord),
    DocumentCreate(DocumentCreate),
    /// 整值替换（同时带上定义版本，版本变更就是换值）。
    DocumentChange {
        id: DocumentId,
        version: u32,
        value: Value,
    },
    /// 退役；同一批里先应用内容再退役，与写入顺序无关。
    DocumentRetire {
        id: DocumentId,
    },
}

// ------------------------------------------------------------------------ trait

/// 存储契约。实现可以是内存、Turso 等；同一套 conformance 用例（`conformance`）都要过。
///
/// 一旦 `commit` 返回，之后经同一个 `Storage` 的读都能看到它。每个方法的错误对 Session
/// 都是终局的，所以后端要自己重试瞬时故障；读上的 [`StorageError::Request`] 例外。
#[async_trait::async_trait]
pub trait Storage: Send + Sync {
    /// 原子提交一批写入，返回这次提交的序号（严格递增，可有空洞）。任何一条被拒整批不生效。
    async fn commit(&self, writes: &[StorageWrite]) -> StorageResult<Seq>;

    /// 在整个库唯一的数字命名空间里铸一个比已铸过、已存过的都大的 ID（跨重开也成立）。
    /// 铸成什么实体类型用 [`StorageExt::mint`]。
    async fn mint_id(&self) -> StorageResult<u64>;

    async fn conversation(&self, id: ConversationId) -> StorageResult<Option<ConversationRecord>>;
    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<ConversationRecord>>;

    /// 按 ID 全局查 entry，不管它属于哪个 conversation。
    async fn entry(&self, id: EntryId) -> StorageResult<Option<StoredEntry>>;
    /// 只在 `id` 从 `conversation` 的 fork 祖先链上可见时返回；未知 conversation 是请求无效。
    async fn visible_entry(
        &self,
        conversation: ConversationId,
        id: EntryId,
    ) -> StorageResult<Option<StoredEntry>>;
    /// `at_or_before`（含）以内、对 `conversation` 可见的最新一条带 `head` 的 entry。
    async fn find_latest_head_marker(
        &self,
        conversation: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> StorageResult<Option<EntryRecord>>;
    /// 在可见历史里按 ID 范围分页，处处套用祖先上限。降序默认；升序先读根一段再读各 fork。
    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<EntryRecord>>;

    async fn task(&self, id: TaskId) -> StorageResult<Option<TaskRecord>>;
    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<TaskRecord>>;

    async fn submission(&self, id: SubmissionId) -> StorageResult<Option<SubmissionRecord>>;
    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<SubmissionRecord>>;
    /// conversation 内按 `request_id` 去重的那条 submission。
    async fn submission_by_request(
        &self,
        conversation: ConversationId,
        request_id: &str,
    ) -> StorageResult<Option<SubmissionRecord>>;

    /// 一个逻辑地址上当前活着的 document；退役了的读不到。
    async fn find_document(
        &self,
        address: &DocumentAddress,
    ) -> StorageResult<Option<DocumentRecord>>;
    async fn document(&self, id: DocumentId) -> StorageResult<Option<DocumentRecord>>;
    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> StorageResult<Page<DocumentRecord>>;

    /// 关闭之后所有操作都报 [`StorageError::Closed`]。
    async fn close(&self) -> StorageResult<()>;
}

/// `mint_id` 的类型化包装；`dyn Storage` 上也能用。
#[async_trait::async_trait]
pub trait StorageExt: Storage {
    async fn mint<I: EntityId + Send>(&self) -> StorageResult<I> {
        Ok(I::from(self.mint_id().await?))
    }
}

impl<S: Storage + ?Sized> StorageExt for S {}

/// 铸 ID 的上限：后端的整数列是有符号 64 位。
pub const MAX_ID: u64 = i64::MAX as u64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_direction_wins_and_a_conflicting_order_is_a_request_error() {
        let cursor = Cursor::new(5, ScanOrder::Descending);
        let start = scan_start(None, Some(&cursor), ScanOrder::Ascending).unwrap();
        assert_eq!(start.order, ScanOrder::Descending);
        assert_eq!(start.after, Some(5));
        assert!(
            scan_start(
                Some(ScanOrder::Descending),
                Some(&cursor),
                ScanOrder::Ascending
            )
            .is_ok()
        );
        let error = scan_start(
            Some(ScanOrder::Ascending),
            Some(&cursor),
            ScanOrder::Ascending,
        )
        .unwrap_err();
        assert!(error.is_request());
    }

    #[test]
    fn page_of_cuts_at_the_limit_and_points_at_the_last_kept_item() {
        let page = page_of(vec![1u64, 2, 3], 2, ScanOrder::Ascending, |v| *v);
        assert_eq!(page.items, [1, 2]);
        assert_eq!(page.next, Some(Cursor::new(2, ScanOrder::Ascending)));
        assert_eq!(
            page_of(vec![1u64, 2], 2, ScanOrder::Ascending, |v| *v).next,
            None
        );
    }

    #[test]
    fn cursors_round_trip_through_json() {
        let cursor = Cursor::new(9, ScanOrder::Descending);
        let text = serde_json::to_string(&cursor).unwrap();
        assert_eq!(serde_json::from_str::<Cursor>(&text).unwrap(), cursor);
    }
}
