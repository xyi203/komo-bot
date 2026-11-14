//! 登录的端到端（`docs/redesign.md` §2.9）：真 Gateway、真 TCP，身份提供方是本机的假 OIDC 服务
//! （axum；发现文档、JWKS、令牌端点、PKCE 校验、授权码单次使用，ID token 用测试里生成的 ES256 密钥签）。
//! 「浏览器」是测试里的 reqwest：自己收 `Set-Cookie`、自己带 `Cookie`，不跟随重定向。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use komo_gateway::service::test_support::harness::{
    FakeLlm, GatewayBuilder, TestGateway, config_toml,
};
use komo_kernel::pkce::{random_token, s256_challenge};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};

const PUBLIC_URL: &str = "https://komo.example.com";
const SECRET: &str = "topsecret";

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("客户端")
}

// ---------------------------------------------------------------- 假身份提供方

/// 这一次登录里提供方要签出怎样的 ID token。
#[derive(Clone)]
struct Mint {
    sub: String,
    email: Option<String>,
    email_verified: Option<bool>,
    /// 覆盖 nonce（`None` = 照抄授权请求里的）。
    nonce: Option<String>,
    /// exp 相对现在的秒数。
    exp_in: i64,
    aud: String,
    /// 用另一把（JWKS 里没有的）密钥签。
    wrong_key: bool,
    /// 令牌端点直接拒绝（`invalid_grant`）。
    reject: bool,
}

impl Mint {
    fn me() -> Mint {
        Mint {
            sub: "u-1".into(),
            email: Some("me@example.com".into()),
            email_verified: Some(true),
            nonce: None,
            exp_in: 3600,
            aud: "komo".into(),
            wrong_key: false,
            reject: false,
        }
    }
}

struct Grant {
    nonce: String,
    challenge: String,
    redirect_uri: String,
    mint: Mint,
}

struct Provider {
    origin: String,
    key: EcdsaKeyPair,
    other: EcdsaKeyPair,
    basic: bool,
    grants: Mutex<HashMap<String, Grant>>,
}

fn new_key() -> EcdsaKeyPair {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
}

impl Provider {
    async fn start(basic: bool) -> Arc<Provider> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let provider = Arc::new(Provider {
            origin,
            key: new_key(),
            other: new_key(),
            basic,
            grants: Mutex::new(HashMap::new()),
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .with_state(Arc::clone(&provider));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        provider
    }

    fn sign(&self, mint: &Mint, nonce: &str) -> String {
        let header = json!({ "alg": "ES256", "typ": "JWT", "kid": "k1" });
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut claims = json!({
            "iss": self.origin,
            "sub": mint.sub,
            "aud": mint.aud,
            "iat": now,
            "exp": now + mint.exp_in,
            "nonce": mint.nonce.clone().unwrap_or_else(|| nonce.to_string()),
        });
        if let Some(email) = &mint.email {
            claims["email"] = json!(email);
        }
        if let Some(verified) = mint.email_verified {
            claims["email_verified"] = json!(verified);
        }
        let signing = format!(
            "{}.{}",
            B64.encode(header.to_string()),
            B64.encode(claims.to_string())
        );
        let key = if mint.wrong_key {
            &self.other
        } else {
            &self.key
        };
        let signature = key.sign(&SystemRandom::new(), signing.as_bytes()).unwrap();
        format!("{signing}.{}", B64.encode(signature.as_ref()))
    }

    /// 浏览器在提供方那边「登录成功」：照授权请求里的 state / nonce / challenge 记下一个码。
    fn authorize(&self, login: &Login, mint: Mint) -> String {
        let code = random_token();
        self.grants.lock().unwrap().insert(
            code.clone(),
            Grant {
                nonce: login.param("nonce"),
                challenge: login.param("code_challenge"),
                redirect_uri: login.param("redirect_uri"),
                mint,
            },
        );
        code
    }
}

async fn discovery(State(provider): State<Arc<Provider>>) -> Json<Value> {
    let methods = if provider.basic {
        json!(["client_secret_basic"])
    } else {
        json!(["client_secret_post"])
    };
    Json(json!({
        "issuer": provider.origin,
        "authorization_endpoint": format!("{}/authorize", provider.origin),
        "token_endpoint": format!("{}/token", provider.origin),
        "jwks_uri": format!("{}/jwks", provider.origin),
        "token_endpoint_auth_methods_supported": methods,
    }))
}

async fn jwks(State(provider): State<Arc<Provider>>) -> Json<Value> {
    let point = provider.key.public_key().as_ref();
    Json(json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": "k1",
        "x": B64.encode(&point[1..33]), "y": B64.encode(&point[33..65]),
    }]}))
}

