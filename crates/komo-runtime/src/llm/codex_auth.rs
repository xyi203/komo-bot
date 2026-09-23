//! ChatGPT 账号 OAuth：给 Codex 模型用，`auth = "chatgpt"`（§13.3、§3 的
//! `komo auth codex login|status`）。
//!
//! 三条硬性质：
//!
//! - **不碰 `~/.codex/auth.json`**。那是 Codex CLI 自己的凭证文件，`refresh_token`
//!   单次有效会轮换——写它一次就可能把 Codex CLI 挤下线。komo 自己的凭证落在
//!   `<数据目录>/codex/auth.json`（[`credentials_path`]）。
//! - **凭证不进任何 Debug / 日志**。[`CodexAuth`] 不派生 `Debug`；对外只暴露账号、
//!   邮箱、套餐与过期时间（[`CodexStatus`]），access_token / refresh_token 永远不
//!   经这条路印出去。
//! - **JWT 只解不验签**：account id、套餐、邮箱、过期时间都在服务端签发的
//!   access_token / id_token 里，本地只 base64url 解 payload 读字段，不做签名校验
//!   （校验是服务端在下一次请求时做的事）。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use komo_kernel::types::digest::ContentHash;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::Mutex;

use super::transport::{HttpRequest, HttpTransport, TransportError};

/// Codex CLI 的公开 OAuth client（同一个 client id，登录与刷新都用它）。
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const DEVICE_USERCODE_ENDPOINT: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_ENDPOINT: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
/// 操作者要在浏览器里打开的那个地址（配对码显示在这上面）。
pub const DEVICE_AUTH_URL: &str = "https://auth.openai.com/codex/device";

/// 快过期的余量：剩余寿命小于它就在下一次取用前刷新（§13.3）。
const REFRESH_MARGIN: time::Duration = time::Duration::minutes(5);
/// 设备码登录最多等这么久（§13.3 背景：约 10 分钟）。
const DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

/// `credential_fingerprints` 里这一条的键名（§3：校验只看快照里有没有这一行）。
pub const CREDENTIAL_FINGERPRINT_KEY: &str = "chatgpt:codex/auth.json";

/// 凭证文件在数据目录里的位置：`<数据目录>/codex/auth.json`。
pub fn credentials_path(data_dir: &Path) -> PathBuf {
    data_dir.join("codex").join("auth.json")
}

/// 登录 / 刷新这条路能出的岔子。
#[derive(Debug, thiserror::Error)]
pub enum CodexAuthError {
    /// 没有登录，或者登录状态已经失效——都要重新跑 `komo auth codex login`。
    #[error("{0}；跑 `komo auth codex login`")]
    NeedsLogin(String),
    /// 429：额度问题，**不是**登录失效（§13.3）。
    #[error("刷新受限（429）：{0}")]
    RateLimited(String),
    /// 凭证文件读写失败。
    #[error("凭证文件 {path}：{message}")]
    Io { path: String, message: String },
    /// 网络往返本身没成功——退避重试是安全的，不代表要重新登录。
    #[error("传输错误：{0}")]
    Transport(String),
}

fn io_error(path: &Path, message: impl std::fmt::Display) -> CodexAuthError {
    CodexAuthError::Io {
        path: path.display().to_string(),
        message: message.to_string(),
    }
}

fn transport_error(error: TransportError) -> CodexAuthError {
    match error {
        TransportError::Timeout => CodexAuthError::Transport("超时".into()),
        TransportError::Failed(message) => CodexAuthError::Transport(message),
    }
}

