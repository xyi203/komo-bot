//! §13.1 最小接口的请求 / 响应类型与统一错误体。
//!
//! 「除最小健康检查外统一认证。提交输入、Cron 与 Memory 变更都支持幂等请求键；
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
//! | `POST /v1/memories/{id}/classify` | [`MemoryClassifyRequest`] | [`MemoryDetail`] |
//! | `POST /v1/memories/{id}/forget` | [`MemoryRevisionRequest`] | [`MemoryDetail`] |
//! | `POST /v1/memories/{id}/restore` | [`MemoryRevisionRequest`] | [`MemoryDetail`] |
//! | `POST /v1/memory-sweep` | [`MemorySweepRequest`] | [`MemorySweepResponse`] |
//! | `POST /v1/memory-consolidate` | [`MemoryConsolidateRequest`] | [`MemoryConsolidateResponse`] |
//! | `POST /v1/matters` | [`MatterAddRequest`] | [`MatterDetail`] |
//! | `POST /v1/matters/{id}/{action}` | [`MatterActionRequest`] | [`MatterDetail`] |
//! | `GET /v1/models` | — | [`ModelsResponse`] |
//! | `GET /v1/config/check` | — | [`ConfigCheckResponse`] |
//! | `POST /v1/config/reload` | — | [`ConfigReloadResponse`]，失败走 [`ErrorBody`] |
//! | `GET /v1/config/models` | — | [`ConfigModelsResponse`] |
//! | `PUT /v1/config/model-providers/{id}` | [`ProviderUpsertRequest`] | [`ConfigEditResponse`] |
//! | `DELETE /v1/config/model-providers/{id}` | [`ConfigVersionRequest`] | [`ConfigEditResponse`] |
//! | `PUT /v1/config/models/{alias}` | [`ModelUpsertRequest`] | [`ConfigEditResponse`] |
//! | `DELETE /v1/config/models/{alias}` | [`ConfigVersionRequest`] | [`ConfigEditResponse`] |
//! | `PUT /v1/config/default-model` | [`DefaultModelRequest`] | [`ConfigEditResponse`] |
//! | `GET /v1/config/settings` | — | [`ConfigSettingsResponse`] |
//! | `PUT /v1/config/settings/{key}` | [`SettingRequest`] | [`ConfigEditResponse`] |
//! | `GET /v1/config/env` | — | [`ConfigEnvResponse`] |
//! | `PUT /v1/config/env` | [`EnvSetRequest`] | [`ConfigEditResponse`] |
//! | `DELETE /v1/config/env/{name}` | [`ConfigVersionRequest`] | [`ConfigEditResponse`] |
//! | `PUT /v1/config/embedding-models/{alias}` | [`EmbeddingUpsertRequest`] | [`ConfigEditResponse`] |
//! | `DELETE /v1/config/embedding-models/{alias}` | [`ConfigVersionRequest`] | [`ConfigEditResponse`] |
//! | `GET /v1/mcp` | — | [`McpServersResponse`] |
//! | `PUT /v1/config/provider-groups/{name}` | [`ProviderGroupRequest`] | [`ConfigEditResponse`] |
//! | `GET /v1/channels` | — | [`ChannelsResponse`] |
//! | `POST /v1/channels/{id}/probe` | — | [`ChannelProbeResponse`] |
//!
//! `confirm` 与 `forget` 都返回**整条**记忆而不是一个 `{ok:true}`：它们带着预期
//! revision 来，回去的那条才说得清现在是第几版（§9.6 的幂等就是靠这个比出来的）。

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[path = "session.rs"]
pub mod session;
pub use session::*;

#[path = "matter.rs"]
pub mod matter;
pub use matter::*;

use super::config::{ConfigIssue, KeyPath, McpExposure, SourceFile};
use crate::cron::{CronFiring, CronJob, JobStatus, NotifyPolicy, OverlapPolicy};
use crate::types::chat::PeerId;
use crate::types::ids::{CronJobId, MatterId, MemoryId, RequestKey, RunId, Seq, SessionId};
use crate::types::memory::{
    MemoryApplication, MemoryItem, MemoryKind, MemoryScope, MemoryState, RetrievalMode,
};
use crate::types::model::{Effort, EmbeddingSpace, ModelConfig};
use crate::types::plan::{PlanHash, PlanSource};
use crate::types::status::{RunState, SessionState, WaitReason};

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
    /// 被拒绝（例如 `write` / `edit` 要写 komo 自己的状态，§7、§8.10）。
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
    /// 遥测导出状态（docs/observability.md §4）；`komo doctor` 显示。
    #[serde(default)]
    pub telemetry: TelemetryHealth,
}

