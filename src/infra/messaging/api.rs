//! HTTP API ingress channel — the gateway's only door.
//!
//! The gateway is the one process that opens komo's state, so every other
//! surface arrives here: the chat TUI, the desktop and web apps, the `komo`
//! CLI, and any OpenAI-compatible client (Open WebUI, LobeChat, …). Three
//! families of endpoints:
//!
//!   - **OpenAI-compatible** (`/v1/*`): `chat/completions` (streaming and not)
//!     and `models`, so third-party chat frontends connect by pointing at
//!     `http://127.0.0.1:8765/v1` with the bearer key.
//!   - **client** (`/api/*`): what a komo client reads and writes about a
//!     conversation — sessions, transcripts, memories, runs, models,
//!     workspaces, the pending approval/question, `status`.
//!   - **operator** (`POST /api/operator`): the whole host-operator surface as
//!     one typed request (`operator_control::request`), because a route per
//!     action meant a hand-written client method and handler per action.
//!
//! Unlike the chat channels, an HTTP request is synchronous request/response,
//! so it calls the [`MessageHandler`] directly and awaits the reply rather than
//! going through the spawn-and-return [`GatewayDispatcher`]. The turn runs in a
//! **non-interactive** session context ([`SessionContext::detached`]), so a tool
//! that needs approval is denied immediately — there is no human on an HTTP
//! request to answer a `/approve` prompt.
//!
//! Auth is a single bearer key (`API_SERVER_KEY`); the listener binds loopback
//! by default. `/health` is unauthenticated so a probe can check liveness.
//!
//! A **CORS layer** ([`cors_layer`]) allows browser clients whose page origin
//! isn't this port: the Electron renderer (a Vite dev origin in development,
//! `file://` — origin `null` — when packaged) and a cross-origin browser dev
//! server. Without it those fetches are blocked before auth ever runs, since a
//! request carrying `Authorization` is preflighted and the preflight would hit
//! the bearer-key middleware. Only loopback origins are allowed and credentials
//! are off, so the bearer key remains the sole thing that grants access.

use komo_bot::gateway::Channel;
use komo_bot::interaction::{Answer, ApprovalState, CancelState, GatewayDispatcher};
use komo_services::tool_execution::{SessionContext, with_session};
use std::path::PathBuf;
use std::sync::Arc;
use std::{convert::Infallible, time::Duration};

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{
        HeaderName, HeaderValue, Method, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    services::{ServeDir, ServeFile},
};
use tracing::{info, warn};

use crate::services::operator_control::{
    MemoryTransitionAction, OperatorCommand, OperatorCommandResult, OperatorQuery,
    OperatorQueryResult, OperatorReply, OperatorRequest, PairApproveOutcome,
    actions::{OperatorActions, TransitionOutcome, no_cron_job_message},
};
use komo_config::{ApiConfig, ModelEntry};
use komo_core::domain::{
    cancel::{CANCELLED_REPLY, is_cancelled},
    events::{ToolEventSink, TurnEvent},
    gateway::MessageHandler,
    memory::{MemoryStatus, parse_memory_status},
    pairing::ApproveOutcome,
    session::Session,
    wakeup::{SUSPENDED_REPLY, is_suspended},
};
use std::net::SocketAddr;

/// What the HTTP transport itself needs, cheaply cloned per request (all
/// `Arc`): the bearer key, the chat handler, the operator use cases, and the
/// two `/api/status` facts. Operator behavior lives in [`OperatorActions`] —
/// this is transport state, not a dependency list.
#[derive(Clone)]
struct AppState {
    api_key: Arc<String>,
    /// The main runtime's tool catalog — read per request, so `/health` reports
    /// what is mounted *now* (python plugins mount and unmount live).
    tools: Arc<komo_core::domain::catalog::ToolCatalog>,
    handler: Arc<dyn MessageHandler>,
    /// The gateway's turn arbiter. An HTTP turn takes the same per-session slot
    /// a chat turn takes (`claim_session`), so "one turn per session" holds
    /// across ingresses. Without it two clients on one session — a second TUI
    /// resuming it, the desktop app beside the terminal — run concurrent turns,
    /// and the later one assembles its history before the earlier one has
    /// written its answer, so it re-runs work that is still in progress.
    dispatcher: Arc<GatewayDispatcher>,
    actions: Arc<OperatorActions>,
    /// Channel names enabled on this gateway (for `/api/status`).
    channels: Arc<Vec<String>>,
    /// Resolved config `home_chat` fallback, if any (for `/api/status`).
    home: Option<String>,
    /// Resolved main model identity (safe, non-secret status metadata).
    provider: Arc<String>,
    model: Arc<String>,
    /// The models a client may switch a session to, each carrying its own
    /// provider and reasoning-effort levels — with a cross-provider menu those
    /// differ per entry (codex has three levels, deepseek none). Advertised over
    /// `GET /api/models`; the chat path validates a request against them so an
    /// unknown id falls back to `model` instead of reaching a provider.
    models: Arc<Vec<ModelEntry>>,
    /// Shared with the gateway dispatcher and the `ChatApprover`: lets a
    /// loopback interactive HTTP turn (the GUI) surface a pending approval over
    /// `GET /api/interactions/{session}` and resolve it over `POST`.
    approvals: Arc<ApprovalState>,
    /// Cancellation slots for in-flight interruptible turns, keyed by session.
    /// Owned here (not shared with the gateway dispatcher) because the api
    /// channel is the only surface with a stop affordance.
    cancels: Arc<CancelState>,
    /// Allow keyed remote (non-loopback) callers to run interactive turns and
    /// resolve approval/clarify prompts. Off by default — those paths assume a
    /// host operator behind a loopback socket. `X-Komo-Trusted` (auto-approve)
    /// stays loopback-only regardless of this flag.
    remote_interactive: bool,
    /// Server-owned workspace catalog exposed to the authenticated local GUI.
    workspace_home: Arc<PathBuf>,
    /// Process workspace used when the UI selects the default entry.
    default_workspace: Arc<PathBuf>,
}

/// The model identity plus the switchable menu the api advertises.
///
/// Derived once from the resolved [`ModelConfig`] so `/api/status`,
/// `/api/models`, and the chat path's validation can never disagree about what
/// is on offer. Everything here is non-secret status metadata.
pub struct ModelMenu {
    /// The default provider's name (status metadata; each entry names its own).
    provider: String,
    default_model: String,
    /// Selectable models, the configured default first (always non-empty). Each
    /// entry carries its provider and that provider's effort levels.
    models: Vec<ModelEntry>,
}

impl ModelMenu {
    pub fn from_config(config: &komo_config::ModelConfig) -> Self {
        Self {
            provider: config.provider.name().to_string(),
            default_model: config.model.clone(),
            // `menu()` drops entries whose provider has no usable credential, so
            // the UI never offers a model that would error on every turn.
            models: config.menu(),
        }
    }
}

