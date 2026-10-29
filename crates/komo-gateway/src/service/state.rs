//! Gateway 的进程状态：一次装配，所有入口共用。
//!
//! HTTP 与聊天渠道走的是**同一段代码**（§13.1 最后一段），所以"提交一条输入"、"答一条
//! 介入"、"把等着的 Run 放回队列"这些动作都只在这里实现一次；`http` 与
//! `dispatcher` 各自只做自己的那层翻译。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::traits::{
    Clock, CronRepo, EmbeddingClient, GatewayError, LlmClient, MemoryRepo, RepoError,
};
use komo_kernel::types::ids::{ExecutorId, RunId};
use komo_kernel::types::turn::{LlmError, TurnRequest};
use komo_runtime::config::{ConfigHolder, EffortCapabilities};
use komo_runtime::durable::harness::RetryPolicy;
use komo_runtime::executor::ToolExecutor;
use komo_runtime::matter::MatterManager;
use komo_runtime::memory::{MemoryManager, MemoryParts};
use komo_store::{Db, TursoCronRepo, TursoDeliveryRepo, TursoMatterRepo, TursoMemoryRepo};
use time::OffsetDateTime;

use crate::channels::ChannelFactory;
use crate::channels::ChannelRegistry;
use crate::deliveries::DeliveryLog;
use crate::notifier::HomeNotifier;
use crate::sse::{EventHub, SharedHub};

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
    /// `aux.db`（toasty 模型，只装 cron / 投递 / memory / 幂等键这类辅助表，
    /// `docs/storage-durable-d5.md` §1）。
    pub db: Db,
    /// 新存储与 harness（`durable.db`）。
    pub durable: Arc<super::durable::DurableCore>,
    pub hub: SharedHub,
    pub cron: Arc<dyn CronRepo>,
    pub memory: Arc<dyn MemoryRepo>,
    /// §9 的那一层：召回与注入、自动积累、向量索引与代次。
    pub memories: Arc<MemoryManager>,
    /// 目标与领域（`docs/goals.md` §4）：`/v1/matters`。
    pub matters: Arc<MatterManager>,
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
    /// MCP 服务器的连接（`docs/mcp.md` §2.2）：受理 Run 前问它一次，登录后才连上的服务器的
    /// 工具从这里挂进执行器。
    pub mcp: Arc<komo_runtime::mcp::McpHub>,
    /// 执行器本身。留一份具名的 `Arc`，只为了 [`Self::wire_task_spawner`]——`dispatch` /
    /// `follow` 的接缝（`TaskSpawner`）要一份 `Arc<GatewayState>`，而执行器是在这份状态
    /// 存在**之前**造出来的，只能在这里回头接上（`docs/background-tasks.md`）。
    pub tool_executor: Arc<ToolExecutor>,
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
    /// 新存储的核心。
    pub durable: Arc<super::durable::DurableCore>,
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
            durable,
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
        // 事件与工作队列走新存储（W3c）。
        let (events, work) = super::durable::memory::seams(&durable, &config, &clock);
        // 事项的承诺类操作与 `memory add` 读同一份事件找用户原话（`docs/goals.md` §4）。
        let matter_repo: Arc<dyn komo_kernel::traits::MatterRepo> =
            Arc::new(TursoMatterRepo::new(db.clone()));
        let matters = Arc::new(
            MatterManager::new(
                Arc::clone(&matter_repo),
                Arc::clone(&events),
                Arc::clone(&clock),
            )
            .with_cron(Arc::clone(&cron)),
        );
        let memories = Arc::new(MemoryManager::new(MemoryParts {
            matters: Some(matter_repo),
            config: snapshot.memory.clone(),
            repo: Arc::clone(&memory),
            catalog: Arc::new(TursoMemoryRepo::new(db.clone())),
            embeddings: injected_embeddings.clone(),
            reranker,
            llm: Arc::clone(&llm_for_memory(&snapshot, &config, &caps, llm.as_ref())),
            events,
            work,
            clock: Arc::clone(&clock),
            render: komo_agent::context::memory::render_block,
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

        // §5.6 的目录行：注册表**启动时按快照造一份**，那一块文本按用途现渲染（系统提示
        // 一处、`skill://` 的挂载点一处）——两处同源，不会有"提示里有、读不到"这种事。
        let skills = Arc::new(std::sync::RwLock::new(skills_registry(
            &snapshot,
            shared_home.as_deref(),
        )));
        let executor_tools = Arc::new(
            ToolExecutor::new(tools)
                .with_codemode(codemode)
                .with_mcp(Arc::clone(&mcp)),
        );

        // 总轮数默认不设上限：任何一个数在真活上都会撞到，撞上就是一次失败的 Run，事情往往快做完了。
        // 停一条 Run 靠取消；单个 cron Job 想要上限就用自己的 `maxRounds`。
        let max_rounds = None;
        let max_retries = RetryPolicy::default().max_attempts;

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
            durable,
            hub,
            cron,
            memory,
            memories,
            matters,
            deliveries,
            reload_notice: Mutex::new(None),
            notifier,
            channels,
            llm,
            windows: super::windows::WindowCache::default(),
            probe_windows,
            auto_title,
            supervisor: Arc::new(super::channels::ChannelSupervisor::new(factories)),
            inbound: std::sync::OnceLock::new(),
            telemetry: std::sync::OnceLock::new(),
            watching: Mutex::new(std::collections::BTreeSet::new()),
            mcp,
            tool_executor: executor_tools,
            skills,
            max_retries,
            max_rounds,
        }))
    }

    /// 新存储的核心（`durable.db` 上的 harness）。历史上这里可能失败（旧存储模式没有它），
    /// 现在恒为 `Ok`；保留 `Result` 是为了不改全部调用点的 `?`。
    pub fn core(&self) -> Result<&Arc<super::durable::DurableCore>, GatewayError> {
        Ok(&self.durable)
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

    /// `komo mcp reconnect`（`docs/mcp.md` §2.5）：点名的服务器（空 = 全部）断开重连，等到有结果；
    /// 新连上的工具挂进执行器，从下一条 Run 起可用。点了配置里没有的名字交回 `Err`，一台也不动。
    pub async fn reconnect_mcp(
        &self,
        servers: &[String],
    ) -> Result<Vec<komo_kernel::protocol::http::McpServerStatus>, String> {
        let statuses = self.mcp.reconnect(servers).await?;
        let added = self.tool_executor.install(self.mcp.new_tools());
        if !added.is_empty() {
            tracing::info!(
                tools = added.len(),
                "MCP 服务器重连后挂上了新工具，从下一条 Run 起可用"
            );
            self.refresh_skills_prompt();
        }
        Ok(statuses)
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
    pub fn cron_scheduler(self: &Arc<Self>) -> komo_runtime::scheduler::CronScheduler {
        // 触发交给 `DurableCronTarget`（一次提交建会话并受理）。
        let target = self.durable_cron_target();
        komo_runtime::scheduler::CronScheduler::new(
            Arc::clone(&self.cron),
            target,
            Arc::clone(&self.clock),
            Arc::new(komo_runtime::scheduler::JiffZoneResolver::new()),
            self.snapshot().model.clone(),
        )
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

    /// 盯一次触发。`scheduled = false` 是手动 run：它**没有触发记录**（§10：不冒充
    /// 定时触发），所以只投递，不回写状态。
    pub fn watch_cron_run(
        self: &Arc<Self>,
        fired: &komo_runtime::scheduler::Fired,
        job: &komo_kernel::cron::CronJob,
        scheduled: bool,
    ) {
        let watched = super::cron_watch::Watched::of(fired, job, scheduled);
        super::durable::cron_watch::watch(Arc::clone(self), watched);
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

    /// 对 `aux.db` 做一次启动期的写（见 [`komo_store::Db::first_write`]）。
    pub async fn warm_aux_db(&self) -> Result<(), GatewayError> {
        Ok(self.db.first_write().await?)
    }
}

/// Durable 模式下 cron / 投递 / memory 等辅助表的库文件名（在数据目录里，与 `durable.db` 并列）。
pub const AUX_DB_FILE: &str = "aux.db";

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
        // D5：新存储的两个库（`covers` 连 `-wal` / `-log` / `-shm` 兄弟一并认）。
        home.join(komo_store::durable::DURABLE_DB_FILE),
        home.join(AUX_DB_FILE),
        snapshot.paths.runtime_dir.clone(),
        home.join(".env"),
    ]
}

/// codemode 脚本**连读也不行**的路径：[`protected_paths`] 去掉 `sessions/`。
///
/// 脚本里的写一律拒，这份名单只管读。会话日志 `read` 与 bash 本来就读得到，脚本里再拦
/// 只会逼着复盘一类的活儿绕去 bash；数据库、`runtime/` 与 `.env` 仍然拦。
pub fn codemode_unreadable_paths(
    snapshot: &ConfigSnapshot,
    home: &std::path::Path,
) -> Vec<PathBuf> {
    protected_paths(snapshot, home)
        .into_iter()
        .filter(|path| *path != snapshot.paths.sessions_dir)
        .collect()
}

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

/// Durable 模式的无状态模型后端（`ModelStream`），按配置路由。
///
/// 与 [`build_llm`] 同一个取舍：造不出来不让 Gateway 起不来，退到按需建的路由，模型调用
/// 时把原因报出来。测试通过 `ServiceOptions::stream` 注入替身，不走这里。
pub(crate) fn build_stream(
    snapshot: &ConfigSnapshot,
    config: &ConfigHolder,
    caps: &EffortCapabilities,
) -> Arc<dyn komo_runtime::llm::ModelStream> {
    let factory = llm_factory(snapshot, config, caps);
    match komo_runtime::llm::RoutingStream::from_snapshot(snapshot, factory) {
        Ok(routing) => Arc::new(routing),
        Err(error) => {
            tracing::warn!(%error, "模型后端造不出来：Gateway 照常启动，模型调用会报这个错");
            Arc::new(komo_runtime::llm::RoutingStream::lazy(llm_factory(
                snapshot, config, caps,
            )))
        }
    }
}

/// `RepoError` / `StoreError` 到 Gateway 错误。
pub fn repo_error(error: RepoError) -> GatewayError {
    GatewayError::Repo(error)
}
