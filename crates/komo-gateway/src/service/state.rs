//! Gateway 的进程状态：一次装配，所有入口共用。
//!
//! HTTP 与聊天渠道走的是**同一段代码**（§13.1 最后一段），所以"提交一条输入"、"记一个
//! 审批决定"、"把等着的 Run 放回队列"这些动作都只在这里实现一次；`http` 与
//! `dispatcher` 各自只做自己的那层翻译。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::protocol::http::{
    ApprovalDecisionResponse, ApprovalRecord, InterventionKind, InterventionListQuery,
    InterventionSummary, SubmitRunResponse,
};
use komo_kernel::traits::{
    ApprovalRepo, Clock, CronRepo, EmbeddingClient, GatewayError, Ledger, LlmClient, MemoryRepo,
    RepoError, RunQueue, StoreError, ToolOutputStore,
};
use komo_kernel::types::agent::{AgentProfile, RunSnapshot};
use komo_kernel::types::chat::{ApprovalScope, ChannelPeer, PeerId};
use komo_kernel::types::ids::{ApprovalId, ExecutorId, RequestKey, RunId, SessionId};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::status::{RunEnd, RunState, SessionState};
use komo_kernel::types::turn::{AcceptInput, LlmError, TurnRequest};
use komo_runtime::agent::handler::{AgentRunHandler, SegmentSource};
use komo_runtime::agent::{AgentLoop, Budget, ResumedRound, RetryBudget, Segment};
use komo_runtime::approvals::ApprovalGate;
use komo_runtime::config::{ConfigHolder, EffortCapabilities};
use komo_runtime::executor::{CallEnv, ToolExecutor};
use komo_runtime::memory::{
    DbMemoryWork, LedgerEvents, MemoryManager, MemoryParts, MemoryPreamble,
};
use komo_runtime::policy::PolicyEngine;
use komo_runtime::recovery::{RecoveryIndex, RecoveryScan, UnfinishedRun};
use komo_runtime::scheduler::{HandlerError, Scheduler, SchedulerConfig, Waker};
use komo_runtime::tools::paths;
use komo_store::{
    Db, PayloadStore, RecoveryStore, TursoApprovalRepo, TursoCronRepo, TursoDeliveryRepo,
    TursoMemoryRepo, TursoRunQueue,
};
use time::OffsetDateTime;

use crate::channels::ChannelFactory;
use crate::channels::ChannelRegistry;
use crate::deliveries::DeliveryLog;
use crate::notifier::HomeNotifier;
use crate::sse::{EventHub, SharedHub};

use super::ledgers::{RoutedLedger, RoutedOutputs, SessionLedgers};
use super::segment::GatewaySegments;

/// 系统时钟。**进程里唯一读墙上时间的地方**（其余一切经 `Clock`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// 可以在热重载时换掉的模型后端。
///
/// 「模型改完，下一个 Run 用新模型，**正在跑的 Run 继续用它开始时抓的快照**」
/// （§3 第 2 步）——`begin_turn` 时读一次当前实例，读到之后那个 driver 就属于那个
/// Run，中途不会被换走。
pub struct SwappableLlm {
    inner: arc_swap::ArcSwap<ArcLlm>,
}

struct ArcLlm(Arc<dyn LlmClient>);

impl std::fmt::Debug for SwappableLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwappableLlm").finish_non_exhaustive()
    }
}

impl SwappableLlm {
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        SwappableLlm {
            inner: arc_swap::ArcSwap::from_pointee(ArcLlm(client)),
        }
    }

    /// 换成新造的那一个。**只影响之后开始的 Run。**
    pub fn swap(&self, client: Arc<dyn LlmClient>) {
        self.inner.store(Arc::new(ArcLlm(client)));
    }
}

#[async_trait]
impl LlmClient for SwappableLlm {
    async fn begin_turn(
        &self,
        req: TurnRequest,
    ) -> Result<Box<dyn komo_kernel::traits::TurnDriver>, LlmError> {
        let current = self.inner.load();
        current.0.begin_turn(req).await
    }
}

/// 没有配置模型 / 配置不可用时的后端：每次调用都报同一个明确的错误。
///
/// 它存在只是为了让 Gateway **起得来**——渠道、审批、恢复扫描与操作命令都不依赖模型，
/// 而一个连 `komo doctor` 都跑不起来的进程什么都诊断不了。
#[derive(Debug)]
pub struct UnconfiguredLlm {
    reason: String,
}

impl UnconfiguredLlm {
    pub fn new(reason: impl Into<String>) -> Self {
        UnconfiguredLlm {
            reason: reason.into(),
        }
    }
}

#[async_trait]
impl LlmClient for UnconfiguredLlm {
    async fn begin_turn(
        &self,
        _req: TurnRequest,
    ) -> Result<Box<dyn komo_kernel::traits::TurnDriver>, LlmError> {
        Err(LlmError::Rejected {
            status: 503,
            message: format!("模型后端不可用：{}", self.reason),
        })
    }
}

/// store 的 `RecoveryStore` 包成 runtime 的 [`RecoveryIndex`]。
///
/// 两件事由这一层补上：
///
/// - **会话那一维**（`RecoveryIndex::session_state`，§8.9）：`RecoveryInput` 要答"这条 Run
///   所属的会话还在不在服务范围里"，store 的 `unfinished_runs` 只带 Run 自己的列，所以
///   这里把它接上。行不在 = `None`，runtime 当"不服务"处置。
/// - **正在本进程手里跑的那些不进这一趟**：一次周期对账不该去判一条活着的 Run。判据是
///   [`InFlight`]——调度器领走时落一行、handler 返回时摘掉。少了它，周期对账会把
///   `running` 且 `claimed_by == self` 的行当成"旧执行者没确认结束"而停成
///   `waiting + intervention`（§8.7 那句"无法确认旧执行已结束时阻止重复启动"的另一面）。
#[derive(Debug)]
pub struct RecoveryIndexOf {
    store: RecoveryStore,
    in_flight: Arc<InFlight>,
}

impl RecoveryIndexOf {
    pub fn new(store: RecoveryStore, in_flight: Arc<InFlight>) -> Self {
        RecoveryIndexOf { store, in_flight }
    }
}

#[async_trait]
impl RecoveryIndex for RecoveryIndexOf {
    async fn unfinished_runs(&self) -> Result<Vec<UnfinishedRun>, StoreError> {
        Ok(self
            .store
            .unfinished_runs()
            .await?
            .into_iter()
            .filter(|run| !self.in_flight.holds(&run.run))
            .map(|run| UnfinishedRun {
                run: run.run,
                session: run.session,
                state: run.state,
                wait: run.wait,
                claimed_by: run.claimed_by,
                result_delivered: run.result_delivered,
            })
            .collect())
    }

    async fn session_state(&self, session: &SessionId) -> Result<Option<SessionState>, StoreError> {
        komo_store::repos::session::state(self.store.db(), session).await
    }

    async fn session_content(&self, session: &SessionId) -> Result<bool, StoreError> {
        // 权威在 store（`content_available`），这里只把 root 接上——root 从配置快照的
        // paths 出来，`RecoveryStore` 建的时候已经拿在手里了。**它只观察，不建目录**
        // （§8.9：观察不改写）。
        Ok(
            komo_store::repos::reconcile::content_available(self.store.sessions_root(), session)
                .await,
        )
    }

    async fn reclaim_running(&self, executor: &ExecutorId) -> Result<u64, StoreError> {
        self.store.reclaim_running(executor).await
    }

    async fn backfill(&self, run: &RunId) -> Result<(), StoreError> {
        self.store.backfill(run).await
    }

    async fn requeue(&self, run: &RunId) -> Result<(), StoreError> {
        self.store.requeue(run).await
    }

    async fn mark_needs_attention(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.store.mark_needs_attention(run, reason).await
    }
}

/// 本进程**此刻**正在跑的 Run（§8.9 的存活判定）。
///
/// §8.7 的租约只用来**发现**"没人管了"；对账见到租约过期时还要过一道"持有者确已不在，
/// 或自己内存里已不再持有它"才回收。少这一道，一次二十分钟的调用会在主人还活着的时候被
/// 当成孤儿——那不是恢复，是真的重复副作用（§8.6）。这个表就是"自己内存里还持有着"的
/// 那一半：调度器每领一条就在这里落一行，handler 返回时摘掉。
#[derive(Debug, Default)]
pub struct InFlight {
    runs: Mutex<std::collections::BTreeSet<RunId>>,
}

impl InFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// 落一行并返回一个**掉了就摘掉**的守卫（handler 返回、panic 展开、任务被取消都会
    /// 走到 `Drop`）。
    pub fn enter(self: &Arc<Self>, run: &RunId) -> InFlightGuard {
        self.runs.lock().expect("在跑表").insert(run.clone());
        InFlightGuard {
            in_flight: Arc::clone(self),
            run: run.clone(),
        }
    }

    /// 这条 Run 现在还在本进程手里吗。
    pub fn holds(&self, run: &RunId) -> bool {
        self.runs.lock().expect("在跑表").contains(run)
    }

    /// 现在在手里的全部。
    pub fn snapshot(&self) -> std::collections::BTreeSet<RunId> {
        self.runs.lock().expect("在跑表").clone()
    }
}

