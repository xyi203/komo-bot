//! Gateway：HTTP/SSE 接口、进程锁与发现文件（docs/komo_bot.md §13.1、§13.4）。

pub mod auth;
pub mod http;
pub mod inbox;
pub mod lock;
pub mod oidc;
pub mod push;
pub mod reload;
pub mod service;
pub mod sse;
pub mod telemetry;
pub mod web;

/// bin 要的几样**不经 Gateway** 的东西（§3 的命令表：`komo skills *` 只读文件系统与配置）。
///
/// 从这里再导一次，而不是让 bin 直接依赖 `komo-config` / `komo-llm` / `komo-agent`：§13.4 给 bin
/// 的依赖是 client / gateway / clap（外加 `__codemode-worker` 要的 komo-tools），加更多会把
/// 二进制挪到另一条依赖边上。
pub use komo_agent::skills;
pub use komo_config as config;
/// `komo auth codex login|status`（§3、§13.3）：设备码登录与凭证文件，全部实现在
/// `komo-llm`（不碰 axum，只要 reqwest），这里只转一道给 bin。
pub use komo_llm::codex_auth;
/// `komo auth mcp login|status`（`docs/mcp.md` §2.1）：同上，OAuth 登录与凭证文件在
/// `komo-tools`。
pub use komo_tools::mcp_auth;

pub use lock::{DiscoveryFile, GatewayDiscovery, InstanceLock};
pub use service::{Running, ServiceError, ServiceOptions, run, start};
pub use telemetry::TelemetryStatus;
