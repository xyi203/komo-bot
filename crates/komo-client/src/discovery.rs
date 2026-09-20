//! 读取发现文件并核对实例身份与健康状态（§3 聊天启动顺序 1–4）。
//!
//! 「检查 Gateway 健康状态与实例身份，**不能只凭 PID 或端口判断**。」——所以这里只把
//! 发现文件当成**地址簿**：它说 Gateway 在哪，`GET /healthz` 说那里的是不是它。文件里
//! 的 `pid` 一个判断都不参与（进程号会被复用，端口会被别人占），只在诊断信息里印出来
//! 给人看。

use std::path::{Path, PathBuf};

use komo_kernel::protocol::PROTOCOL_VERSION;
use komo_kernel::protocol::http::HealthResponse;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::api::KomoClient;
use crate::error::ClientError;

/// 发现文件相对数据目录的位置。§12 只说了 `runtime/` 放「发现文件、锁等」，没有给
/// 文件名与字段。
///
// TODO(decide: §12 未定发现文件的文件名与字段。这里取最保守的一组——地址 + 实例身份 +
// 协议版本 + 可选 token，全部能被 /healthz 独立证实（token 除外）。Gateway 侧落地时
// 若改名或改字段，改这一个常量与 `GatewayDiscovery`。)
pub const DISCOVERY_PATH: &str = "runtime/gateway.json";

/// 发现文件的内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayDiscovery {
    /// 实例身份。与 `/healthz` 的 `instance_id` 不符 = 文件是上一个实例留下的。
    pub instance_id: String,
    /// HTTP 根地址，例如 `http://127.0.0.1:7777`。
    pub base_url: String,
    /// 协议版本。对不上就不是同一个 komo（§13.1）。
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// **只用于诊断信息。**任何判断都不看它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// 认证令牌（§13.1「除最小健康检查外统一认证」）。
    // TODO(decide: §13.1 只说"统一认证"，没说凭证怎么到客户端手上。这里假定 Gateway
    // 把它写进发现文件——与数据目录同权限；若改成 .env 或别的来源，只改这一个字段的
    // 读取处。)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub started_at: Option<OffsetDateTime>,
}

/// 核对通过的一个实例：地址簿那一行，加上它自己报的健康状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub discovery: GatewayDiscovery,
    pub health: HealthResponse,
    /// 这次发现读的是哪个数据目录。带进客户端里，好让它在网关重启之后**自己按发现文件
    /// 跟过去**（令牌每次启动都换，见 [`KomoClient::refresh`]）。
    pub home: Option<PathBuf>,
}

impl Discovered {
    pub fn base_url(&self) -> &str {
        &self.discovery.base_url
    }

    /// 按这次发现的结果建一个客户端（带上令牌，并记住数据目录以便刷新）。
    pub fn client(&self) -> Result<KomoClient, ClientError> {
        match &self.home {
            Some(home) => KomoClient::with_home(
                reqwest::Client::new(),
                &self.discovery.base_url,
                self.discovery.token.clone(),
                home.clone(),
                Some(self.discovery.instance_id.clone()),
            ),
            None => KomoClient::new(&self.discovery.base_url, self.discovery.token.clone()),
        }
    }
}

/// 发现失败的每一种，各自带足以行动的诊断（§3 第 4 步「超时则返回具体诊断信息」）。
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// 没有发现文件——本机实例没在跑，请求服务管理器启动（§3 第 3 步）。
    #[error("没有发现文件 {path}：本机 Gateway 未运行")]
    Missing { path: PathBuf },
    #[error("发现文件 {path} 读不了：{source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("发现文件 {path} 不是合法的 JSON：{message}")]
    Malformed { path: PathBuf, message: String },
    /// 文件在，`/healthz` 不答——进程没了而文件留下，或它还没准备好。
    #[error("{base_url} 上的 Gateway 没有响应 /healthz：{message}")]
    Unreachable { base_url: String, message: String },
    /// 答了，但不是发现文件说的那一个。**这正是"不能只凭 PID 或端口判断"要挡的事。**
    #[error("{base_url} 上运行的是另一个实例（发现文件说 {expected}，它自称 {found}）")]
    InstanceMismatch {
        base_url: String,
        expected: String,
        found: String,
    },
    /// 协议版本对不上就不是同一个 komo（§13.1）。
    #[error("协议版本不符：本客户端 {ours}，{base_url} 上的 Gateway {theirs}")]
    ProtocolMismatch {
        base_url: String,
        ours: u32,
        theirs: u32,
    },
    /// 它活着，但守的是另一个数据目录。
    #[error("{base_url} 上的 Gateway 守着 {found}，不是 {expected}")]
    DataDirMismatch {
        base_url: String,
        expected: String,
        found: String,
    },
    /// 等就绪超时。
    #[error("等待 Gateway 就绪超时（{waited_ms}ms）：{last}")]
    Timeout { waited_ms: u64, last: String },
}

