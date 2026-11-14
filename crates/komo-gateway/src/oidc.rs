//! 标准 OIDC 客户端（授权码流程 + PKCE，`docs/redesign.md` §2.9）：发现文档、授权地址、换令牌、
//! ID token 验签与声明校验。
//!
//! **不认任何提供方的专有路径**：端点全部来自 `{issuer}/.well-known/openid-configuration`。
//! 不引 OIDC / JWT 库：验签只要三种算法（RS256 / ES256 / EdDSA），直接用工作区里已有的
//! `ring`（rustls 的 ring provider 拉进来的同一个包），其余是 JSON 与 base64url。
//!
//! 校验清单（ID token）：算法在白名单内（拒 `none` 与 HS*，不让攻击者把公钥当 HMAC 密钥）、签名、
//! `iss`、`aud`（含 `azp`）、`exp` / `nbf`（容 60 秒时钟差）、`nonce`。允许名单由调用方判
//! （[`IdentityClaims::allowed_by`]）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use komo_kernel::protocol::config::OidcConfig;
use serde::Deserialize;
use tokio::sync::Mutex;

/// 发现文档与 JWKS 缓存多久。
const CACHE_TTL: Duration = Duration::from_secs(3600);
/// 验签找不到 `kid` 时强制重取 JWKS 的最短间隔（提供方轮换密钥时用，防被人拿乱 kid 打爆）。
const JWKS_REFETCH_COOLDOWN: Duration = Duration::from_secs(60);
/// 时钟差容忍。
const LEEWAY_SECS: i64 = 60;
/// 请求提供方的超时。
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    /// 提供方连不上 / 文档不合规：运维要看的，不是用户的错。
    #[error("身份提供方不可用：{0}")]
    Provider(String),
    /// 提供方拒绝了授权码（过期、重放、redirect_uri 不符 …）。
    #[error("换令牌失败：{0}")]
    Exchange(String),
    /// ID token 没过校验。
    #[error("ID token 无效：{0}")]
    Token(String),
}

/// 发现文档里 komo 用到的几项。
#[derive(Debug, Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct JwkSet {
    #[serde(default)]
    keys: Vec<Jwk>,
}

#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kty: String,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    alg: Option<String>,
    #[serde(default, rename = "use")]
    usage: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

/// 校验过的身份声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityClaims {
    pub subject: String,
    pub email: Option<String>,
    /// 提供方明确说邮箱没验证过时是 `Some(false)`。
    pub email_verified: Option<bool>,
}

impl IdentityClaims {
    /// 在 `allowed_subjects` 或 `allowed_emails` 里（邮箱忽略大小写，且不能是明说没验证过的）。
    pub fn allowed_by(&self, config: &OidcConfig) -> bool {
        if config.allowed_subjects.iter().any(|s| *s == self.subject) {
            return true;
        }
        if self.email_verified == Some(false) {
            return false;
        }
        self.email.as_deref().is_some_and(|email| {
            config
                .allowed_emails
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(email))
        })
    }
}

/// 换令牌的结果：ID token 必有，access token 只在要去 userinfo 补邮箱时用。
struct TokenResponse {
    id_token: String,
    access_token: Option<String>,
}

pub struct OidcClient {
    config: OidcConfig,
    http: reqwest::Client,
    discovery: Mutex<Option<(Instant, Arc<Discovery>)>>,
    jwks: Mutex<Option<(Instant, Arc<JwkSet>)>>,
    jwks_forced_at: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for OidcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcClient")
            .field("issuer", &self.config.issuer)
            .finish_non_exhaustive()
    }
}

