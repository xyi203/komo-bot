//! 网页里完成 MCP 的 OAuth（`POST /v1/mcp/{name}/oauth/start` → 浏览器授权 → `GET …/oauth/callback`）：
//! 发起的 400 / 404 分支、state 单次使用、回调必须来自发起它的那条登录状态、错误页转义、成功后凭证落盘并
//! 重连。授权服务器是 `oauth_server.py`（带动态注册与换 token）。

use komo_gateway::service::test_support::harness::{FakeLlm, Gw, Home, config_toml};
use komo_kernel::types::digest::ContentHash;
use komo_store::{AuthKind, NewAuthSession, TursoAuthRepo};
use serde_json::Value;

use super::OAuthServer;

const PUBLIC_URL: &str = "https://komo.example.com";

fn oidc_block() -> String {
    format!(
        "[auth.oidc]\nissuer = \"https://id.example.com\"\nclient_id = \"komo\"\n\
         allowed_subjects = [\"u-1\"]\npublic_url = \"{PUBLIC_URL}\"\n"
    )
}

fn secure_block(port: u16) -> String {
    format!(
        "[mcp.servers.secure]\nurl = \"http://127.0.0.1:{port}/mcp\"\noauth = true\nexposure = \"direct\"\n"
    )
}

/// 直接在 `auth_sessions` 里放一条设备令牌（登录过程本身由 `tests/web/oidc.rs` 覆盖），返回令牌。
async fn device_token(gw: &Gw, label: &str) -> String {
    let token = format!("komodev_{label}_{}", komo_kernel::pkce::random_token());
    TursoAuthRepo::new(gw.state().db.clone())
        .create(NewAuthSession {
            kind: AuthKind::Device,
            token_hash: ContentHash::of_str(&token).as_str().to_string(),
            subject: "u-1".into(),
            email: None,
            label: label.into(),
            now: time::OffsetDateTime::now_utc(),
            expires_at: None,
        })
        .await
        .unwrap();
    token
}

