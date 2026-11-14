//! 登录与鉴权（`docs/redesign.md` §2.9、`docs/clients.md` D6）。
//!
//! 三种凭据，**所有 `/v1/**` 只认这三种**，`/healthz` 与 `/auth/*` 不要求登录：
//!
//! - **本机令牌**（`Authorization: Bearer <token>`，令牌在 `runtime/gateway.json`，每次启动重生成）：
//!   机器上的命令行用，不经 OIDC——这也是身份提供方不可用时的后路；
//! - **设备令牌**（同样是 Bearer，`komodev_…`）：桌面端 / `komo login` 经「跳系统浏览器登录再跳回」
//!   换来，长期有效、可列出可吊销；
//! - **网页会话 cookie**（`komo_session`，HttpOnly + SameSite=Lax，公开地址是 https 时带 Secure）：
//!   OIDC 登录成功后签发。带 cookie 的**非 GET / HEAD** 请求另外核对 `Origin`（同 `Host` 或等于公开
//!   地址的源），否则 403。
//!
//! 设备令牌与会话 cookie 都落在 `aux.db` 的 `auth_sessions`，**只存令牌的 SHA-256**，重启后仍有效；
//! 网页会话 30 天滑动续期（用一次、距上次记录超过 10 分钟就续，并重发 cookie 把浏览器那一侧也续上）。
//! 登录过程中的临时状态（`state` / `nonce` / PKCE verifier、设备登录的一次性授权码）只在内存里，
//! 短 TTL、用一次作废，重启丢了就重新登录。
//!
//! 失败一律 [`ErrorCode::Unauthorized`]，不区分"没带"与"带错了"。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::Response;
use komo_config::ConfigHolder;
use komo_kernel::pkce::{random_token, s256_challenge};
use komo_kernel::protocol::config::OidcConfig;
use komo_kernel::protocol::http::{
    AuthCredentialKind, AuthSessionKind, AuthSessionView, ErrorCode,
};
use komo_kernel::traits::Clock;
use komo_kernel::types::digest::ContentHash;
use komo_store::{AuthKind, AuthSessionRecord, Db, NewAuthSession, TursoAuthRepo};
use time::{Duration, OffsetDateTime};

use crate::http::error::ApiFailure;
use crate::oidc::{IdentityClaims, OidcClient, OidcError};
use crate::web::{cookie_value, same_origin};

/// 网页会话 cookie 的名字。
pub const SESSION_COOKIE: &str = "komo_session";
/// 登录过程中把浏览器与 `state` 绑在一起的短命 cookie（防登录 CSRF：别人拿自己的 code + state 做成
/// 链接骗你点，没有这个 cookie 回调就不认）。只在 `/auth` 下发送。
pub const LOGIN_COOKIE: &str = "komo_oidc";
/// 设备令牌的前缀，给人认（日志、泄露扫描）；Gateway 只靠哈希查。
pub const DEVICE_PREFIX: &str = "komodev_";
/// 网页会话的滑动有效期。
pub const WEB_TTL: Duration = Duration::days(30);
/// 距上次记录超过这么久才写库续期（别每个请求都写）。
const TOUCH_AFTER: Duration = Duration::minutes(10);
/// 一次登录从跳出去到回调最多多久。
const LOGIN_TTL: Duration = Duration::minutes(10);
/// 设备登录的一次性授权码多久失效。
const CODE_TTL: Duration = Duration::minutes(2);
/// 内存里同时挂着的待完成登录 / 授权码上限：满了先丢最早的。
const MAX_PENDING: usize = 256;

/// 这个请求是用哪种凭据过的鉴权（放进请求扩展里，handler 要知道「当前是哪一条」时读它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// 本机令牌；空令牌的 Gateway（测试或显式关掉鉴权）也算这一档。
    Local,
    Device {
        id: String,
    },
    Web {
        id: String,
    },
}

impl Credential {
    pub fn kind(&self) -> AuthCredentialKind {
        match self {
            Credential::Local => AuthCredentialKind::Local,
            Credential::Device { .. } => AuthCredentialKind::Device,
            Credential::Web { .. } => AuthCredentialKind::Web,
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        match self {
            Credential::Local => None,
            Credential::Device { id } | Credential::Web { id } => Some(id),
        }
    }
}

/// 认出来的人与凭据。
#[derive(Debug, Clone)]
pub struct Authed {
    pub credential: Credential,
    pub subject: Option<String>,
    pub email: Option<String>,
    /// 网页会话刚续了期：要把这个 cookie 值用新的 `Max-Age` 重发一遍。
    pub reissue_cookie: Option<String>,
}

/// 登录完成之后去哪。
#[derive(Debug, Clone)]
pub enum Purpose {
    /// 网页：种 cookie，跳回 `next`（本站内的相对路径）。
    Web { next: String },
    /// 桌面端 / CLI：带一次性授权码跳回客户端给的 `redirect_uri`。
    Device(DeviceRequest),
}

/// 设备登录请求（已经过白名单校验）。
#[derive(Debug, Clone)]
pub struct DeviceRequest {
    /// 客户端的 PKCE challenge（S256）。
    pub challenge: String,
    pub redirect_uri: String,
    /// 客户端自己的 `state`，原样带回去。
    pub state: Option<String>,
    /// 设备名（设置页清单里显示）。
    pub name: String,
}

struct PendingLogin {
    nonce: String,
    verifier: String,
    purpose: Purpose,
    expires: OffsetDateTime,
}

struct DeviceCode {
    request: DeviceRequest,
    subject: String,
    email: Option<String>,
    expires: OffsetDateTime,
}

/// 登录这一步失败的几种，由 handler 翻成页面或错误体。
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("这台 Gateway 没有配置登录（config.toml 的 [auth.oidc]）")]
    NotConfigured,
    #[error("{0}")]
    BadRequest(String),
    /// state 对不上、过期、cookie 没带：重新发起登录。
    #[error("登录已失效或不是在这个浏览器里发起的，请重新登录")]
    BadState,
    /// 登录成功但账号不在允许名单里。
    #[error("这个账号不在允许登录的名单里")]
    Denied,
    #[error("{0}")]
    Provider(String),
    #[error("{0}")]
    Internal(String),
}