impl OidcClient {
    pub fn new(config: OidcConfig) -> OidcClient {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            // 发现文档与令牌端点不该重定向：跟着跳等于让提供方（或中间人）把请求带去别处。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        OidcClient {
            config,
            http,
            discovery: Mutex::new(None),
            jwks: Mutex::new(None),
            jwks_forced_at: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &OidcConfig {
        &self.config
    }

    async fn discovery(&self) -> Result<Arc<Discovery>, OidcError> {
        let mut slot = self.discovery.lock().await;
        if let Some((at, doc)) = slot.as_ref()
            && at.elapsed() < CACHE_TTL
        {
            return Ok(Arc::clone(doc));
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.config.issuer.trim_end_matches('/')
        );
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|error| OidcError::Provider(format!("取发现文档失败：{error}")))?;
        if !response.status().is_success() {
            return Err(OidcError::Provider(format!(
                "发现文档 {url} 回了 {}",
                response.status()
            )));
        }
        let doc: Discovery = response
            .json()
            .await
            .map_err(|error| OidcError::Provider(format!("发现文档不是合法 JSON：{error}")))?;
        // 文档里的 issuer 必须就是配置里的那个：否则这份文档不是这个签发方的。
        if doc.issuer.trim_end_matches('/') != self.config.issuer.trim_end_matches('/') {
            return Err(OidcError::Provider(format!(
                "发现文档的 issuer 是 {}，与配置的 {} 不一致",
                doc.issuer, self.config.issuer
            )));
        }
        for endpoint in [
            &doc.authorization_endpoint,
            &doc.token_endpoint,
            &doc.jwks_uri,
        ] {
            if !secure_or_loopback(endpoint) {
                return Err(OidcError::Provider(format!(
                    "端点 {endpoint} 不是 https 地址"
                )));
            }
        }
        let doc = Arc::new(doc);
        *slot = Some((Instant::now(), Arc::clone(&doc)));
        Ok(doc)
    }

    async fn jwks(&self, force: bool) -> Result<Arc<JwkSet>, OidcError> {
        let doc = self.discovery().await?;
        let mut slot = self.jwks.lock().await;
        if let Some((at, set)) = slot.as_ref()
            && !force
            && at.elapsed() < CACHE_TTL
        {
            return Ok(Arc::clone(set));
        }
        if force {
            let mut forced = self.jwks_forced_at.lock().await;
            if forced.is_some_and(|at| at.elapsed() < JWKS_REFETCH_COOLDOWN)
                && let Some((_, set)) = slot.as_ref()
            {
                return Ok(Arc::clone(set));
            }
            *forced = Some(Instant::now());
        }
        let response = self
            .http
            .get(&doc.jwks_uri)
            .send()
            .await
            .map_err(|error| OidcError::Provider(format!("取 JWKS 失败：{error}")))?;
        if !response.status().is_success() {
            return Err(OidcError::Provider(format!(
                "JWKS 回了 {}",
                response.status()
            )));
        }
        let set: JwkSet = response
            .json()
            .await
            .map_err(|error| OidcError::Provider(format!("JWKS 不是合法 JSON：{error}")))?;
        let set = Arc::new(set);
        *slot = Some((Instant::now(), Arc::clone(&set)));
        Ok(set)
    }

