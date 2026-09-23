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
}

fn session_still_active() -> SessionState {
    SessionState::Active
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
    /// 这个会话上待处理的 Intervention（§7.5）。**不是**只列审批：结果不明与阻塞也在
    /// 这里，否则"卡住但清单为空"会从这一个入口重新长出来。
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
    /// 需要人处理的（§7.5）：审批、结果不明、阻塞。**三类一起列**。
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
    /// 少了这一格，"排队二十分钟"就只是一个状态：答不出在等审批、等一个到点时刻、还是
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

/// 落一条审批决定的返回。**已决定的返回原决定，不报错**（§11.3）——同一人连点两次，
/// 第二次得到的是"已决定"。
///
/// 它是**审批层**的结果（`ApprovalRepo::decide` 的返回，也是回执要渲染的那一份）；
/// 统一清单上的答复是 [`InterventionAnswerResponse`]，里面带着同样的
/// [`ApprovalDecisionRecord`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecisionResponse {
    pub approval: ApprovalId,
    pub short_id: ShortId,
    pub decision: ApprovalDecisionRecord,
    /// 这次请求没有改变任何东西，返回的是之前那个决定。
    #[serde(default)]
    pub already_decided: bool,
}

// ---- /v1/interventions（§7.5）----

/// 一条待处理的 Intervention。**清单是派生视图**：它是 `runs` 与 `approval_requests` 的
/// 并集查询，没有自己的表——多一张表就多一处会与权威漂移的状态，而这次改造的全部理由
/// 就是不要那个（§8.9）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterventionSummary {
    /// 答复时用的句柄：审批是短 ID（§11.3），`verify` / `blocked` 是 Run ID。
    pub handle: String,
    pub kind: InterventionKind,
    pub session: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    /// 停在哪一次逻辑调用上（`verify` 一定有；审批可能有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<crate::types::ids::ToolCallId>,
    /// 一句话：审批是"要你放行什么"，另外两类是"哪里不清楚"。
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
    /// 一份执行计划等人放行（§7.4）。
    Approval,
    /// 一次调用的结果不明，等人核对（§8.6）。
    Verify,
    /// 前提没了：引用损坏、旧执行者未确认、会话不可服务（§8.9）。
    Blocked,
}

impl InterventionKind {
    /// 这一类能答什么，按固定顺序（界面直接照抄）。
    pub fn verdicts(self) -> Vec<InterventionVerdict> {
        match self {
            InterventionKind::Approval => {
                vec![InterventionVerdict::Approve, InterventionVerdict::Reject]
            }
            InterventionKind::Verify => vec![
                InterventionVerdict::Satisfied,
                InterventionVerdict::NotPerformed,
                InterventionVerdict::Abandon,
            ],
            InterventionKind::Blocked => {
                vec![InterventionVerdict::Resolve, InterventionVerdict::Abandon]
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            InterventionKind::Approval => "approval",
            InterventionKind::Verify => "verify",
            InterventionKind::Blocked => "blocked",
        }
    }
}

/// 一个结论。**没有"我确认副作用已发生"这一条**——操作者可能看错，而账本一旦这么记
/// 就再也纠不回来（§7.5）。`Satisfied` 说的是"核对之后目标已经是那个样子"，不是"我相信
/// 它跑过了"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionVerdict {
    /// 审批：放行这份计划（范围由 `scope` 另给）。
    Approve,
    Reject,
    /// `verify`：核对后目标已满足——给那次调用补一条结果，原 Run 继续。
    Satisfied,
    /// `verify`：确定没执行——标记后重新入队（一次性授权按 §7.4 的原范围重放）。
    NotPerformed,
    /// `blocked`：前提已处理，**重新观察并重新决策**（不强行放行）。
    Resolve,
    /// 三类共有：这条 Run 到此为止。
    Abandon,
}

impl InterventionVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            InterventionVerdict::Approve => "approve",
            InterventionVerdict::Reject => "reject",
            InterventionVerdict::Satisfied => "satisfied",
            InterventionVerdict::NotPerformed => "not_performed",
            InterventionVerdict::Resolve => "resolve",
            InterventionVerdict::Abandon => "abandon",
        }
    }

    /// CLI / 聊天的写法。也认几个手滑得不算离谱的拼法（`not-performed`、`notperformed`）。
    pub fn parse(raw: &str) -> Option<InterventionVerdict> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "approve" | "y" | "yes" => Some(InterventionVerdict::Approve),
            "reject" | "n" | "no" => Some(InterventionVerdict::Reject),
            "satisfied" | "done" => Some(InterventionVerdict::Satisfied),
            "not_performed" | "not-performed" | "notperformed" => {
                Some(InterventionVerdict::NotPerformed)
            }
            "resolve" | "retry" => Some(InterventionVerdict::Resolve),
            "abandon" | "cancel" => Some(InterventionVerdict::Abandon),
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

