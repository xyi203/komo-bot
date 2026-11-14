//! `komo login` 存下的远程 Gateway 连接（`docs/redesign.md` §2.9）。
//!
//! 命令行默认连本机 Gateway（读 `runtime/gateway.json` 里的本机令牌）。要从另一台机器操作远程
//! 那台，就 `komo login <地址>`：跳浏览器走 OIDC 换一个设备令牌，存在数据目录的 `remote.json`
//! （权限 0600）。这个文件在，操作命令就连它；`komo logout` 吊销令牌并删掉它。
//!
//! 它**不是**发现文件：不核对实例、不刷新——设备令牌长期有效，不随 Gateway 重启而变。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 相对数据目录的位置。
pub const REMOTE_PATH: &str = "remote.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteLogin {
    /// 远程 Gateway 的根地址，如 `https://komo.example.com`。
    pub base_url: String,
    /// 设备令牌（Bearer）。
    pub token: String,
    /// 设备令牌在服务端的 id，`komo logout` 吊销用。
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

pub fn remote_path(home: &Path) -> PathBuf {
    home.join(REMOTE_PATH)
}

/// 读；没有这个文件是 `Ok(None)`，读不了或不是合法 JSON 是 `Err`（别悄悄退回本机，那会让人以为
/// 在操作远程，其实动的是本机）。
pub fn read_remote(home: &Path) -> Result<Option<RemoteLogin>, String> {
    let path = remote_path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读 {} 失败：{error}", path.display())),
    };
    serde_json::from_str(&text).map(Some).map_err(|error| {
        format!(
            "{} 不是合法的登录记录：{error}（`komo logout` 可以清掉它）",
            path.display()
        )
    })
}

/// 写（先写临时文件再 rename，权限 0600）。
pub fn write_remote(home: &Path, remote: &RemoteLogin) -> Result<(), String> {
    let path = remote_path(home);
    let text = serde_json::to_string_pretty(remote).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(home)
        .map_err(|error| format!("建 {} 失败：{error}", home.display()))?;
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, &text).map_err(|error| format!("写 {} 失败：{error}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|error| format!("写 {} 失败：{error}", path.display()))
}

#[cfg(unix)]
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)
}

/// 删；有这个文件才答 `true`。
pub fn remove_remote(home: &Path) -> Result<bool, String> {
    let path = remote_path(home);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("删 {} 失败：{error}", path.display())),
    }
}