/// [`InFlight::enter`] 的守卫。
pub struct InFlightGuard {
    in_flight: Arc<InFlight>,
    run: RunId,
}

impl std::fmt::Debug for InFlightGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlightGuard")
            .field("run", &self.run)
            .finish()
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.in_flight
            .runs
            .lock()
            .expect("在跑表")
            .remove(&self.run);
    }
}

/// 包在 [`AgentRunHandler`] 外面，只为记 [`InFlight`]。
///
/// 它不改任何行为：转发一个调用，前后各动一下那张表。调度器看到的是同一条
/// [`RunHandler`](komo_runtime::scheduler::RunHandler)。
#[derive(Debug)]
pub struct TrackingHandler {
    inner: Arc<AgentRunHandler>,
    in_flight: Arc<InFlight>,
}

impl TrackingHandler {
    pub fn new(inner: Arc<AgentRunHandler>, in_flight: Arc<InFlight>) -> Self {
        TrackingHandler { inner, in_flight }
    }
}

#[async_trait]
impl komo_runtime::scheduler::RunHandler for TrackingHandler {
    async fn run(
        &self,
        claimed: komo_kernel::types::status::Claimed,
    ) -> Result<(), komo_runtime::scheduler::HandlerError> {
        let _guard = self.in_flight.enter(&claimed.run);
        self.inner.run(claimed).await
    }
}

/// 一次装配出来的 Gateway。
pub struct GatewayState {
    pub home: PathBuf,
    /// 共享 agent 目录按哪个家目录算（§5.6）。见 [`Assembly::shared_home`]。
    pub shared_home: Option<PathBuf>,
    pub instance_id: String,
    pub token: String,
    pub started_at: OffsetDateTime,
    pub executor: ExecutorId,
    pub clock: Arc<dyn Clock>,
    pub config: Arc<ConfigHolder>,
    pub caps: EffortCapabilities,
    pub db: Db,
    pub hub: SharedHub,
    pub ledgers: Arc<SessionLedgers>,
    pub routed: Arc<RoutedLedger>,
    /// 审计补写的叫醒铃。审批请求落库之后由 [`RoutedLedger::suspend`] 按一下，周期
    /// （`AUDIT_TICK`）只是兜底——界面等不起那一拍（§8.5 的补写顺序不变）。
    pub audit_wake: Arc<tokio::sync::Notify>,
    /// 一次 Run 的三个写入者共用的那个账本句柄（测试的故障注入口包的就是它）。审计补写
    /// 也走它——补写是账本写入的一种，没有理由绕过同一条缝。
    pub turn_ledger: Arc<dyn Ledger>,
    pub outputs: Arc<dyn ToolOutputStore>,
    pub queue: Arc<TursoRunQueue>,
    pub approvals: Arc<ApprovalGate>,
    pub approval_repo: Arc<dyn ApprovalRepo>,
    pub cron: Arc<dyn CronRepo>,
    pub memory: Arc<dyn MemoryRepo>,
    /// §9 的那一层：召回与注入、自动积累、向量索引与代次。
    pub memories: Arc<MemoryManager>,
    /// 记忆注入接在系统提示后面的那一口。热重载重建模型后端时要把它接回去，所以留着。
    pub preamble: Arc<MemoryPreamble>,
    pub recovery_store: RecoveryStore,
    pub deliveries: Arc<TursoDeliveryRepo>,
    pub notifier: Arc<HomeNotifier>,
    pub channels: Arc<ChannelRegistry>,
    pub llm: Arc<SwappableLlm>,
    /// 上一条投到 home chat 的重载错误。同一条错误不重复投；装上之后清掉并说一声。
    pub reload_notice: Mutex<Option<String>>,
    pub scheduler: Arc<Scheduler>,
    /// 跑 Run 的那个 handler。调度器握着一份（`Arc<dyn RunHandler>`），这里再留一份具名
    /// 的：§7.5 的 `verify` 答复要直接调 [`AgentRunHandler::settle_by_operator`]，而那条
    /// 路不经过调度器。
    pub handler: Arc<AgentRunHandler>,
    /// 本进程此刻还在手里的 Run（§8.9 的存活判定）。
    pub in_flight: Arc<InFlight>,
    /// 判决用的那一份规则表。**热重载换的就是它**（[`Self::install_policy`] 的兄弟：
    /// `reload::apply` 直接调 `policy.install`）。
    pub policy: Arc<PolicyEngine>,
    pub segments: Arc<GatewaySegments>,
    /// 执行器挂着的工具名。提示里的 skills 目录行按它门控（§5.6 的 `requires_tools:`），
    /// 而重载重算那一块时要用——那时执行器已经造好了，名字留在这里最省事。
    pub tool_names: Vec<String>,
    /// §5.6 那份**活的** skill 注册表。与 [`GatewayState::segments`] 共用一个 `Arc`。
    ///
    /// 存的是注册表而不是渲染好的那一段文本：文本按用途现渲染（系统提示一处、`skill://`
    /// 的挂载点一处），**事实只有这一份**——存一份字符串就是第二份事实，它会和这一份
    /// 各走各的。注册表每次查询重扫目录（§5.6），所以"人改了 SKILL.md"不必重启。
    pub skills: Arc<std::sync::RwLock<komo_agent::skills::SkillRegistry>>,
    pub supervisor: Arc<super::channels::ChannelSupervisor>,
    /// Dispatcher。**构造之后才填**：它握着这份状态，反过来也要被渠道拿到。
    pub inbound: std::sync::OnceLock<Arc<dyn komo_kernel::traits::Inbound>>,
    /// 正在被人看着的 Run，以及已经投出去的 Intervention 句柄。
    ///
    /// **一个 Run 只有一个看的人**：Cron 的看客与交互的看客都走
    /// [`run_watch::watch`](super::run_watch::watch)，登记表是它们之间唯一的约定——
    /// 没有它，一条审批会被投两遍（去重键那一层管的是平台重投，管不到这个）。
    ///
    /// 投递表按**句柄**记（审批是短 ID，另外两类是 Run ID，§7.5 那张表）：三类共用一条
    /// 到达率，"投过一次就不再投"这条判据也只能有一个。
    pub watching: Mutex<std::collections::BTreeSet<RunId>>,
    pub approvals_delivered: Mutex<std::collections::BTreeSet<String>>,
    /// 模型请求的有界退避预算上限（§8.5）。
    pub max_retries: u32,
    /// 一段最多跑几轮模型（§6）。
    pub max_rounds: u32,
}

impl std::fmt::Debug for GatewayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayState")
            .field("instance_id", &self.instance_id)
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

/// 装配 Gateway 要的那几样外部东西。
pub struct Assembly {
    pub home: PathBuf,
    pub config: Arc<ConfigHolder>,
    pub caps: EffortCapabilities,
    pub db: Db,
    pub clock: Arc<dyn Clock>,
    pub instance_id: String,
    pub token: String,
    /// 测试注入的模型后端；`None` = 按配置造。
    pub llm: Option<Arc<dyn LlmClient>>,
    /// 测试注入的向量后端；`None` = 按 `memory.embedding` alias 造（造不出来就只有关键词臂）。
    pub embeddings: Option<Arc<dyn EmbeddingClient>>,
    /// 共享 agent 目录（`~/.agents/skills`、`~/.claude/skills`）按哪个家目录算；
    /// `None` = 当前用户的家目录。**注入而不是现场读 `$HOME`**（§5.6）：否则提示里有哪些
    /// skill 取决于这台机器上别人装过什么。
    pub shared_home: Option<PathBuf>,
    pub tools: Vec<Arc<dyn komo_kernel::traits::Tool>>,
    /// 渠道工厂：热重载时按平台重造（§3 第 3 步）。
    pub channels: Vec<Arc<dyn ChannelFactory>>,
}

