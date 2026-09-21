//! `/v1/toolbox`：toolbox 的清单、候选测试与**启用 / 停用**（§5.3、§5.4）。
//!
//! toolbox 是文件系统上的东西，`komo toolbox list` 大可以自己去读那个目录。它**不**
//! 那么做，理由只有一条：**启用要审批**（§7.1 第 4 行「修改启用中的 toolbox → Ask，
//! 展示具体差异」）。审批住在 state.db 里，而 state.db 只有 Gateway 那个进程打得开
//! （§12）。一半走 Gateway、一半直接读盘的 CLI 会给出两种互相矛盾的"当前版本"，所以
//! 整组命令都走这里。
//!
//! 这组接口**不在 §13.1 的那张表里**——见报告的"待拍板"。类型也定义在这个文件里，
//! kernel 一个字没改。
//!
//! 启用的两条路，同一段代码：
//!
//! ```text
//! POST /v1/toolbox/{m}/enable
//!   → Toolbox::enable_preview          版本差异 + 候选测试结果
//!   → ExecutionPlan{ ToolboxChange }   绑定目标版本与环境
//!   → PolicyEngine::decide
//!       Allow → 直接装
//!       Ask   → 写一条审批（changes = diff，evidence = 测试结果）
//!               → 投到 home chat（§11.4）
//!               → 盯着它，批准了就装
//!       Deny  → 拒绝
//! ```
//!
//! 「批准后**再次校验**目标和版本，变化则重新评估」（§7.2）——`Toolbox::enable` 的第一
//! 件事就是拿审批时那个版本号和现在的候选对账，审批窗口里有人改过候选就装不上去。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path, State};
use komo_kernel::protocol::http::ApprovalRecord;
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::ids::{ApprovalId, OperationId, ShortId};
use komo_kernel::types::plan::{
    ConsumeIntent, ExecutionPlan, Operation, PlanSource, PlanTarget, PlanVersions, RecoveryMode,
    TargetAccess,
};
use komo_kernel::types::tool::WorkspaceRoot;
use komo_runtime::approvals::{ApprovalOutcome, ApprovalRequest};
use komo_runtime::policy::{DecisionEnv, PolicyEngine};
use komo_runtime::toolbox::{
    Enabled, ModuleInfo, ModuleVersion, TestReport, Toolbox, ToolboxError,
};
use serde::{Deserialize, Serialize};

use super::Api;
use super::error::{ApiFailure, ApiResult};

/// 盯一条待处理启用要多久看一眼。
const POLL: std::time::Duration = std::time::Duration::from_millis(200);
/// 最多盯多久——比一条审批默认的有效期（24h）略长一点就够了。
const WATCH_LIMIT: std::time::Duration = std::time::Duration::from_secs(25 * 3600);

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

/// 一次启用 / 停用的去向。**三种状态各有名字**：操作者要按它决定下一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    /// 装上了（Policy 直接 Allow，或者批准之后再来了一次）。
    Applied,
    /// 等审批。`approval` / `short_id` 是回答它的那两个号。
    Pending,
    /// 被拒绝——操作者拒的，或者规则拒的。
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<ShortId>,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnableRequest {
    /// 要启用的版本。给了就必须与当前候选逐字相同（§5.4「校验候选哈希与已测版本一致」）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ModuleVersion>,
}

/// 进程里那些"已经问出去、还没答复"的启用。
///
/// 存在的理由只有一个：**同一个模块被问第二次时不该产生第二条审批**。真正把候选装上去
/// 的是 [`watch`] 那个后台任务（`/approve` 从聊天里来，不经过这个进程的 HTTP 层），
/// 这张表只回答"这个模块现在停在哪条审批上"。
#[derive(Debug, Default)]
pub struct PendingEnables(Mutex<BTreeMap<String, ApprovalId>>);

impl PendingEnables {
    pub fn new() -> Self {
        PendingEnables(Mutex::new(BTreeMap::new()))
    }

    fn get(&self, module: &str) -> Option<ApprovalId> {
        self.0.lock().expect("待处理启用表").get(module).cloned()
    }

    fn set(&self, module: &str, approval: ApprovalId) {
        self.0
            .lock()
            .expect("待处理启用表")
            .insert(module.to_string(), approval);
    }

    fn clear(&self, module: &str) {
        self.0.lock().expect("待处理启用表").remove(module);
    }
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
/// **它不产生审批**，因为发起它的是拿着操作者令牌的 CLI ——「候选执行……按真实权限
/// 评估」里那个"真实权限"在这条路上就是操作者本人。模型要跑候选代码走的是 `python` 的
/// `code` 模式，那一条照常过 Policy。见报告的"待拍板"。
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

