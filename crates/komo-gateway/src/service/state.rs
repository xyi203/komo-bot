//! Gateway 的进程状态：一次装配，所有入口共用。
//!
//! HTTP 与聊天渠道走的是**同一段代码**（§13.1 最后一段），所以"提交一条输入"、"答一条
//! 介入"、"把等着的 Run 放回队列"这些动作都只在这里实现一次；`http` 与
//! `dispatcher` 各自只做自己的那层翻译。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::protocol::http::{InterventionListQuery, SubmitRunResponse};
use komo_kernel::traits::{
    Clock, CronRepo, EmbeddingClient, GatewayError, Ledger, LlmClient, MemoryRepo, RepoError,
    RunQueue, StoreError, ToolOutputStore,
};
use komo_kernel::types::agent::{AgentProfile, RunSnapshot};
use komo_kernel::types::chat::ChannelPeer;
use komo_kernel::types::delegate::DelegateContract;
use komo_kernel::types::ids::{ExecutorId, RequestKey, RunId, SessionId};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::status::{RunEnd, RunState, SessionState};
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::turn::{AcceptInput, LlmError, TurnRequest};
use komo_runtime::agent::handler::{AgentRunHandler, SegmentSource};
use komo_runtime::agent::{AgentLoop, Budget, ResumedRound, RetryBudget, Segment};
use komo_runtime::config::{ConfigHolder, EffortCapabilities};
use komo_runtime::executor::{CallEnv, ToolExecutor};
use komo_runtime::memory::{DbMemoryWork, LedgerEvents, MemoryManager, MemoryParts};
use komo_runtime::recovery::{RecoveryIndex, RecoveryScan, UnfinishedRun};
use komo_runtime::scheduler::{HandlerError, Scheduler, SchedulerConfig, Waker};
use komo_runtime::tools::paths;
use komo_store::{
    Db, PayloadStore, RecoveryStore, TursoCronRepo, TursoDeliveryRepo, TursoMemoryRepo,
    TursoRunQueue,
};
use time::OffsetDateTime;

use crate::channels::ChannelFactory;
use crate::channels::ChannelRegistry;
use crate::deliveries::DeliveryLog;
use crate::notifier::HomeNotifier;
use crate::sse::{EventHub, SharedHub};

use super::ledgers::{RoutedLedger, RoutedOutputs, SessionLedgers};
use super::segment::GatewaySegments;

/// 判提示缓存冷没冷用的时钟：生产就是 `clock` 本身，测试可以按数据目录拨快。
#[cfg(any(test, feature = "test-support"))]
fn cache_clock(home: &std::path::Path, clock: Arc<dyn Clock>) -> Arc<dyn Clock> {
    super::test_support::cache_clock(home, clock)
}

#[cfg(not(any(test, feature = "test-support")))]
fn cache_clock(_home: &std::path::Path, clock: Arc<dyn Clock>) -> Arc<dyn Clock> {
    clock
}

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
/// 它存在只是为了让 Gateway **起得来**——渠道、介入、恢复扫描与操作命令都不依赖模型，
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
///   [`InFlight`]——handler 开跑时落一行、返回时摘掉，它盖住的是"已经停在等待上、领取权
///   已交还，handler 却还没返回"那一段。领取已提交而 handler 还没开跑的那一段不在
///   `InFlight` 里，由扫描自己按 `claimed_by == 本实例` 跳过（`RecoveryScan::scan`）；
///   两道之外剩下的竞争由写回时的条件更新挡住（`RecoveryStore::requeue_if_unchanged`）。
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
                generation: run.generation,
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

    async fn reclaim_running(
        &self,
        executor: &ExecutorId,
        keep: &[ExecutorId],
    ) -> Result<u64, StoreError> {
        self.store.reclaim_running(executor, keep).await
    }

    async fn backfill(&self, run: &RunId) -> Result<(), StoreError> {
        self.store.backfill(run).await
    }

    async fn requeue(&self, seen: &UnfinishedRun) -> Result<bool, StoreError> {
        self.store
            .requeue_if_unchanged(&seen.run, &seen_row(seen))
            .await
    }

    async fn mark_needs_attention(
        &self,
        seen: &UnfinishedRun,
        reason: &str,
    ) -> Result<bool, StoreError> {
        self.store
            .mark_needs_attention_if_unchanged(&seen.run, reason, &seen_row(seen))
            .await
    }

    async fn hold_for_previous_executor(
        &self,
        seen: &UnfinishedRun,
        reason: &str,
    ) -> Result<bool, StoreError> {
        self.store
            .hold_for_previous_executor_if_unchanged(&seen.run, reason, &seen_row(seen))
            .await
    }
}