impl From<OidcError> for AuthError {
    fn from(error: OidcError) -> Self {
        // 细节进日志，不给浏览器（里面可能有提供方的内部信息）。
        tracing::warn!(%error, "OIDC 登录失败");
        match error {
            OidcError::Provider(_) => AuthError::Provider("身份提供方暂时不可用".into()),
            OidcError::Exchange(_) | OidcError::Token(_) => {
                AuthError::BadRequest("身份提供方没有通过这次登录".into())
            }
        }
    }
}

impl From<komo_kernel::traits::StoreError> for AuthError {
    fn from(error: komo_kernel::traits::StoreError) -> Self {
        AuthError::Internal(error.to_string())
    }
}

/// 登录状态服务：持久的会话 / 设备令牌 + 内存里的登录临时状态 + OIDC 客户端。
pub struct AuthService {
    repo: TursoAuthRepo,
    config: Arc<ConfigHolder>,
    clock: Arc<dyn Clock>,
    /// 本机令牌；空 = 没设（测试）。
    local_token: String,
    /// 启动时按 `[auth.oidc]` 造；没配就是 `None`（改了要重启，与启动期键一致）。
    oidc: Option<OidcClient>,
    pending: Mutex<HashMap<String, PendingLogin>>,
    codes: Mutex<HashMap<String, DeviceCode>>,
}

impl std::fmt::Debug for AuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthService")
            .field("oidc", &self.oidc.is_some())
            .finish_non_exhaustive()
    }
}

impl AuthService {
    pub fn new(
        db: Db,
        config: Arc<ConfigHolder>,
        clock: Arc<dyn Clock>,
        local_token: String,
    ) -> AuthService {
        let oidc = config
            .current()
            .start_only
            .oidc
            .clone()
            .map(OidcClient::new);
        AuthService {
            repo: TursoAuthRepo::new(db),
            config,
            clock,
            local_token,
            oidc,
            pending: Mutex::new(HashMap::new()),
            codes: Mutex::new(HashMap::new()),
        }
    }

    pub fn oidc_config(&self) -> Option<&OidcConfig> {
        self.oidc.as_ref().map(OidcClient::config)
    }

    pub fn oidc_configured(&self) -> bool {
        self.oidc.is_some()
    }

    /// 公开地址是 https 才给 cookie 加 `Secure`（http 的本机开发下加了浏览器会丢）。
    pub fn secure_cookies(&self) -> bool {
        self.oidc_config()
            .is_some_and(|config| config.public_url.starts_with("https://"))
    }

    /// 公开地址的源（`https://komo.example.com`），`Origin` 核对用。
    pub fn public_origin(&self) -> Option<String> {
        self.oidc_config()
            .map(|config| config.public_url.trim_end_matches('/').to_string())
    }

    /// `Set-Cookie` 的值：会话 cookie。
    pub fn session_cookie(&self, token: &str) -> String {
        format!(
            "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}{}",
            WEB_TTL.whole_seconds(),
            self.secure_attr()
        )
    }

    pub fn clear_session_cookie(&self) -> String {
        format!(
            "{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0{}",
            self.secure_attr()
        )
    }

    pub fn login_cookie(&self, state: &str) -> String {
        format!(
            "{LOGIN_COOKIE}={state}; HttpOnly; SameSite=Lax; Path=/auth; Max-Age={}{}",
            LOGIN_TTL.whole_seconds(),
            self.secure_attr()
        )
    }

    pub fn clear_login_cookie(&self) -> String {
        format!(
            "{LOGIN_COOKIE}=; HttpOnly; SameSite=Lax; Path=/auth; Max-Age=0{}",
            self.secure_attr()
        )
    }

