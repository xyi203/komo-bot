//! 网页壳的网关侧（`docs/clients.md` D5）：Host 校验、静态文件、登录路由的未登录行为。真数据目录、
//! 真 `service::start`、真 TCP。
//!
//! 配对码已删（`docs/redesign.md` P21）。原先靠配对码换 cookie 的几条测试（cookie 的写请求核对
//! Origin、cookie 开 SSE、cookie 不能再铸别的凭据）已删除，待统一验证阶段改成「直接往 `auth_sessions`
//! 写一条会话再带 cookie 请求」重写。

use komo_gateway::service::test_support::harness::{
    FakeLlm, GatewayBuilder, TestGateway, config_toml,
};

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("客户端")
}

/// 起一台带静态目录的 Gateway。目录跟着返回，测试结束才删。
async fn start(extra: &str) -> (TestGateway, tempfile::TempDir) {
    let web = tempfile::tempdir().expect("静态目录");
    std::fs::write(web.path().join("_shell.html"), "<html>shell</html>").unwrap();
    std::fs::create_dir(web.path().join("assets")).unwrap();
    std::fs::write(web.path().join("assets/app.js"), "console.log(1)").unwrap();
    let config = config_toml(&format!(
        "[web]\ndir = \"{}\"\n{extra}",
        web.path().display()
    ));
    let gateway = GatewayBuilder::new(&config)
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;
    (gateway, web)
}

#[tokio::test]
async fn a_foreign_host_is_refused_on_every_route() {
    let (gateway, _web) = start("").await;
    for path in ["/healthz", "/", "/v1/sessions", "/assets/app.js"] {
        let response = client()
            .get(format!("{}{path}", gateway.base_url()))
            .header("host", "evil.example.com")
            .bearer_auth(gateway.token())
            .send()
            .await
            .expect("请求");
        assert_eq!(response.status(), 403, "{path}");
    }
    let login = client()
        .get(format!("{}/auth/me", gateway.base_url()))
        .header("host", "evil.example.com")
        .send()
        .await
        .expect("请求");
    assert_eq!(login.status(), 403);
}

#[tokio::test]
async fn a_trusted_host_from_config_is_let_through() {
    let (gateway, _web) = start("\n[gateway]\ntrusted_hosts = [\"komo.example.com\"]\n").await;
    let response = client()
        .get(format!("{}/healthz", gateway.base_url()))
        .header("host", "komo.example.com")
        .send()
        .await
        .expect("请求");
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn without_a_login_the_api_is_closed_and_me_says_so() {
    let (gateway, _web) = start("").await;
    let base = gateway.base_url();
    let bare = client()
        .get(format!("{base}/v1/sessions"))
        .send()
        .await
        .expect("请求");
    assert_eq!(bare.status(), 401);
    let forged = client()
        .get(format!("{base}/v1/sessions"))
        .header("cookie", "komo_session=forged")
        .send()
        .await
        .expect("请求");
    assert_eq!(forged.status(), 401);
    let device = client()
        .get(format!("{base}/v1/sessions"))
        .bearer_auth("komodev_forged")
        .send()
        .await
        .expect("请求");
    assert_eq!(device.status(), 401);

    // /auth/me 不要登录，永远 200；没配 OIDC 就这么说。
    let me: serde_json::Value = client()
        .get(format!("{base}/auth/me"))
        .send()
        .await
        .expect("请求")
        .json()
        .await
        .expect("JSON");
    assert_eq!(me["authenticated"], false);
    assert_eq!(me["oidc_configured"], false);

    // 本机令牌照常认，并报成 local。
    let me: serde_json::Value = client()
        .get(format!("{base}/auth/me"))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("请求")
        .json()
        .await
        .expect("JSON");
    assert_eq!(me["authenticated"], true);
    assert_eq!(me["credential"], "local");

    // 没配 OIDC：发起登录是一张 404 页，设备登录同理。
    let login = client()
        .get(format!("{base}/auth/login"))
        .send()
        .await
        .expect("请求");
    assert_eq!(login.status(), 404);
}

#[tokio::test]
async fn static_files_need_no_auth_and_unknown_routes_fall_back_to_the_shell() {
    let (gateway, _web) = start("").await;
    let base = gateway.base_url();

    let shell = client()
        .get(format!("{base}/sessions/abc"))
        .send()
        .await
        .expect("请求");
    assert_eq!(shell.status(), 200);
    assert!(
        shell.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert_eq!(shell.text().await.unwrap(), "<html>shell</html>");

    let asset = client()
        .get(format!("{base}/assets/app.js"))
        .send()
        .await
        .expect("请求");
    assert_eq!(asset.status(), 200);
    assert!(
        asset.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );

    let missing = client()
        .get(format!("{base}/assets/gone.js"))
        .send()
        .await
        .expect("请求");
    assert_eq!(missing.status(), 404);

    // /v1 下没有的路径是 JSON 404，不是壳。
    let api = client()
        .get(format!("{base}/v1/nope"))
        .send()
        .await
        .expect("请求");
    assert_eq!(api.status(), 404);
    assert!(api.text().await.unwrap().contains("not_found"));

    let escape = client()
        .get(format!("{base}/..%2f..%2fetc/passwd"))
        .send()
        .await
        .expect("请求");
    assert_ne!(escape.status(), 200);
}

#[tokio::test]
async fn a_missing_web_dir_just_means_no_web_ui() {
    let config = config_toml("[web]\ndir = \"/nonexistent/komo/web\"\n");
    let gateway = GatewayBuilder::new(&config)
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;
    let root = client()
        .get(format!("{}/", gateway.base_url()))
        .send()
        .await
        .expect("请求");
    assert_eq!(root.status(), 404);
    let health = client()
        .get(format!("{}/healthz", gateway.base_url()))
        .send()
        .await
        .expect("请求");
    assert_eq!(health.status(), 200);
}