/// OTLP 导出的现状：端点与最近一次导出的结果。`endpoint = None` = 没配，不上报。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryHealth {
    pub endpoint: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_ok_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_error_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
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
    /// 生命周期状态（§8.10）。老客户端读到的是默认的 `active`（这个字段之前不存在）。
    #[serde(default = "session_still_active")]
    pub state: SessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_state: Option<RunState>,
    /// **当前这条 Run 在等什么**（§8.4）。`komo session list` 的"为什么它不动"全靠这一格；
    /// 它只在 `current_state == Some(Waiting)` 时有值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_wait: Option<WaitReason>,
    pub applied_seq: Seq,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// 会话用途：`main` / `normal` / `task`（`docs/background-tasks.md`）。
    /// 老客户端读到的是默认的 `"normal"`（这个字段之前不存在）。值镜像
    /// `komo_store::models::SessionKind::as_str()` 的那几个词——kernel 不依赖 store，这里
    /// 就不定义那个枚举本身，只带它的字符串表示。
    #[serde(default = "session_kind_default")]
    pub kind: String,
    /// 派出这个任务的会话（`kind == "task"` 才有值，`docs/background-tasks.md`）：
    /// 从 `sessions.origin` 的 `task:{parent}` 里解出来。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<SessionId>,
}

fn session_still_active() -> SessionState {
    SessionState::Active
}

fn session_kind_default() -> String {
    "normal".to_string()
}

/// `GET /v1/sessions` 的 query。逻辑删除过的会话默认不列（§8.10）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListQuery {
    /// 把已逻辑删除的也列出来（`closing` / `deleted`；`purged` 只在显式查看单个会话时可见）。
    #[serde(default)]
    pub all: bool,
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
    /// 这个会话上待处理的 Intervention（§7.5）。结果不明与阻塞都在这里，否则"卡住但清单为空"会从这一个入口重新长出来。
    #[serde(default)]
    pub pending: Vec<InterventionSummary>,
}

// ---- Session 生命周期（§8.10）----

/// `POST /v1/sessions/{id}/delete`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSessionRequest {
    /// 不等待未完成的 Run：立刻把它们各写一条明确取消，再进 `deleted`（§8.10）。
    #[serde(default)]
    pub now: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLifecycleResponse {
    pub session: SessionId,
    pub state: SessionState,
    #[serde(with = "time::serde::rfc3339")]
    pub changed_at: OffsetDateTime,
    /// `now` 时被明确取消的未完成 Run。空数组 = 没有要处置的。
    #[serde(default)]
    pub cancelled: Vec<RunId>,
}

/// `POST /v1/sessions/{id}/purge`：内容回收入 `purged`。引用没处置完就 409。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeSessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeSessionResponse {
    pub session: SessionId,
    pub state: SessionState,
    /// 这次回收掉的字节数（目录已不在时是 0——墓碑先落、内容后删，重跑是幂等的）。
    #[serde(default)]
    pub removed_bytes: u64,
}

/// 引用检查不过时的 409 正文：**列出要先处置什么**，不假装成功（§8.10）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeBlocked {
    pub session: SessionId,
    pub blockers: Vec<PurgeBlocker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeBlocker {
    /// 哪一类（`memory_evidence` / `checkpoint` / `delivery` / `cron_firing` / `unfinished_run`）。
    pub what: String,
    pub detail: String,
}

