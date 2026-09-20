//! 全部 trait（§13.5）。
//!
//! 一个 trait 只在两种情况下存在：**有第二个实现**（生产之外还有测试替身，或有多个
//! 后端），或**要做编译防火墙**（上层依赖 trait 就够，不需要看到 toasty / reqwest /
//! axum 类型）。其余组件是具体类型，直接构造、直接测；不为"以后可能换"预留 trait。
//!
//! 不是 trait 的东西，记在这里免得有人再造一个：`AgentLoop`、`ToolExecutor`、
//! `Scheduler`、`Recovery`、`MemoryManager`、`SkillRegistry`、`Coordinator`、
//! `Dispatcher` —— 各只有一个实现，依赖下面这些 trait 就可测。**审批也不需要
//! `Approver` trait**：`Policy` 答 `Ask` 后 executor 写 `approval_requests` 并
//! `Ledger::suspend`，四个界面都打到 `POST /v1/approvals/{id}/decision`，渠道之间的
//! 差别只在渲染（§11.3），不在决策。
//!
//! 接缝上一律 `Arc<dyn Trait>` / `Box<dyn Trait>`，异步 trait 用 `async_trait`——对象
//! 安全，一次堆分配相对模型往返可忽略。

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::cron::{CronFiring, CronJob, JobStatus, ZoneError, ZoneResolution};
use crate::policy::{Grant, PolicyContext, PolicyDecision};
use crate::protocol::http::{ApprovalDecisionRecord, ApprovalDecisionResponse, ApprovalRecord};
use crate::protocol::{InboundAck, InboundMessage};
use crate::types::chat::{Delivery, DeliveryTarget, Outbound};
use crate::types::ids::{
    ApprovalId, AttemptId, CronJobId, EventId, ExecutorId, MemoryId, RunId, Seq, SessionId,
    ShortId, ToolCallId,
};
use crate::types::memory::{MemoryItem, RecallQuery, RecallResult};
use crate::types::model::{EmbeddingSpace, InputKind, TokenUsage, Vector};
use crate::types::plan::{
    ApprovedPlan, ConsumeIntent, ConsumedApproval, ExecutionPlan, Verification,
};
use crate::types::refs::{AttemptRef, OutputRef, PublishedOutput, ToolResultBody, VerifiedOutput};
use crate::types::status::{Claimed, RunEnd, WaitReason};
use crate::types::tool::{
    CancelToken, PyError, PythonJob, PythonResult, ToolContext, ToolDefinition, ToolError,
    ToolOutput,
};
use crate::types::turn::{
    AcceptInput, Accepted, AssistantRound, EventBatch, GrantUse, LlmError, Round, RoundInput,
    TurnRequest,
};

// ---------------------------------------------------------------- 错误

/// 账本写入失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LedgerError {
    /// 同一请求键对应不同内容（§8.5）。
    #[error("请求键 {key} 已经绑定了另一份输入")]
    RequestKeyConflict { key: String },
    /// 领取代次不对：旧执行者不能继续提交新状态（§8.7）。
    #[error("领取代次过期：持有 {held}，当前 {current}")]
    StaleGeneration { held: u64, current: u64 },
    #[error("找不到 {what}")]
    NotFound { what: String },
    /// 这个状态下做不了这件事。
    #[error("状态冲突：{0}")]
    Conflict(String),
    /// 文件同步失败——**不能执行后续副作用或向客户端确认持久完成**（§8.3）。
    #[error("持久化失败：{0}")]
    Persist(String),
    /// MVCC 冲突重试超限（§8.2）。
    #[error("写入争用，重试超限")]
    Contended,
    /// JSONL 中间损坏、已提交范围缺失或哈希不匹配。
    #[error("记录损坏：{0}")]
    Corrupt(String),
}

/// 存储层失败。
///
/// 它比"IO 出错了"宽一点：store 的 `with_write_retry` 闭包只有这一条错误通道
/// （§8.2），而闭包里跑的正是那些要报**领域性**失败的事务——预期 revision 不符、这条
/// 授权覆盖不到这份计划。没有对应的变体，那些失败只能塞进 `Other(String)` 再靠字符串
/// 前缀在上层认回来，而字符串前缀不是类型。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("找不到 {what}")]
    NotFound { what: String },
    /// 引用的正文缺失或哈希不符。**返回它，不返回内容。**
    #[error("引用损坏：{0}")]
    Corrupt(String),
    /// 预期 revision / 版本不符。与 [`RepoError::VersionConflict`] 一一对应。
    #[error("版本冲突：预期 {expected}，当前 {actual}")]
    VersionConflict { expected: u32, actual: u32 },
    /// 这条授权不能用来执行这份计划。与 [`RepoError::GrantMismatch`] 一一对应。
    #[error("授权不匹配：{0}")]
    GrantMismatch(String),
    #[error("IO 失败：{0}")]
    Io(String),
    #[error("写入争用，重试超限")]
    Contended,
    #[error("{0}")]
    Other(String),
}