    // 「运行测试并保存结果」排在「Policy 审核启用操作」**前面**（§5.4）——所以一个没
    // 测过的候选连审批都不该产生：那条请求会把"版本差异与测试结果"这一项摆出一个空
    // 位，而操作者要的正是看着那份证据决定批不批。
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
        changes: preview.changes_block(),
        evidence: preview.evidence(),
    };
    Ok(Json(decide(&api, &toolbox, change).await?))
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
        changes: format!(
            "{}：停用 {}（正文移出 toolbox/，快照与元数据保留）",
            info.module, enabled.version
        ),
        evidence: match &enabled.tests {
            Some(report) => report.evidence(),
            None => format!("{} 启用时没有留下测试结果", info.module),
        },
    };
    Ok(Json(decide(&api, &toolbox, change).await?))
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
    changes: String,
    evidence: String,
}

/// 一次 toolbox 变更走完 Policy 那条梯子。
async fn decide(
    api: &Api,
    toolbox: &Arc<Toolbox>,
    change: Change,
) -> Result<ToolboxChangeResponse, ApiFailure> {
    let now = api.state.clock.now();
    let plan = plan_of(api, toolbox, &change).await?;

    // 已经在等一条审批了：拿它的决定，不新建第二条（§7.4 的"不重新问人"）。
    if let Some(approval) = api.toolbox.get(&change.module) {
        match api
            .state
            .approvals
            .settle(&approval, &plan, ConsumeIntent::First)
            .await?
        {
            ApprovalOutcome::Pending(record) => {
                return Ok(pending(&change, &record));
            }
            ApprovalOutcome::Approved { .. } => {
                api.toolbox.clear(&change.module);
                return apply(api, toolbox, &change, now, "操作者已批准");
            }
            ApprovalOutcome::Denied { reason, .. } => {
                api.toolbox.clear(&change.module);
                return Ok(refused(&change, reason));
            }
            // 计划 / 版本 / 有效期变了：那条审批管不到眼前这份计划，重新审核。
            ApprovalOutcome::Stale { .. } | ApprovalOutcome::Missing { .. } => {
                api.toolbox.clear(&change.module);
            }
        }
    }

    let snapshot = api.state.snapshot();
    // 与执行器、核对那两份同一个名单（§8.10 第 4 条）：toolbox 的启用是第三个判决入口。
    let policy = PolicyEngine::from_rules(snapshot.policy.clone()).with_protection(
        crate::service::state::protected_paths(&snapshot, &api.state.home),
    );
    let roots = [WorkspaceRoot {
        path: toolbox.layout().root().to_path_buf(),
        writable: true,
        label: "toolbox".into(),
    }];
    let env = DecisionEnv {
        grants: &[],
        principal: None,
        roots: &roots,
        now,
    };
    match policy.decide(&plan, &env) {
        komo_kernel::policy::PolicyDecision::Allow { reason } => {
            apply(api, toolbox, &change, now, &reason)
        }
        komo_kernel::policy::PolicyDecision::Deny { reason } => Ok(refused(&change, reason)),
        komo_kernel::policy::PolicyDecision::Ask { reason, .. } => {
            let session = api.state.default_main_session().await?;
            let record = api
                .state
                .approvals
                .request(ApprovalRequest {
                    session,
                    run: None,
                    call: None,
                    plan: plan.clone(),
                    reason,
                    // §7.2：界面要显示**改动**与**已有验证结果**。toolbox 的"改动"就是
                    // 上一版与候选的 diff，"验证"就是候选测试的结果。
                    changes: Some(change.changes.clone()),
                    evidence: Some(change.evidence.clone()),
                    // 范围只给"本次"：一条"今后任意 toolbox 变更都行"的授权正是
                    // §7.2 最后一句要挡的东西。
                    scopes: vec![ApprovalScope::Once],
                })
                .await?;
            api.toolbox.set(&change.module, record.approval.clone());

            // 投到 home chat（§11.4：没有来源会话的请求只有这一个出口）。
            if let Err(error) = api
                .state
                .notifier
                .deliver_approval(None, komo_runtime::approvals::presentation(&record))
                .await
            {
                tracing::warn!(%error, module = %change.module, "toolbox 启用的审批投不出去");
            }
            watch(
                api.clone(),
                Arc::clone(toolbox),
                change.clone(),
                plan,
                record.approval.clone(),
            );
            Ok(pending(&change, &record))
        }
    }
}

/// 批准之后真正装上去。
fn apply(
    api: &Api,
    toolbox: &Toolbox,
    change: &Change,
    now: time::OffsetDateTime,
    reason: &str,
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
        approval: None,
        short_id: None,
        reason: reason.to_string(),
    })
}

