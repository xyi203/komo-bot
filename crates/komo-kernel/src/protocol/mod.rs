//! 协议线格式：HTTP 接口、SSE、配置快照。

pub mod config;
pub mod http;
pub mod nested;
pub mod sse;
#[cfg(feature = "ts")]
pub mod ts;

pub use config::{
    ConfigIssue, ConfigSnapshot, IssueSeverity, KeyPath, MemoryConfig, PathsConfig,
    OidcConfig, RetrievalConfig, SourceFile, StartOnly,
};
pub use http::*;
pub use sse::{Cursor, SseEvent, SseFrame};

/// 协议版本。`GET /healthz` 回它；对不上就不是同一个 komo。
pub const PROTOCOL_VERSION: u32 = 1;