/// 仓储失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepoError {
    #[error("找不到 {what}")]
    NotFound { what: String },
    /// 预期 revision / 版本不符。
    #[error("版本冲突：预期 {expected}，当前 {actual}")]
    VersionConflict { expected: u32, actual: u32 },
    /// 这条授权不能用来执行这份计划。
    #[error("授权不匹配：{0}")]
    GrantMismatch(String),
    #[error("写入争用，重试超限")]
    Contended,
    #[error("{0}")]
    Other(String),
}

/// store 的事务错误通道抬到仓储接口上。
///
/// 两个领域性变体**逐个对上**，不经字符串；其余的按语义收进最接近的那个。
impl From<StoreError> for RepoError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotFound { what } => RepoError::NotFound { what },
            StoreError::VersionConflict { expected, actual } => {
                RepoError::VersionConflict { expected, actual }
            }
            StoreError::GrantMismatch(message) => RepoError::GrantMismatch(message),
            StoreError::Contended => RepoError::Contended,
            StoreError::Corrupt(message) => RepoError::Other(format!("引用损坏：{message}")),
            StoreError::Io(message) => RepoError::Other(format!("IO 失败：{message}")),
            StoreError::Other(message) => RepoError::Other(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmbedError {
    /// 维度、数值或范数校验不过——截断或结构错误的向量不接受（§9.5）。
    #[error("向量不合法：{0}")]
    InvalidVector(String),
    #[error("端点不可用：{0}")]
    Unavailable(String),
    #[error("超时")]
    Timeout,
    #[error("{0}")]
    Other(String),
}

/// 主动投递失败。**注意 `Deferred` 不是错误**——它是 [`Delivery`] 的一个状态。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeliverError {
    /// 一个 home chat 都没配。**返回错误给调用方，不静默丢弃**（§11.4）。
    #[error("没有可用的投递目标：{0}")]
    NoTarget(String),
    #[error("平台拒绝：{0}")]
    Rejected(String),
    #[error("投递记录写入失败：{0}")]
    Persist(String),
    #[error("{0}")]
    Other(String),
}

/// Dispatcher 的失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GatewayError {
    #[error("未认证")]
    Unauthorized,
    #[error("找不到 {what}")]
    NotFound { what: String },
    #[error("请求不合法：{0}")]
    InvalidRequest(String),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error("{0}")]
    Internal(String),
}

/// 渠道 `serve` 的失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("渠道 {channel}：{message}")]
pub struct ChannelError {
    pub channel: String,
    pub message: String,
}

/// 停机信号。kernel 不依赖 tokio，所以它就是一个可查询的标志；渠道把它和自己的
/// select 包在一起。
pub type Shutdown = CancelToken;

// ---------------------------------------------------------------- Ledger

/// **Agent Loop 的唯一写入口。**每个 Session 一个实例，串行。
///
/// 每个方法对应 §8.5 的一段箭头，方法内部完成"外置正文（如有）→ JSONL 追加并同步
/// → 数据库事务"的顺序。让 loop 和 executor 的测试不碰文件与 Turso——生产实现是
/// store 的 `Coordinator`，测试实现是 `test_support::MemLedger`。
#[async_trait]
pub trait Ledger: Send + Sync {
    /// 预留 Run（ingesting）→ `run.accepted` → queued。同一 `request_key` 返回原 Run，
    /// 哈希不同则拒绝。
    async fn accept_input(&self, input: AcceptInput) -> Result<Accepted, LedgerError>;

    /// 完整 assistant 回复 + 本轮全部调用计划，一个逻辑事件；返回 Runtime 分配的
    /// `ToolCallId`。
    async fn record_round(
        &self,
        run: &RunId,
        round: AssistantRound,
    ) -> Result<Vec<ToolCallId>, LedgerError>;

