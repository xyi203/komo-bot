//! 网页壳的网关侧（`docs/clients.md` D5、D6）：Host 校验、配对换 cookie、cookie 鉴权与同源核对、
//! 静态文件。真数据目录、真 `service::start`、真 TCP。

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

async fn pair_code(gateway: &TestGateway) -> String {
    let response = client()
        .post(format!("{}/v1/web/pair-code", gateway.base_url()))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("请求");
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("JSON");
    body["code"].as_str().expect("码").to_string()
}

/// 配对换 cookie，返回 `komo_session=...` 这一对。
async fn open_session(gateway: &TestGateway, code: &str) -> String {
    let response = client()
        .post(format!("{}/v1/web/session", gateway.base_url()))
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await
        .expect("请求");
    assert_eq!(response.status(), 200);
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .expect("Set-Cookie")
        .to_str()
        .unwrap()
        .to_string();
    for part in ["HttpOnly", "SameSite=Strict", "Path=/"] {
        assert!(set_cookie.contains(part), "{set_cookie}");
    }
    set_cookie.split(';').next().unwrap().to_string()
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
    let paired = client()
        .post(format!("{}/v1/web/session", gateway.base_url()))
        .header("host", "evil.example.com")
        .json(&serde_json::json!({ "code": "x" }))
        .send()
        .await
        .expect("请求");
    assert_eq!(paired.status(), 403);
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
async fn a_pair_code_needs_the_bearer_token() {
    let (gateway, _web) = start("").await;
    let response = client()
        .post(format!("{}/v1/web/pair-code", gateway.base_url()))
        .send()
        .await
        .expect("请求");
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn a_pair_code_opens_a_cookie_session_once() {
    let (gateway, _web) = start("").await;
    let code = pair_code(&gateway).await;
    let cookie = open_session(&gateway, &code).await;

    // 同一个码第二次不行。
    let again = client()
        .post(format!("{}/v1/web/session", gateway.base_url()))
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await
        .expect("请求");
    assert_eq!(again.status(), 401);

    // cookie 能列会话；没带的 401。
    let url = format!("{}/v1/sessions", gateway.base_url());
    let listed = client()
        .get(&url)
        .header("cookie", &cookie)
        .send()
        .await
        .expect("请求");
    assert_eq!(listed.status(), 200);
    let bare = client().get(&url).send().await.expect("请求");
    assert_eq!(bare.status(), 401);
    let forged = client()
        .get(&url)
        .header("cookie", "komo_session=forged")
        .send()
        .await
        .expect("请求");
    assert_eq!(forged.status(), 401);
}

#[tokio::test]
async fn a_cookie_cannot_mint_another_pair_code() {
    let (gateway, _web) = start("").await;
    let cookie = open_session(&gateway, &pair_code(&gateway).await).await;
    let response = client()
        .post(format!("{}/v1/web/pair-code", gateway.base_url()))
        .header("cookie", &cookie)
        .header("origin", gateway.base_url())
        .send()
        .await
        .expect("请求");
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn a_cookie_write_from_another_origin_is_refused() {
    let (gateway, _web) = start("").await;
    let cookie = open_session(&gateway, &pair_code(&gateway).await).await;
    let url = format!("{}/v1/sessions", gateway.base_url());

    let cross = client()
        .post(&url)
        .header("cookie", &cookie)
        .header("origin", "http://evil.example.com")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("请求");
    assert_eq!(cross.status(), 403);

    let no_origin = client()
        .post(&url)
        .header("cookie", &cookie)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("请求");
    assert_eq!(no_origin.status(), 403);

    let same = client()
        .post(&url)
        .header("cookie", &cookie)
        .header("origin", gateway.base_url())
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("请求");
    assert_eq!(same.status(), 200, "同源的写请求放行");

    // Bearer 不受 Origin 约束（CLI 不带 Origin）。
    let bearer = client()
        .post(&url)
        .bearer_auth(gateway.token())
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("请求");
    assert_eq!(bearer.status(), 200);
}

#[tokio::test]
async fn an_sse_stream_opens_with_the_cookie() {
    let (gateway, _web) = start("").await;
    let cookie = open_session(&gateway, &pair_code(&gateway).await).await;
    let (code, body) = gateway.post("/v1/sessions", serde_json::json!({})).await;
    assert_eq!(code, 200, "{body}");
    let session = serde_json::from_str::<serde_json::Value>(&body).unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    let response = client()
        .get(format!(
            "{}/v1/sessions/{session}/events",
            gateway.base_url()
        ))
        .header("cookie", &cookie)
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("请求");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
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