impl DiscoveryError {
    /// 这一种失败是不是「它还没起来」——调用方据此决定是请服务管理器启动还是报错。
    pub fn is_not_running(&self) -> bool {
        matches!(
            self,
            DiscoveryError::Missing { .. } | DiscoveryError::Unreachable { .. }
        )
    }
}

/// 数据目录。**`KOMO_HOME` 是这个 crate 直接读的唯一配置环境变量**（§12）。
///
/// `HOME` 只作为它缺省时的基准出现——不读它就算不出 `~/.komo` 在哪，而这里不允许引入
/// `dirs`（§13.4 的依赖清单里没有它）。
pub fn komo_home() -> PathBuf {
    if let Ok(home) = std::env::var("KOMO_HOME")
        && !home.trim().is_empty()
    {
        return PathBuf::from(home);
    }
    match std::env::var("HOME") {
        Ok(home) if !home.trim().is_empty() => PathBuf::from(home).join(".komo"),
        _ => PathBuf::from(".komo"),
    }
}

/// 只读发现文件，不联网。
pub fn read_discovery_file(home: &Path) -> Result<GatewayDiscovery, DiscoveryError> {
    let path = home.join(DISCOVERY_PATH);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(DiscoveryError::Missing { path });
        }
        Err(source) => return Err(DiscoveryError::Unreadable { path, source }),
    };
    serde_json::from_str(&text).map_err(|e| DiscoveryError::Malformed {
        path,
        message: e.to_string(),
    })
}

/// §3 第 1–2 步：读发现文件，核对健康状态与实例身份。
pub async fn discover(home: &Path) -> Result<Discovered, DiscoveryError> {
    let discovery = read_discovery_file(home)?;
    verify(discovery, Some(home)).await
}

/// 拿一份已经读到的发现文件去核对。`home` 给出时还比对数据目录。
pub async fn verify(
    discovery: GatewayDiscovery,
    home: Option<&Path>,
) -> Result<Discovered, DiscoveryError> {
    let client = KomoClient::new(&discovery.base_url, discovery.token.clone()).map_err(|e| {
        DiscoveryError::Unreachable {
            base_url: discovery.base_url.clone(),
            message: e.to_string(),
        }
    })?;
    let health = client
        .health()
        .await
        .map_err(|e| DiscoveryError::Unreachable {
            base_url: discovery.base_url.clone(),
            message: e.to_string(),
        })?;
    check(discovery, health, home)
}

/// 三条核对，纯函数（测试直接喂两份结构体）。
pub fn check(
    discovery: GatewayDiscovery,
    health: HealthResponse,
    home: Option<&Path>,
) -> Result<Discovered, DiscoveryError> {
    if !discovery.instance_id.is_empty() && discovery.instance_id != health.instance_id {
        return Err(DiscoveryError::InstanceMismatch {
            base_url: discovery.base_url,
            expected: discovery.instance_id,
            found: health.instance_id,
        });
    }
    if health.protocol_version != PROTOCOL_VERSION {
        return Err(DiscoveryError::ProtocolMismatch {
            base_url: discovery.base_url,
            ours: PROTOCOL_VERSION,
            theirs: health.protocol_version,
        });
    }
    if let Some(home) = home {
        let expected = home.to_string_lossy().to_string();
        // 只在两边都给了路径时比。路径写法不同（尾斜杠）不算不符。
        let normalize = |p: &str| p.trim_end_matches('/').to_string();
        if !health.data_dir.is_empty() && normalize(&health.data_dir) != normalize(&expected) {
            return Err(DiscoveryError::DataDirMismatch {
                base_url: discovery.base_url,
                expected,
                found: health.data_dir,
            });
        }
    }
    Ok(Discovered {
        discovery,
        health,
        home: home.map(Path::to_path_buf),
    })
}