/// 一份 ChatGPT 账号凭证。**手写 `Debug`**：access_token / refresh_token / id_token
/// 一个字符都不进去——需要打印的字段单独经 [`CodexStatus`] 那条窄路走。
#[derive(Clone, Serialize, Deserialize)]
pub struct CodexAuth {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

impl CodexAuth {
    /// 剩余寿命小于刷新余量——该在下一次取用前刷新了（§13.3）。
    pub fn needs_refresh(&self, now: OffsetDateTime) -> bool {
        self.expires_at - now < REFRESH_MARGIN
    }
}

impl std::fmt::Debug for CodexAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAuth")
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("plan", &self.plan)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// 状态展示用的一份**窄**投影：不带 access_token / refresh_token / id_token。
pub struct CodexStatus {
    pub account_id: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub expires_at: OffsetDateTime,
}

// ---------------------------------------------------------------- JWT

const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const PROFILE_CLAIM: &str = "https://api.openai.com/profile";

/// 只 base64url 解 payload——不验签（本文件头部说明为什么这样做是对的）。
fn decode_jwt_claims(token: &str) -> Result<Value, CodexAuthError> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| CodexAuthError::NeedsLogin("access_token 不是合法的 JWT".into()))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| CodexAuthError::NeedsLogin(format!("JWT payload 解不出来：{error}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| CodexAuthError::NeedsLogin(format!("JWT payload 不是 JSON：{error}")))
}

fn account_id_from(claims: &Value) -> Result<String, CodexAuthError> {
    claims
        .get(AUTH_CLAIM)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CodexAuthError::NeedsLogin("access_token 里没有 chatgpt_account_id".into()))
}