async fn token(
    State(provider): State<Arc<Provider>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let bad = |error: &str| (StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
    // 客户端认证：按发现文档里声明的那一种。
    let authed = if provider.basic {
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("komo:{SECRET}"))
        );
        headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(expected.as_str())
    } else {
        form.get("client_id").map(String::as_str) == Some("komo")
            && form.get("client_secret").map(String::as_str) == Some(SECRET)
    };
    if !authed {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_client" })),
        );
    }
    let Some(code) = form.get("code") else {
        return bad("invalid_request");
    };
    // 授权码只能用一次。
    let Some(grant) = provider.grants.lock().unwrap().remove(code) else {
        return bad("invalid_grant");
    };
    if grant.mint.reject {
        return bad("invalid_grant");
    }
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    if s256_challenge(&verifier) != grant.challenge {
        return bad("invalid_grant");
    }
    if form.get("redirect_uri") != Some(&grant.redirect_uri) {
        return bad("invalid_grant");
    }
    let id_token = provider.sign(&grant.mint, &grant.nonce);
    (
        StatusCode::OK,
        Json(json!({ "id_token": id_token, "access_token": "at", "token_type": "Bearer" })),
    )
}

// ---------------------------------------------------------------- 浏览器一侧

/// 一次发起登录的结果：Gateway 把浏览器跳去提供方的地址 + 绑定浏览器的 cookie。
struct Login {
    authorize_url: reqwest::Url,
    state: String,
    /// `komo_oidc=<state>`（要原样带回 `/auth/callback`）。
    cookie: String,
}

impl Login {
    fn param(&self, name: &str) -> String {
        self.authorize_url
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .unwrap_or_else(|| panic!("授权地址里没有 {name}：{}", self.authorize_url))
    }
}

fn set_cookies(response: &reqwest::Response) -> Vec<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap().to_string())
        .collect()
}

/// 取某个 cookie 的 `name=value`；`Max-Age=0` 的清除指令不算。
fn cookie_pair(response: &reqwest::Response, name: &str) -> Option<String> {
    set_cookies(response)
        .into_iter()
        .filter(|cookie| !cookie.contains("Max-Age=0"))
        .find_map(|cookie| {
            let pair = cookie.split(';').next()?.to_string();
            pair.starts_with(&format!("{name}=")).then_some(pair)
        })
}

fn location(response: &reqwest::Response) -> String {
    response.headers()["location"].to_str().unwrap().to_string()
}

async fn begin(base: &str, path: &str) -> Login {
    let response = client().get(format!("{base}{path}")).send().await.unwrap();
    assert_eq!(response.status(), 303, "{path}");
    let cookie = cookie_pair(&response, "komo_oidc").expect("绑定浏览器的 cookie");
    let set = set_cookies(&response).join("\n");
    assert!(
        set.contains("HttpOnly") && set.contains("Path=/auth"),
        "{set}"
    );
    let authorize_url = reqwest::Url::parse(&location(&response)).unwrap();
    let state = authorize_url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    assert_eq!(cookie, format!("komo_oidc={state}"));
    Login {
        authorize_url,
        state,
        cookie,
    }
}

async fn callback(
    base: &str,
    code: Option<&str>,
    state: Option<&str>,
    cookie: Option<&str>,
) -> reqwest::Response {
    let mut query = Vec::new();
    if let Some(code) = code {
        query.push(("code", code));
    }
    if let Some(state) = state {
        query.push(("state", state));
    }
    let url = reqwest::Url::parse_with_params(&format!("{base}/auth/callback"), &query).unwrap();
    let mut request = client().get(url);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    request.send().await.unwrap()
}

struct Env {
    gateway: TestGateway,
    provider: Arc<Provider>,
    base: String,
}