/// `POST /v1/reconcile`：立刻跑一次对账（§8.9）。幂等。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileResponse {
    /// 看了多少条非终态 Run。
    pub checked: u32,
    /// 交给 §8.4 决策表继续的。
    pub resumed: u32,
    /// 停成 `blocked` Intervention 的。
    pub blocked: u32,
    /// 没主人的 `running` 回收成 `interrupted` 的。
    pub reclaimed: u32,
    /// `closing → deleted` 推进的会话数。
    pub closed: u32,
    /// 墓碑已落、内容还没删完，这次补齐的会话数。
    pub purged: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
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
    /// `text` 是一条 shell 命令，**不经模型直接跑**（TUI 的 `!`）。命令照样走 `bash`
    /// 工具与执行器，调用与结果记进会话，下一轮模型看得见。只有 HTTP 这一个入口——聊天
    /// 渠道没有，拿到 bot 对话权限不等于拿到这台机器的 shell。
    #[serde(default, skip_serializing_if = "is_false")]
    pub shell: bool,
    /// **插话**（§6）：会话里有 Run 正在跑时，这句话不排到它后面，而是由它在下一次请求
    /// 模型之前并进去。TUI 发；没轮上（那条 Run 已经在给最终回答、停在等待上、模型不同）
    /// 就照常排队。
    #[serde(default, skip_serializing_if = "is_false")]
    pub interject: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitRunResponse {
    pub run: RunId,
    pub session: SessionId,
    pub seq: Seq,
    pub state: RunState,
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
    /// 需要人处理的（§7.5）：结果不明、阻塞。**两类一起列**。
    #[serde(default)]
    pub pending: Vec<InterventionSummary>,
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
    pub state: RunState,
    /// **在等什么**（`state == Waiting` 时有值，§8.4）。
    ///
    /// 少了这一格，"排队二十分钟"就只是一个状态：答不出在等人、等一个到点时刻、还是
    /// 在等同会话里更早的那条 Run。它是界面唯一的依据，也是 `/v1/interventions` 之外
    /// 唯一能看到 `Dependency` 的地方（依赖不进清单——那不是"等人"）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitReason>,
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
    /// 注入这一段记忆时的召回诊断；`reused_pin` = 这条 Run 自己没有 `memory.injected`，
    /// 沿用的是这一段之前钉下的那一块。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall: Option<crate::types::memory::RecallDiagnostics>,
    /// 受理时冻结的模型名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 开始到进终态的毫秒数；没到终态为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 失败原因（`runs.last_error`，缺则取 `run.failed` 的 `reason`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// 各轮 `message.assistant` 用量之和；一轮都没报则为 `None`，某一项没人报则那一项为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenTotals>,
    /// 这条 Run 里的上下文压缩 / 衰减 / 跳过事件，按发生顺序。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compactions: Vec<CompactionSummary>,
}

/// 一条 Run 的 token 合计。未知不是零：没有任何一轮报过的项保持 `None`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
}

/// 一次 `context.compacted` 的要点。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionSummary {
    /// `compacted` / `decayed` / `skipped`。
    pub outcome: String,
    /// 被摘要覆盖的那段原本多少 token（`compacted` 与 `decayed` 的决策里有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_tokens: Option<u64>,
    /// 压缩之后要重写进缓存的上下文大小。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_compaction_tokens: Option<u64>,
    /// `decayed`：换成短视图的调用数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decayed_calls: Option<u32>,
    /// 决策理由（snake_case）或 `skipped` 的原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
    pub state: RunState,
}

// ---- /v1/interventions（§7.5）----

/// 一条待处理的 Intervention。**清单是派生视图**：它是 `runs` 上的查询，没有自己的
/// 表——多一张表就多一处会与权威漂移的状态，而这次改造的全部理由
/// 就是不要那个（§8.9）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterventionSummary {
    /// 答复时用的句柄：`verify` / `blocked` 都是 Run ID。
    pub handle: String,
    pub kind: InterventionKind,
    pub session: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    /// 停在哪一次逻辑调用上（`verify` 一定有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<crate::types::ids::ToolCallId>,
    /// 一句话："哪里不清楚"。
    pub question: String,
    /// 这一条**此刻**允许答复什么。列表项自带答案菜单，界面不必自己推——推错一个
    /// 界面就会给出一个按下去没反应的答案（§11.3 的 TUI 菜单是同一条理由）。
    #[serde(default)]
    pub verdicts: Vec<InterventionVerdict>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionKind {
    /// 一次调用的结果不明，等人核对（§8.6）。
    Verify,
    /// 前提没了：引用损坏、旧执行者未确认、会话不可服务（§8.9）。
    Blocked,
    /// 无人值守的 Run 要调对外工具，等操作者确认（`docs/confirm.md`）。这个枚举标着冻结，这次是
    /// 有意的扩展。
    Confirm,
}

impl InterventionKind {
    /// 这一类能答什么，按固定顺序（界面直接照抄）。
    pub fn verdicts(self) -> Vec<InterventionVerdict> {
        match self {
            InterventionKind::Verify => vec![
                InterventionVerdict::Satisfied,
                InterventionVerdict::NotPerformed,
                InterventionVerdict::Abandon,
            ],
            InterventionKind::Blocked => {
                vec![InterventionVerdict::Resolve, InterventionVerdict::Abandon]
            }
            InterventionKind::Confirm => {
                vec![InterventionVerdict::Approve, InterventionVerdict::Reject]
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            InterventionKind::Verify => "verify",
            InterventionKind::Blocked => "blocked",
            InterventionKind::Confirm => "confirm",
        }
    }
}

/// 一个结论。**没有"我确认副作用已发生"这一条**——操作者可能看错，而账本一旦这么记
/// 就再也纠不回来（§7.5）。`Satisfied` 说的是"核对之后目标已经是那个样子"，不是"我相信
/// 它跑过了"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionVerdict {
    /// `verify`：核对后目标已满足——给那次调用补一条结果，原 Run 继续。
    Satisfied,
    /// `verify`：确定没执行——标记后重新入队，那次调用按原计划重放（§8.4）。
    NotPerformed,
    /// `blocked`：前提已处理，**重新观察并重新决策**（不强行放行）。
    Resolve,
    /// 两类共有：这条 Run 到此为止。
    Abandon,
    /// `confirm`：执行这一次对外调用。
    Approve,
    /// `confirm`：不执行，告诉模型操作者拒绝了。
    Reject,
}

