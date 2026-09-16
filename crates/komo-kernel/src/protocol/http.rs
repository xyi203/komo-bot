//! §13.1 最小接口的请求 / 响应类型与统一错误体。
//!
//! 「除最小健康检查外统一认证。提交输入、审批、Cron 与 Memory 变更都支持幂等请求键；
//! 同一键对应不同内容则拒绝。」——所以每个**写**接口都带一个可选的
//! [`crate::types::ids::RequestKey`]，而 [`ErrorCode::RequestKeyConflict`] 是它对应的
//! 那个明确失败。

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::config::KeyPath;
use crate::cron::{CronJob, JobStatus, OverlapPolicy};
use crate::types::chat::{ApprovalScope, PeerId};
use crate::types::ids::{
    ApprovalId, CronJobId, MemoryId, RequestKey, RunId, Seq, SessionId, ShortId,
};
use crate::types::memory::{MemoryItem, MemoryScope, MemoryState, RetrievalMode};
use crate::types::model::{Effort, EmbeddingSpace};
use crate::types::plan::{ExecutionPlan, PlanHash, PlanSource};
use crate::types::status::RunStatus;

/// 统一错误体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ApiError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    /// 与错误相关的键名（配置校验用）。不带值。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<KeyPath>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            keys: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    Unauthorized,
    /// 同一请求键对应不同内容。
    RequestKeyConflict,
    /// 预期版本 / revision 不符（Memory 的 confirm / forget，文件覆盖）。
    VersionConflict,
    /// 请求本身不合法。
    InvalidRequest,
    /// 被 Policy 拒绝。
    Denied,
    /// 这个状态下做不了这件事（例如对已终态的 Run 取消）。
    Conflict,
    /// 配置校验不过。
    ConfigInvalid,
    /// 向量后端不可用，而调用方明确选了 vector 模式（§9.4）。
    VectorUnavailable,
    /// 引用的正文缺失或哈希不符——停止受影响任务，不重跑来掩盖损坏（§8.5）。
    Corrupt,
    Internal,
}

// ---- GET /healthz ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// 实例身份。**不能只凭 PID 或端口判断**（§3）。
    pub instance_id: String,
    pub version: String,
    pub protocol_version: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    pub data_dir: String,
}

// ---- /v1/sessions ----

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session: SessionId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_status: Option<RunStatus>,
    pub applied_seq: Seq,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub summary: SessionSummary,
    /// 未完成的 Run。
    #[serde(default)]
    pub unfinished: Vec<RunSummary>,
    #[serde(default)]
    pub pending_approvals: Vec<ApprovalRecord>,
}

// ---- GET /v1/sessions/{id}/events ----

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventQuery {
    /// 从哪个游标读起（不含）。
    #[serde(default)]
    pub from: Seq,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventPage {
    pub session: SessionId,
    pub events: Vec<crate::events::Event>,
    pub next: Seq,
    #[serde(default)]
    pub more: bool,
}

// ---- POST /v1/sessions/{id}/runs ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitRunRequest {
    /// 幂等键。同一键重发返回原 Run（§8.5）。
    pub request_key: RequestKey,
    pub text: String,
    /// 本次 Run 的模型覆盖；不给就用当前快照的主模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitRunResponse {
    pub run: RunId,
    pub session: SessionId,
    pub seq: Seq,
    pub status: RunStatus,
    /// 这次命中了同一请求键的原 Run。
    #[serde(default)]
    pub deduplicated: bool,
}

// ---- POST /v1/sessions/{id}/resume ----

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeRequest {
    /// 操作者明确切换模型时带上——要记 `config.changed` 并重新验证协议历史可回放性
    /// （§13.3）。不带 = 沿用原运行的模型 / effort。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeResponse {
    pub session: SessionId,
    /// 已经在跑或刚被接续的 Run。
    #[serde(default)]
    pub resumed: Vec<RunSummary>,
    /// 需要人处理的：审批、时效、结果不明（§8.4）。
    #[serde(default)]
    pub pending: Vec<PendingItem>,
}

/// "2 个任务已接续，1 个等待审批"里的那一项（§8.8）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingItem {
    /// `ApprovalRecord` 里带着整份 `ExecutionPlan`，比别的变体大一个数量级，所以装箱。
    Approval(Box<ApprovalRecord>),
    /// 结果不明，等人判断。
    Uncertain {
        run: RunId,
        call: crate::types::ids::ToolCallId,
        reason: String,
    },
    NeedsAttention {
        run: RunId,
        reason: String,
    },
}

// ---- POST /v1/sessions/{id}/boundary ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryResponse {
    pub session: SessionId,
    pub seq: Seq,
}

