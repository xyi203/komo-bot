//! 客户端：HTTP + SSE 客户端、发现文件读取与 ratatui 聊天 TUI
//! （docs/komo_bot.md §13.4）。只认协议类型，不认识 store / runtime。
//!
//! 四个层次，依赖只向下：
//!
//! ```text
//! discovery  发现文件 + /healthz 身份核对（§3 启动顺序 1–4）
//!     ↓
//! api        §13.1 每个接口一个方法；错误体 → 带 ErrorCode 的 ClientError
//!     ↓
//! sse        按游标订阅，断线自动从最后一个 SseFrame.id 续读
//!     ↓
//! tui        ratatui 聊天前端；状态机在 tui::app，与终端无关
//! render     操作子命令的输出渲染——纯函数，输入协议类型，输出字符串
//! ```

pub mod api;
pub mod discovery;
pub mod error;
pub mod render;
pub mod sse;
#[cfg(test)]
mod test_server;
pub mod tui;

pub use api::KomoClient;
pub use error::{ClientError, ClientResult};
pub use sse::{ConnectionState, SseHandle, SseMessage};
pub use tui::{TuiMode, run_tui};