/// The HTTP API channel. Holds the listen config and the shared handler state.
pub struct ApiChannel {
    bind: String,
    port: u16,
    /// Optional built web SPA served same-origin (static public, api key-gated).
    web_dir: Option<String>,
    state: AppState,
}

impl ApiChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &ApiConfig,
        handler: Arc<dyn MessageHandler>,
        dispatcher: Arc<GatewayDispatcher>,
        actions: Arc<OperatorActions>,
        tools: Arc<komo_core::domain::catalog::ToolCatalog>,
        channels: Vec<String>,
        home: Option<String>,
        models: ModelMenu,
        approvals: Arc<ApprovalState>,
        workspace_home: PathBuf,
        default_workspace: PathBuf,
    ) -> Self {
        Self {
            bind: config.bind.clone(),
            port: config.port,
            web_dir: config.web_dir.clone(),
            state: AppState {
                api_key: Arc::new(config.server_key.clone()),
                handler,
                dispatcher,
                actions,
                tools,
                channels: Arc::new(channels),
                home,
                provider: Arc::new(models.provider),
                model: Arc::new(models.default_model),
                models: Arc::new(models.models),
                approvals,
                cancels: Arc::new(CancelState::new()),
                remote_interactive: config.remote_interactive,
                workspace_home: Arc::new(workspace_home),
                default_workspace: Arc::new(default_workspace),
            },
        }
    }
}

#[async_trait]
impl Channel for ApiChannel {
    fn name(&self) -> &str {
        "api"
    }

    async fn serve(
        &self,
        _dispatcher: Arc<GatewayDispatcher>,
        mut shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let addr = format!("{}:{}", self.bind, self.port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        // With an ephemeral bind (port 0, the loopback-only default) the real
        // port is only known after bind — read it back and advertise it.
        let local = listener.local_addr()?;
        info!(addr = %local, "api channel listening");
        // Publish how to reach this gateway so the local `komo` CLI can route
        // to it instead of opening the db (which Turso's exclusive lock forbids
        // while the gateway holds it). Removed again on graceful shutdown.
        crate::infra::rendezvous::write(&crate::infra::rendezvous::GatewayInfo {
            pid: std::process::id(),
            bind: self.bind.clone(),
            port: local.port(),
            key: self.state.api_key.as_ref().clone(),
        });
        let app = build_router(self.state.clone(), self.web_dir.as_deref());
        let graceful = async move {
            let _ = shutdown.changed().await;
        };
        // `into_make_service_with_connect_info` so handlers can see the peer
        // address — the trusted-chat path is gated to loopback callers.
        let result = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(graceful)
        .await;
        crate::infra::rendezvous::clear();
        result?;
        info!("api channel stopped");
        Ok(())
    }
}

/// Is this `Origin` a local page? Loopback hosts on any port, plus the opaque
/// `null` origin a packaged Electron renderer sends from `file://`.
fn is_local_origin(origin: &HeaderValue) -> bool {
    let Ok(text) = origin.to_str() else {
        return false;
    };
    // Packaged Electron loads the renderer over file://, whose origin is opaque.
    if text == "null" {
        return true;
    }
    let Some(host_port) = text
        .strip_prefix("http://")
        .or_else(|| text.strip_prefix("https://"))
    else {
        return false;
    };
    // Bracketed IPv6 (`[::1]:5273`) keeps its colons inside the brackets.
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        match rest.split_once(']') {
            Some((host, _)) => host,
            None => return false,
        }
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    if host == "localhost" {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// CORS for local browser clients. Applied as the **outermost** layer so a
/// preflight is answered here and never reaches [`require_auth`] (which would
/// 401 it, blocking the real request that follows).
///
/// Deliberately narrow: loopback/`null` origins only, no credentials (the key
/// travels in a header, not a cookie), and the small fixed set of headers this
/// API reads. A page on the open web gets no CORS grant — and even a local one
/// still needs the bearer key.
fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _parts| {
            is_local_origin(origin)
        }))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            AUTHORIZATION,
            CONTENT_TYPE,
            HeaderName::from_static("x-komo-session-id"),
            HeaderName::from_static("x-komo-trusted"),
            HeaderName::from_static("x-komo-interactive"),
            HeaderName::from_static("x-komo-workspace"),
            HeaderName::from_static("x-komo-model"),
            HeaderName::from_static("x-komo-effort"),
        ])
        .max_age(std::time::Duration::from_secs(600))
}

/// Build the router: `/health` and (if configured) the static web SPA are
/// public; everything else sits behind the bearer-key middleware.
fn build_router(state: AppState, web_dir: Option<&str>) -> Router {
    // Host-operator actions: the whole `komo` CLI behind `/api/operator`, plus
    // the writes a *client* performs on its own (memory governance, session
    // rename/archive, `/new`, dreaming). Loopback-gated as a *layer*, not
    // per-handler checks, so a write route added here is gated by construction
    // — a publicly-bound api (`[channels.api] enabled = true`) never reaches
    // these, valid key or not.
    let operator_writes = Router::new()
        // The whole host-operator surface — every `komo` subcommand that reads
        // or writes komo's state — behind one typed endpoint.
        .route("/api/operator", post(operator))
        .route("/api/memories/{id}/promote", post(memory_promote))
        .route("/api/memories/{id}/reject", post(memory_reject))
        .route("/api/sessions/{id}/title", post(set_session_title))
        .route("/api/sessions/{id}/status", post(set_session_status))
        .route("/api/sessions/{id}/boundary", post(conversation_boundary))
        .route("/api/sessions/{id}/workspace", post(add_session_root))
        .route("/api/dream/apply", post(dream_apply))
        .route_layer(middleware::from_fn(require_loopback));

    // Interactive resolution (the GUI's approval modal + clarify answer). Always
    // allowed over loopback; reachable by keyed remote callers only when
    // `remote_interactive` is set (a remote GUI resolving its own prompts). The
    // `require_auth` layer below still applies via the merge into `protected`.
    let interactive_writes = Router::new()
        .route(
            "/api/interactions/{session}/approval",
            post(resolve_approval),
        )
        .route("/api/interactions/{session}/answer", post(answer_question))
        .route("/api/interactions/{session}/cancel", post(cancel_turn))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_interactive_access,
        ));

    let protected = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/api/status", get(status))
        .route("/api/models", get(list_model_menu))
        .route("/api/workspaces", get(list_workspaces))
        .route("/api/home-session", get(get_home_session))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}/messages", get(session_messages))
        .route("/api/memories", get(list_memories))
        .route("/api/runs", get(list_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/dream", get(dream_preview))
        .route("/api/interactions/{session}", get(get_interactions))
        .merge(operator_writes)
        .merge(interactive_writes)
        .route_layer(middleware::from_fn_with_state(
            state.api_key.clone(),
            require_auth,
        ));

    let mut router = Router::new().route("/health", get(health)).merge(protected);

    // Serve the built web SPA same-origin as the unauthenticated fallback: the
    // bundle isn't secret (the key it then uses is), and same-origin means no
    // CORS. Unknown non-API paths fall back to index.html for the SPA. `/api`
    // and `/v1` are matched routes above, so they never reach this fallback.
    if let Some(dir) = web_dir {
        let index = std::path::Path::new(dir).join("index.html");
        router = router.fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)));
    }

    // Outermost: preflights are answered before auth/loopback gating runs.
    router.layer(cors_layer()).with_state(state)
}

