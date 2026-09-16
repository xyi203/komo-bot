//! 数据目录进程锁与发现文件（§3、§8.7、§12）。
//!
//! 「Gateway 对数据目录持有进程锁。多个 CLI 同时启动时，只允许一个 Gateway 接管实例；
//! **启动失败不能通过删除仍有效的锁来强行重试**。」
//!
//! 锁是 `runtime/gateway.lock`，用 `create_new` 原子创建——POSIX 与 Windows 上它都是
//! 「不存在才建，否则失败」的一次系统调用，不需要 `flock`（§13.4 的依赖清单里没有
//! libc / nix / fs2，而为一把锁引一条 C 依赖不划算）。
//!
// TODO(decide: 文档只说"进程锁"，没说用哪种机制。`create_new` + 持有者存活探测是这里
// 能做到的最保守的一种：拿不到就是有别的实例，只有确认持有者**已经不在**才接管，
// 而"查不出来"按"还在"处理（`Liveness::Unknown`，§8.7 的保守方向）。若以后引入
// `flock`，换掉这个文件即可，接口不动。)
//!
//! 发现文件是 `runtime/gateway.json`（[`komo_client::discovery::DISCOVERY_PATH`] 的那一
//! 个），退出时删除。token 写在里面，文件权限 0600：客户端与 Gateway 共用同一个数据
//! 目录，能读这个目录就已经能读 state.db 与全部会话正文了，把令牌放在别处不会多挡住
//! 任何人。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use komo_runtime::recovery::{ChildProcess, Liveness, ProcessProbe, SysProcessProbe};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 发现文件在数据目录下的相对路径。与 `komo_client::discovery::DISCOVERY_PATH` 是同
/// 一个串——客户端读的就是它。
pub const DISCOVERY_PATH: &str = "runtime/gateway.json";
/// 锁文件在数据目录下的相对路径。
pub const LOCK_PATH: &str = "runtime/gateway.lock";

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// 有别的实例握着锁。**不删仍有效的锁。**
    #[error("数据目录 {path} 已被另一个 Gateway 占用（pid {pid}，实例 {instance}）")]
    Held {
        path: PathBuf,
        pid: u32,
        instance: String,
    },
    #[error("锁文件 {path} 读写失败：{message}")]
    Io { path: PathBuf, message: String },
}

/// 锁文件的内容。只为诊断与「持有者还在吗」这一个判断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockFile {
    pub instance_id: String,
    pub pid: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
}

/// 握在手里的锁。drop 时删除自己那一份（并且只删自己那一份）。
#[derive(Debug)]
pub struct InstanceLock {
    path: PathBuf,
    instance_id: String,
}

impl InstanceLock {
    /// 试着拿到数据目录的进程锁。
    pub fn acquire(home: &Path, instance_id: &str, now: OffsetDateTime) -> Result<Self, LockError> {
        Self::acquire_with(home, instance_id, now, &SysProcessProbe)
    }

    /// 同上，但探测方式可替换（测试拿一个"永远还在"或"已经没了"的探针）。
    pub fn acquire_with(
        home: &Path,
        instance_id: &str,
        now: OffsetDateTime,
        probe: &dyn ProcessProbe,
    ) -> Result<Self, LockError> {
        let path = home.join(LOCK_PATH);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| LockError::Io {
                path: path.clone(),
                message: e.to_string(),
            })?;
        }

        match Self::create(&path, instance_id, now) {
            Ok(()) => {
                return Ok(InstanceLock {
                    path,
                    instance_id: instance_id.to_string(),
                });
            }
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(LockError::Io {
                    path,
                    message: error.to_string(),
                });
            }
            Err(_) => {}
        }

        // 已经有一把锁了。只有确认持有者**不在了**才接管；查不出来按"还在"处理。
        let existing = Self::read(&path);
        let (pid, instance) = match &existing {
            Some(file) => (file.pid, file.instance_id.clone()),
            None => (0, "（锁文件读不出来）".to_string()),
        };
        let held = match &existing {
            None => true,
            Some(file) => matches!(
                probe.probe(&ChildProcess {
                    pid: file.pid,
                    pgid: None,
                    started_at: Some(file.started_at),
                    what: format!("gateway {}", file.instance_id),
                }),
                Liveness::Alive | Liveness::Unknown
            ),
        };
        if held {
            return Err(LockError::Held {
                path,
                pid,
                instance,
            });
        }

        tracing::warn!(
            path = %path.display(),
            pid,
            instance = %instance,
            "锁的持有者已经不在了，接管这个数据目录"
        );
        fs::remove_file(&path).map_err(|e| LockError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
        Self::create(&path, instance_id, now).map_err(|e| LockError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
        Ok(InstanceLock {
            path,
            instance_id: instance_id.to_string(),
        })
    }

    fn create(path: &Path, instance_id: &str, now: OffsetDateTime) -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let body = serde_json::to_vec_pretty(&LockFile {
            instance_id: instance_id.to_string(),
            pid: std::process::id(),
            started_at: now,
        })?;
        file.write_all(&body)?;
        file.sync_all()
    }

    fn read(path: &Path) -> Option<LockFile> {
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// 明确放掉（正常停机走它；drop 是兜底）。
    pub fn release(self) {}
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // 只删自己那一份：万一文件已经被别人换成他们的锁，留着它。
        if let Some(file) = Self::read(&self.path)
            && file.instance_id != self.instance_id
        {
            return;
        }
        let _ = fs::remove_file(&self.path);
    }
}

