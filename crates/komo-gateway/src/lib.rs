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

/// bin 要的两样**不经 Gateway** 的东西（§3 的命令表：`komo channel list|probe` 与
/// `komo skills *` 只读文件系统与配置）。
///
/// 从这里再导一次，而不是让 bin 直接依赖 `komo-runtime`：§13.4 给 bin 的依赖是
/// client / gateway / clap 三条，加第四条会把二进制挪到另一条依赖边上。
pub use komo_runtime::config;
pub use komo_runtime::skills;

pub use dispatcher::Dispatcher;
pub use lock::{DiscoveryFile, GatewayDiscovery, InstanceLock};
pub use notifier::HomeNotifier;
pub use service::{Running, ServiceError, ServiceOptions, run, start};
