//! `komo login` / `komo logout`（`docs/redesign.md` §2.9）：命令行连**远程** Gateway 用的设备登录。
//!
//! 标准的「跳系统浏览器登录再跳回」：本机起一个只听 `127.0.0.1` 的临时端口 → 带 PKCE challenge 与
//! 回环回跳地址打开 Gateway 的 `/auth/device/authorize` → 浏览器里走完 OIDC → Gateway 带一次性码
//! 跳回本机端口 → 用码 + verifier 换设备令牌（`POST /auth/device/token`）→ 存进数据目录的
//! `remote.json`（0600，`komo_client::remote`）。之后操作命令就连那台远程 Gateway。
//!
//! 机器上的命令行连本机 Gateway 不需要它（本机令牌）。

use std::path::Path;
use std::time::Duration;

use komo_client::remote::{self, RemoteLogin};
use komo_kernel::pkce::{random_token, s256_challenge};
use komo_kernel::protocol::http::{DeviceTokenRequest, DeviceTokenResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 等浏览器回跳最多等多久。
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// `komo login <地址>`。
pub async fn login(
    home: &Path,
    url: &str,
    name: Option<&str>,
    no_open: bool,
) -> Result<String, String> {
    let base = normalize_base(url)?;
    let verifier = random_token();
    let state = random_token();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("绑定本机回跳端口失败：{error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let authorize = authorize_url(
        &base,
        &s256_challenge(&verifier),
        &redirect_uri,
        &state,
        name.unwrap_or("komo-cli"),
    )?;

    eprintln!("在浏览器里完成登录：\n{authorize}");
    if !no_open && !crate::commands::open_in_browser(&authorize) {
        eprintln!("没能自动打开浏览器，请手动打开上面的链接（在这台机器上登录后回跳到本机端口）。");
    }
    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_callback(&listener, &state))
        .await
        .map_err(|_| "等了 5 分钟浏览器还没回跳，已放弃；重新运行 `komo login`".to_string())??;

    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    let response = http
        .post(format!("{base}/auth/device/token"))
        .json(&DeviceTokenRequest {
            code,
            code_verifier: verifier,
            redirect_uri,
        })
        .send()
        .await
        .map_err(|error| format!("换设备令牌失败：{error}"))?;
    let status = response.status();
    let body = response.text().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!("换设备令牌失败（{status}）：{body}"));
    }
    let token: DeviceTokenResponse =
        serde_json::from_str(&body).map_err(|error| format!("令牌响应读不懂：{error}"))?;
    remote::write_remote(
        home,
        &RemoteLogin {
            base_url: base.clone(),
            token: token.token,
            session_id: token.session_id,
            subject: Some(token.subject.clone()),
        },
    )?;
    Ok(format!(
        "已登录 {base}（账号 {}）。之后的命令连这台远程 Gateway；`komo logout` 退出并回到本机。",
        token.subject
    ))
}

/// `komo logout`：吊销设备令牌（尽力而为）并删掉 `remote.json`。
pub async fn logout(home: &Path) -> Result<String, String> {
    let Some(login) = remote::read_remote(home)? else {
        return Ok("没有远程登录记录，什么都没做。".to_string());
    };
    let revoked = revoke(&login).await;
    remote::remove_remote(home)?;
    Ok(match revoked {
        Ok(()) => format!("已退出 {}（服务端的设备令牌已吊销）。", login.base_url),
        Err(error) => format!(
            "已删除本机的登录记录，但服务端的设备令牌没能吊销（{error}）；到网页设置里的设备清单手动吊销。"
        ),
    })
}

async fn revoke(login: &RemoteLogin) -> Result<(), String> {
    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    let response = http
        .delete(format!(
            "{}/v1/auth/sessions/{}",
            login.base_url, login.session_id
        ))
        .bearer_auth(&login.token)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    // 401 / 404 = 已经被吊销过了，目的达到。
    if status.is_success()
        || status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::NOT_FOUND
    {
        Ok(())
    } else {
        Err(format!("Gateway 回了 {status}"))
    }
}