/// 单项详情。审批类的那一份就是 §7.2 要求界面展示的全部内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InterventionDetail {
    /// `ApprovalRecord` 里带着整份 `ExecutionPlan`，比别的变体大一个数量级，所以装箱。
    Approval(Box<ApprovalRecord>),
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterventionAnswerRequest {
    pub verdict: InterventionVerdict,
    /// 只有 `Approve` 用得上（§7.2 的三种范围）。不给 = `Once`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ApprovalScope>,
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
    /// 审批类才有：这次（或之前那次）决定本身。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<ApprovalDecisionRecord>,
    /// 这条 Run 现在的状态（`abandon` 之后是 `cancelled`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_state: Option<RunState>,
    /// 一句人话，回执直接用它。
    pub note: String,
    /// 这次请求没有改变任何东西，返回的是之前那个结论。
    #[serde(default)]
    pub already_answered: bool,
}

/// `POST /v1/interventions/answers`：**一次答一批**（§11.3 的 `/approve all`）。
///
/// 一条 Run 里连着几个 shell 命令、几个 Run 各自卡在等待上——一次按键答一批是操作者
/// 真正要的那件事。它**不是**一条结论覆盖多个计划：名单里每一条各自落一条结论、各自排
/// 一条审计事件（§7.4），与逐条答完全等价，只是不必按 N 次键。
///
/// 名单由**发起方列出**（TUI 拿手上的待处理集合、CLI 与聊天先列一次），协议里没有
/// "全部"这个词：服务端不替操作者决定"哪些算全部"——那会把答复到达之后新出现的请求
/// 也一起答掉。
///
/// **只答审批，而且范围固定为本次调用**：范围授权绑的是一份具体的计划（shell 绑整条
/// 命令、Python 绑模块与版本），一批互不相干的计划共用一个范围，只能是替操作者猜一个
/// 他没看过的答复。要范围就逐条答。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterventionBatchAnswerRequest {
    pub handles: Vec<String>,
    pub approved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

/// 一批答复的结果。`answered` 与请求同序；点了名却已经不在的那些单独列出，不让整批失败。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterventionBatchAnswerResponse {
    pub answered: Vec<InterventionAnswerResponse>,
    #[serde(default)]
    pub missing: Vec<String>,
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
    /// 这个 Job 当前版本有没有有效的 `GrantScope::CronJob` 授权（§7.2、§10）。命令
    /// Job 在 `cron add` 时就该有——这是操作者在 `cron list` 上能看到这条授权存在
    /// 的地方，不必单独去查 `/v1/approvals`（那条审批不是待处理的，也没有挂在任何一
    /// 条 Run 上，走那条查询路径反而查不到）。
    #[serde(default)]
    pub authorized: bool,
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
    fn an_answer_without_a_scope_is_this_call_only() {
        // 不给 `scope` 就是本次调用——`scope` 只在 `Approve` 上有意义（§7.2）。
        let request: InterventionAnswerRequest =
            serde_json::from_str(r#"{"verdict":"approve"}"#).unwrap();
        assert_eq!(request.verdict, InterventionVerdict::Approve);
        assert_eq!(request.scope, None);

        // 结论按种类分派，不混用（§7.5）：
        assert!(InterventionVerdict::Satisfied.allowed_for(InterventionKind::Verify));
        assert!(!InterventionVerdict::Satisfied.allowed_for(InterventionKind::Approval));
        assert!(InterventionVerdict::Resolve.allowed_for(InterventionKind::Blocked));
        assert!(!InterventionVerdict::Approve.allowed_for(InterventionKind::Blocked));
    }

    #[test]
    fn a_verdict_parses_the_way_operators_type_it() {
        assert_eq!(
            InterventionVerdict::parse("not-performed"),
            Some(InterventionVerdict::NotPerformed)
        );
        assert_eq!(
            InterventionVerdict::parse(" YES "),
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
