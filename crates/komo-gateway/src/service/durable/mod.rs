//! 新存储（`durable.db`）与新 harness 接进 Gateway 的那一半（`docs/storage-durable-d5.md`）。
//!
//! Gateway 只有这一套存储（D7a 删掉了旧的 `state.db` 路径）：`GatewayState.durable` 恒在，启动不打开
//! `state.db`，没有 scheduler / reconcile / audit outbox / `reattach_unfinished`。
//!
//! 按职责分子模块；表里的"包"是 `docs/storage-durable-d5.md` §4 的工作包：
//!
//! | 模块 | 内容 | 包 |
//! |---|---|---|
//! | `core` | [`DurableCore`]：`Engine` + `Harness` + `Storage` | W0b |
//! | `assemble` | 钩子与编排的装配、与 `GatewayState` 的断环 | W0b（W1b 填） |
//! | `hooks` | `AgentHooks`：setup / 工具表 / 系统段 / 压缩（按当前配置现解析） | W1b |
//! | `orchestration` | `OrchestrationHooks`：`check_effort`、契约段、监听委托 `MonitorRunner` | W1b |
//! | `komo` | `komo` 工具的服务：调 HTTP 路由背后的同一个处理函数 | — |
//! | **`sessions`** | 会话与树：建会话、列表、`SessionSummary` 映射、主会话与按来源找会话、`SessionError` | W1c |
//! | `sessions::agent_doc` | `komo.agent` document 的读写与 fork 复制 | W0b |
//! | `sessions::tree` | 读树（`read_tree`）、导航、label / name、context-edit、分支摘要、compact、忙时守卫 | W1c |
//! | `sessions::copy` | 分叉（网页会话菜单）：复制 entry 与 blob | W1c |
//! | `sessions::runs` | `RunSummary` / `RunDetail` / `RunState` 映射、按 RunId 取消 | W2a |
//! | `sessions::tasks` | 任务会话的展示：短号 `#<conversation>`、列表一行、标签 | W2c |
//! | `sessions::titles` | 首轮跑完后起标题（读写 sessions 行） | W3a |
//! | `sessions::search` | 跨会话找回（标题与正文） | S3 |
//! | `sessions::lifecycle` | delete / purge / 启动补做 `closing` / reconcile（只查 blob 引用） | W4a |
//! | **`submit`** | 提交受理：路由到当前分支、惰性 fork、model / effort 覆盖、`/new`、`/cancel` | W1c |
//! | `submit::shell` | `!` 命令直跑（不建 run），落 `shell.command` write | W2a |
//! | **`interventions`** | faulted 任务清单、`abandon` 确认、NeedsAttention 投递 | W4a |
//! | `interventions::confirms` | 待确认的对外调用（`docs/confirm.md` §6） | — |
//! | **`feed`** | 提交订阅 → SSE 帧（投影、`AssistantDelta`）与事件补读 | W2b |
//! | `feed::nested_view` | codemode 子调用给界面的视图 | — |
//! | **`background`** | Durable 模式的后台循环 | W0b（W2b/W3 往里加） |
//! | `background::monitor_runner` | `MonitorRunner`：监听与转后台命令的进程、泵、合批、时限、遗留进程核对 | W2c |
//! | `background::watch` | 看客：终态回给输入的来源对端、并入通知、重启补挂 | W3a |
//! | `background::cron` | cron 的 `CronTarget`（一次提交建会话 + 受理）、重启补做、tick 的 spawn | W3b |
//! | `background::cron_watch` | 按 run 终态帧（`komo.settled`）回写触发状态、按 `notify` 投递 | W3b |
//! | `background::memory` | 记忆的装配（`DurableSessionEvents` / `DurableMemoryWork`）与 `MEMORY_TICK` | W3c |

pub mod assemble;
pub mod background;
pub mod core;
pub mod feed;
pub mod hooks;
pub mod interventions;
pub mod komo;
pub mod orchestration;
pub mod sessions;
pub mod submit;

pub use self::core::DurableCore;
pub use assemble::StateSlot;
pub use sessions::SessionError;

#[cfg(test)]
mod tests;