/// 地址 → 去掉结尾 `/` 的根地址；只认 http(s)。
fn normalize_base(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    let parsed =
        reqwest::Url::parse(url).map_err(|error| format!("`{url}` 不是合法地址：{error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err(format!("`{url}` 要以 http:// 或 https:// 开头"));
    }
    if !matches!(parsed.path(), "" | "/") || parsed.query().is_some() {
        return Err(format!("`{url}` 要写成 Gateway 的根地址，不带路径"));
    }
    Ok(url.to_string())
}

fn authorize_url(
    base: &str,
    challenge: &str,
    redirect_uri: &str,
    state: &str,
    name: &str,
) -> Result<String, String> {
    let mut url = reqwest::Url::parse(&format!("{base}/auth/device/authorize"))
        .map_err(|error| error.to_string())?;
    url.query_pairs_mut()
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", state)
        .append_pair("name", name);
    Ok(url.into())
}

/// 在回环端口上等浏览器回跳：只认 `GET /callback?code=…&state=…`，`state` 要对得上。别的请求
/// （`/favicon.ico` 之类）回 404 继续等。
async fn wait_for_callback(listener: &TcpListener, state: &str) -> Result<String, String> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|error| format!("接收回跳失败：{error}"))?;
        let mut buffer = vec![0u8; 8192];
        let read =
            match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer)).await {
                Ok(Ok(read)) => read,
                _ => continue,
            };
        let outcome = parse_callback(&buffer[..read], state);
        let (status, text) = match &outcome {
            CallbackOutcome::Ignore => ("404 Not Found", "not found".to_string()),
            CallbackOutcome::Code(_) => {
                ("200 OK", "登录完成，可以关掉这个页面回到终端。".to_string())
            }
            CallbackOutcome::Failed(reason) => {
                ("400 Bad Request", format!("登录没有完成：{reason}"))
            }
        };
        let body = format!(
            "<!doctype html><meta charset=\"utf-8\"><title>komo</title><p>{}</p>",
            text.replace('&', "&amp;").replace('<', "&lt;")
        );
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
        match outcome {
            CallbackOutcome::Ignore => continue,
            CallbackOutcome::Code(code) => return Ok(code),
            CallbackOutcome::Failed(reason) => return Err(format!("登录没有完成：{reason}")),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CallbackOutcome {
    Ignore,
    Code(String),
    Failed(String),
}

fn parse_callback(raw: &[u8], state: &str) -> CallbackOutcome {
    let text = String::from_utf8_lossy(raw);
    let Some(line) = text.lines().next() else {
        return CallbackOutcome::Ignore;
    };
    let mut parts = line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return CallbackOutcome::Ignore;
    };
    let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return CallbackOutcome::Ignore;
    };
    if url.path() != "/callback" {
        return CallbackOutcome::Ignore;
    }
    let get = |key: &str| {
        url.query_pairs()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.into_owned())
    };
    if get("state").as_deref() != Some(state) {
        return CallbackOutcome::Failed("state 对不上（不是这次发起的登录）".into());
    }
    match get("code") {
        Some(code) if !code.is_empty() => CallbackOutcome::Code(code),
        _ => CallbackOutcome::Failed("回跳里没有授权码".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_callback_needs_the_matching_state_and_a_code() {
        let ok = b"GET /callback?code=abc&state=s1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert_eq!(
            parse_callback(ok, "s1"),
            CallbackOutcome::Code("abc".into())
        );
        assert!(matches!(
            parse_callback(ok, "other"),
            CallbackOutcome::Failed(_)
        ));
        let no_code = b"GET /callback?state=s1 HTTP/1.1\r\n\r\n";
        assert!(matches!(
            parse_callback(no_code, "s1"),
            CallbackOutcome::Failed(_)
        ));
        let other = b"GET /favicon.ico HTTP/1.1\r\n\r\n";
        assert_eq!(parse_callback(other, "s1"), CallbackOutcome::Ignore);
        assert_eq!(
            parse_callback(b"POST /callback HTTP/1.1\r\n\r\n", "s1"),
            CallbackOutcome::Ignore
        );
    }

    #[test]
    fn the_gateway_address_must_be_a_bare_http_origin() {
        assert_eq!(
            normalize_base("https://komo.example.com/").unwrap(),
            "https://komo.example.com"
        );
        assert!(normalize_base("komo.example.com").is_err());
        assert!(normalize_base("https://komo.example.com/app").is_err());
        assert!(normalize_base("ftp://komo.example.com").is_err());
    }
}