fn seen_row(run: &UnfinishedRun) -> komo_store::SeenRun {
    komo_store::SeenRun {
        state: run.state,
        wait: run.wait.clone(),
        claimed_by: run.claimed_by.clone(),
        generation: run.generation,
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
    /// 审计补写的叫醒铃。有审计事件刚排进 `control_outbox` 时按一下，周期（`AUDIT_TICK`）
    /// 只是兜底（§8.5 的补写顺序不变）。
    pub audit_wake: Arc<tokio::sync::Notify>,
    /// 一次 Run 的三个写入者共用的那个账本句柄（测试的故障注入口包的就是它）。审计补写
    /// 也走它——补写是账本写入的一种，没有理由绕过同一条缝。
    pub turn_ledger: Arc<dyn Ledger>,
    pub outputs: Arc<dyn ToolOutputStore>,
    pub queue: Arc<TursoRunQueue>,
    pub cron: Arc<dyn CronRepo>,
    pub memory: Arc<dyn MemoryRepo>,
    /// §9 的那一层：召回与注入、自动积累、向量索引与代次。
    pub memories: Arc<MemoryManager>,
    pub recovery_store: RecoveryStore,
    pub deliveries: Arc<TursoDeliveryRepo>,
    pub notifier: Arc<HomeNotifier>,
    pub channels: Arc<ChannelRegistry>,
    pub llm: Arc<SwappableLlm>,
    /// provider 报的上下文窗口（配置没写 `context_window` 的模型）。
    pub windows: super::windows::WindowCache,
    /// 注入了模型后端（测试）就不探：探测要走真网络。
    probe_windows: bool,
    /// 首轮跑完后用记忆模型起会话标题（[`super::titles`]）。注入模型后端的测试里关着。
    pub auto_title: bool,
    /// 上一条投到 home chat 的重载错误。同一条错误不重复投；装上之后清掉并说一声。
    pub reload_notice: Mutex<Option<String>>,
    pub scheduler: Arc<Scheduler>,
    /// 跑 Run 的那个 handler。调度器握着一份（`Arc<dyn RunHandler>`），这里再留一份具名
    /// 的：§7.5 的 `verify` 答复要直接调 [`AgentRunHandler::settle_by_operator`]，而那条
    /// 路不经过调度器。
    pub handler: Arc<AgentRunHandler>,
    /// 本进程此刻还在手里的 Run（§8.9 的存活判定）。
    pub in_flight: Arc<InFlight>,
    pub segments: Arc<GatewaySegments>,
    /// MCP 服务器的连接（`docs/mcp.md` §2.2）：受理 Run 前问它一次，登录后才连上的服务器的
    /// 工具从这里挂进执行器。
    pub mcp: Arc<komo_runtime::mcp::McpHub>,
    /// 执行器本身。留一份具名的 `Arc`，只为了 [`Self::wire_task_spawner`]——`dispatch` /
    /// `follow` 的接缝（`TaskSpawner`）要一份 `Arc<GatewayState>`，而执行器是在这份状态
    /// 存在**之前**造出来的，只能在这里回头接上（`docs/background-tasks.md`）。
    pub tool_executor: Arc<ToolExecutor>,
    /// 监听（`docs/monitor.md`）。与任务分发同一个接法：要一份 `Arc<GatewayState>`，
    /// 只能在这份状态造好之后接上（[`Self::wire_monitors`]）。停机与启动收尾要拿到它。
    pub monitors: std::sync::OnceLock<Arc<super::monitors::GatewayMonitors>>,
    /// §5.6 那份**活的** skill 注册表。与 [`GatewayState::segments`] 共用一个 `Arc`。
    ///
    /// 存的是注册表而不是渲染好的那一段文本：文本按用途现渲染（系统提示一处、`skill://`
    /// 的挂载点一处），**事实只有这一份**——存一份字符串就是第二份事实，它会和这一份
    /// 各走各的。注册表每次查询重扫目录（§5.6），所以"人改了 SKILL.md"不必重启。
    pub skills: Arc<std::sync::RwLock<komo_agent::skills::SkillRegistry>>,
    pub supervisor: Arc<super::channels::ChannelSupervisor>,
    /// Dispatcher。**构造之后才填**：它握着这份状态，反过来也要被渠道拿到。
    pub inbound: std::sync::OnceLock<Arc<dyn komo_kernel::traits::Inbound>>,
    /// OTLP 导出的状态，`run` 接上；`/healthz` 读。
    pub telemetry: std::sync::OnceLock<Arc<crate::telemetry::TelemetryStatus>>,
    /// 正在被人看着的 Run，以及已经投出去的 Intervention 句柄。
    ///
    /// **一个 Run 只有一个看的人**：Cron 的看客与交互的看客都走
    /// [`run_watch::watch`](super::run_watch::watch)，登记表是它们之间唯一的约定——
    /// 没有它，一条介入会被投两遍（去重键那一层管的是平台重投，管不到这个）。
    ///
    /// 投递表按**句柄**记（Run ID，§7.5 那张表）：两类共用一条到达率，"投过一次就不再投"
    /// 这条判据也只能有一个。
    pub watching: Mutex<std::collections::BTreeSet<RunId>>,
    pub interventions_delivered: Mutex<std::collections::BTreeSet<String>>,
    /// 模型请求的有界退避预算上限（§8.5）。
    pub max_retries: u32,
    /// 一段最多跑几轮模型（§6）；`None` = 不设。
    pub max_rounds: Option<u32>,
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
    /// 共享 agent 目录（`~/.agents/skills`）按哪个家目录算；
    /// `None` = 当前用户的家目录。**注入而不是现场读 `$HOME`**（§5.6）：否则提示里有哪些
    /// skill 取决于这台机器上别人装过什么。
    pub shared_home: Option<PathBuf>,
    pub tools: Vec<Arc<dyn komo_kernel::traits::Tool>>,
    /// codemode 的 worker（`docs/codemode.md`）；`None` = 没装配，`codemode` 也不在 `tools` 里。
    pub codemode: Option<komo_runtime::codemode::Worker>,
    /// MCP 连接，后台连着；`tools` 里没有 MCP 工具，受理 Run 时才挂进来。
    pub mcp: Arc<komo_runtime::mcp::McpHub>,
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
            codemode,
            mcp,
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

        // 记忆这一层要**先于**模型后端装好：注入是系统提示的一部分（Gateway 装配时拼进
        // `TurnRequest.system_prompt`，§13.2），模型后端只管发送，不再自己接一段。
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
            render: komo_agent::context::memory::render,
        }));
        if injected_embeddings.is_none() {
            spawn_embedding_probe(Arc::clone(&memories), Arc::clone(&config));
        }

        // 注入了模型后端（测试）就不起标题：多出来的那次请求会吃掉脚本里下一段的答复。
        let auto_title = llm.is_none();
        let probe_windows = llm.is_none();
        let llm = Arc::new(SwappableLlm::new(match llm {
            Some(client) => client,
            None => build_llm(&snapshot, &config, &caps),
        }));

        // 一次 Run 的三个账本写入者（执行器、AgentLoop、handler）共用这一个句柄。
        // W5 的故障注入口就在这里：没装过注入时它**就是** `routed` 本身（§14）。
        #[cfg(any(test, feature = "test-support"))]
        let turn_ledger =
            super::test_support::wrap_ledger(&home, Arc::clone(&routed) as Arc<dyn Ledger>);
        #[cfg(not(any(test, feature = "test-support")))]
        let turn_ledger = Arc::clone(&routed) as Arc<dyn Ledger>;

        // §5.6 的目录行：注册表**启动时按快照造一份**，那一块文本按用途现渲染（系统提示
        // 一处、`skill://` 的挂载点一处）——两处同源，不会有"提示里有、读不到"这种事。
        let skills = Arc::new(std::sync::RwLock::new(skills_registry(
            &snapshot,
            shared_home.as_deref(),
        )));
        let executor_tools = Arc::new(
            ToolExecutor::new(
                tools,
                Arc::clone(&turn_ledger),
                Arc::clone(&outputs),
                Arc::clone(&clock),
            )
            .with_codemode(codemode)
            .with_mcp(Arc::clone(&mcp)),
        );

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
            .with_clock(cache_clock(&home, Arc::clone(&clock)))
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
            cron,
            memory,
            memories,
            recovery_store,
            deliveries,
            reload_notice: Mutex::new(None),
            notifier,
            channels,
            llm,
            windows: super::windows::WindowCache::default(),
            probe_windows,
            auto_title,
            scheduler,
            handler,
            in_flight,
            segments,
            supervisor: Arc::new(super::channels::ChannelSupervisor::new(factories)),
            inbound: std::sync::OnceLock::new(),
            telemetry: std::sync::OnceLock::new(),
            watching: Mutex::new(std::collections::BTreeSet::new()),
            interventions_delivered: Mutex::new(std::collections::BTreeSet::new()),
            mcp,
            tool_executor: executor_tools,
            monitors: std::sync::OnceLock::new(),
            skills,
            max_retries,
            max_rounds,
        }))
    }

    /// 接上任务分发的实现（`docs/background-tasks.md`）：`GatewayTaskSpawner` 自己
    /// 要一份 `Arc<GatewayState>`（建会话、`submit`），只能在这份状态造出来**之后**才能
    /// 接上——与 `Dispatcher` / [`Self::inbound`] 同一个"先有整份状态才能自己引用自己"
    /// 的接法。`service::start` 在 [`Self::assemble`] 返回之后、渠道与 HTTP 起来之前调它。
    pub fn wire_task_spawner(self: &Arc<Self>) {
        let spawner = super::tasks::GatewayTaskSpawner::new(Arc::clone(self))
            as Arc<dyn komo_kernel::traits::TaskSpawner>;
        self.tool_executor.set_spawner(spawner);
    }

    /// 接上监听的实现（`docs/monitor.md`），与 [`Self::wire_task_spawner`] 同一个接法。
    /// 脚本按 `bash` 的规矩起：数据目录只读挂载的名单与 `bash` 工具同一份。
    pub fn wire_monitors(self: &Arc<Self>) {
        let snapshot = self.snapshot();
        let shell =
            komo_runtime::tools::BashTool::new().protecting(protected_paths(&snapshot, &self.home));
        let monitors = super::monitors::GatewayMonitors::new(
            Arc::clone(self),
            shell,
            &snapshot.paths.runtime_dir,
        );
        if self.monitors.set(Arc::clone(&monitors)).is_err() {
            tracing::warn!("监听已经接过一次，忽略这次重复设置");
            return;
        }
        self.tool_executor
            .set_monitors(monitors as Arc<dyn komo_kernel::traits::Monitors>);
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
    /// `[memory]` 或它指向的模型 / 凭证变了：按**当前**快照把开关、记忆模型与召回预算换进
    /// `MemoryManager`（§3）。不换的话报告里写着「变了」，提取却还在问旧模型。
    pub fn refresh_memory(&self) {
        let snapshot = self.snapshot();
        self.memories.install_config(
            &snapshot.memory,
            llm_for_memory(&snapshot, &self.config, &self.caps, None),
        );
        tracing::info!(model = %snapshot.memory.model.model, "记忆配置已按新快照换上");
    }

    pub fn refresh_reranker(&self) {
        self.memories
            .install_reranker(build_reranker(&self.snapshot(), &self.config));
    }

    /// 执行器此刻挂着的工具名。提示里的 skills 目录行按它门控（§5.6 的 `requires_tools:`）。
    /// 登录后才连上的 MCP 服务器会让它变长（`docs/mcp.md` §2.2）。
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_executor.tool_names()
    }

    /// 等登录的 MCP 服务器凭证变了就连上，把它的工具挂进执行器（`docs/mcp.md` §2.2，pi 在每个
    /// turn 开始时做同一件事）。受理 Run 之前调：新名字从这条 Run 起进能力面。
    ///
    /// 先等还在连的服务器（启动时、热重载刚加的，照 pi 的 `waitForDirectServers`），从开始连
    /// 时算至多 `STARTUP_WAIT`；到点没连上的这条 Run 先不等它。
    ///
    /// `pub` 是给测试看"受理时目录里有什么"：启动不等 MCP 握手，刚起来的目录里没有 MCP 工具。
    pub async fn refresh_mcp(&self) {
        self.mcp.wait_for_startup().await;
        let added = self.tool_executor.install(self.mcp.refresh().await);
        if !added.is_empty() {
            tracing::info!(
                tools = added.len(),
                "MCP 服务器连上了，工具从这条 Run 起可用"
            );
            // 热重载新加的服务器是后台连上的，到这里工具才挂上：skills 目录行的 `requires_tools`
            // 按新的工具名再门控一次，不等下一次重载。
            self.refresh_skills_prompt();
        }
    }

    /// 热重载换了 `[mcp.servers]`（照 pi 的 `mcp_servers_change`，`docs/mcp.md` §2.2）：删掉的、
    /// 改了的服务器先撤工具再断开，新加的后台连。**不等握手**：连上之后工具在下一条 Run 受理时
    /// 挂进来。交回每台服务器此刻的状态。
    ///
    /// 在跑的 Run 能力面不变：撤掉的名字再调时拿到一句"已从配置移除"，正在执行的那一次照常返回。
    pub fn reconcile_mcp(&self) -> Vec<komo_kernel::protocol::http::McpServerStatus> {
        let snapshot = self.snapshot();
        let change = self.mcp.reconcile(&snapshot.mcp, &self.config.secrets());
        let withdrawn = self.tool_executor.withdraw(change.withdrawn);
        let added = self.tool_executor.install(self.mcp.new_tools());
        tracing::info!(
            added = %change.added.join("、"),
            removed = %change.removed.join("、"),
            withdrawn = withdrawn.len(),
            installed = added.len(),
            "MCP 服务器按新配置增删了"
        );
        self.mcp.statuses()
    }

    /// 按**当前**快照重装 skill 注册表（§5.6）。
    ///
    /// 配置重载是唯一会动它的时刻：`paths.skill_dirs` 改了、人刚 `komo skills disable` 过，
    /// 之后受理的 Run 就该按新的来（注册表本身每次查询重扫目录，人改 `SKILL.md` 不必等重载）；
    /// 已经受理的 Run 用它冻结的那份目录。
    /// 换完渲染出来还是同一段就不吭声——**注册表照换**，因为目录集合本身就是新的事实。
    pub fn refresh_skills_prompt(&self) {
        let snapshot = self.snapshot();
        let tool_names = self.tool_names();
        let fresh = skills_registry(&snapshot, self.shared_home.as_deref());
        let mut current = self.skills.write().expect("skills 注册表");
        let before = skills_block(&current, &tool_names);
        let after = skills_block(&fresh, &tool_names);
        *current = fresh;
        if before == after {
            return;
        }
        tracing::info!(
            skills = after
                .as_deref()
                .map_or(0, |block| block.lines().count().saturating_sub(2)),
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
    /// Cron 没有来源会话，所以"需要人处理"这条路只能从这里投到 home chat——不盯着，
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
        self.interventions_delivered
            .lock()
            .expect("投递表")
            .insert(handle.to_string())
    }

    /// 盯一个交互 Run：终态回到来源会话，需要处理投来源会话 + home chat
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
        self.llm
            .swap(build_llm(&snapshot, &self.config, &self.caps));
        tracing::info!("模型后端已按新配置重建（正在跑的 Run 不受影响）");
    }

    /// 给配置里没写 `context_window` 的 completion 模型各起一次后台探测。
    ///
    /// 不在就绪路径上（与向量探测同一个教训）；已在探或已有结论的键不重复起，上次失败的
    /// 会重探。启动、热重载改了模型键、受理 Run 时都调它。
    pub fn spawn_window_probes(self: &Arc<Self>) {
        if !self.probe_windows {
            return;
        }
        let snapshot = self.snapshot();
        for (alias, model) in snapshot.model_catalog.completions() {
            let Some(config) = model.completion() else {
                continue;
            };
            if config.context_window.is_some() {
                continue;
            }
            let key = super::windows::key_of(config);
            if !self.windows.begin(&key) {
                continue;
            }
            let state = Arc::clone(self);
            let config = config.clone();
            let alias = alias.to_string();
            let factory = llm_factory(&snapshot, &self.config, &self.caps);
            tokio::spawn(async move {
                let result = komo_runtime::llm::probe_context_window(&factory, &config).await;
                let outcome = match result {
                    Ok(Some(window)) => super::windows::WindowState::Found(window),
                    Ok(None) => super::windows::WindowState::Missing,
                    Err(error) => {
                        tracing::warn!(alias, %error, "探测模型上下文窗口失败；可在 model.<alias>.context_window 里写明");
                        super::windows::WindowState::Failed(error.to_string())
                    }
                };
                state.windows.finish(key, outcome);
            });
        }
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

    /// 上一代执行实例停没停（§8.7）：按 `runtime/children` 里的子进程登记判。
    pub fn liveness(&self) -> komo_runtime::recovery::LockHolderLiveness {
        komo_runtime::recovery::LockHolderLiveness::holding_lock(
            self.executor.clone(),
            komo_runtime::recovery::ChildRegistry::new(
                self.snapshot().paths.runtime_dir.join("children"),
            ),
        )
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
            self.executor.clone(),
            Arc::new(self.liveness()),
        )
        // 「output.json 已完成但 JSONL 结果事件尚未写入 → 校验身份、计划及完成状态后补记
        // 结果；**不能只凭文件存在判断**」（§14 故障注入表）。没有这一口时恢复只能把
        // `started` 而无结果的调用交给工具核对，那条路对一次已经跑完的 `bash` 答不出
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
    ///
    /// 规则本身在 kernel（`AgentConfig::owner_or_default`）：没有冻结快照时的兜底身份
    /// （`context_sources::ambient_identity`）走的是同一条，这里只是按当前快照现读一次。
    fn owner_or_default(&self, agent_id: String) -> String {
        let config = self.snapshot();
        config.agent.owner_or_default(&agent_id).to_string()
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
    /// 工具表、工作目录、模型 alias。介入可能一小时之后才答复，而那时这些文件与配置都可能
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
        //
        // 目录先补一次：刚登录的 MCP 服务器这时连上，它的工具进这条 Run 的能力面。
        self.refresh_mcp().await;
        let mut surface = komo_agent::surface_of(
            profile,
            &self.tool_names(),
            &super::agent_config_file(&self.config),
        );
        // 任务会话里没有 `task`：深度只有一层，结果只交回一层（`docs/background-tasks.md`）。
        // `GatewayTaskSpawner::parent_of` 还留着第二道。
        if record.is_some_and(|record| record.kind == komo_store::models::SessionKind::Task) {
            surface = AgentSurface::new(
                surface
                    .names()
                    .iter()
                    .filter(|name| name.as_str() != komo_agent::TASK_TOOL)
                    .cloned(),
            );
        }

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
        let mut model = model.unwrap_or_else(|| self.agent_model(config, profile));
        // 配置没写窗口时用探到的；探测还没回来就是 None，快照冻结后照此回放。
        model.context_window = self.windows.effective(&model);

        // 身份指令正文**进 `PayloadStore`**，快照里只留引用（§4.3）：存的是内容，不是
        // 文件路径——等待期间配置文件可能已经被改过，恢复出来的 Run 不能换一副面孔。
        // `PayloadStore` 是内容寻址的，同一段正文重复受理只写一次。
        let payloads = PayloadStore::new(self.ledgers.paths_for(session));
        let instructions_ref = match &profile.instructions {
            Some(text) => Some(payloads.put(text.as_bytes()).await?),
            None => None,
        };
        // 项目规范文件（`AGENTS.md` 等）同一个理由：从**解析过的**工作目录往上找，正文进
        // `PayloadStore`。Run 停着的时候文件被改了，恢复出来的那一段仍是受理时那一版。
        // 同一会话里系统提示沿用上一条主 Run 那一份；现场找到的不一样，就另存一份，回放
        // 时补在这条 Run 的任务后面——改动从这一条起生效，前缀缓存不作废。
        let found = super::project_files::discover(&workspace);
        let current = super::project_files::freeze(&payloads, &found).await?;
        let events = super::compaction::read_events(self.routed.as_ref(), session).await?;
        let (project_files_ref, project_files_update_ref) =
            match super::project_files::pinned(&events, &profile.id, &workspace) {
                Some(pinned) if pinned != current => {
                    (pinned, Some(payloads.put_json(&found, None).await?))
                }
                Some(pinned) => (pinned, None),
                None => (current, None),
            };
        // skills 目录同一个理由，冻结的是**渲染好的正文**：续跑那一段的系统提示要与受理时
        // 逐字相同（provider 的前缀缓存从系统提示起算），注册表之后被重载、`SKILL.md` 被改，
        // 从下一条 Run 起生效。门控按这条 Run 的能力面。
        let skills = skills_block(&self.skills.read().expect("skills 注册表"), surface.names());
        let skills_ref = match skills {
            Some(block) => Some(payloads.put(block.as_bytes()).await?),
            None => None,
        };

        Ok(RunSnapshot {
            agent_id: profile.id.clone(),
            profile_revision: profile.revision(),
            model,
            workspace,
            surface,
            instructions_ref,
            project_files_ref,
            project_files_update_ref,
            skills_ref,
            memory_scope: profile.memory_scope.clone(),
            projection: Some(config.execution.projection()),
            direct_command: None,
            contract: None,
        })
    }

    /// 这个 Agent 现在的默认模型（当前配置）：`task` 只给了 effort 时，按它校验档位。
    pub(crate) fn default_model_of(&self, agent_id: &str) -> ModelConfig {
        let config = self.snapshot();
        let profile = config.agent.profile_of(agent_id);
        self.agent_model(&config, profile)
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
        self.accept(session, request_key, text, peer, model, None, None, false)
            .await
    }

    /// 同 [`Self::submit`]，再带上 `task` 的结果契约（冻结进这条 Run 的快照，§4）。
    pub async fn submit_task(
        self: &Arc<Self>,
        session: &SessionId,
        request_key: RequestKey,
        text: String,
        peer: Option<ChannelPeer>,
        model: Option<ModelConfig>,
        contract: Option<DelegateContract>,
    ) -> Result<SubmitRunResponse, GatewayError> {
        self.accept(
            session,
            request_key,
            text,
            peer,
            model,
            None,
            contract,
            false,
        )
        .await
    }

    /// 同 [`Self::submit`]，但这句话要求**插话**（§6）：会话里有 Run 正在跑时，由它在下一次
    /// 请求模型之前并进去，而不是排到它后面。HTTP 调它（TUI 的 Enter，
    /// `SubmitRunRequest::interject`，没有对端）；`task` 交回的结果也走它，带着任务 Run 的
    /// 来源对端——对端与宿主不同就不并（§6）。聊天渠道的消息照旧排队。
    pub async fn submit_interjecting(
        self: &Arc<Self>,
        session: &SessionId,
        request_key: RequestKey,
        text: String,
        peer: Option<ChannelPeer>,
        model: Option<ModelConfig>,
    ) -> Result<SubmitRunResponse, GatewayError> {
        self.accept(session, request_key, text, peer, model, None, None, true)
            .await
    }

    /// TUI 的 `!`：受理一条**不经模型**直接跑的 shell 命令。
    ///
    /// 走的是同一条受理路：会话状态检查、冻结身份、次序规则、看客都一样。区别只在快照里
    /// 多一格 `direct_command`——装配那一段按它换上 `CommandDriver`（与命令 Job 同一个
    /// 驱动），于是命令照样经 `bash` 工具与执行器，调用与结果记进会话，下一轮模型看得见。
    /// 不做记忆提取：没有对话，提取本身还要发一次模型请求。
    ///
    /// 只有 HTTP 调它（`SubmitRunRequest::shell`）；聊天渠道不接这条路。
    pub async fn submit_shell(
        self: &Arc<Self>,
        session: &SessionId,
        request_key: RequestKey,
        command: String,
    ) -> Result<SubmitRunResponse, GatewayError> {
        let command = command.trim().to_string();
        if command.is_empty() {
            return Err(GatewayError::InvalidRequest(
                "`!` 后面要跟一条命令，例如 `!ls -la`".into(),
            ));
        }
        let text = format!("!{command}");
        self.accept(
            session,
            request_key,
            text,
            None,
            None,
            Some(command),
            None,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn accept(
        self: &Arc<Self>,
        session: &SessionId,
        request_key: RequestKey,
        text: String,
        peer: Option<ChannelPeer>,
        model: Option<ModelConfig>,
        direct_command: Option<String>,
        contract: Option<DelegateContract>,
        interject: bool,
    ) -> Result<SubmitRunResponse, GatewayError> {
        if let Some(state) = self.input_refusal(session).await? {
            return Err(GatewayError::InvalidRequest(format!(
                "这个会话是 {}，不接受新输入（§8.10）",
                state.as_str()
            )));
        }
        // **受理这一刻冻结身份与能力**（§4.3）：哪个 Agent（这个会话的归属）、哪一版
        // Profile、哪个模型、哪个工作目录、这次允许调用的工具、身份指令与项目规范的正文、
        // 记忆作用域。
        // 它随 `run.accepted` 落账，恢复时按它装配——等待期间 Profile 被改过也换不掉
        // 这一条 Run 的面孔。
        let config = self.snapshot();
        let record = komo_store::repos::session::get(&self.db, session).await?;
        let agent_id = self.owner_or_default(
            record
                .as_ref()
                .map(|record| record.agent_id.clone())
                .unwrap_or_default(),
        );
        let mut frozen = self
            .freeze_run(&config, &agent_id, session, record.as_ref(), model)
            .await?;
        // 这条 Run 的窗口若还没探到（或上次失败），顺手补一次后台探测，不等它。
        self.spawn_window_probes();
        let skip_memory = direct_command.is_some();
        if direct_command.is_some() && !frozen.surface.allows("bash") {
            return Err(GatewayError::InvalidRequest(format!(
                "这个会话的 Agent（{}）没有 bash 工具，`!` 用不了",
                frozen.agent_id
            )));
        }
        frozen.direct_command = direct_command;
        frozen.contract = contract;
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
                // 交互输入是真的对话，记忆提取照常（§9.3）；跳过只留给命令 Job（§10）
                // 与 `!` 直接执行的命令。
                skip_memory,
                interject,
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
        // 有人看着它：终态回到来源会话，停下来要人处理时把那条介入投出去（§11.4）。
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

    /// 把 `control_outbox` 里还没补写的审计事件追加到各自 Session 的 JSONL。
    ///
    /// 「重启后重发同一个 outbox 事件先按 `event_id` 去重，已写入就复用原事件位置」
    /// （§8.5）——幂等在 `Coordinator::append_audit` 里，这里只负责把它们递过去。返回
    /// 这次补写了几条。
    ///
    /// 所属会话已经 `purged` 的行**不补写，直接标成已交付**（`seq` 记 0：不在任何 JSONL 里）。
    /// 审计副本要落的那份 JSONL 按设计已经删了（§8.10 第 3 条），`sessions.state` 才是权威；
    /// 硬去写只会每拍报一次"已提交范围缺失"、永远留在 outbox 里。实测（2026-10-15）：
    /// `purge` 排完 `session.deleted` / `session.purged` 就删目录，补写器跑在删之后的那 7 个
    /// 会话留下 17 行，每分钟各报一次 WARN 报了三周；攒到 [`AUDIT_DRAIN_LIMIT`] 行还会把
    /// 后面所有新的审计事件挡在外面——§8.10 说的"不能让一条旧行卡死补发循环"。
    pub async fn drain_audit(&self) -> usize {
        let pending = match komo_store::repos::outbox::pending(&self.db, AUDIT_DRAIN_LIMIT).await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "读不出待补写的审计事件");
                return 0;
            }
        };
        let mut written = 0;
        let mut discarded = 0;
        for audit in pending {
            if self.session_purged(&audit.session).await {
                match self.discard_audit(&audit.event_id).await {
                    Ok(()) => discarded += 1,
                    Err(error) => tracing::warn!(
                        %error,
                        event = %audit.event_id,
                        "会话已 purged，审计事件丢弃不了，留在 outbox"
                    ),
                }
                continue;
            }
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
        if discarded > 0 {
            tracing::info!(discarded, "所属会话已 purged，审计事件不再补写，标成已交付");
        }
        written
    }

    /// 这个会话是不是已经 `purged`。读不出来按"不是"走：照常补写，失败了下一拍再看。
    async fn session_purged(&self, session: &SessionId) -> bool {
        matches!(
            komo_store::repos::session::state(&self.db, session).await,
            Ok(Some(SessionState::Purged))
        )
    }

    /// 不补写，只把这一行标成已交付。
    async fn discard_audit(
        &self,
        event_id: &komo_kernel::types::ids::EventId,
    ) -> Result<(), StoreError> {
        let event_id = event_id.clone();
        self.db
            .with_write_retry(move |ex| {
                let event_id = event_id.clone();
                Box::pin(async move {
                    komo_store::repos::outbox::mark_delivered_in(ex, &event_id).await
                }) as komo_store::db::BoxFuture<'_, Result<(), StoreError>>
            })
            .await
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
            let key = crate::notifier::intervention_key(&summary);
            if !self.start_delivering_intervention(&key) {
                continue;
            }
            tracing::info!(
                handle = %summary.handle,
                kind = summary.kind.as_str(),
                session = %summary.session,
                "这一条没人看着，投到 home chat"
            );
            // 两类都是一条"哪里不清楚"的投递（§11.4：共用这条到达率）。
            let Some(run) = summary.run.clone() else {
                continue;
            };
            let failure = self
                .notifier
                .deliver_home_once(
                    komo_kernel::types::chat::Outbound::NeedsAttention {
                        session: summary.session.clone(),
                        run,
                        reason: summary.question.clone(),
                    },
                    &key,
                )
                .await
                .err()
                .map(|error| error.to_string());
            if let Some(error) = failure {
                tracing::warn!(%error, handle = %summary.handle, "这条投不出去：没人能回答它");
            }
        }
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
/// 各处判决入口（工具装配、监听脚本）共用它，所以它只有一处。
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

/// 模型后端与窗口探测共用的工厂：凭证、effort 能力表、ChatGPT 凭证路径只在这一处接。
fn llm_factory(
    snapshot: &ConfigSnapshot,
    config: &ConfigHolder,
    caps: &EffortCapabilities,
) -> komo_runtime::llm::LlmFactory {
    komo_runtime::llm::LlmFactory::new(config.secrets(), caps.clone())
        .with_chatgpt_credentials_path(komo_runtime::llm::codex_auth::credentials_path(
            &snapshot.start_only.data_dir,
        ))
}

/// 记忆提取 / 冲突整理用的那个后端。
///
/// 测试注入了模型时就用那一个（脚本化的 driver 要同时答两种问）；否则按快照造一份自己
/// 的 [`RoutingLlm`]。记忆段只拼进对话那一路的 `TurnRequest.system_prompt`
/// （§13.2）——记忆模型自己的请求不经过这一步，它本就不该读到记忆注入。
fn llm_for_memory(
    snapshot: &ConfigSnapshot,
    config: &ConfigHolder,
    caps: &EffortCapabilities,
    injected: Option<&Arc<dyn LlmClient>>,
) -> Arc<dyn LlmClient> {
    if let Some(client) = injected {
        return Arc::clone(client);
    }
    let factory = llm_factory(snapshot, config, caps);
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
                // 后端就绪就补齐索引（§9.5）：每次启动都走这里，所以重启天然接上未完成
                // 的代次；欠谁由仓储现算，不靠进程内记着。
                if let Err(error) = memories.schedule_index() {
                    tracing::warn!(%error, "索引补齐没提交上，先只靠关键词命中");
                }
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
/// `shared_home` 是**注入**的（`~/.agents/skills` 按它算）；`None`
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
    komo_agent::skills::SkillRegistry::from_snapshot(snapshot, home.as_deref())
}

/// 系统提示里那一块目录行（§5.6）：按**这套工具**从注册表现渲染。`None` = 没有一条能
/// 露面，或者没挂能读文件的工具。
///
/// 受理 Run 时渲染一次、冻结进 `RunSnapshot.skills_ref`（`GatewayState::freeze_run`），
/// 之后这条 Run 的每一段都按那份正文装配；没有快照的兜底身份才每段现渲染。
pub(crate) fn skills_block(
    registry: &komo_agent::skills::SkillRegistry,
    tool_names: &[String],
) -> Option<String> {
    registry.prompt_block(&komo_agent::skills::OfferContext::here(
        tool_names.iter().cloned(),
    ))
}

/// 按快照造模型后端；造不出来就退到 [`UnconfiguredLlm`]，**不让 Gateway 起不来**。
fn build_llm(
    snapshot: &ConfigSnapshot,
    config: &ConfigHolder,
    caps: &EffortCapabilities,
) -> Arc<dyn LlmClient> {
    let factory = llm_factory(snapshot, config, caps);
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
