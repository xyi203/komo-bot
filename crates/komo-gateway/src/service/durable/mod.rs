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
//! | `hooks` | `AgentHooks`：setup / 工具表 / 系统段 / 压缩（按当前配置现解析） | W1b |
//! | `orchestration` | `OrchestrationHooks`：`check_effort`、契约段、监听委托 `MonitorRunner` | W1b |
//! | `background` | Durable 模式的后台循环 | W0b（W2b/W3 往里加） |
//! | `sessions` | 建会话、列表、`SessionSummary` 映射、主会话与按来源找会话、`SessionError` | W1c |
//! | `submit` | 提交输入：路由到当前分支、惰性 fork、model / effort 覆盖、`/new`、`/cancel` | W1c |
//! | `tree` | 读树（`read_tree`）、导航、label / name、context-edit、分支摘要、compact、忙时守卫 | W1c |
//! | `copy` | `/fork`、`/clone`：复制 entry 与 blob | W1c |
//! | `runs` | `RunSummary` / `RunDetail` / `RunState` 映射、按 RunId 取消 | W2a |
//! | `shell` | `!` 命令直跑（不建 run），落 `shell.command` write | W2a |
//! | `monitor_runner` | `MonitorRunner`：监听与转后台命令的进程、泵、合批、时限、遗留进程核对 | W2c |
//! | `tasks` | 任务会话的展示：短号 `#<conversation>`、列表一行、标签 | W2c |
//! | `watch` | 看客：终态回给输入的来源对端、并入通知、重启补挂 | W3a |
//! | `titles` | 首轮跑完后起标题（读写 sessions 行） | W3a |
//! | `cron` | cron 的 `CronTarget`（一次提交建会话 + 受理）、重启补做、tick 的 spawn | W3b |
//! | `cron_watch` | 按 run 终态帧（`komo.settled`）回写触发状态、按 `notify` 投递 | W3b |
//! | `feed` | 提交订阅 → SSE 帧（投影、`AssistantDelta`）与事件补读 | W2b |

pub mod agent_doc;
pub mod assemble;
pub mod background;
pub mod copy;
pub mod core;
pub mod cron;
pub mod cron_watch;
pub mod feed;
pub mod hooks;
pub mod monitor_runner;
pub mod orchestration;
pub mod runs;
pub mod sessions;
pub mod shell;
pub mod submit;
pub mod tasks;
pub mod titles;
pub mod tree;
pub mod watch;

pub use self::core::DurableCore;
pub use assemble::StateSlot;
pub use sessions::SessionError;

#[cfg(test)]
mod feed_tests;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod watch_tests;
