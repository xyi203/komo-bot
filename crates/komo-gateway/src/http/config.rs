//! `/v1/models`、`/v1/config/check`、`/v1/config/reload`（W3 契约修订补的三个端点），
//! 以及 `/v1/config/{models,settings,env}` 那几组：从 Web 改模型配置、常用设置与环境变量
//! （§3「从 Web 改模型配置」「从 Web 改常用设置与环境变量」）。

use axum::Json;
use axum::extract::{Path, State};
use komo_kernel::protocol::http::{
    ConfigCheckResponse, ConfigEditResponse, ConfigEnvResponse, ConfigModelsResponse,
    ConfigReloadResponse, ConfigSettingsResponse, ConfigVersionRequest, DefaultModelRequest,
    EnvSetRequest, ErrorCode, ModelMenuEntry, ModelUpsertRequest, ModelsResponse,
    ProviderUpsertRequest, SettingRequest,
};
use komo_kernel::types::model::{CatalogModel, ModelConfig};
use komo_runtime::config::{EditError, ModelFields, ProviderFields};

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

// ---------------------------------------------------------------- 从 Web 改模型配置

/// `GET /v1/config/models`：文件里写着的 provider 与 completion 模型，凭证只报在不在。
pub async fn model_settings(State(api): State<Api>) -> ApiResult<Json<ConfigModelsResponse>> {
    let (config, env) = api.state.config.read_texts().map_err(io_failure)?;
    Ok(Json(view(&api, &config, &env)?))
}

/// `PUT /v1/config/model-providers/{id}`
pub async fn put_provider(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<ProviderUpsertRequest>,
) -> ApiResult<Json<ConfigEditResponse>> {
    edit(&api, &request.version, |config, env| {
        let fields = ProviderFields {
            base_url: request.base_url.as_deref(),
            api_backend: request.api_backend.as_deref(),
            env_key: request.env_key.as_deref(),
            auth: request.auth.as_deref(),
            timeout_secs: request.timeout_secs,
            api_key: request.api_key.as_deref(),
        };
        let (config, env) = komo_runtime::config::upsert_provider(config, env, &id, &fields)?;
        Ok((Some(config), env))
    })
    .await
}

/// `DELETE /v1/config/model-providers/{id}`
pub async fn delete_provider(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(request): Json<ConfigVersionRequest>,
) -> ApiResult<Json<ConfigEditResponse>> {
    edit(&api, &request.version, |config, _| {
        Ok((
            Some(komo_runtime::config::remove_provider(config, &id)?),
            None,
        ))
    })
    .await
}

/// `PUT /v1/config/models/{alias}`
pub async fn put_model(
    State(api): State<Api>,
    Path(alias): Path<String>,
    Json(request): Json<ModelUpsertRequest>,
) -> ApiResult<Json<ConfigEditResponse>> {
    edit(&api, &request.version, |config, _| {
        let fields = ModelFields {
            provider: &request.provider,
            model: &request.model,
            name: request.name.as_deref(),
            effort: request.effort.as_deref(),
            efforts: request.efforts.as_deref(),
            context_window: request.context_window,
        };
        Ok((
            Some(komo_runtime::config::upsert_model(config, &alias, &fields)?),
            None,
        ))
    })
    .await
}

/// `DELETE /v1/config/models/{alias}`
pub async fn delete_model(
    State(api): State<Api>,
    Path(alias): Path<String>,
    Json(request): Json<ConfigVersionRequest>,
) -> ApiResult<Json<ConfigEditResponse>> {
    edit(&api, &request.version, |config, _| {
        Ok((
            Some(komo_runtime::config::remove_model(config, &alias)?),
            None,
        ))
    })
    .await
}

/// `PUT /v1/config/default-model`
pub async fn put_default_model(
    State(api): State<Api>,
    Json(request): Json<DefaultModelRequest>,
) -> ApiResult<Json<ConfigEditResponse>> {
    edit(&api, &request.version, |config, _| {
        Ok((
            Some(komo_runtime::config::set_default_model(
                config,
                &request.alias,
            )?),
            None,
        ))
    })
    .await
}

/// `GET /v1/config/settings`：常用设置白名单。
pub async fn settings(State(api): State<Api>) -> ApiResult<Json<ConfigSettingsResponse>> {
    let (config, env) = api.state.config.read_texts().map_err(io_failure)?;
    Ok(Json(settings_view(&api, &config, &env)?))
}

/// `PUT /v1/config/settings/{key}`
pub async fn put_setting(
    State(api): State<Api>,
    Path(key): Path<String>,
    Json(request): Json<SettingRequest>,
) -> ApiResult<Json<ConfigEditResponse>> {
    edit(&api, &request.version, |config, _| {
        Ok((
            Some(komo_runtime::config::set_setting(
                config,
                &key,
                request.value.as_ref(),
            )?),
            None,
        ))
    })
    .await
}