async fn start(basic: bool) -> Env {
    let provider = Provider::start(basic).await;
    let config = config_toml(&format!(
        "\n[auth.oidc]\nissuer = \"{}\"\nclient_id = \"komo\"\nallowed_subjects = [\"u-1\"]\n\
         allowed_emails = [\"me@example.com\"]\npublic_url = \"{PUBLIC_URL}\"\n",
        provider.origin
    ));
    let gateway = GatewayBuilder::new(&config)
        .env(&format!(
            "KOMO_LLM_API_KEY=test-key\nKOMO_OIDC_CLIENT_SECRET={SECRET}\n"
        ))
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;
    let base = gateway.base_url().to_string();
    Env {
        gateway,
        provider,
        base,
    }
}

impl Env {
    /// 走完网页登录，拿到 `komo_session=<token>`。
    async fn web_login(&self, path: &str, mint: Mint) -> (String, reqwest::Response) {
        let login = begin(&self.base, path).await;
        let code = self.provider.authorize(&login, mint);
        let response = callback(
            &self.base,
            Some(&code),
            Some(&login.state),
            Some(&login.cookie),
        )
        .await;
        assert_eq!(response.status(), 303, "登录没成功");
        let session = cookie_pair(&response, "komo_session").expect("会话 cookie");
        (session, response)
    }

    async fn get_with(&self, path: &str, cookie: &str) -> reqwest::Response {
        client()
            .get(format!("{}{path}", self.base))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
    }

    async fn me(&self, cookie: &str) -> Value {
        self.get_with("/auth/me", cookie)
            .await
            .json()
            .await
            .unwrap()
    }

    fn local(&self) -> String {
        self.gateway.token().to_string()
    }
}

// ---------------------------------------------------------------- 网页登录