/// Gate the interactive-resolution endpoints: always allow loopback (the local
/// GUI / CLI); allow keyed remote callers only when `remote_interactive` is on.
/// Auth is enforced separately by the `require_auth` layer.
async fn require_interactive_access(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if peer.ip().is_loopback() || state.remote_interactive {
        return next.run(req).await;
    }
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "interactive endpoints are loopback-only \
                      (set [channels.api] remote_interactive = true to allow keyed remote access)"
        })),
    )
        .into_response()
}

/// Reject any control-plane write not arriving over loopback. These are
/// host-operator actions — like the trusted chat path, they must be unreachable
/// on an external bind regardless of the bearer key.
async fn require_loopback(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if !peer.ip().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "operator write endpoints are loopback-only" })),
        )
            .into_response();
    }
    next.run(req).await
}

/// Reject any request whose `Authorization: Bearer <key>` does not match.
///
/// Takes the key rather than the whole [`AppState`]: it is the only thing this
/// decides on, and a middleware's state is independent of the router's, so the
/// narrow value is also what lets the gate be exercised without standing up a
/// dispatcher and a database.
async fn require_auth(
    State(key): State<Arc<String>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if bearer_matches(token, key.as_str()) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Constant-time bearer-token check. Both sides are SHA-256'd to a fixed-size
/// digest (so neither length nor content leaks) and compared with the shared
/// constant-time primitive (`domain::pairing::ct_eq`) — a plain `==` on the
/// tokens would let a timing side-channel probe the key byte by byte when the
/// api channel is bound externally (`[channels.api] enabled = true`), where
/// the key is the only auth.
fn bearer_matches(presented: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    let digest_hex = |s: &str| -> String {
        Sha256::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    komo_core::domain::pairing::ct_eq(&digest_hex(presented), &digest_hex(expected))
}

/// Maps a handler error to a JSON body — a 500 unless the handler says
/// otherwise. A caller's own malformed input is a 400: reporting it as a server
/// fault puts the client into a retry loop over the one thing only it can fix.
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    error: anyhow::Error,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: anyhow::anyhow!(message.into()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // `{:#}` renders the whole context chain. The outermost line alone is
        // the *least* specific thing known about the failure — "the embedding
        // backend is not reachable" hides the "no route to host" underneath
        // that says which kind of unreachable it was.
        let message = format!("{:#}", self.error);
        // A rejected request is the caller's business, not an incident.
        if self.status.is_server_error() {
            warn!(error = %message, "api request failed");
        }
        (self.status, Json(json!({ "error": message }))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: error.into(),
        }
    }
}

// ---- OpenAI-compatible endpoints -------------------------------------------

#[derive(Deserialize)]
struct ChatCompletionRequest {
    #[serde(default)]
    model: String,
    #[serde(default)]
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
}

#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: String,
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    // Plugin state rides along so `komo doctor` can tell "not enabled" from
    // "enabled but the running gateway predates the plugins directory" — the
    // second is invisible from the filesystem alone. Counts only, no names:
    // /health answers unauthenticated.
    let snapshot = state.tools.snapshot();
    let python = snapshot.get("python").is_some();
    let plugin_tools = snapshot.names().filter(|n| n.starts_with("py__")).count();
    Json(json!({
        "status": "ok",
        "version": crate::cli::VERSION,
        "plugins": { "python": python, "tools": plugin_tools },
    }))
}

async fn list_models() -> impl IntoResponse {
    Json(json!({
        "object": "list",
        "data": [{
            "id": "komo",
            "object": "model",
            "created": 0,
            "owned_by": "komo",
        }],
    }))
}

async fn chat_completions(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<Response, ApiError> {
    let (session_id, stateful) = resolve_session(&headers)?;
    let input = build_input(&req.messages, stateful);
    let model = if req.model.is_empty() {
        "komo".to_string()
    } else {
        req.model.clone()
    };

    let is_loopback = peer.ip().is_loopback();
    // Workspace selection is local-operator-only, like trusted mode: the client
    // sends an opaque id, resolved either through the gateway's own catalog or —
    // for a folder the desktop shell picked via the native dialog — decoded from
    // it. A remote caller can therefore never widen its filesystem root.
    let requested = is_loopback
        .then(|| requested_workspace_root(&state, &headers))
        .flatten();
    let roots = open_session(&state, &session_id, requested).await?;

    // The model / effort choice travels with the session rather than with the
    // turn: a conversation may switch models mid-thread. The client sends its
    // current selection on every turn and we persist it — the turn itself reads
    // it back off the session row (see `infra::llm::RigLlm::agent_for`), which
    // is also what makes it visible to another client opening the same
    // conversation.
    if let Some(selection) = requested_model(&state.models, &state.model, &headers) {
        state
            .actions
            .sessions
            .set_model(&session_id, &selection.model, &selection.effort)
            .await?;
    }

    // Loopback callers may opt into one of two richer contexts (both ignored on
    // an external bind, where there is no host operator behind the socket):
    //   - `X-Komo-Trusted`: auto-approve side-effecting tools (the CLI user is
    //     the host operator — this is what the local TUI uses).
    //   - `X-Komo-Interactive`: prompt for approval / clarify and suspend the
    //     turn, exactly like a chat channel, but resolved out-of-band over the
    //     `/api/interactions/*` endpoints (this is what the GUI uses). The reply
    //     sink is a no-op — the GUI reads the pending prompt by polling.
    // Trusted wins over interactive if a caller somehow sets both; anyone else
    // gets the detached (auto-deny) context.
    // Trusted (auto-approve) is loopback-only; interactive may also be granted to
    // keyed remote callers when `remote_interactive` is configured (they resolve
    // approvals/clarify out-of-band, same as the local GUI).
    let trusted = is_loopback && headers.contains_key("x-komo-trusted");
    let interactive =
        (is_loopback || state.remote_interactive) && headers.contains_key("x-komo-interactive");
    let mut ctx = if trusted {
        SessionContext::trusted(&session_id)
    } else if interactive {
        SessionContext::interactive_http(&session_id)
    } else {
        SessionContext::detached(&session_id)
    };
    ctx = ctx.with_workspace_roots(roots);
    let id = format!("chatcmpl-{}", uuid::Uuid::now_v7());
    let created = now();

    if req.stream {
        // Live path: run the turn on a spawned task and stream tool-call events
        // (started/finished) as they happen, then the final reply + [DONE]. The
        // reply itself isn't token-incremental (rig's tool loop has no token
        // stream) — this streams the *tool-call process*, which is the point.
        Ok(stream_turn(
            state.handler.clone(),
            state.dispatcher.clone(),
            state.cancels.clone(),
            ctx,
            session_id,
            input,
            id,
            created,
            model,
        ))
    } else {
        // Synchronous: take the session's turn slot, drive the turn, await the
        // full reply. The claim waits out any turn already running on this
        // session — a chat channel's or another HTTP caller's — so this turn
        // reads a history that already holds the previous answer instead of
        // starting the same work over.
        // Every api turn is interruptible: the caller holds the connection, so
        // it is the one surface with somewhere to put a stop button. Registered
        // **before** the slot is claimed — the wait for it is unbounded, and a
        // caller Stop cannot reach while it waits is one that then runs the very
        // work the user stopped. Retired when the ticket drops.
        let ticket = state.cancels.register(&session_id);
        let ctx = ctx.with_cancel(ticket.signal());
        let claim = tokio::select! {
            claim = state.dispatcher.claim_session(&session_id) => Some(claim),
            // Stopped while queued: answer now rather than wait out a turn whose
            // successor the caller no longer wants.
            () = ticket.cancelled() => None,
        };
        let reply = match claim {
            Some(claim) => {
                let reply = with_session(ctx, state.handler.handle(&session_id, input)).await;
                drop(ticket);
                claim.release();
                match reply {
                    Err(error) if is_cancelled(&error) => CANCELLED_REPLY.to_string(),
                    // The turn stopped for an approval and gave up its slot.
                    // Not an error: the prompt is with whoever can answer it,
                    // and the turn continues once they do — its reply lands in
                    // the transcript, which is where this caller reads it from
                    // anyway.
                    Err(error) if is_suspended(&error) => SUSPENDED_REPLY.to_string(),
                    other => other?,
                }
            }
            None => CANCELLED_REPLY.to_string(),
        };
        Ok(Json(json!({
            "id": id,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": reply },
                "finish_reason": "stop",
            }],
        }))
        .into_response())
    }
}

