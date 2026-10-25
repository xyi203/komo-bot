//! MemoryManager：自动积累、混合检索、向量索引与代次（§9）。
//!
//! 「MemoryManager 对 AgentRuntime 提供三个主要入口：recall、process_completed_run、
//! apply_user_decision。提取、去重、证据校验、冲突处理与索引协调隐藏在该模块内部；
//! **它不向 LLM 注册额外工具**」（§9.3）——所以这个模块没有 `Tool` 实现，一个都没有。
//! 记忆进上下文只有一条路：召回结果渲染成系统提示末尾的一段**数据**，而 §9.7 那句
//! "不能成为系统指令、授权或自我更新工具的依据"就写在那段正文的抬头里。渲染
//! 函数本身在 `komo-agent::context::memory`（runtime 不依赖 agent），这里只按
//! [`MemoryParts::render`] 那个函数指针调用它（`docs/agent.md` §13.2）；渲染出的
//! [`Injection`] 经 Gateway 拼进 `TurnRequest.system_prompt`，不再绕经适配器的钩子。
//!
//! 模块之间：
//!
//! ```text
//! MemoryManager ─┬─ extract::LearningPass   一个已完成 Run → 结构化观察（记忆模型）
//!                ├─ consolidate::Consolidator  去重 / 冲突 → 库里的变更
//!                ├─ index::IndexBuilder     空间指纹 → 代次 → 批量向量 → 事务切换
//!                └─ render（外部函数指针）   召回结果 → 注入段 + 审计用的 MemoryUse
//! ```
//!
//! **模型与仓储之间隔着这一层，是为了守住三句话**：模型返回的 `user_confirmed` 字段没
//! 有写入权限（§9.2）；新推断不能覆盖用户陈述（§9.6）；模型调用期间不持有数据库事务
//! （§9.5）。前两句在 [`consolidate`]，第三句在 [`index`] 与 [`MemoryManager::search`]
//! ——向量在事务外算好，交给仓储的是一个 [`RecallQuery::query_vector`]。

mod add;
mod consolidate;
pub mod durable;
mod extract;
mod index;
mod offline;
mod rerank;
mod stale;
mod sweep;
mod work;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::config::{ConsolidateConfig, MemoryConfig, RetrievalConfig};
use komo_kernel::protocol::http::{
    IndexState, MemoryIndexStatus, MemorySweepRequest, MemorySweepResponse,
};
use komo_kernel::traits::{
    Clock, EmbedError, EmbeddingClient, LlmClient, MemoryRepo, RepoError, StoreError, SystemOne,
};
use komo_kernel::types::agent::MemoryMode;
use komo_kernel::types::ids::{MemoryId, RunId, Seq, SessionId};
use komo_kernel::types::memory::{
    Injection, MemoryItem, MemoryScope, MemoryState, MemoryWork, RecallDiagnostics, RecallQuery,
    RecallResult, RetrievalMode, scope_allowed,
};
use komo_kernel::types::model::{EmbeddingSpace, InputKind, ModelConfig, Vector};
use komo_kernel::types::turn::{LlmError, MemoryUse};
use komo_store::repos::memory::{GenerationRecord, IndexCandidate};
use time::OffsetDateTime;

pub use consolidate::{Relation, Verdict};
pub use extract::Transcript;
pub use extract::{Observation, PROMPT_VERSION, RawObservation, SaidBy};
// 从子代理最后一条回复里取 JSON 的那一步与提取共用：两边都是"模型回复里那个配平的 JSON
// 对象"，两套括号处理迟早会有一边少一个分支（`delegate` 的复验在 executor 里）。
pub(crate) use extract::parse_json_object;
pub use index::{IndexBuilder, IndexOutcome, MIN_ACTIVATION_COVERAGE};
pub use work::{MemoryBatch, MemoryWorkItem, MemoryWorkLog};

/// 记忆这一层的失败。
///
/// **`VectorUnconfigured` 与 `VectorUnavailable` 是两件事**：前者是配置错误（选了
/// hybrid / vector 却没有 `memory.embedding` alias，§9.4 说它"不能静默变成长期关键词模式"），
/// 后者是端点这一刻不通。两者都不能被当成"没有相关记忆"。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    #[error("`[memory]` 没有启用")]
    Disabled,
    #[error("检索模式要向量，但没有配置 `memory.embedding` alias：这是配置错误，不是空结果")]
    VectorUnconfigured,
    #[error("向量后端不可用：{0}")]
    VectorUnavailable(String),
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("读不出会话：{0}")]
    Ledger(String),
    #[error("记忆模型：{0}")]
    Model(String),
    #[error("提取结果不合法：{0}")]
    Invalid(String),
}

impl MemoryError {
    /// 这一次失败**再试也不会变**。
    ///
    /// 只有一种：证据读不出来。JSONL 损坏或已提交范围缺失是 §8.3 的"停止受影响会话，
    /// 报告损坏"，不是"等一会儿再试"——文件不会自己长回来。其余几种都能好起来：端点
    /// 恢复、配置改对、模型的下一句话不一样（[`MemoryError::Model`]、
    /// [`MemoryError::Invalid`]、[`MemoryError::VectorUnavailable`]、
    /// [`MemoryError::VectorUnconfigured`]、[`MemoryError::Store`]），所以照旧放回
    /// `pending`。
    pub fn is_permanent(&self) -> bool {
        matches!(self, MemoryError::Ledger(_))
    }
}

impl From<LlmError> for MemoryError {
    fn from(error: LlmError) -> Self {
        MemoryError::Model(error.to_string())
    }
}

impl From<EmbedError> for MemoryError {
    fn from(error: EmbedError) -> Self {
        MemoryError::VectorUnavailable(error.to_string())
    }
}

/// `MemoryRepo` 之外的那几口：库存、索引候选、代次切换、使用计数。
///
/// 它不在 kernel 的 [`MemoryRepo`] 上，因为那个 trait 说的是"一条记忆的读写与一次检索"，
/// 而这里说的是**库存与索引工程**。存活在 runtime 这一侧，于是 kernel 的接口不必为索引
/// 器再长一截，测试也能给一个内存替身。
#[async_trait]
pub trait MemoryCatalog: Send + Sync {
    async fn list(
        &self,
        scope: Option<MemoryScope>,
        state: Option<MemoryState>,
        limit: usize,
    ) -> Result<Vec<MemoryItem>, RepoError>;

    async fn find_by_content(
        &self,
        scope: &MemoryScope,
        content: &str,
    ) -> Result<Option<MemoryItem>, RepoError>;

    /// 当前版本的证据引了这个会话事件的那些记忆（提取前找手动记过的，§9.3）。
    async fn citing_session(&self, session: &SessionId) -> Result<Vec<MemoryItem>, RepoError>;

    async fn pending_vectors(
        &self,
        generation: &str,
        after: Option<String>,
        limit: usize,
    ) -> Result<Vec<IndexCandidate>, RepoError>;

    async fn indexable_total(&self) -> Result<u64, RepoError>;

    async fn indexed_in(&self, generation: &str) -> Result<u64, RepoError>;

    async fn generations(&self) -> Result<Vec<GenerationRecord>, RepoError>;

    async fn put_generation(
        &self,
        generation: &str,
        space: Option<&EmbeddingSpace>,
        state: IndexState,
        active: bool,
    ) -> Result<(), RepoError>;

    async fn set_generation_progress(
        &self,
        generation: &str,
        state: IndexState,
        indexed: u64,
        total: u64,
        errors: &[String],
    ) -> Result<(), RepoError>;

    async fn activate_generation(&self, generation: &str) -> Result<(), RepoError>;

    async fn prune_generations(&self, keep: &str) -> Result<u64, RepoError>;

    async fn record_use(&self, ids: &[MemoryId], at: OffsetDateTime) -> Result<(), RepoError>;

    /// 一批提取里被指认用上的记忆各 +1（§9.10）。
    async fn record_adoption(&self, ids: &[MemoryId], at: OffsetDateTime) -> Result<(), RepoError>;
}

#[async_trait]
impl MemoryCatalog for komo_store::TursoMemoryRepo {
    async fn list(
        &self,
        scope: Option<MemoryScope>,
        state: Option<MemoryState>,
        limit: usize,
    ) -> Result<Vec<MemoryItem>, RepoError> {
        komo_store::TursoMemoryRepo::list(self, scope, state, limit).await
    }

