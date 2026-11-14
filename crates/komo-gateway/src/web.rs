//! 网页壳的网关侧（`docs/clients.md` D5）：Host 校验与静态文件。登录与会话在 [`crate::auth`]。
//!
//! - **Host 校验**：只接受回环、监听地址、`[gateway] trusted_hosts` 与 `[auth.oidc] public_url` 的
//!   主机，挡 DNS 重绑定。它只负责放行，不替代鉴权（`auth` 层仍然要登录 cookie 或令牌）。
//!
//! 静态文件不嵌进二进制：从 `[web] dir`（或二进制旁的 `web/`）按请求读，目录不存在就是没有网页。

use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use komo_kernel::protocol::http::ErrorCode;

use crate::http::Api;
use crate::http::error::ApiFailure;

/// 哪些 `Host` 放行。
#[derive(Debug, Clone)]
pub struct HostPolicy {
    /// 小写、不带端口与方括号的主机名。
    allowed: Vec<String>,
}

impl HostPolicy {
    /// 回环地址 + 监听地址 + 配置里列的主机名。
    pub fn new(listen: SocketAddr, trusted: &[String]) -> Self {
        let mut allowed: Vec<String> = ["127.0.0.1", "localhost", "::1"]
            .iter()
            .map(|host| host.to_string())
            .collect();
        allowed.push(listen.ip().to_string());
        allowed.extend(trusted.iter().map(|host| hostname(host)));
        allowed.sort();
        allowed.dedup();
        HostPolicy { allowed }
    }

    /// `Host` 头的值是否放行。端口不参与比较：同一台机器上端口不是身份。
    pub fn allows(&self, host: &str) -> bool {
        let host = hostname(host);
        !host.is_empty() && self.allowed.contains(&host)
    }
}

/// `Host` / 配置值 → 小写主机名（去端口、去 IPv6 方括号、去结尾的点）。
fn hostname(raw: &str) -> String {
    let raw = raw.trim();
    let name = if let Some(rest) = raw.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        match raw.rsplit_once(':') {
            // 恰好一个冒号才是 `主机:端口`；裸 IPv6（多个冒号）原样留着。
            Some((host, port))
                if !host.contains(':') && port.chars().all(|c| c.is_ascii_digit()) =>
            {
                host
            }
            _ => raw,
        }
    };
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Host 策略与静态目录。
#[derive(Debug)]
pub struct WebAuth {
    hosts: HostPolicy,
    web_dir: Option<PathBuf>,
}

impl WebAuth {
    pub fn new(listen: SocketAddr, trusted_hosts: &[String], web_dir: Option<PathBuf>) -> Self {
        WebAuth {
            hosts: HostPolicy::new(listen, trusted_hosts),
            web_dir,
        }
    }

    pub fn web_dir(&self) -> Option<&Path> {
        self.web_dir.as_deref()
    }
}

// ---------------------------------------------------------------- 请求头

/// `Cookie` 头里名叫 `name` 的那个值。
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.trim().to_string())
        })
}

/// `Origin` 是不是自己人：与 `Host` 同源，或等于公开地址（`[auth.oidc] public_url`）的源。
/// 没带 `Origin` 不算：会改东西的请求，浏览器一定会带。
///
/// 经隧道 / 反向代理时 `Host` 可能被改写成内网地址，所以公开地址也认——它是操作者自己配的。
pub fn same_origin(headers: &HeaderMap, public_origin: Option<&str>) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    if public_origin.is_some_and(|public| origin.eq_ignore_ascii_case(public)) {
        return true;
    }
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    authority.is_some_and(|authority| authority.eq_ignore_ascii_case(host.trim()))
}

// ---------------------------------------------------------------- 中间件与接口

/// 最外层：`Host` 不在名单里一律 403（包括静态文件、`/healthz`、`/auth/*`）。
pub async fn host_guard(
    State(web): State<Arc<WebAuth>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiFailure> {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| request.uri().authority().map(|a| a.to_string()));
    if host.as_deref().is_some_and(|host| web.hosts.allows(host)) {
        return Ok(next.run(request).await);
    }
    Err(ApiFailure::new(
        ErrorCode::Denied,
        "Host 不在允许的名单里（回环地址、监听地址，或 config.toml 的 [gateway] trusted_hosts）",
    ))
}

// ---------------------------------------------------------------- 静态文件

