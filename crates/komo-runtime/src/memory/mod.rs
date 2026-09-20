//! MemoryManager：自动积累、混合检索、向量索引与代次（§9）。
//!
//! 「MemoryManager 对 AgentRuntime 提供三个主要入口：recall、process_completed_run、
//! apply_user_decision。提取、去重、证据校验、冲突处理与索引协调隐藏在该模块内部；
//! **它不向 LLM 注册额外工具**」（§9.3）——所以这个模块没有 `Tool` 实现，一个都没有。
//! 记忆进上下文的路只有一条：[`preamble::MemoryPreamble`] 把召回结果渲染成系统提示后
//! 面的一段**数据**，而 §9.7 那句"不能成为系统指令、Policy 授权或自我更新工具的依据"
//! 就写在那段正文的抬头里。
//!
//! 模块之间：
//!
//! ```text
//! MemoryManager ─┬─ extract::LearningPass   一个已完成 Run → 结构化观察（记忆模型）
//!                ├─ consolidate::Consolidator  去重 / 冲突 → 库里的变更
//!                ├─ index::IndexBuilder     空间指纹 → 代次 → 批量向量 → 事务切换
//!                └─ preamble::MemoryPreamble   召回结果 → 注入段 + 审计用的 MemoryUse
//! ```
//!
//! **模型与仓储之间隔着这一层，是为了守住三句话**：模型返回的 `user_confirmed` 字段没
//! 有写入权限（§9.2）；新推断不能覆盖用户陈述（§9.6）；模型调用期间不持有数据库事务
//! （§9.5）。前两句在 [`consolidate`]，第三句在 [`index`] 与 [`MemoryManager::search`]
//! ——向量在事务外算好，交给仓储的是一个 [`RecallQuery::query_vector`]。

mod consolidate;
mod extract;
mod index;
mod preamble;
mod work;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use komo_kernel::protocol::config::{MemoryConfig, RetrievalConfig};
use komo_kernel::protocol::http::{IndexState, MemoryIndexStatus};
use komo_kernel::traits::{
    Clock, EmbedError, EmbeddingClient, Ledger, LedgerError, LlmClient, MemoryRepo, RepoError,
    StoreError,
};
use komo_kernel::types::ids::{MemoryId, RunId, Seq, SessionId};
use komo_kernel::types::memory::{
    MemoryItem, MemoryScope, MemoryState, MemoryWork, RecallQuery, RecallResult, RetrievalMode,
};
use komo_kernel::types::model::{EmbeddingSpace, InputKind, ModelConfig, Vector};
use komo_kernel::types::turn::{LlmError, MemoryUse};
use komo_store::repos::memory::{GenerationRecord, IndexCandidate};
use time::OffsetDateTime;

pub use consolidate::{Relation, Verdict};
pub use extract::Transcript;
pub use extract::{Observation, PROMPT_VERSION, RawObservation, SaidBy};
pub use index::{IndexBuilder, IndexOutcome, MIN_ACTIVATION_COVERAGE};
pub use preamble::{Injection, MemoryPreamble, render_injection};
pub use work::{DbMemoryWork, MemoryWorkItem, MemoryWorkLog};

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

impl From<LedgerError> for MemoryError {
    fn from(error: LedgerError) -> Self {
        MemoryError::Ledger(error.to_string())
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
}

/// 一个 Session 的事件从哪里来。
///
/// 它比 [`Ledger`] 窄得多，只为一件事：提取只**读**，而一个只读的依赖不该把整个账本
/// 接口拖进测试里。生产实现是 [`LedgerEvents`]，测试给一个装好事件的表。
#[async_trait]
pub trait SessionEvents: Send + Sync {
    async fn events_of(
        &self,
        session: &SessionId,
    ) -> Result<Vec<komo_kernel::events::Event>, MemoryError>;
}

/// 账本上的那一个：分页读到头。
pub struct LedgerEvents(pub Arc<dyn Ledger>);

impl std::fmt::Debug for LedgerEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerEvents").finish_non_exhaustive()
    }
}

