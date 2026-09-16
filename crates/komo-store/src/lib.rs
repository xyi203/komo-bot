//! 持久化层：Session 目录的三个写入点与 Turso 存储（docs/komo_bot.md §8.2、§13.4）。
//!
//! 两个持久化边界，各自是真的落盘，但**合起来不是一个跨文件原子事务**（§8.2）：
//!
//! - **Session 目录**是内容的权威。每一步先 `sync_all` 再往下走。
//! - **state.db**（Turso，MVCC）是调度与授权的权威。提交即 fsync。
//!
//! 所以写入顺序永远是 §8.5 的那条箭头：**外置正文（如有）→ JSONL 追加并同步 →
//! 数据库事务**。[`coordinator::Coordinator`] 的每个方法内部完成这一顺序，它是
//! `Ledger` 的生产实现，也是 Agent Loop 的唯一写入口。
//!
//! 模块之间的关系：
//!
//! ```text
//! Coordinator ─┬─ payloads::PayloadStore   外置正文（内容寻址）
//!              ├─ session_log::SessionLog  events.jsonl（分配 seq、sync_all、尾部校验）
//!              └─ db::Db ─ models/         Turso + toasty；repos/ 是各 trait 的实现
//! FileToolOutputStore  tool-output/<run>/<call>/<attempt>/（独立于 Coordinator）
//! ```

pub mod checkpoint;
pub mod coordinator;
pub mod db;
pub mod models;
pub mod payloads;
pub mod repos;
pub mod session_log;
pub mod tool_output;

pub use checkpoint::{CheckpointRecord, CheckpointStore};
pub use coordinator::Coordinator;
pub use db::{Db, DbOptions, RetryConfig};
pub use payloads::PayloadStore;
pub use repos::approvals::TursoApprovalRepo;
pub use repos::cron::TursoCronRepo;
pub use repos::deliveries::{DeliveryRecord, TursoDeliveryRepo};
pub use repos::memory::{TursoMemoryRepo, lexical_terms};
pub use repos::queue::{TursoRunQueue, reclaim_abandoned_runs};
pub use session_log::{SessionLog, SessionPaths, TailExpectation, TailRepair};
pub use tool_output::FileToolOutputStore;

#[cfg(feature = "test-support")]
pub mod test_support;
