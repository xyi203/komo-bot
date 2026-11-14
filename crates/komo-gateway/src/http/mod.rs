//! §13.1 的 axum 路由表。
//!
//! 「CLI 通过 HTTP 发命令，通过 SSE 观察运行。Gateway 内部采用函数调用，无需在本机
//! 模块之间再发 HTTP。」——所以这一层很薄：请求体 / 响应体就是 kernel 的协议类型，
//! 真正的动作在 [`GatewayState`] 上。
//!
//! **除 `GET /healthz`、`/auth/*` 与静态文件外统一认证**（所有 `/v1/**`），由 [`crate::auth`] 的 layer
//! 一次性挡住；所有路由之外还有一层 Host 校验（[`crate::web::host_guard`]）。

pub mod attachments;
pub mod auth;
pub mod config;
pub mod error;
pub mod idempotency;
pub mod matters;
pub mod mcp;
pub mod memories;
pub mod push;
pub mod schedule;
pub mod skills;
pub mod v2;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::routing::{delete, get, post, put};
use axum::{Router, extract::State};
use komo_kernel::protocol::PROTOCOL_VERSION;
use komo_kernel::protocol::http::HealthResponse;

use crate::auth::AuthService;
use crate::service::state::GatewayState;
use crate::web::{self, WebAuth};

use idempotency::Idempotency;

/// 路由共享的东西。
#[derive(Clone)]
pub struct Api {
    pub state: Arc<GatewayState>,
    pub idempotency: Arc<Idempotency>,
    /// Host 策略 / 静态目录（`docs/clients.md` D5、D6）。
    pub web: Arc<WebAuth>,
    /// 登录：OIDC、网页会话、设备令牌（`docs/redesign.md` §2.9）。
    pub auth: Arc<AuthService>,
    /// 等回调的网页 MCP 登录（`mcp::oauth_start` / `oauth_callback`）。
    pub mcp_logins: Arc<mcp::McpLogins>,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api").finish_non_exhaustive()
    }
}

impl Api {
    /// `listen` 是**实际绑定**的地址（端口 0 绑出来的也算），Host 校验放行它。
    pub fn new(state: Arc<GatewayState>, listen: SocketAddr) -> Self {
        let snapshot = state.snapshot();
        // `[web] serve = false`：网页单独发布，Gateway 不托管静态文件（fallback 对非 /v1 路径回 404）。
        let web_dir = if snapshot.start_only.web_serve {
            snapshot
                .start_only
                .web_dir
                .clone()
                .or_else(crate::web::default_web_dir)
        } else {
            None
        };
        // 公开地址的主机自动放行：它是操作者在 `[auth.oidc]` 里写的，回调就落在它上面。
        let mut trusted = snapshot.start_only.trusted_hosts.clone();
        if let Some(oidc) = &snapshot.start_only.oidc
            && let Ok(url) = reqwest::Url::parse(&oidc.public_url)
            && let Some(host) = url.host_str()
        {
            trusted.push(match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            });
        }
        let web = Arc::new(WebAuth::new(listen, &trusted, web_dir));
        let auth = Arc::new(AuthService::new(
            state.db.clone(),
            Arc::clone(&state.config),
            Arc::clone(&state.clock),
            state.token.clone(),
        ));
        Api {
            state,
            idempotency: Arc::new(Idempotency::new()),
            web,
            auth,
            mcp_logins: Arc::new(mcp::McpLogins::default()),
        }
    }
}

/// §13.1 的接口表，加上 W3 契约修订里的三个（`/v1/models`、`/v1/config/check`、
/// `/v1/config/reload`），以及 `/v1/home-session`。
///
/// 会话、run、树、事件、intervention、导出在 [`v2::routes`]（路径与协议类型不变，handler
/// 读写 `durable.db`）；memories、config、models、mcp、healthz 在这里。
pub fn router(api: Api) -> Router {
    let auth = Arc::clone(&api.auth);
    let hosts = Arc::clone(&api.web);
    // 登录相关的路由与健康检查不要求登录（`/auth/device/token` 的凭据是一次性授权码本身）。
    let open = Router::new()
        .route("/healthz", get(healthz))
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .route("/auth/device/authorize", get(auth::device_authorize))
        .route("/auth/device/token", post(auth::device_token));
    let guarded = shared_routes()
        .merge(v2::routes())
        .route("/v1/auth/sessions", get(auth::sessions))
        .route("/v1/auth/sessions/{id}", delete(auth::revoke))
        .layer(axum::middleware::from_fn_with_state(
            auth,
            crate::auth::check,
        ));

    // 其余路径是网页壳的静态文件（`/v1/*` 在 fallback 里回 JSON 404）。Host 校验在最外层，
    // 连静态文件与 /healthz 一起挡。
    open.merge(guarded)
        .fallback(web::fallback)
        .with_state(api)
        .layer(axum::middleware::from_fn_with_state(hosts, web::host_guard))
}

