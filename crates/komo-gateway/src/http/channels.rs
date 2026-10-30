//! `GET /v1/channels` 与 `POST /v1/channels/{id}/probe`（§3「Web 看渠道」）。

use axum::Json;
use axum::extract::{Path, State};
use komo_kernel::protocol::http::{
    ChannelProbeResponse, ChannelRunState, ChannelView, ChannelsResponse,
};
use komo_kernel::types::chat::ChannelPlatform;

use super::Api;
use super::error::{ApiFailure, ApiResult};

/// 配置里的每个渠道：配置摘要加监督器记下的运行状态。不等、不探测。
pub async fn list(State(api): State<Api>) -> Json<ChannelsResponse> {
    let snapshot = api.state.snapshot();
    let health = api.state.supervisor.health();
    let channels = snapshot
        .channels
        .platforms()
        .into_iter()
        .filter_map(|platform| {
            let config = snapshot.channels.get(platform.clone())?;
            let now = config.enabled.then(|| health.get(&platform)).flatten();
            Some(ChannelView {
                channel: platform.to_string(),
                enabled: config.enabled,
                custom_plugin: config.plugin.is_some(),
                allow_from: config.allow_from.len(),
                allow_tenant: config.allow_tenant,
                home_chat: config.home_chat.as_ref().map(ToString::to_string),
                outbound_only: config.is_outbound_only(),
                state: now.map_or(ChannelRunState::Off, |h| h.state),
                since: now.map(|h| h.since),
                detail: now.and_then(|h| h.detail.clone()),
                restarts: now.map_or(0, |h| h.restarts),
            })
        })
        .collect();
    Json(ChannelsResponse { channels })
}

/// 和 `komo channel probe` 同一条核对：另起一个插件进程问平台，不起消费循环。人点的按钮要当场
/// 看到结果，所以等它；30 秒没回就算失败。
pub async fn probe(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> ApiResult<Json<ChannelProbeResponse>> {
    let platform = ChannelPlatform::parse(&id)
        .filter(|p| *p != ChannelPlatform::Api)
        .ok_or_else(|| ApiFailure::not_found(format!("渠道 `{id}`")))?;
    let snapshot = api.state.snapshot();
    if snapshot.channels.get(platform.clone()).is_none() {
        return Err(ApiFailure::not_found(format!("配置里的渠道 `{id}`")));
    }
    let factory = api.state.supervisor.factory(&platform);
    let secrets = api.state.config.secrets();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        factory.probe(&snapshot, &secrets),
    )
    .await;
    Ok(Json(match outcome {
        Ok(Ok(message)) => ChannelProbeResponse { ok: true, message },
        Ok(Err(error)) => ChannelProbeResponse {
            ok: false,
            message: error.message,
        },
        Err(_) => ChannelProbeResponse {
            ok: false,
            message: "30 秒没有结果".into(),
        },
    }))
}
