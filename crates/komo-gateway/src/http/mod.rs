//! §13.1 的 axum 路由表。
//!
//! 「CLI 通过 HTTP 发命令，通过 SSE 观察运行。Gateway 内部采用函数调用，无需在本机
//! 模块之间再发 HTTP。」——所以这一层很薄：请求体 / 响应体就是 kernel 的协议类型，
//! 真正的动作在 [`GatewayState`] 上，聊天渠道调的是同一批函数（§13.1 最后一段）。
//!
//! **除 `GET /healthz` 外统一认证**，由 [`crate::auth`] 的 layer 一次性挡住。

pub mod approvals;
pub mod config;
pub mod cron;
pub mod error;
pub mod events;
pub mod idempotency;
pub mod memories;
pub mod runs;
pub mod sessions;

use std::sync::Arc;

use axum::Json;
use axum::routing::{get, patch, post};
use axum::{Router, extract::State};
use komo_kernel::events::Event;
use komo_kernel::protocol::PROTOCOL_VERSION;
use komo_kernel::protocol::http::{HealthResponse, SessionSummary};
use komo_kernel::traits::Ledger;
use komo_kernel::types::ids::{Seq, SessionId};
use time::OffsetDateTime;

use crate::service::state::GatewayState;

use error::{ApiFailure, ApiResult};
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
pub fn router(api: Api) -> Router {
    let token = api.state.token.clone();
    let open = Router::new().route("/healthz", get(healthz));
    let guarded = Router::new()
        .route("/v1/home-session", get(home_session))
        .route("/v1/sessions", get(sessions::list).post(sessions::create))
        .route("/v1/sessions/{id}", get(sessions::detail))
        .route("/v1/sessions/{id}/events", get(events::stream))
        .route("/v1/sessions/{id}/runs", post(sessions::submit))
        .route("/v1/sessions/{id}/resume", post(sessions::resume))
        .route("/v1/sessions/{id}/boundary", post(sessions::boundary))
        .route("/v1/runs/{id}", get(runs::detail))
        .route("/v1/runs/{id}/cancel", post(runs::cancel))
        .route("/v1/approvals", get(approvals::list))
        .route("/v1/approvals/{id}", get(approvals::show))
        .route("/v1/approvals/{id}/decision", post(approvals::decide))
        .route("/v1/cron", get(cron::list).post(cron::create))
        .route("/v1/cron/{id}", patch(cron::update).delete(cron::remove))
        .route("/v1/cron/{id}/run", post(cron::run_now))
        .route("/v1/memories", get(memories::list))
        .route("/v1/memories/{id}", get(memories::show))
        .route("/v1/memories/{id}/confirm", post(memories::confirm))
        .route("/v1/memories/{id}/forget", post(memories::forget))
        .route("/v1/memory-index", get(memories::index))
        .route("/v1/memory-index/rebuild", post(memories::rebuild))
        .route("/v1/models", get(config::models))
        .route("/v1/config/check", get(config::check))
        .route("/v1/config/reload", post(config::reload))
        .layer(crate::auth::bearer(token));

    open.merge(guarded).with_state(api)
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
    })
}

/// `GET /v1/home-session`：操作者那一个常驻会话（§11.2）。
///
// TODO(decide: §13.1 的接口表里没有它。`komo home` 需要一个"哪个会话是 home"的答案，
// 而 `SessionSummary` 上没有 `origin`，按列表也筛不出来。这里加一个只读端点，类型仍是
// 现成的 `SessionSummary`——kernel 一个字没改。见报告。)
async fn home_session(State(api): State<Api>) -> ApiResult<Json<SessionSummary>> {
    let session = api.state.home_session().await?;
    let summary = sessions::summary_of(&api, &session).await?;
    Ok(Json(summary))
}

/// 读完一个 Session 的日志（短事务分页，§14 的既定对策）。
pub async fn events_of(api: &Api, session: &SessionId) -> Result<Vec<Event>, ApiFailure> {
    let mut all = Vec::new();
    let mut from = Seq::ZERO;
    loop {
        let batch = api.state.routed.read(session, from, 0).await?;
        if batch.events.is_empty() {
            return Ok(all);
        }
        for event in batch.events {
            from = from.max(event.seq);
            all.push(event);
        }
        if batch.next.is_none() {
            return Ok(all);
        }
    }
}

/// 从 UUIDv7 里取出它的生成时刻。
///
/// 「UUIDv7 的前 48 位就是这个时间戳」（kernel 的 `ids` 模块）——所以 `created_at` 不需要
/// 一个额外的列，ID 自己就带着它。取不出来（测试里的 `from_raw`）就退回给定的兜底值。
pub fn created_at(id: &str, fallback: OffsetDateTime) -> OffsetDateTime {
    let Ok(uuid) = uuid::Uuid::parse_str(id) else {
        return fallback;
    };
    let Some(timestamp) = uuid.get_timestamp() else {
        return fallback;
    };
    let (seconds, nanos) = timestamp.to_unix();
    OffsetDateTime::from_unix_timestamp(seconds as i64)
        .map(|at| at + time::Duration::nanoseconds(nanos as i64))
        .unwrap_or(fallback)
}

/// `GET /v1/home-session`，从进程外面问（`komo home` 用它）。
///
/// `komo-client` 的 `KomoClient` 是按 §13.1 的接口表写的，这个端点不在那张表里
/// （见上面的 TODO），所以这里给一个最小的调用，而不是去改 client。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uuid_v7_carries_its_own_creation_time() {
        let at = time::macros::datetime!(2026-09-16 08:00:00 UTC);
        let id = komo_kernel::types::ids::SessionId::new_at(at);
        let read = created_at(id.as_str(), OffsetDateTime::UNIX_EPOCH);
        assert!(
            (read - at).abs() < time::Duration::seconds(1),
            "{read} vs {at}"
        );
    }

    #[test]
    fn a_non_uuid_id_falls_back() {
        let fallback = time::macros::datetime!(2026-09-16 08:00:00 UTC);
        assert_eq!(created_at("sess-1", fallback), fallback);
    }
}