impl GatewayState {
    /// 把 store、runtime 与渠道接成一个进程。
    pub async fn assemble(parts: Assembly) -> Result<Arc<GatewayState>, GatewayError> {
        let Assembly {
            home,
            config,
            caps,
            db,
            clock,
            instance_id,
            token,
            llm,
            embeddings,
            shared_home,
            tools,
            channels: factories,
        } = parts;

        let snapshot = config.current();
        let executor = ExecutorId::from_raw(instance_id.clone());
        let hub: SharedHub = Arc::new(EventHub::new());
        let sessions_root = snapshot.paths.sessions_dir.clone();

        let ledgers = Arc::new(SessionLedgers::new(
            db.clone(),
            sessions_root.clone(),
            Arc::clone(&clock),
            Arc::clone(&hub),
            executor.clone(),
        ));
        let audit_wake = Arc::new(tokio::sync::Notify::new());
        let routed = Arc::new(RoutedLedger::new(
            Arc::clone(&ledgers),
            db.clone(),
            Arc::clone(&clock),
            Arc::clone(&audit_wake),
        ));
        let outputs: Arc<dyn ToolOutputStore> = Arc::new(RoutedOutputs::new(
            Arc::clone(&ledgers),
            Arc::clone(&routed),
        ));

        // 租约窗口用 store 的默认值（2 分钟）：**gateway 不另发明一个配置项**。窗口越长
        // "handler 死了"被发现得越晚，越短长调用续租得越勤——那是 store 那一侧的取舍，
        // 这里只是把它交给队列（§8.7）。
        let queue = Arc::new(TursoRunQueue::with_lease(
            db.clone(),
            komo_store::repos::queue::DEFAULT_LEASE_WINDOW,
        ));
        let approval_repo: Arc<dyn ApprovalRepo> = Arc::new(TursoApprovalRepo::new(db.clone()));
        let approvals = Arc::new(ApprovalGate::new(
            Arc::clone(&approval_repo),
            Arc::clone(&clock),
        ));
        let cron: Arc<dyn CronRepo> = Arc::new(TursoCronRepo::new(db.clone()));
        let memory: Arc<dyn MemoryRepo> = Arc::new(TursoMemoryRepo::new(db.clone()));
        let deliveries = Arc::new(TursoDeliveryRepo::new(db.clone()));
        let channels = Arc::new(ChannelRegistry::new());
        let delivery_log = Arc::new(DeliveryLog::new(
            Arc::clone(&deliveries),
            Arc::clone(&channels),
            Arc::clone(&clock),
        ));
        let notifier = Arc::new(HomeNotifier::new(delivery_log, Arc::clone(&config)));

        // 记忆这一层要**先于**模型后端装好：注入是系统提示的一部分，而
        // `LlmFactory::with_preamble` 在造后端时就要拿到它（§9.4）。
        //
        // 向量后端**注入的优先**（测试），否则这里只留空槽：探测在后台跑
        // （`spawn_embedding_probe`），不等它。
        let injected_embeddings = embeddings;
        // 判断后端（§9.4 的可选一步）：**造它不碰网络**，所以直接装，不用像向量维度那样
        // 后台探测。配置热重载会换掉它（`GatewayState::refresh_reranker`）。
        let reranker = build_reranker(&snapshot, &config);
        let memories = Arc::new(MemoryManager::new(MemoryParts {
            config: snapshot.memory.clone(),
            repo: Arc::clone(&memory),
            catalog: Arc::new(TursoMemoryRepo::new(db.clone())),
            embeddings: injected_embeddings.clone(),
            reranker,
            llm: Arc::clone(&llm_for_memory(&snapshot, &config, &caps, llm.as_ref())),
            events: Arc::new(LedgerEvents(Arc::clone(&routed) as Arc<dyn Ledger>)),
            work: Arc::new(DbMemoryWork::new(db.clone(), Arc::clone(&clock))),
            clock: Arc::clone(&clock),
        }));
        let preamble = Arc::new(MemoryPreamble::new(Arc::clone(&memories)));
        if injected_embeddings.is_none() {
            spawn_embedding_probe(Arc::clone(&memories), Arc::clone(&config));
        }

        let llm = Arc::new(SwappableLlm::new(match llm {
            Some(client) => client,
            None => build_llm(&snapshot, &config, &caps, &preamble),
        }));

        // 一次 Run 的三个账本写入者（执行器、AgentLoop、handler）共用这一个句柄。
        // W5 的故障注入口就在这里：没装过注入时它**就是** `routed` 本身（§14）。
        #[cfg(any(test, feature = "test-support"))]
        let turn_ledger =
            super::test_support::wrap_ledger(&home, Arc::clone(&routed) as Arc<dyn Ledger>);
        #[cfg(not(any(test, feature = "test-support")))]
        let turn_ledger = Arc::clone(&routed) as Arc<dyn Ledger>;

        // **一份 engine，两处用**：executor 判每一次调用，热重载换的是同一处的规则表
        // （§3：Policy 每次决策读规则，不缓存）。各造一份的写法会让 `policy.toml` 改完
        // 只有快照变了、判决还是旧的——"改 policy 不用重启"就成了假话。
        let policy = Arc::new(
            PolicyEngine::from_rules(snapshot.policy.clone())
                // §8.10 第 4 条：komo 自己的状态不许被工具写（`sessions/`、`state.db`、
                // `runtime/`、`.env`）。**缺省空名单 = 那条规则永不命中**。
                .with_protection(protected_paths(&snapshot, &home)),
        );
        // §5.6 的目录行：注册表**启动时按快照造一份**，那一块文本按用途现渲染（系统提示
        // 一处、`skill://` 的挂载点一处）——两处同源，不会有"提示里有、读不到"这种事。
        let tool_names: Vec<String> = tools.iter().map(|tool| tool.definition().name).collect();
        let skills = Arc::new(std::sync::RwLock::new(skills_registry(
            &snapshot,
            shared_home.as_deref(),
        )));
        let executor_tools = Arc::new(ToolExecutor::new(
            tools,
            Arc::clone(&turn_ledger),
            Arc::clone(&outputs),
            ApprovalGate::new(Arc::clone(&approval_repo), Arc::clone(&clock)),
            policy.as_ref().clone(),
            Arc::clone(&clock),
        ));

        let agent = Arc::new(AgentLoop::new(
            Arc::clone(&llm) as Arc<dyn LlmClient>,
            Arc::clone(&turn_ledger),
            Arc::clone(&executor_tools),
            Arc::clone(&clock),
        ));

        let max_rounds = Budget::default().max_rounds;
        let max_retries = RetryBudget::default().max_attempts;

        let segments = Arc::new(
            GatewaySegments::new(
                Arc::clone(&routed),
                db.clone(),
                Arc::clone(&approval_repo),
                RecoveryStore::new(db.clone(), sessions_root.clone()),
                snapshot.paths.workspaces_dir.clone(),
                max_rounds,
                max_retries,
            )
            // §8.3：这个 Session 自己的输出与产物是一段只读根，模型才读得到"完整输出在哪"。
            .with_session_files(sessions_root.clone())
            // 回放那一侧的投影：同一份输出存储；预算按当前快照现读（§3）。
            .with_projection(Arc::clone(&outputs))
            .with_config(Arc::clone(&config))
            // Cron Run 用它那个 Job 的执行预算（§10）。
            .with_cron(Arc::clone(&cron))
            // 每一段装配时按当前用户输入召回一次，并把用到的条目记进
            // `TurnRequest::memories`（§9.7 的审计证据）。
            .with_memories(
                Arc::clone(&memories),
                komo_store::CheckpointStore::new(db.clone()),
            )
            // §5.6 的目录行：与 state 共用一个 `Arc`，重载时就地换内容。
            .with_skills(Arc::clone(&skills))
            // 交给模型的 Schema 由执行器按这一次的能力面渲染（§4 末）：`definitions_for`
            // 与执行器查找工具用的是同一份实现与同一份目录。
            .with_executor(Arc::clone(&executor_tools)),
        );

        let handler = Arc::new(AgentRunHandler::new(
            agent,
            Arc::clone(&executor_tools),
            Arc::clone(&turn_ledger),
            Arc::clone(&segments) as Arc<dyn SegmentSource>,
            Arc::clone(&queue) as Arc<dyn RunQueue>,
            executor.clone(),
        ));

        // §8.9 的存活判定：调度器领走时落一行，handler 返回时摘掉。
        let in_flight = Arc::new(InFlight::new());
        let scheduler = Arc::new(Scheduler::new(
            Arc::clone(&queue) as Arc<dyn RunQueue>,
            Arc::new(TrackingHandler::new(
                Arc::clone(&handler),
                Arc::clone(&in_flight),
            )) as Arc<dyn komo_runtime::scheduler::RunHandler>,
            executor.clone(),
            SchedulerConfig::default(),
        ));

        let recovery_store = RecoveryStore::new(db.clone(), sessions_root);

        Ok(Arc::new(GatewayState {
            home,
            shared_home,
            instance_id,
            token,
            started_at: clock.now(),
            executor,
            clock,
            config,
            caps,
            db,
            hub,
            ledgers,
            routed,
            audit_wake,
            turn_ledger,
            outputs,
            queue,
            approvals,
            approval_repo,
            cron,
            memory,
            memories,
            preamble,
            recovery_store,
            deliveries,
            reload_notice: Mutex::new(None),
            notifier,
            channels,
            llm,
            scheduler,
            handler,
            in_flight,
            policy,
            segments,
            supervisor: Arc::new(super::channels::ChannelSupervisor::new(factories)),
            inbound: std::sync::OnceLock::new(),
            watching: Mutex::new(std::collections::BTreeSet::new()),
            approvals_delivered: Mutex::new(std::collections::BTreeSet::new()),
            tool_names,
            skills,
            max_retries,
            max_rounds,
        }))
    }

    pub fn snapshot(&self) -> Arc<ConfigSnapshot> {
        self.config.current()
    }

    /// 按**当前**快照重算系统提示里的 skills 目录行（§5.6）。
    ///
    /// 目录行是启动快照，配置重载是唯一会动它的时刻：`paths.skill_dirs` 改了、人刚
    /// `komo skills disable` 过，重载之后新的一段就该按新的来。算了但是没变就不吭声。
    /// 按**当前**快照重装判断后端（§9.4）：`[typesafe]` 或 `memory.retrieval.rerank`
    /// 一变，下一次召回就按新的来——与 `refresh_skills_prompt` 同一个理由（§3 的热重载
    /// 不能有"改了没反应"的键）。
    pub fn refresh_reranker(&self) {
        self.memories
            .install_reranker(build_reranker(&self.snapshot(), &self.config));
    }