    async fn find_by_content(
        &self,
        scope: &MemoryScope,
        content: &str,
    ) -> Result<Option<MemoryItem>, RepoError> {
        komo_store::TursoMemoryRepo::find_by_content(self, scope, content).await
    }

    async fn citing_session(&self, session: &SessionId) -> Result<Vec<MemoryItem>, RepoError> {
        komo_store::TursoMemoryRepo::citing_session(self, session).await
    }

    async fn pending_vectors(
        &self,
        generation: &str,
        after: Option<String>,
        limit: usize,
    ) -> Result<Vec<IndexCandidate>, RepoError> {
        komo_store::TursoMemoryRepo::pending_vectors(self, generation, after, limit).await
    }

    async fn indexable_total(&self) -> Result<u64, RepoError> {
        komo_store::TursoMemoryRepo::indexable_total(self).await
    }

    async fn indexed_in(&self, generation: &str) -> Result<u64, RepoError> {
        komo_store::TursoMemoryRepo::indexed_in(self, generation).await
    }

    async fn generations(&self) -> Result<Vec<GenerationRecord>, RepoError> {
        komo_store::TursoMemoryRepo::generations(self).await
    }

    async fn put_generation(
        &self,
        generation: &str,
        space: Option<&EmbeddingSpace>,
        state: IndexState,
        active: bool,
    ) -> Result<(), RepoError> {
        komo_store::TursoMemoryRepo::put_generation(self, generation, space, state, active).await
    }

    async fn set_generation_progress(
        &self,
        generation: &str,
        state: IndexState,
        indexed: u64,
        total: u64,
        errors: &[String],
    ) -> Result<(), RepoError> {
        komo_store::TursoMemoryRepo::set_generation_progress(
            self, generation, state, indexed, total, errors,
        )
        .await
    }

    async fn activate_generation(&self, generation: &str) -> Result<(), RepoError> {
        komo_store::TursoMemoryRepo::activate_generation(self, generation).await
    }

    async fn prune_generations(&self, keep: &str) -> Result<u64, RepoError> {
        komo_store::TursoMemoryRepo::prune_generations(self, keep).await
    }

    async fn record_use(&self, ids: &[MemoryId], at: OffsetDateTime) -> Result<(), RepoError> {
        komo_store::TursoMemoryRepo::record_use(self, ids, at).await
    }

    async fn record_adoption(&self, ids: &[MemoryId], at: OffsetDateTime) -> Result<(), RepoError> {
        komo_store::TursoMemoryRepo::record_adoption(self, ids, at).await
    }
}

/// 一个 Session 的事件从哪里来。
///
/// 提取只**读**事件：生产实现是 `DurableSessionEvents`（`durable.rs`，按 conversation 读 entry
/// 再投影成旧事件词汇），测试给一个装好事件的表。
#[async_trait]
pub trait SessionEvents: Send + Sync {
    async fn events_of(
        &self,
        session: &SessionId,
    ) -> Result<Vec<komo_kernel::events::Event>, MemoryError>;
}

/// 一批后台处理的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassReport {
    /// 领到并处理完的 Run 数。
    pub processed: usize,
    /// 明确跳过的（取消的 Run、没有新证据的）。
    pub skipped: usize,
    /// 写进库的记忆变更数。
    pub applied: usize,
    /// 被指认"用上了"并通过校验的记忆数（§9.10，每批每条至多一次）。
    pub used: usize,
    /// 失败并放回 `pending` 的。
    pub failed: usize,
    /// 失败且**不再重试**的（证据读不出来，标记翻成 `error`）。
    pub abandoned: usize,
}

/// 召回结果 → 注入段的渲染函数（§13.2）：实现在 `komo-agent::context::memory::render`，
/// Gateway 装配 [`MemoryParts`] 时把它递进来。`fn` 指针而不是 trait：只有一个实现，也
/// 不必为了传一份渲染逻辑就让 `MemoryManager` 认识 `dyn` 对象——runtime 不依赖 agent。
pub type RenderInjection = fn(&[MemoryItem], u32) -> Injection;

/// 装配一台 MemoryManager 要的东西。
pub struct MemoryParts {
    pub config: MemoryConfig,
    pub repo: Arc<dyn MemoryRepo>,
    pub catalog: Arc<dyn MemoryCatalog>,
    /// `None` = 没有配置 `memory.embedding` alias，只有关键词臂。
    pub embeddings: Option<Arc<dyn EmbeddingClient>>,
    /// 召回短名单的判断后端（§9.4）。`None` = 没配 `[typesafe]`，或者 `retrieval.rerank`
    /// 关着——那就照融合顺序给。
    pub reranker: Option<Reranker>,
    pub llm: Arc<dyn LlmClient>,
    pub events: Arc<dyn SessionEvents>,
    pub work: Arc<dyn MemoryWorkLog>,
    pub clock: Arc<dyn Clock>,
    /// 召回结果怎样渲染成注入段；Gateway 装配时给 `komo_agent::context::memory::render`。
    pub render: RenderInjection,
}

/// 记忆重排用的判断后端（§9.4）。
#[derive(Clone)]
pub struct Reranker {
    pub backend: Arc<dyn SystemOne>,
    /// 判断层的模型名（`[typesafe].model`）。**进请求体，不是凭证**，所以可以进日志。
    pub model: String,
}

impl std::fmt::Debug for Reranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reranker")
            .field("model", &self.model)
            .finish()
    }
}

/// §9 的那一层。
pub struct MemoryManager {
    /// 开关、记忆模型与召回预算：配置热重载换得掉（[`MemoryManager::install_config`]），
    /// 每次用时读当前这一份（§3）。
    live: RwLock<Live>,
    repo: Arc<dyn MemoryRepo>,
    catalog: Arc<dyn MemoryCatalog>,
    /// 向量后端。**可以后装**：Gateway 先把服务起来，再在后台探维度（§9.5「省略
    /// `dimensions` 时先探一次」）——那一次探测是一个网络往返，挂在启动路径上就是让
    /// 「Gateway 就绪」等网络（本机实测一个只收不回话的端点 = 等满模型超时）。探到就
    /// [`MemoryManager::install_embeddings`]，探不到就一直空着，检索按 §9.4 如实降级。
    embeddings: RwLock<Option<Arc<dyn EmbeddingClient>>>,
    /// 判断后端。与向量后端同一个形状（可以后装）：配置热重载时换成新的那一份，运行中的
    /// 查询下一次就读到新的。
    reranker: RwLock<Option<Reranker>>,
    /// 配了 `memory.embedding` alias 吗。**与"后端在不在手上"是两件事**：配了但探测还没
    /// 落定（或探测失败）时，§9.4 的规则是"hybrid 退化成关键词、vector-only 报不可用"，
    /// 不是"配置错误"。
    configured: bool,
    events: Arc<dyn SessionEvents>,
    work: Arc<dyn MemoryWorkLog>,
    clock: Arc<dyn Clock>,
    /// 召回结果 → 注入段（`komo-agent::context::memory::render`，§13.2）。
    render: RenderInjection,
    /// 每个会话**当前这一段对话**注入的那一块（§9.4）。
    ///
    /// 为什么要它：注入段在 system 消息里，而服务端的前缀缓存按**最长公共前缀**命中。
    /// 每次 Run 重新召回、重新渲染——哪怕只是条目顺序变了、某条记忆的 revision 数字变了
    /// ——整个请求从第一条消息起就与上一次不同，缓存命中率归零，钱和延迟都付在重复的
    /// 前缀上。所以：**同一段对话里逐字复用**，换段（`conversation.boundary`，即 `/new`）
    /// 或者这一段第一次用时才重新召回。
    ///
    /// 代价写在 §9.4 里：会话进行中新召回得到的条目，要等下一段才进来；被改写的条目在
    /// 本段内仍按原样注入（正文里带着 `id@revision`，读的人知道引用的是哪一版）。
    pins: Mutex<BTreeMap<SessionId, Pinned>>,
    /// 索引调度的账（§9.5「索引自动补齐」）：同一时刻最多一个构建任务。
    /// `POST /v1/memory-index/rebuild` 的幂等也是它。
    indexing: Mutex<IndexSchedule>,
    /// 叫醒退避中的构建任务。只有操作者手动重建用它：人在等结果。
    index_wake: tokio::sync::Notify,
    /// 已经报过一次失败的 Run。**只为不刷日志**：一分钟后还失败是同一件事，每拍都
    /// `warn` 一遍只会把别人的日志挤下去。成功的那些在这里被划掉，下次再失败照样报。
    reported: Mutex<BTreeSet<RunId>>,
}

