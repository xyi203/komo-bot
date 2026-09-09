//! HTTP client the `komo` CLI and TUI use to reach the **gateway**.
//!
//! Turso takes an exclusive cross-process lock per db file, so the gateway is
//! the only process that opens komo's state. That makes its loopback api
//! channel (`infra/messaging/api.rs`) not one of two transports but *the*
//! transport: [`GatewayClient::connect_or_start`] is what every state-touching
//! command calls, and finding no gateway it starts one rather than reaching for
//! the db itself.
//!
//! Two families of calls live here. The **client plane** is what any komo
//! client speaks — chat turns, one session's transcript, the interaction
//! prompts — and the desktop/web apps speak it too. The **operator plane** is
//! one method (`operator_query` / `operator_command`) over one endpoint: the
//! host-operator surface is a typed enum, not a route per action.

use std::{path::Path, time::Duration};

use anyhow::Context;
use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::infra::rendezvous::{self, GatewayInfo};
use crate::services::operator_control::{
    OperatorCommand, OperatorCommandResult, OperatorQuery, OperatorQueryResult, OperatorReply,
    OperatorRequest, SessionSummary,
};
use komo_core::domain::{events::TurnEvent, message::Message};

/// How long to wait for the gateway to answer a request (a turn can take a
/// while — chat goes through the full agent loop server-side).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// The liveness probe must be quick: a stale rendezvous file (crashed gateway)
/// must not hang the CLI before it starts a fresh one.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a just-started gateway is given to answer `/health`. Generous
/// because a cold start opens the db, migrates it, and wires every tool.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the starting gateway is re-probed. Short so the common case —
/// a warm start in a second or two — does not sit in a sleep.
const START_POLL: Duration = Duration::from_millis(400);

/// Fail with the reason the gateway gave, not merely its status code.
///
/// `reqwest`'s `error_for_status` throws the response body away — and the body
/// is exactly where `ApiError` puts the reason. Every gateway-side failure
/// therefore reached the operator as a bare "500 Internal Server Error" with
/// nothing to act on, while the real cause sat in the gateway log.
///
/// Callers that special-case a status (404) still check it first; this only has
/// to turn *unhandled* failures into something readable.
async fn checked(resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    let status = resp.status();
    if !status.is_client_error() && !status.is_server_error() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let reason = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_string))
        .unwrap_or(body);
    match reason.trim() {
        "" => anyhow::bail!("gateway returned {status}"),
        reason => anyhow::bail!("{reason}"),
    }
}

/// Encode a locally chosen directory for the gateway's workspace header.
///
/// The gateway accepts this form only from loopback callers and canonicalizes
/// it; a **new** task session is bound to the result, and a session that is
/// already bound ignores it. It deliberately carries a path as base64url rather
/// than exposing path syntax in an HTTP header (which also keeps Unicode paths
/// valid).
pub fn folder_workspace_id(dir: &Path) -> anyhow::Result<String> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("cannot resolve workspace `{}`", dir.display()))?;
    if !dir.is_dir() {
        anyhow::bail!("workspace `{}` is not a directory", dir.display());
    }
    let path = dir.to_str().context("workspace path is not valid UTF-8")?;
    Ok(format!(
        "folder:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(path.as_bytes())
    ))
}

pub struct GatewayClient {
    base: String,
    key: String,
    /// Bounded client for ordinary control-plane requests.
    http: reqwest::Client,
    /// Streaming turns can legitimately outlive `REQUEST_TIMEOUT`: one turn
    /// may contain several bounded LLM completions and tool calls. Their
    /// lifetime is enforced server-side; applying a whole-response timeout in
    /// the client would instead cut off a healthy SSE body mid-turn. Vault
    /// indexing and memory backfill ride it for the same reason.
    streaming_http: reqwest::Client,
}

/// The lightweight live snapshot published by the gateway. It deliberately
/// reports mounted channels rather than claiming provider connectivity.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GatewayStatus {
    #[serde(default)]
    pub channels: Vec<String>,
}

/// What a suspended turn on this session is waiting on, as
/// `GET /api/interactions/{session}` reports it. Either field is `None` when
/// nothing of that kind is pending.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Interactions {
    #[serde(default)]
    pub approval: Option<komo_bot::interaction::PendingApproval>,
    #[serde(default)]
    pub question: Option<String>,
}

impl GatewayClient {
    /// Reachable gateway → `Some`; no rendezvous file, unparseable, or the probe
    /// fails (stale file / crashed gateway) → `None`.
    pub async fn try_connect() -> Option<GatewayClient> {
        Self::from_info(rendezvous::read()?).await
    }

