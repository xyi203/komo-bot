//! 各仓储实现：CronRepo / MemoryRepo / 投递记录（§13.5），都在 `aux.db` 上。
//!
//! 一个 trait 一个模块。**写一律走 [`crate::db::Db::with_write_retry`]**（每个写，包括
//! 只有一条语句的，都在一个 `BEGIN CONCURRENT` 事务里，§8.2）；读走
//! [`crate::db::Db::read`]。
//!
//! raw SQL 只允许出现在 [`memory`]（关键词臂的 `instr`）——别处一律走 toasty 的类型化 API。
//! 理由：**`rows affected` 是唯一可用的信号**（toasty 的类型化 `UPDATE` 恒返回 `Ok(())`，
//! 命中 0 行与 1 行不可区分，§8.2 那张表）。

pub mod cron;
pub mod deliveries;
pub mod memory;
