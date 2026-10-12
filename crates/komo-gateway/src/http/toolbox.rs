//! `/v1/toolbox`：toolbox 的清单、候选测试与**启用 / 停用**（§5.3、§5.4）。
//!
//! toolbox 是文件系统上的东西，`komo toolbox list` 大可以自己去读那个目录。它**不**
//! 那么做：启用记的是"谁、什么时候、哪一版"，整组命令走同一个进程，CLI 才不会一半走
//! Gateway、一半直接读盘，给出两种互相矛盾的"当前版本"（§12）。
//!
//! 这组接口**不在 §13.1 的那张表里**。类型也定义在这个文件里。
//!
//! 启用 / 停用**直接生效**（§7：2026-10-11 删掉 Policy 与审批之后，没有 Pending，也没有
//! 后台盯审批的任务）：
//!
//! ```text
//! POST /v1/toolbox/{m}/enable
//!   → Toolbox::enable_preview          版本差异 + 候选测试结果
//!   → 测试没过 / 版本对不上            拒绝（422 / 409）
//!   → 目标落在 komo 自己的状态里        Refused（§8.10 的那份名单）
//!   → Toolbox::enable                  装上
//! ```

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use komo_runtime::toolbox::{
    Enabled, ModuleInfo, ModuleVersion, TestReport, Toolbox, ToolboxError,
};
use serde::{Deserialize, Serialize};

use super::Api;
use super::error::{ApiFailure, ApiResult};

// ---------------------------------------------------------------- 响应类型

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolboxListResponse {
    pub modules: Vec<ModuleInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolboxTestResponse {
    pub module: String,
    pub report: TestReport,
}

/// 一次启用 / 停用的去向。操作者要按它决定下一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    /// 装上了（或者停用了）。
    Applied,
    /// 没有执行——目标落在 komo 自己的状态里（§8.10），`reason` 说清楚是哪一条。
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolboxChangeResponse {
    pub module: String,
    pub status: ChangeStatus,
    /// 这次要装的那一版。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ModuleVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<Enabled>,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnableRequest {
    /// 要启用的版本。给了就必须与当前候选逐字相同（§5.4「校验候选哈希与已测版本一致」）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ModuleVersion>,
}

// ---------------------------------------------------------------- 路由

/// `GET /v1/toolbox`
pub async fn list(State(api): State<Api>) -> ApiResult<Json<ToolboxListResponse>> {
    let modules = toolbox_of(&api).list().map_err(failure)?;
    Ok(Json(ToolboxListResponse { modules }))
}

/// `GET /v1/toolbox/{module}`
pub async fn show(
    State(api): State<Api>,
    Path(module): Path<String>,
) -> ApiResult<Json<ModuleInfo>> {
    Ok(Json(toolbox_of(&api).inspect(&module).map_err(failure)?))
}

/// `POST /v1/toolbox/{module}/test`：跑候选自带的测试，结果记进候选的元数据（§5.4）。
///
/// 发起它的是拿着操作者令牌的 CLI，"真实权限"在这条路上就是操作者本人。
pub async fn test(
    State(api): State<Api>,
    Path(module): Path<String>,
) -> ApiResult<Json<ToolboxTestResponse>> {
    let toolbox = toolbox_of(&api);
    let config = crate::service::python_env(&api.state.config, &toolbox);
    let host = komo_runtime::python_runtime::PythonRuntime::probe(config)
        .await
        .map_err(|error| {
            ApiFailure::new(
                komo_kernel::protocol::http::ErrorCode::Internal,
                format!("跑不了候选测试：Python 环境探测不到（{error}）"),
            )
        })?;
    let report = toolbox
        .run_candidate_tests(&module, &host, api.state.clock.now())
        .await
        .map_err(failure)?;
    Ok(Json(ToolboxTestResponse {
        module: module.trim().trim_start_matches("toolbox.").to_string(),
        report,
    }))
}

/// `POST /v1/toolbox/{module}/enable`
pub async fn enable(
    State(api): State<Api>,
    Path(module): Path<String>,
    body: Option<Json<EnableRequest>>,
) -> ApiResult<Json<ToolboxChangeResponse>> {
    let wanted = body.and_then(|Json(body)| body.version);
    let toolbox = toolbox_of(&api);
    let preview = toolbox.enable_preview(&module).map_err(failure)?;
    if let Some(wanted) = &wanted
        && wanted != &preview.version
    {
        return Err(failure(ToolboxError::VersionMismatch {
            module: preview.module.clone(),
            candidate: preview.version.clone(),
            wanted: wanted.clone(),
        }));
    }

    // 「运行测试并保存结果」排在启用**前面**（§5.4）：一个没测过的候选不装。
    match &preview.tests {
        Some(report) if report.passed && report.version == preview.version => {}
        other => {
            return Err(failure(ToolboxError::Untested {
                module: preview.module.clone(),
                version: preview.version.clone(),
                why: match other {
                    Some(report) if report.version != preview.version => {
                        format!("最近一次测试测的是 {}，代码在那之后改过", report.version)
                    }
                    Some(report) => report.headline(),
                    None => "还没有跑过候选测试".into(),
                },
            }));
        }
    }

    let change = Change {
        module: preview.module.clone(),
        version: preview.version.clone(),
        action: Action::Enable,
    };
    Ok(Json(decide(&api, &toolbox, change)?))
}

/// `POST /v1/toolbox/{module}/disable`
pub async fn disable(
    State(api): State<Api>,
    Path(module): Path<String>,
) -> ApiResult<Json<ToolboxChangeResponse>> {
    let toolbox = toolbox_of(&api);
    let info = toolbox.inspect(&module).map_err(failure)?;
    let Some(enabled) = info.enabled else {
        return Err(failure(ToolboxError::NotEnabled {
            module: info.module,
        }));
    };
    let change = Change {
        module: info.module.clone(),
        version: enabled.version.clone(),
        action: Action::Disable,
    };
    Ok(Json(decide(&api, &toolbox, change)?))
}

// ---------------------------------------------------------------- 一次变更

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Enable,
    Disable,
}