impl InterventionVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            InterventionVerdict::Satisfied => "satisfied",
            InterventionVerdict::NotPerformed => "not_performed",
            InterventionVerdict::Resolve => "resolve",
            InterventionVerdict::Abandon => "abandon",
            InterventionVerdict::Approve => "approve",
            InterventionVerdict::Reject => "reject",
        }
    }

    /// CLI / 聊天的写法。也认几个手滑得不算离谱的拼法（`not-performed`、`notperformed`）。
    pub fn parse(raw: &str) -> Option<InterventionVerdict> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "satisfied" | "done" => Some(InterventionVerdict::Satisfied),
            "not_performed" | "not-performed" | "notperformed" => {
                Some(InterventionVerdict::NotPerformed)
            }
            "resolve" | "retry" => Some(InterventionVerdict::Resolve),
            "abandon" | "cancel" => Some(InterventionVerdict::Abandon),
            "approve" => Some(InterventionVerdict::Approve),
            "reject" => Some(InterventionVerdict::Reject),
            _ => None,
        }
    }

    /// 这个结论属于这一类吗（§7.5：结论按种类分派，不混用）。
    pub fn allowed_for(self, kind: InterventionKind) -> bool {
        kind.verdicts().contains(&self)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterventionListQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<InterventionKind>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterventionListResponse {
    pub interventions: Vec<InterventionSummary>,
}

/// 单项详情。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InterventionDetail {
    Verify {
        summary: InterventionSummary,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_hash: Option<PlanHash>,
        reason: String,
    },
    Blocked {
        summary: InterventionSummary,
        reason: String,
    },
    Confirm {
        summary: InterventionSummary,
        tool: String,
        arguments: serde_json::Value,
        /// 到这一刻还没答就不执行。
        #[serde(with = "time::serde::rfc3339")]
        deadline: OffsetDateTime,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterventionAnswerRequest {
    pub verdict: InterventionVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

/// 答复的结果。**已答复的返回原结论，不报错**（§11.3）——同一人连点两次，第二次得到
/// 的是"已经答过了"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterventionAnswerResponse {
    pub handle: String,
    pub kind: InterventionKind,
    pub verdict: InterventionVerdict,
    /// 这条 Run 现在的状态（`abandon` 之后是 `cancelled`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_state: Option<RunState>,
    /// 一句人话，回执直接用它。
    pub note: String,
    /// 这次请求没有改变任何东西，返回的是之前那个结论。
    #[serde(default)]
    pub already_answered: bool,
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
    // 审批删掉之前这里还有 `authorized`（命令 Job 有没有 Cron 授权）。旧客户端读不到
    // 它时按默认值走，新客户端读到旧响应里的它会忽略。
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateCronRequest {
    pub name: String,
    /// 五字段 cron 表达式或 `@at YYYY-MM-DD HH:MM`。
    pub schedule: String,
    /// IANA 名字。Gateway 侧解析成偏移。
    pub timezone: String,
    /// 与 `command` 二选一：给了 `command` 这一个必须是空串（§10）。
    pub prompt: String,
    /// 命令直跑模式：不经模型，触发时固定跑这一条 shell 命令（§10）。与 `prompt`
    /// 二选一——旧客户端不传这个字段就是 `None`，行为和以前一样。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
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
    /// 这次 PATCH 改的是**定义**吗（§10：改定义 → 版本 +1）。
    ///
    /// `status` 单独一个不算：`pause` / `resume` 不是定义变更，版本回答的是"定义改没
    /// 改过"。
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

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryListQuery {
    /// **query string 里是一行文本**（`personal` / `project:<id>` / `environment:<id>` / `agent:<id>`），
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
    /// 带上已归档的（`docs/goals.md` §6）。默认不带；`state = archived` 明确问时不受它限制。
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_archived: bool,
    /// 在 Run 里跑的 CLI 带上自己的会话（`KOMO_SESSION`）：Gateway 按这个会话的 Profile 放行
    /// 作用域，与自动召回同一处判断（§9.4「Agent 主动查」）；`memory = off` 的会话被拒。
    /// 不带 = 操作者终端，不按会话限制。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
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

/// `POST /v1/memories`：手动记一条（§9.3「手动记一条」）。
///
/// `session` 与 `run` 要么都给、要么都不给：都给 = Agent 在这个 Run 里替用户记，证据是
/// 这个 Run 里用户说的话，记成未确认；都不给 = 操作者在终端亲口说的，直接记成已确认。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryAddRequest {
    pub content: String,
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
    /// 使用方式（§9.2）。省略 = reference：默认 kind 是 preference，但"是偏好"不等于
    /// "是长期约定"。`convention` 要求 kind 为 preference 且带 `applies_when`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<MemoryApplication>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applies_when: Option<String>,
    /// 为哪件事记的（`komo memory add --for`）：事项必须存在且未结案。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub belongs_to: Option<MatterId>,
}

/// `POST /v1/memories/{id}/classify`：操作者给指定 revision 定使用方式与条件（§9.3
/// 「存量分类」）。写成新 revision，证据与确认原样保留；规则同 add。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryClassifyRequest {
    pub expected_revision: u32,
    /// 省略 = 沿用现有使用方式（只改归属时不给）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<MemoryApplication>,
    /// 省略 = 沿用现有条件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applies_when: Option<String>,
    /// 改挂到这件事项（`classify --for <id>`）：必须存在且未结案。与 `clear_belongs_to` 互斥；
    /// 两者都不给 = 不动归属。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub belongs_to: Option<MatterId>,
    /// 清空归属（`classify --for none`）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub clear_belongs_to: bool,
    /// 在 Run 里跑的 CLI 带上自己的会话：改归属要过 `memory = private / off` 的闸
    /// （`docs/goals.md` §2）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

/// 记下的那一条；`created = false` 是库里已有逐字相同的一条，这次只补了证据或确认。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryAddResponse {
    pub memory: MemoryItem,
    pub created: bool,
}

/// `POST /v1/memories/sweep`：按规则遗忘。`apply = false` 只预演，什么都不改。
///
/// 已确认的记忆不在任何规则的射程里：操作者亲口认过的东西，不该被"很久没用"拿掉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySweepRequest {
    #[serde(default)]
    pub apply: bool,
    /// 入库超过这么多天、一次都没被用过的，遗忘。
    pub never_used_days: u32,
    /// 最近这么多天没被用过的，遗忘。
    pub idle_days: u32,
}

/// sweep 按哪条规则处理了一条记忆。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepRule {
    /// `valid_until` 已过。
    Expired,
    /// 入库很久、从没被用过。
    NeverUsed,
    /// 用过，但很久没再用。
    Idle,
    /// 正文提到的路径已经不在了：标 contested，不直接忘。
    MissingPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepAction {
    pub memory: MemoryId,
    pub revision: u32,
    pub rule: SweepRule,
    /// 处理后的状态：forgotten 或 contested。
    pub to: MemoryState,
    pub content: String,
    /// 给人看的一句理由（多少天没用、哪个路径不在了）。
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySweepResponse {
    pub applied: bool,
    /// 看过多少条（active + candidate）。
    pub scanned: u64,
    pub actions: Vec<SweepAction>,
    /// 应用时失败的那些（版本冲突之类），不让一条失败拦住其余的。
    #[serde(default)]
    pub errors: Vec<String>,
}

/// `POST /v1/memory-consolidate`：离线整理（§9.8）。`apply = false` 只预演，什么都不写。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConsolidateRequest {
    /// 只整理这个作用域；不给就逐个作用域跑。
    #[serde(
        default,
        deserialize_with = "scope_from_text_or_object",
        skip_serializing_if = "Option::is_none"
    )]
    pub scope: Option<MemoryScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MemoryKind>,
    /// 交给模型的软提示；不改变任何授权规则。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(default)]
    pub apply: bool,
}

