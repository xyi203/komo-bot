//! Claude provider auth, borrowed from hermes-agent's Anthropic OAuth path
//! (`agent/anthropic_credentials.py`, `agent/anthropic_adapter.py`).
//!
//! Claude models are reached at `https://api.anthropic.com/v1/messages` — the
//! same Messages API the [`Provider::Anthropic`](komo_config::Provider) backend
//! uses — but authenticated with the OAuth tokens the official Claude Code CLI
//! writes on login rather than with an `ANTHROPIC_API_KEY`. That is what makes
//! komo's turns draw on a Claude Pro/Max **subscription** instead of metered API
//! billing. We reuse that login wholesale and never mint our own: `claude
//! setup-token` (or any `claude` login) is the whole setup step.
//!
//! Claude Code keeps those credentials in two places, and both are read here:
//! `~/.claude/.credentials.json`, and — since Claude Code 2.1.114 — the macOS
//! Keychain item `Claude Code-credentials`. A given release refreshes one but
//! not always the other, so when both exist the fresher one wins
//! ([`read_credentials`]).
//!
//! `$CLAUDE_CONFIG_DIR` is honored (Claude Code's own override), and
//! `$KOMO_HOME/.claude/` is accepted as a fallback for the same reason
//! [`super::codex`] accepts one: a container has no CLI and no browser to log in
//! with, so the operator copies `.credentials.json` into the volume that already
//! carries `.env`. A real `~/.claude/.credentials.json` still wins, so a
//! workstation keeps reading the file the CLI itself rotates.
//!
//! The access token lives ~8 hours and the gateway is a long-running process, so
//! refresh cannot happen once at startup: [`ClaudeCodeAuth`] resolves a token on
//! demand through [`TokenSource`], and the provider layer stamps a bearer on
//! every outgoing request. Refresh tokens are **single use**, and Claude Code
//! rotates on its own schedule, so before spending ours we re-read the live
//! sources and adopt a token the CLI already refreshed — otherwise the two
//! processes race each other into `invalid_grant`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, anyhow, bail};
use serde::Deserialize;
use tokio::sync::Mutex;

use komo_provider::TokenSource;

/// Claude Code's OAuth client id, shared by every client that borrows this login
/// (Claude Code itself, hermes, pi-ai, OpenCode).
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Token endpoints, tried in order. `platform.claude.com` is the live host;
/// `console.anthropic.com` 404s today but is the historical one, so it stays as
/// a fallback rather than as the only address we know.
const OAUTH_TOKEN_URLS: [&str; 2] = [
    "https://platform.claude.com/v1/oauth/token",
    "https://console.anthropic.com/v1/oauth/token",
];
/// The token endpoint rate-limits (429) requests whose user agent looks like
/// `claude-code/…`; the real CLI uses bare axios there. Inference is the
/// opposite — see [`static_headers`].
const OAUTH_TOKEN_USER_AGENT: &str = "axios/1.7.9";
/// Refresh this many seconds before the access token's `expiresAt`.
const REFRESH_SKEW_SECS: u64 = 60;

/// Inference endpoint. Same Messages API as the API-key backend.
pub const CLAUDE_BASE_URL: &str = "https://api.anthropic.com/v1";

/// Betas an OAuth/subscription request must carry. `claude-code-*` and `oauth-*`
/// are what the subscription grant is scoped to; the other two are GA on Claude
/// 4.6+ (harmless there) and are what Claude Code itself sends.
///
/// Deliberately **not** including `context-1m-2025-08-07`: subscriptions without
/// the long-context beta answer 400 to every request carrying it, which would
/// break short auxiliary calls along with everything else.
const BETAS: &str = "claude-code-20250219,oauth-2025-04-20,\
                     interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";

/// Prepended to the system prompt on this backend. Anthropic routes OAuth
/// traffic by client identity, and requests that do not present Claude Code's
/// intermittently answer 500.
pub const SYSTEM_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// User-agent version used when no local `claude` CLI answers `--version`.
/// Anthropic rejects OAuth requests whose claimed version is far behind the
/// actual release, so this is a floor to keep current, not a pin.
const CLAUDE_CODE_VERSION_FALLBACK: &str = "2.1.74";

