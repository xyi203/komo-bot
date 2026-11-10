//! MCP 的 OAuth（`docs/mcp.md` §2.1）：`[mcp.servers.<name>]` 写了 `oauth = true` 的 HTTP
//! 服务器，token 不在 `.env`，在 `<数据目录>/mcp/<name>/credentials.json`（0600）。
//!
//! - **登录不经 Gateway**（§3）：`komo auth mcp login <name>` 在本机跑授权码 + PKCE，
//!   回调只听 `127.0.0.1` 的一个随机端口，同 `komo auth codex login` 的先例直接落盘。
//! - 客户端靠动态注册（RFC 7591）拿 `client_id`，与 token 存在同一份文件里。
//! - **取用与刷新交给 rmcp**：Gateway 连接时用 [`authorized_manager`] 装好的管理器，每次请求
//!   从文件读 token，快过期时用 refresh token 换新并写回。服务器不发 refresh token 时，
//!   过期了就要重新登录——新 token 写进同一份文件，在跑的 Gateway 下一次请求就读到。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    StoredCredentials,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 等浏览器回调的时限。
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// 回调请求头最多读这么多：只要请求行。
const MAX_CALLBACK_REQUEST: usize = 16 * 1024;

/// 凭证文件在数据目录里的位置：`<数据目录>/mcp/<server>/credentials.json`。
pub fn credentials_path(data_dir: &Path, server: &str) -> PathBuf {
    data_dir.join("mcp").join(server).join("credentials.json")
}

#[derive(Debug, thiserror::Error)]
pub enum McpAuthError {
    #[error("{0}")]
    Auth(#[from] AuthError),
    #[error("本机回调：{0}")]
    Callback(String),
    #[error("{}s 内没等到浏览器回调", LOGIN_TIMEOUT.as_secs())]
    Timeout,
    #[error("{path}：{message}")]
    Io { path: PathBuf, message: String },
}

/// 一份凭证文件，实现 rmcp 的 [`CredentialStore`]：读写都整份进出，写是临时文件 + rename
/// （刷新写回时别人不会读到半份）。
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

fn store_error(path: &Path, message: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError(format!("{}：{message}", path.display()))
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| store_error(&self.path, format!("解析失败：{error}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(store_error(&self.path, error)),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || save_file(&path, &credentials))
            .await
            .map_err(|error| store_error(&self.path, error))?
    }

    async fn clear(&self) -> Result<(), AuthError> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(store_error(&self.path, error)),
        }
    }
}

fn save_file(path: &Path, credentials: &StoredCredentials) -> Result<(), AuthError> {
    let dir = path
        .parent()
        .ok_or_else(|| store_error(path, "没有父目录"))?;
    std::fs::create_dir_all(dir).map_err(|error| store_error(path, error))?;
    let body = serde_json::to_vec_pretty(credentials)
        .map_err(|error| store_error(path, format!("序列化失败：{error}")))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|error| store_error(path, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| store_error(path, error))?;
    }
    tmp.write_all(&body)
        .map_err(|error| store_error(path, error))?;
    tmp.persist(path)
        .map_err(|error| store_error(path, error.error))?;
    Ok(())
}

/// Gateway 连接时用：从凭证文件装好一个管理器。`Ok(None)` = 还没登录过（或授权服务器换了、
/// 旧凭证被清掉），该跑 `komo auth mcp login`。
pub async fn authorized_manager(
    url: &str,
    path: &Path,
) -> Result<Option<AuthorizationManager>, AuthError> {
    let mut manager = AuthorizationManager::new(url).await?;
    manager.set_credential_store(FileCredentialStore::new(path));
    Ok(manager.initialize_from_store().await?.then_some(manager))
}

/// `komo auth mcp login <name>`：发现授权服务器 → 动态注册 → 打开浏览器 → 等本机回调 →
/// 换 token → 落盘。`out` 只打提示，token 只经凭证文件。
pub async fn login(
    url: &str,
    path: &Path,
    out: &mut dyn std::io::Write,
) -> Result<(), McpAuthError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| McpAuthError::Callback(error.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|error| McpAuthError::Callback(error.to_string()))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let mut manager = AuthorizationManager::new(url).await?;
    manager.set_credential_store(FileCredentialStore::new(path));
    let resolution = manager.resolve_metadata().await?;
    manager.set_metadata(resolution.metadata);
    let session = AuthorizationSession::new(
        manager,
        AuthorizationRequest::new(&redirect_uri).with_client_name("komo"),
    )
    .await
    .map_err(|(_manager, error)| error)?;

    let auth_url = session.get_authorization_url();
    writeln!(
        out,
        "在浏览器里完成授权（没自动打开就复制这个地址）：\n{auth_url}"
    )
    .ok();
    open_browser(auth_url);

    let callback = tokio::time::timeout(LOGIN_TIMEOUT, accept_callback(&listener, port))
        .await
        .map_err(|_elapsed| McpAuthError::Timeout)??;
    session.handle_callback_url(&callback).await?;
    Ok(())
}