    /// 按**当前**快照重装 skill 注册表（§5.6）。
    ///
    /// 配置重载是唯一会动它的时刻：`paths.skill_dirs` 改了、人刚 `komo skills disable` 过，
    /// 之后新的一段就该按新的来（注册表本身每次查询重扫目录，人改 `SKILL.md` 不必等重载）。
    /// 换完渲染出来还是同一段就不吭声——**注册表照换**，因为目录集合本身就是新的事实。
    pub fn refresh_skills_prompt(&self) {
        let snapshot = self.snapshot();
        let tool_names = self.tool_names.clone();
        let fresh = skills_registry(&snapshot, self.shared_home.as_deref());
        let mut current = self.skills.write().expect("skills 注册表");
        let before = skills_block(&current, &tool_names);
        let after = skills_block(&fresh, &tool_names);
        *current = fresh;
        if before == after {
            return;
        }
        tracing::info!(
            skills = after.lines().count().saturating_sub(2),
            "系统提示里的 skills 目录行变了"
        );
    }

    /// 按**当前**快照造一个 Cron 调度器。
    ///
    /// 每次现造而不是存一个：Job 的模型覆盖之外，它还握着主模型的那一份快照，而模型
    /// 改完之后下一次触发就该用新的（§3 第 2 步）。
    pub fn cron_scheduler(&self) -> komo_runtime::scheduler::CronScheduler {
        komo_runtime::scheduler::CronScheduler::new(
            Arc::clone(&self.cron),
            Arc::clone(&self.routed) as Arc<dyn Ledger>,
            Arc::clone(&self.clock),
            Arc::new(komo_runtime::scheduler::JiffZoneResolver::new()),
            self.snapshot().model.clone(),
        )
        .with_waker(self.waker())
    }

    /// 这一轮扫描投出去的每一次触发都找个人盯着（§10 的最后两步）。
    ///
    /// Cron 没有来源会话，所以"等待审批"这条路只能从这里投到 home chat——不盯着，
    /// 一个 Cron Run 会停在等待上而没有人知道它在等（§7.4 / §11.4）。
    pub async fn watch_fired(self: &Arc<Self>, fired: &[komo_runtime::scheduler::Fired]) {
        for one in fired {
            // Job 可能在触发与这一刻之间被改了；读不出来就按默认的 `always` 盯着，
            // **投多一条胜过一次静默的等待**。
            let job = self.cron.get(&one.job).await.ok().flatten();
            match job {
                Some(job) => self.watch_cron_run(one, &job, true),
                None => tracing::debug!(job = %one.job, "触发之后这个 Job 不在了，不盯了"),
            }
        }
    }

    /// 开始盯这个 Run。答 `false` = 已经有人在看了，别再起一个。
    pub fn start_watching(&self, run: &RunId) -> bool {
        self.watching.lock().expect("看客表").insert(run.clone())
    }

    pub fn stop_watching(&self, run: &RunId) {
        self.watching.lock().expect("看客表").remove(run);
    }

    /// 这个句柄的 Intervention 还没被投出去过。答 `false` = 投过了（同一条只问一次人）。
    pub fn start_delivering_intervention(&self, handle: &str) -> bool {
        self.approvals_delivered
            .lock()
            .expect("投递表")
            .insert(handle.to_string())
    }

    /// 盯一个交互 Run：终态回到来源会话，等待审批 / 需要处理投来源会话 + home chat
    /// （§11.4）。`peer` 为 `None` 就是 TUI / HTTP——那时只有 home chat。
    pub fn watch_interactive_run(
        self: &Arc<Self>,
        session: &SessionId,
        run: &RunId,
        peer: Option<ChannelPeer>,
    ) {
        super::run_watch::watch(
            Arc::clone(self),
            session.clone(),
            run.clone(),
            super::run_watch::Watcher::Interactive { peer },
        );
    }

    /// 盯一次触发。`scheduled = false` 是手动 run：它**没有触发记录**（§10：不冒充
    /// 定时触发），所以只投递，不回写状态。
    pub fn watch_cron_run(
        self: &Arc<Self>,
        fired: &komo_runtime::scheduler::Fired,
        job: &komo_kernel::cron::CronJob,
        scheduled: bool,
    ) {
        super::cron_watch::watch(
            Arc::clone(self),
            super::cron_watch::Watched::of(fired, job, scheduled),
        );
    }

    /// 模型 / 凭证变了：换掉实例。**正在跑的 Run 不换**——它握着自己那一个 driver。
    pub fn rebuild_llm(&self) {
        let snapshot = self.snapshot();
        self.llm.swap(build_llm(
            &snapshot,
            &self.config,
            &self.caps,
            &self.preamble,
        ));
        tracing::info!("模型后端已按新配置重建（正在跑的 Run 不受影响）");
    }

    /// 某个 `[channels.*]` 变了：只停掉并重启那一个渠道（§3 第 3 步）。
    pub async fn restart_channel(
        self: &Arc<Self>,
        platform: komo_kernel::types::chat::ChannelPlatform,
    ) {
        let supervisor = Arc::clone(&self.supervisor);
        supervisor.restart(self, platform).await;
    }

    pub fn waker(&self) -> Waker {
        self.scheduler.waker()
    }

    /// 恢复扫描（§8.7）。观察与决策在 runtime，索引在 store，这一层只把它俩接上。
    pub fn recovery(&self) -> RecoveryScan {
        RecoveryScan::new(
            Arc::clone(&self.routed) as Arc<dyn Ledger>,
            Arc::new(RecoveryIndexOf::new(
                self.recovery_store.clone(),
                Arc::clone(&self.in_flight),
            )),
            Arc::clone(&self.outputs),
            Arc::clone(&self.approval_repo),
            Arc::clone(&self.clock),
            self.executor.clone(),
            Arc::new(komo_runtime::recovery::LockHolderLiveness::holding_lock(
                self.executor.clone(),
                komo_runtime::recovery::ChildRegistry::new(
                    self.snapshot().paths.runtime_dir.join("children"),
                ),
            )),
        )
        // 「output.json 已完成但 JSONL 结果事件尚未写入 → 校验身份、计划及完成状态后补记
        // 结果；**不能只凭文件存在判断**」（§14 故障注入表）。没有这一口时恢复只能把
        // `started` 而无结果的调用交给工具核对，那条路对一次已经跑完的 `shell` 答不出
        // "它到底发生了没有"。
        .with_orphan_outputs(Arc::new(
            komo_runtime::recovery::SessionDirOrphanOutputs::new(
                self.snapshot().paths.sessions_dir.clone(),
            ),
        ))
    }

    /// 一个 Agent 的**主会话**（§11.2 的 home session，按 Agent 各一份）。
    ///
    /// 「操作者的私聊——飞书 DM、Telegram DM、WeChat、TUI——全部落到同一个 home
    /// session」这条规则在多 Agent 下变成**每个 Agent 一份主会话**（§4.2）：归属记在
    /// `sessions.agent_id` + `sessions.kind = 'main'` 上，唯一性由 store 的部分唯一索引
    /// （`sessions_main_per_agent`）保证——不是"各建一个再挑最早的"。
    ///
    /// 归属**在创建时定下来**，此后不随消息路由变化（§4.2）：换助手是路由到另一个会话，
    /// 不是给已有会话换人格、换能力，同时留着原来的全部上下文。
    pub async fn main_session(&self, agent_id: &str) -> Result<SessionId, GatewayError> {
        if let Some(main) = komo_store::repos::session::main_for_agent(&self.db, agent_id).await? {
            // 同一个 Agent 有多条主会话只可能是**升级过来的旧库**在说话（索引补列之后才
            // 存在）。store 取最早的那条，这里把其余的**报出来**：两个入口各建过一个主会话
            // 是操作者该知道的事。
            if !main.duplicates.is_empty() {
                tracing::warn!(
                    agent = %agent_id,
                    main = %main.row.id,
                    duplicates = %main
                        .duplicates
                        .iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join("、"),
                    "这个 Agent 有多条主会话，取最早的那一条"
                );
            }
            return Ok(SessionId::from_raw(main.row.id));
        }

        // 候选：**升级之前**那个全局主会话（`origin = 'home'`、还没有归属）。它按 §八
        // 「已有 Session 归入默认 Agent」读作默认 Agent 的那一份——所以**只有默认 Agent
        // 认它**：别的 Agent 认了它，一次升级之后操作者原来那段对话就会凭空换一个助手。
        // 没有候选（或者不归我）就现铸一个。
        let candidate = match self.unowned_main().await? {
            Some(session) if agent_id == self.snapshot().agent.default_agent => session,
            _ => SessionId::new_at(self.clock.now()),
        };
        let row = komo_store::repos::session::ensure_main(
            &self.db,
            &candidate,
            agent_id,
            HOME_ORIGIN,
            &session_log_path(&candidate),
        )
        .await?;
        let session = SessionId::from_raw(row.id);
        // **建行与建目录是两件事**：上面那一句只写库（`kind` / `agent_id` / `jsonl_path`），
        // 会话目录与 `events.jsonl` 由 Coordinator 的 `open` 落下来。它认到行已经在就原样
        // 返回，一行归属都不动。
        self.ledgers.open(&session, HOME_ORIGIN).await?;
        Ok(session)
    }