/// §3 第 4 步：等服务就绪，超时给出**具体**诊断——最后一次失败的原文，不是"超时"三个字。
pub async fn wait_ready(
    home: &Path,
    timeout: std::time::Duration,
    poll: std::time::Duration,
) -> Result<Discovered, DiscoveryError> {
    let deadline = std::time::Instant::now() + timeout;
    // 每一圈要么返回，要么给 `last` 赋值，所以它到超时检查时一定已经初始化。
    let mut last: String;
    loop {
        match discover(home).await {
            Ok(found) => return Ok(found),
            Err(error) => {
                // 身份 / 协议 / 数据目录不符不是"还没起来"，再等也不会变。
                if !error.is_not_running() {
                    return Err(error);
                }
                last = error.to_string();
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(DiscoveryError::Timeout {
                waited_ms: timeout.as_millis() as u64,
                last,
            });
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn discovery() -> GatewayDiscovery {
        GatewayDiscovery {
            instance_id: "inst-1".into(),
            base_url: "http://127.0.0.1:7777".into(),
            protocol_version: PROTOCOL_VERSION,
            version: Some("0.8.0".into()),
            pid: Some(4242),
            data_dir: Some("/home/u/.komo".into()),
            token: Some("t".into()),
            started_at: Some(datetime!(2026-09-16 08:00:00 UTC)),
        }
    }

    fn health(instance: &str, protocol: u32, data_dir: &str) -> HealthResponse {
        HealthResponse {
            instance_id: instance.into(),
            version: "0.8.0".into(),
            protocol_version: protocol,
            started_at: datetime!(2026-09-16 08:00:00 UTC),
            data_dir: data_dir.into(),
        }
    }

    #[test]
    fn a_matching_instance_passes() {
        let found = check(
            discovery(),
            health("inst-1", PROTOCOL_VERSION, "/home/u/.komo"),
            Some(Path::new("/home/u/.komo")),
        )
        .unwrap();
        assert_eq!(found.base_url(), "http://127.0.0.1:7777");
    }

    #[test]
    fn a_different_instance_on_the_same_port_is_refused() {
        let error = check(
            discovery(),
            health("inst-2", PROTOCOL_VERSION, "/home/u/.komo"),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error, DiscoveryError::InstanceMismatch { .. }),
            "{error}"
        );
        // 端口对得上、进程活着，仍然不算发现——这正是 §3 第 2 步那句话。
        assert!(!error.is_not_running());
    }

    #[test]
    fn a_different_protocol_version_is_not_the_same_komo() {
        let error = check(
            discovery(),
            health("inst-1", PROTOCOL_VERSION + 1, "/home/u/.komo"),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error, DiscoveryError::ProtocolMismatch { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_gateway_guarding_another_data_dir_is_refused() {
        let error = check(
            discovery(),
            health("inst-1", PROTOCOL_VERSION, "/srv/other"),
            Some(Path::new("/home/u/.komo")),
        )
        .unwrap_err();
        assert!(
            matches!(error, DiscoveryError::DataDirMismatch { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_trailing_slash_is_not_a_different_directory() {
        assert!(
            check(
                discovery(),
                health("inst-1", PROTOCOL_VERSION, "/home/u/.komo/"),
                Some(Path::new("/home/u/.komo")),
            )
            .is_ok()
        );
    }

    #[test]
    fn a_missing_file_reads_as_not_running() {
        let dir = std::env::temp_dir().join("komo-client-discovery-missing");
        let error = read_discovery_file(&dir).unwrap_err();
        assert!(matches!(error, DiscoveryError::Missing { .. }), "{error}");
        assert!(error.is_not_running());
    }

    #[test]
    fn a_discovery_file_round_trips() {
        let text = serde_json::to_string(&discovery()).unwrap();
        assert_eq!(
            serde_json::from_str::<GatewayDiscovery>(&text).unwrap(),
            discovery()
        );
    }

    #[test]
    fn a_file_written_without_the_optional_fields_still_reads() {
        let parsed: GatewayDiscovery = serde_json::from_str(
            r#"{"instance_id":"i","base_url":"http://127.0.0.1:1","protocol_version":1}"#,
        )
        .unwrap();
        assert!(parsed.token.is_none());
        assert!(parsed.pid.is_none());
    }
}
