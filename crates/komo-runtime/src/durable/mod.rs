//! 通用 Task 引擎（pi-durable spec §4、§5；设计 §12.4）：提交线 + 调度器。
//!
//! 纯判定在 `komo_kernel::durable::task`；这里是线（[`Line`]，串行执行提交闭包、原子写入、
//! 提交后发布）、事务视图（[`Tx`]）、任务定义（[`TaskKind`]）与调度器（reservation、
//! invocation、abort 协议、close、开库 reconciliation）。不含 agent 的任何东西。

pub mod engine;
pub mod error;
pub mod harness;
pub mod kind;
pub mod line;
pub mod runtime;
pub mod scheduler;
pub mod tx;

#[cfg(test)]
mod tests;

pub use engine::{Engine, EngineOptions};
pub use error::{DurableError, TaskError};
pub use kind::{Migrated, NoHooks, Registry, RegistrySnapshot, Reporter, TaskHooks, TaskKind};
pub use line::{Line, Publication, Subscription, TxFuture};
pub use runtime::TaskRuntime;
pub use scheduler::{AbortOutcome, TaskInspection, TaskInspectionState};
pub use tx::{ConversationOwnership, DraftHead, EntryDraft, Ownership, TaskOptions, TaskScope, Tx};