    /// 跳去提供方登录的地址（授权码 + PKCE S256）。
    pub async fn authorization_url(
        &self,
        state: &str,
        nonce: &str,
        code_challenge: &str,
    ) -> Result<String, OidcError> {
        let doc = self.discovery().await?;
        let mut url = reqwest::Url::parse(&doc.authorization_endpoint)
            .map_err(|error| OidcError::Provider(format!("授权端点不是合法 URL：{error}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", &self.config.callback_url())
            .append_pair("scope", "openid email profile")
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url.into())
    }

    /// 回调：拿授权码换令牌，验 ID token，（必要时）去 userinfo 补邮箱。
    ///
    /// `client_secret` 为 `None` 就按公开客户端换（只靠 PKCE）。
    pub async fn complete_login(
        &self,
        code: &str,
        code_verifier: &str,
        nonce: &str,
        client_secret: Option<&str>,
        now_unix: i64,
    ) -> Result<IdentityClaims, OidcError> {
        let doc = self.discovery().await?;
        let tokens = self
            .exchange(&doc, code, code_verifier, client_secret)
            .await?;
        let mut claims = self
            .verify_with_rotation(&tokens.id_token, nonce, now_unix)
            .await?;
        if claims.email.is_none()
            && !self.config.allowed_emails.is_empty()
            && let (Some(endpoint), Some(access)) = (
                doc.userinfo_endpoint.as_deref(),
                tokens.access_token.as_deref(),
            )
        {
            self.fill_email_from_userinfo(endpoint, access, &mut claims)
                .await;
        }
        Ok(claims)
    }

    async fn exchange(
        &self,
        doc: &Discovery,
        code: &str,
        code_verifier: &str,
        client_secret: Option<&str>,
    ) -> Result<TokenResponse, OidcError> {
        let callback = self.config.callback_url();
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", callback.as_str()),
            ("code_verifier", code_verifier),
        ];
        let mut request = self.http.post(&doc.token_endpoint);
        match client_secret {
            // 提供方只列了 basic 才用 basic；其余（含没列）按最通用的 client_secret_post。
            Some(secret) if prefers_basic(&doc.token_endpoint_auth_methods_supported) => {
                request = request.basic_auth(&self.config.client_id, Some(secret));
            }
            Some(secret) => {
                form.push(("client_id", self.config.client_id.as_str()));
                form.push(("client_secret", secret));
            }
            None => form.push(("client_id", self.config.client_id.as_str())),
        }
        let response = request
            .form(&form)
            .send()
            .await
            .map_err(|error| OidcError::Provider(format!("请求令牌端点失败：{error}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| OidcError::Provider(format!("读令牌端点响应失败：{error}")))?;
        if !status.is_success() {
            // 响应体里是提供方的错误码（`invalid_grant` …），不含我们发出去的秘密。
            return Err(OidcError::Exchange(format!(
                "令牌端点回了 {status}：{}",
                clip(&body)
            )));
        }
        #[derive(Deserialize)]
        struct Raw {
            id_token: Option<String>,
            access_token: Option<String>,
        }
        let raw: Raw = serde_json::from_str(&body)
            .map_err(|error| OidcError::Exchange(format!("令牌响应不是合法 JSON：{error}")))?;
        let id_token = raw
            .id_token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                OidcError::Exchange("令牌响应里没有 id_token（scope 要含 openid）".into())
            })?;
        Ok(TokenResponse {
            id_token,
            access_token: raw.access_token,
        })
    }

    async fn fill_email_from_userinfo(
        &self,
        endpoint: &str,
        access_token: &str,
        claims: &mut IdentityClaims,
    ) {
        #[derive(Deserialize)]
        struct UserInfo {
            sub: String,
            email: Option<String>,
            email_verified: Option<bool>,
        }
        let Ok(response) = self
            .http
            .get(endpoint)
            .bearer_auth(access_token)
            .send()
            .await
        else {
            return;
        };
        let Ok(info) = response.json::<UserInfo>().await else {
            return;
        };
        // 只认和 ID token 同一个 sub 的。
        if info.sub == claims.subject {
            claims.email = info.email;
            claims.email_verified = info.email_verified;
        }
    }

    /// 验签；`kid` 对不上就强制重取一次 JWKS 再试（密钥轮换）。
    async fn verify_with_rotation(
        &self,
        token: &str,
        nonce: &str,
        now_unix: i64,
    ) -> Result<IdentityClaims, OidcError> {
        let set = self.jwks(false).await?;
        match verify_id_token(token, &set, &self.config, nonce, now_unix) {
            Err(VerifyFailure::NoKey) => {
                let set = self.jwks(true).await?;
                verify_id_token(token, &set, &self.config, nonce, now_unix)
                    .map_err(|failure| OidcError::Token(failure.to_string()))
            }
            other => other.map_err(|failure| OidcError::Token(failure.to_string())),
        }
    }
}

fn prefers_basic(methods: &[String]) -> bool {
    !methods.is_empty()
        && methods.iter().any(|m| m == "client_secret_basic")
        && !methods.iter().any(|m| m == "client_secret_post")
}

fn secure_or_loopback(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    url.scheme() == "https"
        || (url.scheme() == "http"
            && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
}

/// 错误体最多带多长（提供方的错误 JSON 很短；防它回一整页 HTML）。
fn clip(body: &str) -> String {
    body.chars().take(300).collect()
}

// ---------------------------------------------------------------- ID token 校验

#[derive(Debug, thiserror::Error)]
enum VerifyFailure {
    /// JWKS 里找不到能验这个 token 的密钥（也许轮换了）。
    #[error("JWKS 里没有对得上的密钥")]
    NoKey,
    #[error("{0}")]
    Invalid(String),
}

fn invalid(message: impl Into<String>) -> VerifyFailure {
    VerifyFailure::Invalid(message.into())
}

fn b64(value: &str) -> Result<Vec<u8>, VerifyFailure> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.trim_end_matches('='))
        .map_err(|_| invalid("不是合法的 base64url"))
}