// ---- /v1/runs ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run: RunId,
    pub session: SessionId,
    pub status: RunStatus,
    pub source: PlanSource,
    #[serde(default)]
    pub rounds: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub ended_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunDetail {
    #[serde(flatten)]
    pub summary: RunSummary,
    #[serde(default)]
    pub calls: Vec<ToolCallSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    /// 本次 Run 用到的记忆条目（审计证据，§9.7）。
    #[serde(default)]
    pub memories: Vec<crate::types::turn::MemoryUse>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub call: crate::types::ids::ToolCallId,
    pub tool: String,
    pub state: crate::types::status::ToolCallState,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_hash: Option<PlanHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<crate::types::refs::OutputRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRunRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRunResponse {
    pub run: RunId,
    pub status: RunStatus,
}

// ---- /v1/approvals ----

/// 一条审批请求（`approval_requests`）。也是 `GET /v1/approvals/{id}` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub approval: ApprovalId,
    /// 待处理集合内唯一的 4 位短 ID。
    pub short_id: ShortId,
    pub session: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<crate::types::ids::ToolCallId>,
    pub plan_hash: PlanHash,
    /// 具体动作。§7.2 要求界面显示它。
    pub plan: ExecutionPlan,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(default)]
    pub scopes: Vec<ApprovalScope>,
    #[serde(with = "time::serde::rfc3339")]
    pub requested_at: OffsetDateTime,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub valid_until: Option<OffsetDateTime>,
    /// 已经决定过的话，决定本身。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<ApprovalDecisionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecisionRecord {
    pub approved: bool,
    pub scope: ApprovalScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<PeerId>,
    #[serde(with = "time::serde::rfc3339")]
    pub decided_at: OffsetDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<crate::types::ids::GrantId>,
    /// 这条授权有没有被消费过。
    #[serde(default)]
    pub consumed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalListResponse {
    pub approvals: Vec<ApprovalRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecisionRequest {
    pub approved: bool,
    /// `/approve <id>` 是 `Once`，`/approve <id> run` 是 `Run`（§11.3）。
    #[serde(default = "scope_once")]
    pub scope: ApprovalScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

fn scope_once() -> ApprovalScope {
    ApprovalScope::Once
}

/// 决定的结果。**已决定的返回原决定，不报错**（§11.3）——同一人连点两次，第二次得到
/// 的是"已决定"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecisionResponse {
    pub approval: ApprovalId,
    pub short_id: ShortId,
    pub decision: ApprovalDecisionRecord,
    /// 这次请求没有改变任何东西，返回的是之前那个决定。
    #[serde(default)]
    pub already_decided: bool,
}

// ---- /v1/cron ----

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CronListResponse {
    pub jobs: Vec<CronJob>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateCronRequest {
    pub name: String,
    /// 五字段 cron 表达式或 `@at YYYY-MM-DD HH:MM`。
    pub schedule: String,
    /// IANA 名字。Gateway 侧解析成偏移。
    pub timezone: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub overlap: OverlapPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

/// `PATCH /v1/cron/{id}`：给出的字段才改。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdateCronRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// `pause` / `resume` 就是把它改成 `paused` / `active`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<JobStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap: Option<OverlapPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
}

/// `POST /v1/cron/{id}/run`：手动触发。**使用独立请求幂等键，不冒充定时触发**（§10）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualCronRunResponse {
    pub job: CronJobId,
    pub session: SessionId,
    pub run: RunId,
    pub request_key: RequestKey,
}

/// `DELETE /v1/cron/{id}`：移除后续调度，已有执行历史保留。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronDeleteResponse {
    pub job: CronJobId,
    pub removed: bool,
    /// 保留下来的历史触发条数。
    #[serde(default)]
    pub firings_kept: u32,
}

// ---- /v1/memories ----

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryListQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<MemoryScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<MemoryState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RetrievalMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryListResponse {
    pub memories: Vec<MemoryItem>,
    /// 检索降级要明说（§9.4）。
    #[serde(default)]
    pub degraded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
}

/// confirm / forget 都携带预期 revision；重复请求幂等（§9.6）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRevisionRequest {
    pub expected_revision: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryDetail {
    pub memory: MemoryItem,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryIndexStatus {
    /// 当前向量空间的指纹来源。没有配置 embedding 时为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<EmbeddingSpace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    pub state: IndexState,
    /// 已经有当前代次向量的条目占比。
    #[serde(default)]
    pub coverage: f32,
    #[serde(default)]
    pub indexed: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexState {
    /// 没有配置向量模型。
    Unconfigured,
    Building,
    Ready,
    Failed,
}

/// `POST /v1/memory-index/rebuild`：幂等提交（§13.1）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildIndexRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildIndexResponse {
    pub generation: String,
    /// 这次是不是新提交的（false = 命中了正在跑的同一个代次）。
    #[serde(default)]
    pub accepted: bool,
}

/// `GET /v1/memories/{id}` 的 404、`POST .../confirm` 的 409 都用同一个错误体，见
/// [`ErrorBody`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRef {
    pub memory: MemoryId,
    pub revision: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_body_round_trips() {
        let body = ErrorBody {
            error: ApiError::new(ErrorCode::RequestKeyConflict, "同一请求键对应不同内容"),
        };
        let text = serde_json::to_string(&body).unwrap();
        assert!(text.contains("request_key_conflict"), "{text}");
        assert_eq!(serde_json::from_str::<ErrorBody>(&text).unwrap(), body);
    }

    #[test]
    fn a_decision_request_defaults_to_this_call_only() {
        let request: ApprovalDecisionRequest =
            serde_json::from_str(r#"{"approved":true}"#).unwrap();
        assert_eq!(request.scope, ApprovalScope::Once);
    }

    #[test]
    fn a_submit_response_without_the_dedup_flag_reads_as_new() {
        let response: SubmitRunResponse =
            serde_json::from_str(r#"{"run":"run-1","session":"sess-1","seq":1,"status":"queued"}"#)
                .unwrap();
        assert!(!response.deduplicated);
    }
}