    /// **没有归属的入口**（TUI / CLI / `/v1/home-session`，以及没有绑定 Agent 的私聊）
    /// 落在哪个会话。
    ///
    /// §四：「`default_agent` 指名"没有归属的入口（TUI、CLI、Cron）走谁"」。这些入口答不出
    /// "找谁"，所以它们走同一个名字，而不是各自挑一个。
    pub async fn default_main_session(&self) -> Result<SessionId, GatewayError> {
        self.main_session(&self.snapshot().agent.default_agent)
            .await
    }

    /// 升级之前那个**还没有归属**的全局主会话：`origin = 'home'` + `agent_id` 为空。
    ///
    /// 补 `kind` 那一列时按 `origin = 'home'` 回填成主会话（store 的 `BACKFILL_KIND`），
    /// 而 `agent_id` **故意留空**——"迁移不替路由认主"。认主是这里的事：默认 Agent 认它
    /// （[`Self::main_session`] 的候选分支）。
    async fn unowned_main(&self) -> Result<Option<SessionId>, GatewayError> {
        let mut found: Vec<SessionId> = komo_store::repos::session::list(&self.db, true)
            .await?
            .into_iter()
            .filter(|record| record.agent_id.is_empty() && record.origin == HOME_ORIGIN)
            .map(|record| record.session)
            .collect();
        found.sort();
        Ok(found.into_iter().next())
    }

    /// 这个会话归哪个 Agent（§4.2 的执行身份那一半）。
    ///
    /// 归属写在 `sessions.agent_id` 上、建会话时定下来，**不随消息路由变化**。空串 =
    /// 还没有归属（升级前建的行）：按 `default_agent` 用它，**不改写这一行**——归属只写
    /// 一次，写的人是建会话的那一步（`main_session` / `set_agent`）。
    pub async fn agent_of_session(&self, session: &SessionId) -> Result<String, GatewayError> {
        let agent_id = komo_store::repos::session::get(&self.db, session)
            .await?
            .map(|record| record.agent_id)
            .unwrap_or_default();
        Ok(self.owner_or_default(agent_id))
    }

    /// 一条记载下来的归属，或者默认 Agent（空串 = 还没有归属）。
    fn owner_or_default(&self, agent_id: String) -> String {
        if agent_id.is_empty() {
            self.snapshot().agent.default_agent.clone()
        } else {
            agent_id
        }
    }

    /// 把一个**刚建出来、还没有归属**的会话记在默认 Agent 名下。
    ///
    /// 「一个 Session 的 `agent_id` 创建后不随消息路由变化」（§4.2）：不写这一笔，一处
    /// 会话就成了"当前谁是默认 Agent 就归谁"——操作者换一次 `default_agent`，那些群聊会
    /// 悄悄换一个人格继续说话。写了就只写一次（store 的 `set_agent` 是条件更新），之后
    /// 谁也改不动它。
    ///
    /// 写不进去**不让这次路由失败**：它最坏是回到"按当前默认 Agent 用它"，而拿不到会话是
    /// 拿不到任务。
    pub async fn own_session(&self, session: &SessionId) -> Result<(), GatewayError> {
        let agent_id = self.snapshot().agent.default_agent.clone();
        match komo_store::repos::session::set_agent(&self.db, session, &agent_id).await {
            Ok(_) => Ok(()),
            Err(error) => {
                tracing::warn!(%error, session = %session, agent = %agent_id, "归属没记到会话上");
                Ok(())
            }
        }
    }

    /// **受理这一刻的身份与能力**（§4.3）。
    ///
    /// 算出来的 [`RunSnapshot`] 随 `run.accepted` 落账，此后就是这一条 Run 的唯一身份
    /// 依据：恢复时按它装配，**不拿当前配置去覆盖它**。
    ///
    /// 落在快照里的每一样都在这一步定死，理由是它们**都可能在中途被改**：Profile 的指令、
    /// 工具表、工作目录、模型 alias。审批可能一小时之后才答复，而那时这些文件与配置都可能
    /// 已经变过——恢复出来的 Run 不能因此换一副面孔。
    async fn freeze_run(
        &self,
        config: &ConfigSnapshot,
        agent_id: &str,
        session: &SessionId,
        record: Option<&komo_store::repos::session::SessionRecord>,
        model: Option<ModelConfig>,
    ) -> Result<RunSnapshot, GatewayError> {
        let profile = match config.agent.get(agent_id) {
            Some(profile) => profile,
            None => {
                // 配置里已经没有这个 Agent 了（`[agents]` 被改过）。**一条配置改动不该让
                // 输入失败**：退回默认 Agent 并把"是谁不见了"说清楚（§八：旧归属归入
                // 默认 Agent）。
                let fallback = config.agent.default_profile();
                tracing::warn!(
                    agent = %agent_id,
                    fallback = %fallback.id,
                    session = %session,
                    "这个会话绑的 Agent 已经不在配置里，按默认 Agent 受理"
                );
                fallback
            }
        };

        // 能力面：Profile 在**这份工具目录**里挑出来的那一份（§4 末）。名字写了、目录里
        // 没有的名字**不算数**，而且必须报出来——静默采纳一份写错的配置，等于让操作者
        // 以为某个工具给了而其实没给。
        let surface = komo_agent::surface_of(
            profile,
            &self.tool_names,
            &super::agent_config_file(&self.config),
        );

        // 工作目录：**会话自己的 `workdir` 优先**，其次 Profile 的 `workspace`，最后
        // `[paths] workspaces_dir`。
        //
        // 为什么会话优先：`workdir` 是操作者对**这一段对话**的显式选择（`POST /v1/sessions`
        // 写下的那一行，界面读的也是它，见 §八 那次收口），而 Profile 的 `workspace` 是
        // "没写的时候用哪个"——一个默认值不该盖掉一个显式选择。反过来（Profile 优先）
        // 会让界面显示的那个目录与执行实际用的那个目录重新裂成两份事实，正是 §八 已经
        // 修掉的那件事。
        //
        // 冻结的是**解析完的真实路径**：符号链接在这一刻解掉，Profile / 会话之后改过
        // 也不影响这一条 Run（`tools::paths::resolve` 与根用的是同一个口径）。
        let workspace = record
            .and_then(|record| record.workdir.clone())
            .map(PathBuf::from)
            .or_else(|| profile.workspace.clone())
            .unwrap_or_else(|| config.paths.workspaces_dir.clone());
        let workspace = paths::real_root(&workspace);

        // 模型：这次请求里显式给的最优先（那是操作者**这一次**的选择），其次 Profile 的
        // alias（进 `model_catalog` 解析），最后主模型。
        let model = model.unwrap_or_else(|| self.agent_model(config, profile));

        // 身份指令正文**进 `PayloadStore`**，快照里只留引用（§4.3）：存的是内容，不是
        // 文件路径——审批期间配置文件可能已经被改过，恢复出来的 Run 不能换一副面孔。
        // `PayloadStore` 是内容寻址的，同一段正文重复受理只写一次。
        let instructions_ref = match &profile.instructions {
            Some(text) => Some(
                PayloadStore::new(self.ledgers.paths_for(session))
                    .put(text.as_bytes())
                    .await?,
            ),
            None => None,
        };

        Ok(RunSnapshot {
            agent_id: profile.id.clone(),
            profile_revision: profile.revision(),
            model,
            workspace,
            surface,
            instructions_ref,
            memory_scope: profile.memory_scope.clone(),
        })
    }

    /// Profile 指的那个模型 alias，解析不出来就用主模型。
    ///
    /// **不因为一个写错的 alias 让输入失败**：那会把一次配置笔误变成一条丢掉的输入。
    /// 解析不出来时说清楚是哪个 alias，然后按主模型跑。
    fn agent_model(&self, config: &ConfigSnapshot, profile: &AgentProfile) -> ModelConfig {
        let Some(alias) = profile.model.as_deref() else {
            return config.model.clone();
        };
        match config.model_catalog.completion(alias) {
            Some(model) => model.clone(),
            None => {
                tracing::warn!(
                    file = %super::agent_config_file(&self.config).display(),
                    agent = %profile.id,
                    alias,
                    "`[agents]` 指的模型 alias 不在 model_catalog 里，这一次用主模型"
                );
                config.model.clone()
            }
        }
    }