const CREDENTIALS_FILE: &str = ".credentials.json";
/// macOS Keychain service name Claude Code 2.1.114+ stores the login under.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Where a Claude Code login may live, in the order we accept one.
/// `$CLAUDE_CONFIG_DIR` is explicit and answers alone.
fn config_dir_candidates() -> Vec<PathBuf> {
    if let Some(explicit) = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
    {
        return vec![explicit];
    }
    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".claude"));
    }
    candidates.push(komo_config::komo_home().join(".claude"));
    candidates
}

/// The first candidate that actually holds a credentials file, else the first
/// one — so an absent login is reported against the place it is most expected.
fn pick_config_dir(candidates: &[PathBuf]) -> PathBuf {
    candidates
        .iter()
        .find(|dir| dir.join(CREDENTIALS_FILE).is_file())
        .or_else(|| candidates.first())
        .cloned()
        .unwrap_or_else(|| PathBuf::from(".claude"))
}

/// Path to the credentials file we read and write back.
pub fn credentials_file_path() -> PathBuf {
    pick_config_dir(&config_dir_candidates()).join(CREDENTIALS_FILE)
}

/// What to tell an operator with no Claude Code login. `claude setup-token` is
/// the whole answer on a workstation; on a host without the CLI it is no answer
/// at all, so name every file that would be accepted instead.
pub fn missing_login_hint() -> String {
    let looked = config_dir_candidates()
        .iter()
        .map(|dir| dir.join(CREDENTIALS_FILE).display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "no Claude Code login found (looked in {looked}, and the macOS Keychain item \
         `{KEYCHAIN_SERVICE}`) — run `claude setup-token` to log in, or copy an existing \
         .credentials.json to one of those paths ($CLAUDE_CONFIG_DIR overrides them all)"
    )
}

/// Which of Claude Code's two stores a login came out of.
///
/// Tracked because a refresh has to be written back to **the store it was read
/// from**. A rotation is single use: refreshing from the Keychain's copy and
/// committing to the file would leave the CLI holding a spent pair, and the next
/// `claude` launch would answer `invalid_grant` and demand a fresh login. komo
/// borrows this login; it must not cost the CLI its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    File,
    Keychain,
}

/// The fields we need out of a Claude Code login.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ClaudeTokens {
    source: Source,
    access_token: String,
    refresh_token: String,
    /// Unix milliseconds. `0` means *unknown* (a managed key rather than a
    /// rotating grant), never "expired" — see [`ClaudeTokens::is_expiring`].
    expires_at_ms: u64,
    /// Persisted verbatim on write-back: Claude Code 2.1.81+ refuses a login
    /// whose stored scopes do not include `user:inference`, so dropping them
    /// would break the CLI we borrowed from.
    scopes: Vec<String>,
}

/// `.credentials.json` shape — only the fields we read. Everything else in the
/// file is preserved verbatim on write-back via a raw [`serde_json::Value`].
#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthRecord>,
}