#[async_trait]
impl SessionEvents for LedgerEvents {
    async fn events_of(
        &self,
        session: &SessionId,
    ) -> Result<Vec<komo_kernel::events::Event>, MemoryError> {
        let mut all = Vec::new();
        let mut from = Seq::ZERO;
        loop {
            let batch = self.0.read(session, from, 0).await?;
            if batch.events.is_empty() {
                return Ok(all);
            }
            for event in batch.events {
                from = from.max(event.seq);
                all.push(event);
            }
            if batch.next.is_none() {
                return Ok(all);
            }
        }
    }
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
    /// 失败并放回 `pending` 的。
    pub failed: usize,
    /// 失败且**不再重试**的（证据读不出来，标记翻成 `error`）。
    pub abandoned: usize,
}

/// 装配一台 MemoryManager 要的东西。
pub struct MemoryParts {
    pub config: MemoryConfig,
    pub repo: Arc<dyn MemoryRepo>,
    pub catalog: Arc<dyn MemoryCatalog>,
    /// `None` = 没有配置 `memory.embedding` alias，只有关键词臂。
    pub embeddings: Option<Arc<dyn EmbeddingClient>>,
    pub llm: Arc<dyn LlmClient>,
    pub events: Arc<dyn SessionEvents>,
    pub work: Arc<dyn MemoryWorkLog>,
    pub clock: Arc<dyn Clock>,
}

/// §9 的那一层。
pub struct MemoryManager {
    enabled: bool,
    /// **记忆模型自己的那份完整配置**（§13.3）：端点、模型、effort 一起来，不拼装。
    model: ModelConfig,
    retrieval: RetrievalConfig,
    repo: Arc<dyn MemoryRepo>,
    catalog: Arc<dyn MemoryCatalog>,
    /// 向量后端。**可以后装**：Gateway 先把服务起来，再在后台探维度（§9.5「省略
    /// `dimensions` 时先探一次」）——那一次探测是一个网络往返，挂在启动路径上就是让
    /// 「Gateway 就绪」等网络（本机实测一个只收不回话的端点 = 等满模型超时）。探到就
    /// [`MemoryManager::install_embeddings`]，探不到就一直空着，检索按 §9.4 如实降级。
    embeddings: RwLock<Option<Arc<dyn EmbeddingClient>>>,
    /// 配了 `memory.embedding` alias 吗。**与"后端在不在手上"是两件事**：配了但探测还没
    /// 落定（或探测失败）时，§9.4 的规则是"hybrid 退化成关键词、vector-only 报不可用"，
    /// 不是"配置错误"。
    configured: bool,
    llm: Arc<dyn LlmClient>,
    events: Arc<dyn SessionEvents>,
    work: Arc<dyn MemoryWorkLog>,
    clock: Arc<dyn Clock>,
    /// 每个 Run 这一次选了哪些条目。「正常情况下沿用 Run 的选择，不逐轮重复请求
    /// embedding」（§9.4）——`SystemPreamble` 是同步的，正文也从这里取。
    selections: Mutex<BTreeMap<RunId, Injection>>,
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
    /// 正在重建的代次。`POST /v1/memory-index/rebuild` 的幂等就是它。
    building: Mutex<BTreeSet<String>>,
    /// 已经报过一次失败的 Run。**只为不刷日志**：一分钟后还失败是同一件事，每拍都
    /// `warn` 一遍只会把别人的日志挤下去。成功的那些在这里被划掉，下次再失败照样报。
    reported: Mutex<BTreeSet<RunId>>,
}

impl std::fmt::Debug for MemoryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryManager")
            .field("enabled", &self.enabled)
            .field("model", &self.model.model)
            .field("has_embeddings", &self.vector_backend().is_some())
            .finish()
    }
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

/// 一次后台处理最多领几个 Run。
pub const WORK_BATCH: usize = 8;