    /// `run.started`：某个执行实例领取了这个 Run，带上它的领取代次。
    ///
    /// （事件词汇里本来就有 `run.started`——`RunQueue::claim` 拿到 `Claimed` 之后总得有
    /// 人把它写下来，而 trait 上原先没有一个方法写得出它。`fold` 认它：状态变
    /// `Running`，`generation` 记在 `RunView` 上。）
    async fn start_run(
        &self,
        run: &RunId,
        executor: &ExecutorId,
        generation: u64,
    ) -> Result<(), LedgerError>;

    /// `tool.planned`：准备好的执行计划落盘。返回承载它的事件 ID，`tool.started` 的
    /// `plan_ref` 指向它。
    ///
    /// （§13.5 的签名表没有列它——那张表把计划的持久化并在 `record_round` 里说。但
    /// §8.4 第 6 行要求"调用 planned，确定尚未执行"是一个**可分辨的状态**，而计划要到
    /// 执行前才由 `prepare` 产生，所以它必须是自己的一步。）
    async fn plan_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
    ) -> Result<EventId, LedgerError>;

    /// `tool.started` + 执行尝试 + 首次授权消费，同一事务；**返回后才允许产生真实
    /// 副作用**。
    async fn start_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
        grant: Option<GrantUse>,
    ) -> Result<AttemptId, LedgerError>;

    /// 输出已由 [`ToolOutputStore`] 发布；这里只写 `tool.result` 元信息与引用。
    async fn finish_call(
        &self,
        attempt: &AttemptId,
        published: PublishedOutput,
    ) -> Result<(), LedgerError>;

    /// 停在某个外部条件上（§8.4）：状态变 `Waiting`，理由进 `WaitReason`，释放执行名额。
    async fn suspend(&self, run: &RunId, wait: WaitReason) -> Result<(), LedgerError>;

    /// Run 的终态。
    ///
    /// **调用它之前，这一轮的最终回复必须已经作为 `message.assistant` 落过盘**：会话
    /// 消息面只认 `message.assistant`，`run.completed` 里的 `final_message` 是给"结果
    /// 已保存但客户端没收到"补读用的副本，不是消息面上的一个节点。runtime 的顺序因此
    /// 是 `record_round` → `complete`，倒过来会让这一轮的回答从历史里消失，而账本看上去
    /// 一切正常。
    async fn complete(&self, run: &RunId, end: RunEnd) -> Result<(), LedgerError>;

    /// 按 seq 范围读**一页**事件；引用正文按需加载并校验哈希。
    ///
    /// **读不得创建或修改内容**（§8.9：观察不改写）。实现不许在 `read` 里建目录、建
    /// 文件、补行或改写任何字节——一个还没写过日志的会话读回来就是空的一页。理由不止
    /// "干净"：§8.10 判一条未完成的 Run 能不能领，靠的是"内容到底在不在"，而一个会
    /// 顺手把内容造出来的 `read` 会让那个判断永远答"在"。
    ///
    /// `limit` 不是可选的：§14 的待验证项里「Turso 长读事务与并发写提交的快照语义」还
    /// 没核实，而它的对策已经定了——读路径改成短事务分页。接口先把这件事定死，实现换
    /// 起来就不用动调用方。0 表示"由实现决定一页多大"。
    async fn read(
        &self,
        session: &SessionId,
        from: Seq,
        limit: u32,
    ) -> Result<EventBatch, LedgerError>;

    /// `/new`：追加一个 `conversation.boundary`，**不切 Session**（§13.1）。
    async fn boundary(&self, session: &SessionId) -> Result<Seq, LedgerError>;

    /// 控制审计补写：把 `control_outbox` 里的一条审批事件追加到 JSONL（§8.5 的反向
    /// 顺序）。按 `event_id` 幂等；已写入就复用原事件位置。**它不创建授权。**
    async fn append_audit(
        &self,
        session: &SessionId,
        event_id: &EventId,
        payload: crate::events::EventPayload,
        occurred_at: OffsetDateTime,
    ) -> Result<Seq, LedgerError>;
}

// ---------------------------------------------------------------- 输出存储

/// 子进程 stdout / stderr 的流式落盘。`shell` / `python` 的测试需要替身。
#[async_trait]
pub trait ToolOutputStore: Send + Sync {
    /// 为一次尝试打开流式写入器（stdout / stderr 写 `.partial`）。
    async fn begin(&self, attempt: &AttemptRef) -> Result<Box<dyn OutputWriter>, StoreError>;