/// 一组里的一条：真实的 `id@revision`（编号表只活在一次模型调用里，§9.9）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidateMember {
    pub memory: MemoryId,
    pub revision: u32,
}

/// 模型给出的动作（§9.8）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ConsolidateAction {
    Merge {
        keep: MemoryId,
        absorb: Vec<MemoryId>,
    },
    Supersede {
        old: MemoryId,
        by: MemoryId,
    },
    Contest {
        a: MemoryId,
        b: MemoryId,
    },
    Rereadable {
        item: MemoryId,
    },
}

/// 授权结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidateOutcome {
    Executed,
    /// 越权，降级成 `executed_as` 里的动作。
    Downgraded,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidateActionReport {
    pub action: ConsolidateAction,
    /// 模型给的一句理由；只进报告，不进库。
    pub reason: String,
    pub outcome: ConsolidateOutcome,
    /// 降级后实际执行的动作。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed_as: Option<ConsolidateAction>,
    /// 降级或跳过的原因；"需要人看"的写在这里。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidateGroupStatus {
    /// 判完了（预演或执行；没有动作 = 保持原样）。
    Judged,
    /// 判定之后、写库之前组里有条目变了：整组跳过，没盖游标。
    Conflict,
    /// 模型调用或落库失败：没盖游标。
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidateGroupReport {
    pub scope: MemoryScope,
    pub members: Vec<ConsolidateMember>,
    pub actions: Vec<ConsolidateActionReport>,
    pub status: ConsolidateGroupStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 预演与执行同一种清单；不落盘。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConsolidateResponse {
    pub applied: bool,
    pub groups: Vec<ConsolidateGroupReport>,
    /// 成员自上次整理以来都没变、没调模型的组数。
    pub unchanged_groups: u32,
    /// 超过一次运行上限、留给下一次的组数。
    pub deferred_groups: u32,
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
    /// 每个 completion alias 的上下文窗口与来源。旧服务端不带这个字段。
    #[serde(default)]
    pub model_windows: Vec<ModelWindow>,
}

/// 窗口取值的来源：配置 > provider `/models` > 未知。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowSource {
    Config,
    Provider,
    Unknown,
}

/// `komo config check` 里一个 completion alias 的上下文窗口。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelWindow {
    pub alias: String,
    pub window: Option<u64>,
    pub source: WindowSource,
    /// 只在 `Unknown` 时有：探测中 / provider 没报告 / 失败原因。
    #[serde(default)]
    pub detail: Option<String>,
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
    /// `mcp.*` 有键变化时，每台 MCP 服务器此刻的状态（`docs/mcp.md` §2.2）。重载不等握手：
    /// 刚加的、刚改的多半还是"连接中"。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<McpServerStatus>,
}