fn plan_from(claims: &Value) -> Option<String> {
    claims
        .get(AUTH_CLAIM)
        .and_then(|auth| auth.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn email_from(claims: &Value) -> Option<String> {
    claims
        .get(PROFILE_CLAIM)
        .and_then(|profile| profile.get("email"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn expires_at_from(claims: &Value) -> Result<OffsetDateTime, CodexAuthError> {
    let exp = claims
        .get("exp")
        .and_then(Value::as_i64)
        .ok_or_else(|| CodexAuthError::NeedsLogin("access_token 没有 exp".into()))?;
    OffsetDateTime::from_unix_timestamp(exp)
        .map_err(|error| CodexAuthError::NeedsLogin(format!("exp 无法解析：{error}")))
}

/// 从三样拿到的东西（换回来的或刷新回来的）拼出一份 [`CodexAuth`]。
fn build(
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
) -> Result<CodexAuth, CodexAuthError> {
    let claims = decode_jwt_claims(&access_token)?;
    let account_id = account_id_from(&claims)?;
    let plan = plan_from(&claims);
    let expires_at = expires_at_from(&claims)?;
    // 邮箱：id_token 优先（§13.3 背景），没有 id_token 或它里面没有就退回 access_token。
    let email = id_token
        .as_deref()
        .and_then(|token| decode_jwt_claims(token).ok())
        .and_then(|claims| email_from(&claims))
        .or_else(|| email_from(&claims));
    Ok(CodexAuth {
        access_token,
        refresh_token,
        id_token,
        account_id,
        email,
        plan,
        expires_at,
    })
}

// ---------------------------------------------------------------- 文件

/// 读一份凭证。
pub fn load(path: &Path) -> Result<CodexAuth, CodexAuthError> {
    if !path.exists() {
        return Err(CodexAuthError::NeedsLogin(format!(
            "没有 ChatGPT 凭证（{}）",
            path.display()
        )));
    }
    let data = std::fs::read_to_string(path).map_err(|error| io_error(path, error))?;
    serde_json::from_str(&data).map_err(|error| io_error(path, format!("解析失败：{error}")))
}

/// 写一份凭证：临时文件 + rename，权限 0600（§13.3：并发刷新时不能读到半份文件）。
pub fn save(path: &Path, creds: &CodexAuth) -> Result<(), CodexAuthError> {
    let dir = path.parent().ok_or_else(|| io_error(path, "没有父目录"))?;
    std::fs::create_dir_all(dir).map_err(|error| io_error(path, error))?;
    let body = serde_json::to_string_pretty(creds)
        .map_err(|error| io_error(path, format!("序列化失败：{error}")))?;

    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|error| io_error(path, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error(path, error))?;
    }
    tmp.write_all(body.as_bytes())
        .map_err(|error| io_error(path, error))?;
    tmp.persist(path)
        .map_err(|error| io_error(path, error.error))?;
    Ok(())
}

/// `komo config check` 的凭证指纹：文件不存在就没有这一行（§3 的口径与 `.env` 一致）。
pub fn fingerprint(path: &Path) -> Option<ContentHash> {
    let data = std::fs::read_to_string(path).ok()?;
    (!data.trim().is_empty()).then(|| ContentHash::of_str(&data))
}

// ---------------------------------------------------------------- 刷新 / 换码

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

/// 拿 `refresh_token` 换一份新凭证。
///
/// 429 = 额度问题，不是登录失效；400 / 401（`invalid_grant` / `refresh_token_reused`
/// 一类）= 需要重新登录；别的状态码算传输失败，退避重试是安全的（§13.3）。
pub async fn refresh(
    transport: &dyn HttpTransport,
    refresh_token: &str,
) -> Result<CodexAuth, CodexAuthError> {
    let form = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh_token.to_string()),
        ("client_id".to_string(), CLIENT_ID.to_string()),
    ];
    let request = HttpRequest::form(TOKEN_ENDPOINT, form).with_timeout(Duration::from_secs(30));
    let response = transport.post(request).await.map_err(transport_error)?;
    let status = response.status;
    let text = response.text().await.map_err(transport_error)?;
    match status {
        200..=299 => {
            let parsed: TokenResponse = serde_json::from_str(&text).map_err(|error| {
                CodexAuthError::Transport(format!("刷新响应解不出来：{error}（{text}）"))
            })?;
            let refresh_token = parsed
                .refresh_token
                .unwrap_or_else(|| refresh_token.to_string());
            build(parsed.access_token, refresh_token, parsed.id_token)
        }
        429 => Err(CodexAuthError::RateLimited(text)),
        400 | 401 => Err(CodexAuthError::NeedsLogin(format!(
            "刷新被拒绝（{status}）：{text}"
        ))),
        _ => Err(CodexAuthError::Transport(format!(
            "刷新失败（{status}）：{text}"
        ))),
    }
}

/// 设备码登录第一步：申请一个 `user_code`。
#[derive(Debug, Clone)]
pub struct DeviceLogin {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval_secs: u64,
}

#[derive(Debug, Deserialize)]
struct UserCodeResponse {
    device_auth_id: String,
    user_code: String,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

pub async fn start_device_login(
    transport: &dyn HttpTransport,
) -> Result<DeviceLogin, CodexAuthError> {
    let request = HttpRequest::new(DEVICE_USERCODE_ENDPOINT, json!({ "client_id": CLIENT_ID }))
        .with_timeout(Duration::from_secs(30));
    let response = transport.post(request).await.map_err(transport_error)?;
    let status = response.status;
    let text = response.text().await.map_err(transport_error)?;
    if !(200..300).contains(&status) {
        return Err(CodexAuthError::Transport(format!(
            "申请设备码失败（{status}）：{text}"
        )));
    }
    let parsed: UserCodeResponse = serde_json::from_str(&text)
        .map_err(|error| CodexAuthError::Transport(format!("设备码响应解不出来：{error}")))?;
    Ok(DeviceLogin {
        device_auth_id: parsed.device_auth_id,
        user_code: parsed.user_code,
        interval_secs: parsed.interval,
    })
}

/// 一次轮询的结果：还没确认，或者拿到了换令牌用的 `authorization_code`。
enum PollOutcome {
    Pending,
    Ready {
        authorization_code: String,
        code_verifier: String,
    },
}

#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

async fn poll_device_login(
    transport: &dyn HttpTransport,
    login: &DeviceLogin,
) -> Result<PollOutcome, CodexAuthError> {
    let body = json!({
        "device_auth_id": login.device_auth_id,
        "user_code": login.user_code,
    });
    let request =
        HttpRequest::new(DEVICE_TOKEN_ENDPOINT, body).with_timeout(Duration::from_secs(30));
    let response = transport.post(request).await.map_err(transport_error)?;
    match response.status {
        // 403 / 404：手机上还没确认，继续等（背景§的原话）。
        403 | 404 => Ok(PollOutcome::Pending),
        200..=299 => {
            let text = response.text().await.map_err(transport_error)?;
            let parsed: DeviceTokenResponse = serde_json::from_str(&text).map_err(|error| {
                CodexAuthError::Transport(format!("设备码轮询响应解不出来：{error}"))
            })?;
            Ok(PollOutcome::Ready {
                authorization_code: parsed.authorization_code,
                code_verifier: parsed.code_verifier,
            })
        }
        status => {
            let text = response.text().await.unwrap_or_default();
            Err(CodexAuthError::Transport(format!(
                "设备码轮询失败（{status}）：{text}"
            )))
        }
    }
}

/// 拿 `authorization_code` + `code_verifier` 换第一份凭证。
pub async fn exchange_code(
    transport: &dyn HttpTransport,
    authorization_code: &str,
    code_verifier: &str,
) -> Result<CodexAuth, CodexAuthError> {
    let form = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("client_id".to_string(), CLIENT_ID.to_string()),
        ("code".to_string(), authorization_code.to_string()),
        ("code_verifier".to_string(), code_verifier.to_string()),
        ("redirect_uri".to_string(), REDIRECT_URI.to_string()),
    ];
    let request = HttpRequest::form(TOKEN_ENDPOINT, form).with_timeout(Duration::from_secs(30));
    let response = transport.post(request).await.map_err(transport_error)?;
    let status = response.status;
    let text = response.text().await.map_err(transport_error)?;
    if !(200..300).contains(&status) {
        return Err(CodexAuthError::NeedsLogin(format!(
            "换取令牌失败（{status}）：{text}"
        )));
    }
    let parsed: TokenResponse = serde_json::from_str(&text)
        .map_err(|error| CodexAuthError::Transport(format!("令牌响应解不出来：{error}")))?;
    build(
        parsed.access_token,
        parsed.refresh_token.unwrap_or_default(),
        parsed.id_token,
    )
}

/// 整个设备码登录流程：申请配对码 → 提示操作者 → 轮询 → 换令牌 → 落盘。
///
/// `out` 只用来打这一句提示；真正的凭证只经 [`save`] 落盘，不经它。
pub(crate) async fn login_with(
    transport: &dyn HttpTransport,
    path: &Path,
    out: &mut dyn std::io::Write,
) -> Result<CodexAuth, CodexAuthError> {
    let device = start_device_login(transport).await?;
    writeln!(
        out,
        "打开 {DEVICE_AUTH_URL} ，输入这个码：{}",
        device.user_code
    )
    .ok();

    let interval = Duration::from_secs(device.interval_secs.max(1) + 3);
    let deadline = tokio::time::Instant::now() + DEVICE_LOGIN_TIMEOUT;
    loop {
        tokio::time::sleep(interval).await;
        match poll_device_login(transport, &device).await? {
            PollOutcome::Pending => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(CodexAuthError::NeedsLogin("登录超时（10 分钟）".into()));
                }
            }
            PollOutcome::Ready {
                authorization_code,
                code_verifier,
            } => {
                let creds = exchange_code(transport, &authorization_code, &code_verifier).await?;
                save(path, &creds)?;
                writeln!(out, "登录成功：账号 {}", creds.account_id).ok();
                return Ok(creds);
            }
        }
    }
}

