//! 聊天启动顺序（§3）：读发现文件 → 核对健康与实例身份 → 没起来就请服务管理器起
//! → 等就绪，超时给**具体**诊断。

use std::path::{Path, PathBuf};
use std::time::Duration;

use komo_client::KomoClient;
use komo_client::discovery::{self, DiscoveryError};

/// 等 Gateway 就绪最多等多久。
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const READY_POLL: Duration = Duration::from_millis(200);

/// 数据目录（`KOMO_HOME`，再没有就是 `~/.komo`）。
pub fn komo_home() -> PathBuf {
    discovery::komo_home()
}

/// 用户的家目录——launchd / systemd 的单元文件写在它下面。
pub fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 连上本机 Gateway；**没起来就请服务管理器起它**（§3 第 3 步）。
pub async fn connect_or_start(home: &Path) -> Result<KomoClient, String> {
    match discovery::discover(home).await {
        Ok(found) => return found.client().map_err(|error| error.to_string()),
        // 身份 / 协议 / 数据目录不符不是"还没起来"，再等也不会变。
        Err(error) if !error.is_not_running() => return Err(error.to_string()),
        Err(_) => {}
    }

    eprintln!("本机 Gateway 没在跑，正在启动…");
    request_start(home)?;
    wait_ready(home).await
}

/// 只连，不启动（`status` / `stop` **不隐式启动服务**，§3）。
pub async fn connect(home: &Path) -> Result<KomoClient, String> {
    discovery::discover(home)
        .await
        .map_err(|error| error.to_string())
        .and_then(|found| found.client().map_err(|error| error.to_string()))
}

/// 请服务管理器起它；没有服务管理器就说清楚该怎么办。
pub fn request_start(home: &Path) -> Result<(), String> {
    use komo_gateway::service::units;
    match units::start(&user_home(), home) {
        Ok(()) => Ok(()),
        Err(units::UnitError::NoManager) => Err(
            "这台机器上没有可用的服务管理器（launchd / systemd --user）。\
             请另开一个终端前台运行：`komo gateway --foreground`"
                .to_string(),
        ),
        Err(error) => Err(error.to_string()),
    }
}

/// 等就绪，超时给**具体**诊断（§3 第 4 步）。
pub async fn wait_ready(home: &Path) -> Result<KomoClient, String> {
    match discovery::wait_ready(home, READY_TIMEOUT, READY_POLL).await {
        Ok(found) => found.client().map_err(|error| error.to_string()),
        Err(DiscoveryError::Timeout { waited_ms, last }) => Err(format!(
            "等了 {waited_ms}ms 还没就绪：{last}\n看一眼日志：{}",
            home.join("logs/gateway.log").display()
        )),
        Err(error) => Err(error.to_string()),
    }
}