impl std::fmt::Debug for MemoryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self.live();
        f.debug_struct("MemoryManager")
            .field("enabled", &live.enabled)
            .field("model", &live.model.model)
            .field("has_embeddings", &self.vector_backend().is_some())
            .finish()
    }
}

/// `[memory]` 里热重载换得掉的那几样。
///
/// 模型配置与后端实例**成对换**：`RoutingLlm` 按 `model` 挑实例，两样分开换就会有一拍
/// 拿新模型名去问旧后端。
#[derive(Clone)]
struct Live {
    enabled: bool,
    /// **记忆模型自己的那份完整配置**（§13.3）：端点、模型、effort 一起来，不拼装。
    model: ModelConfig,
    retrieval: RetrievalConfig,
    consolidate: ConsolidateConfig,
    /// 会话空闲多久之后才领来提取（§9.10）。
    extract_idle: time::Duration,
    /// 一批最多收几条观察（§9.10）。
    max_observations: usize,
    llm: Arc<dyn LlmClient>,
}

/// 一个会话里**这一段对话**注入的那一块。
#[derive(Debug, Clone)]
struct Pinned {
    /// 钉住时的 `Surface::boundary()`（`conversation.boundary` 的位置）。它一变就是新一段。
    boundary: usize,
    injection: Injection,
    /// 最后一次用到它的时刻。表按会话攒，长时间跑着的进程要能收掉早就没人用的那些。
    seen: OffsetDateTime,
}

/// 注入锚表超过这么多会话时，顺手收掉 [`PIN_TTL`] 没碰过的（长跑进程里它会一直攒）。
const PIN_SOFT_LIMIT: usize = 256;
/// 多久没碰过就可以丢：一段对话不会跨这么久还接着答。
const PIN_TTL: time::Duration = time::Duration::hours(6);

/// 一次后台处理最多领几个会话（每个会话一批，§9.10）。
pub const WORK_BATCH: usize = 8;

fn extract_idle(config: &MemoryConfig) -> time::Duration {
    time::Duration::seconds(i64::try_from(config.extract_idle_secs).unwrap_or(i64::MAX))
}

/// 索引构建失败后第一次重试等多久；之后每次翻倍，封顶 [`INDEX_RETRY_MAX`]（§9.5）。
const INDEX_RETRY_FIRST: time::Duration = time::Duration::seconds(30);
const INDEX_RETRY_MAX: time::Duration = time::Duration::minutes(30);

/// 索引调度的账。
#[derive(Debug, Default)]
struct IndexSchedule {
    /// 有一个构建任务在跑（或在退避里等着重跑）。
    running: bool,
    /// 跑着的时候又来了信号：这一轮结束后再来一轮。
    again: bool,
    /// 连着失败了几轮；成功一轮清零。
    failures: u32,
    /// 退避中：下一次自动重试的时刻。
    retry_at: Option<OffsetDateTime>,
}

/// 第 `failures` 次连续失败之后等多久。
fn index_backoff(failures: u32) -> time::Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (INDEX_RETRY_FIRST * 2_i32.pow(doublings)).min(INDEX_RETRY_MAX)
}

impl MemoryManager {
    pub fn new(parts: MemoryParts) -> Self {
        let configured = parts.config.embedding.is_some();
        MemoryManager {
            live: RwLock::new(Live {
                enabled: parts.config.enabled,
                model: parts.config.model.clone(),
                retrieval: parts.config.retrieval.clone(),
                consolidate: parts.config.consolidate.clone(),
                extract_idle: extract_idle(&parts.config),
                max_observations: parts.config.max_observations,
                llm: parts.llm,
            }),
            repo: parts.repo,
            catalog: parts.catalog,
            embeddings: RwLock::new(parts.embeddings),
            reranker: RwLock::new(parts.reranker),
            configured,
            events: parts.events,
            work: parts.work,
            clock: parts.clock,
            render: parts.render,
            pins: Mutex::new(BTreeMap::new()),
            indexing: Mutex::new(IndexSchedule::default()),
            index_wake: tokio::sync::Notify::new(),
            reported: Mutex::new(BTreeSet::new()),
        }
    }

    /// 当前这一份。**取出就放锁**：下面的召回与模型请求都要 await。
    fn live(&self) -> Live {
        self.live.read().expect("记忆配置槽").clone()
    }

    /// 配置热重载的落点（§3）：开关、记忆模型（连同按新快照造的后端）与召回预算一起换。
    /// 正在跑的那一次提取 / 召回已经拿着旧的那一份，下一次才用新的。
    ///
    /// 向量后端不在这里：`memory.embedding` 换了是换向量空间（§9.5），要重建索引。
    pub fn install_config(&self, config: &MemoryConfig, llm: Arc<dyn LlmClient>) {
        *self.live.write().expect("记忆配置槽") = Live {
            enabled: config.enabled,
            model: config.model.clone(),
            retrieval: config.retrieval.clone(),
            consolidate: config.consolidate.clone(),
            extract_idle: extract_idle(config),
            max_observations: config.max_observations,
            llm,
        };
    }

    pub fn enabled(&self) -> bool {
        self.live.read().expect("记忆配置槽").enabled
    }

    pub fn retrieval(&self) -> RetrievalConfig {
        self.live().retrieval
    }

    /// 记忆模型的那一份配置。**`komo model list` 之类要印它，测试要断言它没被聊天模型
    /// 串掉**（§13.3）。
    pub fn model(&self) -> ModelConfig {
        self.live().model
    }

    /// 记忆模型与它的后端，同一拍取出的一对。
    fn backend(&self) -> (ModelConfig, Arc<dyn LlmClient>) {
        let live = self.live();
        (live.model, live.llm)
    }

    pub fn repo(&self) -> &Arc<dyn MemoryRepo> {
        &self.repo
    }

    pub fn catalog(&self) -> &Arc<dyn MemoryCatalog> {
        &self.catalog
    }

    /// 当前这一拍的向量后端。**取出就放锁**：`None` = 还没探到（或探不到），检索按
    /// §9.4 降级。
    fn vector_backend(&self) -> Option<Arc<dyn EmbeddingClient>> {
        self.embeddings.read().expect("向量后端槽").clone()
    }

    /// 装上向量后端。**后台探测的落点**（§9.5）：探测拿到维度才调它，拿不到就一直空着，
    /// 检索按 §9.4 把降级如实报出去。
    pub fn install_embeddings(&self, client: Arc<dyn EmbeddingClient>) {
        *self.embeddings.write().expect("向量后端槽") = Some(client);
    }

    /// 换上（或撤掉）判断后端。配置热重载走这里：`[typesafe]` 或
    /// `memory.retrieval.rerank` 一变，下一次召回就按新的来。
    pub fn install_reranker(&self, reranker: Option<Reranker>) {
        *self.reranker.write().expect("判断后端槽") = reranker;
    }

    /// 这一次查询要不要重排、用哪个后端。**两个条件都要**：开关开着，而且后端在手上。
    fn reranker(&self) -> Option<Reranker> {
        if !self.retrieval().rerank {
            return None;
        }
        self.reranker.read().expect("判断后端槽").clone()
    }

    /// 手上没有向量后端时的错误。**配了 alias = 端点这一刻不可用，没配 = 配置错误**
    /// （§9.4 的两句话分开报，HTTP 侧也分开映射：503 与 422）。
    fn missing_backend_error(&self) -> MemoryError {
        if self.configured {
            MemoryError::VectorUnavailable(
                "向量后端还没就绪：维度探测还在跑，或者上一次探测失败了".into(),
            )
        } else {
            MemoryError::VectorUnconfigured
        }
    }

    pub fn space(&self) -> Option<EmbeddingSpace> {
        self.vector_backend().map(|client| client.space().clone())
    }

    // ------------------------------------------------------------ 检索