    /// 收齐后同步、原子写 `output.json`，返回带路径、大小、哈希的引用。
    async fn publish(
        &self,
        writer: Box<dyn OutputWriter>,
        result: ToolResultBody,
    ) -> Result<PublishedOutput, StoreError>;

    /// 按引用读取并校验哈希；不匹配返回 [`StoreError::Corrupt`]，**不返回内容**。
    async fn open(&self, output: &OutputRef) -> Result<VerifiedOutput, StoreError>;
}

/// 一次尝试的流式写入器。
#[async_trait]
pub trait OutputWriter: Send + Sync {
    async fn write_stdout(&mut self, chunk: &[u8]) -> Result<(), StoreError>;
    async fn write_stderr(&mut self, chunk: &[u8]) -> Result<(), StoreError>;
    /// 这次尝试的身份。
    fn attempt(&self) -> &AttemptRef;
    /// 到目前为止写了多少字节——输出长度预算按它判。
    fn bytes_written(&self) -> u64;
}

// ---------------------------------------------------------------- 队列

/// 调度器、恢复扫描、Cron、手动 resume 共用的**领取入口**。并发领取测试需要替身。
#[async_trait]
pub trait RunQueue: Send + Sync {
    /// 领取**下一个**可执行的 Run。条件更新 + 递增代次；返回 `None` 表示没有可领取的。
    /// 调度器用它。
    async fn claim(&self, executor: &ExecutorId) -> Result<Option<Claimed>, StoreError>;

    /// 领取**指定**的那一个 Run。手动 `resume` 与启动扫描要的是这个——它们手里已经有
    /// 一个 Run 号，"下一个到期的"回答的是另一个问题。
    ///
    /// 同样是条件更新 + 递增代次，所以两个执行者同时来只有一个拿得到；返回 `None` 表示
    /// 这个 Run 此刻不可领取（已终态、已被别人拿走、或者根本不在队列里）。
    async fn claim_run(
        &self,
        run: &RunId,
        executor: &ExecutorId,
    ) -> Result<Option<Claimed>, StoreError>;

    /// 交还名额：Run 让出执行（等审批 / 等重试）或执行者退出时调用。代次不对就什么
    /// 都不做。
    async fn release(&self, claimed: &Claimed) -> Result<(), StoreError>;

    /// **续租**：还在跑就别说它没人管（§8.7）。
    ///
    /// handler 每 `TTL/3` 调一次。租约只用来**发现**"没人管了"——对账见到过期还要过
    /// 一道"持有者确已不在"才回收（§8.9）：少那一道，一次二十分钟的调用会在主人还活着
    /// 的时候被第二个执行者抢走，那不是恢复，是重复副作用（§8.6）。
    ///
    /// 复用一个代次围栏：`false` = 这个 Run 的领取权已经不是自己的了，调用方应当
    /// **停止这个任务的一切写入**（与状态提交拿到 `StaleGeneration` 同一个信号，所以它
    /// 返回布尔而不是错误）。
    async fn renew(
        &self,
        claimed: &Claimed,
        executor: &ExecutorId,
        until: time::OffsetDateTime,
    ) -> Result<bool, StoreError>;
}

// ---------------------------------------------------------------- 审批

/// executor 等待、聊天 / TUI 答复、outbox 补写三方共用。
#[async_trait]
pub trait ApprovalRepo: Send + Sync {
    async fn create(&self, request: ApprovalRecord) -> Result<ApprovalRecord, RepoError>;

    async fn get(&self, id: &ApprovalId) -> Result<Option<ApprovalRecord>, RepoError>;

    /// 短 ID 在**待处理集合内**唯一，所以这个查找只在待处理集合里做（§11.3）。
    async fn find_by_short_id(&self, short: &ShortId) -> Result<Option<ApprovalRecord>, RepoError>;

    /// 聊天里的 `/approve <short_id>` 第二次到达时用：**待处理的优先**，没有就取**最近一条
    /// 已决定的**同短 ID 记录，让 Dispatcher 能回「已决定：原决定」而不是「没有这条」
    /// （§11.3「已决定的返回原决定，不报错」）。短 ID 的重用窗口是「下一条审批产生之前」，
    /// 所以待处理那条才是用户正看着的那张卡。
    async fn find_latest_by_short_id(
        &self,
        short: &ShortId,
    ) -> Result<Option<ApprovalRecord>, RepoError>;

