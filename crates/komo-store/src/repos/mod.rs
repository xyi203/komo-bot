//! 各仓储实现：ApprovalRepo / CronRepo / MemoryRepo / RunQueue 等（§13.5）。
//!
//! 一个 trait 一个模块。**写一律走 [`crate::db::Db::with_write_retry`]**（每个写，包括
//! 只有一条语句的，都在一个 `BEGIN CONCURRENT` 事务里，§8.2）；读走
//! [`crate::db::Db::read`]。
//!
//! raw SQL 只允许出现在 [`queue`]（§8.7 的领取 / 回收 / 租约语句）、[`memory`]（关键词臂
//! 的 `instr`）与 [`session`]（生命周期状态的 CAS）——别处一律走 toasty 的类型化 API。
//! 这三个地方的理由是同一个：**`rows affected` 是唯一可用的信号**（toasty 的类型化
//! `UPDATE` 恒返回 `Ok(())`，命中 0 行与 1 行不可区分，§8.2 那张表）。
//!
//! [`session`]、[`calls`]、[`outbox`] 不是 trait 的实现：它们是 store 内部的具体类型，
//! 只被 [`crate::coordinator::Coordinator`] 用（§13.5 末尾）。[`interventions`] 与
//! [`reconcile`] 也不是 trait 实现：它们是 §7.5 / §8.9 的**派生查询与判定**，由 runtime
//! 的清单接口与对账扫描直接调。

pub mod approvals;
pub mod calls;
pub mod cron;
pub mod deliveries;
pub mod interventions;
pub mod memory;
pub mod outbox;
pub mod queue;
pub mod reconcile;
pub mod recovery;
pub mod runs;
pub mod session;
