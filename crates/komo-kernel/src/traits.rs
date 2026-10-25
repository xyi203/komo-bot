//! 全部 trait（§13.5）。
//!
//! 一个 trait 只在两种情况下存在：**有第二个实现**（生产之外还有测试替身，或有多个
//! 后端），或**要做编译防火墙**（上层依赖 trait 就够，不需要看到 toasty / reqwest /
//! axum 类型）。其余组件是具体类型，直接构造、直接测；不为"以后可能换"预留 trait。
//!
//! 不是 trait 的东西，记在这里免得有人再造一个：`ToolExecutor`、`MemoryManager`、
//! `SkillRegistry`、`Dispatcher`、`CronScheduler` —— 各只有一个实现，依赖下面这些 trait 就可测。**也没有 `Policy`**：
//! 2026-10-11 起 komo 不再在执行前做判决（§7）——模型要的就执行，唯一的硬限制是
//! `write` / `edit` 在 `prepare` 里拒绝写 komo 自己的状态，那是工具的事，不是一个接缝。
//!
//! 接缝上一律 `Arc<dyn Trait>` / `Box<dyn Trait>`，异步 trait 用 `async_trait`——对象
//! 安全，一次堆分配相对模型往返可忽略。

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::cron::{CronFiring, CronJob, JobStatus, ZoneError, ZoneResolution};
use crate::protocol::{InboundAck, InboundMessage};
use crate::types::chat::{Delivery, DeliveryTarget, Outbound};
use crate::types::ids::{CronJobId, MemoryId};
use crate::types::memory::{MemoryItem, RecallQuery, RecallResult};
use crate::types::model::{EmbeddingSpace, InputKind, TokenUsage, Vector};
use crate::types::plan::{ExecutionPlan, Replay};
use crate::types::refs::{
    AttemptRef, ImageRef, InterruptedOutput, OutputRef, PublishedOutput, ToolResultBody,
    VerifiedOutput,
};
use crate::types::systemone::{SystemOneError, SystemOneRequest, SystemOneResponse};
use crate::types::tool::{
    CancelToken, ToolContext, ToolDefinition, ToolError, ToolExposure, ToolNamespace, ToolOutput,
};
use crate::types::turn::{LlmError, Round, RoundInput, TurnRequest};

// ---------------------------------------------------------------- 错误

/// 存储层失败。
///
/// 它比"IO 出错了"宽一点：store 的 `with_write_retry` 闭包只有这一条错误通道
/// （§8.2），而闭包里跑的正是那些要报**领域性**失败的事务——比如预期 revision
/// 不符。没有对应的变体，那些失败只能塞进 `Other(String)` 再靠字符串
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
    #[error("写入争用，重试超限")]
    Contended,
    #[error("{0}")]
    Other(String),
}

/// store 的事务错误通道抬到仓储接口上。
///
/// 领域性变体**逐个对上**，不经字符串；其余的按语义收进最接近的那个。
impl From<StoreError> for RepoError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotFound { what } => RepoError::NotFound { what },
            StoreError::VersionConflict { expected, actual } => {
                RepoError::VersionConflict { expected, actual }
            }
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

/// [`TaskSpawner`] 的失败：`task` 拿不到一个能交给模型的结论。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpawnError {
    /// 建任务会话或提交第一条输入没做成（store / 账本错误）。
    #[error("{0}")]
    Failed(String),
    /// `task_id` 的短号解析不到——不是这个会话派出去的任务，或者根本没有这个短号。
    #[error("{0}")]
    UnknownTask(String),
    /// 短号在候选里不唯一：正文列出候选（延长到 6 位，§4.3）。
    #[error("{0}")]
    Ambiguous(String),
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

// ---------------------------------------------------------------- 输出存储

/// 子进程 stdout / stderr 的流式落盘。`bash` 的测试需要替身。
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

    /// 按引用读回一张结果图片并校验大小与哈希；不匹配返回 [`StoreError::Corrupt`]。
    /// 默认不支持：没有图片的替身不用管它。
    async fn open_image(&self, image: &ImageRef) -> Result<Vec<u8>, StoreError> {
        Err(StoreError::Other(format!(
            "这个输出存储不存图片：{}",
            image.content.path
        )))
    }

    /// 一次被打断的尝试留下的部分输出，各取末尾至多 `tail_bytes`（§8.6）。没有留下就是
    /// [`InterruptedOutput::default`]。只读：接管它的是之后对同一尝试的 [`Self::begin`]。
    async fn interrupted(
        &self,
        attempt: &AttemptRef,
        tail_bytes: usize,
    ) -> Result<InterruptedOutput, StoreError>;

    /// 这次尝试盘上有没有一份**完整**的输出：`output.json` 在，身份（Session / run / call /
    /// attempt）与这个 [`AttemptRef`] 对得上，stdout / stderr 的引用也对得上（§5 恢复表
    /// "盘上有身份齐全、哈希对得上的 output.json"）。有就返回发布时那份 [`PublishedOutput`]，
    /// 调用方据此补提交结果、复用原输出，不重做。没有（或对不上）就是 `None`——`.partial`
    /// 不算完成。默认不支持：没有落盘的替身不用管它。
    async fn find_published(
        &self,
        _attempt: &AttemptRef,
    ) -> Result<Option<PublishedOutput>, StoreError> {
        Ok(None)
    }
}