    fn secure_attr(&self) -> &'static str {
        if self.secure_cookies() {
            "; Secure"
        } else {
            ""
        }
    }

    fn now(&self) -> OffsetDateTime {
        self.clock.now()
    }

    // ------------------------------------------------------------ 鉴权

    /// 从请求头认人：本机令牌 → 设备令牌 → 会话 cookie。都不是就 `Ok(None)`。
    ///
    /// 带 cookie 的非 GET / HEAD 请求，`Origin` 不对是 `Err(Denied)`。库读写失败是 `Err(Internal)`
    /// ——不能把它当成"没登录"，否则一次库抖动就把人踢回登录页。
    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
        method: &Method,
    ) -> Result<Option<Authed>, ApiFailure> {
        if self.local_token.is_empty() {
            return Ok(Some(local()));
        }
        let now = self.now();
        if let Some(token) = bearer(headers) {
            if constant_time_eq(token.as_bytes(), self.local_token.as_bytes()) {
                return Ok(Some(local()));
            }
            let found = self
                .repo
                .find(AuthKind::Device, &hash(&token), now)
                .await
                .map_err(internal)?;
            if let Some(record) = found {
                self.touch(&record, now, None).await;
                return Ok(Some(Authed {
                    credential: Credential::Device {
                        id: record.id.clone(),
                    },
                    subject: Some(record.subject),
                    email: record.email,
                    reissue_cookie: None,
                }));
            }
        }
        if let Some(cookie) = cookie_value(headers, SESSION_COOKIE) {
            let found = self
                .repo
                .find(AuthKind::Web, &hash(&cookie), now)
                .await
                .map_err(internal)?;
            if let Some(record) = found {
                if !matches!(*method, Method::GET | Method::HEAD)
                    && !same_origin(headers, self.public_origin().as_deref())
                {
                    return Err(ApiFailure::new(
                        ErrorCode::Denied,
                        "用 cookie 发起的修改请求要求 Origin 与 Host（或公开地址）同源",
                    ));
                }
                let renewed = self
                    .touch(&record, now, Some(now + WEB_TTL))
                    .await
                    .then(|| cookie);
                return Ok(Some(Authed {
                    credential: Credential::Web {
                        id: record.id.clone(),
                    },
                    subject: Some(record.subject),
                    email: record.email,
                    reissue_cookie: renewed,
                }));
            }
        }
        Ok(None)
    }

    /// 超过 [`TOUCH_AFTER`] 才写库；返回是否写了。写失败不挡请求（登录状态本身是读到了的）。
    async fn touch(
        &self,
        record: &AuthSessionRecord,
        now: OffsetDateTime,
        renew_to: Option<OffsetDateTime>,
    ) -> bool {
        if now - record.last_used_at < TOUCH_AFTER {
            return false;
        }
        match self.repo.touch(&record.id, now, renew_to).await {
            Ok(()) => renew_to.is_some(),
            Err(error) => {
                tracing::warn!(%error, "登录状态续期没写成");
                false
            }
        }
    }

    // ------------------------------------------------------------ OIDC 登录

    /// 发起登录：记下 `state` / `nonce` / verifier，交回要跳去的提供方地址与 `state`。
    pub async fn begin(&self, purpose: Purpose) -> Result<(String, String), AuthError> {
        let oidc = self.oidc.as_ref().ok_or(AuthError::NotConfigured)?;
        let state = random_token();
        let nonce = random_token();
        let verifier = random_token();
        let url = oidc
            .authorization_url(&state, &nonce, &s256_challenge(&verifier))
            .await?;
        let now = self.now();
        insert_bounded(
            &self.pending,
            state.clone(),
            PendingLogin {
                nonce,
                verifier,
                purpose,
                expires: now + LOGIN_TTL,
            },
            now,
            |entry| entry.expires,
        );
        Ok((url, state))
    }

    /// 回调：核对 state（含浏览器 cookie 绑定）、换令牌、验 ID token、核对允许名单。
    pub async fn finish(
        &self,
        state: &str,
        cookie_state: Option<&str>,
        code: &str,
    ) -> Result<(IdentityClaims, Purpose), AuthError> {
        let oidc = self.oidc.as_ref().ok_or(AuthError::NotConfigured)?;
        let now = self.now();
        // **先取走**：不管后面成不成，一个 state 只能用一次。
        let pending = self.pending.lock().expect("登录表").remove(state);
        let Some(pending) = pending.filter(|p| p.expires > now) else {
            return Err(AuthError::BadState);
        };
        if cookie_state != Some(state) {
            return Err(AuthError::BadState);
        }
        let secret = self
            .config
            .secrets()
            .get(&oidc.config().client_secret_env)
            .map(str::trim)
            .filter(|secret| !secret.is_empty())
            .map(str::to_string);
        let claims = oidc
            .complete_login(
                code,
                &pending.verifier,
                &pending.nonce,
                secret.as_deref(),
                now.unix_timestamp(),
            )
            .await?;
        if !claims.allowed_by(oidc.config()) {
            tracing::warn!(subject = %claims.subject, "不在允许名单里的账号尝试登录，已拒绝");
            return Err(AuthError::Denied);
        }
        Ok((claims, pending.purpose))
    }

    /// 签发网页会话，交回要写进 cookie 的令牌。
    pub async fn issue_web(
        &self,
        subject: &str,
        email: Option<&str>,
        user_agent: &str,
    ) -> Result<String, AuthError> {
        let now = self.now();
        let token = random_token();
        self.repo
            .create(NewAuthSession {
                kind: AuthKind::Web,
                token_hash: hash(&token),
                subject: subject.to_string(),
                email: email.map(str::to_string),
                label: browser_label(user_agent),
                now,
                expires_at: Some(now + WEB_TTL),
            })
            .await?;
        // 顺手清掉过期的行（登录是写库的少数时机；清不掉不影响这次登录）。
        if let Err(error) = self.repo.purge_expired(now).await {
            tracing::warn!(%error, "清理过期的登录状态没做成");
        }
        Ok(token)
    }

    /// 已登录的浏览器（有效的会话 cookie）：拿它的身份。给 `/auth/device/authorize` 跳过提供方用。
    pub async fn web_identity(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<(String, Option<String>)>, AuthError> {
        let Some(cookie) = cookie_value(headers, SESSION_COOKIE) else {
            return Ok(None);
        };
        let found = self
            .repo
            .find(AuthKind::Web, &hash(&cookie), self.now())
            .await?;
        Ok(found.map(|record| (record.subject, record.email)))
    }

    /// 退出：吊销这个 cookie 对应的会话（没有或已失效也算成功）。
    pub async fn logout(&self, headers: &HeaderMap) -> Result<(), AuthError> {
        if let Some(cookie) = cookie_value(headers, SESSION_COOKIE) {
            self.repo
                .revoke_by_hash(AuthKind::Web, &hash(&cookie))
                .await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ 设备登录

    /// 登录成功后给设备发一次性授权码（两分钟、用一次）。
    pub fn issue_code(&self, request: DeviceRequest, subject: &str, email: Option<&str>) -> String {
        let now = self.now();
        let code = random_token();
        insert_bounded(
            &self.codes,
            code.clone(),
            DeviceCode {
                request,
                subject: subject.to_string(),
                email: email.map(str::to_string),
                expires: now + CODE_TTL,
            },
            now,
            |entry| entry.expires,
        );
        code
    }

    /// 授权码 + PKCE verifier 换长期设备令牌。**码用一次就作废**，不管成不成。
    pub async fn redeem_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<(String, AuthSessionRecord), AuthError> {
        let now = self.now();
        let entry = self.codes.lock().expect("授权码表").remove(code);
        let Some(entry) = entry.filter(|entry| entry.expires > now) else {
            return Err(AuthError::BadRequest(
                "授权码无效或已过期，请重新登录".into(),
            ));
        };
        if entry.request.redirect_uri != redirect_uri
            || !constant_time_eq(
                s256_challenge(verifier).as_bytes(),
                entry.request.challenge.as_bytes(),
            )
        {
            return Err(AuthError::BadRequest(
                "授权码与 code_verifier / redirect_uri 对不上".into(),
            ));
        }
        let token = format!("{DEVICE_PREFIX}{}", random_token());
        let record = self
            .repo
            .create(NewAuthSession {
                kind: AuthKind::Device,
                token_hash: hash(&token),
                subject: entry.subject,
                email: entry.email,
                label: entry.request.name,
                now,
                expires_at: None,
            })
            .await?;
        Ok((token, record))
    }

    // ------------------------------------------------------------ 清单与吊销

    pub async fn list(&self, current: Option<&str>) -> Result<Vec<AuthSessionView>, AuthError> {
        let records = self.repo.list(self.now()).await?;
        Ok(records
            .into_iter()
            .map(|record| AuthSessionView {
                current: current == Some(record.id.as_str()),
                id: record.id,
                kind: match record.kind {
                    AuthKind::Web => AuthSessionKind::Web,
                    AuthKind::Device => AuthSessionKind::Device,
                },
                label: record.label,
                subject: record.subject,
                email: record.email,
                created_at: record.created_at,
                last_used_at: record.last_used_at,
                expires_at: record.expires_at,
            })
            .collect())
    }

    pub async fn revoke(&self, id: &str) -> Result<bool, AuthError> {
        Ok(self.repo.revoke(id).await?)
    }

    /// 清掉已过期的行（后台定时调）。
    pub async fn purge_expired(&self) -> Result<usize, AuthError> {
        Ok(self.repo.purge_expired(self.now()).await?)
    }
}

fn local() -> Authed {
    Authed {
        credential: Credential::Local,
        subject: None,
        email: None,
        reissue_cookie: None,
    }
}

fn internal(error: komo_kernel::traits::StoreError) -> ApiFailure {
    ApiFailure::new(ErrorCode::Internal, format!("读登录状态失败：{error}"))
}

/// 令牌 → 库里存的哈希。
fn hash(token: &str) -> String {
    ContentHash::of_str(token).as_str().to_string()
}

/// 插入并顺手清掉过期的；还是满了就丢最早到期的那个。
fn insert_bounded<V>(
    table: &Mutex<HashMap<String, V>>,
    key: String,
    value: V,
    now: OffsetDateTime,
    expires: impl Fn(&V) -> OffsetDateTime,
) {
    let mut table = table.lock().expect("内存表");
    table.retain(|_, entry| expires(entry) > now);
    while table.len() >= MAX_PENDING {
        let Some(oldest) = table
            .iter()
            .min_by_key(|(_, entry)| expires(entry))
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        table.remove(&oldest);
    }
    table.insert(key, value);
}

/// 不因第一个不同的字节就提前返回（令牌比较）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// User-Agent → 设置页里认得出的一个名字。
fn browser_label(user_agent: &str) -> String {
    let browser = if user_agent.contains("Edg/") {
        "Edge"
    } else if user_agent.contains("Firefox/") {
        "Firefox"
    } else if user_agent.contains("Chrome/") || user_agent.contains("Chromium/") {
        "Chrome"
    } else if user_agent.contains("Safari/") {
        "Safari"
    } else {
        "浏览器"
    };
    format!("网页 · {browser}")
}

/// 请求里带的那个 Bearer 令牌。
fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// 设备登录回跳地址的白名单：`komo://auth/callback`（桌面端深链），或本机回环地址上的任意端口
/// （`http://127.0.0.1:<port>/…`、`localhost`、`[::1]`；CLI 与桌面端的回环回跳）。不带账号、不带
/// fragment。其余一律不认——否则登录完成的授权码会被送去别人手里。
pub fn redirect_allowed(uri: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(uri) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return false;
    }
    match url.scheme() {
        "komo" => {
            url.host_str() == Some("auth") && url.path() == "/callback" && url.port().is_none()
        }
        "http" => {
            matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
                && url.port().is_some()
        }
        _ => false,
    }
}

/// 把授权码（与客户端的 `state`）拼进回跳地址。
pub fn redirect_with_code(redirect_uri: &str, code: &str, state: Option<&str>) -> Option<String> {
    let mut url = reqwest::Url::parse(redirect_uri).ok()?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("code", code);
        if let Some(state) = state {
            query.append_pair("state", state);
        }
    }
    Some(url.into())
}

