//! 提交层与任务引擎的错误。

use komo_kernel::durable::{StorageError, TaskId, task::StateError};

#[derive(Debug, Clone, thiserror::Error)]
pub enum DurableError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// Session 正在关闭或已关闭：不再受理提交。
    #[error("the session is closed")]
    Closed,
    /// 存储出了致命错误，Session 已失败（spec §10.1）；重开后从已提交状态恢复。
    #[error("the session failed: {0}")]
    Failed(String),
    /// invocation 已经结束：它的 runtime 操作都拒绝。
    #[error("task {0} invocation has ended")]
    InvocationEnded(TaskId),
    /// 提交内容不合规（所有者不活、等待成环、任务已 terminal……），整笔不生效。
    #[error("{0}")]
    Rejected(String),
    #[error("table read `{0}` after a table write in the same commit")]
    ReadAfterWrite(&'static str),
    #[error("task {0} does not exist")]
    NoSuchTask(TaskId),
}

impl DurableError {
    pub fn rejected(message: impl Into<String>) -> Self {
        DurableError::Rejected(message.into())
    }

    /// 让整条线失败的错误：存储的非"请求无效"错误。
    pub fn is_fatal_storage(&self) -> bool {
        matches!(self, DurableError::Storage(e) if !e.is_request())
    }

    /// 只是"线已关闭 / 已失败"，不必再报告。
    pub fn is_shutdown(&self) -> bool {
        matches!(self, DurableError::Closed | DurableError::Failed(_))
    }
}

impl From<StateError> for DurableError {
    fn from(error: StateError) -> Self {
        DurableError::Rejected(error.to_string())
    }
}

/// phase handler / abort handler 返回的错误：只让这个任务 `faulted`。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct TaskError(pub String);

impl TaskError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl From<DurableError> for TaskError {
    fn from(error: DurableError) -> Self {
        Self(error.to_string())
    }
}

impl From<&str> for TaskError {
    fn from(message: &str) -> Self {
        Self(message.to_owned())
    }
}

impl From<String> for TaskError {
    fn from(message: String) -> Self {
        Self(message)
    }
}