/// The `X-Komo-Workspace` id for "the gateway's own startup directory". Header
/// vocabulary, not a stored value: a session records the *path* it is bound to,
/// or nothing at all.
const DEFAULT_WORKSPACE: &str = "__default__";

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceEntry {
    id: String,
    name: String,
    /// Canonicalized, so the path a client matches a session's `roots[0]`
    /// against is the same string the binding wrote.
    path: PathBuf,
}

fn workspace_entries(state: &AppState) -> Vec<WorkspaceEntry> {
    let mut entries = Vec::new();
    if let Some(path) = canonical_dir(state.default_workspace.as_ref()) {
        entries.push(WorkspaceEntry {
            id: DEFAULT_WORKSPACE.to_string(),
            name: path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or("默认 workspace")
                .to_string(),
            path,
        });
    }
    let catalogued = entries.len();
    if let Ok(children) = std::fs::read_dir(state.workspace_home.as_ref()) {
        for child in children.flatten() {
            let Some(path) = canonical_dir(&child.path()) else {
                continue;
            };
            let Some(id) = child.file_name().to_str().map(str::to_string) else {
                continue;
            };
            // Header values are deliberately conservative. The path itself is
            // always discovered server-side and is never copied from the id.
            if id.is_empty()
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            {
                continue;
            }
            entries.push(WorkspaceEntry {
                name: id.clone(),
                id,
                path,
            });
        }
    }
    entries[catalogued..].sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// One normalization for every path that reaches a client or a `roots` entry:
/// canonicalized, and a directory. Anything else is skipped rather than offered
/// in a form nothing else will match.
fn canonical_dir(path: &std::path::Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    canonical.is_dir().then_some(canonical)
}

fn resolve_workspace_id(state: &AppState, id: &str) -> Option<PathBuf> {
    let catalogued = workspace_entries(state)
        .into_iter()
        .find_map(|entry| (entry.id == id).then_some(entry.path));
    if catalogued.is_some() {
        return catalogued;
    }
    // Not in the catalog: the desktop shell may have attached a folder the
    // operator chose through the native OS dialog. Only the loopback branch
    // above calls this, so widening the root stays a host-operator act — the
    // same boundary that grants `X-Komo-Trusted`.
    resolve_folder_workspace(id)
}

/// A validated per-session model selection. Either field may be empty, meaning
/// "back to the gateway/provider default".
struct ModelSelection {
    model: String,
    effort: String,
}

/// Read `X-Komo-Model` / `X-Komo-Effort` off a chat request, validated against
/// what this gateway actually advertises.
///
/// `None` = neither header present, so the session keeps whatever it already
/// stored (an OpenAI-compatible client that knows nothing about these headers
/// must not silently reset a conversation's model). Present-but-unknown values
/// resolve to empty — i.e. the default — rather than being forwarded verbatim,
/// so a stale UI or a typo can't push a bogus model id at a provider.
///
/// Effort is validated against **the model that will actually run**, not against
/// a gateway-wide list: switching a session to a provider with no effort scale
/// clears a stale level instead of storing one that silently does nothing.
fn requested_model(
    models: &[ModelEntry],
    default_model: &str,
    headers: &axum::http::HeaderMap,
) -> Option<ModelSelection> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    let model = header("x-komo-model");
    let effort = header("x-komo-effort");
    if model.is_none() && effort.is_none() {
        return None;
    }
    // Empty = "run the gateway default", which is a legitimate selection.
    let chosen = model
        .filter(|want| models.iter().any(|entry| entry.id == *want))
        .unwrap_or_default();
    let effective = if chosen.is_empty() {
        default_model
    } else {
        chosen
    };
    let allowed = models
        .iter()
        .find(|entry| entry.id == effective)
        .map_or(&[][..], |entry| entry.efforts);
    Some(ModelSelection {
        model: chosen.to_string(),
        effort: effort
            .filter(|level| allowed.contains(level))
            .unwrap_or_default()
            .to_string(),
    })
}

/// The directory a loopback caller asked this turn to run in, resolved
/// server-side from the opaque `X-Komo-Workspace` id. `None` when the header is
/// absent or names nothing that resolves — the caller falls back to the process
/// workspace, never to a path it typed.
fn requested_workspace_root(state: &AppState, headers: &axum::http::HeaderMap) -> Option<PathBuf> {
    let id = headers
        .get("x-komo-workspace")
        .and_then(|value| value.to_str().ok())
        .unwrap_or(DEFAULT_WORKSPACE);
    resolve_workspace_id(state, id)
}

/// What a turn does with the workspace it was offered, given the session row it
/// lands on.
#[derive(Debug, PartialEq, Eq)]
struct WorkspacePlan {
    /// Roots to write onto the row being created. Empty = an unbound session.
    bind: Vec<PathBuf>,
    /// Roots this turn's tools confine to. Empty = the process workspace.
    turn: Vec<PathBuf>,
}

