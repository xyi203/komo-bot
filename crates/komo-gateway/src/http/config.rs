//! `/v1/models`、`/v1/config/check`、`/v1/config/reload`（W3 契约修订补的三个端点）。

use axum::Json;
use axum::extract::State;
use komo_kernel::protocol::http::{
    ConfigCheckResponse, ConfigReloadResponse, ModelMenuEntry, ModelsResponse,
};
use komo_kernel::types::model::{CatalogModel, ModelConfig};

use super::Api;
use super::error::{ApiFailure, ApiResult};

/// 把外部提交的 completion alias 解析成一份完整配置。Session 与 Cron 共用这一条路，
/// 避免一个按 alias、另一个又退回“只换上游 model id”。
pub(super) fn completion_model(api: &Api, alias: &str) -> Result<ModelConfig, ApiFailure> {
    let snapshot = api.state.snapshot();
    snapshot
        .model_catalog
        .completion(alias.trim())
        .cloned()
        .ok_or_else(|| {
            let configured = snapshot
                .model_catalog
                .completions()
                .map(|(alias, _)| alias)
                .collect::<Vec<_>>()
                .join(", ");
            ApiFailure::invalid(format!(
                "未知 completion 模型 alias `{}`；可选值：{}",
                alias.trim(),
                configured
            ))
        })
}

/// `GET /v1/models`：可选模型清单。
///
/// 清单来自 `model.<alias>` 目录中的 completion 项。调用者提交的是 alias，不是上游模型
/// id；这样一次选择会带上该项完整的端点、协议与凭证引用。
pub async fn models(State(api): State<Api>) -> Json<ModelsResponse> {
    let snapshot = api.state.snapshot();
    let models = snapshot
        .model_catalog
        .completions()
        .map(|(alias, model)| entry(&api, alias, model, alias == snapshot.model_catalog.default))
        .collect();
    Json(ModelsResponse { models })
}

fn entry(api: &Api, alias: &str, model: &CatalogModel, default: bool) -> ModelMenuEntry {
    let config = model.completion().expect("调用方只遍历 completion 目录项");
    ModelMenuEntry {
        id: alias.to_string(),
        name: model.name().to_string(),
        model: config.model.clone(),
        provider: model.model_provider().unwrap_or("standalone").to_string(),
        api_backend: config.provider.clone(),
        context_window: api.state.windows.effective(config),
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
    let mut issues = komo_runtime::config::validate_with(&snapshot, &api.state.caps);
    // 加载时的警告也要看得见（被忽略的键、还留着的 `policy.toml`，§3 / §7）：它们不在快照
    // 上，校验那一步看不到。`policy.toml` 那一条按文件**此刻**在不在：删掉之后不必等一次
    // 重载才不报。
    let retired = api
        .state
        .home
        .join(komo_runtime::config::RETIRED_POLICY_FILE);
    for warning in api.state.config.warnings() {
        let stale =
            warning.key.as_str() == komo_runtime::config::RETIRED_POLICY_FILE && !retired.exists();
        if !stale && !issues.contains(&warning) {
            issues.push(warning);
        }
    }
    Json(ConfigCheckResponse {
        issues,
        loaded_at: snapshot.loaded_at,
        // 「`komo doctor` 显示当前生效配置的加载时间与来源文件 mtime，两者不一致就是
        // "文件改了但没装上"」——所以这里给的是**文件此刻**的 mtime。
        sources: api.state.config.sources().stamps(),
        model_windows: snapshot
            .model_catalog
            .completions()
            .filter_map(|(alias, model)| {
                model
                    .completion()
                    .map(|config| api.state.windows.describe(alias, config))
            })
            .collect(),
    })
}

/// `POST /v1/config/reload`：校验通过才装，装完报告差异。
///
/// **校验不过走 `ErrorBody` 的 `config_invalid`，旧快照原样保留**（§3 第 1 步）。
pub async fn reload(State(api): State<Api>) -> ApiResult<Json<ConfigReloadResponse>> {
    Ok(Json(crate::reload::reload(&api.state).await?))
}