    async fn list_pending(
        &self,
        session: Option<&SessionId>,
    ) -> Result<Vec<ApprovalRecord>, RepoError>;

    /// 记录决定。**重复回答幂等**：已决定的返回原决定并把
    /// [`ApprovalDecisionResponse::already_decided`] 置位，不报错（§11.3）。
    async fn decide(
        &self,
        id: &ApprovalId,
        decision: ApprovalDecisionRecord,
    ) -> Result<ApprovalDecisionResponse, RepoError>;

    /// 消费一条已批准的审批，换一张可以执行这份计划的凭据。
    ///
    /// 参数是**整份计划**而不是它的哈希，因为三种范围核对的不是同一件事（§7.2）：
    ///
    /// - **本次调用授权**（`GrantScope::Once`）：计划哈希逐字相同，且这条授权还没被
    ///   消费过；消费成功后标记 `consumed`。
    /// - **本次 Run / Cron Job 的范围授权**：按 [`Grant::covers`] 判定——匹配器命中、
    ///   绑定的版本全部对得上、没过期。范围授权**不因一次使用而消耗**，它本来就是给
    ///   这个范围里的多次调用用的；但返回的 [`ConsumedApproval`] 仍然记下用的是哪一条
    ///   `grant`，于是 `GrantUse` 进账本，事后答得出"这次是凭哪条授权跑的"。
    ///
    /// 只有哈希是不够的：一条"本次 Run 内可以跑 cargo test"的授权，按定义覆盖的是一族
    /// 计划，每个都有自己的哈希。
    ///
    /// `intent` 是 §7.4 那两句话的分界线：「已取消或**已完成调用不能再次执行**」与
    /// 「恢复时**若确定原动作未发生**……可在原授权范围内继续；**已经消费授权本身不是
    /// 重试依据**」。一条已经用掉的一次性授权，对
    /// [`ConsumeIntent::First`] 必须是 [`RepoError::GrantMismatch`]——一个可分辨的失败，
    /// 而不是悄悄放行第二次副作用；只有恢复流程核对过、拿着
    /// [`ConsumeIntent::KnownNotToHaveRun`] 来，才准重用它。
    ///
    /// **[`PolicyDecision::Deny`] 时不会走到这里**——executor 在 Deny 上直接返回拒绝，
    /// 根本不去消费任何授权，这就是"审批不覆盖显式 Deny"在执行侧成立的方式。
    async fn consume(
        &self,
        id: &ApprovalId,
        plan: &ExecutionPlan,
        intent: ConsumeIntent,
        now: OffsetDateTime,
    ) -> Result<ConsumedApproval, RepoError>;

    /// 写下一条范围授权（§7.2 的第二、第三种）。
    ///
    /// **它不是"批准"**：批准是 [`ApprovalRepo::decide`] 那一行，这里只是把操作者答应
    /// 的那个**范围**落成一条可以被 [`Grant::covers`](crate::policy::Grant::covers) 查到
    /// 的记录。两步分开，是因为一条 `Once` 的批准根本不产生授权（审批自己按计划哈希
    /// 绑定），而把两者合在一起就得让"没有授权"和"写不下授权"长成同一个样子。
    ///
    /// 幂等：同一个 [`GrantId`](crate::types::ids::GrantId) 重写覆盖原行。
    async fn put_grant(&self, grant: Grant) -> Result<Grant, RepoError>;

    /// 这个 Run 当前有效的范围授权。
    async fn grants_for_run(
        &self,
        run: &RunId,
        now: OffsetDateTime,
    ) -> Result<Vec<Grant>, RepoError>;

    /// 这个 Job 当前有效的范围授权。Job 版本变了就不该再返回旧的（§10）。
    async fn grants_for_job(
        &self,
        job: &CronJobId,
        job_version: u64,
        now: OffsetDateTime,
    ) -> Result<Vec<Grant>, RepoError>;
}

// ---------------------------------------------------------------- Cron

/// 调度器 + CLI + Gateway API 三方共用。
#[async_trait]
pub trait CronRepo: Send + Sync {
    async fn list(&self) -> Result<Vec<CronJob>, RepoError>;

    async fn get(&self, id: &CronJobId) -> Result<Option<CronJob>, RepoError>;

    /// 创建或整体替换一个 Job。**版本递增**——Job 改了，绑定它的授权失效。
    async fn put(&self, job: CronJob) -> Result<CronJob, RepoError>;