impl Action {
    fn apply(
        self,
        toolbox: &Toolbox,
        change: &Change,
        now: time::OffsetDateTime,
    ) -> Result<Enabled, ToolboxError> {
        match self {
            Action::Enable => toolbox.enable(&change.module, Some(&change.version), now),
            Action::Disable => toolbox.disable(&change.module),
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Action::Enable => "启用 toolbox 模块的新版本",
            Action::Disable => "停用一个 toolbox 模块",
        }
    }
}

#[derive(Debug, Clone)]
struct Change {
    module: String,
    version: ModuleVersion,
    action: Action,
}

/// 一次 toolbox 变更：**直接生效**（§7 删掉审批之后没有中间态）。
///
/// 唯一的拒绝与 `write` / `edit` 同一份名单（§8.10）：要写的目标落在 komo 自己的状态里
/// （`sessions/`、`state.db*`、`runtime/`、`.env`）就不装。toolbox 目录默认不在那里，
/// 这一条只挡"把 toolbox 配进了数据目录的受保护部分"这种配置。
fn decide(
    api: &Api,
    toolbox: &Arc<Toolbox>,
    change: Change,
) -> Result<ToolboxChangeResponse, ApiFailure> {
    let snapshot = api.state.snapshot();
    let protected = komo_runtime::tools::protect::Protected::new(
        crate::service::state::protected_paths(&snapshot, &api.state.home),
    );
    let target =
        komo_runtime::tools::paths::real_root(&toolbox.layout().enabled_code(&change.module));
    if let Err(error) = protected.check(&target) {
        return Ok(refused(&change, error.to_string()));
    }
    apply(api, toolbox, &change, api.state.clock.now())
}

/// 真正装上去（或者停掉）。
fn apply(
    api: &Api,
    toolbox: &Toolbox,
    change: &Change,
    now: time::OffsetDateTime,
) -> Result<ToolboxChangeResponse, ApiFailure> {
    let enabled = change.action.apply(toolbox, change, now).map_err(failure)?;
    // 已启用模块的 `__komo_env__` 变了，下一个解释器进程才会带上新变量——那要重启
    // Gateway。这里只记一句，不假装已经生效。
    tracing::info!(
        module = %change.module,
        version = %enabled.version,
        instance = %api.state.instance_id,
        "toolbox 变更已应用"
    );
    Ok(ToolboxChangeResponse {
        module: change.module.clone(),
        status: ChangeStatus::Applied,
        version: Some(enabled.version.clone()),
        enabled: Some(enabled),
        reason: change.action.reason().to_string(),
    })
}

fn refused(change: &Change, reason: String) -> ToolboxChangeResponse {
    ToolboxChangeResponse {
        module: change.module.clone(),
        status: ChangeStatus::Refused,
        version: Some(change.version.clone()),
        enabled: None,
        reason,
    }
}

/// 当前快照里的 toolbox 目录。**不装内置模块**：那是启动时的事（`service::toolbox_of`，
/// §5.5 的「首次启动」）。每次请求都装的话，刚停用的 memos 下一次请求就被装回去——
/// 启用 / 停用直接生效之后（D9），这一步就是"停用不住"。
fn toolbox_of(api: &Api) -> Arc<Toolbox> {
    let toolbox = Arc::new(Toolbox::new(api.state.snapshot().paths.toolbox_dir.clone()));
    if let Err(error) = toolbox.ensure_layout() {
        tracing::warn!(%error, "toolbox 目录建不出来");
    }
    toolbox
}

