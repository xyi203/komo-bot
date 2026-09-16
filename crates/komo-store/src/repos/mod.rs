//! 各仓储实现：ApprovalRepo / CronRepo / MemoryRepo / RunQueue 等（§13.5）。
//!
//! 一个 trait 一个模块。**写一律走 [`crate::db::Db::with_write_retry`]**（每个写，包括
//! 只有一条语句的，都在一个 `BEGIN CONCURRENT` 事务里，§8.2）；读走
//! [`crate::db::Db::read`]。
//!
//! raw SQL 只允许出现在 [`queue`]（§8.7 的四条领取语句）和 [`memory`]（关键词臂的
//! `instr`）——别处一律走 toasty 的类型化 API，那是"拿不到受影响行数"这件事真正要紧的
//! 地方之外的全部。
//!
//! [`session`]、[`calls`]、[`outbox`] 不是 trait 的实现：它们是 store 内部的具体类型，
//! 只被 [`crate::coordinator::Coordinator`] 用（§13.5 末尾）。

pub mod approvals;
pub mod calls;
pub mod cron;
pub mod deliveries;
pub mod memory;
pub mod outbox;
pub mod queue;
pub mod runs;
pub mod session;