    /// 提交一条输入：**HTTP 与聊天渠道共用的那一段**（§13.1）。
    ///
    /// **不接受新输入**（§8.10）：`state != active` 的会话在这里就挡下——`closing` 只是
    /// 不再收新活，`deleted` / `purged` 连内容都不该再长出来。挡在这里而不是挡在 HTTP 层，
    /// 是因为聊天渠道走的是同一条路（§13.1 最后一段）。
    pub async fn submit(
        self: &Arc<Self>,
        session: &SessionId,
        request_key: RequestKey,
        text: String,
        peer: Option<ChannelPeer>,
        model: Option<ModelConfig>,
    ) -> Result<SubmitRunResponse, GatewayError> {
        if let Some(state) = self.input_refusal(session).await? {
            return Err(GatewayError::InvalidRequest(format!(
                "这个会话是 {}，不接受新输入（§8.10）",
                state.as_str()
            )));
        }
        // **受理这一刻冻结身份与能力**（§4.3）：哪个 Agent（这个会话的归属）、哪一版
        // Profile、哪个模型、哪个工作目录、这次允许调用的工具、身份指令正文、记忆作用域。
        // 它随 `run.accepted` 落账，恢复时按它装配——审批期间 Profile 被改过也换不掉
        // 这一条 Run 的面孔。
        let config = self.snapshot();
        let record = komo_store::repos::session::get(&self.db, session).await?;
        let agent_id = self.owner_or_default(
            record
                .as_ref()
                .map(|record| record.agent_id.clone())
                .unwrap_or_default(),
        );
        let frozen = self
            .freeze_run(&config, &agent_id, session, record.as_ref(), model)
            .await?;
        let accepted = self
            .routed
            .accept_input(AcceptInput {
                session: session.clone(),
                request_key,
                text,
                source: PlanSource::Interactive {
                    session: session.clone(),
                },
                peer: peer.clone(),
                // 「新 Run 在 `accept_input` 时抓一份模型 / effort 快照」（§3 第 2 步）。
                // 这一份就是快照里那一个——同一个值写两处（`runs.model_snapshot` 与
                // `run.accepted` 的 `snapshot.model`），它们的缘分是同一个受理时刻，
                // 不是两次各自解析。
                model: frozen.model.clone(),
                workdir: None,
                // 交互输入不是委派：子 Run 只由 executor 在委托那一步受理（§4）。
                delegate: None,
                snapshot: Some(Box::new(frozen)),
                // 交互输入是真的对话，记忆提取照常（§9.3）；跳过只留给命令 Job（§10）。
                skip_memory: false,
                at: self.clock.now(),
            })
            .await?;
        // **同一 Session 后面的 Run 不越过前面的**（§8.4）：前一条没进终态时，这一条是
        // `waiting + dependency` 而不是 `queued`——它写得出在等谁，而不是一句"排队中"。
        //
        // 那是**受理那一笔事务**写的（store 的 `accept_input`：写 `queued` 之前先看同会话
        // 有没有更早的非终态 Run）。放在这里有一条谁也躲不开的窗口：受理与 suspend 之间调度
        // 器就能把它领走，而那一步是"越过前面那条"——正是次序规则要防的事。一处事实一个
        // 写者，所以这里只**如实地把它读回来**。
        self.waker().wake();
        // 有人看着它：终态回到来源会话，停下来等审批时把那条审批投出去（§11.4）。
        // 重发命中原 Run 时不再起第二个看客——登记表也会挡住，这里先省一次 spawn。
        if !accepted.deduplicated {
            self.watch_interactive_run(&accepted.session, &accepted.run, peer);
        }
        // 如实报它现在的状态：受理可能把它写成 `queued`，也可能写成
        // `waiting + dependency`（前面还有一条没跑完，§8.4）。
        let state = komo_store::repos::runs::get(&self.db, &accepted.run)
            .await?
            .map(|record| record.state)
            .unwrap_or(RunState::Queued);
        Ok(SubmitRunResponse {
            run: accepted.run,
            session: accepted.session,
            seq: accepted.seq,
            state,
            deduplicated: accepted.deduplicated,
        })
    }

    /// 前一条 Run 进终态了：把它后面等着的那些放回队列（§8.4）。
    ///
    /// 对账每一拍也会做（§8.9），但"下一条要白等一分钟"是操作者看得见的延迟——终态的
    /// 那一刻这里顺手放一次，代价是一条条件 UPDATE（判据与写入在同一句里，所以不会出现
    /// "判完了、写之前前置又被改回去"的窗口）。
    pub async fn release_dependents(&self) {
        match komo_store::repos::queue::release_satisfied_dependencies(&self.db, self.clock.now())
            .await
        {
            Ok(0) => {}
            Ok(released) => {
                tracing::info!(released, "放行了等到依赖的 Run");
                self.waker().wake();
            }
            // 放过它：这一拍的对账会补上（§8.9 的第三步）。
            Err(error) => tracing::warn!(%error, "放行依赖没做成，等下一拍对账"),
        }
    }

    /// 记一个审批决定，并把等着它的 Run 放回队列。
    ///
    /// **重复回答幂等**：已决定的返回原决定（§11.3）。已经决定过的那一次不再叫醒调度器
    /// ——它早就被叫醒过了。
    pub async fn decide_approval(
        &self,
        approval: &ApprovalId,
        approved: bool,
        scope: ApprovalScope,
        by: Option<PeerId>,
    ) -> Result<ApprovalDecisionResponse, GatewayError> {
        let record =
            self.approval_repo
                .get(approval)
                .await?
                .ok_or_else(|| GatewayError::NotFound {
                    what: format!("审批 {approval}"),
                })?;
        // 范围授权（§7.2 的第二、第三种）：先把**范围**落成一条授权，再把决定写下来并
        // 指着它。顺序是这一边的：一条写不下的授权只该让这次批准**降级成 `Once`**，
        // 不该让批准本身失败——操作者已经说了"可以"，而一条授权写不下去不改变这件事。
        //
        // 落不成范围的（计划里没有 Run、在交互 Run 里说 `cron`、Policy 根本没给这个
        // 范围）一律按 `Once` 处理，**不静默放宽**：一句说错的话不该变成一条比它宽的
        // 授权。
        let grant = if approved && scope != ApprovalScope::Once {
            self.mint_grant(&record, scope).await
        } else {
            None
        };
        let response = self
            .approvals
            .decide_with_grant(approval, approved, scope, by, grant)
            .await
            .map_err(GatewayError::from)?;

        if !response.already_decided {
            self.hub.publish_decision(
                &record.session,
                approval,
                response.decision.approved,
                komo_kernel::types::ids::Seq::ZERO,
            );
            // 界面回写：**决定先落账本，再回写界面**（§11.3）。渠道据此把卡片原地
            // 更新成"已批准 / 已拒绝 · 谁 · 何时"；投不出去不改变结论。
            let settled = komo_kernel::types::chat::Outbound::ApprovalSettled {
                approval: record.approval.clone(),
                short_id: record.short_id.clone(),
                approved: response.decision.approved,
                by: response
                    .decision
                    .by
                    .clone()
                    .unwrap_or_else(|| PeerId::new("operator")),
                at: response.decision.decided_at,
            };
            self.settle_everywhere(approval, settled).await;

            // §8.5 的**反向补写**：决定的权威是 state.db，审计事件随后补进 JSONL。这里
            // 只**排队**——补写由启动顺序的第 3 步与后台的周期补写做（§8.7）。
            //
            // 不在这里当场补写，是因为审批决定与 Run 的续跑是同一瞬间的事：决定之后
            // `wake_run` 会让这个 Run 立刻回到队列，而补写是另一次账本写入，插在中间只会
            // 让"决定"这条路径多一个可能失败的步骤——而它一个字都不该影响决定（§8.2：
            // 提交即 fsync，审批生效不依赖审计补写成功）。
            self.enqueue_audit(&record, &response.decision).await;

            if let Some(run) = &record.run {
                self.wake_run(run).await?;
            }
        }
        Ok(response)
    }

    /// 操作者答应的那个范围，落成一条授权。
    ///
    /// Policy 没在这条请求上给出这个范围就不给——`record.scopes` 是 §11.3 的
    /// 「只对 Policy 标记为可范围化的计划生效」在代码里的样子。
    async fn mint_grant(
        &self,
        record: &komo_kernel::protocol::http::ApprovalRecord,
        scope: ApprovalScope,
    ) -> Option<komo_kernel::policy::Grant> {
        if !record.scopes.contains(&scope) {
            tracing::info!(
                approval = %record.approval,
                ?scope,
                "Policy 没给这个范围，按本次调用处理"
            );
            return None;
        }
        let grant_scope = komo_kernel::policy::scope_for(&record.plan, scope)?;
        let grant = komo_kernel::policy::Grant {
            id: komo_kernel::types::ids::GrantId::new_at(self.clock.now()),
            approval: record.approval.clone(),
            scope: grant_scope,
            granted_at: self.clock.now(),
            // 授权跟着审批本身的有效期走（§8.4：过期不能按旧指令直接产生新的外部影响）。
            valid_until: record.valid_until,
            consumed: false,
            reason: record.reason.clone(),
        };
        match self.approval_repo.put_grant(grant).await {
            Ok(grant) => Some(grant),
            Err(error) => {
                tracing::warn!(%error, approval = %record.approval, "范围授权写不下，按本次调用处理");
                None
            }
        }
    }

