//! pi-durable 提交模型的纯值层（docs/storage-durable.md §9 D1）。
//!
//! 数字 ID、持久记录、上下文推导（spec §2.1 规则 1–10）、fork 遍历、`Storage` trait，
//! 加一个内存实现与可复用的 conformance 套件。这里没有 I/O、没有 tokio；store / runtime
//! 在后续阶段才开始用它，旧的 ledger 与 session 树照常并存。

pub mod compaction;
#[cfg(any(test, feature = "test-support"))]
pub mod conformance;
pub mod context;
pub mod fork;
pub mod ids;
pub mod legacy_ids;
#[cfg(any(test, feature = "test-support"))]
pub mod memory;
pub mod message;
pub mod records;
pub mod storage;
pub mod task;

pub use context::{ContextView, derive_context, order_tool_results};
pub use fork::{ForkError, ForkSegment, fork_chain, visible_entries};
pub use ids::{
    ConversationId, DocumentId, EntityId, EntryId, ROOT_CONVERSATION_ID, Seq, SubmissionId, TaskId,
};
#[cfg(any(test, feature = "test-support"))]
pub use memory::MemoryStorage;
pub use message::{
    AssistantMessage, Content, ContentBlock, Message, Sections, StopReason, SystemMessage,
    ToolResultMessage, UserMessage,
};
pub use records::{
    ContextEdit, ConversationRecord, DocumentAddress, DocumentRecord, DocumentScope, EditAction,
    EntryRecord, InputState, SessionKind, SessionLifecycle, SessionRecord, SubmissionKind,
    SubmissionRecord, SubmissionSettlement, SubmissionStatus, TaskOutcome, TaskRecord, TaskState,
    TaskStatus, WriteState,
};
pub use storage::{
    Cursor, Page, ScanOrder, SessionCursor, SessionPage, SessionQuery, Storage, StorageError,
    StorageExt, StorageWrite, check_progress,
};