    /// 只写调度状态：下一个槽位、生命周期、触发层面的错误。
    ///
    /// **它不递增版本**，这是它与 [`CronRepo::put`] 的全部区别，也是它存在的全部理由：
    /// 推进槽位不是定义变更。走 `put` 的话每次触发都会让版本 +1，而 `GrantScope::CronJob`
    /// 绑的正是版本（§7.2、§10「Job 版本变了就不该再返回旧的」）——于是一个每天跑的
    /// Job 的授权活不过第一次触发，操作者每天早上都要重新批一遍。
    async fn advance(
        &self,
        id: &CronJobId,
        next_run_at: Option<OffsetDateTime>,
        status: JobStatus,
        last_error: Option<String>,
    ) -> Result<(), RepoError>;

    async fn remove(&self, id: &CronJobId) -> Result<bool, RepoError>;

    /// 到期的 Job。
    async fn due(&self, now: OffsetDateTime) -> Result<Vec<CronJob>, RepoError>;

    /// 插入一条触发记录并占住它。唯一键是 `job_id + scheduled_at`，**已经存在就返回
    /// `false`**——同一计划时间不重复创建运行（§10）。
    async fn claim_firing(&self, firing: CronFiring) -> Result<bool, RepoError>;

    /// 给一条**已经存在**的触发记录补字段：它产生的 Session / Run，或它最终的状态。
    ///
    /// 与 [`CronRepo::claim_firing`] 分开是因为两件事不同：claim 回答"这一槽归谁"，
    /// 它回答"那一槽后来怎么样了"。用 claim 兼做更新，就得靠它返回 `false` 来表示
    /// "行已在、字段已补"，而那个 `false` 同时还是"别人抢走了"的意思——两个相反的结论
    /// 一个返回值（§10「更新本次触发状态」）。
    ///
    /// 行不在时答 `false`，不是错误：一次手动 run 本来就没有触发记录。
    async fn update_firing(&self, firing: CronFiring) -> Result<bool, RepoError>;

    /// 这个 Job 上一次触发还没结束吗（含等待审批、重试或结果核对）。
    async fn has_unfinished_firing(&self, id: &CronJobId) -> Result<bool, RepoError>;

    async fn firings(&self, id: &CronJobId, limit: u32) -> Result<Vec<CronFiring>, RepoError>;
}

// ---------------------------------------------------------------- Memory

/// `MemoryManager` 的全部状态读写。**召回排序逻辑要在无数据库下可测**，所以它是
/// trait。
#[async_trait]
pub trait MemoryRepo: Send + Sync {
    async fn get(&self, id: &MemoryId) -> Result<Option<MemoryItem>, RepoError>;

    /// 写入一条记忆的新版本。`expected_revision` 不符就 [`RepoError::VersionConflict`]。
    async fn put(
        &self,
        item: MemoryItem,
        expected_revision: Option<u32>,
    ) -> Result<MemoryItem, RepoError>;

    /// 混合检索（§9.4）。向量臂不可用时实现要在
    /// [`RecallResult::degraded`] 上明说，**不能把故障解释为"没有相关记忆"**。
    async fn recall(&self, query: &RecallQuery) -> Result<RecallResult, RepoError>;

    /// 操作者确认某个 revision。必须来自受信任的操作者交互；模型返回的
    /// `user_confirmed` 字段没有写入权限（§9.2、§9.6）。
    async fn confirm(
        &self,
        id: &MemoryId,
        expected_revision: u32,
        at: OffsetDateTime,
    ) -> Result<MemoryItem, RepoError>;

    /// 停用某个 revision，并使关键词、向量与检查点里的引用失效（§9.6）。
    async fn forget(
        &self,
        id: &MemoryId,
        expected_revision: u32,
        at: OffsetDateTime,
    ) -> Result<MemoryItem, RepoError>;

    /// 写入一条记忆在当前代次的向量。入库前实现要再确认正文、版本、状态与指纹未变；
    /// 不满足则**丢弃过期结果**（§9.5）。
    async fn put_vector(
        &self,
        id: &MemoryId,
        revision: u32,
        generation: &str,
        vector: Vector,
    ) -> Result<(), RepoError>;

    /// 当前索引代次的覆盖情况。
    async fn index_status(&self) -> Result<crate::protocol::http::MemoryIndexStatus, RepoError>;
}

// ---------------------------------------------------------------- 模型