/// **One task is one session, and its workspace is the task's environment.**
///
/// A task session is bound on its first turn and honored on every later one, so
/// running `komo resume` from another directory continues the task rather than
/// silently moving where its tools write; widening is the explicit
/// `/workspace add`. Everything else stays unbound and takes the caller's root
/// per turn: home is entered from wherever the operator is standing
/// (docs/bot-runtime.md §2 D6), and a row written before this existed must not
/// be bound by whichever directory happened to send its next message.
///
/// `existing` is the row's roots (`None` = no row yet); `requested` is the
/// header-resolved directory, already `None` for a caller not entitled to one.
fn plan_workspace(
    existing: Option<&[String]>,
    requested: Option<PathBuf>,
    is_home: bool,
) -> WorkspacePlan {
    if let Some(bound) = existing.filter(|roots| !roots.is_empty()) {
        return WorkspacePlan {
            bind: Vec::new(),
            turn: bound.iter().map(PathBuf::from).collect(),
        };
    }
    let turn: Vec<PathBuf> = requested.into_iter().collect();
    let binds = existing.is_none() && !is_home;
    WorkspacePlan {
        bind: if binds { turn.clone() } else { Vec::new() },
        turn,
    }
}

/// Make sure the session row exists before the turn lands on it, and answer
/// which roots the turn runs in — see [`plan_workspace`].
async fn open_session(
    state: &AppState,
    session_id: &str,
    requested: Option<PathBuf>,
) -> Result<Vec<PathBuf>, ApiError> {
    let existing = state.actions.sessions.find(session_id).await?;
    // Asked only when a row has to be created, which is once per conversation:
    // the home id is a stored setting, minted on first ask.
    let is_home = match &existing {
        Some(_) => false,
        None => state.actions.home_session().await? == session_id,
    };
    let plan = plan_workspace(
        existing.as_ref().map(|s| s.roots.as_slice()),
        requested,
        is_home,
    );
    if existing.is_none() {
        let roots = plan.bind.iter().map(|p| p.display().to_string()).collect();
        state
            .actions
            .sessions
            .save(&Session::with_roots(session_id, roots))
            .await?;
    }
    Ok(plan.turn)
}

/// Decode a `folder:<base64url path>` workspace id into an existing directory.
///
/// The opaque encoding is what lets an arbitrary Unicode path ride in an
/// ASCII-only header. Canonicalizing before the `is_dir` check means a
/// nonexistent path, a file, or anything that doesn't decode yields `None`, and
/// the caller falls back to the process workspace.
fn resolve_folder_workspace(id: &str) -> Option<PathBuf> {
    let encoded = id.strip_prefix("folder:")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()?;
    canonical_dir(&PathBuf::from(String::from_utf8(bytes).ok()?))
}

/// What a session may be switched to: the model menu (default first, each with
/// its best-known context window) and the provider's reasoning-effort levels.
/// An empty `efforts` means this provider has no effort knob — the client should
/// say so rather than offer a switch.
async fn list_model_menu(State(state): State<AppState>) -> Json<Value> {
    let models = state
        .models
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "provider": entry.provider.name(),
                "model": entry.model,
                "context_window": model_context_window(&entry.model),
                // Per entry, not per gateway: a cross-provider menu mixes models
                // that have an effort scale with ones that don't.
                "efforts": entry.efforts,
            })
        })
        .collect::<Vec<_>>();
    Json(json!({
        "provider": state.provider.as_ref(),
        "default_model": state.model.as_ref(),
        "models": models,
    }))
}

async fn list_workspaces(State(state): State<AppState>) -> Json<Value> {
    let workspaces = workspace_entries(&state)
        .into_iter()
        .map(|entry| json!({ "id": entry.id, "name": entry.name, "path": entry.path }))
        .collect::<Vec<_>>();
    Json(json!({ "workspaces": workspaces }))
}

/// One item on the SSE stream for a streaming turn.
enum SseMsg {
    /// A live tool-call event (emitted from the executor via the sink).
    Tool(TurnEvent),
    /// The turn's final assistant reply (or an error rendered as text).
    Final(String),
}

/// State for the SSE `unfold`: draining the channel, then a one-shot `[DONE]`.
enum SseState {
    Live(mpsc::UnboundedReceiver<SseMsg>),
    Done,
}

/// [`ToolEventSink`] that forwards each `TurnEvent` onto the SSE channel.
struct ChannelEventSink {
    tx: mpsc::UnboundedSender<SseMsg>,
}

impl ToolEventSink for ChannelEventSink {
    fn emit(&self, event: TurnEvent) {
        // Best-effort: if the client hung up, the receiver is gone — drop it.
        let _ = self.tx.send(SseMsg::Tool(event));
    }
}

/// Run the turn on a spawned task and return an SSE response that streams live
/// tool-call events as they happen, then the final reply and `[DONE]`.
///
/// Tool events go out as SSE frames with `event: tool` and a JSON `TurnEvent`
/// body; the final reply goes out as an OpenAI-style `chat.completion.chunk`
/// (default `message` event) carrying the whole text with `finish_reason:stop`.
/// The reply is not token-incremental — rig's tool loop has no token stream —
/// so this streams the tool-call process, not the assistant text.
#[allow(clippy::too_many_arguments)]
fn stream_turn(
    handler: Arc<dyn MessageHandler>,
    dispatcher: Arc<GatewayDispatcher>,
    cancels: Arc<CancelState>,
    ctx: SessionContext,
    session_id: String,
    input: String,
    id: String,
    created: i64,
    model: String,
) -> Response {
    let (tx, rx) = mpsc::unbounded_channel::<SseMsg>();
    // Attach the event sink so the executor emits tool events onto the channel.
    let ctx = ctx.with_event_sink(Arc::new(ChannelEventSink { tx: tx.clone() }));

    // Drive the turn; on completion push the final reply, then drop every sender
    // (this `tx` and the sink's clone inside `ctx`) so the receiver closes.
    tokio::spawn(async move {
        // Claimed here rather than in the request handler so the SSE response
        // starts at once: a caller queued behind a long turn sees keepalives
        // and a live connection instead of a stalled request. See
        // `GatewayDispatcher::claim_session` for why the wait exists at all.
        // Registered before the claim, for the same reason as the synchronous
        // path: a caller queued behind a long turn is a caller Stop has to
        // reach.
        let ticket = cancels.register(&session_id);
        let ctx = ctx.with_cancel(ticket.signal());
        let claim = tokio::select! {
            claim = dispatcher.claim_session(&session_id) => Some(claim),
            () = ticket.cancelled() => None,
        };
        let Some(claim) = claim else {
            let _ = tx.send(SseMsg::Final(CANCELLED_REPLY.to_string()));
            return;
        };
        let outcome = with_session(ctx, handler.handle(&session_id, input)).await;
        drop(ticket);
        claim.release();
        let final_msg = match outcome {
            Ok(text) => text,
            // A cancel is the caller's own doing, not a failure to report as one.
            Err(error) if is_cancelled(&error) => CANCELLED_REPLY.to_string(),
            Err(error) if is_suspended(&error) => SUSPENDED_REPLY.to_string(),
            Err(error) => format!("请求失败：{error:#}"),
        };
        let _ = tx.send(SseMsg::Final(final_msg));
    });

    let stream = futures_util::stream::unfold(SseState::Live(rx), move |state| {
        let id = id.clone();
        let model = model.clone();
        async move {
            match state {
                SseState::Live(mut rx) => match rx.recv().await {
                    Some(SseMsg::Tool(event)) => {
                        let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
                        let ev = Event::default().event("tool").data(data);
                        Some((Ok::<Event, Infallible>(ev), SseState::Live(rx)))
                    }
                    Some(SseMsg::Final(text)) => {
                        let chunk = json!({
                            "id": id,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": { "role": "assistant", "content": text },
                                "finish_reason": "stop",
                            }],
                        });
                        let ev = Event::default().data(chunk.to_string());
                        Some((Ok::<Event, Infallible>(ev), SseState::Live(rx)))
                    }
                    None => Some((
                        Ok::<Event, Infallible>(Event::default().data("[DONE]")),
                        SseState::Done,
                    )),
                },
                SseState::Done => None,
            }
        }
    });
    // A turn can spend minutes in one completion before it emits a tool event.
    // Keep the response alive through that quiet period so a healthy long turn
    // is not mistaken for an idle connection by the TUI or an intermediary.
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response()
}