/// 盯着一条待处理的启用，批准了就装上（§7.4 的"继续前重新校验并消费授权"）。
///
/// 为什么要一个后台任务：`/approve` 通常从**聊天里**来，它落在 state.db 上，没有人
/// 会替这次启用再发一次 HTTP。没有这一步，操作者在飞书里点了"批准"，模块却要等到他
/// 想起来再敲一次 `komo toolbox enable` 才装上。
fn watch(
    api: Api,
    toolbox: Arc<Toolbox>,
    change: Change,
    plan: ExecutionPlan,
    approval: ApprovalId,
) {
    tokio::spawn(async move {
        let deadline = std::time::Instant::now() + WATCH_LIMIT;
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            let outcome = match api
                .state
                .approvals
                .settle(&approval, &plan, ConsumeIntent::First)
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::warn!(%error, module = %change.module, "读不出这条 toolbox 审批");
                    continue;
                }
            };
            match outcome {
                ApprovalOutcome::Pending(_) => continue,
                ApprovalOutcome::Approved { .. } => {
                    api.toolbox.clear(&change.module);
                    let now = api.state.clock.now();
                    let text = match change.action.apply(&toolbox, &change, now) {
                        Ok(enabled) => {
                            format!("toolbox：{} 已切换到 {}", change.module, enabled.version)
                        }
                        // **批准了也可能装不上**：审批窗口里有人改过候选（§7.2「批准后
                        // 再次校验目标和版本，变化则重新评估」）。说出来，不静默。
                        Err(error) => format!("toolbox：{} 没能装上——{error}", change.module),
                    };
                    let _ = api
                        .state
                        .notifier
                        .deliver_home(komo_kernel::types::chat::Outbound::Text { text })
                        .await;
                    return;
                }
                ApprovalOutcome::Denied { .. }
                | ApprovalOutcome::Stale { .. }
                | ApprovalOutcome::Missing { .. } => {
                    api.toolbox.clear(&change.module);
                    return;
                }
            }
        }
        api.toolbox.clear(&change.module);
    });
}

/// 这次变更的执行计划（§7.2）。
///
/// 绑三样：**目标版本**（候选的 [`ModuleVersion`]）、**环境版本**、以及真实目标路径。
/// 版本进 [`PlanVersions::module`]，所以一条按这份计划批下来的授权，在候选改过之后
/// 自动覆盖不到新的那一份。
async fn plan_of(
    api: &Api,
    toolbox: &Arc<Toolbox>,
    change: &Change,
) -> Result<ExecutionPlan, ApiFailure> {
    let session = api.state.default_main_session().await?;
    Ok(ExecutionPlan {
        operation_id: OperationId::new_at(api.state.clock.now()),
        source: PlanSource::Interactive { session },
        tool: "toolbox".into(),
        operation: Operation::ToolboxChange {
            module: komo_runtime::toolbox::dotted(&change.module),
        },
        run: None,
        tool_call: None,
        args: serde_json::json!({
            "action": match change.action { Action::Enable => "enable", Action::Disable => "disable" },
            "module": change.module,
            "version": change.version,
        }),
        cwd: Some(toolbox.layout().root().to_path_buf()),
        targets: vec![PlanTarget {
            path: toolbox.layout().enabled_code(&change.module),
            access: TargetAccess::Write,
            expected_version: None,
        }],
        versions: PlanVersions {
            code: None,
            module: Some(change.version.to_string()),
            env: python_env_version(api, toolbox).await,
        },
        resources: vec![],
        // 装一份代码是个可核对的动作：目标要么已经是这一版，要么还是上一版。核对由
        // `Toolbox::enable` 自己的版本对账做，所以这里是"可以核对目标状态"。
        recovery: RecoveryMode::VerifyTarget,
    })
}

/// 这台 Gateway 现在的 Python 环境版本。探不到就不绑——**不编一个**。
///
/// 探一次要起一个子进程，所以走 `tokio::process`：这是在 axum 的 handler 里，一次同步
/// 的 `std::process` 会把整个执行线程按住。
async fn python_env_version(
    api: &Api,
    toolbox: &Arc<Toolbox>,
) -> Option<komo_kernel::types::plan::EnvVersion> {
    let config = crate::service::python_env(&api.state.config, toolbox);
    let interpreter = config.interpreter_path();
    let output = tokio::process::Command::new(&interpreter)
        .arg("-c")
        .arg("import sys; print('.'.join(map(str, sys.version_info[:3])))")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(komo_runtime::python_runtime::env_version_of(
        &version,
        config.requirements.as_deref(),
    ))
}

fn pending(change: &Change, record: &ApprovalRecord) -> ToolboxChangeResponse {
    ToolboxChangeResponse {
        module: change.module.clone(),
        status: ChangeStatus::Pending,
        version: Some(change.version.clone()),
        enabled: None,
        approval: Some(record.approval.clone()),
        short_id: Some(record.short_id.clone()),
        reason: format!(
            "{}：等操作者批准（/approve {}）",
            change.action.reason(),
            record.short_id
        ),
    }
}

fn refused(change: &Change, reason: String) -> ToolboxChangeResponse {
    ToolboxChangeResponse {
        module: change.module.clone(),
        status: ChangeStatus::Refused,
        version: Some(change.version.clone()),
        enabled: None,
        approval: None,
        short_id: None,
        reason,
    }
}

fn toolbox_of(api: &Api) -> Arc<Toolbox> {
    crate::service::toolbox_of(&api.state.snapshot())
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