/// 发现文件：写入与删除。
#[derive(Debug)]
pub struct DiscoveryFile {
    path: PathBuf,
}

impl DiscoveryFile {
    /// 写一份发现文件；权限 0600。
    pub fn write(home: &Path, body: &GatewayDiscovery) -> Result<Self, LockError> {
        let path = home.join(DISCOVERY_PATH);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| LockError::Io {
                path: path.clone(),
                message: e.to_string(),
            })?;
        }
        let text = serde_json::to_string_pretty(body).map_err(|e| LockError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
        // 先写临时文件再改名：客户端读到的永远是一份完整的 JSON。
        let temp = path.with_extension("json.partial");
        fs::write(&temp, text.as_bytes()).map_err(|e| LockError::Io {
            path: temp.clone(),
            message: e.to_string(),
        })?;
        restrict(&temp)?;
        fs::rename(&temp, &path).map_err(|e| LockError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
        Ok(DiscoveryFile { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 退出时删除。
    pub fn remove(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for DiscoveryFile {
    fn drop(&mut self) {
        self.remove();
    }
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<(), LockError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| LockError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<(), LockError> {
    Ok(())
}

/// 发现文件的内容。字段与 `komo_client::discovery::GatewayDiscovery` 逐字对应——
/// 客户端按那个结构读，这里按同一组字段写。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayDiscovery {
    pub instance_id: String,
    pub base_url: String,
    pub protocol_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub started_at: Option<OffsetDateTime>,
}

/// 32 字节随机数的 base64url（无填充）。认证令牌用它。
pub fn random_token() -> String {
    // uuid v4 走的是操作系统的随机数源；两个凑够 32 字节。
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64url(&bytes)
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 63] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Dead;
    impl ProcessProbe for Dead {
        fn probe(&self, _child: &ChildProcess) -> Liveness {
            Liveness::Gone
        }
    }

    #[derive(Debug)]
    struct Alive;
    impl ProcessProbe for Alive {
        fn probe(&self, _child: &ChildProcess) -> Liveness {
            Liveness::Alive
        }
    }

    fn now() -> OffsetDateTime {
        time::macros::datetime!(2026-09-16 08:00:00 UTC)
    }

    #[test]
    fn only_one_of_two_starters_gets_the_lock() {
        let home = tempfile::tempdir().unwrap();
        let first = InstanceLock::acquire(home.path(), "inst-1", now()).expect("第一个拿到");
        let second = InstanceLock::acquire(home.path(), "inst-2", now());
        assert!(
            matches!(second, Err(LockError::Held { .. })),
            "{second:?}：第二个必须失败"
        );
        drop(first);
        // 放掉之后才轮得到下一个。
        InstanceLock::acquire(home.path(), "inst-2", now()).expect("放掉之后能拿到");
    }

    #[test]
    fn a_live_holder_is_never_evicted() {
        let home = tempfile::tempdir().unwrap();
        let _held = InstanceLock::acquire(home.path(), "inst-1", now()).unwrap();
        let error =
            InstanceLock::acquire_with(home.path(), "inst-2", now(), &Alive).expect_err("拿不到");
        assert!(matches!(error, LockError::Held { .. }), "{error:?}");
        assert!(
            home.path().join(LOCK_PATH).exists(),
            "仍然有效的锁不能被删掉"
        );
    }

    #[test]
    fn a_dead_holders_lock_is_taken_over() {
        let home = tempfile::tempdir().unwrap();
        let stale = InstanceLock::acquire(home.path(), "inst-1", now()).unwrap();
        std::mem::forget(stale); // 模拟崩溃：锁文件留着，没有人删。
        let taken =
            InstanceLock::acquire_with(home.path(), "inst-2", now(), &Dead).expect("可以接管");
        assert_eq!(taken.instance_id(), "inst-2");
    }

    #[test]
    fn a_discovery_file_round_trips_and_is_removed_on_drop() {
        let home = tempfile::tempdir().unwrap();
        let body = GatewayDiscovery {
            instance_id: "inst-1".into(),
            base_url: "http://127.0.0.1:7777".into(),
            protocol_version: komo_kernel::protocol::PROTOCOL_VERSION,
            version: Some("0.8.0".into()),
            pid: Some(std::process::id()),
            data_dir: Some(home.path().display().to_string()),
            token: Some("tok".into()),
            started_at: Some(now()),
        };
        let file = DiscoveryFile::write(home.path(), &body).unwrap();
        let path = file.path().to_path_buf();
        let read: GatewayDiscovery =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read, body);
        drop(file);
        assert!(!path.exists(), "退出时删除发现文件");
    }

    #[test]
    fn a_token_is_thirty_two_bytes_of_base64url() {
        let token = random_token();
        assert_eq!(token.len(), 43, "32 字节 base64url（无填充）");
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(token, random_token());
    }
}