    /// 一次检索：向量在**事务外**算好，再交给仓储融合（§9.4、§9.5）。
    ///
    /// 降级规则逐条对着 §9.4 最后一段：
    ///
    /// - 没配置 embedding 却选了 hybrid / vector → [`MemoryError::VectorUnconfigured`]，
    ///   **不静默变成长期关键词模式**。
    /// - 配了但这一刻不通：`hybrid` 走关键词并把 `degraded` / 原因 / 覆盖率带回去；
    ///   `vector` → [`MemoryError::VectorUnavailable`]。
    /// - 两种情况都不能变成"没有相关记忆"。
    pub async fn search(&self, mut query: RecallQuery) -> Result<RecallResult, MemoryError> {
        if !self.enabled() {
            return Err(MemoryError::Disabled);
        }
        if query.mode != RetrievalMode::Keyword {
            match self.vector_backend() {
                Some(client) => match self.embed_query(client.as_ref(), &query.text).await {
                    Ok(Some(vector)) => query.query_vector = Some(vector),
                    Ok(None) => {
                        // 空查询文本没有向量可言；这不是故障，交给关键词臂（它也会返回空）。
                    }
                    Err(error) => {
                        if query.mode == RetrievalMode::Vector {
                            return Err(MemoryError::VectorUnavailable(error.to_string()));
                        }
                        tracing::warn!(%error, "向量端点不通，这一次 hybrid 退化为关键词");
                    }
                },
                // 配了 alias 但后端还没在手上（维度探测在跑，或探测失败）：§9.4 的
                // "配了但这一刻不通"——hybrid 退化成关键词并留下降级说明，vector-only
                // 明确报不可用。**不是配置错误**。
                None if self.configured => {
                    if query.mode == RetrievalMode::Vector {
                        return Err(self.missing_backend_error());
                    }
                    tracing::warn!("向量后端还没就绪，这一次 hybrid 退化为关键词");
                }
                // 没配 alias 却选了 hybrid / vector：**配置错误**，不能静默变成长期
                // 关键词模式（§9.4）。
                None => return Err(MemoryError::VectorUnconfigured),
            }
        }

        // 重排（§9.4，可选）：**短名单要比 `top_k` 宽**——一样宽就只是换个顺序，换不出任何
        // 本来进不了 `top_k` 的条目（配置校验也拦这个组合）。判断后端不在手上时按融合
        // 顺序给，用户什么都不会发现（日志里有一次 warn）。
        let reranker = self.reranker();
        let top_k = query.top_k.max(1);
        let shortlist = reranker.as_ref().map(|_| {
            self.retrieval()
                .rerank_shortlist
                .max(top_k + 1)
                .min(query.candidate_limit.max(1))
        });
        if let Some(shortlist) = shortlist {
            query.top_k = shortlist;
        }

        let mut result = self.repo.recall(&query).await?;
        if query.mode == RetrievalMode::Vector && result.degraded {
            // vector-only **明确报不可用**，不能把故障解释成"没有相关记忆"（§9.4）。
            return Err(MemoryError::VectorUnavailable(
                result
                    .degraded_reason
                    .unwrap_or_else(|| "向量臂这一次没跑成".into()),
            ));
        }

        if let (Some(_), Some(reranker)) = (shortlist, reranker) {
            match rerank::order(
                reranker.backend.as_ref(),
                &reranker.model,
                &query.text,
                &result.items,
            )
            .await
            {
                Ok(Some(order)) => {
                    result.items = rerank::apply_order(result.items, &order);
                    tracing::debug!(candidates = order.len(), "记忆短名单按判断重排过（§9.4）");
                }
                // 它说这批候选都不相关：**不采纳它的排序**，保持融合顺序。候选不因为
                // 一次判断就被丢掉——丢候选是阈值的事，不是这一步的事。
                Ok(None) => {
                    tracing::debug!("判断说这批候选都不相关，保持融合顺序（§9.4）");
                }
                Err(error) => {
                    tracing::warn!(%error, "判断后端这一次没给出排序，保持融合顺序（§9.4）");
                }
            }
            result.items.truncate(top_k as usize);
        }
        Ok(result)
    }

    async fn embed_query(
        &self,
        client: &dyn EmbeddingClient,
        text: &str,
    ) -> Result<Option<Vector>, EmbedError> {
        if text.trim().is_empty() {
            return Ok(None);
        }
        // 查询侧用查询侧的输入规则——同一空间规定的那一套（§9.5）。
        let mut vectors =
            crate::telemetry::embed(client, InputKind::Query, &[text.to_string()]).await?;
        match vectors.pop() {
            Some(vector) if vector.is_usable(client.space().dimensions) => Ok(Some(vector)),
            Some(_) => Err(EmbedError::InvalidVector("查询向量不合法".into())),
            None => Err(EmbedError::InvalidVector("端点没有返回向量".into())),
        }
    }

    /// 按配置的预算造一次检索。
    pub fn query(&self, text: impl Into<String>, mode: Option<RetrievalMode>) -> RecallQuery {
        let retrieval = self.retrieval();
        RecallQuery {
            text: text.into(),
            mode: mode.unwrap_or(retrieval.mode),
            scopes: vec![],
            candidate_limit: retrieval.candidate_limit,
            top_k: retrieval.top_k,
            max_tokens: retrieval.max_tokens,
            now: self.clock.now(),
            query_vector: None,
            include_states: vec![],
        }
    }

    // ------------------------------------------------------------ 注入（§9.4、§9.7）

    /// 这个 Run 这一次注入哪些条目。
    ///
    /// `carried` 是检查点里记下的那一批（§9.7）：**先重新核对**当前状态、有效期与版本，
    /// 活下来的就沿用——"resume 和旧检查点不能重新注入已经遗忘的内容"是这一步的全部理由，
    /// 顺带也满足"正常情况下沿用 Run 的选择，不逐轮重复请求 embedding"（§9.4）。
    ///
    /// `scopes` 是这一次放行的记忆作用域（§9.2）：空 = 不按作用域过滤。**过滤发生在检索
    /// 里**（[`RecallQuery::scopes`]），不是拿到结果之后再遮掉别的 Agent 的记录。
    pub async fn prepare(
        &self,
        run: &RunId,
        text: &str,
        carried: &[MemoryUse],
        scopes: &[MemoryScope],
    ) -> Injection {
        if !self.enabled() {
            return Injection::default();
        }
        let now = self.clock.now();
        let mut items = self.revalidate(carried, now).await;
        // 带过来的旧注入也要落在**现在**放行的作用域里：模式或作用域改过、或者那一条本来
        // 就不该被这个 Agent 看见（别的 Agent 的私有记忆）的，掉出去重新召回。
        items.retain(|item| scope_allowed(scopes, &item.scope));
        let mut diagnostics = RecallDiagnostics::default();

        if items.is_empty() && !text.trim().is_empty() {
            let mut query = self.query(text, None);
            query.scopes = scopes.to_vec();
            diagnostics.mode = Some(query.mode);
            match self.search(query).await {
                Ok(result) => {
                    if result.degraded {
                        tracing::info!(
                            reason = result.degraded_reason.as_deref().unwrap_or(""),
                            "这一次注入只有关键词臂"
                        );
                    }
                    diagnostics.mode = Some(result.mode);
                    diagnostics.degraded = result.degraded;
                    diagnostics.degraded_reason = result.degraded_reason;
                    diagnostics.candidates_before_fusion = result.candidates_before_fusion;
                    diagnostics.candidates_after_fusion = result.candidates_after_fusion;
                    items = result.items;
                }
                Err(error) => {
                    // 召回失败**不拦住这一轮**：没有记忆的回答仍然是回答，而把一次故障
                    // 变成一次 Run 失败不是 §9 要的。它已经在日志里说清了是哪一种。
                    tracing::warn!(%error, run = %run, "这一次召回没跑成，本轮不注入记忆");
                    diagnostics.degraded = true;
                    diagnostics.degraded_reason = Some(error.to_string());
                }
            }
        }

        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let items = stale::mark_missing_paths(items, home.as_deref());
        let mut injection = (self.render)(&items, self.retrieval().max_tokens);
        diagnostics.dropped_by_budget =
            Some(items.len().saturating_sub(injection.uses.len()) as u32);
        injection.diagnostics = Some(diagnostics);
        self.record_usage(&injection, now).await;
        injection
    }