#[derive(Deserialize)]
struct OauthRecord {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<u64>,
    scopes: Option<Vec<String>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl ClaudeTokens {
    /// Whether this token expires within `skew` seconds. An absent `expiresAt`
    /// is treated as *not* expiring: we would rather send it and let the wire
    /// answer 401 than refresh blindly on every request.
    fn is_expiring(&self, skew: u64) -> bool {
        self.expires_at_ms != 0 && now_ms() + skew * 1_000 >= self.expires_at_ms
    }

    fn from_json(raw: &str, source: Source) -> Option<Self> {
        let file: CredentialsFile = serde_json::from_str(raw).ok()?;
        let record = file.claude_ai_oauth?;
        let access_token = record.access_token.filter(|s| !s.is_empty())?;
        Some(Self {
            source,
            access_token,
            refresh_token: record.refresh_token.unwrap_or_default(),
            expires_at_ms: record.expires_at.unwrap_or(0),
            scopes: record.scopes.unwrap_or_default(),
        })
    }
}

fn read_credentials_file(path: &Path) -> Option<ClaudeTokens> {
    ClaudeTokens::from_json(&std::fs::read_to_string(path).ok()?, Source::File)
}

/// The Keychain item's raw payload — the same `{"claudeAiOauth": …}` document
/// the file holds. `None` off macOS and on any failure: an absent item is the
/// ordinary case on a machine whose Claude Code predates 2.1.114.
fn read_keychain_raw() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn read_credentials_keychain() -> Option<ClaudeTokens> {
    ClaudeTokens::from_json(&read_keychain_raw()?, Source::Keychain)
}

/// The freshest login among the sources Claude Code writes.
///
/// A release refreshes the Keychain item or the file but not always both, so
/// "whichever is still valid" comes first and the later `expiresAt` breaks a tie
/// — that way a refresh spends the newest refresh token rather than a stale copy
/// of it.
fn read_credentials(path: &Path) -> Option<ClaudeTokens> {
    let (keychain, file) = (read_credentials_keychain(), read_credentials_file(path));
    let (Some(keychain), Some(file)) = (&keychain, &file) else {
        return keychain.or(file);
    };
    Some(pick_fresher(keychain, file).clone())
}

/// Which of two logins to trust: the one still valid, else the later expiry.
fn pick_fresher<'a>(keychain: &'a ClaudeTokens, file: &'a ClaudeTokens) -> &'a ClaudeTokens {
    let (kc_valid, file_valid) = (
        !keychain.is_expiring(REFRESH_SKEW_SECS),
        !file.is_expiring(REFRESH_SKEW_SECS),
    );
    if kc_valid != file_valid {
        if kc_valid { keychain } else { file }
    } else if keychain.expires_at_ms >= file.expires_at_ms {
        keychain
    } else {
        file
    }
}

/// Commit refreshed tokens back to the store they were read from, so komo and
/// the Claude Code CLI keep sharing one live grant.
fn write_back(path: &Path, tokens: &ClaudeTokens) -> anyhow::Result<()> {
    match tokens.source {
        Source::File => write_back_file(path, tokens),
        Source::Keychain => write_back_keychain(tokens),
    }
}

/// The login document to store: the refreshed grant merged over whatever else
/// the store already held, so nothing the CLI wrote there is dropped.
fn merged_document(existing: Option<&str>, tokens: &ClaudeTokens) -> serde_json::Value {
    let mut root = existing
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let mut record = serde_json::json!({
        "accessToken": tokens.access_token,
        "refreshToken": tokens.refresh_token,
        "expiresAt": tokens.expires_at_ms,
    });
    if !tokens.scopes.is_empty() {
        record["scopes"] = serde_json::json!(tokens.scopes);
    }
    root.as_object_mut()
        .expect("object ensured above")
        .insert("claudeAiOauth".into(), record);
    root
}

/// The Keychain item's account, read off the existing item so an update lands on
/// it rather than creating a second one. Falls back to the login name, which is
/// what Claude Code itself uses.
fn keychain_account() -> String {
    let attrs = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE])
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    attrs
        .lines()
        .find_map(|line| {
            let rest = line.trim().strip_prefix("\"acct\"<blob>=\"")?;
            rest.strip_suffix('"').map(str::to_string)
        })
        .filter(|acct| !acct.is_empty())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_default()
}

/// Update the Keychain item in place (`-U`).
///
/// The payload rides in argv, because `security` offers no way to pass a secret
/// on stdin. That is a brief exposure to anything reading the process table on
/// this host; the alternative — committing the rotation somewhere the CLI does
/// not read — costs the operator their Claude Code login, which is worse.
fn write_back_keychain(tokens: &ClaudeTokens) -> anyhow::Result<()> {
    let body = serde_json::to_string(&merged_document(read_keychain_raw().as_deref(), tokens))?;
    let account = keychain_account();
    if account.is_empty() {
        bail!("could not determine the `{KEYCHAIN_SERVICE}` Keychain account to update");
    }
    let status = Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-a",
            &account,
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
            &body,
        ])
        .status()
        .context("running `security add-generic-password`")?;
    if !status.success() {
        bail!("`security add-generic-password` exited with {status}");
    }
    Ok(())
}

/// Write refreshed tokens into `.credentials.json`, preserving every other field
/// so the Claude Code CLI keeps working. Atomic (temp file + rename), 0600.
fn write_back_file(path: &Path, tokens: &ClaudeTokens) -> anyhow::Result<()> {
    let root = merged_document(std::fs::read_to_string(path).ok().as_deref(), tokens);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&root)?;
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(CREDENTIALS_FILE),
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

/// Version of the locally installed Claude Code, for the request user agent.
/// Detected once per process — only the OAuth headers need it.
fn claude_code_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        for cmd in ["claude", "claude-code"] {
            let Ok(out) = Command::new(cmd).arg("--version").output() else {
                continue;
            };
            // "2.1.74 (Claude Code)" or bare "2.1.74".
            let version = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            if out.status.success() && version.starts_with(|c: char| c.is_ascii_digit()) {
                return version;
            }
        }
        CLAUDE_CODE_VERSION_FALLBACK.to_string()
    })
}

