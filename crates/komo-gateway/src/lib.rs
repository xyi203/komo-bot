//! Gateway：HTTP/SSE 接口、进程锁与发现文件、Dispatcher 与三个聊天渠道
//! （docs/komo_bot.md §13.1、§13.4）。

pub mod auth;
pub mod channels;
pub mod deliveries;
pub mod dispatcher;
pub mod http;
pub mod lock;
pub mod notifier;
pub mod reload;
pub mod render;
pub mod service;
pub mod sse;