/// Continue an existing conversation only when the client opts in with
/// `X-Komo-Session-Id`. Without it, mint an ephemeral session so no server-side
/// history accrues — the client manages its own context.
///
/// The header **must be a UUID**, and is used verbatim. It used to be wrapped in
/// an `api:` namespace, which every client then stripped back off — but that
/// wrapper was doing one thing nobody had written down: keeping a caller from
/// addressing another ingress's session. Without it a client could send
/// `feishu:oc_x` and have its turn evaluated against that channel's permission
/// scope and write into its memory scope. Requiring a UUID is what replaces the
/// wrapper, and is why a rejected header is a 400 rather than a fresh session:
/// silently answering somewhere else is how a client loses a conversation.
fn resolve_session(headers: &axum::http::HeaderMap) -> Result<(String, bool), ApiError> {
    let Some(id) = headers
        .get("x-komo-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    else {
        return Ok((uuid::Uuid::now_v7().to_string(), false));
    };
    if uuid::Uuid::parse_str(id).is_err() {
        return Err(ApiError::bad_request(
            "X-Komo-Session-Id must be a UUID (komo mints one when the header is absent)",
        ));
    }
    Ok((id.to_string(), true))
}

/// Reduce the OpenAI `messages` array to one input string for the turn.
///
/// Stateful (header given): the agent already has its history in the db, so we
/// pass only the latest user message. Stateless: the client owns the history,
/// so we flatten the whole exchange into the single ephemeral turn.
fn build_input(messages: &[ChatMessage], stateful: bool) -> String {
    if stateful {
        messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .or_else(|| messages.last())
            .map(|m| m.content.clone())
            .unwrap_or_default()
    } else {
        messages
            .iter()
            .filter(|m| !m.content.trim().is_empty())
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

// ---- dashboard endpoints ---------------------------------------------------

async fn status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let sessions = state.actions.session_summaries().await?.len();
    Ok(Json(json!({
        "ok": true,
        "version": crate::cli::VERSION,
        "channels": state.channels.as_ref(),
        "home_chat": state.home,
        "provider": state.provider.as_ref(),
        "model": state.model.as_ref(),
        "context_window": model_context_window(state.model.as_ref()),
        // The provider adapters do not currently expose per-turn token usage.
        "token_usage": Value::Null,
        "sessions": sessions,
    })))
}

/// Best-known capacities for model families supported by Komo. Unknown model
/// ids deliberately return null rather than showing an invented limit.
fn model_context_window(model: &str) -> Option<u64> {
    let model = model.to_ascii_lowercase();
    if model.starts_with("gpt-4.1") {
        Some(1_047_576)
    } else if model.starts_with("gpt-5") || model.contains("codex") {
        Some(400_000)
    } else if model.starts_with("claude-") {
        Some(200_000)
    } else if model.starts_with("gemini-2.5") || model.starts_with("gemini-3") {
        Some(1_048_576)
    } else if model.starts_with("deepseek") {
        Some(128_000)
    } else {
        None
    }
}

/// The operator's home conversation, opened on first ask. What a local client
/// starts in, so the TUI and a Telegram DM are one thread rather than two.
async fn get_home_session(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({ "session": state.actions.home_session().await? }),
    ))
}

/// `/new`: draw a context boundary in this conversation.
async fn conversation_boundary(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state.actions.conversation_boundary(&id).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn list_sessions(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    // Summaries only — never dump every transcript in a list view.
    let sessions = state.actions.session_summaries().await?;
    Ok(Json(json!({ "sessions": sessions })))
}

async fn session_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let messages = state.actions.session_messages(&id).await?;
    Ok(Json(json!({ "session_id": id, "messages": messages })))
}

#[derive(Deserialize)]
struct MemoryQueryParams {
    status: Option<String>,
}

async fn list_memories(
    State(state): State<AppState>,
    Query(params): Query<MemoryQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let status: Option<MemoryStatus> = params
        .status
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(parse_memory_status);
    // Memory derives Serialize, so it serializes verbatim.
    let memories = state.actions.list_memories(status).await?;
    Ok(Json(json!({ "memories": memories })))
}

// Memory governance writes (`komo memory promote/reject` while the gateway
// holds the db lock). Host-operator actions — loopback-gated by the
// `require_loopback` layer on the operator-writes router.

async fn memory_promote(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    memory_transition(&state, &id, MemoryTransitionAction::Promote).await
}

async fn memory_reject(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    memory_transition(&state, &id, MemoryTransitionAction::Reject).await
}

/// Apply one governance transition (the shared operator definition — the
/// domain owns the semantics) and return the updated memory. 404 on an
/// unknown id.
async fn memory_transition(
    state: &AppState,
    id: &str,
    action: MemoryTransitionAction,
) -> Result<Response, ApiError> {
    match state.actions.memory_transition(id, action).await? {
        TransitionOutcome::Applied(memory) => Ok(Json(json!({ "memory": memory })).into_response()),
        TransitionOutcome::NotFound => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no memory with id `{id}`") })),
        )
            .into_response()),
    }
}

#[derive(Deserialize)]
struct RunsQueryParams {
    limit: Option<usize>,
}