/// Static headers (besides the per-request bearer) an OAuth Messages request
/// needs: the API version, the subscription betas, and Claude Code's own
/// identity — Anthropic routes OAuth traffic by client, and traffic that does
/// not look like the CLI is served 500s.
pub fn static_headers() -> Vec<(String, String)> {
    vec![
        (
            "anthropic-version".to_string(),
            komo_provider::messages::ANTHROPIC_VERSION.to_string(),
        ),
        ("anthropic-beta".to_string(), BETAS.to_string()),
        (
            "user-agent".to_string(),
            format!("claude-code/{} (external, cli)", claude_code_version()),
        ),
        ("x-app".to_string(), "cli".to_string()),
    ]
}

/// Resolves and refreshes the Claude Code OAuth login.
/// Held behind an `Arc` and shared by every request this backend makes.
pub struct ClaudeCodeAuth {
    path: PathBuf,
    http: reqwest::Client,
    /// Live token set; the lock serializes concurrent refreshes so the
    /// single-use refresh token is never spent twice in parallel.
    state: Mutex<ClaudeTokens>,
}

/// The provider layer asks for a token per request; that is what keeps a
/// long-running gateway authenticated as the access token rotates.
#[async_trait::async_trait]
impl TokenSource for ClaudeCodeAuth {
    async fn token(&self) -> anyhow::Result<String> {
        self.resolve().await
    }
}

impl ClaudeCodeAuth {
    /// Load the login from the Keychain / `.credentials.json`. Errors when there
    /// is none — surfaced at startup so the operator is told to run `claude
    /// setup-token` rather than hitting a 401 mid-turn.
    pub fn load() -> anyhow::Result<Arc<Self>> {
        let path = credentials_file_path();
        let tokens = read_credentials(&path).ok_or_else(|| anyhow!("{}", missing_login_hint()))?;
        Ok(Arc::new(Self {
            http: reqwest::Client::new(),
            path,
            state: Mutex::new(tokens),
        }))
    }

    /// A non-expiring access token, refreshing in place when the current one is
    /// within [`REFRESH_SKEW_SECS`] of expiry.
    pub async fn resolve(&self) -> anyhow::Result<String> {
        let mut guard = self.state.lock().await;
        if !guard.is_expiring(REFRESH_SKEW_SECS) {
            return Ok(guard.access_token.clone());
        }

        // Claude Code (or another komo run) may have already rotated the shared
        // login. Adopt it before spending our own single-use refresh token: if
        // it is fresh we are done, otherwise we at least refresh from its newer
        // refresh token instead of racing into `invalid_grant`.
        if let Some(fresh) = read_credentials(&self.path) {
            let was_fresh = !fresh.is_expiring(REFRESH_SKEW_SECS);
            *guard = fresh;
            if was_fresh {
                return Ok(guard.access_token.clone());
            }
        }

        let refreshed = self
            .refresh(&guard.refresh_token, &guard.scopes, guard.source)
            .await
            .context("refreshing Claude Code token (run `claude setup-token` if this persists)")?;
        *guard = refreshed;
        if let Err(e) = write_back(&self.path, &guard) {
            // The POST already spent the old refresh token, so this process
            // keeps the rotated one in memory and stays usable — but the store
            // still holds the spent pair, and whoever reads it next (the CLI
            // included) will be told to log in again. Loud on purpose.
            tracing::error!(
                "claude-code: refreshed the login but could not persist it ({e}) — \
                 run `claude setup-token` if the CLI starts asking for a login"
            );
        }
        Ok(guard.access_token.clone())
    }