    /// Reach the gateway, starting one if none answers.
    ///
    /// komo's state lives in the gateway's process, so "no gateway" is not a
    /// mode with its own behaviour — it is a process that has to exist before
    /// the command can mean anything. On macOS the launchd job is started and
    /// polled; elsewhere the supervisor owns the process, so this only says so.
    pub async fn connect_or_start() -> anyhow::Result<GatewayClient> {
        if let Some(client) = Self::try_connect().await {
            return Ok(client);
        }
        start_gateway()?;
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            tokio::time::sleep(START_POLL).await;
            if let Some(client) = Self::try_connect().await {
                return Ok(client);
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "the gateway did not answer /health within {}s — see `komo logs`",
                    START_TIMEOUT.as_secs()
                );
            }
        }
    }

    /// Build a client for an advertised gateway and confirm it answers `/health`.
    /// Split out from [`try_connect`] so it is testable without a rendezvous file.
    async fn from_info(info: GatewayInfo) -> Option<GatewayClient> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .ok()?;
        let streaming_http = reqwest::Client::builder().build().ok()?;
        let base = info.base_url();
        Self::health_ok(&http, &base).await.then(|| GatewayClient {
            base,
            key: info.key,
            http,
            streaming_http,
        })
    }

    /// Whatever an advertised gateway reports on `/health`, if one answers.
    ///
    /// For `komo doctor`: the payload carries the gateway's build (the CLI and
    /// the gateway are installed by separate steps, so they drift routinely,
    /// and a mismatch surfaces as a deserialization error deep in some command
    /// rather than as anything naming the cause) and its plugin state (mounted
    /// live, invisible from the filesystem). `None` means no gateway is
    /// advertised or it did not answer; the absence of a report is not itself
    /// a failure — `doctor` probes, it never starts anything.
    pub async fn advertised_health() -> Option<Value> {
        let info = rendezvous::read()?;
        let http = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .ok()?;
        http.get(format!("{}/health", info.base_url()))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()
    }

    /// One quick unauthenticated `/health` probe. Shared by [`from_info`] and
    /// `komo health` (the Docker HEALTHCHECK command).
    pub async fn health_ok(http: &reqwest::Client, base: &str) -> bool {
        http.get(format!("{base}/health"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// GET `path` and pull `key` out of the `{ "<key>": T }` envelope.
    async fn get_field<T: DeserializeOwned>(&self, path: &str, key: &str) -> anyhow::Result<T> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.key)
            .send()
            .await?;
        let mut map: Map<String, Value> = checked(resp).await?.json().await?;
        let val = map
            .remove(key)
            .with_context(|| format!("gateway response missing `{key}`"))?;
        Ok(serde_json::from_value(val)?)
    }

    /// POST a control-plane write and return the JSON reply object.
    ///
    /// The one request path every write shares — auth, error mapping, and the
    /// version-skew case: a running gateway from before the endpoint existed
    /// answers 404, which would otherwise surface as an opaque reqwest error
    /// with nothing to do about it (the old gateway holds the lock), so it
    /// becomes an actionable "restart the gateway" message instead.
    async fn post_json(&self, path: &str, body: Value) -> anyhow::Result<Map<String, Value>> {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "the running gateway doesn't serve `{path}` — it predates this command.\n\
                 Restart it onto the current binary (`komo gateway restart`) and retry."
            );
        }
        Ok(checked(resp).await?.json().await?)
    }

    /// [`post_json`], pulling one field out of the `{ "<field>": T }` envelope.
    async fn post_field<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
        field: &str,
    ) -> anyhow::Result<T> {
        let mut map = self.post_json(path, body).await?;
        let val = map
            .remove(field)
            .with_context(|| format!("gateway response missing `{field}`"))?;
        Ok(serde_json::from_value(val)?)
    }

    // ---- operator plane ----------------------------------------------------

    /// Run one read-only operator query.
    pub async fn operator_query(
        &self,
        query: OperatorQuery,
    ) -> anyhow::Result<OperatorQueryResult> {
        match self.operator(OperatorRequest::Query(query)).await? {
            OperatorReply::Query(result) => Ok(result),
            OperatorReply::Command(_) => {
                anyhow::bail!("the gateway answered a query with a command reply")
            }
        }
    }

    /// Run one state-changing operator command.
    pub async fn operator_command(
        &self,
        command: OperatorCommand,
    ) -> anyhow::Result<OperatorCommandResult> {
        match self.operator(OperatorRequest::Command(command)).await? {
            OperatorReply::Command(result) => Ok(result),
            OperatorReply::Query(_) => {
                anyhow::bail!("the gateway answered a command with a query reply")
            }
        }
    }

    /// The whole host-operator surface: one typed request, one typed reply, one
    /// endpoint. A minutes-long call (vault indexing, memory backfill) rides the
    /// unbounded client — the server bounds those, not the client.
    async fn operator(&self, request: OperatorRequest) -> anyhow::Result<OperatorReply> {
        let http = if request.is_long_running() {
            &self.streaming_http
        } else {
            &self.http
        };
        let resp = http
            .post(self.url("/api/operator"))
            .bearer_auth(&self.key)
            .json(&request)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "the running gateway doesn't serve `/api/operator` — it predates this command.\n\
                 Restart it onto the current binary (`komo gateway restart`) and retry."
            );
        }
        Ok(checked(resp).await?.json().await?)
    }

    // ---- client plane ------------------------------------------------------

    pub async fn status(&self) -> anyhow::Result<GatewayStatus> {
        self.get_field("/api/status", "channels")
            .await
            .map(|channels| GatewayStatus { channels })
    }

    pub async fn sessions(&self) -> anyhow::Result<Vec<SessionSummary>> {
        self.get_field("/api/sessions", "sessions").await
    }

    /// The operator's home conversation, opened on first ask. A local client
    /// starts here rather than minting its own id, which is what makes the TUI
    /// and a Telegram DM one thread (docs/bot-runtime.md §2 D6).
    pub async fn home_session(&self) -> anyhow::Result<String> {
        self.get_field("/api/home-session", "session").await
    }

    /// `/new`: draw a context boundary in `session`.
    pub async fn conversation_boundary(&self, session: &str) -> anyhow::Result<bool> {
        self.post_field(
            &format!("/api/sessions/{session}/boundary"),
            json!({}),
            "ok",
        )
        .await
    }

    /// `/workspace add <path>`: widen a task session's environment with another
    /// directory. The path must be absolute — the caller resolves a relative one
    /// against its own cwd, which the gateway cannot see.
    pub async fn add_session_root(
        &self,
        session: &str,
        path: &Path,
    ) -> anyhow::Result<Vec<String>> {
        self.post_field(
            &format!("/api/sessions/{session}/workspace"),
            json!({ "path": path.display().to_string() }),
            "roots",
        )
        .await
    }

    /// Transcript entries for one known session, used to hydrate a resumed TUI
    /// and to pick up a continuation another surface's answer set going.
    pub async fn session_messages(&self, id: &str) -> anyhow::Result<Vec<Message>> {
        let mut url = reqwest::Url::parse(&self.base)?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("gateway base URL cannot contain a path"))?
            .extend(["api", "sessions", id, "messages"]);
        let resp = self.http.get(url).bearer_auth(&self.key).send().await?;
        let mut map: Map<String, Value> = checked(resp).await?.json().await?;
        let messages = map
            .remove("messages")
            .context("gateway response missing `messages`")?;
        Ok(serde_json::from_value(messages)?)
    }

    /// What a suspended turn on this session is waiting on — the same read the
    /// GUI's approval modal and inline reply poll.
    pub async fn interactions(&self, session: &str) -> anyhow::Result<Interactions> {
        let resp = self
            .http
            .get(self.url(&format!("/api/interactions/{session}")))
            .bearer_auth(&self.key)
            .send()
            .await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// Resolve a pending approval: `"once"` | `"session"` | `"deny"`, the same
    /// decisions the GUI's modal posts.
    pub async fn resolve_approval(
        &self,
        session: &str,
        decision: &str,
        feedback: Option<String>,
    ) -> anyhow::Result<bool> {
        self.post_field(
            &format!("/api/interactions/{session}/approval"),
            json!({ "decision": decision, "feedback": feedback }),
            "resolved",
        )
        .await
    }

    /// Answer the question a suspended turn asked, and let it continue.
    pub async fn answer_question(&self, session: &str, text: &str) -> anyhow::Result<bool> {
        self.post_field(
            &format!("/api/interactions/{session}/answer"),
            json!({ "text": text }),
            "resolved",
        )
        .await
    }

    /// Stop the turn in flight on `session`. `true` when a turn was actually
    /// signalled; `false` when there was nothing to stop (it had already
    /// finished, or never started).
    ///
    /// One request covers all three ways a turn can be stuck: the endpoint
    /// denies a pending approval and answers a pending `ask_user` question
    /// before flipping the cancel signal. A turn parked on either of those would
    /// not observe the signal at all until it was resolved, so the order matters
    /// and is the server's to own — see `api::cancel_turn`.
    pub async fn cancel_turn(&self, session: &str) -> anyhow::Result<bool> {
        self.post_field(
            &format!("/api/interactions/{session}/cancel"),
            json!({}),
            "cancelled",
        )
        .await
    }

    /// Run one chat turn server-side, asking the gateway to **stream** it
    /// (`stream: true`), and invoke `on_event` for each live [`TurnEvent`] as it
    /// arrives; returns the final reply.
    ///
    /// `workspace` binds a **new** task session to the caller's startup
    /// directory; a session already bound to roots ignores it, and an unbound
    /// one (home) takes it for this turn alone.
    ///
    /// The gateway's `/v1/chat/completions` streams SSE frames: `event: tool`
    /// frames carry a JSON [`TurnEvent`]; the final default-event frame is an
    /// OpenAI-style `chat.completion.chunk` whose `delta.content` is the whole
    /// reply; a trailing `[DONE]` closes it (see `infra/messaging/api.rs`).
    pub async fn chat_streaming(
        &self,
        session_id: &str,
        message: &str,
        workspace: &str,
        mut on_event: impl FnMut(TurnEvent),
    ) -> anyhow::Result<String> {
        let body = json!({
            "model": "komo",
            "stream": true,
            "messages": [{ "role": "user", "content": message }],
        });
        let request = self
            .streaming_http
            .post(self.url("/v1/chat/completions"))
            .bearer_auth(&self.key)
            .header("X-Komo-Session-Id", session_id)
            .header("X-Komo-Trusted", "1")
            .header("X-Komo-Workspace", workspace);
        let mut resp = checked(request.json(&body).send().await?).await?;

        let mut reply = String::new();
        let mut buf = String::new();
        // SSE frames are separated by a blank line; read the body incrementally
        // (reqwest's `chunk()` needs no extra feature) and dispatch whole frames.
        while let Some(chunk) = resp.chunk().await? {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find("\n\n") {
                let frame: String = buf.drain(..pos + 2).collect();
                parse_sse_frame(&frame, &mut on_event, &mut reply);
            }
        }
        if !buf.trim().is_empty() {
            parse_sse_frame(&buf, &mut on_event, &mut reply);
        }
        Ok(reply)
    }
}