async fn list_runs(
    State(state): State<AppState>,
    Query(params): Query<RunsQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let runs = state.actions.runs(limit).await?;
    Ok(Json(json!({ "runs": runs })))
}

async fn get_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    // `Run` and `RunStep` serialize verbatim; the CLI reuses its run renderer.
    match state.actions.run(&id).await? {
        Some((run, steps)) => Ok(Json(json!({ "run": run, "steps": steps })).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "run not found" })),
        )
            .into_response()),
    }
}

// ---- session and memory governance (shared with the CLI) -------------------
//
// These are the writes a *client* performs — the desktop/web apps rename and
// archive sessions, and act on a memory from the memory view. The CLI reaches
// the same `OperatorActions` through `/api/operator`, so neither surface owns a
// second definition of what the action does.

#[derive(Deserialize)]
struct TitleBody {
    title: String,
}

/// Rename a session (operator/GUI). Loopback-gated.
async fn set_session_title(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TitleBody>,
) -> Result<Json<Value>, ApiError> {
    state.actions.set_session_title(&id, &body.title).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct WorkspaceBody {
    /// An absolute path on this host. Canonicalized server-side; a client never
    /// gets to say what a root resolves to.
    path: String,
}

/// `/workspace add <path>`: widen a task's environment with another directory.
///
/// Explicit on purpose. A task's roots are bound on its first turn, so working
/// across projects is a decision someone makes for *this* task — never
/// something a `cd` does on its behalf. Loopback-gated with the rest of the
/// operator writes.
async fn add_session_root(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<WorkspaceBody>,
) -> Result<Response, ApiError> {
    let requested = std::path::Path::new(body.path.trim());
    if !requested.is_absolute() {
        return Err(ApiError::bad_request(format!(
            "`{}` is not an absolute path",
            body.path.trim()
        )));
    }
    let Some(root) = canonical_dir(requested) else {
        return Err(ApiError::bad_request(format!(
            "`{}` is not an existing directory",
            body.path.trim()
        )));
    };
    let Some(session) = state.actions.sessions.find(&id).await? else {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no session with id `{id}`") })),
        )
            .into_response());
    };
    if session.origin != komo_core::domain::context::SessionOrigin::User
        || state.actions.home_session().await? == id
    {
        return Err(ApiError::bad_request(
            "只有任务会话有自己的 workspace：home 和 komo 自己的会话按每回合的目录运行".to_string(),
        ));
    }
    if session.roots.is_empty() {
        return Err(ApiError::bad_request(
            "这条会话没有绑定 workspace，没有可加宽的范围；在目标目录下用 `komo` 开一个任务会话"
                .to_string(),
        ));
    }
    let path = root.display().to_string();
    let mut roots = session.roots;
    if !roots.contains(&path) {
        roots.push(path);
        state.actions.sessions.set_roots(&id, &roots).await?;
    }
    Ok(Json(json!({ "ok": true, "roots": roots })).into_response())
}

#[derive(Deserialize)]
struct StatusBody {
    /// `"active"` | `"archive"` | `"deleted"`.
    status: String,
}

/// Set a session's lifecycle status — archive / unarchive / soft-delete. A
/// `deleted` session is hidden from the list but its rows remain. Loopback-gated.
async fn set_session_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StatusBody>,
) -> Result<Json<Value>, ApiError> {
    state.actions.set_session_status(&id, &body.status).await?;
    Ok(Json(json!({ "ok": true })))
}

/// The dreaming dry-run classification (backs the GUI's memory view and
/// `komo dream` with no `--apply`): which candidates would promote / archive,
/// with their scores, plus the full candidate count so a no-op does not look
/// like an empty memory library.
async fn dream_preview(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let report = state.actions.dream_preview().await?;
    Ok(Json(json!({
        "promote": report.promote,
        "archive": report.archive,
        "candidate_count": report.candidate_count,
    })))
}

/// Run one dreaming consolidation cycle — the same `DreamSweep` the gateway
/// schedules.
async fn dream_apply(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let (promoted, archived) = state.actions.dream_apply().await?;
    Ok(Json(json!({ "promoted": promoted, "archived": archived })))
}

// ---- the operator endpoint --------------------------------------------------
//
// One route for the whole `komo` CLI. It used to be a route, a hand-written
// client method and a hand-written handler per action — forty of each, and a
// version skew between CLI and gateway surfaced as a 404 on a path nobody could
// name. Now the request *is* the typed enum both sides hold
// (`operator_control::request`), and adding an operator action is a variant
// plus an arm here.

/// Dispatch one operator call onto the shared use cases and answer with the
/// matching reply arm.
async fn operator(
    State(state): State<AppState>,
    Json(request): Json<OperatorRequest>,
) -> Result<Json<OperatorReply>, ApiError> {
    Ok(Json(match request {
        OperatorRequest::Query(query) => OperatorReply::Query(operator_query(&state, query).await?),
        OperatorRequest::Command(command) => {
            OperatorReply::Command(operator_command(&state, command).await?)
        }
    }))
}

async fn operator_query(
    state: &AppState,
    query: OperatorQuery,
) -> Result<OperatorQueryResult, ApiError> {
    let actions = &state.actions;
    Ok(match query {
        OperatorQuery::Runs { limit } => OperatorQueryResult::Runs(actions.runs(limit).await?),
        OperatorQuery::Run { id } => OperatorQueryResult::Run(actions.run(&id).await?),
        OperatorQuery::Sessions => {
            OperatorQueryResult::Sessions(actions.session_summaries().await?)
        }
        OperatorQuery::Memories => {
            OperatorQueryResult::Memories(actions.list_memories(None).await?)
        }
        OperatorQuery::MemorySearch { query, limit } => {
            OperatorQueryResult::MemorySearch(actions.memory_search(&query, limit).await?)
        }
        OperatorQuery::Pairings => OperatorQueryResult::Pairings(actions.pairing_views().await?),
        OperatorQuery::DreamPreview => {
            OperatorQueryResult::DreamPreview(actions.dream_preview().await?)
        }
        OperatorQuery::HomeOverride => {
            OperatorQueryResult::HomeOverride(actions.home_override().await?)
        }
        OperatorQuery::WikiSearch { query, limit } => {
            OperatorQueryResult::WikiHits(actions.wiki_search(&query, limit).await?)
        }
        OperatorQuery::WikiStatus => OperatorQueryResult::WikiStatus(actions.wiki_status().await?),
        OperatorQuery::CronJobs => OperatorQueryResult::CronJobs(actions.list_cron_jobs().await?),
    })
}