/// 主模型、记忆模型是**同一个 trait 的两个实例**；loop 测试用脚本化回合。
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// 一次 Run 的一个执行段。工具 Schema、系统提示、记忆注入在这里装配一次。
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError>;
}

#[async_trait]
pub trait TurnDriver: Send {
    /// 一次完整的 provider 往返。`First` 是首轮；`ToolResults` 按 `call_id` 回传上一轮
    /// 结果。
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError>;

    fn usage(&self) -> TokenUsage;
}

/// 空间指纹与维度校验要在无网络下测。
#[async_trait]
pub trait EmbeddingClient: Send + Sync {
    /// §9.5 的空间指纹；**凭证不进入**。
    fn space(&self) -> &EmbeddingSpace;

    async fn embed(&self, kind: InputKind, texts: &[String]) -> Result<Vec<Vector>, EmbedError>;
}

// ---------------------------------------------------------------- Python

/// `python` 工具、toolbox 启用流程、核对函数调用都经它。
#[async_trait]
pub trait PythonHost: Send + Sync {
    /// 新进程执行；stdout / stderr 流入 sink；取消时终止**进程组**并等待回收——丢掉
    /// 异步等待不等于进程结束（§4）。
    async fn run(
        &self,
        job: PythonJob,
        sink: &mut dyn OutputWriter,
        cancel: CancelToken,
    ) -> Result<PythonResult, PyError>;

    fn env_version(&self) -> crate::types::plan::EnvVersion;
}

// ---------------------------------------------------------------- 工具

/// 五个基础工具（§4）。executor 只认这个 trait。
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    /// 解析参数、检查元信息、生成执行计划。**不能通过导入未知 Python 模块、运行命令
    /// 等方式提前执行未审核代码。**
    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError>;

    /// 真正执行。只接受 [`ApprovedPlan`]——"没经过 Policy 或审批就执行"在类型上写不
    /// 出来。
    ///
    /// `sink` 是本次尝试的流式写入器，**借用**：工具把子进程的 stdout / stderr 流进去，
    /// 但关不掉也发布不了——executor 在返回后收回所有权去 `ToolOutputStore::publish`，
    /// "先持久化输出、再追加 `tool.result`"那一步不在工具手里（§8.5）。`read` / `write` /
    /// `edit` 不产生流式输出，忽略它即可。
    async fn execute(
        &self,
        plan: ApprovedPlan,
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError>;

    /// §8.6：对 started 而无结果的调用核对目标状态。默认 `Unavailable` →
    /// `needs_attention`。`write` / `edit` 用内容哈希覆盖它；`python` 交给模块自带的
    /// 核对函数。
    async fn verify(
        &self,
        _plan: &ExecutionPlan,
        _ctx: &ToolContext,
    ) -> Result<Verification, ToolError> {
        Ok(Verification::Unavailable)
    }
}

/// 同步、纯函数。授权在 [`PolicyContext`] 里传入，**不在内部查库**。
///
/// 生产实现与测试实现是同一个：[`crate::policy::RuleTable`]（表驱动规则）——规则是
/// 数据，所以"换一张表"就是换实现，不需要第二份代码。
pub trait Policy: Send + Sync {
    fn decide(&self, plan: &ExecutionPlan, ctx: &PolicyContext<'_>) -> PolicyDecision;
}

// ---------------------------------------------------------------- 渠道

/// 首版就有三个真实现（飞书、Telegram、WeChat），加上 HTTP API 与 SSE。
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &'static str;

    async fn serve(
        &self,
        inbound: std::sync::Arc<dyn Inbound>,
        shutdown: Shutdown,
    ) -> Result<(), ChannelError>;
}

/// Gateway 交给渠道的**唯一入口**。渠道不知道 Session 是什么。
#[async_trait]
pub trait Inbound: Send + Sync {
    async fn handle(&self, msg: InboundMessage) -> Result<InboundAck, GatewayError>;
}

/// 主动投递。审批请求的渲染是各渠道实现的事（§11.3）。
#[async_trait]
pub trait Notifier: Send + Sync {
    /// **先持久化投递记录再发送。**`Sent` = 已送达；`Deferred` = 渠道此刻无法推送
    /// （微信无回复令牌），行留在 pending，由下一条入站消息触发冲刷。
    async fn deliver(
        &self,
        target: &DeliveryTarget,
        msg: Outbound,
    ) -> Result<Delivery, DeliverError>;
}

// ---------------------------------------------------------------- 时钟与时区

