//! OpenAI Codex provider auth, borrowed from hermes-agent's `openai-codex`
//! OAuth path.
//!
//! Codex models are reached through the ChatGPT backend
//! (`https://chatgpt.com/backend-api/codex`, an OpenAI **Responses API**
//! surface), authenticated not with an API key but with the OAuth tokens the
//! official Codex CLI writes to `~/.codex/auth.json` (`$CODEX_HOME` honored).
//! We reuse that login wholesale: read the token set, decode the access-token
//! JWT to know when it is expiring, and refresh it against
//! `auth.openai.com/oauth/token` with the Codex CLI's pinned client id.
//!
//! `$KOMO_HOME/codex/` is accepted as a second location, because the login and
//! the process that uses it need not sit on the same machine: a container has
//! no Codex CLI and no browser to log in with, so the operator copies
//! `auth.json` into the volume that already carries `.env`. It is a *fallback*
//! — a real `~/.codex/auth.json` still wins, so a workstation keeps reading the
//! file the CLI actually rotates.
//!
//! Because the access token lives only a few hours and the gateway is a
//! long-running process, refresh can't happen once at startup. [`CodexAuth`]
//! resolves a fresh token on demand, and the provider layer's
//! [`TokenSource`](komo_provider::TokenSource) hook calls it to stamp a
//! bearer on **every** outgoing request, so a turn an hour into the process
//! still authenticates.
//! Refreshed tokens are written back to `auth.json` so the Codex CLI and komo
//! stay in sync.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use serde::Deserialize;
use tokio::sync::Mutex;

use komo_provider::TokenSource;

/// Codex CLI's OAuth client id (matches `codex-rs`), used for token refresh.
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// OpenAI's OAuth token endpoint (refresh-token grant).
const CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// ChatGPT-backed Codex inference endpoint (OpenAI Responses API surface).
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models?client_version=1.0.0";
/// Refresh this many seconds before the access token's `exp`.
const REFRESH_SKEW_SECS: u64 = 120;
/// `originator` allow-listed by the Codex backend's Cloudflare layer; non-codex
/// originators from non-residential IPs are served a 403 challenge.
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
/// `User-Agent` shaped like the upstream `codex-rs` CLI (beats SDK fingerprinting).
const CODEX_USER_AGENT: &str = "codex_cli_rs/0.0.0 (komo)";

pub const DEFAULT_CODEX_MODELS: &[&str] = &[
    "gpt-5.5",
    "gpt-5.4-mini",
    "gpt-5.4",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
];

const FORWARD_COMPAT_TEMPLATE_MODELS: &[(&str, &[&str])] = &[
    ("gpt-5.5", &["gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex"]),
    ("gpt-5.4-mini", &["gpt-5.3-codex"]),
    ("gpt-5.4", &["gpt-5.3-codex"]),
    ("gpt-5.3-codex-spark", &["gpt-5.3-codex"]),
];

const AUTH_FILE: &str = "auth.json";

/// Where a Codex login may live, in the order we accept one. `$CODEX_HOME` is
/// explicit and answers alone; otherwise the CLI's own directory comes first
/// and komo's home is the fallback for hosts that have no CLI.
fn codex_home_candidates() -> Vec<PathBuf> {
    if let Some(explicit) = std::env::var("CODEX_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
    {
        return vec![explicit];
    }
    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".codex"));
    }
    candidates.push(komo_core::paths::komo_home().join("codex"));
    candidates
}

