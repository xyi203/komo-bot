//! 客户端：HTTP + SSE 客户端、发现文件读取与 ratatui 聊天 TUI
//! （docs/komo_bot.md §13.4）。只认协议类型，不认识 store / runtime。

pub mod api;
pub mod discovery;
pub mod render;
pub mod sse;
pub mod tui;