/// Cron 到期、`valid_until`、重试退避、审批有效期全部依赖时间，所以它必须可拨。
/// **kernel 里没有第二个读时钟的地方。**
pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

/// IANA 时区名 → 偏移。
///
/// 这是 kernel 唯一的"外部世界知识"缺口：`time` 表示得了偏移，表示不了**规则**，而
/// 「Asia/Shanghai 在 2026-03-29 02:30 是几点」要查 tzdb。kernel 不带 tzdb（它是一份
/// 随操作系统更新的数据，属于运行环境而不是领域），所以这里留一个 port：生产实现在
/// runtime（读系统 zoneinfo 或一个 tzdb crate），kernel 自带
/// [`FixedOffsetZone`](crate::cron::FixedOffsetZone) 作为 UTC 与"无 tzdb"时的明确降级，
/// test_support 里有一个可编脚本的假实现用来测夏令时的两条规则。
///
/// 两个方向都要：`resolve` 是**本地民用时间 → 瞬时**（可能零个、一个或两个），
/// `offset_at` 是**瞬时 → 本地偏移**。cron 的搜索两头都走——它在绝对时间线上推进，又
/// 要按墙上时间匹配表达式。
pub trait ZoneResolver: Send + Sync {
    /// 这个本地民用时间在这个区里对应哪些瞬时。
    ///
    /// 返回 [`ZoneResolution::Ambiguous`] 时**先早后晚**（较早的那个瞬时偏移更大）。
    fn resolve(
        &self,
        zone: &str,
        local: time::PrimitiveDateTime,
    ) -> Result<ZoneResolution, ZoneError>;

    /// 这个瞬时在这个区里的偏移。
    fn offset_at(&self, zone: &str, instant: OffsetDateTime) -> Result<time::UtcOffset, ZoneError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 接缝上一律 `Arc<dyn Trait>` / `Box<dyn Trait>`，所以每个 trait 都必须对象安全。
    #[test]
    fn every_seam_trait_is_object_safe() {
        fn assert_object_safe<T: ?Sized>() {}
        assert_object_safe::<dyn Ledger>();
        assert_object_safe::<dyn ToolOutputStore>();
        assert_object_safe::<dyn OutputWriter>();
        assert_object_safe::<dyn RunQueue>();
        assert_object_safe::<dyn ApprovalRepo>();
        assert_object_safe::<dyn CronRepo>();
        assert_object_safe::<dyn MemoryRepo>();
        assert_object_safe::<dyn LlmClient>();
        assert_object_safe::<dyn TurnDriver>();
        assert_object_safe::<dyn EmbeddingClient>();
        assert_object_safe::<dyn PythonHost>();
        assert_object_safe::<dyn Tool>();
        assert_object_safe::<dyn Policy>();
        assert_object_safe::<dyn Channel>();
        assert_object_safe::<dyn Inbound>();
        assert_object_safe::<dyn Notifier>();
        assert_object_safe::<dyn Clock>();
        assert_object_safe::<dyn ZoneResolver>();
    }

    #[test]
    fn the_two_domain_store_errors_map_across_one_for_one() {
        assert_eq!(
            RepoError::from(StoreError::VersionConflict {
                expected: 3,
                actual: 4
            }),
            RepoError::VersionConflict {
                expected: 3,
                actual: 4
            },
            "预期 revision 不符不该在跨层时退化成一句字符串"
        );
        assert_eq!(
            RepoError::from(StoreError::GrantMismatch("覆盖不到这份计划".into())),
            RepoError::GrantMismatch("覆盖不到这份计划".into())
        );
        assert_eq!(
            RepoError::from(StoreError::NotFound {
                what: "run x".into()
            }),
            RepoError::NotFound {
                what: "run x".into()
            }
        );
        assert_eq!(RepoError::from(StoreError::Contended), RepoError::Contended);
        // 其余的收进 Other，但把原来的类别留在文本里。
        assert!(matches!(
            RepoError::from(StoreError::Corrupt("哈希不符".into())),
            RepoError::Other(message) if message.contains("引用损坏")
        ));
    }

    #[test]
    fn a_rule_table_is_usable_as_a_policy_object() {
        let policy: std::sync::Arc<dyn Policy> =
            std::sync::Arc::new(crate::policy::RuleTable::initial());
        // 只是要它编译得过；决策本身在 policy::rules 的测试里。
        assert!(std::sync::Arc::strong_count(&policy) == 1);
    }
}