    /// Exchange a refresh token for a new access token, trying each token host
    /// in turn.
    async fn refresh(
        &self,
        refresh_token: &str,
        scopes: &[String],
        source: Source,
    ) -> anyhow::Result<ClaudeTokens> {
        if refresh_token.is_empty() {
            bail!(
                "Claude Code login has no refreshToken — run `claude setup-token` to log in again"
            );
        }
        // Anthropic refresh tokens are `sk-ant-ort01-<base64url>` — all URL-safe
        // characters, so direct interpolation needs no percent-encoding.
        // (reqwest's `.form()` helper is compiled out by our
        // `default-features = false` build.)
        let form = format!(
            "grant_type=refresh_token&refresh_token={refresh_token}&client_id={OAUTH_CLIENT_ID}"
        );
        let mut last_error = None;
        for url in OAUTH_TOKEN_URLS {
            match self.post_token(url, &form).await {
                Ok(json) => {
                    return Self::tokens_from_response(&json, refresh_token, scopes, source);
                }
                Err(e) => {
                    tracing::debug!("claude-code: token refresh failed at {url}: {e:#}");
                    last_error = Some(e);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("Claude Code token refresh failed")))
    }

    async fn post_token(&self, url: &str, form: &str) -> anyhow::Result<serde_json::Value> {
        let resp = self
            .http
            .post(url)
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(http::header::ACCEPT, "application/json")
            // Not the claude-code agent: the token endpoint 429s that one.
            .header(http::header::USER_AGENT, OAUTH_TOKEN_USER_AGENT)
            .body(form.to_string())
            .send()
            .await
            .context("Claude Code token endpoint request failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Claude Code token refresh failed ({status}): {body}");
        }
        serde_json::from_str(&body).context("Claude Code token refresh returned invalid JSON")
    }