/// Bring a gateway up, or say who is supposed to.
#[cfg(target_os = "macos")]
fn start_gateway() -> anyhow::Result<()> {
    tracing::info!("no gateway is running — starting it");
    crate::cli::service::start()
}

#[cfg(not(target_os = "macos"))]
fn start_gateway() -> anyhow::Result<()> {
    anyhow::bail!(
        "no gateway is running, and komo's state lives in the gateway's process.\n\
         Start it with `komo gateway` (in Docker it is the container's main process)."
    )
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    #[test]
    fn folder_workspace_id_is_base64url_encoded() {
        let dir = std::env::temp_dir();
        let id = folder_workspace_id(&dir).unwrap();
        let encoded = id.strip_prefix("folder:").unwrap();
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .unwrap();
        assert_eq!(
            Path::new(std::str::from_utf8(&decoded).unwrap()),
            dir.canonicalize().unwrap()
        );
    }
}

/// Parse one SSE frame from the chat stream: dispatch a `TurnEvent` (on the
/// `tool` event) via `on_event`, or capture the final reply from a
/// `chat.completion.chunk` delta. `[DONE]` and anything unrecognized are
/// ignored. Multiple `data:` lines in a frame are joined with newlines per the
/// SSE spec (komo's payloads are single-line, but this stays spec-correct).
fn parse_sse_frame(frame: &str, on_event: &mut impl FnMut(TurnEvent), reply: &mut String) {
    let mut event_name = "message".to_string();
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event_name = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() || data == "[DONE]" {
        return;
    }
    if event_name == "tool" {
        if let Ok(event) = serde_json::from_str::<TurnEvent>(&data) {
            on_event(event);
        }
        return;
    }
    // Default event: the OpenAI-style chunk carrying the final reply text.
    if let Ok(v) = serde_json::from_str::<Value>(&data)
        && let Some(text) = v
            .pointer("/choices/0/delta/content")
            .and_then(|c| c.as_str())
    {
        *reply = text.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn from_info_returns_none_when_nothing_listening() {
        // Port 1 is privileged and (essentially) never has a listener → the
        // health probe fails fast and the caller starts a gateway instead.
        let info = GatewayInfo {
            pid: 0,
            bind: "127.0.0.1".into(),
            port: 1,
            key: "k".into(),
        };
        assert!(GatewayClient::from_info(info).await.is_none());
    }
}