/// The first candidate that actually holds an `auth.json`, else the first one —
/// so an absent login is reported against the place it is most expected.
fn pick_codex_home(candidates: &[PathBuf]) -> PathBuf {
    candidates
        .iter()
        .find(|home| home.join(AUTH_FILE).is_file())
        .or_else(|| candidates.first())
        .cloned()
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

/// Path to the Codex CLI's shared credential file.
fn codex_auth_path() -> PathBuf {
    codex_home().join(AUTH_FILE)
}

pub fn codex_auth_file_path() -> PathBuf {
    codex_auth_path()
}

fn codex_home() -> PathBuf {
    pick_codex_home(&codex_home_candidates())
}

/// What to tell an operator with no Codex login. `codex` is the whole answer on
/// a workstation; on a host without the CLI (a container) it is no answer at
/// all, so name every file that would be accepted instead.
pub fn missing_login_hint() -> String {
    let looked = codex_home_candidates()
        .iter()
        .map(|home| home.join(AUTH_FILE).display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "no Codex login found (looked in {looked}) — run `codex` to log in, or copy \
         an existing auth.json to one of those paths ($CODEX_HOME overrides both)"
    )
}

pub fn looks_like_codex_model_id(model: &str) -> bool {
    let model = model.trim().to_lowercase();
    model.starts_with("codex-")
        || model.contains("-codex")
        || model.starts_with("gpt-5.")
        || model.starts_with("gpt-5-")
}

/// The three fields we need out of the Codex token set.
#[derive(Clone)]
struct CodexTokens {
    access_token: String,
    refresh_token: String,
    /// ChatGPT account id (for the `ChatGPT-Account-ID` header). Read from the
    /// `tokens.account_id` field, falling back to the JWT's `chatgpt_account_id`
    /// claim.
    account_id: Option<String>,
}

/// `auth.json` shape — only the fields we read; everything else is preserved
/// verbatim on write-back via a raw [`serde_json::Value`].
#[derive(Deserialize)]
struct AuthFile {
    tokens: Option<AuthTokens>,
}

#[derive(Deserialize)]
struct AuthTokens {
    access_token: Option<String>,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Decode a JWT's payload (middle segment) without verifying its signature — we
/// only inspect claims (`exp`, account id). Returns `None` for any malformed token.
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Extract the `chatgpt_account_id` claim from a Codex access token.
fn account_id_from_jwt(token: &str) -> Option<String> {
    jwt_claims(token)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

/// Whether `token` expires within `skew` seconds. A token whose `exp` can't be
/// read is treated as *not* expiring — we'd rather try it and let the wire
/// return 401 than refresh blindly on every request.
fn is_expiring(token: &str, skew: u64) -> bool {
    match jwt_claims(token).and_then(|c| c.get("exp").and_then(|e| e.as_u64())) {
        Some(exp) => exp <= now_secs() + skew,
        None => false,
    }
}

/// Read and validate the Codex token set from `path`.
fn read_tokens(path: &Path) -> anyhow::Result<CodexTokens> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("{}", missing_login_hint()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let file: AuthFile = serde_json::from_str(&content)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let tokens = file.tokens.ok_or_else(|| {
        anyhow!(
            "{} has no `tokens` block — re-run `codex` to log in",
            path.display()
        )
    })?;
    let access_token = tokens
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{} is missing tokens.access_token", path.display()))?;
    let refresh_token = tokens.refresh_token.unwrap_or_default();
    let account_id = tokens
        .account_id
        .filter(|s| !s.is_empty())
        .or_else(|| account_id_from_jwt(&access_token));
    Ok(CodexTokens {
        access_token,
        refresh_token,
        account_id,
    })
}

fn push_unique(out: &mut Vec<String>, value: impl Into<String>) {
    let value = value.into();
    if !value.is_empty() && !out.iter().any(|v| v == &value) {
        out.push(value);
    }
}

fn add_forward_compat_models(model_ids: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for model in model_ids {
        push_unique(&mut out, model);
    }
    for (synthetic, templates) in FORWARD_COMPAT_TEMPLATE_MODELS {
        if out.iter().any(|m| m == synthetic) {
            continue;
        }
        if templates
            .iter()
            .any(|template| out.iter().any(|m| m == template))
        {
            out.push((*synthetic).to_string());
        }
    }
    out
}

fn read_default_model(codex_home: &Path) -> Option<String> {
    let content = std::fs::read_to_string(codex_home.join("config.toml")).ok()?;
    let payload: toml::Value = toml::from_str(&content).ok()?;
    payload
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

fn read_cache_models(codex_home: &Path) -> Vec<String> {
    let content = match std::fs::read_to_string(codex_home.join("models_cache.json")) {
        Ok(content) => content,
        Err(_) => return Vec::new(),
    };
    let payload: serde_json::Value = match serde_json::from_str(&content) {
        Ok(payload) => payload,
        Err(_) => return Vec::new(),
    };
    let mut sortable = Vec::new();
    if let Some(models) = payload.get("models").and_then(|m| m.as_array()) {
        for item in models {
            let Some(slug) = item
                .get("slug")
                .and_then(|s| s.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let hidden = item
                .get("visibility")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .is_some_and(|v| matches!(v.to_lowercase().as_str(), "hide" | "hidden"));
            if hidden {
                continue;
            }
            let rank = item
                .get("priority")
                .and_then(|p| p.as_i64())
                .unwrap_or(10_000);
            sortable.push((rank, slug.to_string()));
        }
    }
    sortable.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = Vec::new();
    for (_, slug) in sortable {
        push_unique(&mut out, slug);
    }
    out
}

async fn fetch_models_from_api(access_token: &str) -> Vec<String> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };
    let resp = match client
        .get(CODEX_MODELS_URL)
        .bearer_auth(access_token)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp,
        _ => return Vec::new(),
    };
    let payload: serde_json::Value = match resp.json().await {
        Ok(payload) => payload,
        Err(_) => return Vec::new(),
    };
    let mut sortable = Vec::new();
    if let Some(models) = payload.get("models").and_then(|m| m.as_array()) {
        for item in models {
            let Some(slug) = item
                .get("slug")
                .and_then(|s| s.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let hidden = item
                .get("visibility")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .is_some_and(|v| matches!(v.to_lowercase().as_str(), "hide" | "hidden"));
            if hidden {
                continue;
            }
            let rank = item
                .get("priority")
                .and_then(|p| p.as_i64())
                .unwrap_or(10_000);
            sortable.push((rank, slug.to_string()));
        }
    }
    sortable.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    add_forward_compat_models(sortable.into_iter().map(|(_, slug)| slug).collect())
}

pub async fn codex_model_ids(access_token: Option<&str>) -> Vec<String> {
    if let Some(token) = access_token.filter(|t| !t.is_empty()) {
        let live = fetch_models_from_api(token).await;
        if !live.is_empty() {
            return live;
        }
    }

    let home = codex_home();
    let mut out = Vec::new();
    if let Some(model) = read_default_model(&home) {
        push_unique(&mut out, model);
    }
    for model in read_cache_models(&home) {
        push_unique(&mut out, model);
    }
    for model in DEFAULT_CODEX_MODELS {
        push_unique(&mut out, *model);
    }
    add_forward_compat_models(out)
}

/// Write refreshed tokens back into `auth.json`, preserving every other field
/// (`auth_mode`, `OPENAI_API_KEY`, …) so the Codex CLI keeps working. Atomic
/// (temp file + rename), 0600. Best-effort — failure is logged by the caller.
fn write_back(path: &Path, tokens: &CodexTokens) -> anyhow::Result<()> {
    let mut root: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let obj = root.as_object_mut().expect("object ensured above");
    let entry = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
    if let Some(tobj) = entry.as_object_mut() {
        tobj.insert("access_token".into(), tokens.access_token.clone().into());
        tobj.insert("refresh_token".into(), tokens.refresh_token.clone().into());
        if let Some(acc) = &tokens.account_id {
            tobj.insert("account_id".into(), acc.clone().into());
        }
    }
    obj.insert(
        "last_refresh".into(),
        chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            .into(),
    );

    let body = serde_json::to_string_pretty(&root)?;
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("auth.json"),
        std::process::id()
    ));
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Resolves and refreshes Codex OAuth credentials from `~/.codex/auth.json`.
/// Held behind an `Arc` and shared by every request the Codex backend makes.
pub struct CodexAuth {
    path: PathBuf,
    http: reqwest::Client,
    /// Stable account id, snapshotted at construction for the static
    /// `ChatGPT-Account-ID` header.
    account_id: Option<String>,
    /// Live token set; the lock serializes concurrent refreshes so the
    /// single-use refresh token is never spent twice in parallel.
    state: Mutex<CodexTokens>,
}

/// The provider layer asks for a token per request; that is what keeps a
/// long-running gateway authenticated as the hourly access token rotates.
#[async_trait::async_trait]
impl TokenSource for CodexAuth {
    async fn token(&self) -> anyhow::Result<String> {
        self.resolve().await
    }
}

impl CodexAuth {
    /// Load credentials from `$CODEX_HOME/auth.json` (default `~/.codex`).
    /// Errors if the file is absent or malformed — surfaced at startup so the
    /// user is told to run `codex` rather than hitting a 401 mid-turn.
    pub fn load() -> anyhow::Result<Arc<Self>> {
        let path = codex_auth_path();
        let tokens = read_tokens(&path)?;
        Ok(Arc::new(Self {
            account_id: tokens.account_id.clone(),
            http: reqwest::Client::new(),
            path,
            state: Mutex::new(tokens),
        }))
    }

    /// The ChatGPT account id for the `ChatGPT-Account-ID` request header.
    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    /// Return a non-expiring access token, refreshing in place if the current
    /// one is within [`REFRESH_SKEW_SECS`] of expiry. Adopts a newer token from
    /// the shared file first (the Codex CLI may have rotated it), and persists
    /// any refresh back to `auth.json`.
    pub async fn resolve(&self) -> anyhow::Result<String> {
        let mut guard = self.state.lock().await;
        if !is_expiring(&guard.access_token, REFRESH_SKEW_SECS) {
            return Ok(guard.access_token.clone());
        }

        // The Codex CLI (or another komo run) may have already refreshed the
        // shared file. Adopt it before spending our own (single-use) refresh
        // token: if it's fresh we're done, otherwise use its newer refresh token.
        if let Ok(fresh) = read_tokens(&self.path) {
            let was_fresh = !is_expiring(&fresh.access_token, REFRESH_SKEW_SECS);
            *guard = fresh;
            if was_fresh {
                return Ok(guard.access_token.clone());
            }
        }

        let refreshed = self
            .refresh(&guard.refresh_token)
            .await
            .context("refreshing Codex token (run `codex` to re-login if this persists)")?;
        *guard = refreshed;
        if let Err(e) = write_back(&self.path, &guard) {
            tracing::warn!("codex: could not persist refreshed tokens: {e}");
        }
        Ok(guard.access_token.clone())
    }

    /// Exchange a refresh token for a new access token at OpenAI's OAuth endpoint.
    async fn refresh(&self, refresh_token: &str) -> anyhow::Result<CodexTokens> {
        if refresh_token.is_empty() {
            bail!("Codex auth has no refresh_token — run `codex` to log in again");
        }
        // Codex refresh tokens are `rt.<n>.<base64url>` — all URL-safe chars, so
        // direct interpolation needs no percent-encoding. (reqwest's `.form()`
        // helper is compiled out by our `default-features = false` build.)
        let form = format!(
            "grant_type=refresh_token&refresh_token={refresh_token}&client_id={CODEX_OAUTH_CLIENT_ID}"
        );
        let resp = self
            .http
            .post(CODEX_OAUTH_TOKEN_URL)
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(http::header::ACCEPT, "application/json")
            .body(form)
            .send()
            .await
            .context("Codex token endpoint request failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Codex token refresh failed ({status}): {body}");
        }
        let json: serde_json::Value =
            serde_json::from_str(&body).context("Codex token refresh returned invalid JSON")?;
        let access_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Codex token refresh response missing access_token"))?
            .to_string();
        // refresh_token may rotate; keep the old one if the response omits it.
        let refresh_token = json
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| refresh_token.to_string());
        let account_id = self
            .account_id
            .clone()
            .or_else(|| account_id_from_jwt(&access_token));
        Ok(CodexTokens {
            access_token,
            refresh_token,
            account_id,
        })
    }
}