/// `GET /v1/config/models`：文件里**写着的**模型配置（§3「从 Web 改模型配置」）。
///
/// 不是展开后的快照：看到的就是要改的。凭证只给"在不在"，**不给值**。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigModelsResponse {
    /// `config.toml` 与 `.env` 内容的哈希；写的时候原样带回来，对不上就 409。
    pub version: String,
    pub providers: Vec<ProviderEntry>,
    /// 只有 completion 类；embedding 等其余模型项不在这里改。
    pub models: Vec<ConfigModelEntry>,
    /// `[models] default`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// 两个文件的路径，给"去文件里改"的提示用。
    pub config_path: String,
    pub env_path: String,
    /// `type = "embedding"` 的模型项，只读（换它要重建索引，§3「Web 看 MCP 服务器与向量模型」）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embeddings: Vec<EmbeddingModelEntry>,
    /// `[memory] embedding` 指的 alias。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_embedding: Option<String>,
}

/// 一个 embedding 模型项，文件里写着的样子。凭证只报在不在。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingModelEntry {
    pub alias: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    /// `env_key` 有非空值（没写 `env_key` 的本机端点也算就绪）。
    pub key_set: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u64>,
}

/// 一个 `[model_providers.<id>]`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub id: String,
    /// 设置页的分组（`group`）；没写就自成一组，组名是 `id`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_backend: Option<String>,
    /// 凭证所在的变量名（`env_key`，旧拼法 `api_key_env` 也读成它）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    /// 不走变量的凭证来源（目前只有 `"chatgpt"`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// `env_key` 指向的变量此刻有没有值；`auth` 不走变量时为 `true`，什么都没写时为 `false`。
    pub key_set: bool,
}

/// 一个 completion 类的 `[model.<alias>]`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigModelEntry {
    pub alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// 不挂 provider、自己写了 `base_url` 的模型项：网页上只看不改它的端点。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub standalone: bool,
}