/// `GET /v1/config/env`：变量名与状态，不带值。
pub async fn env(State(api): State<Api>) -> ApiResult<Json<ConfigEnvResponse>> {
    let (config, env) = api.state.config.read_texts().map_err(io_failure)?;
    Ok(Json(env_view(&api, &config, &env)?))
}

/// `PUT /v1/config/env`
pub async fn put_env(
    State(api): State<Api>,
    Json(request): Json<EnvSetRequest>,
) -> ApiResult<Json<ConfigEditResponse>> {
    edit(&api, &request.version, |_, env| {
        let vars = request
            .vars
            .iter()
            .map(|v| (v.name.as_str(), v.value.as_str()));
        Ok((None, Some(komo_runtime::config::set_env_values(env, vars)?)))
    })
    .await
}

/// `DELETE /v1/config/env/{name}`
pub async fn delete_env(
    State(api): State<Api>,
    Path(name): Path<String>,
    Json(request): Json<ConfigVersionRequest>,
) -> ApiResult<Json<ConfigEditResponse>> {
    edit(&api, &request.version, |config, env| {
        Ok((
            None,
            Some(komo_runtime::config::remove_env_value(config, env, &name)?),
        ))
    })
    .await
}

/// 每次写都走这一段：拿锁 → 比版本 → 改文本 → 与加载同一个校验 → 落盘 → 重载。
/// 校验不过文件一个字节不动；重载仍失败（两步之间文件被外部改了）就写回改前的原文。
async fn edit<F>(api: &Api, version: &str, change: F) -> ApiResult<Json<ConfigEditResponse>>
where
    F: FnOnce(&str, &str) -> Result<(Option<String>, Option<String>), EditError>,
{
    let holder = &api.state.config;
    let _writing = holder.edit_lock().lock().await;
    let (config, env) = holder.read_texts().map_err(io_failure)?;
    if komo_runtime::config::config_version(&config, &env) != version {
        return Err(ApiFailure::new(
            ErrorCode::VersionConflict,
            "配置文件在你打开之后被改过了（编辑器里，或另一个窗口）：刷新再改",
        ));
    }
    let (next_config, next_env) = change(&config, &env).map_err(edit_failure)?;
    let config_after = next_config.as_deref().unwrap_or(&config);
    let env_after = next_env.as_deref().unwrap_or(&env);
    holder
        .check_texts(config_after, env_after)
        .map_err(crate::reload::config_failure)?;
    holder
        .write_texts(next_config.as_deref(), next_env.as_deref())
        .map_err(io_failure)?;
    let reload = match crate::reload::reload(&api.state).await {
        Ok(report) => report,
        Err(failure) => {
            let restore_env = next_env.is_some().then_some(env.as_str());
            if let Err(error) = holder.write_texts(Some(&config), restore_env) {
                tracing::error!(%error, "改配置后重载失败，写回原文也失败了");
            }
            return Err(failure);
        }
    };
    // 生效值从重载后的快照读，三份视图共用这个新 version
    Ok(Json(ConfigEditResponse {
        models: view(api, config_after, env_after)?,
        settings: settings_view(api, config_after, env_after)?,
        env: env_view(api, config_after, env_after)?,
        reload,
    }))
}

fn secrets(api: &Api, env: &str) -> ApiResult<komo_runtime::config::Secrets> {
    komo_runtime::config::secrets_from_text(Some(env), &api.state.config.sources().env)
        .map_err(io_failure)
}

fn settings_view(api: &Api, config: &str, env: &str) -> ApiResult<ConfigSettingsResponse> {
    komo_runtime::config::settings_view(
        config,
        &api.state.config.current(),
        komo_runtime::config::config_version(config, env),
    )
    .map_err(edit_failure)
}

fn env_view(api: &Api, config: &str, env: &str) -> ApiResult<ConfigEnvResponse> {
    komo_runtime::config::env_view(
        config,
        env,
        &secrets(api, env)?,
        komo_runtime::config::config_version(config, env),
    )
    .map_err(edit_failure)
}

fn view(api: &Api, config: &str, env: &str) -> ApiResult<ConfigModelsResponse> {
    let sources = api.state.config.sources();
    let secrets = secrets(api, env)?;
    komo_runtime::config::models_view(
        config,
        &secrets,
        komo_runtime::config::config_version(config, env),
        sources.config.display().to_string(),
        sources.env.display().to_string(),
    )
    .map_err(edit_failure)
}

fn edit_failure(error: EditError) -> ApiFailure {
    match &error {
        EditError::Invalid(message) => ApiFailure::invalid(message.clone()),
        EditError::NotFound(what) => ApiFailure::not_found(what),
        EditError::InUse(message) => ApiFailure::new(ErrorCode::Conflict, message.clone()),
        EditError::Unreadable(_) => ApiFailure::config_invalid(error.to_string(), Vec::new()),
    }
}

fn io_failure(error: std::io::Error) -> ApiFailure {
    ApiFailure::new(ErrorCode::Internal, format!("读写配置文件失败：{error}"))
}
