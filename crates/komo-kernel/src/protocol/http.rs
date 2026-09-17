//! §13.1 最小接口的请求 / 响应类型与统一错误体。
//!
//! 「除最小健康检查外统一认证。提交输入、审批、Cron 与 Memory 变更都支持幂等请求键；
//! 同一键对应不同内容则拒绝。」——所以每个**写**接口都带一个可选的
//! [`crate::types::ids::RequestKey`]，而 [`ErrorCode::RequestKeyConflict`] 是它对应的
//! 那个明确失败。
//!
//! 几个端点的响应类型在这里**定死**，免得服务端和客户端各自猜一个：
//!
//! | 端点 | 请求体 | 响应体 |
//! |---|---|---|
//! | `POST /v1/sessions` | [`CreateSessionRequest`] | [`SessionSummary`] |
//! | `POST /v1/sessions/{id}/boundary` | [`BoundaryRequest`] | [`BoundaryResponse`] |
//! | `POST /v1/cron` | [`CreateCronRequest`] | [`CronJob`] |
//! | `PATCH /v1/cron/{id}` | [`UpdateCronRequest`] | [`CronJob`] |
//! | `POST /v1/cron/{id}/run` | [`ManualCronRunRequest`] | [`ManualCronRunResponse`] |
//! | `POST /v1/memories/{id}/confirm` | [`MemoryRevisionRequest`] | [`MemoryDetail`] |
//! | `POST /v1/memories/{id}/forget` | [`MemoryRevisionRequest`] | [`MemoryDetail`] |
//! | `GET /v1/models` | — | [`ModelsResponse`] |
//! | `GET /v1/config/check` | — | [`ConfigCheckResponse`] |
//! | `POST /v1/config/reload` | — | [`ConfigReloadResponse`]，失败走 [`ErrorBody`] |
//!
//! `confirm` 与 `forget` 都返回**整条**记忆而不是一个 `{ok:true}`：它们带着预期
//! revision 来，回去的那条才说得清现在是第几版（§9.6 的幂等就是靠这个比出来的）。

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::config::{ConfigIssue, KeyPath, SourceFile};
use crate::cron::{CronFiring, CronJob, JobStatus, NotifyPolicy, OverlapPolicy};
use crate::types::chat::{ApprovalScope, PeerId};
use crate::types::ids::{
    ApprovalId, CronJobId, MemoryId, RequestKey, RunId, Seq, SessionId, ShortId,
};
use crate::types::memory::{MemoryItem, MemoryScope, MemoryState, RetrievalMode};
use crate::types::model::{Effort, EmbeddingSpace, ModelConfig};
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

/// `POST /v1/sessions/{id}/boundary`：`/new`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryRequest {
    /// 谁划的这一刀。聊天渠道带上发送者，TUI 不带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<PeerId>,
}

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

/// `GET /v1/approvals` 的 query。
///
/// `include_decided` 存在是因为 `komo run inspect` 要答「这一步是谁放行的」——那条审批
/// 早就不在待处理集合里了，按默认的 pending 过滤永远查不到它（§7.4 的审计面）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalListQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// 默认 `false` = 只列待处理。
    #[serde(default)]
    pub include_decided: bool,
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
    /// 每个 Job 的**最近一次触发**与下一次时间（§10「Cron 的结果去原 Session 查看」的
    /// 入口）。与 `jobs` 按 `job` 对齐，不按下标——一个读不出来的 Job 会从 `jobs` 里
    /// 掉出去，按下标配对就会错位。
    #[serde(default)]
    pub status: Vec<CronJobStatus>,
}

/// 一个 Job 的运行面：下一次什么时候，上一次怎么样。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronJobStatus {
    pub job: CronJobId,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_run_at: Option<OffsetDateTime>,
    /// 最近一次触发。没有就是这个 Job 还没响过。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<CronFiring>,
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
    /// 触发结果的投递规则（§10 的 `notify`）。缺省 `always`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<NotifyPolicy>,
    /// **完整**模型配置覆盖（§10：「覆盖按完整模型配置解析」）。给了它就整份用它，
    /// `model` 那个名字被忽略——不拿一个模型名去配另一个端点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_config: Option<ModelConfig>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 见 [`CreateCronRequest::model_config`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_config: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<NotifyPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

impl UpdateCronRequest {
    /// 这次 PATCH 改的是**定义**吗（§7.2 / §10：改定义 → 版本 +1，绑定这个 Job 的授权
    /// 失效）。
    ///
    /// `status` 单独一个不算：`pause` / `resume` 不是定义变更，让它递增版本等于让操作者
    /// 每暂停一次就重批一遍授权。
    pub fn changes_definition(&self) -> bool {
        self.name.is_some()
            || self.schedule.is_some()
            || self.timezone.is_some()
            || self.prompt.is_some()
            || self.overlap.is_some()
            || self.max_rounds.is_some()
            || self.workdir.is_some()
            || self.skills.is_some()
            || self.model.is_some()
            || self.model_config.is_some()
            || self.effort.is_some()
            || self.notify.is_some()
    }
}

/// `POST /v1/cron/{id}/run`：手动触发。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualCronRunRequest {
    /// 不给就由服务端铸一个。无论哪种，它都是一个**独立**的键——手动触发不冒充定时
    /// 触发（§10）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

