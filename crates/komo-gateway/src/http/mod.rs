//! §13.1 的 axum 路由表。
//!
//! 「CLI 通过 HTTP 发命令，通过 SSE 观察运行。Gateway 内部采用函数调用，无需在本机
//! 模块之间再发 HTTP。」——所以这一层很薄：请求体 / 响应体就是 kernel 的协议类型，
//! 真正的动作在 [`GatewayState`] 上，聊天渠道调的是同一批函数（§13.1 最后一段）。
//!
//! **除 `GET /healthz` 外统一认证**，由 [`crate::auth`] 的 layer 一次性挡住。

pub mod channels;
pub mod config;
pub mod cron;
pub mod error;
pub mod idempotency;
pub mod matters;
pub mod mcp;
pub mod memories;
pub mod v2;

use std::sync::Arc;

use axum::Json;
use axum::routing::{delete, get, post, put};
use axum::{Router, extract::State};
use komo_kernel::protocol::PROTOCOL_VERSION;
use komo_kernel::protocol::http::{HealthResponse, SessionSummary};

use crate::service::state::GatewayState;

use idempotency::Idempotency;

/// 路由共享的东西。
#[derive(Clone)]
pub struct Api {
    pub state: Arc<GatewayState>,
    pub idempotency: Arc<Idempotency>,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api").finish_non_exhaustive()
    }
}

impl Api {
    pub fn new(state: Arc<GatewayState>) -> Self {
        Api {
            state,
            idempotency: Arc::new(Idempotency::new()),
        }
    }
}

/// §13.1 的接口表，加上 W3 契约修订里的三个（`/v1/models`、`/v1/config/check`、
/// `/v1/config/reload`），以及 `/v1/home-session`。
///
/// 会话、run、树、事件、cron、intervention、导出在 [`v2::routes`]（路径与协议类型不变，handler
/// 读写 `durable.db`）；memories、config、models、mcp、healthz 在这里。
pub fn router(api: Api) -> Router {
    let token = api.state.token.clone();
    let open = Router::new().route("/healthz", get(healthz));
    let guarded = shared_routes()
        .merge(v2::routes())
        .layer(crate::auth::bearer(token));

    open.merge(guarded).with_state(api)
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
