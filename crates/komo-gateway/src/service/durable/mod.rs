//! D5：新存储（`durable.db`）与新 harness 接进 Gateway 的那一半
//! （`docs/storage-durable-d5.md`）。
//!
//! 只在 [`StorageMode::Durable`](super::StorageMode) 下才有这些东西：`GatewayState.durable`
//! 是 `Some`，启动不打开 `state.db`，旧的 scheduler / reconcile / audit outbox /
//! `reattach_unfinished` 一概不跑。旧代码一行不改，D7 整文件删。
//!
//! 文件按工作包预留（W0b 只建骨架，后续包往里填；各文件的归属见 §4）：
//!
//! | 文件 | 内容 | 包 |
//! |---|---|---|
//! | `core` | [`DurableCore`]：`Engine` + `Harness` + `Storage` | W0b |
//! | `agent_doc` | `komo.agent` document 的读写与 fork 复制 | W0b |
//! | `assemble` | 钩子与编排的装配、与 `GatewayState` 的断环 | W0b（W1b 填） |
//! | `background` | Durable 模式的后台循环 | W0b（W2b/W3 往里加） |

pub mod agent_doc;
pub mod assemble;
pub mod background;
pub mod core;

pub use self::core::DurableCore;
pub use assemble::StateSlot;

#[cfg(test)]
mod tests;
