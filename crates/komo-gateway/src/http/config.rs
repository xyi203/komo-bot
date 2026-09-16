//! `/v1/models`、`/v1/config/check`、`/v1/config/reload`（W3 契约修订补的三个端点）。

use axum::Json;
use axum::extract::State;
use komo_kernel::protocol::http::{
    ConfigCheckResponse, ConfigReloadResponse, ModelMenuEntry, ModelsResponse,
};

use super::Api;
use super::error::ApiResult;

/// `GET /v1/models`：可选模型清单。
///
/// 清单从**当前快照的模型角色**来：主模型，以及独立配置的记忆模型（§13.3——「切换聊天
/// 模型或 effort 不影响独立配置的记忆模型」，所以它们是两条，不是一条）。每条带自己
/// 支持的档位；**空表就是"一档都不支持"，不是"还不知道"**——不知道的模型不该出现在
/// 给人挑的清单里。
pub async fn models(State(api): State<Api>) -> Json<ModelsResponse> {
    let snapshot = api.state.snapshot();
    let mut models = vec![entry(&api, &snapshot.model, true)];
    if snapshot.memory.enabled && snapshot.memory.model.model != snapshot.model.model {
        models.push(entry(&api, &snapshot.memory.model, false));
    }
    Json(ModelsResponse { models })
}

fn entry(
    api: &Api,
    config: &komo_kernel::types::model::ModelConfig,
    default: bool,
) -> ModelMenuEntry {
    ModelMenuEntry {
        id: config.model.clone(),
        provider: config.provider.clone(),
        efforts: config
            .efforts
            .clone()
            .or_else(|| {
                api.state
                    .caps
                    .support(config)
                    .map(|support| support.levels().to_vec())
            })
            .unwrap_or_default(),
        default,
    }
}

/// `GET /v1/config/check`：只读，**不改变运行中的 Gateway**（§3 的命令表）。
pub async fn check(State(api): State<Api>) -> Json<ConfigCheckResponse> {
    let snapshot = api.state.snapshot();
    Json(ConfigCheckResponse {
        issues: komo_runtime::config::validate_with(&snapshot, &api.state.caps),
        loaded_at: snapshot.loaded_at,
        // 「`komo doctor` 显示当前生效配置的加载时间与来源文件 mtime，两者不一致就是
        // "文件改了但没装上"」——所以这里给的是**文件此刻**的 mtime。
        sources: api.state.config.sources().stamps(),
    })
}

/// `POST /v1/config/reload`：校验通过才装，装完报告差异。
///
/// **校验不过走 `ErrorBody` 的 `config_invalid`，旧快照原样保留**（§3 第 1 步）。
pub async fn reload(State(api): State<Api>) -> ApiResult<Json<ConfigReloadResponse>> {
    Ok(Json(crate::reload::reload(&api.state).await?))
}