    /// 这一段的注入段（§9.4）。
    ///
    /// **同一段对话里逐字复用**：`boundary` 没变就把上一块原样交回，一次召回、一次
    /// embedding 都不做。变了（`/new`）或者这一段还没有过，才走 [`MemoryManager::prepare`]
    /// 那条正常路径（重新核对带过来的引用 → 召回 → 渲染）。
    ///
    /// 一条都没召回出来时**也钉住**：不钉的话这个会话每一轮都要为"确实没有相关记忆"
    /// 付一次 embedding。
    ///
    /// `recorded` 是这一段在 Session 日志里记下的那一块（`memory.injected`）。进程里的锚
    /// 重启就没了，那时按它逐字复用——同样先核对还算不算数；否则重启之后会按另一句用户
    /// 输入重新召回，换掉整段系统提示（前缀缓存从头失效）。调用方拿到结果与 `recorded`
    /// 不同时负责把新的一块记下来。
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_segment(
        &self,
        session: &SessionId,
        run: &RunId,
        text: &str,
        carried: &[MemoryUse],
        boundary: usize,
        scopes: &[MemoryScope],
        recorded: Option<Injection>,
    ) -> Injection {
        if !self.enabled() {
            return Injection::default();
        }
        // 先取出来再判断：锁不能跨 `await` 拿着（下面的核对与写库都要 await）。
        let now = self.clock.now();
        let pinned = {
            let mut pins = self.pins.lock().expect("注入锚");
            let found = pins
                .get(session)
                .filter(|pinned| pinned.boundary == boundary)
                .map(|pinned| pinned.injection.clone());
            if found.is_some()
                && let Some(pinned) = pins.get_mut(session)
            {
                pinned.seen = now;
            }
            found
        };
        let from_log = pinned.is_none() && recorded.is_some();
        let pinned = pinned.or(recorded);
        // §9.4「每次模型请求前复查被选条目的有效性」+ §9.2「forget 立即停用」：沿用之前
        // **先核对这一块现在还算不算数**。被遗忘/失效的条目还在锚里，就是让遗忘失效——
        // 这一条是验收项（`a_forgotten_memory_never_comes_back_into_a_turn`），不是优化
        // 可以牺牲的东西。
        let pinned = match pinned {
            Some(pinned) if self.pin_still_valid(&pinned, scopes, now).await => Some(pinned),
            Some(pinned) => {
                tracing::info!(
                    %session,
                    boundary,
                    lines = pinned.uses.len(),
                    "这一段的注入锚里有条目已被遗忘/失效，重算这一块"
                );
                None
            }
            None => None,
        };
        if let Some(pinned) = pinned {
            if from_log {
                self.pin(session, boundary, &pinned, now);
                tracing::info!(
                    %session,
                    boundary,
                    lines = pinned.uses.len(),
                    "这一段的注入按 Session 日志里记下的那一块逐字复用"
                );
            }
            // 使用计数照记：这一段里每用一次都算用了一次（§9.2 的度量，不影响正文）。
            self.record_usage(&pinned, now).await;
            tracing::debug!(
                %session,
                boundary,
                lines = pinned.uses.len(),
                "这一段对话沿用上一块注入，不改动提示前缀"
            );
            return Injection {
                diagnostics: Some(RecallDiagnostics {
                    reused_pin: true,
                    ..RecallDiagnostics::default()
                }),
                ..pinned
            };
        }

        let injection = self.prepare(run, text, carried, scopes).await;
        self.pin(session, boundary, &injection, now);
        // 这一段就此定下来：从这里到下一次换段，system 消息逐字不变（前缀缓存的命门）。
        tracing::info!(
            %session,
            boundary,
            lines = injection.uses.len(),
            "这一段对话的注入已定；此后逐字复用，换段（/new）才重算"
        );
        injection
    }

    /// 把这一块钉成这个会话这一段的注入锚。
    fn pin(
        &self,
        session: &SessionId,
        boundary: usize,
        injection: &Injection,
        now: OffsetDateTime,
    ) {
        let mut pins = self.pins.lock().expect("注入锚");
        if pins.len() >= PIN_SOFT_LIMIT {
            pins.retain(|_, pinned| now - pinned.seen < PIN_TTL);
        }
        pins.insert(
            session.clone(),
            Pinned {
                boundary,
                injection: injection.clone(),
                seen: now,
            },
        );
    }

    /// 钉住的那一块**现在还算不算数**：里面的条目都还在、都还能召回。
    ///
    /// 只看"还在不在、还能不能召回"，**不看 revision**：`forget` 必须立刻生效（§9.2、
    /// §9.7），而改写只影响正文文字——那一点让给提示前缀的稳定（正文里带着 `id@revision`，
    /// 读的人知道引用的是哪一版），本段结束后自然换过来。
    ///
    /// 代价是每个 Run 至多 `top_k` 次按 id 的短读；不涉及 embedding，也不涉及向量检索。
    async fn pin_still_valid(
        &self,
        injection: &Injection,
        scopes: &[MemoryScope],
        now: OffsetDateTime,
    ) -> bool {
        for use_ in &injection.uses {
            match self.repo.get(&use_.memory).await {
                Ok(Some(item))
                    if item.is_recallable_at(now) && scope_allowed(scopes, &item.scope) => {}
                Ok(_) => return false,
                Err(error) => {
                    // 读不出来 ≠ 没问题：宁可按失效处理，下一段重算（§8.5 一贯的口径）。
                    tracing::debug!(%error, memory = %use_.memory, "核对注入锚里的条目没读成，这一块作废重算");
                    return false;
                }
            }
        }
        true
    }

    /// 记使用计数（§9.2）：这一块注入里的每条记忆都算用了一次。
    async fn record_usage(&self, injection: &Injection, now: OffsetDateTime) {
        let ids: Vec<MemoryId> = injection
            .uses
            .iter()
            .map(|use_| use_.memory.clone())
            .collect();
        if !ids.is_empty()
            && let Err(error) = self.catalog.record_use(&ids, now).await
        {
            // 使用计数只度量使用，记不上不影响这一轮（§9.2）。
            tracing::debug!(%error, "使用计数没记上");
        }
    }

    /// 重新核对一批记忆引用的**当前**状态（§9.7）。过期、遗忘、改了版本的都掉出去。
    pub async fn revalidate(&self, uses: &[MemoryUse], now: OffsetDateTime) -> Vec<MemoryItem> {
        let mut alive = Vec::new();
        for use_ in uses {
            match self.repo.get(&use_.memory).await {
                Ok(Some(item)) => {
                    if item.revision != use_.revision {
                        tracing::debug!(memory = %use_.memory, "版本变了，这一条不再沿用");
                        continue;
                    }
                    if !item.is_recallable_at(now) {
                        tracing::debug!(memory = %use_.memory, "已经不可召回，这一条不再注入");
                        continue;
                    }
                    alive.push(item);
                }
                Ok(None) => {}
                Err(error) => tracing::debug!(%error, memory = %use_.memory, "核对不了，先不沿用"),
            }
        }
        alive
    }

    // ------------------------------------------------------------ 自动积累（§9.3）

    /// 领走空闲够久的会话，一个会话一批处理。
    ///
    /// 「run.completed 在 JSONL 持久保存后，state.db 提交终态与 memory_work = pending
    /// → **后台领取该 Run 尚未处理的新证据**」（§9.3）——§9.10 把"领取"改成按会话：会话
    /// 没有未到终态的 Run、且最后一个 Run 结束已满 `[memory].extract_idle`，才一次领走它
    /// 全部 pending 的 Run，提取一次。失败的 Run 放回 pending，下次重试；已经写进去的
    /// 记忆正文不回滚——"索引失败独立重试，不撤销已经保存的记忆正文"。
    ///
    /// **只有会好的失败才放回 pending。**证据读不出来是 §8.3 的"停止受影响会话，报告
    /// 损坏"——重试一万次也读不出来，所以标记翻成 `error`（§9.3 的四个状态里就是这一个
    /// 的用处），队列不再为它空转。见 [`MemoryError::is_permanent`]。
    pub async fn process_pending(&self, limit: usize) -> Result<PassReport, MemoryError> {
        let live = self.live();
        if !live.enabled {
            return Ok(PassReport::default());
        }
        let claimed = self.work.claim(live.extract_idle, limit).await?;
        let mut report = PassReport::default();
        for batch in &claimed {
            self.process_batch(batch, &mut report).await;
        }
        Ok(report)
    }

