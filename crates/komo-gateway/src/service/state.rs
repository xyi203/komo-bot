//! Gateway 的进程状态：一次装配，所有入口共用。
//!
//! HTTP 与聊天渠道走的是**同一段代码**（§13.1 最后一段），所以"提交一条输入"、"记一个
//! 审批决定"、"把等着的 Run 放回队列"这些动作都只在这里实现一次；`http` 与
//! `dispatcher` 各自只做自己的那层翻译。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::protocol::http::{ApprovalDecisionResponse, ApprovalRecord, SubmitRunResponse};
use komo_kernel::traits::{
    ApprovalRepo, Clock, CronRepo, EmbeddingClient, GatewayError, Ledger, LlmClient, MemoryRepo,
    RepoError, RunQueue, StoreError, ToolOutputStore,
};
use komo_kernel::types::chat::{ApprovalScope, ChannelPeer, PeerId};
use komo_kernel::types::ids::{ApprovalId, ExecutorId, RequestKey, RunId, SessionId};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::status::RunStatus;
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
use komo_store::{
    Db, RecoveryStore, TursoApprovalRepo, TursoCronRepo, TursoDeliveryRepo, TursoMemoryRepo,
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
/// 「`UnfinishedRun.retry.exhausted` 由拿着预算的那一层填」——预算在这里
/// （[`GatewayState::max_retries`]），所以填在这里。
#[derive(Debug)]
pub struct RecoveryIndexOf {
    store: RecoveryStore,
    max_retries: u32,
}

impl RecoveryIndexOf {
    pub fn new(store: RecoveryStore, max_retries: u32) -> Self {
        RecoveryIndexOf { store, max_retries }
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
            .map(|run| UnfinishedRun {
                run: run.run,
                session: run.session,
                status: run.status,
                claimed_by: run.claimed_by,
                retry: run.retry.map(|mut retry| {
                    retry.exhausted = retry.attempts >= self.max_retries;
                    retry
                }),
                result_delivered: run.result_delivered,
            })
            .collect())
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

/// 一次装配出来的 Gateway。
pub struct GatewayState {
    pub home: PathBuf,
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
    pub segments: Arc<GatewaySegments>,
    pub supervisor: Arc<super::channels::ChannelSupervisor>,
    /// Dispatcher。**构造之后才填**：它握着这份状态，反过来也要被渠道拿到。
    pub inbound: std::sync::OnceLock<Arc<dyn komo_kernel::traits::Inbound>>,
    /// 正在被人看着的 Run，以及已经投出去的审批。
    ///
    /// **一个 Run 只有一个看的人**：Cron 的看客与交互的看客都走
    /// [`run_watch::watch`](super::run_watch::watch)，登记表是它们之间唯一的约定——
    /// 没有它，一条审批会被投两遍（去重键那一层管的是平台重投，管不到这个）。
    pub watching: Mutex<std::collections::BTreeSet<RunId>>,
    pub approvals_delivered: Mutex<std::collections::BTreeSet<ApprovalId>>,
    /// 会话的工作目录。
    ///
    // TODO(decide: `sessions.workdir` 这一列 store 只在建行时写（`ensure_in` 永远写
    // `None`），公开面上没有改它的函数，而 `POST /v1/sessions` 收 `workdir`。这里先记在
    // 进程里——重启后会话回到"用 workspaces/"。要持久化需要 store 加一个
    // `repos::session::set_workdir_in`，见报告。)
    pub workdirs: Mutex<std::collections::BTreeMap<SessionId, String>>,
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
        let routed = Arc::new(RoutedLedger::new(Arc::clone(&ledgers), db.clone()));
        let outputs: Arc<dyn ToolOutputStore> = Arc::new(RoutedOutputs::new(
            Arc::clone(&ledgers),
            Arc::clone(&routed),
        ));

        let queue = Arc::new(TursoRunQueue::new(db.clone()));
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
        let embeddings = match embeddings {
            Some(client) => Some(client),
            None => build_embeddings(&snapshot, &config).await,
        };
        let memories = Arc::new(MemoryManager::new(MemoryParts {
            config: snapshot.memory.clone(),
            repo: Arc::clone(&memory),
            catalog: Arc::new(TursoMemoryRepo::new(db.clone())),
            embeddings,
            llm: Arc::clone(&llm_for_memory(&snapshot, &config, &caps, llm.as_ref())),
            events: Arc::new(LedgerEvents(Arc::clone(&routed) as Arc<dyn Ledger>)),
            work: Arc::new(DbMemoryWork::new(db.clone(), Arc::clone(&clock))),
            clock: Arc::clone(&clock),
        }));
        let preamble = Arc::new(MemoryPreamble::new(Arc::clone(&memories)));

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

        let executor_tools = Arc::new(ToolExecutor::new(
            tools,
            Arc::clone(&turn_ledger),
            Arc::clone(&outputs),
            ApprovalGate::new(Arc::clone(&approval_repo), Arc::clone(&clock)),
            PolicyEngine::from_rules(snapshot.policy.clone()),
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
            // Cron Run 用它那个 Job 的执行预算（§10）。
            .with_cron(Arc::clone(&cron))
            // 每一段装配时按当前用户输入召回一次，并把用到的条目记进
            // `TurnRequest::memories`（§9.7 的审计证据）。
            .with_memories(
                Arc::clone(&memories),
                komo_store::CheckpointStore::new(db.clone()),
            ),
        );

        let handler = Arc::new(AgentRunHandler::new(
            agent,
            Arc::clone(&executor_tools),
            Arc::clone(&turn_ledger),
            Arc::clone(&segments) as Arc<dyn SegmentSource>,
            executor.clone(),
        ));

        let scheduler = Arc::new(Scheduler::new(
            Arc::clone(&queue) as Arc<dyn RunQueue>,
            handler,
            executor.clone(),
            SchedulerConfig::default(),
        ));

        let recovery_store = RecoveryStore::new(db.clone(), sessions_root);

        Ok(Arc::new(GatewayState {
            home,
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
            segments,
            supervisor: Arc::new(super::channels::ChannelSupervisor::new(factories)),
            inbound: std::sync::OnceLock::new(),
            watching: Mutex::new(std::collections::BTreeSet::new()),
            approvals_delivered: Mutex::new(std::collections::BTreeSet::new()),
            workdirs: Mutex::new(std::collections::BTreeMap::new()),
            max_retries,
            max_rounds,
        }))
    }

    pub fn snapshot(&self) -> Arc<ConfigSnapshot> {
        self.config.current()
    }

    /// 记下一个会话的工作目录。
    pub fn remember_workdir(&self, session: &SessionId, workdir: &str) {
        self.workdirs
            .lock()
            .expect("工作目录表")
            .insert(session.clone(), workdir.to_string());
    }

    pub fn workdir_of(&self, session: &SessionId) -> Option<String> {
        self.workdirs
            .lock()
            .expect("工作目录表")
            .get(session)
            .cloned()
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

    /// 这条审批还没被投出去过。答 `false` = 投过了（同一条审批只问一次人）。
    pub fn start_delivering_approval(&self, approval: &ApprovalId) -> bool {
        self.approvals_delivered
            .lock()
            .expect("审批投递表")
            .insert(approval.clone())
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

    /// 恢复扫描（§8.7）。
    pub fn recovery(&self) -> RecoveryScan {
        RecoveryScan::new(
            Arc::clone(&self.routed) as Arc<dyn Ledger>,
            Arc::new(RecoveryIndexOf::new(
                self.recovery_store.clone(),
                self.max_retries,
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

    /// 操作者那**一个**常驻会话（§11.2 的 home session）。
    ///
    /// 「操作者的私聊——飞书 DM、Telegram DM、WeChat、TUI——全部落到同一个 home
    /// session」：它是 `sessions` 表里 `origin = "home"` 的那一行，第一次问的时候铸
    /// 出来，此后一直是它。**不另存一个文件**：会话表本来就答得出这个问题。
    pub async fn home_session(&self) -> Result<SessionId, GatewayError> {
        if let Some(existing) = self.find_home_session().await? {
            return Ok(existing);
        }
        let session = SessionId::new_at(self.clock.now());
        self.ledgers.open(&session, HOME_ORIGIN).await?;
        // 开的过程里可能有别人也开了一个；以表里最早的那一行为准。
        Ok(self.find_home_session().await?.unwrap_or(session))
    }

    async fn find_home_session(&self) -> Result<Option<SessionId>, GatewayError> {
        let mut homes: Vec<SessionId> = komo_store::repos::session::list(&self.db)
            .await?
            .into_iter()
            .filter(|record| record.origin == HOME_ORIGIN)
            .map(|record| record.session)
            .collect();
        homes.sort();
        Ok(homes.into_iter().next())
    }

    /// 提交一条输入：**HTTP 与聊天渠道共用的那一段**（§13.1）。
    pub async fn submit(
        self: &Arc<Self>,
        session: &SessionId,
        request_key: RequestKey,
        text: String,
        peer: Option<ChannelPeer>,
        model: Option<ModelConfig>,
    ) -> Result<SubmitRunResponse, GatewayError> {
        let snapshot = self.snapshot();
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
                model: model.unwrap_or_else(|| snapshot.model.clone()),
                workdir: None,
                at: self.clock.now(),
            })
            .await?;
        self.waker().wake();
        // 有人看着它：终态回到来源会话，停下来等审批时把那条审批投出去（§11.4）。
        // 重发命中原 Run 时不再起第二个看客——登记表也会挡住，这里先省一次 spawn。
        if !accepted.deduplicated {
            self.watch_interactive_run(&accepted.session, &accepted.run, peer);
        }
        Ok(SubmitRunResponse {
            run: accepted.run,
            session: accepted.session,
            seq: accepted.seq,
            status: RunStatus::Queued,
            deduplicated: accepted.deduplicated,
        })
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
        }
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

    /// 把一个等着的 Run 放回队列并叫醒调度器（`/approve` 之后、`resume` 之后都走它）。
    pub async fn wake_run(&self, run: &RunId) -> Result<(), GatewayError> {
        self.recovery_store.requeue(run).await?;
        self.waker().wake();
        Ok(())
    }

    /// 取消一个 Run。
    ///
    /// 已终态的返回原状态而不是报错——重复取消是幂等的，而"取消一个已经完成的任务"不是
    /// 一个错误，是一句"它已经完成了"。
    pub async fn cancel_run(&self, run: &RunId) -> Result<RunStatus, GatewayError> {
        let record = komo_store::repos::runs::get(&self.db, run)
            .await?
            .ok_or_else(|| GatewayError::NotFound {
                what: format!("run {run}"),
            })?;
        if record.status.is_terminal() {
            return Ok(record.status);
        }
        self.segments.cancel(run);
        let session = record.session.clone();
        let entry = self.ledgers.open(&session, "agent").await?;
        entry
            .ledger
            .complete(
                run,
                komo_kernel::types::status::RunEnd::Cancelled { by: None },
            )
            .await?;
        Ok(RunStatus::Cancelled)
    }
}

/// `sessions.origin` 里 home session 的那个值。
pub const HOME_ORIGIN: &str = "home";

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
    let factory = komo_runtime::llm::LlmFactory::new(config.secrets(), caps.clone());
    match komo_runtime::llm::RoutingLlm::from_snapshot(snapshot, factory) {
        Ok(routing) => Arc::new(routing),
        Err(error) => Arc::new(UnconfiguredLlm::new(error.to_string())),
    }
}

/// 按 `memory.embedding` alias 解析后的完整配置造向量后端。
///
/// **端点这一刻不通不该让 Gateway 起不来**：没有向量客户端时 hybrid 会如实降级并说明
/// 原因（§9.4），这比一个起不来的进程强。配了 `dimensions` 就不碰网络；省略时要探一次
/// （§9.5），那一次探测失败就是这里唯一会用到网络的地方。
async fn build_embeddings(
    snapshot: &ConfigSnapshot,
    config: &ConfigHolder,
) -> Option<Arc<dyn EmbeddingClient>> {
    if !snapshot.memory.enabled {
        return None;
    }
    let embedding = snapshot.memory.embedding.as_ref()?;
    let transport = komo_runtime::llm::default_transport();
    match komo_runtime::embedding::connect_embedding(embedding, &config.secrets(), transport).await
    {
        Ok(client) => Some(client),
        Err(error) => {
            tracing::warn!(%error, "向量后端造不出来：检索这一侧会如实报降级，不静默当成关键词模式");
            None
        }
    }
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
        .with_preamble(Arc::clone(preamble) as Arc<dyn komo_runtime::llm::SystemPreamble>);
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