impl MemoryManager {
    pub fn new(parts: MemoryParts) -> Self {
        let configured = parts.config.embedding.is_some();
        MemoryManager {
            enabled: parts.config.enabled,
            model: parts.config.model.clone(),
            retrieval: parts.config.retrieval.clone(),
            repo: parts.repo,
            catalog: parts.catalog,
            embeddings: RwLock::new(parts.embeddings),
            configured,
            llm: parts.llm,
            events: parts.events,
            work: parts.work,
            clock: parts.clock,
            selections: Mutex::new(BTreeMap::new()),
            pins: Mutex::new(BTreeMap::new()),
            building: Mutex::new(BTreeSet::new()),
            reported: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn retrieval(&self) -> &RetrievalConfig {
        &self.retrieval
    }

    /// 记忆模型的那一份配置。**`komo model list` 之类要印它，测试要断言它没被聊天模型
    /// 串掉**（§13.3）。
    pub fn model(&self) -> &ModelConfig {
        &self.model
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
        if !self.enabled {
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

        let result = self.repo.recall(&query).await?;
        if query.mode == RetrievalMode::Vector && result.degraded {
            // vector-only **明确报不可用**，不能把故障解释成"没有相关记忆"（§9.4）。
            return Err(MemoryError::VectorUnavailable(
                result
                    .degraded_reason
                    .unwrap_or_else(|| "向量臂这一次没跑成".into()),
            ));
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
        let mut vectors = client.embed(InputKind::Query, &[text.to_string()]).await?;
        match vectors.pop() {
            Some(vector) if vector.is_usable(client.space().dimensions) => Ok(Some(vector)),
            Some(_) => Err(EmbedError::InvalidVector("查询向量不合法".into())),
            None => Err(EmbedError::InvalidVector("端点没有返回向量".into())),
        }
    }

    /// 按配置的预算造一次检索。
    pub fn query(&self, text: impl Into<String>, mode: Option<RetrievalMode>) -> RecallQuery {
        RecallQuery {
            text: text.into(),
            mode: mode.unwrap_or(self.retrieval.mode),
            scopes: vec![],
            candidate_limit: self.retrieval.candidate_limit,
            top_k: self.retrieval.top_k,
            max_tokens: self.retrieval.max_tokens,
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
    pub async fn prepare(&self, run: &RunId, text: &str, carried: &[MemoryUse]) -> Injection {
        if !self.enabled {
            return Injection::default();
        }
        let now = self.clock.now();
        let mut items = self.revalidate(carried, now).await;

        if items.is_empty() && !text.trim().is_empty() {
            let query = self.query(text, None);
            match self.search(query).await {
                Ok(result) => {
                    if result.degraded {
                        tracing::info!(
                            reason = result.degraded_reason.as_deref().unwrap_or(""),
                            "这一次注入只有关键词臂"
                        );
                    }
                    items = result.items;
                }
                Err(error) => {
                    // 召回失败**不拦住这一轮**：没有记忆的回答仍然是回答，而把一次故障
                    // 变成一次 Run 失败不是 §9 要的。它已经在日志里说清了是哪一种。
                    tracing::warn!(%error, run = %run, "这一次召回没跑成，本轮不注入记忆");
                }
            }
        }

        let injection = render_injection(&items, self.retrieval.max_tokens);
        self.settle_selection(run, &injection, now).await;
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
    pub async fn prepare_segment(
        &self,
        session: &SessionId,
        run: &RunId,
        text: &str,
        carried: &[MemoryUse],
        boundary: usize,
    ) -> Injection {
        if !self.enabled {
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
        // §9.4「每次模型请求前复查被选条目的有效性」+ §9.2「forget 立即停用」：沿用之前
        // **先核对这一块现在还算不算数**。被遗忘/失效的条目还在锚里，就是让遗忘失效——
        // 这一条是验收项（`a_forgotten_memory_never_comes_back_into_a_turn`），不是优化
        // 可以牺牲的东西。
        let pinned = match pinned {
            Some(pinned) if self.pin_still_valid(&pinned, now).await => Some(pinned),
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
            // 使用计数照记：这一段里每用一次都算用了一次（§9.2 的度量，不影响正文）。
            self.settle_selection(run, &pinned, now).await;
            tracing::debug!(
                %session,
                boundary,
                lines = pinned.uses.len(),
                "这一段对话沿用上一块注入，不改动提示前缀"
            );
            return pinned;
        }

        let injection = self.prepare(run, text, carried).await;
        {
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
        // 这一段就此定下来：从这里到下一次换段，system 消息逐字不变（前缀缓存的命门）。
        tracing::info!(
            %session,
            boundary,
            lines = injection.uses.len(),
            "这一段对话的注入已定；此后逐字复用，换段（/new）才重算"
        );
        injection
    }

    /// 钉住的那一块**现在还算不算数**：里面的条目都还在、都还能召回。
    ///
    /// 只看"还在不在、还能不能召回"，**不看 revision**：`forget` 必须立刻生效（§9.2、
    /// §9.7），而改写只影响正文文字——那一点让给提示前缀的稳定（正文里带着 `id@revision`，
    /// 读的人知道引用的是哪一版），本段结束后自然换过来。
    ///
    /// 代价是每个 Run 至多 `top_k` 次按 id 的短读；不涉及 embedding，也不涉及向量检索。
    async fn pin_still_valid(&self, injection: &Injection, now: OffsetDateTime) -> bool {
        for use_ in &injection.uses {
            match self.repo.get(&use_.memory).await {
                Ok(Some(item)) if item.is_recallable_at(now) => {}
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

    /// 记使用计数 + 把这一块挂到这个 Run 上（`SystemPreamble` 从那里取正文）。
    async fn settle_selection(&self, run: &RunId, injection: &Injection, now: OffsetDateTime) {
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
        self.selections
            .lock()
            .expect("注入表")
            .insert(run.clone(), injection.clone());
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

    /// `SystemPreamble` 取正文的那一口（同步）。
    pub fn injection_for(&self, run: &RunId) -> Option<String> {
        self.selections
            .lock()
            .expect("注入表")
            .get(run)
            .and_then(|injection| injection.text.clone())
    }

    /// Run 结束后把它的选择丢掉——这张表是本进程的缓存，不是账本。
    pub fn forget_run(&self, run: &RunId) {
        self.selections.lock().expect("注入表").remove(run);
    }

    // ------------------------------------------------------------ 自动积累（§9.3）

    /// 领一批待处理的 Run 并逐个处理。
    ///
    /// 「run.completed 在 JSONL 持久保存后，state.db 提交终态与 memory_work = pending
    /// → **后台领取该 Run 尚未处理的新证据**」（§9.3）。失败的放回 pending，下次重试；
    /// 已经写进去的记忆正文不回滚——"索引失败独立重试，不撤销已经保存的记忆正文"。
    ///
    /// **只有会好的失败才放回 pending。**证据读不出来是 §8.3 的"停止受影响会话，报告
    /// 损坏"——重试一万次也读不出来，所以标记翻成 `error`（§9.3 的四个状态里就是这一个
    /// 的用处），队列不再为它空转。见 [`MemoryError::is_permanent`]。
    pub async fn process_pending(&self, limit: usize) -> Result<PassReport, MemoryError> {
        if !self.enabled {
            return Ok(PassReport::default());
        }
        let claimed = self.work.claim(limit).await?;
        let mut report = PassReport::default();
        for item in claimed {
            match self.process_run(&item).await {
                Ok(Outcome::Applied { changes, cursor }) => {
                    report.processed += 1;
                    report.applied += changes;
                    self.settle(&item.run, MemoryWork::Done, cursor).await;
                }
                Ok(Outcome::Skipped { reason, cursor }) => {
                    report.skipped += 1;
                    tracing::debug!(run = %item.run, %reason, "这个 Run 没有可提取的证据");
                    self.settle(&item.run, MemoryWork::Done, cursor).await;
                }
                Err(error) if error.is_permanent() => {
                    report.abandoned += 1;
                    tracing::warn!(
                        %error,
                        run = %item.run,
                        "证据读不出来，这个 Run 不再重试（标记翻成 error）"
                    );
                    self.settle(&item.run, MemoryWork::Error, item.cursor).await;
                }
                Err(error) => {
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

    /// 一个 Run 的完整处理：读日志 → 提取 → 校验 → 去重 / 冲突 → 写库。
    pub async fn process_run(&self, item: &MemoryWorkItem) -> Result<Outcome, MemoryError> {
        // 「取消、失败或结果未知的动作不能整理成成功经验」（§9.3）——取消的整个跳过：
        // 它停在用户的选择上，剩下的半截不是证据。
        if item.status == komo_kernel::types::status::RunState::Cancelled {
            return Ok(Outcome::Skipped {
                reason: "用户取消的任务".into(),
                cursor: item.cursor,
            });
        }

        let events = self.events.events_of(&item.session).await?;
        let transcript = extract::Transcript::build(&item.session, &item.run, &events, item.cursor);
        if !transcript.has_new_evidence() {
            return Ok(Outcome::Skipped {
                reason: "没有新的用户陈述或工具结果".into(),
                cursor: transcript.cursor,
            });
        }

        let pass = extract::LearningPass::new(
            Arc::clone(&self.llm),
            self.model.clone(),
            Arc::clone(&self.clock),
        );
        let raw = pass.extract(&transcript).await?;
        let observations = transcript.validate(raw, item, &self.model, self.clock.now());
        if observations.is_empty() {
            return Ok(Outcome::Skipped {
                reason: "提取结果一条都没通过校验".into(),
                cursor: transcript.cursor,
            });
        }

        let consolidator = consolidate::Consolidator::new(self);
        let changes = consolidator.apply(&observations).await?;
        Ok(Outcome::Applied {
            changes,
            cursor: transcript.cursor,
        })
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

    /// 遗忘：停用正文，并使关键词、向量与**检查点里的引用**失效（§9.6）。
    ///
    /// 前两样在仓储里（`forget` 删词条与向量行）；第三样在这里——注入表里挂着的选择要
    /// 一并清掉，否则同一个 Run 的下一段还会把它渲染进提示。检查点本身不改：它是历史
    /// 审计，而 [`Self::revalidate`] 保证它复活不了（§9.7）。
    pub async fn forget(
        &self,
        id: &MemoryId,
        expected_revision: u32,
    ) -> Result<MemoryItem, MemoryError> {
        let item = self
            .repo
            .forget(id, expected_revision, self.clock.now())
            .await?;
        let mut selections = self.selections.lock().expect("注入表");
        for injection in selections.values_mut() {
            if injection.uses.iter().any(|use_| &use_.memory == id) {
                injection.uses.retain(|use_| &use_.memory != id);
                injection.text = None;
            }
        }
        Ok(item)
    }

    // ------------------------------------------------------------ 索引（§9.5）

    /// 当前索引状态，外加一条**配置漂移**提示。
    ///
    /// 「更换向量模型、维度或预处理方式时…创建新索引代次」（§9.5），而热重载不在半路
    /// 偷偷重建（§13.3 最后一段的精神）：配置换了模型，这里只把"指纹对不上、需要一次
    /// 重建"说出来，真正动手要操作者按 `komo memory index rebuild`。
    pub async fn index_status(&self) -> Result<MemoryIndexStatus, MemoryError> {
        let mut status = self.repo.index_status().await?;
        if let Some(space) = self.space() {
            let configured = IndexBuilder::generation_id(&space);
            match &status.generation {
                Some(active) if active == &configured => {}
                Some(active) => status.errors.push(format!(
                    "配置的向量空间（{configured}）与生效代次（{active}）不是同一个：\
                     换了模型 / 维度 / 前缀就要新建代次，跑一次 `komo memory index rebuild`"
                )),
                None => status.errors.push(format!(
                    "还没有生效代次；配置的空间是 {configured}，跑一次 \
                     `komo memory index rebuild` 建起来"
                )),
            }
            if status.state == IndexState::Unconfigured {
                // 配了 embedding 却一个代次都没有，是"还没建"，不是"没配置"。
                status.state = IndexState::Building;
                status.space = Some(space);
            }
        }
        Ok(status)
    }

    /// 幂等提交一次重建（§9.5、§13.1）。
    ///
    /// 同一个代次已经在跑时返回 `accepted = false` 并给回同一个代次号——「同代次进行中
    /// 返回同一个任务」。
    pub fn request_rebuild(self: &Arc<Self>) -> Result<(String, bool), MemoryError> {
        let Some(space) = self.space() else {
            return Err(MemoryError::VectorUnconfigured);
        };
        let generation = IndexBuilder::generation_id(&space);
        {
            let mut building = self.building.lock().expect("重建表");
            if building.contains(&generation) {
                return Ok((generation, false));
            }
            building.insert(generation.clone());
        }
        let manager = Arc::clone(self);
        let named = generation.clone();
        tokio::spawn(async move {
            let outcome = manager.build_index().await;
            manager.building.lock().expect("重建表").remove(&named);
            match outcome {
                Ok(outcome) => tracing::info!(
                    generation = %named,
                    indexed = outcome.indexed,
                    total = outcome.total,
                    activated = outcome.activated,
                    "索引重建这一轮结束"
                ),
                Err(error) => tracing::error!(%error, generation = %named, "索引重建失败"),
            }
        });
        Ok((generation, true))
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

/// 一个 Run 处理完的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Applied { changes: usize, cursor: Seq },
    Skipped { reason: String, cursor: Seq },
}

#[cfg(test)]
mod tests;