/// 验一个 ID token：签名、`iss`、`aud`、`exp`、`nbf`、`nonce`。
fn verify_id_token(
    token: &str,
    jwks: &JwkSet,
    config: &OidcConfig,
    nonce: &str,
    now_unix: i64,
) -> Result<IdentityClaims, VerifyFailure> {
    let mut parts = token.split('.');
    let (Some(header_b64), Some(payload_b64), Some(signature_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("不是三段式 JWT"));
    };

    #[derive(Deserialize)]
    struct Header {
        alg: String,
        #[serde(default)]
        kid: Option<String>,
    }
    let header: Header =
        serde_json::from_slice(&b64(header_b64)?).map_err(|_| invalid("头部不是合法 JSON"))?;
    let algorithm = Algorithm::parse(&header.alg).ok_or_else(|| {
        invalid(format!(
            "不支持的签名算法 {}（只认 RS256 / ES256 / EdDSA）",
            header.alg
        ))
    })?;

    let message = format!("{header_b64}.{payload_b64}");
    let signature = b64(signature_b64)?;
    let candidates: Vec<&Jwk> = jwks
        .keys
        .iter()
        .filter(|key| key.usage.as_deref().is_none_or(|usage| usage == "sig"))
        .filter(|key| key.alg.as_deref().is_none_or(|alg| alg == header.alg))
        .filter(|key| algorithm.matches_key(key))
        .filter(|key| match (&header.kid, &key.kid) {
            (Some(want), Some(have)) => want == have,
            // token 没带 kid：只在 JWKS 里恰好一把同类型的钥匙时才认（下面按数量判）。
            (None, _) => true,
            // token 指名了 kid，而这把钥匙没有 kid：不认。
            (Some(_), None) => false,
        })
        .collect();
    if candidates.is_empty() || (header.kid.is_none() && candidates.len() > 1) {
        return Err(VerifyFailure::NoKey);
    }
    let verified = candidates.iter().any(|key| {
        algorithm
            .verify(key, message.as_bytes(), &signature)
            .is_ok()
    });
    if !verified {
        return Err(invalid("签名不对"));
    }

    // 签名过了才信载荷。
    #[derive(Deserialize)]
    struct Payload {
        iss: Option<String>,
        sub: Option<String>,
        aud: Option<Audience>,
        exp: Option<i64>,
        nbf: Option<i64>,
        nonce: Option<String>,
        azp: Option<String>,
        email: Option<String>,
        email_verified: Option<EmailVerified>,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Audience {
        One(String),
        Many(Vec<String>),
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum EmailVerified {
        Bool(bool),
        Text(String),
    }
    let payload: Payload =
        serde_json::from_slice(&b64(payload_b64)?).map_err(|_| invalid("载荷不是合法 JSON"))?;

    let issuer = payload.iss.as_deref().unwrap_or_default();
    if issuer.trim_end_matches('/') != config.issuer.trim_end_matches('/') {
        return Err(invalid("iss 与配置的签发方不一致"));
    }
    let audiences: Vec<&str> = match &payload.aud {
        Some(Audience::One(one)) => vec![one.as_str()],
        Some(Audience::Many(many)) => many.iter().map(String::as_str).collect(),
        None => vec![],
    };
    if !audiences.contains(&config.client_id.as_str()) {
        return Err(invalid("aud 不含本客户端"));
    }
    if audiences.len() > 1
        && payload
            .azp
            .as_deref()
            .is_some_and(|azp| azp != config.client_id)
    {
        return Err(invalid("azp 不是本客户端"));
    }
    let Some(exp) = payload.exp else {
        return Err(invalid("没有 exp"));
    };
    if exp + LEEWAY_SECS <= now_unix {
        return Err(invalid("已过期"));
    }
    if payload.nbf.is_some_and(|nbf| nbf - LEEWAY_SECS > now_unix) {
        return Err(invalid("还没到生效时间"));
    }
    if payload.nonce.as_deref() != Some(nonce) {
        return Err(invalid("nonce 不符"));
    }
    let Some(subject) = payload.sub.filter(|sub| !sub.is_empty()) else {
        return Err(invalid("没有 sub"));
    };
    Ok(IdentityClaims {
        subject,
        email: payload.email.filter(|email| !email.is_empty()),
        email_verified: payload.email_verified.map(|value| match value {
            EmailVerified::Bool(flag) => flag,
            // 个别提供方把它写成字符串。
            EmailVerified::Text(text) => text.eq_ignore_ascii_case("true"),
        }),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Algorithm {
    Rs256,
    Es256,
    EdDsa,
}

impl Algorithm {
    fn parse(alg: &str) -> Option<Algorithm> {
        match alg {
            "RS256" => Some(Algorithm::Rs256),
            "ES256" => Some(Algorithm::Es256),
            "EdDSA" => Some(Algorithm::EdDsa),
            _ => None,
        }
    }

    /// 这把钥匙的类型是不是这个算法用的。
    fn matches_key(self, key: &Jwk) -> bool {
        match self {
            Algorithm::Rs256 => key.kty == "RSA",
            Algorithm::Es256 => key.kty == "EC" && key.crv.as_deref() == Some("P-256"),
            Algorithm::EdDsa => key.kty == "OKP" && key.crv.as_deref() == Some("Ed25519"),
        }
    }

    fn verify(self, key: &Jwk, message: &[u8], signature: &[u8]) -> Result<(), VerifyFailure> {
        let field = |value: &Option<String>, name: &str| -> Result<Vec<u8>, VerifyFailure> {
            b64(value
                .as_deref()
                .ok_or_else(|| invalid(format!("JWK 缺 {name}")))?)
        };
        let rejected = |_| invalid("签名不对");
        match self {
            Algorithm::Rs256 => {
                let components = ring::signature::RsaPublicKeyComponents {
                    n: field(&key.n, "n")?,
                    e: field(&key.e, "e")?,
                };
                components
                    .verify(
                        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                        message,
                        signature,
                    )
                    .map_err(rejected)
            }
            Algorithm::Es256 => {
                let (x, y) = (field(&key.x, "x")?, field(&key.y, "y")?);
                if x.len() != 32 || y.len() != 32 {
                    return Err(invalid("P-256 的 x / y 要各 32 字节"));
                }
                // 未压缩点：0x04 || x || y。
                let mut point = Vec::with_capacity(65);
                point.push(0x04);
                point.extend_from_slice(&x);
                point.extend_from_slice(&y);
                ring::signature::UnparsedPublicKey::new(
                    &ring::signature::ECDSA_P256_SHA256_FIXED,
                    point,
                )
                .verify(message, signature)
                .map_err(rejected)
            }
            Algorithm::EdDsa => {
                let x = field(&key.x, "x")?;
                if x.len() != 32 {
                    return Err(invalid("Ed25519 的 x 要 32 字节"));
                }
                ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, x)
                    .verify(message, signature)
                    .map_err(rejected)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OidcConfig {
        OidcConfig {
            issuer: "https://id.example.com".into(),
            client_id: "komo".into(),
            client_secret_env: "KOMO_OIDC_CLIENT_SECRET".into(),
            allowed_subjects: vec!["u-1".into()],
            allowed_emails: vec!["Me@Example.com".into()],
            public_url: "https://komo.example.com".into(),
        }
    }

    #[test]
    fn the_allow_list_matches_subject_or_email() {
        let cfg = config();
        let by_sub = IdentityClaims {
            subject: "u-1".into(),
            email: None,
            email_verified: None,
        };
        assert!(by_sub.allowed_by(&cfg));
        let by_email = IdentityClaims {
            subject: "other".into(),
            email: Some("me@example.com".into()),
            email_verified: Some(true),
        };
        assert!(by_email.allowed_by(&cfg));
        let unverified = IdentityClaims {
            email_verified: Some(false),
            ..by_email.clone()
        };
        assert!(!unverified.allowed_by(&cfg));
        let stranger = IdentityClaims {
            subject: "x".into(),
            email: Some("x@example.com".into()),
            email_verified: None,
        };
        assert!(!stranger.allowed_by(&cfg));
    }

    #[test]
    fn a_token_that_is_not_three_segments_is_refused() {
        let set = JwkSet { keys: vec![] };
        assert!(matches!(
            verify_id_token("a.b", &set, &config(), "n", 0),
            Err(VerifyFailure::Invalid(_))
        ));
    }

    #[test]
    fn hmac_and_none_algorithms_are_refused() {
        let set = JwkSet { keys: vec![] };
        for alg in ["none", "HS256"] {
            let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(format!(r#"{{"alg":"{alg}"}}"#));
            let token = format!("{header}.e30.c2ln");
            assert!(
                matches!(
                    verify_id_token(&token, &set, &config(), "n", 0),
                    Err(VerifyFailure::Invalid(_))
                ),
                "{alg}"
            );
        }
    }

    #[test]
    fn basic_auth_is_only_chosen_when_post_is_not_offered() {
        assert!(prefers_basic(&["client_secret_basic".into()]));
        assert!(!prefers_basic(&[
            "client_secret_basic".into(),
            "client_secret_post".into()
        ]));
        assert!(!prefers_basic(&[]));
    }

    // ------------------------------------------------------------ ID token 验签与声明校验（本地签的 token）

    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair};
    use serde_json::{Value, json};

    const NOW: i64 = 1_800_000_000;

    struct Es256 {
        pair: EcdsaKeyPair,
    }

    impl Es256 {
        fn new() -> Es256 {
            let rng = SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            Es256 {
                pair: EcdsaKeyPair::from_pkcs8(
                    &ECDSA_P256_SHA256_FIXED_SIGNING,
                    pkcs8.as_ref(),
                    &rng,
                )
                .unwrap(),
            }
        }

        fn jwk(&self, kid: Option<&str>) -> Jwk {
            let point = self.pair.public_key().as_ref();
            Jwk {
                kty: "EC".into(),
                kid: kid.map(str::to_string),
                alg: None,
                usage: None,
                n: None,
                e: None,
                crv: Some("P-256".into()),
                x: Some(B64.encode(&point[1..33])),
                y: Some(B64.encode(&point[33..65])),
            }
        }

        fn sign(&self, kid: Option<&str>, claims: &Value) -> String {
            let mut header = json!({ "alg": "ES256", "typ": "JWT" });
            if let Some(kid) = kid {
                header["kid"] = json!(kid);
            }
            let input = format!(
                "{}.{}",
                B64.encode(header.to_string()),
                B64.encode(claims.to_string())
            );
            let signature = self
                .pair
                .sign(&SystemRandom::new(), input.as_bytes())
                .unwrap();
            format!("{input}.{}", B64.encode(signature.as_ref()))
        }
    }

    fn claims() -> Value {
        json!({ "iss": "https://id.example.com", "sub": "u-1", "aud": "komo",
                "exp": NOW + 600, "nonce": "n-1", "email": "me@example.com", "email_verified": true })
    }

    fn verify(token: &str, keys: Vec<Jwk>) -> Result<IdentityClaims, VerifyFailure> {
        verify_id_token(token, &JwkSet { keys }, &config(), "n-1", NOW)
    }

    fn rejected(result: Result<IdentityClaims, VerifyFailure>, needle: &str) {
        match result {
            Err(VerifyFailure::Invalid(message)) => {
                assert!(message.contains(needle), "{message} 里该有 {needle}")
            }
            other => panic!("该被拒（{needle}）：{other:?}"),
        }
    }

    #[test]
    fn a_valid_es256_token_yields_the_claims() {
        let key = Es256::new();
        let token = key.sign(Some("k1"), &claims());
        let got = verify(&token, vec![key.jwk(Some("k1"))]).unwrap();
        assert_eq!(got.subject, "u-1");
        assert_eq!(got.email.as_deref(), Some("me@example.com"));
        assert_eq!(got.email_verified, Some(true));
        // `email_verified` 写成字符串的提供方也认；`aud` 是数组、含本客户端也行。
        let mut odd = claims();
        odd["email_verified"] = json!("true");
        odd["aud"] = json!(["other", "komo"]);
        odd["azp"] = json!("komo");
        assert_eq!(
            verify(&key.sign(Some("k1"), &odd), vec![key.jwk(Some("k1"))])
                .unwrap()
                .email_verified,
            Some(true)
        );
        odd["email_verified"] = json!("false");
        assert_eq!(
            verify(&key.sign(Some("k1"), &odd), vec![key.jwk(Some("k1"))])
                .unwrap()
                .email_verified,
            Some(false)
        );
    }

    #[test]
    fn a_valid_eddsa_token_is_verified_too() {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let header = B64.encode(json!({ "alg": "EdDSA", "kid": "e1" }).to_string());
        let input = format!("{header}.{}", B64.encode(claims().to_string()));
        let token = format!(
            "{input}.{}",
            B64.encode(pair.sign(input.as_bytes()).as_ref())
        );
        let jwk = Jwk {
            kty: "OKP".into(),
            kid: Some("e1".into()),
            alg: Some("EdDSA".into()),
            usage: Some("sig".into()),
            n: None,
            e: None,
            crv: Some("Ed25519".into()),
            x: Some(B64.encode(pair.public_key().as_ref())),
            y: None,
        };
        assert_eq!(verify(&token, vec![jwk.clone()]).unwrap().subject, "u-1");
        let mut tampered = token.clone();
        tampered.replace_range(token.len() - 3.., "AAA");
        rejected(verify(&tampered, vec![jwk]), "签名");
    }

    #[test]
    fn claims_are_checked_after_the_signature_with_a_sixty_second_leeway() {
        let key = Es256::new();
        let jwks = || vec![key.jwk(Some("k1"))];
        let with = |edit: &dyn Fn(&mut Value)| {
            let mut value = claims();
            edit(&mut value);
            verify(&key.sign(Some("k1"), &value), jwks())
        };
        rejected(
            with(&|c| c["iss"] = json!("https://evil.example.com")),
            "iss",
        );
        rejected(with(&|c| c["aud"] = json!("someone-else")), "aud");
        // 多个 aud 而没写 azp：只要含本客户端就认（azp 缺席不算错）。
        assert!(with(&|c| c["aud"] = json!(["komo", "other"])).is_ok());
        rejected(
            with(&|c| {
                c["aud"] = json!(["komo", "other"]);
                c["azp"] = json!("other");
            }),
            "azp",
        );
        rejected(with(&|c| c["nonce"] = json!("other")), "nonce");
        rejected(
            with(&|c| {
                c.as_object_mut().unwrap().remove("nonce");
            }),
            "nonce",
        );
        rejected(
            with(&|c| {
                c.as_object_mut().unwrap().remove("exp");
            }),
            "exp",
        );
        rejected(with(&|c| c["sub"] = json!("")), "sub");
        rejected(
            with(&|c| {
                c.as_object_mut().unwrap().remove("sub");
            }),
            "sub",
        );
        // exp 的边界：过期 60 秒内还认，刚好 60 秒就不认了。
        assert!(with(&|c| c["exp"] = json!(NOW - 59)).is_ok());
        rejected(with(&|c| c["exp"] = json!(NOW - 60)), "过期");
        // nbf：未来 60 秒内还认，再晚就不认。
        assert!(with(&|c| c["nbf"] = json!(NOW + 60)).is_ok());
        rejected(with(&|c| c["nbf"] = json!(NOW + 61)), "生效");
        // iss 尾斜杠不影响。
        assert!(with(&|c| c["iss"] = json!("https://id.example.com/")).is_ok());
    }

    #[test]
    fn the_key_is_picked_by_kid_and_a_wrong_or_unknown_key_is_refused() {
        let (a, b) = (Es256::new(), Es256::new());
        let token = a.sign(Some("ka"), &claims());
        // 两把钥匙，按 kid 取对的那把。
        assert!(verify(&token, vec![b.jwk(Some("kb")), a.jwk(Some("ka"))]).is_ok());
        // token 指名的 kid 在 JWKS 里没有：NoKey（调用方据此强制重取一次）。
        assert!(matches!(
            verify(&token, vec![b.jwk(Some("kb"))]),
            Err(VerifyFailure::NoKey)
        ));
        // JWKS 里的钥匙没有 kid、token 却指了 kid：不认。
        assert!(matches!(
            verify(&token, vec![a.jwk(None)]),
            Err(VerifyFailure::NoKey)
        ));
        // 同 kid 但是另一把钥匙：签名不对。
        rejected(verify(&token, vec![b.jwk(Some("ka"))]), "签名");
        // token 没带 kid：只有一把同类钥匙才试；两把以上宁可当没有。
        let bare = a.sign(None, &claims());
        assert!(verify(&bare, vec![a.jwk(Some("ka"))]).is_ok());
        assert!(matches!(
            verify(&bare, vec![a.jwk(Some("ka")), b.jwk(Some("kb"))]),
            Err(VerifyFailure::NoKey)
        ));
        // 钥匙类型与算法对不上（RSA 钥匙验 ES256）：当没有这把钥匙。
        let rsa = Jwk {
            kty: "RSA".into(),
            crv: None,
            x: None,
            y: None,
            n: Some("AQAB".into()),
            e: Some("AQAB".into()),
            ..a.jwk(Some("ka"))
        };
        assert!(matches!(
            verify(&token, vec![rsa]),
            Err(VerifyFailure::NoKey)
        ));
        // 用途不是签名的钥匙不用。
        let mut enc = a.jwk(Some("ka"));
        enc.usage = Some("enc".into());
        assert!(matches!(
            verify(&token, vec![enc]),
            Err(VerifyFailure::NoKey)
        ));
        // JWK 里 alg 与 token 不一致的钥匙不用。
        let mut other_alg = a.jwk(Some("ka"));
        other_alg.alg = Some("RS256".into());
        assert!(matches!(
            verify(&token, vec![other_alg]),
            Err(VerifyFailure::NoKey)
        ));
    }

    #[test]
    fn a_short_ec_point_is_an_invalid_key_not_a_panic() {
        let key = Es256::new();
        let token = key.sign(Some("k1"), &claims());
        let mut jwk = key.jwk(Some("k1"));
        jwk.x = Some(B64.encode([1u8; 31]));
        rejected(verify(&token, vec![jwk]), "签名");
    }

    #[test]
    fn the_http_discovery_endpoints_must_be_https_or_loopback() {
        for ok in [
            "https://id.example.com/authorize",
            "http://127.0.0.1:9000/token",
            "http://localhost/x",
            "http://[::1]:1/x",
        ] {
            assert!(secure_or_loopback(ok), "{ok}");
        }
        for bad in [
            "http://id.example.com/authorize",
            "ftp://127.0.0.1/x",
            "not a url",
            "http://127.0.0.1.evil.com/x",
        ] {
            assert!(!secure_or_loopback(bad), "{bad}");
        }
        assert_eq!(clip(&"x".repeat(1000)).chars().count(), 300);
    }
}