    async fn settle(&self, run: &RunId, work: MemoryWork, cursor: Seq) {
        // 这一条已经不在"报过的失败"里了：下次再失败要重新出声，而不是被旧记录压住。
        if work != MemoryWork::Pending {
            self.reported.lock().expect("失败表").remove(run);
        }
        if let Err(error) = self.work.finish(run, work, cursor).await {
            tracing::warn!(%error, run = %run, "记忆处理标记写不下去");
        }
    }

    /// 一个会话这一批的完整处理，并发一条结构化日志（§9.10「逐会话记录」）。
    async fn process_batch(&self, batch: &MemoryBatch, report: &mut PassReport) {
        let mut record = BatchRecord::default();
        let span = tracing::info_span!(
            "komo.memory.extract",
            komo.session.kind = batch.kind.as_str(),
            komo.memory.runs = batch.items.len() as u64,
            komo.memory.window_chars = tracing::field::Empty,
            komo.memory.skip_reason = tracing::field::Empty,
            komo.memory.returned = tracing::field::Empty,
            komo.memory.applied = tracing::field::Empty,
            komo.memory.used = tracing::field::Empty,
        );
        let result = tracing::Instrument::instrument(
            self.run_batch(batch, report, &mut record),
            span.clone(),
        )
        .await;
        if let Err(error) = result {
            let permanent = error.is_permanent();
            record.skip_reason = Some(format!("失败：{error}"));
            for item in batch
                .items
                .iter()
                .filter(|item| !record.settled.contains(&item.run))
            {
                if permanent {
                    report.abandoned += 1;
                    tracing::warn!(
                        %error,
                        run = %item.run,
                        "证据读不出来，这个 Run 不再重试（标记翻成 error）"
                    );
                    self.settle(&item.run, MemoryWork::Error, item.cursor).await;
                } else {
                    report.failed += 1;
                    // 同一条错误只报一次：下一拍还是它，`warn` 一遍没有新信息。
                    let first = self
                        .reported
                        .lock()
                        .expect("失败表")
                        .insert(item.run.clone());
                    if first {
                        tracing::warn!(%error, run = %item.run, "记忆提取失败，放回 pending 下次重试");
                    } else {
                        tracing::debug!(%error, run = %item.run, "记忆提取仍然失败");
                    }
                    // **失败不推进游标**（§9.3）：放回 pending 就是"下次重试"。
                    self.settle(&item.run, MemoryWork::Pending, item.cursor)
                        .await;
                }
            }
        }
        // 失败原因带错误正文，只进日志；span 与指标用低基数标签。
        span.record("komo.memory.window_chars", record.window_chars as u64);
        span.record("komo.memory.returned", record.observations as u64);
        span.record("komo.memory.applied", record.applied as u64);
        span.record("komo.memory.used", record.used as u64);
        if let Some(reason) = skip_label(record.skip_reason.as_deref()) {
            span.record("komo.memory.skip_reason", reason);
            tracing::event!(
                target: "komo::metrics",
                tracing::Level::DEBUG,
                {
                monotonic_counter.komo.memory.extract.skipped = 1u64,
                komo.memory.skip_reason = reason,
                }
            );
        }
        tracing::info!(
            session = %batch.session,
            session_kind = batch.kind.as_str(),
            runs = batch.items.len(),
            window_chars = record.window_chars,
            skip_reason = record.skip_reason.as_deref().unwrap_or(""),
            observations = record.observations,
            applied = record.applied,
            used = record.used,
            "记忆提取处理了一批"
        );
    }

    async fn skip(
        &self,
        item: &MemoryWorkItem,
        cursor: Seq,
        reason: &str,
        report: &mut PassReport,
        record: &mut BatchRecord,
    ) {
        report.skipped += 1;
        tracing::debug!(run = %item.run, %reason, "这个 Run 没有可提取的证据");
        record.settled.insert(item.run.clone());
        record.skip_reason.get_or_insert_with(|| reason.to_string());
        self.settle(&item.run, MemoryWork::Done, cursor).await;
    }

