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

/// bin 要的几样**不经 Gateway** 的东西（§3 的命令表：`komo channel list|probe` 与
/// `komo skills *` 只读文件系统与配置）。
///
/// 从这里再导一次，而不是让 bin 直接依赖 `komo-runtime` / `komo-agent`：§13.4 给 bin
/// 的依赖是 client / gateway / clap 三条，加更多会把二进制挪到另一条依赖边上。
pub use komo_agent::skills;
pub use komo_runtime::config;
/// `komo auth codex login|status`（§3、§13.3）：设备码登录与凭证文件，全部实现在
/// `komo-runtime`（不碰 axum / ratatui，只要 reqwest），这里只转一道给 bin。
pub use komo_runtime::llm::codex_auth;
/// `komo toolbox` 的那几个类型（§5.3）。同一个理由：bin 的依赖是 client / gateway /
/// clap 三条，不加第四条。
pub use komo_runtime::toolbox;

pub use dispatcher::Dispatcher;
pub use lock::{DiscoveryFile, GatewayDiscovery, InstanceLock};
pub use notifier::HomeNotifier;
pub use service::{Running, ServiceError, ServiceOptions, run, start};