/// toolbox 的失败 → HTTP。**每一种都有自己的状态码**：404 是"没有这个模块"，
/// 409 是"版本对不上"，422 是"还没测过"——客户端按它决定下一句话说什么。
fn failure(error: ToolboxError) -> ApiFailure {
    use komo_kernel::protocol::http::ErrorCode;
    match &error {
        ToolboxError::NoSuchModule { .. } | ToolboxError::NotEnabled { .. } => {
            ApiFailure::new(ErrorCode::NotFound, error.to_string())
        }
        ToolboxError::BadName(_)
        | ToolboxError::NoCandidate { .. }
        | ToolboxError::NoExports { .. }
        | ToolboxError::NotExported { .. } => {
            ApiFailure::new(ErrorCode::InvalidRequest, error.to_string())
        }
        ToolboxError::VersionMismatch { .. } => {
            ApiFailure::new(ErrorCode::VersionConflict, error.to_string())
        }
        // 「校验候选哈希与已测版本一致」没过——这是一次**可以补救**的失败：跑测试去。
        ToolboxError::Untested { .. } => ApiFailure::new(
            ErrorCode::ConfigInvalid,
            format!("{error}；先跑 komo toolbox test"),
        ),
        ToolboxError::TestRun(_) | ToolboxError::Io { .. } => {
            ApiFailure::new(ErrorCode::Internal, error.to_string())
        }
    }
}

// ---------------------------------------------------------------- 从进程外面问

/// `komo toolbox ...` 用的那几个调用。
///
/// `komo-client` 的 `KomoClient` 是按 §13.1 的接口表写的，而这组端点不在那张表里
/// （见模块开头），所以这里给一组最小的调用，而不是去改 client——这一点与
/// `http::fetch_home_session` 是同一个理由。
pub mod client {
    use super::{
        EnableRequest, ModuleInfo, ToolboxChangeResponse, ToolboxListResponse, ToolboxTestResponse,
    };

    async fn call<T: serde::de::DeserializeOwned>(
        base_url: &str,
        token: Option<&str>,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, String> {
        let url = format!("{}{path}", base_url.trim_end_matches('/'));
        let mut request = reqwest::Client::new().request(method, url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        let status = response.status();
        let text = response.text().await.map_err(|error| error.to_string())?;
        if !status.is_success() {
            // 错误体是 `ErrorBody`——把里面那句话拎出来，操作者要的是它，不是一段 JSON。
            let message = serde_json::from_str::<komo_kernel::protocol::http::ErrorBody>(&text)
                .map(|body| body.error.message)
                .unwrap_or(text);
            return Err(message);
        }
        serde_json::from_str(&text).map_err(|error| format!("{error}（{text}）"))
    }

    pub async fn list(base_url: &str, token: Option<&str>) -> Result<ToolboxListResponse, String> {
        call(base_url, token, reqwest::Method::GET, "/v1/toolbox", None).await
    }

    pub async fn show(
        base_url: &str,
        token: Option<&str>,
        module: &str,
    ) -> Result<ModuleInfo, String> {
        call(
            base_url,
            token,
            reqwest::Method::GET,
            &format!("/v1/toolbox/{module}"),
            None,
        )
        .await
    }

    pub async fn test(
        base_url: &str,
        token: Option<&str>,
        module: &str,
    ) -> Result<ToolboxTestResponse, String> {
        call(
            base_url,
            token,
            reqwest::Method::POST,
            &format!("/v1/toolbox/{module}/test"),
            Some(serde_json::json!({})),
        )
        .await
    }

    pub async fn enable(
        base_url: &str,
        token: Option<&str>,
        module: &str,
        version: Option<String>,
    ) -> Result<ToolboxChangeResponse, String> {
        let body = EnableRequest {
            version: version.map(komo_runtime::toolbox::ModuleVersion),
        };
        call(
            base_url,
            token,
            reqwest::Method::POST,
            &format!("/v1/toolbox/{module}/enable"),
            Some(serde_json::to_value(body).unwrap_or_default()),
        )
        .await
    }

    pub async fn disable(
        base_url: &str,
        token: Option<&str>,
        module: &str,
    ) -> Result<ToolboxChangeResponse, String> {
        call(
            base_url,
            token,
            reqwest::Method::POST,
            &format!("/v1/toolbox/{module}/disable"),
            Some(serde_json::json!({})),
        )
        .await
    }
}
