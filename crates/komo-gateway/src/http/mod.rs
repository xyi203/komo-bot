//! §13.1 的 axum 路由表。
//!
//! 「CLI 通过 HTTP 发命令，通过 SSE 观察运行。Gateway 内部采用函数调用，无需在本机
//! 模块之间再发 HTTP。」——所以这一层很薄：请求体 / 响应体就是 kernel 的协议类型，
//! 真正的动作在 [`GatewayState`] 上，聊天渠道调的是同一批函数（§13.1 最后一段）。
//!
//! **除 `GET /healthz`、配对换 cookie 与静态文件外统一认证**，由 [`crate::auth`] 的 layer 一次性挡住；
//! 所有路由之外还有一层 Host 校验（[`crate::web::host_guard`]）。

pub mod channels;
pub mod config;
pub mod cron;
pub mod error;
pub mod idempotency;
pub mod matters;
pub mod mcp;
pub mod memories;
pub mod v2;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::routing::{delete, get, post, put};
use axum::{Router, extract::State};
use komo_kernel::protocol::PROTOCOL_VERSION;
use komo_kernel::protocol::http::{HealthResponse, SessionSummary};

use crate::service::state::GatewayState;
use crate::web::{self, WebAuth};

use idempotency::Idempotency;

/// 路由共享的东西。
#[derive(Clone)]
pub struct Api {
    pub state: Arc<GatewayState>,
    pub idempotency: Arc<Idempotency>,
    /// 配对码 / 会话 cookie / Host 策略 / 静态目录（`docs/clients.md` D5、D6）。
    pub web: Arc<WebAuth>,
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
        let web_dir = snapshot
            .start_only
            .web_dir
            .clone()
            .or_else(crate::web::default_web_dir);
        let web = Arc::new(WebAuth::new(
            listen,
            &snapshot.start_only.trusted_hosts,
            web_dir,
        ));
        Api {
            state,
            idempotency: Arc::new(Idempotency::new()),
            web,
        }
    }
}

/// §13.1 的接口表，加上 W3 契约修订里的三个（`/v1/models`、`/v1/config/check`、
/// `/v1/config/reload`），以及 `/v1/home-session`。
///
/// 会话、run、树、事件、cron、intervention、导出在 [`v2::routes`]（路径与协议类型不变，handler
/// 读写 `durable.db`）；memories、config、models、mcp、healthz 在这里。
pub fn router(api: Api) -> Router {
    let auth = crate::auth::AuthState {
        token: api.state.token.clone(),
        web: Arc::clone(&api.web),
    };
    let hosts = Arc::clone(&api.web);
    // 配对换 cookie 不需要别的鉴权（码本身就是凭据）；申请配对码要 Bearer（在 handler 里核对）。
    let open = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/web/session", post(web::open_session));
    let guarded = shared_routes()
        .merge(v2::routes())
        .route("/v1/web/pair-code", post(web::pair_code))
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
        .route("/v1/channels", get(channels::list))
        .route("/v1/channels/{id}/probe", post(channels::probe))
        .route("/v1/mcp", get(mcp::list))
        .route("/v1/mcp/reconnect", post(mcp::reconnect))
        .route("/v1/mcp/disconnect", post(mcp::disconnect))
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

/// `GET /v1/home-session`，从进程外面问（`komo home` 用它）。
///
/// `komo-client` 的 `KomoClient` 是按 §13.1 的接口表写的，这个端点不在那张表里（`v2::sessions::home`），
/// 所以这里给一个最小的调用，而不是去改 client。
pub async fn fetch_home_session(
    base_url: &str,
    token: Option<&str>,
) -> Result<SessionSummary, String> {
    let mut request = reqwest::Client::new().get(format!(
        "{}/v1/home-session",
        base_url.trim_end_matches('/')
    ));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response.text().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!("{status}：{body}"));
    }
    serde_json::from_str(&body).map_err(|error| format!("{error}（{body}）"))
}

/// `POST /v1/web/pair-code`，从进程外面要一个浏览器配对码（`komo web` 用它）。
///
/// 同 [`fetch_home_session`]：这个端点不在 `komo-client` 按 §13.1 写的接口表里，给一个最小调用。
pub async fn request_pair_code(
    base_url: &str,
    token: Option<&str>,
) -> Result<komo_kernel::protocol::http::PairCodeResponse, String> {
    let mut request = reqwest::Client::new().post(format!(
        "{}/v1/web/pair-code",
        base_url.trim_end_matches('/')
    ));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response.text().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!("{status}：{body}"));
    }
    serde_json::from_str(&body).map_err(|error| format!("{error}（{body}）"))
}
