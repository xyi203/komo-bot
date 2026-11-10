//! 工具层：内置工具、工具目录、codemode 的 Monty worker、MCP 客户端与子进程登记
//! （docs/komo_bot.md §13.4）。不碰 store，也不认识 durable 的引擎——工具任务由 runtime 的
//! harness 驱动。

pub mod cancel;
pub mod codemode;
pub mod executor;
pub mod limits;
pub mod mcp;
pub mod mcp_auth;
pub mod recovery;
pub mod spawn;
pub mod tools;