/// `PUT /v1/config/model-providers/{id}`：整份写这个 provider 的几个键（没给的键删掉），
/// 表里别的键与注释不动。`Debug` 不印 `api_key` 的值。
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUpsertRequest {
    pub version: String,
    /// 改名：先把这一项搬到这个新名字下，再按其余字段写。省略或同名 = 不改名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename: Option<String>,
    /// 设置页的分组；省略 = 没有组（自成一组）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// 要写进 `.env` 的 key。省略 = 不动现有的值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl std::fmt::Debug for ProviderUpsertRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderUpsertRequest")
            .field("version", &self.version)
            .field("rename", &self.rename)
            .field("group", &self.group)
            .field("base_url", &self.base_url)
            .field("api_backend", &self.api_backend)
            .field("env_key", &self.env_key)
            .field("auth", &self.auth)
            .field("timeout_secs", &self.timeout_secs)
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

/// `PUT /v1/config/embedding-models/{alias}`：整份写这个 embedding 模型项的几个键，表里别的键
/// （`revision`、前缀等）与注释不动（§3「从 Web 加改向量模型」）。`Debug` 不印 `api_key` 的值。
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingUpsertRequest {
    pub version: String,
    /// 改名：先把这一项搬到这个新名字下，再按其余字段写。省略或同名 = 不改名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename: Option<String>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u64>,
    /// 要写进 `.env` 的 key。省略 = 不动现有的值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl std::fmt::Debug for EmbeddingUpsertRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingUpsertRequest")
            .field("version", &self.version)
            .field("rename", &self.rename)
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("api_backend", &self.api_backend)
            .field("base_url", &self.base_url)
            .field("env_key", &self.env_key)
            .field("dimensions", &self.dimensions)
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

/// `PUT /v1/config/models/{alias}`：整份写这个模型的几个键，表里别的键与注释不动。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUpsertRequest {
    pub version: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

/// `PUT /v1/config/default-model`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultModelRequest {
    pub version: String,
    pub alias: String,
}

/// `PUT /v1/config/provider-groups/{name}`：给一组改名（组里每个 provider 的 `group` 一起改）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderGroupRequest {
    pub version: String,
    pub rename: String,
}

/// 删除只带版本。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigVersionRequest {
    pub version: String,
}

/// 写完之后：模型、常用设置、环境变量三份新视图（共用一个新 `version`），以及这次重载的
/// 报告（变了哪些键，不带值）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigEditResponse {
    pub models: ConfigModelsResponse,
    #[serde(default)]
    pub settings: ConfigSettingsResponse,
    #[serde(default)]
    pub env: ConfigEnvResponse,
    pub reload: ConfigReloadResponse,
}

/// `GET /v1/config/settings`：白名单里的常用设置（§3「从 Web 改常用设置与环境变量」）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSettingsResponse {
    pub version: String,
    pub settings: Vec<ConfigSetting>,
}

/// 一项常用设置。`key` 是文件里的点路径（`memory.retrieval.mode`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSetting {
    pub key: String,
    pub kind: SettingKind,
    /// 文件里写的值；没写为空，生效的是默认。
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    /// 当前快照里生效的值。
    #[serde(default)]
    pub effective: Option<serde_json::Value>,
    /// `choice` 的可选值。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingKind {
    Bool,
    Integer,
    Choice,
}

/// `PUT /v1/config/settings/{key}`。`value: null` = 删掉这一行，回到默认。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingRequest {
    pub version: String,
    #[serde(default)]
    pub value: Option<serde_json::Value>,
}

/// `GET /v1/config/env`：变量名与状态，**不带任何值**。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigEnvResponse {
    pub version: String,
    pub vars: Vec<EnvVarEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVarEntry {
    pub name: String,
    /// `.env` 里有这一行。没有的是配置引用了却缺的名字。
    pub in_file: bool,
    /// `.env` 或进程环境里有非空值。
    pub set: bool,
    /// 引用它的配置项，如 `provider magpie`、`渠道 feishu`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_by: Vec<String>,
}

/// `PUT /v1/config/env`：一次设一批变量。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvSetRequest {
    pub version: String,
    pub vars: Vec<EnvAssignment>,
}

/// 一个变量的新值。值是凭证：`Debug` 只印名字，免得哪条日志把它带出去。
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvAssignment {
    pub name: String,
    pub value: String,
}

impl std::fmt::Debug for EnvAssignment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvAssignment")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// 一台 MCP 服务器此刻的状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    pub state: McpServerState,
    /// 连不上的原因，或要登录时的提示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `POST /v1/mcp/reconnect`：点名的 MCP 服务器断开重连（`docs/mcp.md` §2.5）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpReconnectRequest {
    /// `[mcp.servers.<name>]` 里的名字；空 = 全部。
    #[serde(default)]
    pub servers: Vec<String>,
}

/// 重连等握手：每台服务器连完之后的状态。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpReconnectResponse {
    #[serde(default)]
    pub servers: Vec<McpServerStatus>,
}

/// `GET /v1/mcp`：每台 MCP 服务器此刻的样子（`docs/mcp.md` §2.6）。不等握手、不重连。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServersResponse {
    #[serde(default)]
    pub servers: Vec<McpServerView>,
}