/// 尽力打开浏览器；打不开就只靠上面那行地址。
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// 等一次打到 `/callback` 的请求，交回完整的回调地址。别的路径（`/favicon.ico` 之类）回
/// 404 接着等；授权服务器带回 `error=` 时报出来。
async fn accept_callback(listener: &TcpListener, port: u16) -> Result<String, McpAuthError> {
    loop {
        let (mut stream, _peer) = listener
            .accept()
            .await
            .map_err(|error| McpAuthError::Callback(error.to_string()))?;
        let target = match read_request_target(&mut stream).await {
            Some(target) => target,
            None => continue,
        };
        if !target.starts_with("/callback") {
            respond(&mut stream, "404 Not Found", "").await;
            continue;
        }
        let callback = format!("http://127.0.0.1:{port}{target}");
        if let Some(error) = query_value(&callback, "error") {
            respond(
                &mut stream,
                "400 Bad Request",
                "授权没有完成，回终端看原因。",
            )
            .await;
            let detail = query_value(&callback, "error_description").unwrap_or_default();
            return Err(McpAuthError::Callback(format!(
                "授权服务器拒绝：{error} {detail}"
            )));
        }
        respond(&mut stream, "200 OK", "komo：授权完成，可以关掉这个页面。").await;
        return Ok(callback);
    }
}

async fn read_request_target(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buffer.windows(2).any(|w| w == b"\r\n") && buffer.len() < MAX_CALLBACK_REQUEST {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    request_target(&buffer)
}

/// `GET /callback?code=… HTTP/1.1` 里的第二段。
fn request_target(request: &[u8]) -> Option<String> {
    let line = request.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split_whitespace();
    (parts.next()? == "GET").then_some(())?;
    parts.next().map(str::to_string)
}

fn query_value(url: &str, key: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// `komo auth mcp status` 的一行：登录了没有、token 什么时候到期、有没有 refresh token。
/// 不打印 token。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAuthStatus {
    /// token 到期的 Unix 秒；服务器没给 `expires_in` 时为 `None`。
    pub expires_at: Option<u64>,
    pub refreshable: bool,
}

/// 读凭证文件。`Ok(None)` = 没登录过。
pub fn status(path: &Path) -> Result<Option<McpAuthStatus>, McpAuthError> {
    let io = |message: String| McpAuthError::Io {
        path: path.to_path_buf(),
        message,
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io(error.to_string())),
    };
    let stored: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| io(format!("解析失败：{error}")))?;
    let token = &stored["token_response"];
    if token.is_null() {
        return Ok(None);
    }
    let expires_at = match (
        token["expires_in"].as_u64(),
        stored["token_received_at"].as_u64(),
    ) {
        (Some(expires_in), Some(received_at)) => Some(received_at + expires_in),
        _ => None,
    };
    Ok(Some(McpAuthStatus {
        expires_at,
        refreshable: token["refresh_token"].is_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_target_is_the_path_of_a_get_line() {
        assert_eq!(
            request_target(b"GET /callback?code=a&state=b HTTP/1.1\r\nHost: x\r\n\r\n").as_deref(),
            Some("/callback?code=a&state=b")
        );
        assert_eq!(request_target(b"POST /callback HTTP/1.1\r\n"), None);
    }

    #[tokio::test]
    async fn the_callback_skips_other_paths_and_returns_the_full_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = tokio::spawn(async move {
            for target in ["/favicon.ico", "/callback?code=c1&state=s1"] {
                let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .unwrap();
                stream
                    .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).await.unwrap();
                assert!(response.starts_with("HTTP/1.1"), "{response}");
            }
        });
        let callback = accept_callback(&listener, port).await.unwrap();
        client.await.unwrap();
        assert_eq!(
            callback,
            format!("http://127.0.0.1:{port}/callback?code=c1&state=s1")
        );
    }

    #[tokio::test]
    async fn an_error_redirect_is_reported_instead_of_exchanged() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream
                .write_all(b"GET /callback?error=access_denied HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response).await;
        });
        let error = accept_callback(&listener, port).await.unwrap_err();
        assert!(error.to_string().contains("access_denied"), "{error}");
    }

    #[tokio::test]
    async fn the_file_store_round_trips_and_reports_status_without_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = credentials_path(dir.path(), "devops");
        let store = FileCredentialStore::new(&path);
        assert!(store.load().await.unwrap().is_none());
        assert_eq!(status(&path).unwrap(), None);

        let token: rmcp::transport::auth::OAuthTokenResponse = serde_json::from_value(
            serde_json::json!({"access_token": "secret", "token_type": "Bearer", "expires_in": 3600}),
        )
        .unwrap();
        store
            .save(StoredCredentials::new(
                "client-1".into(),
                Some(token),
                vec![],
                Some(1_000),
            ))
            .await
            .unwrap();

        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "client-1");
        assert_eq!(
            status(&path).unwrap(),
            Some(McpAuthStatus {
                expires_at: Some(4_600),
                refreshable: false,
            })
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
    }
}