/// 工具起了一个子进程：身份足够事后判断"还是不是那个进程"（pid 会被复用，所以带上起来的
/// 时刻；`docs/storage-durable.md` §5 的 `probe` 阶段读它）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildStarted {
    pub pid: u32,
    /// 进程组（取消时终止的是它）。
    pub pgid: u32,
    /// 起来的时刻（Unix 毫秒，真实时钟）。
    pub started_at_ms: i64,
}

/// 一次尝试的流式写入器。
#[async_trait]
pub trait OutputWriter: Send + Sync {
    async fn write_stdout(&mut self, chunk: &[u8]) -> Result<(), StoreError>;
    async fn write_stderr(&mut self, chunk: &[u8]) -> Result<(), StoreError>;
    /// 把一张结果图片写进这次尝试的输出目录（同步落盘），返回它的引用。执行器在发布前调。
    /// 默认不支持。
    async fn write_image(&mut self, mime: &str, _bytes: &[u8]) -> Result<ImageRef, StoreError> {
        Err(StoreError::Other(format!("这个写入器不存图片（{mime}）")))
    }
    /// 这次尝试的身份。
    fn attempt(&self) -> &AttemptRef;
    /// 到目前为止写了多少字节——输出长度预算按它判。
    fn bytes_written(&self) -> u64;
    /// 工具刚起了一个子进程。默认什么都不做；新存储的工具任务借它把子进程身份写进
    /// `komo.live.tools[].pid`，重启后据此进 `probe` 阶段。
    fn child_started(&mut self, _child: ChildStarted) {}
}

// ---------------------------------------------------------------- Cron

/// 调度器 + CLI + Gateway API 三方共用。
#[async_trait]
pub trait CronRepo: Send + Sync {
    async fn list(&self) -> Result<Vec<CronJob>, RepoError>;

    async fn get(&self, id: &CronJobId) -> Result<Option<CronJob>, RepoError>;

    /// 创建或整体替换一个 Job。**版本递增**。
    async fn put(&self, job: CronJob) -> Result<CronJob, RepoError>;

    /// 只写调度状态：下一个槽位、生命周期、触发层面的错误。
    ///
    /// **它不递增版本**，这是它与 [`CronRepo::put`] 的全部区别，也是它存在的全部理由：
    /// 推进槽位不是定义变更。走 `put` 的话每次触发都会让版本 +1，而版本回答的是"Job
    /// 定义改没改过"（§10）——每次触发都 +1，这个数就什么都说明不了。
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

    async fn firings(&self, id: &CronJobId, limit: u32) -> Result<Vec<CronFiring>, RepoError>;
}

// ---------------------------------------------------------------- Memory

/// 离线整理一组的落库请求（§9.8）：一个事务里先核对、再写、最后盖游标。
#[derive(Debug, Clone)]
pub struct ConsolidationWrite {
    /// 判定时看到的每个成员：id、revision、updated_at。任何一个对不上（被后台提取、
    /// 操作者的 confirm / forget 动过）整组一个字都不写、也不盖游标。
    pub expected: Vec<(MemoryId, u32, OffsetDateTime)>,
    /// 要写的新形态。revision 与 `expected` 里的相同——整理不升 revision。
    pub puts: Vec<MemoryItem>,
    /// 走 forget 同一条路停用的条目（状态、关键词与向量一起失效）。
    pub forgets: Vec<MemoryId>,
    /// 写完后给 `expected` 里每条盖上的 `consolidated_at`。
    pub stamp_at: OffsetDateTime,
}

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

    /// 离线整理的游标（§9.8）：每条最近一次被整理看过的时刻，没看过是 `None`。
    async fn consolidation_marks(
        &self,
        ids: &[MemoryId],
    ) -> Result<Vec<(MemoryId, Option<OffsetDateTime>)>, RepoError>;

    /// 离线整理一组的落库（§9.8）：见 [`ConsolidationWrite`]。对不上预期就
    /// [`RepoError::VersionConflict`]，什么都不写。
    async fn apply_consolidation(&self, write: ConsolidationWrite) -> Result<(), RepoError>;

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
    /// 结果，`revised` 按 `provider_call_id` 就地换掉更早结果的正文。
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