    /// 把结论投回**当初投过这条审批的每一个会话**（§11.4）。
    ///
    /// 「决定过的请求不该还长着可点的按钮」（§11.3）——请求投到了来源会话**加上** home
    /// chat，那么结论也要回到这两处，而不是只回其中一处。目标从 `deliveries` 里查：那张
    /// 表记的正是"当初投到过哪儿"，用它就不必让决定这一侧再猜一遍路由。
    async fn settle_everywhere(
        &self,
        approval: &ApprovalId,
        settled: komo_kernel::types::chat::Outbound,
    ) {
        let mut targets = match self.deliveries.targets_for_approval(approval).await {
            Ok(targets) => targets,
            Err(error) => {
                tracing::warn!(%error, "查不到这条审批投过哪儿，只回写 home chat");
                Vec::new()
            }
        };
        // home chat 永远补一份：它可能是在请求之后才配上的。
        for target in self.notifier.home_targets() {
            if !targets.contains(&target.peer) {
                targets.push(target.peer);
            }
        }
        if targets.is_empty() {
            tracing::debug!(%approval, "没有可回写的会话，跳过界面回写");
            return;
        }
        for peer in targets {
            if let Err(error) = self
                .notifier
                .log()
                .deliver(
                    &komo_kernel::types::chat::DeliveryTarget::to_peer(peer.clone()),
                    settled.clone(),
                )
                .await
            {
                // 界面回写失败**不改变结论**（§11.3）。
                tracing::debug!(%error, %peer, "结论回写投不出去");
            }
        }
    }

    /// 把一条 `approval.decided` 排进 `control_outbox`（§8.5 的反向顺序第一步）。
    async fn enqueue_audit(
        &self,
        record: &ApprovalRecord,
        decision: &komo_kernel::protocol::http::ApprovalDecisionRecord,
    ) {
        let event_id = komo_kernel::types::ids::EventId::new_at(decision.decided_at);
        let payload = komo_kernel::events::EventPayload::ApprovalDecided(
            komo_kernel::events::ApprovalDecided {
                approval: record.approval.clone(),
                approved: decision.approved,
                scope: decision.scope,
                by: decision.by.clone(),
                decided_at: decision.decided_at,
                grant: decision.grant.clone(),
            },
        );
        let session = record.session.clone();
        let occurred_at = decision.decided_at;
        let result = self
            .db
            .with_write_retry(move |ex| {
                let (session, event_id, payload) =
                    (session.clone(), event_id.clone(), payload.clone());
                Box::pin(async move {
                    komo_store::repos::outbox::enqueue_in(
                        ex,
                        &session,
                        &event_id,
                        payload,
                        occurred_at,
                    )
                    .await
                }) as komo_store::db::BoxFuture<'_, Result<(), StoreError>>
            })
            .await;
        if let Err(error) = result {
            // 排不进去只是**审计**补不上，决定本身已经提交了（§8.2：提交即 fsync）。
            tracing::warn!(%error, approval = %record.approval, "审计事件排不进 outbox");
            return;
        }
        // 与请求那条对称：`approval.decided` 也走补写（§8.5 的反向顺序），而别的界面
        // （第二个 TUI、聊天里那张卡旁边的会话）正是靠它知道这条已经答过了。等周期就是
        // 让他们最多晚一分钟才看到结论。
        self.audit_wake.notify_one();
    }

    /// 把 `control_outbox` 里还没补写的审计事件追加到各自 Session 的 JSONL。
    ///
    /// 「重启后重发同一个 outbox 事件先按 `event_id` 去重，已写入就复用原事件位置」
    /// （§8.5）——幂等在 `Coordinator::append_audit` 里，这里只负责把它们递过去。返回
    /// 这次补写了几条。
    pub async fn drain_audit(&self) -> usize {
        let pending = match komo_store::repos::outbox::pending(&self.db, AUDIT_DRAIN_LIMIT).await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "读不出待补写的审计事件");
                return 0;
            }
        };
        let mut written = 0;
        for audit in pending {
            match self
                .turn_ledger
                .append_audit(
                    &audit.session,
                    &audit.event_id,
                    audit.payload,
                    audit.occurred_at,
                )
                .await
            {
                // `append_audit` 在同一个提交里标记 outbox 已交付，这里不必再写一次。
                Ok(_) => written += 1,
                Err(error) => {
                    tracing::warn!(%error, event = %audit.event_id, "审计事件补写不进去，留在 outbox");
                }
            }
        }
        if written > 0 {
            tracing::info!(written, "补写了审计事件");
        }
        written
    }

    /// 把卡在 `processing` 的记忆处理放回 `pending`（§9.3）。返回放回了几行。
    pub async fn requeue_memory_work(&self) -> Result<u64, GatewayError> {
        Ok(komo_store::repos::runs::requeue_stuck_memory_work(&self.db).await?)
    }

    /// 把**没人看着**的待处理 Intervention 补投到 home chat（§11.4 / §10 的兜底）。
    ///
    /// 三类共用这一条（"需要人判断"不该因为种类不同而有不同的到达率，§11.4）：屏幕上有人
    /// 时那条只弹在他面前（[`super::run_watch`] 的投递规则），可他要是关掉界面走人了，这一
    /// 条就成了"停在那里没人知道"——§10 明说不能这样。周期（`AUDIT_TICK`，与审计补写同一
    /// 拍）扫一遍清单：**没有任何 SSE 订阅者的 Session** 上还挂着的，投 home chat，投过一次
    /// 就不再投（[`GatewayState::start_delivering_intervention`]，按句柄记）。
    ///
    /// 已经在 chat 里投过的那条不会被重复投：那个名额在投出去时就被占了。
    pub async fn sweep_unseen_interventions(&self) {
        let pending = match self.interventions(&InterventionListQuery::default()).await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "读不出待处理清单，这一拍的兜底没做成");
                return;
            }
        };
        for summary in pending {
            if self.hub.viewers(&summary.session) > 0 {
                continue;
            }
            if !self.start_delivering_intervention(&summary.handle) {
                continue;
            }
            tracing::info!(
                handle = %summary.handle,
                kind = summary.kind.as_str(),
                session = %summary.session,
                "这一条没人看着，投到 home chat"
            );
            let failure = match summary.kind {
                // 审批那一份要按 §11.3 的渲染表呈现：短 ID、计划、原因、范围。
                InterventionKind::Approval => match self.approval_of(&summary).await {
                    Ok(Some(record)) => self
                        .notifier
                        .deliver_approval(None, komo_runtime::approvals::presentation(&record))
                        .await
                        .err()
                        .map(|error| error.to_string()),
                    Ok(None) => {
                        tracing::warn!(handle = %summary.handle, "这条审批读不出权威行，投不出去");
                        continue;
                    }
                    Err(error) => Some(error.to_string()),
                },
                // 另外两类是一条"哪里不清楚"的投递（§11.4：三类共用这条到达率）。
                InterventionKind::Verify | InterventionKind::Blocked => {
                    let Some(run) = summary.run.clone() else {
                        continue;
                    };
                    self.notifier
                        .deliver_home(komo_kernel::types::chat::Outbound::NeedsAttention {
                            session: summary.session.clone(),
                            run,
                            reason: summary.question.clone(),
                        })
                        .await
                        .err()
                        .map(|error| error.to_string())
                }
            };
            if let Some(error) = failure {
                tracing::warn!(%error, handle = %summary.handle, "这条投不出去：没人能回答它");
            }
        }
    }

    /// 一个审批句柄（短 ID）对应的权威行。
    async fn approval_of(
        &self,
        summary: &InterventionSummary,
    ) -> Result<Option<ApprovalRecord>, GatewayError> {
        let Some(short) = komo_kernel::types::ids::ShortId::parse(&summary.handle) else {
            return Ok(None);
        };
        Ok(self
            .approval_repo
            .find_latest_by_short_id(&short)
            .await?
            .filter(|record| record.decision.is_none()))
    }

    /// 把一个等着的 Run 放回队列并叫醒调度器（答复之后、`resume` 之后都走它）。
    pub async fn wake_run(&self, run: &RunId) -> Result<(), GatewayError> {
        self.recovery_store.requeue(run).await?;
        self.waker().wake();
        Ok(())
    }

    /// 取消一个 Run。
    ///
    /// 已终态的返回原状态而不是报错——重复取消是幂等的，而"取消一个已经完成的任务"不是
    /// 一个错误，是一句"它已经完成了"。
    pub async fn cancel_run(self: &Arc<Self>, run: &RunId) -> Result<RunState, GatewayError> {
        let record = komo_store::repos::runs::get(&self.db, run)
            .await?
            .ok_or_else(|| GatewayError::NotFound {
                what: format!("run {run}"),
            })?;
        if record.state.is_terminal() {
            return Ok(record.state);
        }
        self.finish_run(&record.session, run, RunEnd::Cancelled { by: None })
            .await
    }
}