/// memories、matters、config、models、mcp 这几组路由（handler 在本目录，不碰存储模型）。
fn shared_routes() -> Router<Api> {
    Router::new()
        .route("/v1/memories", get(memories::list).post(memories::add))
        .route("/v1/memories/{id}", get(memories::show))
        .route("/v1/memories/{id}/confirm", post(memories::confirm))
        .route("/v1/memories/{id}/classify", post(memories::classify))
        .route("/v1/memories/{id}/forget", post(memories::forget))
        .route("/v1/memories/{id}/restore", post(memories::restore))
        .route("/v1/memory-sweep", post(memories::sweep))
        .route("/v1/memory-consolidate", post(memories::consolidate))
        .route("/v1/memory-index", get(memories::index))
        .route("/v1/memory-index/rebuild", post(memories::rebuild))
        .route("/v1/matters", get(matters::list).post(matters::add))
        .route("/v1/matters/{id}", get(matters::show))
        .route("/v1/matters/{id}/runs", get(matters::runs))
        .route(
            "/v1/matters/{id}/chat",
            post(matters::bind_chat).delete(matters::unbind_chat),
        )
        .route("/v1/matters/{id}/{action}", post(matters::act))
        .route("/v1/models", get(config::models))
        .route("/v1/config/check", get(config::check))
        .route("/v1/config/reload", post(config::reload))
        .route("/v1/config/models", get(config::model_settings))
        .route(
            "/v1/config/model-providers/{id}",
            put(config::put_provider).delete(config::delete_provider),
        )
        .route(
            "/v1/config/models/{alias}",
            put(config::put_model).delete(config::delete_model),
        )
        .route(
            "/v1/config/embedding-models/{alias}",
            put(config::put_embedding_model).delete(config::delete_embedding_model),
        )
        .route("/v1/config/provider-models", post(config::provider_models))
        .route("/v1/config/default-model", put(config::put_default_model))
        .route("/v1/config/settings", get(config::settings))
        .route("/v1/config/settings/{key}", put(config::put_setting))
        .route("/v1/config/env", get(config::env).put(config::put_env))
        .route("/v1/config/env/{name}", delete(config::delete_env))
        .route(
            "/v1/attachments",
            post(attachments::upload).layer(axum::extract::DefaultBodyLimit::max(
                attachments::BODY_LIMIT_BYTES,
            )),
        )
        .route("/v1/attachments/{inbox}/{name}", get(attachments::image))
        .route("/v1/mcp", get(mcp::list))
        .route("/v1/mcp/reconnect", post(mcp::reconnect))
        .route("/v1/mcp/disconnect", post(mcp::disconnect))
        .route("/v1/mcp/{name}", put(mcp::set_server))
        .route("/v1/mcp/{name}/oauth/start", post(mcp::oauth_start))
        .route("/v1/mcp/{name}/oauth/callback", get(mcp::oauth_callback))
        .route("/v1/mcp/{name}/tools/{tool}", put(mcp::set_tool))
        .route("/v1/push/key", get(push::key))
        .route(
            "/v1/push/subscriptions",
            post(push::subscribe).delete(push::unsubscribe),
        )
        .route("/v1/skills", get(skills::list))
        .route("/v1/skills/{name}", put(skills::set))
}

/// `GET /healthz`：最小健康检查与**实例标识**。
///
/// 「检查 Gateway 健康状态与实例身份，**不能只凭 PID 或端口判断**」（§3）。
async fn healthz(State(api): State<Api>) -> Json<HealthResponse> {
    Json(HealthResponse {
        instance_id: api.state.instance_id.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_VERSION,
        started_at: api.state.started_at,
        data_dir: api.state.home.display().to_string(),
        telemetry: api
            .state
            .telemetry
            .get()
            .map(|status| status.health())
            .unwrap_or_default(),
    })
}