/// 登录后跳回的路径：只认本站内的相对路径，别的一律回 `/`（防开放重定向）。
pub fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(next)
            if next.starts_with('/')
                && !next.starts_with("//")
                && !next.contains('\\')
                && !next.chars().any(char::is_control) =>
        {
            next.to_string()
        }
        _ => "/".to_string(),
    }
}

/// 挂在受保护路由上的那一层（`axum::middleware::from_fn_with_state(Arc<AuthService>, check)`）。
pub async fn check(
    State(auth): State<Arc<AuthService>>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiFailure> {
    let authed = auth
        .authenticate(request.headers(), request.method())
        .await?;
    let Some(authed) = authed else {
        return Err(ApiFailure::new(
            ErrorCode::Unauthorized,
            "需要登录：浏览器走 /auth/login，桌面端 / CLI 用设备令牌（Authorization: Bearer），机器上的命令行用 runtime/gateway.json 里的本机令牌",
        ));
    };
    request.extensions_mut().insert(authed.credential.clone());
    let mut response = next.run(request).await;
    // 网页会话刚续了期：把 cookie 的有效期也往后推，否则浏览器那一侧还是从登录那天起算。
    if let Some(cookie) = authed.reissue_cookie
        && let Ok(value) = HeaderValue::from_str(&auth.session_cookie(&cookie))
    {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn only_loopback_and_the_deep_link_may_receive_a_device_code() {
        for ok in [
            "http://127.0.0.1:53211/callback",
            "http://localhost:8765/cb",
            "http://[::1]:9000/callback",
            "komo://auth/callback",
        ] {
            assert!(redirect_allowed(ok), "{ok}");
        }
        for bad in [
            "http://127.0.0.1/callback",
            "https://127.0.0.1:53211/callback",
            "http://evil.example.com:53211/callback",
            "http://127.0.0.1.evil.com:80/callback",
            "http://user@127.0.0.1:53211/callback",
            "http://127.0.0.1:53211/callback#frag",
            "komo://session/abc",
            "komo://auth/other",
            "javascript:alert(1)",
            "not a url",
        ] {
            assert!(!redirect_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn the_code_and_state_are_appended_to_the_redirect() {
        let url = redirect_with_code("http://127.0.0.1:1234/callback", "c1", Some("s 1")).unwrap();
        assert_eq!(url, "http://127.0.0.1:1234/callback?code=c1&state=s+1");
        let url = redirect_with_code("komo://auth/callback", "c1", None).unwrap();
        assert_eq!(url, "komo://auth/callback?code=c1");
    }

    #[test]
    fn next_must_be_a_local_path() {
        assert_eq!(safe_next(Some("/chats/1?x=2")), "/chats/1?x=2");
        for bad in [
            Some("//evil.com"),
            Some("https://evil.com"),
            Some("/\\evil.com"),
            Some("evil"),
            Some("/a\nb"),
            None,
        ] {
            assert_eq!(safe_next(bad), "/", "{bad:?}");
        }
    }

    #[test]
    fn bearer_is_read_case_insensitively_and_other_schemes_are_not_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer abc"),
        );
        assert_eq!(bearer(&headers).as_deref(), Some("abc"));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer(&headers), None);
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn constant_time_eq_compares_whole_values() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn the_device_token_hash_is_stable_and_not_the_token() {
        let token = format!("{DEVICE_PREFIX}{}", random_token());
        assert_eq!(hash(&token), hash(&token));
        assert!(!hash(&token).contains(&token));
        assert_eq!(hash(&token).len(), 64);
    }

    // ------------------------------------------------------------ AuthService（库是真的，时钟可拨）

    use crate::service::test_support::harness::{DEFAULT_ENV, config_toml, write_home};
    use komo_config::LoadOptions;
    use komo_kernel::test_support::TestClock;

    struct Fixture {
        auth: AuthService,
        clock: TestClock,
        _dir: tempfile::TempDir,
    }

    async fn fixture(local_token: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        write_home(dir.path(), &config_toml(""), DEFAULT_ENV);
        let holder = Arc::new(ConfigHolder::load(&LoadOptions::at(dir.path())).unwrap());
        let clock = TestClock::fixed();
        let auth = AuthService::new(
            Db::open_memory().await.unwrap(),
            holder,
            Arc::new(clock.clone()),
            local_token.to_string(),
        );
        Fixture {
            auth,
            clock,
            _dir: dir,
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn cookie(token: &str) -> HeaderMap {
        headers(&[("cookie", &format!("{SESSION_COOKIE}={token}"))])
    }

    fn bearer_header(token: &str) -> HeaderMap {
        headers(&[("authorization", &format!("Bearer {token}"))])
    }

    fn device_request(verifier: &str) -> DeviceRequest {
        DeviceRequest {
            challenge: s256_challenge(verifier),
            redirect_uri: "http://127.0.0.1:5000/cb".into(),
            state: Some("s".into()),
            name: "桌面".into(),
        }
    }

    /// 签发 → 查找：只认对的令牌与对的种类；库里没有令牌本身；清单里带名字与账号。
    #[tokio::test]
    async fn an_issued_web_session_is_found_by_its_token_and_only_as_a_cookie() {
        let f = fixture("local-token").await;
        let token = f
            .auth
            .issue_web(
                "u-1",
                Some("me@example.com"),
                "Mozilla/5.0 Chrome/130 Safari/537",
            )
            .await
            .unwrap();
        let found = f
            .auth
            .authenticate(&cookie(&token), &Method::GET)
            .await
            .unwrap()
            .expect("cookie 认得");
        assert!(matches!(found.credential, Credential::Web { .. }));
        assert_eq!(found.subject.as_deref(), Some("u-1"));
        assert_eq!(found.email.as_deref(), Some("me@example.com"));
        assert!(found.reissue_cookie.is_none(), "刚签发不用续");

        // 别的令牌、没带、网页令牌当 Bearer 用：都不认。
        assert!(
            f.auth
                .authenticate(&cookie("nope"), &Method::GET)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            f.auth
                .authenticate(&HeaderMap::new(), &Method::GET)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            f.auth
                .authenticate(&bearer_header(&token), &Method::GET)
                .await
                .unwrap()
                .is_none()
        );

        let list = f.auth.list(found.credential.session_id()).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].label, "网页 · Chrome");
        assert_eq!(list[0].kind, AuthSessionKind::Web);
        assert!(list[0].current);
        assert_eq!(list[0].expires_at, Some(f.clock.now() + WEB_TTL));
        assert!(!format!("{list:?}").contains(&token), "清单里没有令牌");
        assert!(!f.auth.list(None).await.unwrap()[0].current);
    }

    /// 本机令牌恒定时间比较；空本机令牌的 Gateway 一切都算本机。
    #[tokio::test]
    async fn the_local_token_and_the_open_gateway() {
        let f = fixture("local-token").await;
        let local = f
            .auth
            .authenticate(&bearer_header("local-token"), &Method::POST)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(local.credential, Credential::Local);
        assert!(local.subject.is_none());
        assert!(
            f.auth
                .authenticate(&bearer_header("local-tokeX"), &Method::GET)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            f.auth
                .authenticate(&bearer_header("local"), &Method::GET)
                .await
                .unwrap()
                .is_none()
        );

        let open = fixture("").await;
        let anyone = open
            .auth
            .authenticate(&HeaderMap::new(), &Method::DELETE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(anyone.credential, Credential::Local);
    }

    /// 网页会话 30 天滑动续期：10 分钟内不写库不重发 cookie；超过 10 分钟的下一次请求续期并要求重发 cookie；
    /// 距上次续期超过 30 天才过期。
    #[tokio::test]
    async fn a_web_session_slides_and_finally_expires() {
        let f = fixture("t").await;
        let token = f.auth.issue_web("u-1", None, "").await.unwrap();
        let headers = cookie(&token);
        let authenticate = || f.auth.authenticate(&headers, &Method::GET);

        f.clock.advance(Duration::minutes(5));
        let early = authenticate().await.unwrap().unwrap();
        assert!(early.reissue_cookie.is_none(), "5 分钟：不续");
        let created = f.auth.list(None).await.unwrap()[0].expires_at.unwrap();
        assert_eq!(created, TestClock::fixed().now() + WEB_TTL, "也没写库");

        f.clock.advance(Duration::minutes(6));
        let renewed = authenticate().await.unwrap().unwrap();
        assert_eq!(
            renewed.reissue_cookie.as_deref(),
            Some(token.as_str()),
            "11 分钟：续并重发 cookie"
        );
        assert_eq!(
            f.auth.list(None).await.unwrap()[0].expires_at,
            Some(f.clock.now() + WEB_TTL)
        );
        assert!(
            authenticate()
                .await
                .unwrap()
                .unwrap()
                .reissue_cookie
                .is_none(),
            "续完马上再来不重复"
        );

        // 每隔不到 30 天用一次就一直有效。
        for _ in 0..3 {
            f.clock.advance(Duration::days(29));
            assert!(authenticate().await.unwrap().is_some());
        }
        // 超过 30 天没用：过期，清单里也没了。
        f.clock.advance(Duration::days(31));
        assert!(authenticate().await.unwrap().is_none());
        assert!(f.auth.list(None).await.unwrap().is_empty());
        assert_eq!(f.auth.purge_expired().await.unwrap(), 1);
        assert_eq!(f.auth.purge_expired().await.unwrap(), 0);
    }

    /// 带 cookie 的写请求要核对 Origin；GET / HEAD 与 Bearer 不看。
    #[tokio::test]
    async fn a_cookie_write_needs_a_same_origin_request() {
        let f = fixture("t").await;
        let token = f.auth.issue_web("u-1", None, "").await.unwrap();
        let with_origin = |origin: Option<&str>| {
            let mut map = cookie(&token);
            map.insert(header::HOST, HeaderValue::from_static("komo.local:7878"));
            if let Some(origin) = origin {
                map.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
            }
            map
        };
        for method in [Method::GET, Method::HEAD] {
            assert!(
                f.auth
                    .authenticate(&with_origin(None), &method)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            let denied = f.auth.authenticate(&with_origin(None), &method).await;
            assert!(
                matches!(&denied, Err(failure) if failure.code() == ErrorCode::Denied),
                "{method}"
            );
            let cross = f
                .auth
                .authenticate(&with_origin(Some("http://evil.example.com")), &method)
                .await;
            assert!(cross.is_err(), "{method} 跨源");
            let same = f
                .auth
                .authenticate(&with_origin(Some("http://komo.local:7878")), &method)
                .await;
            assert!(same.unwrap().is_some(), "{method} 同源");
        }
        // 设备令牌（Bearer）不被 CSRF 影响，不核对 Origin。
        let (device, _) = redeem_device(&f).await;
        assert!(
            f.auth
                .authenticate(&bearer_header(&device), &Method::POST)
                .await
                .unwrap()
                .is_some()
        );
    }

    async fn redeem_device(f: &Fixture) -> (String, AuthSessionRecord) {
        let verifier = random_token();
        let code = f
            .auth
            .issue_code(device_request(&verifier), "u-1", Some("me@example.com"));
        f.auth
            .redeem_code(&code, &verifier, "http://127.0.0.1:5000/cb")
            .await
            .unwrap()
    }

    /// 授权码：一次性、两分钟、PKCE 与 redirect_uri 都要对；换到的设备令牌不过期。
    #[tokio::test]
    async fn a_device_code_is_single_use_short_lived_and_bound_to_pkce_and_redirect() {
        let f = fixture("t").await;
        let redirect = "http://127.0.0.1:5000/cb";
        let verifier = random_token();

        let code = f.auth.issue_code(device_request(&verifier), "u-1", None);
        let (token, record) = f
            .auth
            .redeem_code(&code, &verifier, redirect)
            .await
            .unwrap();
        assert!(token.starts_with(DEVICE_PREFIX));
        assert_eq!(
            (record.label.as_str(), record.subject.as_str()),
            ("桌面", "u-1")
        );
        assert!(record.expires_at.is_none());
        assert!(matches!(
            f.auth.redeem_code(&code, &verifier, redirect).await,
            Err(AuthError::BadRequest(_))
        ));

        // 错的 verifier、错的 redirect_uri、过期：都拒，而且码作废。
        for (name, verifier_used, redirect_used, wait) in [
            ("verifier", "x".repeat(43), redirect, 0),
            ("redirect", verifier.clone(), "http://127.0.0.1:5001/cb", 0),
            ("过期", verifier.clone(), redirect, 3),
        ] {
            let code = f.auth.issue_code(device_request(&verifier), "u-1", None);
            f.clock.advance(Duration::minutes(wait));
            assert!(
                matches!(
                    f.auth
                        .redeem_code(&code, &verifier_used, redirect_used)
                        .await,
                    Err(AuthError::BadRequest(_))
                ),
                "{name}"
            );
            assert!(
                matches!(
                    f.auth.redeem_code(&code, &verifier, redirect).await,
                    Err(AuthError::BadRequest(_))
                ),
                "{name}：码已作废"
            );
        }
        assert!(matches!(
            f.auth
                .redeem_code("never-issued", &verifier, redirect)
                .await,
            Err(AuthError::BadRequest(_))
        ));

        // 设备令牌长期有效，且不会要求重发 cookie。
        f.clock.advance(Duration::days(400));
        let found = f
            .auth
            .authenticate(&bearer_header(&token), &Method::GET)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(found.credential, Credential::Device { .. }));
        assert!(found.reissue_cookie.is_none());
        // 设备令牌放在 cookie 里不认。
        assert!(
            f.auth
                .authenticate(&cookie(&token), &Method::GET)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// 吊销：按 id、按退出；清单按最近使用排序；吊销了就认不出，再吊销答没有。
    #[tokio::test]
    async fn revoking_and_logging_out_end_a_session_and_the_list_orders_by_last_use() {
        let f = fixture("t").await;
        let web = f.auth.issue_web("u-1", None, "Firefox/130").await.unwrap();
        f.clock.advance(Duration::minutes(1));
        let (device, device_record) = redeem_device(&f).await;
        let list = f.auth.list(None).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].kind, AuthSessionKind::Device, "后用的在前");
        assert_eq!(list[1].label, "网页 · Firefox");

        assert!(f.auth.revoke(&device_record.id).await.unwrap());
        assert!(!f.auth.revoke(&device_record.id).await.unwrap());
        assert!(
            f.auth
                .authenticate(&bearer_header(&device), &Method::GET)
                .await
                .unwrap()
                .is_none()
        );

        // 退出只吊销这个 cookie 的会话；没带 cookie 也算成功。
        f.auth.logout(&HeaderMap::new()).await.unwrap();
        assert!(
            f.auth
                .authenticate(&cookie(&web), &Method::GET)
                .await
                .unwrap()
                .is_some()
        );
        f.auth.logout(&cookie(&web)).await.unwrap();
        f.auth.logout(&cookie(&web)).await.unwrap();
        assert!(
            f.auth
                .authenticate(&cookie(&web), &Method::GET)
                .await
                .unwrap()
                .is_none()
        );
        assert!(f.auth.list(None).await.unwrap().is_empty());
    }

    /// 重开库之后登录状态仍有效（落盘的是令牌的哈希）。
    #[tokio::test]
    async fn sessions_are_still_valid_after_the_database_is_reopened() {
        let dir = tempfile::tempdir().unwrap();
        write_home(dir.path(), &config_toml(""), DEFAULT_ENV);
        let holder = Arc::new(ConfigHolder::load(&LoadOptions::at(dir.path())).unwrap());
        let clock = TestClock::fixed();
        let path = dir.path().join("aux.db");
        let make = || async {
            AuthService::new(
                Db::connect(&path).await.unwrap(),
                Arc::clone(&holder),
                Arc::new(clock.clone()),
                "t".into(),
            )
        };
        let first = make().await;
        let web = first
            .issue_web("u-1", Some("me@example.com"), "")
            .await
            .unwrap();
        let verifier = random_token();
        let code = first.issue_code(device_request(&verifier), "u-1", None);
        let (device, _) = first
            .redeem_code(&code, &verifier, "http://127.0.0.1:5000/cb")
            .await
            .unwrap();
        drop(first);

        let second = make().await;
        assert!(
            second
                .authenticate(&cookie(&web), &Method::GET)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            second
                .authenticate(&bearer_header(&device), &Method::GET)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(second.list(None).await.unwrap().len(), 2);
        // 内存里的临时状态（授权码、待完成的登录）不跟着走：重启后换不了。
        assert!(
            second
                .redeem_code(&code, &verifier, "http://127.0.0.1:5000/cb")
                .await
                .is_err()
        );
    }

    /// 没配 `[auth.oidc]`：发起与回调都是 NotConfigured，cookie 不加 Secure。
    #[tokio::test]
    async fn without_oidc_login_is_not_configured() {
        let f = fixture("t").await;
        assert!(!f.auth.oidc_configured());
        assert!(f.auth.oidc_config().is_none());
        assert!(!f.auth.secure_cookies());
        assert!(f.auth.public_origin().is_none());
        assert!(matches!(
            f.auth.begin(Purpose::Web { next: "/".into() }).await,
            Err(AuthError::NotConfigured)
        ));
        assert!(matches!(
            f.auth.finish("s", Some("s"), "c").await,
            Err(AuthError::NotConfigured)
        ));
        assert!(!f.auth.session_cookie("tok").contains("Secure"));
        assert!(f.auth.session_cookie("tok").contains("HttpOnly"));
        assert!(f.auth.clear_session_cookie().contains("Max-Age=0"));
    }

    /// 内存里的待完成登录有上限：先清过期的，仍满了就丢最早到期的。
    #[test]
    fn the_pending_table_is_bounded_and_drops_the_oldest_first() {
        let table: Mutex<HashMap<String, OffsetDateTime>> = Mutex::new(HashMap::new());
        let now = datetime!(2026-09-15 08:00:00 UTC);
        for index in 0..(MAX_PENDING + 20) {
            insert_bounded(
                &table,
                format!("k{index}"),
                now + Duration::seconds(index as i64 + 1),
                now,
                |expires| *expires,
            );
        }
        let table = table.lock().unwrap();
        assert_eq!(table.len(), MAX_PENDING);
        assert!(!table.contains_key("k0"), "最早到期的先丢");
        assert!(table.contains_key(&format!("k{}", MAX_PENDING + 19)));

        // 过期的在下一次插入时被清掉。
        drop(table);
        let table: Mutex<HashMap<String, OffsetDateTime>> = Mutex::new(HashMap::from([
            ("old".to_string(), now - Duration::seconds(1)),
            ("live".to_string(), now + Duration::seconds(1)),
        ]));
        insert_bounded(&table, "new".into(), now + Duration::seconds(5), now, |e| {
            *e
        });
        let keys: std::collections::BTreeSet<_> = table.lock().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["live".to_string(), "new".to_string()].into());
    }

    #[test]
    fn a_browser_label_names_the_browser_with_edge_before_chrome() {
        for (agent, label) in [
            ("Mozilla/5.0 Edg/130 Chrome/130 Safari/537", "网页 · Edge"),
            ("Mozilla/5.0 Firefox/130", "网页 · Firefox"),
            ("Mozilla/5.0 Chrome/130 Safari/537", "网页 · Chrome"),
            ("Mozilla/5.0 Version/17 Safari/605", "网页 · Safari"),
            ("curl/8", "网页 · 浏览器"),
            ("", "网页 · 浏览器"),
        ] {
            assert_eq!(browser_label(agent), label, "{agent}");
        }
    }
}
