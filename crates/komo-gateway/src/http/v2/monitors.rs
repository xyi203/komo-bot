//! `GET /v1/monitors`：监听清单（`komo monitor list`，roadmap-muse S4）。只读：登记在 `durable.db` 的
//! `komo.monitors`（`docs/monitor.md`），这里只把它翻成协议类型、补上挂它的会话。停监听仍走
//! `monitor_stop`（模型在挂它的会话里调）或删会话。

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use komo_kernel::protocol::http::{MonitorListQuery, MonitorListResponse, MonitorSummary};
use komo_kernel::types::ids::SessionId;
use komo_runtime::durable::harness::orchestration::monitor::{self, MonitorState};

use crate::http::Api;
use crate::http::error::ApiResult;
use crate::service::durable::sessions::{SessionError, time_of};

/// 清单里脚本最多留几个字。
const COMMAND_CHARS: usize = 120;

pub fn routes() -> Router<Api> {
    Router::new().route("/v1/monitors", get(list))
}

/// 还在跑的在前（按开始时刻），`all` 时已结束的接在后面。
async fn list(
    State(api): State<Api>,
    Query(query): Query<MonitorListQuery>,
) -> ApiResult<Json<MonitorListResponse>> {
    let core = api.state.core()?;
    let mut records = monitor::list(&*core.storage)
        .await
        .map_err(SessionError::from)?;
    records.retain(|record| query.all || record.is_live());
    records.sort_by_key(|record| (!record.is_live(), record.started_at));
    let mut monitors = Vec::with_capacity(records.len());
    for record in records {
        let session = core
            .storage
            .conversation(record.conversation)
            .await
            .map_err(SessionError::from)?
            .and_then(|conversation| conversation.session)
            .map(SessionId::from_raw);
        let (state, reason) = match &record.state {
            MonitorState::Starting => ("starting", None),
            MonitorState::Running => ("running", None),
            MonitorState::Ended { reason, .. } => ("ended", Some(reason.clone())),
        };
        let mut command: String = record.command.chars().take(COMMAND_CHARS).collect();
        if record.command.chars().count() > COMMAND_CHARS {
            command.push('…');
        }
        monitors.push(MonitorSummary {
            handle: record.handle(),
            title: record.title.clone(),
            session,
            state: state.to_owned(),
            reason,
            started_at: time_of(record.started_at),
            deadline: time_of(record.deadline),
            command,
            // 转后台的命令登记成 `timeout_minutes = 0`（`monitor_runner::detached::is_detached`）
            detached: record.timeout_minutes == 0,
        });
    }
    Ok(Json(MonitorListResponse { monitors }))
}
