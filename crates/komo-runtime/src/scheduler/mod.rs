//! Cron 调度：到期 → 重叠判定 → 占位 → 投出（`cron.rs`），时区解析（`zone.rs`）。
//!
//! 触发投到哪里由 gateway 的 `CronTarget` 实现决定（新存储下是一次 engine 提交）。

pub mod cron;
mod zone;

pub use cron::{
    CronError, CronScheduler, CronTarget, CronTick, FireSpec, Fired, FiredRef, SkipReason, Skipped,
    firing_key, model_for,
};
pub use zone::JiffZoneResolver;