/// **使用独立请求幂等键，不冒充定时触发**（§10）。
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
    /// **query string 里是一行文本**（`personal` / `project:<id>` / `environment:<id>`），
    /// JSON 里是一个带 `kind` 的对象——[`MemoryScope`] 的文档说的就是这两种写法。
    ///
    /// 这里必须有一个自定义反序列化：`?scope=` 送来的是字符串，而 `MemoryScope` 是内部
    /// 标记的枚举，`serde_urlencoded` 对着它只会报 "expected internally tagged enum"。
    /// 两种写法都收，于是文档里那句往返关系才真的成立。
    #[serde(
        default,
        deserialize_with = "scope_from_text_or_object",
        skip_serializing_if = "Option::is_none"
    )]
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

/// `?scope=project:komo` 与 `{"kind":"project","project_id":"komo"}` 都收。
fn scope_from_text_or_object<'de, D>(deserializer: D) -> Result<Option<MemoryScope>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Text(String),
        Object(MemoryScope),
    }

    match Option::<Either>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Either::Object(scope)) => Ok(Some(scope)),
        Some(Either::Text(text)) if text.trim().is_empty() => Ok(None),
        Some(Either::Text(text)) => text
            .parse::<MemoryScope>()
            .map(Some)
            .map_err(D::Error::custom),
    }
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

// ---- GET /v1/models ----

/// 可选模型清单里的一项。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMenuEntry {
    /// 配置目录 alias；提交 Run / Cron 时使用它。
    pub id: String,
    /// 面向人的显示名；省略时等于 alias。
    #[serde(default)]
    pub name: String,
    /// 上游服务使用的真实 model id。
    #[serde(default)]
    pub model: String,
    /// 连接默认值来自哪个 `model_providers.<name>`；独立配置时为 `standalone`。
    pub provider: String,
    /// 实际协议适配器，如 `responses` / `chat_completions`。
    #[serde(default)]
    pub api_backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// 这个模型支持的档位。**空表就是"一档都不支持"**，不是"还不知道"——不知道的模型
    /// 不该出现在给人挑的清单里（§13.3「能力未知……无法确定时拒绝该显式参数」）。
    #[serde(default)]
    pub efforts: Vec<Effort>,
    /// 当前配置里的主模型。
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub models: Vec<ModelMenuEntry>,
}

// ---- /v1/config ----

/// `GET /v1/config/check`：只读，**不改变运行中的 Gateway**（§3 的命令表）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigCheckResponse {
    /// 空 = 校验通过。
    #[serde(default)]
    pub issues: Vec<ConfigIssue>,
    /// 当前**生效**配置的加载时间。
    #[serde(with = "time::serde::rfc3339")]
    pub loaded_at: OffsetDateTime,
    /// 各来源文件与它们的 mtime。和 `loaded_at` 对不上就是"文件改了但没装上"（§3）。
    #[serde(default)]
    pub sources: Vec<SourceFile>,
}

/// `POST /v1/config/reload`：校验通过才装，装完报告差异。
///
/// **校验不过不走这个响应**——走 [`ErrorBody`] 的 [`ErrorCode::ConfigInvalid`]，`keys`
/// 带定位；旧快照原样保留（§3 第 1 步：「校验不过的配置永远不会被装上，哪怕只错一个
/// 键」）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigReloadResponse {
    /// 变化了的键名，**不带值**（§3 第 3 步）。
    #[serde(default)]
    pub changed: Vec<KeyPath>,
    /// 其中只在启动时生效的那些——这些**没有**生效，要 `komo gateway restart`（§3 第 4
    /// 步：不静默忽略，也不假装已生效）。
    #[serde(default)]
    pub start_only: Vec<KeyPath>,
    /// 装上了，但有话要说（例如"某渠道 enabled 但 allow_from 为空"）。
    #[serde(default)]
    pub warnings: Vec<ConfigIssue>,
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

    /// `?scope=` 的两种写法都收得下——文档里那句"两种写法必须能互相还原"落到接口上。
    ///
    /// 这里用 JSON 的字符串值走**同一个**反序列化分支；真正的 query string 那一路在
    /// `komo-gateway` 的 `tests/memory` 里端到端测（kernel 不依赖 `serde_urlencoded`）。
    #[test]
    fn a_scope_arrives_either_as_text_or_as_an_object() {
        let from_text: MemoryListQuery =
            serde_json::from_str(r#"{"scope":"project:komo","state":"candidate"}"#)
                .expect("一行文本收得下");
        assert_eq!(
            from_text.scope,
            Some(MemoryScope::Project {
                project_id: "komo".into()
            })
        );
        assert_eq!(from_text.state, Some(MemoryState::Candidate));

        let from_object: MemoryListQuery =
            serde_json::from_str(r#"{"scope":{"kind":"environment","instance_id":"nas"}}"#)
                .expect("一个对象也收得下");
        assert_eq!(
            from_object.scope,
            Some(MemoryScope::Environment {
                instance_id: "nas".into()
            })
        );

        assert_eq!(
            serde_json::from_str::<MemoryListQuery>(r#"{"query":"x"}"#)
                .unwrap()
                .scope,
            None,
            "不写就是不筛"
        );
        assert!(
            serde_json::from_str::<MemoryListQuery>(r#"{"scope":"team:x"}"#).is_err(),
            "写错了要报错，不是静默不筛"
        );
    }

    #[test]
    fn a_submit_response_without_the_dedup_flag_reads_as_new() {
        let response: SubmitRunResponse =
            serde_json::from_str(r#"{"run":"run-1","session":"sess-1","seq":1,"status":"queued"}"#)
                .unwrap();
        assert!(!response.deduplicated);
    }
}