/// 成功路径（密钥走 post 方式）：发起 → 提供方 → 回调种会话 cookie → 带 cookie 能用 `/v1/**`、`/auth/me` 报账号；
/// 写请求核对 Origin；退出吊销；回跳的 `next` 只认站内路径。
#[tokio::test]
async fn a_web_login_signs_the_browser_in_and_a_logout_signs_it_out() {
    let env = start(false).await;

    // 没登录：/v1/** 401，/auth/me 200 并说明配了 OIDC。
    let anonymous = client()
        .get(format!("{}/v1/sessions", env.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
    let me: Value = client()
        .get(format!("{}/auth/me", env.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["authenticated"], false);
    assert_eq!(me["oidc_configured"], true);

    // 授权地址：标准授权码 + PKCE（S256），回调地址由公开地址拼成。
    let login = begin(&env.base, "/auth/login?next=/chats/abc").await;
    assert_eq!(
        login.authorize_url.origin().ascii_serialization(),
        env.provider.origin
    );
    assert_eq!(login.authorize_url.path(), "/authorize");
    assert_eq!(login.param("response_type"), "code");
    assert_eq!(login.param("client_id"), "komo");
    assert_eq!(
        login.param("redirect_uri"),
        format!("{PUBLIC_URL}/auth/callback")
    );
    assert_eq!(login.param("code_challenge_method"), "S256");
    assert!(login.param("scope").contains("openid"));
    assert_eq!(login.param("code_challenge").len(), 43);
    assert_ne!(login.param("state"), login.param("nonce"));

    let code = env.provider.authorize(&login, Mint::me());
    let response = callback(
        &env.base,
        Some(&code),
        Some(&login.state),
        Some(&login.cookie),
    )
    .await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), "/chats/abc", "跳回发起时的站内路径");
    let session = cookie_pair(&response, "komo_session").expect("会话 cookie");
    let cookies = set_cookies(&response).join("\n");
    assert!(
        cookies.contains("HttpOnly") && cookies.contains("SameSite=Lax"),
        "{cookies}"
    );
    assert!(
        cookies.contains("Secure"),
        "公开地址是 https：带 Secure：{cookies}"
    );
    assert!(cookies.contains("Max-Age=2592000"), "{cookies}");
    assert!(
        cookies.contains("komo_oidc=;") && cookies.contains("Max-Age=0"),
        "临时 cookie 用完即清：{cookies}"
    );
    assert_eq!(response.headers()["cache-control"], "no-store");

    // 带 cookie：GET 通，me 报账号。
    let sessions = env.get_with("/v1/sessions", &session).await;
    assert_eq!(sessions.status(), 200);
    let me = env.me(&session).await;
    assert_eq!(me["authenticated"], true);
    assert_eq!(me["credential"], "web");
    assert_eq!(me["subject"], "u-1");
    assert_eq!(me["email"], "me@example.com");

    // 写请求：同源（Origin 等于 Host 或公开地址）才行；没有 Origin 或跨源是 403。
    let post = |origin: Option<&str>| {
        let mut request = client()
            .post(format!("{}/v1/sessions", env.base))
            .header("cookie", &session)
            .json(&json!({}));
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        request.send()
    };
    assert_eq!(post(None).await.unwrap().status(), 403, "没有 Origin");
    let cross = post(Some("https://evil.example.com")).await.unwrap();
    assert_eq!(cross.status(), 403);
    assert!(cross.text().await.unwrap().contains("denied"));
    assert_eq!(
        post(Some(&env.base)).await.unwrap().status(),
        200,
        "同 Host"
    );
    assert_eq!(
        post(Some(PUBLIC_URL)).await.unwrap().status(),
        200,
        "公开地址"
    );
    // 本机令牌不看 Origin。
    let local = client()
        .post(format!("{}/v1/sessions", env.base))
        .bearer_auth(env.local())
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(local.status(), 200);

    // 登录状态清单：能看到这条、标 current；不带令牌与哈希。
    let list: Value = env
        .get_with("/v1/auth/sessions", &session)
        .await
        .json()
        .await
        .unwrap();
    let rows = list["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{list}");
    assert_eq!(rows[0]["kind"], "web");
    assert_eq!(rows[0]["current"], true);
    assert_eq!(rows[0]["email"], "me@example.com");
    assert!(
        !list
            .to_string()
            .contains(session.trim_start_matches("komo_session="))
    );

    // 退出：跨源的退出被拒，同源的吊销并清 cookie；之后同一个 cookie 401。
    let evil_logout = client()
        .post(format!("{}/auth/logout", env.base))
        .header("cookie", &session)
        .header("origin", "https://evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(evil_logout.status(), 403);
    assert_eq!(
        env.get_with("/v1/sessions", &session).await.status(),
        200,
        "没被别人退掉"
    );
    let logout = client()
        .post(format!("{}/auth/logout", env.base))
        .header("cookie", &session)
        .header("origin", &env.base)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), 200);
    assert!(set_cookies(&logout).join("\n").contains("komo_session=;"));
    assert_eq!(env.get_with("/v1/sessions", &session).await.status(), 401);
    assert_eq!(env.me(&session).await["authenticated"], false);

    // `next` 防开放重定向：站外与协议相对地址都回 `/`。
    for next in ["//evil.example.com", "https://evil.example.com", "/%0d%0ax"] {
        let (_, response) = env
            .web_login(&format!("/auth/login?next={next}"), Mint::me())
            .await;
        let target = location(&response);
        assert!(
            target == "/" || target.starts_with("/%0d"),
            "{next} → {target}"
        );
        assert!(!target.contains("evil"), "{next} → {target}");
    }
    let (_, response) = env
        .web_login("/auth/login?next=//evil.example.com", Mint::me())
        .await;
    assert_eq!(location(&response), "/");
}

/// 提供方只列了 client_secret_basic：用 Basic 认证换令牌；名单按邮箱（忽略大小写）放行。
#[tokio::test]
async fn a_provider_that_only_offers_basic_auth_is_used_with_basic_auth_and_emails_match_case_insensitively()
 {
    let env = start(true).await;
    let mint = Mint {
        sub: "u-9".into(),
        email: Some("ME@Example.com".into()),
        ..Mint::me()
    };
    let (session, _) = env.web_login("/auth/login", mint).await;
    let me = env.me(&session).await;
    assert_eq!(me["subject"], "u-9");
}