async fn operator_command(
    state: &AppState,
    command: OperatorCommand,
) -> Result<OperatorCommandResult, ApiError> {
    let actions = &state.actions;
    // A name or expression the operator got wrong is theirs to fix, not a
    // server fault — the CLI prints the message either way, but a 500 also
    // logs an incident that never happened.
    let caller_error = |error: anyhow::Error| ApiError::bad_request(format!("{error:#}"));
    Ok(match command {
        OperatorCommand::MemoryTransition { id, action } => {
            match actions.memory_transition(&id, action).await? {
                TransitionOutcome::Applied(_) => OperatorCommandResult::MemoryTransitioned,
                TransitionOutcome::NotFound => {
                    return Err(ApiError::bad_request(format!("no memory with id `{id}`")));
                }
            }
        }
        OperatorCommand::PruneRuns { cutoff } => OperatorCommandResult::RunsPruned {
            removed: actions.prune_runs(cutoff).await?,
        },
        OperatorCommand::CleanSessions => OperatorCommandResult::SessionsCleaned {
            removed: actions.clean_sessions().await?,
        },
        OperatorCommand::PairApprove { code } => OperatorCommandResult::PairApproved(match actions
            .pair_approve(&code)
            .await?
        {
            ApproveOutcome::Approved(request) => PairApproveOutcome::Approved { id: request.id },
            ApproveOutcome::NotFound => PairApproveOutcome::NotFound,
            ApproveOutcome::Locked { retry_after_secs } => {
                PairApproveOutcome::Locked { retry_after_secs }
            }
        }),
        OperatorCommand::PairRevoke { id } => OperatorCommandResult::PairRevoked {
            revoked: actions.pair_revoke(&id).await?,
        },
        OperatorCommand::DreamApply => {
            let (promoted, archived) = actions.dream_apply().await?;
            OperatorCommandResult::DreamApplied { promoted, archived }
        }
        OperatorCommand::MemoryBackfill => OperatorCommandResult::MemoryBackfilled {
            embedded: actions.memory_backfill().await?,
        },
        OperatorCommand::ChunkIndex { rebuild } => {
            OperatorCommandResult::WikiIndexed(actions.wiki_index(rebuild).await?)
        }
        OperatorCommand::CronAdd { spec } => OperatorCommandResult::CronAdded(Box::new(
            actions.add_cron_job(spec).await.map_err(caller_error)?,
        )),
        OperatorCommand::CronRemove { name } => {
            if !actions.remove_cron_job(&name).await? {
                return Err(ApiError::bad_request(no_cron_job_message(&name)));
            }
            OperatorCommandResult::CronRemoved
        }
        OperatorCommand::CronSetEnabled { name, enabled } => {
            match actions.set_cron_enabled(&name, enabled).await? {
                Some(job) => OperatorCommandResult::CronUpdated(Box::new(job)),
                None => return Err(ApiError::bad_request(no_cron_job_message(&name))),
            }
        }
        OperatorCommand::CronTrigger { name } => {
            match actions
                .trigger_cron_job(&name)
                .await
                .map_err(caller_error)?
            {
                Some(job) => OperatorCommandResult::CronUpdated(Box::new(job)),
                None => return Err(ApiError::bad_request(no_cron_job_message(&name))),
            }
        }
    })
}

// ---- interactive approval / clarify (for the GUI) --------------------------
//
// An interactive HTTP turn (`X-Komo-Interactive`) suspends on approval and
// clarify prompts just like a chat channel, but there is no reply sink a human
// reads — the GUI polls `GET /api/interactions/{session}` for the pending
// prompt and resolves it with a `POST`. The GET is an ordinary protected read;
// the two POSTs sit behind `require_interactive_access` (loopback always;
// keyed remote only with `[channels.api] remote_interactive = true`).

/// The prompt(s) a suspended interactive turn is currently waiting on. Either
/// field is `null` when nothing of that kind is pending.
async fn get_interactions(
    State(state): State<AppState>,
    Path(session): Path<String>,
) -> Json<Value> {
    let approval = state.approvals.pending_info(&session);
    let question = state.dispatcher.pending_question(&session).await;
    Json(json!({ "approval": approval, "question": question }))
}

/// Stop the session's in-flight turn.
///
/// Order matters: a turn suspended on an approval or a question is not running
/// at all, so the cancel signal has nothing to interrupt — the answer is what
/// brings it back. Both are resolved first (a denial / a stop answer), and the
/// continuation then reaches its next cancellation check immediately.
///
/// Cancelling stops further rounds and further tool calls; a tool call already
/// executing runs to completion (see `domain::cancel`). The turn's reply becomes
/// `CANCELLED_REPLY`, which is what lands in the transcript.
async fn cancel_turn(State(state): State<AppState>, Path(session): Path<String>) -> Json<Value> {
    let denied = state.approvals.resolve(&session, Answer::Deny(None));
    let answered = state
        .dispatcher
        .answer_question(&session, CANCELLED_REPLY, None)
        .await;
    let cancelled = state.cancels.cancel(&session);
    if cancelled {
        info!(%session, denied, answered, "turn cancelled by client");
    }
    Json(json!({
        "cancelled": cancelled,
        "denied_pending_approval": denied,
        "answered_pending_question": answered,
    }))
}

#[derive(Deserialize)]
struct ApprovalDecisionBody {
    /// `"once"` | `"session"` | `"deny"`.
    decision: String,
    /// Optional reason for a denial, relayed to the model so it can correct the
    /// call instead of retrying it (ignored for the two allow decisions).
    #[serde(default)]
    feedback: Option<String>,
}

/// Map the wire decision string to an [`Answer`], attaching `feedback` to a
/// denial. `None` = unrecognized.
fn parse_decision(s: &str, feedback: Option<String>) -> Option<Answer> {
    match s {
        "once" => Some(Answer::Once),
        "session" => Some(Answer::Session),
        "deny" => Some(Answer::Deny(
            feedback.filter(|text| !text.trim().is_empty()),
        )),
        _ => None,
    }
}

/// Resolve a pending approval for `session` (the GUI's approval modal).
async fn resolve_approval(
    State(state): State<AppState>,
    Path(session): Path<String>,
    Json(body): Json<ApprovalDecisionBody>,
) -> Result<Json<Value>, ApiError> {
    let decision = parse_decision(&body.decision, body.feedback).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown decision `{}` (want once|session|deny)",
            body.decision
        )
    })?;
    // One entry for both surfaces: the modal's answer does exactly what a chat
    // `/approve` does — clear the prompt *and* write the durable resolution the
    // suspended turn is parked on.
    let resolved = state
        .dispatcher
        .answer_approval(&session, None, decision)
        .await;
    Ok(Json(json!({ "resolved": resolved })))
}

#[derive(Deserialize)]
struct AnswerBody {
    text: String,
}

/// Answer a pending clarify question for `session` (the GUI's inline reply).
async fn answer_question(
    State(state): State<AppState>,
    Path(session): Path<String>,
    Json(body): Json<AnswerBody>,
) -> Result<Json<Value>, ApiError> {
    let resolved = state
        .dispatcher
        .answer_question(&session, &body.text, None)
        .await;
    Ok(Json(json!({ "resolved": resolved })))
}

/// Unix seconds, for OpenAI `created` fields.
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests;
