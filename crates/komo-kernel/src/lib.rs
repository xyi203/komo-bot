//! komo 的内核层：值类型、状态机、纯函数与全部 trait（docs/komo_bot.md §13.4、§13.5）。

pub mod cron;
pub mod events;
pub mod fold;
pub mod policy;
pub mod projection;
pub mod protocol;
pub mod recovery;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod traits;
pub mod types;