/// `komo auth codex login`：不经 Gateway，凭证直接落盘（同 `komo channel wechat
/// login` 的先例）。
pub async fn login(path: &Path, out: &mut dyn std::io::Write) -> Result<CodexAuth, CodexAuthError> {
    login_with(super::default_transport().as_ref(), path, out).await
}

/// `komo auth codex status`：账号、邮箱、套餐、过期时间——**不打印 token**。
pub fn status(path: &Path) -> Result<CodexStatus, CodexAuthError> {
    let creds = load(path)?;
    Ok(CodexStatus {
        account_id: creds.account_id,
        email: creds.email,
        plan: creds.plan,
        expires_at: creds.expires_at,
    })
}

// ---------------------------------------------------------------- 请求时取用

/// 每次请求前取 token：不过期就直接用文件里那份；快过期时加锁、锁内重读（可能另一个
/// 请求已经刷新过了），仍然过期才真的去刷新并写回（§13.3）。
///
/// 锁是进程级的一把：主模型、记忆模型、Cron 覆盖、热重载后的新 factory 各有一个
/// source，但 refresh_token 只能用一次，两个 source 同时刷新，输的那个会被当成需要重登，
/// 服务端还可能因为重复使用而作废整组 token。不跨进程：数据目录只有一个 Gateway 持有（§3）。
static REFRESH_LOCK: Mutex<()> = Mutex::const_new(());