async fn start_as(gw: &Gw, token: Option<&str>, name: &str) -> (u16, Value) {
    let mut request =
        reqwest::Client::new().post(format!("{}/v1/mcp/{name}/oauth/start", gw.base_url()));
    request = request.bearer_auth(token.unwrap_or(&gw.state().token));
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn callback_as(
    gw: &Gw,
    token: Option<&str>,
    name: &str,
    query: &[(&str, &str)],
) -> (u16, String) {
    let url = reqwest::Url::parse_with_params(
        &format!("{}/v1/mcp/{name}/oauth/callback", gw.base_url()),
        query,
    )
    .unwrap();
    let response = reqwest::Client::new()
        .get(url)
        .bearer_auth(token.unwrap_or(&gw.state().token))
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

fn param(url: &str, name: &str) -> String {
    reqwest::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == name)
        .unwrap_or_else(|| panic!("授权地址里没有 {name}：{url}"))
        .1
        .into_owned()
}

#[tokio::test]
async fn starting_a_web_login_refuses_what_cannot_work() {
    let server = OAuthServer::start("tok-1");
    // 没配公开地址：400，并指路 `komo auth mcp login`。
    let home = Home::with_config(&config_toml(&secure_block(server.port)));
    let gw = home.start(FakeLlm::finisher("好")).await;
    let (status, body) = start_as(&gw, None, "secure").await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("komo auth mcp login"),
        "{body}"
    );
    gw.stop().await;

    // 配了公开地址：不存在 404；不是 oauth 的 HTTP 服务器 400。
    let extra = format!(
        "{}\n{}\n[mcp.servers.plain]\nurl = \"http://127.0.0.1:{}/mcp\"\n",
        oidc_block(),
        secure_block(server.port),
        server.port
    );
    let home = Home::with_config(&config_toml(&extra));
    let gw = home.start(FakeLlm::finisher("好")).await;
    let (status, body) = start_as(&gw, None, "ghost").await;
    assert_eq!(status, 404, "{body}");
    let (status, body) = start_as(&gw, None, "plain").await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("oauth = true"),
        "{body}"
    );
    // 没登录的人不能发起。
    let anonymous = reqwest::Client::new()
        .post(format!("{}/v1/mcp/secure/oauth/start", gw.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
    gw.stop().await;
}

#[tokio::test]
async fn a_web_login_completes_once_for_the_login_that_started_it() {
    let server = OAuthServer::start("tok-web");
    let extra = format!("{}\n{}", oidc_block(), secure_block(server.port));
    let home = Home::with_config(&config_toml(&extra));
    let gw = home.start(FakeLlm::finisher("好")).await;
    let credentials = komo_tools::mcp_auth::credentials_path(home.path(), "secure");
    assert!(!credentials.exists());

    // 发起：授权地址指向授权服务器，回调地址由公开地址拼成，带 PKCE 与 state。
    let alice = device_token(&gw, "alice").await;
    let bob = device_token(&gw, "bob").await;
    let (status, started) = start_as(&gw, Some(&alice), "secure").await;
    assert_eq!(status, 200, "{started}");
    let url = started["authorization_url"].as_str().unwrap().to_string();
    assert!(
        url.starts_with(&format!("http://127.0.0.1:{}/authorize", server.port)),
        "{url}"
    );
    assert_eq!(
        param(&url, "redirect_uri"),
        format!("{PUBLIC_URL}/v1/mcp/secure/oauth/callback")
    );
    assert_eq!(param(&url, "code_challenge_method"), "S256");
    assert_eq!(param(&url, "client_id"), "komo-web-test");
    let state = param(&url, "state");

    // 缺参数 / 提供方报错（页面转义）/ 不认识的 state / 别的服务器名 / 别的登录状态：都 400，且不动凭证。
    let (status, _) = callback_as(&gw, Some(&alice), "secure", &[("state", &state)]).await;
    assert_eq!(status, 400, "缺 code");
    let (status, page) = callback_as(
        &gw,
        Some(&alice),
        "secure",
        &[
            ("error", "access_denied"),
            ("error_description", "<b>no</b>"),
        ],
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        page.contains("access_denied") && !page.contains("<b>"),
        "{page}"
    );
    let (status, page) = callback_as(
        &gw,
        Some(&alice),
        "secure",
        &[("code", "c"), ("state", "never-issued")],
    )
    .await;
    assert_eq!(status, 400);
    assert!(page.contains("授权已失效"), "{page}");
    assert!(!credentials.exists());
    // 别人（bob）拿到了 alice 的 state 也不行——而且这一次尝试不会烧掉 alice 的会话？
    // 会：`take` 先取走再核对，宁可让 alice 重新发起，也不给别人第二次试探的机会。
    let (status, _) = callback_as(
        &gw,
        Some(&bob),
        "secure",
        &[("code", "c"), ("state", &state)],
    )
    .await;
    assert_eq!(status, 400, "别的登录状态");
    let (status, _) = callback_as(
        &gw,
        Some(&alice),
        "secure",
        &[("code", "c"), ("state", &state)],
    )
    .await;
    assert_eq!(status, 400, "state 已经被取走作废");
    assert!(!credentials.exists());

    // 重新发起 → 正确的人用正确的 state 完成：凭证落盘，服务器随后连上；同一个 state 不能再用。
    let (_, started) = start_as(&gw, Some(&alice), "secure").await;
    let state = param(started["authorization_url"].as_str().unwrap(), "state");
    let (status, page) = callback_as(
        &gw,
        Some(&alice),
        "secure",
        &[("code", "the-code"), ("state", &state)],
    )
    .await;
    assert_eq!(status, 200, "{page}");
    assert!(page.contains("授权完成"), "{page}");
    let saved: Value = serde_json::from_slice(&std::fs::read(&credentials).unwrap()).unwrap();
    assert_eq!(saved["client_id"], "komo-web-test");
    assert_eq!(saved["token_response"]["access_token"], "tok-web");
    let (status, _) = callback_as(
        &gw,
        Some(&alice),
        "secure",
        &[("code", "the-code"), ("state", &state)],
    )
    .await;
    assert_eq!(status, 400, "state 单次使用");

    for _ in 0..200 {
        let (_, body) = gw.get_json("/v1/mcp").await;
        if body["servers"][0]["state"] == "connected" {
            gw.stop().await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("授权完成之后没有重连上");
}