/// 失败矩阵：每一种都**不发会话 cookie**，状态码对，之后仍未登录。
#[tokio::test]
async fn every_way_a_login_can_go_wrong_ends_without_a_session() {
    let env = start(false).await;
    let base = &env.base;

    // (名字, ID token 的样子, 期望状态码)
    let cases: Vec<(&str, Mint, u16)> = vec![
        (
            "不在名单里",
            Mint {
                sub: "stranger".into(),
                email: Some("x@example.com".into()),
                ..Mint::me()
            },
            403,
        ),
        (
            "只靠邮箱放行，但提供方说邮箱没验证",
            Mint {
                sub: "u-2".into(),
                email_verified: Some(false),
                ..Mint::me()
            },
            403,
        ),
        (
            "nonce 对不上",
            Mint {
                nonce: Some("another".into()),
                ..Mint::me()
            },
            400,
        ),
        (
            "已过期",
            Mint {
                exp_in: -3600,
                ..Mint::me()
            },
            400,
        ),
        (
            "aud 不是本客户端",
            Mint {
                aud: "someone-else".into(),
                ..Mint::me()
            },
            400,
        ),
        (
            "签名不对",
            Mint {
                wrong_key: true,
                ..Mint::me()
            },
            400,
        ),
        (
            "令牌端点拒绝",
            Mint {
                reject: true,
                ..Mint::me()
            },
            400,
        ),
    ];
    for (name, mint, status) in cases {
        let login = begin(base, "/auth/login").await;
        let code = env.provider.authorize(&login, mint);
        let response = callback(base, Some(&code), Some(&login.state), Some(&login.cookie)).await;
        assert_eq!(response.status(), status, "{name}");
        assert!(
            cookie_pair(&response, "komo_session").is_none(),
            "{name}：不该发会话"
        );
        assert!(
            set_cookies(&response).join("\n").contains("komo_oidc=;"),
            "{name}：临时 cookie 照清"
        );
        let page = response.text().await.unwrap();
        assert!(page.contains("<h1"), "{name}：是一张页面：{page}");
        assert!(!page.contains(SECRET), "{name}：页面里没有密钥");
    }

    // state 重放：成功一次之后，同一个 state 带新的有效码也不认。
    let login = begin(base, "/auth/login").await;
    let first = env.provider.authorize(&login, Mint::me());
    let ok = callback(base, Some(&first), Some(&login.state), Some(&login.cookie)).await;
    assert_eq!(ok.status(), 303);
    let second = env.provider.authorize(&login, Mint::me());
    let replay = callback(base, Some(&second), Some(&login.state), Some(&login.cookie)).await;
    assert_eq!(replay.status(), 400, "state 用一次作废");
    assert!(cookie_pair(&replay, "komo_session").is_none());

    // 缺 cookie / cookie 里的 state 不是这一次的 / 不认识的 state / 缺参数 / 提供方回 error。
    let login = begin(base, "/auth/login").await;
    let code = env.provider.authorize(&login, Mint::me());
    let no_cookie = callback(base, Some(&code), Some(&login.state), None).await;
    assert_eq!(no_cookie.status(), 400, "缺 komo_oidc cookie（登录 CSRF）");
    // 缺 cookie 的那次也把 state 作废了：重新来。
    let login = begin(base, "/auth/login").await;
    let code = env.provider.authorize(&login, Mint::me());
    let wrong = callback(
        base,
        Some(&code),
        Some(&login.state),
        Some("komo_oidc=other"),
    )
    .await;
    assert_eq!(wrong.status(), 400);
    let unknown = callback(
        base,
        Some("c"),
        Some("never-issued"),
        Some("komo_oidc=never-issued"),
    )
    .await;
    assert_eq!(unknown.status(), 400);
    assert_eq!(callback(base, None, Some("s"), None).await.status(), 400);
    assert_eq!(callback(base, Some("c"), None, None).await.status(), 400);
    let denied = client()
        .get(
            reqwest::Url::parse_with_params(
                &format!("{base}/auth/callback"),
                &[("error", "access_denied<script>")],
            )
            .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    let page = denied.text().await.unwrap();
    assert!(
        page.contains("access_denied") && !page.contains("<script>"),
        "错误码转义：{page}"
    );

    let me: Value = client()
        .get(format!("{base}/auth/me"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["authenticated"], false);

    // 失败之后正常登录仍然成功。
    let (session, _) = env.web_login("/auth/login", Mint::me()).await;
    assert_eq!(env.get_with("/v1/sessions", &session).await.status(), 200);
}

// ---------------------------------------------------------------- 设备登录

async fn device_authorize(
    env: &Env,
    challenge: &str,
    redirect: &str,
    extra: &str,
    cookie: Option<&str>,
) -> reqwest::Response {
    let mut request = client().get(format!(
        "{}/auth/device/authorize?code_challenge={challenge}&code_challenge_method=S256&redirect_uri={}&state=xyz&name=%E6%A1%8C%E9%9D%A2{extra}",
        env.base,
        urlencode(redirect)
    ));
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    request.send().await.unwrap()
}

fn urlencode(text: &str) -> String {
    reqwest::Url::parse_with_params("http://x/", &[("v", text)])
        .unwrap()
        .query()
        .unwrap()
        .trim_start_matches("v=")
        .to_string()
}

async fn exchange(env: &Env, code: &str, verifier: &str, redirect: &str) -> reqwest::Response {
    client()
        .post(format!("{}/auth/device/token", env.base))
        .json(&json!({ "code": code, "code_verifier": verifier, "redirect_uri": redirect }))
        .send()
        .await
        .unwrap()
}

/// 设备登录：回跳白名单、PKCE、一次性码；换到的设备令牌长期有效、可列出、可吊销，不发 cookie。
#[tokio::test]
async fn a_device_login_exchanges_a_one_time_code_for_a_revocable_token() {
    let env = start(false).await;
    let redirect = "http://127.0.0.1:53211/callback";
    let verifier = random_token();
    let challenge = s256_challenge(&verifier);

    // 回跳地址不在白名单 / 方法不是 S256 / challenge 不像样：都是 400 页，不发起任何跳转。
    for bad in [
        "https://evil.example.com/cb",
        "http://evil.example.com:80/cb",
        "http://127.0.0.1/cb",
        "komo://auth/other",
    ] {
        let response = device_authorize(&env, &challenge, bad, "", None).await;
        assert_eq!(response.status(), 400, "{bad}");
        assert!(response.headers().get("location").is_none(), "{bad}");
    }
    let plain = client()
        .get(format!(
            "{}/auth/device/authorize?code_challenge={challenge}&code_challenge_method=plain&redirect_uri={}",
            env.base,
            urlencode(redirect)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), 400);
    let short = device_authorize(&env, "short", redirect, "", None).await;
    assert_eq!(short.status(), 400);

    // 正常发起：没登录过 → 跳去提供方；提供方回调 → 带码跳回客户端，不种 cookie。
    let response = device_authorize(&env, &challenge, redirect, "", None).await;
    assert_eq!(response.status(), 303);
    let login = {
        let authorize_url = reqwest::Url::parse(&location(&response)).unwrap();
        let cookie = cookie_pair(&response, "komo_oidc").unwrap();
        let state = cookie.trim_start_matches("komo_oidc=").to_string();
        Login {
            authorize_url,
            state,
            cookie,
        }
    };
    let code = env.provider.authorize(&login, Mint::me());
    let back = callback(
        &env.base,
        Some(&code),
        Some(&login.state),
        Some(&login.cookie),
    )
    .await;
    assert_eq!(back.status(), 303);
    assert!(
        cookie_pair(&back, "komo_session").is_none(),
        "设备登录不种网页 cookie"
    );
    let target = reqwest::Url::parse(&location(&back)).unwrap();
    assert_eq!(
        target.origin().ascii_serialization(),
        "http://127.0.0.1:53211"
    );
    let pairs: HashMap<_, _> = target.query_pairs().into_owned().collect();
    assert_eq!(pairs["state"], "xyz", "客户端的 state 原样带回");
    let device_code = pairs["code"].clone();

    // 码 + 对的 verifier + 同一个 redirect_uri → 设备令牌。
    let exchanged = exchange(&env, &device_code, &verifier, redirect).await;
    assert_eq!(exchanged.status(), 200);
    assert_eq!(exchanged.headers()["cache-control"], "no-store");
    let body: Value = exchanged.json().await.unwrap();
    let token = body["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("komodev_"), "{body}");
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["email"], "me@example.com");
    let bearer = |token: &str| {
        client()
            .get(format!("{}/auth/me", env.base))
            .bearer_auth(token.to_string())
            .send()
    };
    let me: Value = bearer(&token).await.unwrap().json().await.unwrap();
    assert_eq!(
        (me["credential"].as_str(), me["subject"].as_str()),
        (Some("device"), Some("u-1"))
    );
    let listed: Value = client()
        .get(format!("{}/v1/auth/sessions", env.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["sessions"][0]["kind"], "device");
    assert_eq!(listed["sessions"][0]["label"], "桌面");
    assert_eq!(listed["sessions"][0]["current"], true);
    assert!(
        listed["sessions"][0]
            .get("expires_at")
            .is_none_or(Value::is_null),
        "设备令牌不过期"
    );

    // 同一个码再换：401（用一次就作废）。
    assert_eq!(
        exchange(&env, &device_code, &verifier, redirect)
            .await
            .status(),
        401
    );

    // PKCE 不匹配 / redirect_uri 不一样：401，而且码当场作废（对的 verifier 随后也换不到）。
    for (name, wrong_verifier, wrong_redirect) in [
        ("verifier 不对", Some(random_token()), None),
        (
            "redirect_uri 不一样",
            None,
            Some("http://127.0.0.1:1/other"),
        ),
    ] {
        let response = device_authorize(&env, &challenge, redirect, "", None).await;
        let authorize_url = reqwest::Url::parse(&location(&response)).unwrap();
        let cookie = cookie_pair(&response, "komo_oidc").unwrap();
        let state = cookie.trim_start_matches("komo_oidc=").to_string();
        let login = Login {
            authorize_url,
            state,
            cookie,
        };
        let code = env.provider.authorize(&login, Mint::me());
        let back = callback(
            &env.base,
            Some(&code),
            Some(&login.state),
            Some(&login.cookie),
        )
        .await;
        let target = reqwest::Url::parse(&location(&back)).unwrap();
        let code = target
            .query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .into_owned();
        let bad = exchange(
            &env,
            &code,
            wrong_verifier.as_deref().unwrap_or(&verifier),
            wrong_redirect.unwrap_or(redirect),
        )
        .await;
        assert_eq!(bad.status(), 401, "{name}");
        assert_eq!(
            exchange(&env, &code, &verifier, redirect).await.status(),
            401,
            "{name}：码已作废"
        );
    }

    // 已登录的浏览器（有效会话 cookie）发起设备登录：不再绕提供方，直接带码跳回。
    let (session, _) = env.web_login("/auth/login", Mint::me()).await;
    let direct = device_authorize(&env, &challenge, redirect, "", Some(&session)).await;
    assert_eq!(direct.status(), 303);
    assert!(
        location(&direct).starts_with(redirect),
        "{}",
        location(&direct)
    );
    assert!(location(&direct).contains("code="));

    // 吊销设备令牌（用本机令牌管）：之后它 401；吊销不存在的 id 是 404。
    let id = body["session_id"].as_str().unwrap();
    let revoked = client()
        .delete(format!("{}/v1/auth/sessions/{id}", env.base))
        .bearer_auth(env.local())
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), 200);
    let after = client()
        .get(format!("{}/v1/sessions", env.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 401);
    let missing = client()
        .delete(format!("{}/v1/auth/sessions/{id}", env.base))
        .bearer_auth(env.local())
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

/// 登录状态落盘：重启 Gateway 之后，网页会话 cookie 与设备令牌仍然有效。
#[tokio::test]
async fn sessions_survive_a_gateway_restart() {
    let mut env = start(false).await;
    let (session, _) = env.web_login("/auth/login", Mint::me()).await;
    assert_eq!(env.get_with("/v1/sessions", &session).await.status(), 200);
    let provider_origin = env.provider.origin.clone();
    let config = config_toml(&format!(
        "\n[auth.oidc]\nissuer = \"{provider_origin}\"\nclient_id = \"komo\"\nallowed_subjects = [\"u-1\"]\n\
         allowed_emails = [\"me@example.com\"]\npublic_url = \"{PUBLIC_URL}\"\n"
    ));
    env.gateway.restart_with(&config).await;
    env.base = env.gateway.base_url().to_string();
    assert_eq!(
        env.get_with("/v1/sessions", &session).await.status(),
        200,
        "重启后 cookie 仍有效"
    );
    assert_eq!(env.me(&session).await["subject"], "u-1");
}