pub struct CodexTokenSource {
    path: PathBuf,
    transport: Arc<dyn HttpTransport>,
}

impl CodexTokenSource {
    pub fn new(path: PathBuf, transport: Arc<dyn HttpTransport>) -> Self {
        CodexTokenSource { path, transport }
    }

    pub async fn token(&self) -> Result<CodexAuth, CodexAuthError> {
        let creds = load(&self.path)?;
        if !creds.needs_refresh(OffsetDateTime::now_utc()) {
            return Ok(creds);
        }
        let _guard = REFRESH_LOCK.lock().await;
        // 锁内重读：等锁的这段时间里，另一个请求可能已经刷新过了。
        let creds = load(&self.path)?;
        if !creds.needs_refresh(OffsetDateTime::now_utc()) {
            return Ok(creds);
        }
        let refreshed = refresh(self.transport.as_ref(), &creds.refresh_token).await?;
        save(&self.path, &refreshed)?;
        Ok(refreshed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::transport::testing::{Reply, ScriptedTransport};
    use super::super::transport::{HttpResponse, RequestBody};
    use super::*;
    use time::macros::datetime;

    /// 一个最小可用的 access_token：`{"alg":"none"}.{claims}.`（不验签，签名段随便写）。
    fn jwt(claims: Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("{header}.{payload}.sig")
    }

    fn access_token(account_id: &str, plan: &str, exp: i64) -> String {
        jwt(json!({
            "exp": exp,
            AUTH_CLAIM: { "chatgpt_account_id": account_id, "chatgpt_plan_type": plan },
        }))
    }

    fn sample(exp: i64) -> CodexAuth {
        build(
            access_token("acct_1", "plus", exp),
            "refresh-1".into(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn account_id_and_plan_and_expiry_come_from_the_access_token_jwt() {
        let creds = sample(datetime!(2026-09-23 12:00:00 UTC).unix_timestamp());
        assert_eq!(creds.account_id, "acct_1");
        assert_eq!(creds.plan.as_deref(), Some("plus"));
        assert_eq!(creds.expires_at, datetime!(2026-09-23 12:00:00 UTC));
    }

    #[test]
    fn the_email_comes_from_the_id_token_when_present() {
        let id_token = jwt(json!({ PROFILE_CLAIM: { "email": "u@example.com" } }));
        let creds = build(
            access_token("acct_1", "plus", 4_000_000_000),
            "refresh-1".into(),
            Some(id_token),
        )
        .unwrap();
        assert_eq!(creds.email.as_deref(), Some("u@example.com"));
    }

    #[test]
    fn a_jwt_missing_the_account_id_needs_login_not_a_panic() {
        let bare = jwt(json!({ "exp": 4_000_000_000_i64 }));
        let error = build(bare, "refresh-1".into(), None).unwrap_err();
        assert!(matches!(error, CodexAuthError::NeedsLogin(_)), "{error:?}");
    }

    #[test]
    fn saving_then_loading_round_trips_byte_for_byte_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex").join("auth.json");
        let creds = sample(4_000_000_000);
        save(&path, &creds).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.account_id, creds.account_id);
        assert_eq!(loaded.access_token, creds.access_token);
        assert_eq!(loaded.expires_at, creds.expires_at);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "凭证只给自己读：{:o}", mode & 0o777);
        }
    }

    #[test]
    fn a_missing_file_is_needs_login_not_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex").join("auth.json");
        let error = load(&path).unwrap_err();
        assert!(matches!(error, CodexAuthError::NeedsLogin(_)), "{error:?}");
    }

    #[tokio::test]
    async fn an_unexpired_credential_is_not_refreshed() {
        let far_future = (OffsetDateTime::now_utc() + time::Duration::hours(1)).unix_timestamp();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex").join("auth.json");
        save(&path, &sample(far_future)).unwrap();

        // 脚本里没有任何回应：真被调用就会因为"脚本演完了"（500）报错。
        let transport = ScriptedTransport::new(vec![]);
        let source = CodexTokenSource::new(path, Arc::new(transport.clone()));
        let token = source.token().await.unwrap();
        assert_eq!(token.account_id, "acct_1");
        assert!(transport.requests().is_empty(), "没过期就不该发请求");
    }