/// §8.10 第 4 条要拦的那些路径：**komo 自己的状态**。
///
/// 「删除只有一条路……默认规则里工具对数据目录（`sessions/`、`state.db*`、`runtime/`、
/// `.env`）的写入是 `Deny`」——名单从**配置快照的 paths 与数据目录**算出来，不写死
/// `~/.komo`：`KOMO_HOME` 可以改，写死只会让那条规则在自定义目录上完全不命中。
///
/// 三个判决入口（工具执行、Python 核对、toolbox 的 HTTP 入口）共用它，所以它只有一处。
pub fn protected_paths(snapshot: &ConfigSnapshot, home: &std::path::Path) -> Vec<PathBuf> {
    vec![
        snapshot.paths.sessions_dir.clone(),
        snapshot.start_only.db_path.clone(),
        snapshot.paths.runtime_dir.clone(),
        home.join(".env"),
    ]
}

/// `sessions.origin` 里**主会话**的那个值。
///
/// 它是"这条会话从哪个入口来"，不是身份：身份是 `agent_id`（§4.2）。一个 Agent 的主会话
/// 与别的 Agent 的主会话在这一列上**长得一样**，区别在归属那一列。
///
/// 光秃秃的 `"home"` 也是**这次改造之前**那个全局主会话的取值——升级之后它仍然能被认成
/// 默认 Agent 的主会话（见 `GatewayState::main_session` 的候选分支），**旧行不重写**。
pub const HOME_ORIGIN: &str = "home";

/// 会话日志在库里记的那条**相对路径**（`sessions/<id>/events.jsonl`）。
///
/// 建一行会话时要带上它，而 `main_session` 是 Gateway 唯一自己建行的地方。这一串是
/// **store 的约定**（`Coordinator::open` 与 `queue` / `interventions` 里写的是同一条）；
/// 收在这一个函数里，是为了让"改口径"只有一处要跟。
fn session_log_path(session: &SessionId) -> String {
    format!("sessions/{session}/events.jsonl")
}

/// 一次补写最多处理多少条审计事件。
const AUDIT_DRAIN_LIMIT: usize = 128;

/// 记忆提取 / 冲突整理用的那个后端。
///
/// 测试注入了模型时就用那一个（脚本化的 driver 要同时答两种问）；否则按快照造一份自己
/// 的 [`RoutingLlm`]——它**不带 preamble**：记忆模型不读记忆，注入只属于对话那一路。
fn llm_for_memory(
    snapshot: &ConfigSnapshot,
    config: &ConfigHolder,
    caps: &EffortCapabilities,
    injected: Option<&Arc<dyn LlmClient>>,
) -> Arc<dyn LlmClient> {
    if let Some(client) = injected {
        return Arc::clone(client);
    }
    let factory = komo_runtime::llm::LlmFactory::new(config.secrets(), caps.clone())
        .with_chatgpt_credentials_path(komo_runtime::llm::codex_auth::credentials_path(
            &snapshot.start_only.data_dir,
        ));
    match komo_runtime::llm::RoutingLlm::from_snapshot(snapshot, factory) {
        Ok(routing) => Arc::new(routing),
        Err(error) => Arc::new(UnconfiguredLlm::new(error.to_string())),
    }
}

/// §9.5 的「省略 `dimensions` 时先探一次维度」。**这一步不在就绪路径上。**
///
/// 那一次探测是一次网络往返（`connect_embedding` 拿模型返回的维度，再固定成空间指纹），
/// 端点慢、或者在收连接但不回话，就要等满模型超时——本机实测 120s。它原本跑在
/// `GatewayState::assemble` 里、也就是绑监听与写发现文件**之前**，于是 `Gateway 就绪`
/// 与发现文件一起被推后整整一个超时：`komo gateway restart` 看上去就是卡住。
///
/// 顺序改成「先起服务，再探模型」：探到了走 `install_embeddings` 装上，检索立刻能用
/// 向量臂；探不到就一直空着，检索按 §9.4 **如实报降级**（与端点本来就不通时同一个行为，
/// 不是新的静默失败）。
fn spawn_embedding_probe(memories: Arc<MemoryManager>, config: Arc<ConfigHolder>) {
    tokio::spawn(async move {
        // 探测时读**当前**快照与凭证：热重载换掉的端点、`.env` 里新填的 key 都在这一
        // 刻生效，而不是启动那一刻的旧值。
        let snapshot = config.current();
        if !snapshot.memory.enabled {
            return;
        }
        let Some(embedding) = snapshot.memory.embedding.clone() else {
            return;
        };
        let transport = komo_runtime::llm::default_transport();
        match komo_runtime::embedding::connect_embedding(&embedding, &config.secrets(), transport)
            .await
        {
            Ok(client) => {
                tracing::info!(
                    model = %embedding.model.model,
                    endpoint = %embedding.model.base_url,
                    dimensions = client.space().dimensions,
                    "向量后端就绪"
                );
                memories.install_embeddings(client);
            }
            Err(error) => {
                tracing::warn!(%error, "向量后端造不出来：检索这一侧会如实报降级，不静默当成关键词模式");
            }
        }
    });
}

/// 按快照造判断后端（§9.4）。
///
/// 没开开关、没配后端、没凭证、造不出来——**一律 `None` + 一句日志**，绝不让 Gateway
/// 起不来：重排是加分项，没有它召回照样按关键词 + 向量的融合顺序给。
fn build_reranker(
    snapshot: &ConfigSnapshot,
    config: &ConfigHolder,
) -> Option<komo_runtime::memory::Reranker> {
    if !snapshot.memory.retrieval.rerank {
        return None;
    }
    match komo_runtime::typesafe::connect(
        &snapshot.typesafe,
        &config.secrets(),
        komo_runtime::llm::default_transport(),
    ) {
        Ok(backend) => Some(komo_runtime::memory::Reranker {
            backend,
            model: snapshot.typesafe.model.clone(),
        }),
        Err(error) => {
            tracing::warn!(%error, "判断后端建不出来：记忆召回按融合顺序给（§9.4）");
            None
        }
    }
}

/// 按当前快照造一份 skill 注册表（§5.6）。**这是唯一的那个来源**：系统提示里的目录行与
/// `skill://` 的挂载点都从它来（各存一份就会出现"提示里有、读不到"）。
///
/// `<workspace>` 取 Gateway 的 workspaces 目录，**不是** Session 的 `workdir`：这一块是
/// 启动快照（§5.6 要的就是提示前缀稳定），而 `workdir` 是逐个会话变的——按它算出来的是
/// 一份每段都可能不一样的前缀。
///
/// `shared_home` 是**注入**的（`~/.agents/skills`、`~/.claude/skills` 按它算）；`None`
/// 才回落到当前用户的家目录。现场读 `$HOME` 会让"提示里有哪些 skill"取决于这台机器上
/// 别的 agent 装过什么，测试之间因此会互相污染。
fn skills_registry(
    snapshot: &ConfigSnapshot,
    shared_home: Option<&std::path::Path>,
) -> komo_agent::skills::SkillRegistry {
    let home = match shared_home {
        Some(home) => Some(home.to_path_buf()),
        None => komo_runtime::config::user_home().ok(),
    };
    komo_agent::skills::SkillRegistry::from_snapshot(
        snapshot,
        Some(&snapshot.paths.workspaces_dir),
        home.as_deref(),
    )
}

/// 系统提示里那一块目录行（§5.6）：按**当前这套工具**从注册表现渲染。
///
/// 现渲染而不是存一段文本：存的字符串是第二份事实，它和注册表会各走各的——而这一块本来
/// 就是"这一刻有哪些 skill 能露面"的答案。
pub(crate) fn skills_block(
    registry: &komo_agent::skills::SkillRegistry,
    tool_names: &[String],
) -> String {
    registry
        .prompt_block(&komo_agent::skills::OfferContext::here(
            tool_names.iter().cloned(),
        ))
        .unwrap_or_default()
}

/// 按快照造模型后端；造不出来就退到 [`UnconfiguredLlm`]，**不让 Gateway 起不来**。
fn build_llm(
    snapshot: &ConfigSnapshot,
    config: &ConfigHolder,
    caps: &EffortCapabilities,
    preamble: &Arc<MemoryPreamble>,
) -> Arc<dyn LlmClient> {
    let factory = komo_runtime::llm::LlmFactory::new(config.secrets(), caps.clone())
        // 记忆注入的位置（§9.4）：正文由 MemoryManager 给，这里只是把它放进系统提示。
        .with_preamble(Arc::clone(preamble) as Arc<dyn komo_runtime::llm::SystemPreamble>)
        .with_chatgpt_credentials_path(komo_runtime::llm::codex_auth::credentials_path(
            &snapshot.start_only.data_dir,
        ));
    match komo_runtime::llm::RoutingLlm::from_snapshot(snapshot, factory) {
        Ok(routing) => Arc::new(routing),
        Err(error) => {
            tracing::warn!(%error, "模型后端造不出来：Gateway 照常启动，模型调用会报这个错");
            Arc::new(UnconfiguredLlm::new(error.to_string()))
        }
    }
}

/// `RepoError` / `StoreError` 到 Gateway 错误。
pub fn repo_error(error: RepoError) -> GatewayError {
    GatewayError::Repo(error)
}

#[allow(dead_code)]
fn assert_segment_types(_: &ResumedRound, _: &Segment, _: &CallEnv, _: HandlerError) {}