/// 一台服务器：状态之外，再带传输方式与最近一次握手列出的工具。命令、URL、变量名不给。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerView {
    pub name: String,
    pub state: McpServerState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub transport: McpTransportKind,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub oauth: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub exposure: McpExposure,
    /// 最近一次握手列出、按 `tools` 白名单挑过的；没连上过就是空的。
    #[serde(default)]
    pub tools: Vec<McpToolView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    Stdio,
    Http,
}

/// 服务器上的一个工具（原名）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolView {
    pub name: String,
    /// 服务器给的说明，只取第一行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `tool_exposure` 覆盖之后的实际值。
    pub exposure: McpExposure,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outward: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

/// 照 pi 的 `connecting / connected / needs-auth / failed`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    Connecting,
    Connected,
    NeedsAuth,
    Failed,
}

/// `GET /v1/channels`：每个配置里的渠道，配置摘要加上 Gateway 看得见的运行状态。凭证与
/// 插件命令不给。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelsResponse {
    #[serde(default)]
    pub channels: Vec<ChannelView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelView {
    pub channel: String,
    pub enabled: bool,
    /// 配了 `plugin.command`（自定义渠道，或换了命令的官方渠道）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom_plugin: bool,
    pub allow_from: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_tenant: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_chat: Option<String>,
    /// 启用了却没人能通过它下指令（§11.2 的那条警告）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outbound_only: bool,
    pub state: ChannelRunState,
    /// 进入当前状态的时间；未启用时没有。
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub since: Option<OffsetDateTime>,
    /// 退出或起不来的原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// 这次启动以来插件退出后重建了几次（配置重载重启的不算）。
    #[serde(default)]
    pub restarts: u32,
}

/// Gateway 只看得见插件进程这一层：在跑（握手、平台连接在插件里）、退出后退避重试、
/// 起不来（缺凭证、缺插件这类重建前就失败的）、没启用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelRunState {
    Off,
    Running,
    Retrying,
    Failed,
}

/// `POST /v1/channels/{id}/probe`：和 `komo channel probe` 同一条连通性核对，不起消费循环。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelProbeResponse {
    pub ok: bool,
    pub message: String,
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
    fn a_verdict_belongs_to_its_kind() {
        // 旧客户端可能还带着 `scope`：忽略，不报错。
        let request: InterventionAnswerRequest =
            serde_json::from_str(r#"{"verdict":"satisfied","scope":"once"}"#).unwrap();
        assert_eq!(request.verdict, InterventionVerdict::Satisfied);

        // 结论按种类分派，不混用（§7.5）：
        assert!(InterventionVerdict::Satisfied.allowed_for(InterventionKind::Verify));
        assert!(!InterventionVerdict::Satisfied.allowed_for(InterventionKind::Blocked));
        assert!(InterventionVerdict::Resolve.allowed_for(InterventionKind::Blocked));
        assert!(!InterventionVerdict::Resolve.allowed_for(InterventionKind::Verify));

        // 审批那一类已经删掉（`approval` 不是种类）；`approve` / `reject` 只答对外动作确认
        // （`docs/confirm.md`）。
        assert!(serde_json::from_str::<InterventionKind>(r#""approval""#).is_err());
        assert!(InterventionVerdict::Approve.allowed_for(InterventionKind::Confirm));
        assert!(!InterventionVerdict::Approve.allowed_for(InterventionKind::Blocked));
        assert!(!InterventionVerdict::Abandon.allowed_for(InterventionKind::Confirm));
    }

    #[test]
    fn a_verdict_parses_the_way_operators_type_it() {
        assert_eq!(
            InterventionVerdict::parse("not-performed"),
            Some(InterventionVerdict::NotPerformed)
        );
        assert_eq!(
            InterventionVerdict::parse(" DONE "),
            Some(InterventionVerdict::Satisfied)
        );
        assert_eq!(
            InterventionVerdict::parse("yes"),
            None,
            "y / yes 是审批的写法，已经删了；确认卡要写全 approve"
        );
        assert_eq!(
            InterventionVerdict::parse("Approve"),
            Some(InterventionVerdict::Approve)
        );
        assert_eq!(InterventionVerdict::parse("maybe"), None);
        assert_eq!(InterventionKind::Verify.as_str(), "verify");
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
            serde_json::from_str(r#"{"run":"run-1","session":"sess-1","seq":1,"state":"queued"}"#)
                .unwrap();
        assert!(!response.deduplicated);
        assert_eq!(response.state, RunState::Queued);
    }
}