/// 一个**判断后端**（TypeSafe「System One」）：给一份 `state` 与几问，回几个能直接进
/// 代码的数（概率、分数、选择）。
///
/// 它不是一个模型角色：输出不是文本、不进对话历史、也不产生工具调用。首版的用处只有一处
/// ——把融合后的记忆短名单重排一遍（§9.4）。有自己的 trait 是为了能**在没有网络、没有凭证**
/// 的情况下把"重排怎么用这些数"测完。
#[async_trait]
pub trait SystemOne: Send + Sync {
    async fn ask(&self, request: SystemOneRequest) -> Result<SystemOneResponse, SystemOneError>;
}

// ---------------------------------------------------------------- 工具

/// 六个基础工具（§4）。executor 只认这个 trait。
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    /// 解析参数、检查元信息、生成执行计划。**不能通过导入未知 Python 模块、运行命令
    /// 等方式提前执行代码**——计划先落盘（`tool.planned`），副作用才能发生（§8.5）。
    ///
    /// 唯一的硬限制也在这里：`write` / `edit` 拒绝目标落在 komo 自己的状态里，返回
    /// [`ToolError::Denied`]（§7、§8.10）。
    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError>;

    /// 真正执行。`plan` 按值交进来：executor 在 [`Ledger::start_call`] 返回之后才调它
    /// （§8.5），组合计划的第二步从 `plan.then_run` 取。
    ///
    /// `sink` 是本次尝试的流式写入器，**借用**：工具把子进程的 stdout / stderr 流进去，
    /// 但关不掉也发布不了——executor 在返回后收回所有权去 `ToolOutputStore::publish`，
    /// "先持久化输出、再追加 `tool.result`"那一步不在工具手里（§8.5）。`read` / `write` /
    /// `edit` 不产生流式输出，忽略它即可。
    async fn execute(
        &self,
        plan: ExecutionPlan,
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError>;

    /// §8.6：这个工具**现在**声明的重放策略。重启后一次 started 而没有结果的调用，
    /// 只有计划里记下的与这里的都是 [`Replay::Safe`] 才带原计划重跑；否则中断结果交给
    /// 模型。默认 `Unsafe`：只有重跑一遍确实无害的工具（读、搜索）才覆盖它。
    fn replay(&self) -> Replay {
        Replay::Unsafe
    }

    /// 模型怎么够到它（`docs/mcp.md` §3）：直接声明，还是只在 codemode 脚本里调。默认直接
    /// 声明；只改 schema 给不给模型，不改能力面——codemode 里的那些照样要在能力面里。
    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    /// 它属于哪一组（一台 MCP 服务器）。脚本里的 `describe_namespace` 与系统提示的服务器
    /// 段按它列。
    fn namespace(&self) -> Option<ToolNamespace> {
        None
    }
}

// ---------------------------------------------------------------- 监听

/// `monitor` / `monitor_stop` 的接缝（`docs/monitor.md`）：挂一个长时间跑的脚本，它的
/// stdout 每出一行（按批合并）就作为一条内部输入交回挂它的会话。
///
/// 与 [`TaskSpawner`] 同一个理由走编排操作：进程要活得比这次调用、这条 Run 都久，只有
/// Gateway 拿得住它，也只有 Gateway 能 `submit`。生产实现是 Gateway 的
/// `GatewayMonitors`；没有装配它时两个操作按"这台 Gateway 没有接监听"干净失败。
#[async_trait]
pub trait Monitors: Send + Sync {
    /// 起一个监听。事件交回 `from` 所在的会话，投给 `from` 的 `runs.peer`。
    ///
    /// `request_key` 由调用方按 `monitor:{run}:{call}` 拼好。`fresh = false` 是"上一世
    /// 已经 `start_call` 过、这一世只是回来收口"：只认同一个键下还在跑的那个，认不到就答
    /// [`MonitorError::Interrupted`]——**绝不重新起进程**（§8.6：任意命令不自动重跑）。
    async fn start(
        &self,
        from: &crate::types::ids::RunId,
        request_key: crate::types::ids::RequestKey,
        spec: crate::types::monitor::MonitorSpec,
        fresh: bool,
    ) -> Result<crate::types::monitor::MonitorHandle, MonitorError>;