/// Static headers (besides the per-request bearer) the Codex backend needs to
/// pass its Cloudflare layer: it serves a 403 challenge to requests that don't
/// look like the Codex CLI.
pub fn codex_static_headers(account_id: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("originator".to_string(), CODEX_ORIGINATOR.to_string()),
        ("user-agent".to_string(), CODEX_USER_AGENT.to_string()),
    ];
    if let Some(account) = account_id {
        headers.push(("chatgpt-account-id".to_string(), account.to_string()));
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an unsigned JWT with the given payload (header.payload.sig shape).
    fn fake_jwt(payload: serde_json::Value) -> String {
        let b64 = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.{}",
            b64(b"{\"alg\":\"none\"}"),
            b64(payload.to_string().as_bytes()),
            "sig"
        )
    }

    #[test]
    fn jwt_exp_is_decoded() {
        let token = fake_jwt(serde_json::json!({ "exp": now_secs() + 3600 }));
        assert!(
            !is_expiring(&token, REFRESH_SKEW_SECS),
            "an hour out is not expiring"
        );
    }

    #[test]
    fn expired_token_is_flagged() {
        let token = fake_jwt(serde_json::json!({ "exp": now_secs().saturating_sub(10) }));
        assert!(is_expiring(&token, REFRESH_SKEW_SECS));
    }

    #[test]
    fn token_within_skew_is_expiring() {
        let token = fake_jwt(serde_json::json!({ "exp": now_secs() + 30 }));
        assert!(is_expiring(&token, REFRESH_SKEW_SECS), "30s < 120s skew");
    }

    #[test]
    fn unreadable_exp_is_not_expiring() {
        assert!(!is_expiring("not-a-jwt", REFRESH_SKEW_SECS));
        let token = fake_jwt(serde_json::json!({ "sub": "x" }));
        assert!(
            !is_expiring(&token, REFRESH_SKEW_SECS),
            "no exp claim → assume valid"
        );
    }

    #[test]
    fn account_id_is_pulled_from_claim() {
        let token = fake_jwt(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc-123" }
        }));
        assert_eq!(account_id_from_jwt(&token).as_deref(), Some("acc-123"));
    }

    #[test]
    fn komo_home_is_only_a_fallback_for_the_cli_directory() {
        let dir = std::env::temp_dir().join(format!("komo_codex_pick_{}", std::process::id()));
        let cli = dir.join(".codex");
        let fallback = dir.join("komo-home").join("codex");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        let candidates = vec![cli.clone(), fallback.clone()];

        // Neither exists yet: report against the place the CLI would write.
        assert_eq!(pick_codex_home(&candidates), cli);

        std::fs::write(fallback.join(AUTH_FILE), "{}").unwrap();
        assert_eq!(pick_codex_home(&candidates), fallback, "container case");

        std::fs::write(cli.join(AUTH_FILE), "{}").unwrap();
        assert_eq!(
            pick_codex_home(&candidates),
            cli,
            "a real CLI login still wins — it is the one that rotates"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_tokens_parses_chatgpt_shape() {
        let dir = std::env::temp_dir().join(format!("komo_codex_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,
               "tokens":{"access_token":"at","refresh_token":"rt","account_id":"acc"},
               "last_refresh":"2026-06-14T00:00:00Z"}"#,
        )
        .unwrap();
        let t = read_tokens(&path).unwrap();
        assert_eq!(t.access_token, "at");
        assert_eq!(t.refresh_token, "rt");
        assert_eq!(t.account_id.as_deref(), Some("acc"));
    }

    #[test]
    fn write_back_preserves_other_fields() {
        let dir = std::env::temp_dir().join(format!("komo_codex_wb_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,
               "tokens":{"access_token":"old","refresh_token":"oldrt","account_id":"acc"}}"#,
        )
        .unwrap();
        write_back(
            &path,
            &CodexTokens {
                access_token: "new".into(),
                refresh_token: "newrt".into(),
                account_id: Some("acc".into()),
            },
        )
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["auth_mode"], "chatgpt");
        assert_eq!(v["tokens"]["access_token"], "new");
        assert_eq!(v["tokens"]["refresh_token"], "newrt");
        assert!(v.get("last_refresh").is_some());
    }

    #[test]
    fn cache_models_keep_visible_codex_backend_slugs() {
        let dir = std::env::temp_dir().join(format!("komo_codex_cache_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("models_cache.json"),
            r#"{"models":[
                {"slug":"gpt-5.3-codex","priority":20,"supported_in_api":true},
                {"slug":"gpt-5.3-codex-spark","priority":6,"supported_in_api":false},
                {"slug":"gpt-5.4","priority":1,"supported_in_api":true},
                {"slug":"gpt-5-hidden-codex","priority":2,"visibility":"hidden"}
            ]}"#,
        )
        .unwrap();

        let models = read_cache_models(&dir);

        assert_eq!(models[0], "gpt-5.4");
        assert!(models.iter().any(|m| m == "gpt-5.3-codex-spark"));
        assert!(!models.iter().any(|m| m == "gpt-5-hidden-codex"));
    }

    #[test]
    fn forward_compat_models_are_added_from_templates() {
        let models = add_forward_compat_models(vec!["gpt-5.3-codex".into()]);
        assert_eq!(
            models,
            vec![
                "gpt-5.3-codex",
                "gpt-5.5",
                "gpt-5.4-mini",
                "gpt-5.4",
                "gpt-5.3-codex-spark"
            ]
        );
    }

    #[test]
    fn codex_model_detection_is_narrow() {
        assert!(looks_like_codex_model_id("gpt-5.5"));
        assert!(looks_like_codex_model_id("gpt-5.3-codex-spark"));
        assert!(looks_like_codex_model_id("codex-fast"));
        assert!(!looks_like_codex_model_id("gpt-4o-mini"));
        assert!(!looks_like_codex_model_id("deepseek-chat"));
    }
}