    fn tokens_from_response(
        json: &serde_json::Value,
        refresh_token: &str,
        scopes: &[String],
        source: Source,
    ) -> anyhow::Result<ClaudeTokens> {
        let access_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Claude Code token refresh response missing access_token"))?
            .to_string();
        // The refresh token rotates; keep the old one if the response omits it.
        let refresh_token = json
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| refresh_token.to_string());
        // `expires_in` is seconds; an hour is the endpoint's historical value.
        let expires_in = json
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3_600);
        let scopes = json
            .get("scope")
            .and_then(|v| v.as_str())
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(|| scopes.to_vec());
        Ok(ClaudeTokens {
            source,
            access_token,
            refresh_token,
            expires_at_ms: now_ms() + expires_in * 1_000,
            scopes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(expires_at_ms: u64, access: &str) -> ClaudeTokens {
        ClaudeTokens {
            source: Source::File,
            access_token: access.to_string(),
            refresh_token: "rt".to_string(),
            expires_at_ms,
            scopes: vec!["user:inference".to_string()],
        }
    }

    #[test]
    fn an_hour_out_is_not_expiring() {
        assert!(!tokens(now_ms() + 3_600_000, "a").is_expiring(REFRESH_SKEW_SECS));
    }

    #[test]
    fn inside_the_skew_is_expiring() {
        assert!(tokens(now_ms() + 30_000, "a").is_expiring(REFRESH_SKEW_SECS));
        assert!(tokens(now_ms().saturating_sub(10_000), "a").is_expiring(REFRESH_SKEW_SECS));
    }

    #[test]
    fn absent_expiry_is_never_expiring() {
        // A managed key carries no `expiresAt`; refreshing it blindly on every
        // request would be worse than letting the wire answer 401.
        assert!(!tokens(0, "a").is_expiring(REFRESH_SKEW_SECS));
    }

    #[test]
    fn the_valid_source_wins_over_the_later_expiry() {
        // Claude Code refreshes the Keychain but not the file (or the reverse),
        // so validity decides before recency does.
        let valid = tokens(now_ms() + 3_600_000, "fresh");
        let expired = tokens(now_ms() + 7_200_000, "stale");
        let expired = ClaudeTokens {
            expires_at_ms: now_ms().saturating_sub(1),
            ..expired
        };
        assert_eq!(pick_fresher(&expired, &valid).access_token, "fresh");
        assert_eq!(pick_fresher(&valid, &expired).access_token, "fresh");
    }

    #[test]
    fn among_two_valid_sources_the_later_expiry_wins() {
        let older = tokens(now_ms() + 3_600_000, "older");
        let newer = tokens(now_ms() + 7_200_000, "newer");
        assert_eq!(pick_fresher(&older, &newer).access_token, "newer");
        assert_eq!(pick_fresher(&newer, &older).access_token, "newer");
    }

    #[test]
    fn credentials_parse_from_the_cli_shape() {
        let raw = r#"{
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-x",
                "refreshToken": "sk-ant-ort01-y",
                "expiresAt": 1893456000000,
                "scopes": ["user:inference", "user:profile"]
            },
            "somethingElse": 1
        }"#;
        let parsed = ClaudeTokens::from_json(raw, Source::File).expect("parses");
        assert_eq!(parsed.access_token, "sk-ant-oat01-x");
        assert_eq!(parsed.refresh_token, "sk-ant-ort01-y");
        assert_eq!(parsed.expires_at_ms, 1_893_456_000_000);
        assert_eq!(parsed.scopes, ["user:inference", "user:profile"]);
    }

    #[test]
    fn a_login_without_an_access_token_is_no_login() {
        for raw in [
            r#"{"claudeAiOauth": {"refreshToken": "y"}}"#,
            "{}",
            "not json",
        ] {
            assert!(
                ClaudeTokens::from_json(raw, Source::File).is_none(),
                "{raw}"
            );
        }
    }

    /// A rotation is single use, so it has to land where it was read from: a
    /// Keychain login committed to the file would leave the CLI holding a spent
    /// pair and demanding a fresh login.
    #[test]
    fn a_refresh_keeps_the_source_it_came_from() {
        let response = serde_json::json!({
            "access_token": "new", "refresh_token": "rt2", "expires_in": 3600,
            "scope": "user:inference user:profile",
        });
        for source in [Source::File, Source::Keychain] {
            let refreshed =
                ClaudeCodeAuth::tokens_from_response(&response, "rt1", &[], source).unwrap();
            assert_eq!(refreshed.source, source);
            assert_eq!(refreshed.refresh_token, "rt2");
            assert_eq!(refreshed.scopes, ["user:inference", "user:profile"]);
        }
    }

    /// The response's own `scope` is authoritative when it sends one; otherwise
    /// the stored scopes carry forward, because Claude Code 2.1.81+ gates on
    /// `user:inference` being present.
    #[test]
    fn scopes_carry_forward_when_the_response_omits_them() {
        let response = serde_json::json!({ "access_token": "new" });
        let stored = vec!["user:inference".to_string()];
        let refreshed =
            ClaudeCodeAuth::tokens_from_response(&response, "rt1", &stored, Source::File).unwrap();
        assert_eq!(refreshed.scopes, stored);
        // An omitted `refresh_token` means the old one did not rotate.
        assert_eq!(refreshed.refresh_token, "rt1");
    }

    #[test]
    fn write_back_preserves_foreign_fields_and_scopes() {
        let dir = std::env::temp_dir().join(format!("komo_cc_write_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CREDENTIALS_FILE);
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"old","scopes":["user:inference"]},"other":"keep"}"#,
        )
        .unwrap();

        let mut refreshed = tokens(now_ms() + 3_600_000, "new");
        refreshed.refresh_token = "rt2".to_string();
        write_back(&path, &refreshed).unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["other"], "keep", "foreign fields survive");
        assert_eq!(root["claudeAiOauth"]["accessToken"], "new");
        assert_eq!(root["claudeAiOauth"]["refreshToken"], "rt2");
        // Claude Code 2.1.81+ gates on `user:inference` being stored.
        assert_eq!(root["claudeAiOauth"]["scopes"][0], "user:inference");
        assert_eq!(read_credentials_file(&path).unwrap(), refreshed);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn komo_home_is_only_a_fallback_for_the_cli_directory() {
        let dir = std::env::temp_dir().join(format!("komo_cc_pick_{}", std::process::id()));
        let cli = dir.join(".claude");
        let fallback = dir.join("komo-home").join(".claude");
        for d in [&cli, &fallback] {
            std::fs::create_dir_all(d).unwrap();
        }
        let candidates = vec![cli.clone(), fallback.clone()];

        // Nothing exists yet: report against the place the CLI would write.
        assert_eq!(pick_config_dir(&candidates), cli);
        std::fs::write(fallback.join(CREDENTIALS_FILE), "{}").unwrap();
        assert_eq!(pick_config_dir(&candidates), fallback);
        std::fs::write(cli.join(CREDENTIALS_FILE), "{}").unwrap();
        assert_eq!(pick_config_dir(&candidates), cli, "the CLI's own file wins");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_oauth_betas_carry_the_subscription_grant() {
        let headers = static_headers();
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let betas = get("anthropic-beta");
        assert!(betas.contains("oauth-2025-04-20"));
        assert!(betas.contains("claude-code-20250219"));
        // A subscription without the long-context beta 400s on every request
        // carrying it, auxiliary calls included.
        assert!(!betas.contains("context-1m"));
        assert!(get("user-agent").starts_with("claude-code/"));
        assert_eq!(get("x-app"), "cli");
    }
}