    /// 停掉 `from` 所在会话挂着的一个监听。`monitor_id` 是短号原文。
    async fn stop(
        &self,
        from: &crate::types::ids::RunId,
        monitor_id: &str,
    ) -> Result<crate::types::monitor::MonitorStopped, MonitorError>;
}

/// [`Monitors`] 的失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MonitorError {
    /// 起不来、记不上。
    #[error("{0}")]
    Failed(String),
    /// 上一世挂的监听随 Gateway 重启没了，没有自动重挂。
    #[error("{0}")]
    Interrupted(String),
    /// 短号对不上这个会话正在跑的监听；正文列出还在跑的那些。
    #[error("{0}")]
    Unknown(String),
    /// 短号在候选里不唯一。
    #[error("{0}")]
    Ambiguous(String),
}

// ---------------------------------------------------------------- 任务分发

/// `task` 的接缝（`docs/background-tasks.md`）：把一个自包含任务派进一条**独立的任务
/// 会话**，或者往已有的任务会话里再提交一句。旧计划里的 `dispatch` / `follow` 走同两个方法。
///
/// 工具只有 [`crate::types::tool::ToolContext`]，没有 Ledger 也没有 Gateway 句柄，
/// `komo-runtime` 不依赖 Gateway——所以这条缝走**编排操作**（`Operation::Task`）：executor
/// 在放行之后调它，立即以普通工具结果收尾，不等任务跑完（不返回 `RoundStop::Dependency`）。
///
/// 生产实现是 Gateway 的 `GatewayTaskSpawner`（建会话、`GatewayState::submit`）；没有
/// 装配它时（`ToolExecutor` 的这个槽是空的）按"这台 Gateway 没有接任务分发"干净失败，
/// 不是一个悬着的调用。
#[async_trait]
pub trait TaskSpawner: Send + Sync {
    /// 建一个任务会话并提交第一条输入。任务收尾时结果交回**派它的那个会话**，由那边
    /// 转告来源渠道（`from` 的 `runs.peer`）。
    ///
    /// `request_key` 由调用方按 `task:{run}:{call}`（旧计划 `dispatch:{run}:{call}`）拼好
    /// （§4.2 的幂等）：同一次调用重放时，它落到
    /// [`crate::types::turn::AcceptInput::request_key`] 上，账本按请求键去重，所以重放拿回
    /// 的是**同一个**任务会话，不会多建一个。
    async fn spawn(
        &self,
        from: &crate::types::ids::RunId,
        request_key: crate::types::ids::RequestKey,
        spec: crate::types::task::TaskSpec,
    ) -> Result<crate::types::task::TaskHandle, SpawnError>;

    /// 往一个已有的任务会话里再提交一条输入。`task_id` 是任务短号（模型给的原文，
    /// 解析在实现里做——只有 Gateway 查得到"这个会话派出去过哪些任务"）。`options.model`
    /// 省略时沿用那个任务上一条 Run 的模型与档位。
    async fn follow(
        &self,
        from: &crate::types::ids::RunId,
        request_key: crate::types::ids::RequestKey,
        task_id: &str,
        text: &str,
        options: crate::types::task::TaskOptions,
    ) -> Result<crate::types::task::FollowOutcome, SpawnError>;

    /// 查一个任务现在跑到哪了：只读，不往那个会话里提交任何东西。`task_id` 解析同 `follow`。
    async fn status(
        &self,
        from: &crate::types::ids::RunId,
        task_id: &str,
    ) -> Result<crate::types::task::TaskProgress, SpawnError>;
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

/// 主动投递。各类消息的渲染是各渠道实现的事（§11.3）。
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

/// Cron 到期、`valid_until`、重试退避全部依赖时间，所以它必须可拨。
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
        assert_object_safe::<dyn ToolOutputStore>();
        assert_object_safe::<dyn OutputWriter>();
        assert_object_safe::<dyn CronRepo>();
        assert_object_safe::<dyn MemoryRepo>();
        assert_object_safe::<dyn LlmClient>();
        assert_object_safe::<dyn TurnDriver>();
        assert_object_safe::<dyn EmbeddingClient>();
        assert_object_safe::<dyn Tool>();
        assert_object_safe::<dyn TaskSpawner>();
        assert_object_safe::<dyn Channel>();
        assert_object_safe::<dyn Inbound>();
        assert_object_safe::<dyn Notifier>();
        assert_object_safe::<dyn Clock>();
        assert_object_safe::<dyn ZoneResolver>();
    }

    #[test]
    fn the_domain_store_errors_map_across_one_for_one() {
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
}