/// 路由表没有命中的请求：`/v1/*` 回 JSON 404；其余先找静态文件，没有扩展名的路径回落到壳 html
/// （客户端路由），带扩展名而找不到的（缺的脚本、图片）老实 404。
pub async fn fallback(State(api): State<Api>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    if path == "/v1" || path.starts_with("/v1/") {
        return ApiFailure::not_found(&path).into_response();
    }
    let Some(dir) = api.web.web_dir() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match resolve(dir, &path).await {
        Some(file) => serve_file(&file, &path, request.method() == Method::HEAD).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// URL 路径 → 目录里的文件。`..`、绝对路径、以 `.` 开头的段一律不认。
async fn resolve(dir: &Path, url_path: &str) -> Option<PathBuf> {
    let relative = Path::new(url_path.trim_start_matches('/'));
    let mut file = dir.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str()?;
                if part.starts_with('.') || part.contains('\\') {
                    return None;
                }
                file.push(part);
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    if tokio::fs::metadata(&file)
        .await
        .is_ok_and(|meta| meta.is_file())
    {
        return Some(file);
    }
    let has_extension = relative.extension().is_some();
    if has_extension {
        return None;
    }
    // 壳 html：TanStack Start 的 SPA 模式叫 `_shell.html`，普通静态站是 `index.html`。
    for name in ["_shell.html", "index.html"] {
        let shell = dir.join(name);
        if tokio::fs::metadata(&shell)
            .await
            .is_ok_and(|meta| meta.is_file())
        {
            return Some(shell);
        }
    }
    None
}

async fn serve_file(file: &Path, url_path: &str, head: bool) -> Response {
    let Ok(body) = tokio::fs::read(file).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let is_html = file.extension().is_some_and(|ext| ext == "html");
    // 带哈希的构建产物永不变；壳与其余文件每次问一下。
    let cache = if url_path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(file))
        .header(header::CACHE_CONTROL, cache)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if is_html {
        builder = builder
            .header(header::REFERRER_POLICY, "no-referrer")
            .header(header::X_FRAME_OPTIONS, "DENY");
    }
    let length = body.len();
    let body = if head {
        axum::body::Body::empty()
    } else {
        axum::body::Body::from(body)
    };
    builder
        .header(header::CONTENT_LENGTH, length)
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn content_type(file: &Path) -> &'static str {
    match file.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// 没有配 `[web] dir` 时的默认：二进制旁的 `web/`，存在才算。
pub fn default_web_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.join("web");
    dir.is_dir().then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn policy(listen: &str, trusted: &[&str]) -> HostPolicy {
        let trusted: Vec<String> = trusted.iter().map(|host| host.to_string()).collect();
        HostPolicy::new(listen.parse().expect("地址"), &trusted)
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                header::HeaderName::from_bytes(name.as_bytes()).expect("头名"),
                HeaderValue::from_str(value).expect("头值"),
            );
        }
        map
    }

    #[test]
    fn loopback_hosts_are_allowed_with_any_port() {
        let policy = policy("127.0.0.1:7777", &[]);
        for host in [
            "127.0.0.1:7777",
            "localhost",
            "LOCALHOST:5173",
            "[::1]:7777",
            "::1",
        ] {
            assert!(policy.allows(host), "{host}");
        }
    }

    #[test]
    fn a_foreign_host_is_refused_unless_trusted() {
        let closed = policy("127.0.0.1:7777", &[]);
        assert!(!closed.allows("evil.example.com"));
        assert!(!closed.allows("evil.example.com:7777"));
        assert!(!closed.allows(""));
        let open = policy("127.0.0.1:7777", &["Komo.Example.com"]);
        assert!(open.allows("komo.example.com"));
        assert!(open.allows("komo.example.com:443"));
        assert!(!open.allows("evil.example.com"));
    }

    #[test]
    fn the_listen_address_itself_is_allowed() {
        let policy = policy("192.168.1.5:7777", &[]);
        assert!(policy.allows("192.168.1.5:7777"));
        assert!(!policy.allows("192.168.1.6:7777"));
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let map = headers(&[("cookie", "a=1; komo_session=tok; b=2")]);
        assert_eq!(cookie_value(&map, "komo_session").as_deref(), Some("tok"));
        assert_eq!(
            cookie_value(&headers(&[("cookie", "a=1")]), "komo_session"),
            None
        );
    }

    #[test]
    fn origin_must_match_host() {
        let ok = headers(&[
            ("host", "127.0.0.1:7777"),
            ("origin", "http://127.0.0.1:7777"),
        ]);
        assert!(same_origin(&ok, None));
        let other = headers(&[
            ("host", "127.0.0.1:7777"),
            ("origin", "http://evil.example.com"),
        ]);
        assert!(!same_origin(&other, None));
        // 公开地址的源也算自己人（经隧道时 Host 可能被改写）。
        let behind_tunnel = headers(&[
            ("host", "127.0.0.1:7777"),
            ("origin", "https://komo.example.com"),
        ]);
        assert!(same_origin(
            &behind_tunnel,
            Some("https://komo.example.com")
        ));
        assert!(!same_origin(&behind_tunnel, None));
        let missing = headers(&[("host", "127.0.0.1:7777")]);
        assert!(!same_origin(&missing, None));
    }

    #[tokio::test]
    async fn static_paths_cannot_leave_the_directory() {
        let dir = tempfile::tempdir().expect("目录");
        std::fs::write(dir.path().join("_shell.html"), "<html>").unwrap();
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/app.js"), "x").unwrap();
        assert!(resolve(dir.path(), "/assets/app.js").await.is_some());
        assert!(resolve(dir.path(), "/../secret").await.is_none());
        assert!(
            resolve(dir.path(), "/assets/../../secret.txt")
                .await
                .is_none()
        );
        assert!(resolve(dir.path(), "/.env").await.is_none());
        // 客户端路由回落到壳；缺的资源不回落。
        assert!(
            resolve(dir.path(), "/sessions/abc")
                .await
                .unwrap()
                .ends_with("_shell.html")
        );
        assert!(resolve(dir.path(), "/assets/missing.js").await.is_none());
    }
}