    #[tokio::test]
    async fn an_expiring_credential_is_refreshed_and_the_new_refresh_token_is_saved() {
        let almost_now = (OffsetDateTime::now_utc() + time::Duration::seconds(30)).unix_timestamp();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex").join("auth.json");
        save(&path, &sample(almost_now)).unwrap();

        let new_exp = (OffsetDateTime::now_utc() + time::Duration::hours(1)).unix_timestamp();
        let body = json!({
            "access_token": access_token("acct_1", "plus", new_exp),
            "refresh_token": "refresh-2",
        });
        let transport = ScriptedTransport::new(vec![Reply::json(200, &body.to_string())]);
        let source = CodexTokenSource::new(path.clone(), Arc::new(transport.clone()));

        let token = source.token().await.unwrap();
        assert_eq!(
            token.refresh_token, "refresh-2",
            "换过的 refresh_token 要用新的"
        );
        assert!(!token.needs_refresh(OffsetDateTime::now_utc()));

        let saved = load(&path).unwrap();
        assert_eq!(saved.refresh_token, "refresh-2", "写回的也要是新的");
        assert_eq!(transport.requests().len(), 1);
    }

    /// 先让出一次再应答：不让出的话，第一个 source 会一口气跑完，第二个根本进不了竞争。
    #[derive(Debug)]
    struct Yielding(ScriptedTransport);

    #[async_trait::async_trait]
    impl HttpTransport for Yielding {
        async fn post(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            tokio::task::yield_now().await;
            self.0.post(request).await
        }
    }

    #[tokio::test]
    async fn two_sources_on_one_file_refresh_only_once() {
        let almost_now = (OffsetDateTime::now_utc() + time::Duration::seconds(30)).unix_timestamp();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex").join("auth.json");
        save(&path, &sample(almost_now)).unwrap();

        let new_exp = (OffsetDateTime::now_utc() + time::Duration::hours(1)).unix_timestamp();
        let body = json!({
            "access_token": access_token("acct_1", "plus", new_exp),
            "refresh_token": "refresh-2",
        });
        let scripted = ScriptedTransport::new(vec![Reply::json(200, &body.to_string())]);
        let transport: Arc<dyn HttpTransport> = Arc::new(Yielding(scripted.clone()));
        let main = CodexTokenSource::new(path.clone(), Arc::clone(&transport));
        let memory = CodexTokenSource::new(path.clone(), transport);

        let (a, b) = tokio::join!(main.token(), memory.token());
        assert_eq!(a.unwrap().refresh_token, "refresh-2");
        assert_eq!(b.unwrap().refresh_token, "refresh-2");
        assert_eq!(
            scripted.requests().len(),
            1,
            "单次有效的 refresh_token 只能用一次"
        );
    }

    #[tokio::test]
    async fn a_401_refresh_failure_needs_login() {
        let transport =
            ScriptedTransport::new(vec![Reply::json(401, r#"{"error":"invalid_grant"}"#)]);
        let error = refresh(&transport, "stale-refresh").await.unwrap_err();
        assert!(matches!(error, CodexAuthError::NeedsLogin(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_429_refresh_failure_is_not_a_login_problem() {
        let transport =
            ScriptedTransport::new(vec![Reply::json(429, r#"{"error":"rate_limited"}"#)]);
        let error = refresh(&transport, "some-refresh").await.unwrap_err();
        assert!(matches!(error, CodexAuthError::RateLimited(_)), "{error:?}");
    }

    #[tokio::test]
    async fn refreshing_sends_a_form_encoded_body_not_json() {
        let transport = ScriptedTransport::new(vec![Reply::json(200, "{}")]);
        // 响应解析失败没关系——这里只看请求本身的形状。
        let _ = refresh(&transport, "r").await;
        let RequestBody::Form(fields) = &transport.requests()[0].body else {
            panic!("刷新必须是表单编码，不是 JSON（§13.3）")
        };
        assert!(
            fields.contains(&("grant_type".to_string(), "refresh_token".to_string())),
            "{fields:?}"
        );
        assert!(
            fields.contains(&("client_id".to_string(), CLIENT_ID.to_string())),
            "{fields:?}"
        );
    }
}