    /// 读日志 → 确定性跳过 → 按预算切段 → 每段一次提取 → 校验 → 去重 / 冲突 → 写库。
    /// 处理完的 Run 当场记 `done` 与游标；出错时调用方把没处理完的放回去。
    async fn run_batch(
        &self,
        batch: &MemoryBatch,
        report: &mut PassReport,
        record: &mut BatchRecord,
    ) -> Result<(), MemoryError> {
        use komo_kernel::types::plan::PlanSource;
        use komo_kernel::types::status::RunState;
        use komo_store::models::SessionKind;

        // 「取消、失败或结果未知的动作不能整理成成功经验」（§9.3）——取消的整个跳过：
        // 它停在用户的选择上，剩下的半截不是证据。
        let mut items: Vec<&MemoryWorkItem> = Vec::new();
        for item in &batch.items {
            if item.status == RunState::Cancelled {
                self.skip(item, item.cursor, "用户取消的任务", report, record)
                    .await;
            } else {
                items.push(item);
            }
        }
        if items.is_empty() {
            return Ok(());
        }

        let events = self.events.events_of(&batch.session).await?;
        let end_of = |item: &MemoryWorkItem| {
            events
                .iter()
                .filter(|event| event.run.as_ref() == Some(&item.run))
                .map(|event| event.seq)
                .fold(item.cursor, Seq::max)
        };

        // 不调模型就跳过（§9.10）：后台任务会话里排查出来的东西只是任务的过程，home 收到
        // 的是摘要，摘要不是证据；Cron 的 Run 是自己的报告，不是用户说的；`memory = "off"`
        // 的 Agent 受理时就说了不要记忆。
        //
        // 记忆模式取**受理时冻结的那一份**（`RunSnapshot::memory_mode`）：同一批里的 Run
        // 可能各自不同，所以按 Run 分——`off` 的当场跳过，`private` 的按 `agent:<id>` 分组
        // （一组锁在一个作用域里提取），`shared` 的一组、模型自己选作用域。每个 Run 都只在
        // 自己那一档的限制下被提取，不存在"取一档最严的套在全批上"的失配。
        let mut groups: BTreeMap<Option<MemoryScope>, Vec<extract::RunWindow>> = BTreeMap::new();
        for item in items {
            let (mode, agent_id) = item
                .frozen
                .clone()
                .unwrap_or_else(|| frozen_memory(&events, &item.run));
            let reason = if batch.kind == SessionKind::Task {
                Some("后台任务会话")
            } else if mode == MemoryMode::Off {
                Some("Agent 关了记忆")
            } else if matches!(item.source, PlanSource::Cron { .. }) {
                Some("Cron 触发的 Run")
            } else {
                None
            };
            match reason {
                Some(reason) => {
                    self.skip(item, end_of(item), reason, report, record).await;
                }
                None => groups
                    .entry(match mode {
                        MemoryMode::Private => Some(MemoryScope::Agent { agent_id }),
                        _ => None,
                    })
                    .or_default()
                    .push(extract::RunWindow {
                        run: item.run.clone(),
                        status: item.status,
                        cursor: item.cursor,
                    }),
            }
        }
        if groups.is_empty() {
            return Ok(());
        }

        let live = self.live();
        let pass = extract::LearningPass::new(
            live.llm,
            live.model.clone(),
            Arc::clone(&self.clock),
            live.max_observations,
        );
        for (confined, windows) in groups {
            for segment in extract::split_batch(&batch.session, &windows, &events) {
                let mut transcript = extract::Transcript::build(&batch.session, &segment, &events);
                if let Some(scope) = &confined {
                    transcript = transcript.confined_to(scope.clone());
                }
                record.window_chars += transcript.window_chars();
                let mut applied = 0;
                let mut skipped = None;
                if !transcript.has_new_evidence() {
                    skipped = Some("没有新的用户陈述或工具结果");
                } else {
                    let consolidator = consolidate::Consolidator::new(self);
                    // 提取前整批召回一次，编号后随提取调用一起交给模型（§9.10）。
                    let scopes: Vec<MemoryScope> = confined.iter().cloned().collect();
                    let mut candidates =
                        consolidator.recall(transcript.statements(), &scopes).await;
                    // 这段对话里已经手动记过的（§9.3「手动记一条」）：不一定召回得到，却最容易
                    // 被再提一遍。补进编号表并标出来，apply 时再兜一道。
                    let mut by_hand = self.recorded_by_hand(&transcript).await;
                    by_hand
                        .retain(|item| confined.as_ref().is_none_or(|scope| &item.scope == scope));
                    for item in &by_hand {
                        if !candidates.iter().any(|have| have.id == item.id) {
                            candidates.push(item.clone());
                        }
                    }
                    let consolidator = consolidator.with_recorded(by_hand);
                    // 本批注入的记忆：只列版本没变的——正文变了，模型看到的就不是注入的那句话。
                    let mut injected = Vec::new();
                    for use_ in transcript.injected() {
                        if let Ok(Some(item)) = self.repo.get(&use_.memory).await
                            && item.revision == use_.revision
                            && scope_allowed(&scopes, &item.scope)
                        {
                            injected.push(item);
                        }
                    }
                    let raw = pass.extract(&transcript, &candidates, &injected).await?;
                    record.observations += raw.observations.len();
                    let adopted = transcript.validate_used(raw.used, &injected);
                    let observations = transcript.validate(
                        raw.observations,
                        live.max_observations,
                        &live.model,
                        &candidates,
                        self.clock.now(),
                    );
                    if observations.is_empty() {
                        skipped = Some("提取结果一条都没通过校验");
                    } else {
                        applied = consolidator.apply(&observations).await?;
                        record.applied += applied;
                    }
                    // 采纳计数放在 apply 之后、游标推进之前：apply 失败整批重试时还没记过，
                    // 不会重复计数；游标推进后这一批不再被领取。
                    if !adopted.is_empty() {
                        if let Err(error) = self
                            .catalog
                            .record_adoption(&adopted, self.clock.now())
                            .await
                        {
                            tracing::warn!(%error, "采纳计数没记上");
                        } else {
                            record.used += adopted.len();
                            report.used += adopted.len();
                            tracing::event!(
                                target: "komo::metrics",
                                tracing::Level::DEBUG,
                                {
                                monotonic_counter.komo.memory.used = adopted.len() as u64,
                                komo.session.kind = batch.kind.as_str(),
                                }
                            );
                        }
                    }
                }
                report.applied += applied;
                for (run, cursor) in transcript.run_cursors() {
                    record.settled.insert(run.clone());
                    if let Some(reason) = skipped {
                        report.skipped += 1;
                        record.skip_reason.get_or_insert_with(|| reason.to_string());
                        tracing::debug!(run = %run, %reason, "这个 Run 没有可提取的证据");
                    } else {
                        report.processed += 1;
                    }
                    self.settle(run, MemoryWork::Done, cursor).await;
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ 操作者路径（§9.6）

    /// 操作者确认某个版本。**只有这条路能把确认等级抬起来**——模型返回的 `user_confirmed`
    /// 字段在 [`extract`] 里根本没有被读进来（§9.2）。
    pub async fn confirm(
        &self,
        id: &MemoryId,
        expected_revision: u32,
    ) -> Result<MemoryItem, MemoryError> {
        Ok(self
            .repo
            .confirm(id, expected_revision, self.clock.now())
            .await?)
    }

    /// 遗忘：停用正文，并使关键词与向量失效（§9.6）。
    ///
    /// 两样都在仓储里（`forget` 删词条与向量行）。会话级的注入锚不在这里特殊处理：
    /// [`Self::pin_still_valid`] 在下一次 `prepare_segment` 时重新核对每个 id，遗忘的
    /// 条目读不出来就让整块作废重算——这一条是验收项
    /// （`a_forgotten_memory_never_comes_back_into_a_turn`）。检查点本身不改：它是历史
    /// 审计，而 [`Self::revalidate`] 保证它复活不了（§9.7）。
    pub async fn forget(
        &self,
        id: &MemoryId,
        expected_revision: u32,
    ) -> Result<MemoryItem, MemoryError> {
        Ok(self
            .repo
            .forget(id, expected_revision, self.clock.now())
            .await?)
    }

    /// 撤回一次遗忘或争议：状态回到 active，版本号不动（forget 也不动它）。
    ///
    /// 走 `put` 是因为它会把关键词索引跟着正文重写；向量在 forget 时删了，这里不同步补，
    /// 交给增量索引（调用方随后提交一次 rebuild，同代次只补缺的那些）。
    pub async fn restore(
        &self,
        id: &MemoryId,
        expected_revision: u32,
    ) -> Result<MemoryItem, MemoryError> {
        let mut item = self
            .repo
            .get(id)
            .await?
            .ok_or_else(|| RepoError::NotFound {
                what: format!("记忆 {id}"),
            })?;
        if !matches!(item.state, MemoryState::Forgotten | MemoryState::Contested) {
            return Err(MemoryError::Invalid(format!(
                "记忆 {id} 现在是 {:?}，只有 forgotten / contested 能恢复",
                item.state
            )));
        }
        item.state = MemoryState::Active;
        item.updated_at = self.clock.now();
        Ok(self.repo.put(item, Some(expected_revision)).await?)
    }

    /// 按规则遗忘（[`sweep`]）。`apply = false` 只给出会做什么。
    ///
    /// 逐条落库，一条失败（版本冲突：这期间被抽取改写了）记进 `errors`，不拦住其余的；
    /// 返回的 `actions` 在应用时只含真正做成了的那些。
    pub async fn sweep(
        &self,
        request: &MemorySweepRequest,
    ) -> Result<MemorySweepResponse, MemoryError> {
        let now = self.clock.now();
        let mut items = self
            .catalog
            .list(None, Some(MemoryState::Active), usize::MAX)
            .await?;
        items.extend(
            self.catalog
                .list(None, Some(MemoryState::Candidate), usize::MAX)
                .await?,
        );
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let policy = sweep::SweepPolicy::from_days(request.never_used_days, request.idle_days);
        let planned = sweep::plan(&items, now, policy, home.as_deref());
        if !request.apply {
            return Ok(MemorySweepResponse {
                applied: false,
                scanned: items.len() as u64,
                actions: planned,
                errors: vec![],
            });
        }

        let mut actions = Vec::new();
        let mut errors = Vec::new();
        for action in planned {
            let outcome = match action.to {
                MemoryState::Contested => self.contest(&action.memory, action.revision, now).await,
                _ => self
                    .repo
                    .forget(&action.memory, action.revision, now)
                    .await
                    .map(|_| ())
                    .map_err(MemoryError::from),
            };
            match outcome {
                Ok(()) => {
                    tracing::info!(
                        memory = %action.memory,
                        rule = ?action.rule,
                        to = ?action.to,
                        detail = %action.detail,
                        "sweep 处理了一条记忆"
                    );
                    actions.push(action);
                }
                Err(error) => errors.push(format!("{}：{error}", action.memory)),
            }
        }
        Ok(MemorySweepResponse {
            applied: true,
            scanned: items.len() as u64,
            actions,
            errors,
        })
    }

    /// 暂停召回、等人定夺（§9.6 的 contested）。版本号不动，与 forget 一致。
    async fn contest(
        &self,
        id: &MemoryId,
        expected_revision: u32,
        at: OffsetDateTime,
    ) -> Result<(), MemoryError> {
        let mut item = self
            .repo
            .get(id)
            .await?
            .ok_or_else(|| RepoError::NotFound {
                what: format!("记忆 {id}"),
            })?;
        item.state = MemoryState::Contested;
        item.updated_at = at;
        self.repo.put(item, Some(expected_revision)).await?;
        Ok(())
    }

    // ------------------------------------------------------------ 索引（§9.5）

    /// 当前索引状态，外加**配置漂移**、首代进度与调度的说明。
    ///
    /// 「更换向量模型、维度或预处理方式时…创建新索引代次」（§9.5）：配置的空间与生效
    /// 代次对不上时，后台调度会把新空间那一代建起来、追平后切换，这里把漂移说出来。
    pub async fn index_status(&self) -> Result<MemoryIndexStatus, MemoryError> {
        let mut status = self.repo.index_status().await?;
        let Some(space) = self.space() else {
            if self.configured {
                // 配了 alias、后端还没装上：等后端，不是"没配置"（§9.4）。
                status.errors.push(
                    "配了 `memory.embedding`，但向量后端还没就绪（启动探测还在跑，或者失败了，\
                     见 Gateway 日志）：先只有关键词臂，就绪后自动补齐索引"
                        .into(),
                );
            }
            return Ok(status);
        };
        let configured = IndexBuilder::generation_id(&space);
        match &status.generation {
            Some(active) if active == &configured => {}
            Some(active) => status.errors.push(format!(
                "配置的向量空间（{configured}）与生效代次（{active}）不是同一个：后台会把新代次\
                 建起来、追平后切换；要立刻开始就跑 `komo memory index rebuild`"
            )),
            None => {
                // 首代还没生效：报这一代的真实进度，而不是生效代次的 0%。
                let record = self
                    .catalog
                    .generations()
                    .await?
                    .into_iter()
                    .find(|row| row.id == configured);
                status.space = Some(space);
                status.indexed = self.catalog.indexed_in(&configured).await?;
                status.coverage = if status.total == 0 {
                    0.0
                } else {
                    status.indexed as f32 / status.total as f32
                };
                status.state = record
                    .as_ref()
                    .map(|row| row.state)
                    .unwrap_or(IndexState::Building);
                status.errors.push(format!(
                    "还没有生效代次；配置的空间是 {configured}，后台在补齐（`komo memory index \
                     rebuild` 可以立刻开始）"
                ));
                if let Some(row) = record {
                    status.errors.extend(row.errors);
                }
            }
        }
        let retry_at = self.indexing.lock().expect("索引调度").retry_at;
        if let Some(at) = retry_at {
            status.errors.push(format!(
                "上一轮索引构建失败，{at} 自动重试；`komo memory index rebuild` 立刻重试"
            ));
        }
        Ok(status)
    }

    /// 提交一次索引补齐（§9.5「索引自动补齐」）。自动信号走这里：向量后端就绪、后台
    /// 提取写进了变更、恢复了一条记忆。
    ///
    /// 同一时刻最多一个构建任务：已经有一个在跑（或在退避里等）时只记一笔"这一轮结束
    /// 再来一轮"，返回 `accepted = false` 和同一个代次号——「同代次进行中返回同一个任务」。
    pub fn schedule_index(self: &Arc<Self>) -> Result<(String, bool), MemoryError> {
        let Some(space) = self.space() else {
            return Err(MemoryError::VectorUnconfigured);
        };
        let generation = IndexBuilder::generation_id(&space);
        {
            let mut schedule = self.indexing.lock().expect("索引调度");
            if schedule.running {
                schedule.again = true;
                return Ok((generation, false));
            }
            schedule.running = true;
        }
        let manager = Arc::clone(self);
        tokio::spawn(async move { manager.index_loop().await });
        Ok((generation, true))
    }

    /// 操作者的 `komo memory index rebuild`（§9.5、§13.1）：同 [`Self::schedule_index`]，
    /// 另外叫醒退避中的任务——人在等结果，不该让他等满退避。
    ///
    /// `notify_one` 而不是 `notify_waiters`：任务刚记下退避、还没进 `select` 的那一瞬间
    /// 叫它，后者会丢；前者留一张许可。代价是没人在等时那张许可留到下一次退避，让那次
    /// 立刻重跑一轮——多跑一轮无害。
    pub fn request_rebuild(self: &Arc<Self>) -> Result<(String, bool), MemoryError> {
        let submitted = self.schedule_index()?;
        self.index_wake.notify_one();
        Ok(submitted)
    }

    /// 构建任务本身：跑一轮；跑的时候又来了信号就再来一轮；失败就退避后重跑。
    ///
    /// 退避期间到来的信号不提前叫醒它（端点不通时新记忆不该变成紧密重试），手动重建叫得醒。
    async fn index_loop(self: Arc<Self>) {
        loop {
            let outcome = self.build_index().await;
            let failure = match &outcome {
                Ok(outcome) if outcome.errors.is_empty() => None,
                Ok(outcome) => Some(outcome.errors.join("；")),
                Err(error) => Some(error.to_string()),
            };
            // 去留在**同一次加锁**里定：放开锁再看 `running`，中间可能已经有人起了新任务。
            let (delay, failures, again) = {
                let mut schedule = self.indexing.lock().expect("索引调度");
                match failure {
                    None => {
                        schedule.failures = 0;
                        schedule.retry_at = None;
                        let again = std::mem::take(&mut schedule.again);
                        if !again {
                            schedule.running = false;
                        }
                        (None, 0, again)
                    }
                    Some(_) => {
                        schedule.failures += 1;
                        // 重跑本来就会补上这期间来的那些。
                        schedule.again = false;
                        let delay = index_backoff(schedule.failures);
                        schedule.retry_at = Some(self.clock.now() + delay);
                        (Some(delay), schedule.failures, false)
                    }
                }
            };
            match (&outcome, &failure) {
                (Ok(outcome), None) => tracing::info!(
                    generation = %outcome.generation,
                    indexed = outcome.indexed,
                    total = outcome.total,
                    activated = outcome.activated,
                    "索引补齐这一轮结束"
                ),
                // 同一串失败只在第一次出声：端点挂着的时候每次退避都 `warn` 没有新信息。
                (_, Some(failure)) if failures == 1 => {
                    tracing::warn!(%failure, ?delay, "索引补齐失败，退避后重试")
                }
                (_, Some(failure)) => {
                    tracing::debug!(%failure, failures, ?delay, "索引补齐仍然失败")
                }
                _ => {}
            }
            let Some(delay) = delay else {
                if again {
                    continue;
                }
                return;
            };
            let wait = std::time::Duration::try_from(delay).unwrap_or_default();
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.index_wake.notified() => {}
            }
            self.indexing.lock().expect("索引调度").retry_at = None;
        }
    }

    /// 同步地跑一轮索引构建（测试与启动补齐走它）。
    pub async fn build_index(&self) -> Result<IndexOutcome, MemoryError> {
        let Some(client) = self.vector_backend() else {
            return Err(self.missing_backend_error());
        };
        IndexBuilder::new(Arc::clone(&self.repo), Arc::clone(&self.catalog), client)
            .run()
            .await
    }
}

/// 跳过原因的低基数标签：`skip` 记下的是中文原句，"失败：…"还带着错误正文。
fn skip_label(reason: Option<&str>) -> Option<&'static str> {
    let reason = reason?;
    Some(if reason.starts_with("失败") {
        "failed"
    } else if reason == "用户取消的任务" {
        "cancelled"
    } else if reason == "后台任务会话" {
        "task_session"
    } else if reason == "Agent 关了记忆" {
        "memory_off"
    } else if reason == "Cron 触发的 Run" {
        "cron"
    } else if reason == "提取结果一条都没通过校验" {
        "invalid"
    } else {
        "no_evidence"
    })
}

/// 这个 Run 受理时冻结的记忆模式与 Agent（`run.accepted` 的快照）。子 Run 没有自己的
/// 快照时用派它的那条 Run 的；都没有（旧行）= `Shared`，与改造之前一样。
fn frozen_memory(events: &[Event], run: &RunId) -> (MemoryMode, String) {
    let accepted = |run: &RunId| {
        events.iter().find_map(|event| match &event.payload {
            EventPayload::RunAccepted(body) if event.run.as_ref() == Some(run) => Some(body),
            _ => None,
        })
    };
    let Some(body) = accepted(run) else {
        return (MemoryMode::Shared, String::new());
    };
    let snapshot = body.snapshot.as_deref().or_else(|| {
        let parent = &body.delegate.as_ref()?.parent;
        accepted(parent)?.snapshot.as_deref()
    });
    match snapshot {
        Some(snapshot) => (snapshot.memory_mode, snapshot.agent_id.clone()),
        None => (MemoryMode::Shared, String::new()),
    }
}

/// 一批处理里攒下来的账：已经落定的 Run，以及发进逐批日志的那几个数（§9.10）。
#[derive(Debug, Default)]
struct BatchRecord {
    settled: BTreeSet<RunId>,
    window_chars: usize,
    skip_reason: Option<String>,
    observations: usize,
    applied: usize,
    used: usize,
}

#[cfg(test)]
mod tests;
