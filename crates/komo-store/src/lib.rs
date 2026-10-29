//! 持久化层：新存储（`durable`）、`aux.db` 上的 cron / 投递 / 记忆仓储，以及 Session 目录的
//! 内容（docs/komo_bot.md §8.2、§13.4）。
//!
//! 两个库文件、一个目录，各管各的：
//!
//! - **`durable.db`**（[`durable`]）：会话、提交、收件箱、任务——串行提交线，WAL，提交即 fsync。
//! - **`aux.db`**（[`db::Db`] + toasty，[`models`] / [`repos`]）：cron、投递记录、记忆、事项。
//!   跨库靠预分配 ID 与请求键幂等。
//! - **Session 目录**（[`session_dir`] / [`tool_output`]）：工具输出与产物的内容文件。
//!   写入顺序是 §8.5 的箭头：**外置正文 → 数据库提交**，崩溃后靠对账收尾。
//!
//! ```text
//! durable::TursoStorage   durable.db：Line（串行提交线）+ blob 先落后引用
//! db::Db ─ models/        aux.db：Turso + toasty；repos/ 是各 trait 的实现
//! FileToolOutputStore     tool-output/<conversation>/<task>/<attempt>/
//! ```

pub mod db;
pub mod durable;
pub mod models;
pub mod purge;
pub mod repos;
pub mod session_dir;
pub mod tool_output;

pub use db::{Db, DbOptions, RetryConfig};
pub use repos::cron::TursoCronRepo;
pub use repos::deliveries::{DeliveryRecord, TursoDeliveryRepo};
pub use repos::matter::TursoMatterRepo;
pub use repos::memory::{TursoMemoryRepo, lexical_terms};
pub use session_dir::SessionPaths;
pub use tool_output::FileToolOutputStore;

#[cfg(feature = "test-support")]
pub mod test_support;
