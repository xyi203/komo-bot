//! Gateway 进程本体：§8.7 的启动顺序，一步不换位。
//!
//! ```text
//! 读配置 → 取得实例锁 → 打开 aux.db 与 durable.db → 绑监听 → 写发现文件
//!        → 引擎恢复（订阅与看客先就位，再 `core.resume()`）
//!        → 起配置轮询 / Cron / 记忆领取 → 起渠道与 HTTP → 等停机信号
//!        → 渠道停 → 引擎收尾 → 删发现文件 → 放锁
//! ```
//!
//! 存储只有一套（`docs/storage-durable.md`）：`durable.db`（harness 的 entry / task / submission）与
//! `aux.db`（cron / 投递 / memory / 幂等键等辅助表）。恢复由引擎开库时的 reconciliation 与
//! [`durable::lifecycle::boot`] 负责，后台循环在 [`durable::background`]。

pub mod channels;
pub mod cron_watch;
pub mod durable;
pub mod interventions;
pub(crate) mod project_files;
pub mod state;
pub mod units;
pub(crate) mod windows;

// W5 恢复故障注入验收（§14）要在集成测试里包一层故障账本，所以它也在 feature 后面。
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use komo_kernel::traits::{Clock, Inbound, LlmClient, Shutdown, Tool};
use komo_runtime::config::{ConfigHolder, EffortCapabilities, LoadOptions};
use komo_store::Db;

use crate::channels::ChannelFactory;
use crate::dispatcher::Dispatcher;
use crate::http::Api;
use crate::lock::{DiscoveryFile, GatewayDiscovery, InstanceLock, LockError, random_token};

use state::{Assembly, GatewayState, SystemClock};

/// 停机时给手上的任务多少时间收尾（§8.7「给正在完成的工具短暂收尾时间」）。
const DRAIN: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Config(#[from] komo_runtime::config::ConfigError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("打不开数据库：{0}")]
    Store(String),
    #[error("监听 {addr} 失败：{message}")]
    Listen { addr: String, message: String },
    #[error("装配失败：{0}")]
    Assemble(String),
}

/// 起一台 Gateway 要的东西。
#[derive(Default)]
pub struct ServiceOptions {
    /// 数据目录；`None` = `KOMO_HOME`，再没有就是 `~/.komo`。
    pub home: Option<PathBuf>,
    /// 监听地址；`None` = 配置里的（默认 `127.0.0.1:7777`）。
    pub listen: Option<String>,
    /// 渠道工厂。三个渠道各自一个 feature，接线由调用方给（`komo` 的 `main`）。
    pub channels: Vec<Arc<dyn ChannelFactory>>,
    /// 测试注入的模型后端。
    pub llm: Option<Arc<dyn LlmClient>>,
    /// 测试注入的无状态模型后端（harness 的主模型）；`None` = 按配置造
    /// [`RoutingStream`](komo_runtime::llm::RoutingStream)。
    pub stream: Option<Arc<dyn komo_runtime::llm::ModelStream>>,
    /// 测试注入的向量后端；`None` = 按 `memory.embedding` alias 造。
    pub embeddings: Option<Arc<dyn komo_kernel::traits::EmbeddingClient>>,
    /// 共享 agent 目录（`~/.agents/skills`）按**哪个家目录**算；
    /// `None` = 当前用户的家目录（§5.6）。
    ///
    /// 它是注入的，不是现场读 `$HOME`：不然"提示里有哪些 skill"取决于跑这台进程的机器，
    /// 测试之间会互相污染（谁在 `~/.agents/skills` 里装过东西，别人的断言就变），多实例
    /// 部署也没法把共享面钉死。
    pub shared_home: Option<PathBuf>,
    /// bin 里 OTLP 导出层记结果的地方；`/healthz` 读它。`None` = 没接遥测。
    pub telemetry: Option<Arc<crate::telemetry::TelemetryStatus>>,
}

impl std::fmt::Debug for ServiceOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceOptions")
            .field("home", &self.home)
            .field("listen", &self.listen)
            .finish_non_exhaustive()
    }
}

/// 一台起着的 Gateway。
pub struct Running {
    pub state: Arc<GatewayState>,
    pub dispatcher: Arc<Dispatcher>,
    pub addr: SocketAddr,
    pub base_url: String,
    pub shutdown: Shutdown,
    discovery: DiscoveryFile,
    lock: InstanceLock,
}

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Running")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl Running {
    /// 正常停机（§8.7）：**先停止接收新执行**，给手上的收尾时间，再删发现文件、放锁。
    pub async fn stop(self) {
        self.state.supervisor.stop_all(&self.state).await;
        self.shutdown.cancel();
        // 引擎收尾：先终止监听的进程组（登记留给下一代启动时收）、封住受理、等 invocation 走完、
        // 关库（放掉 Turso 的文件锁）。
        match tokio::time::timeout(DRAIN, self.state.durable.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "durable.db 没有干净地关掉"),
            Err(_) => tracing::warn!("durable.db 收尾超时，下次启动从 checkpoint 继续"),
        }
        self.discovery.remove();
        self.lock.release();
        tracing::info!("Gateway 已停止");
    }
}

/// 起一台，返回句柄（测试与 `run` 都用它）。
pub async fn start(options: ServiceOptions) -> Result<Running, ServiceError> {
    // 1. 读配置。**校验不过一个快照都不装**（§3 第 1 步）。
    let load = match &options.home {
        Some(home) => LoadOptions::at(home),
        None => LoadOptions::default(),
    };
    let config = Arc::new(ConfigHolder::load(&load)?);
    let snapshot = config.current();
    let home = config.home().to_path_buf();
    for issue in config.warnings() {
        tracing::warn!(key = %issue.key, message = %issue.message, "配置警告");
    }

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let instance_id = komo_kernel::types::ids::uuid_v7_at(clock.now()).to_string();
    let token = random_token();

    // 2. 取得实例锁。**拿不到就是有别的实例在跑**，不删仍有效的锁（§3）。
    let lock = InstanceLock::acquire(&home, &instance_id, clock.now())?;

    // 2b. 数据目录的骨架。**`workspaces/` 必须真的存在**：它是会话默认的 cwd（§12），
    //     少了它每一次 shell 的 spawn 都以 ENOENT 失败，而报错只会说是程序
    //     起不来——一个把人引偏的结论。建不出来不让整台 Gateway 起来：别的工具照常，
    //     这一条报在日志里（与渠道、向量端点的处理同一个取舍）。
    if let Err(error) = std::fs::create_dir_all(&snapshot.paths.workspaces_dir) {
        tracing::warn!(
            dir = %snapshot.paths.workspaces_dir.display(),
            %error,
            "工作目录建不出来，shell 会在这一步上失败"
        );
    }

    // 3. 打开 aux.db（cron / 投递 / memory / 幂等键这些辅助表，toasty + Turso MVCC；Turso 对 db 文件持进程
    //    独占锁，所以这一步也再证一次只有一个实例）；`durable.db` 在下面随工具一起打开。
    let db_path = home.join(state::AUX_DB_FILE);
    let db = Db::connect(&db_path)
        .await
        .map_err(|error| ServiceError::Store(format!("{}：{error}", db_path.display())))?;

    // 4. 装配。
    // 转后台的接手方要等整份状态才造得出来：`bash` 先拿着空槽，`wire` 再接上。
    let detach_slot = Arc::new(komo_runtime::tools::detach::DetachSlot::default());
    let (tools, codemode, mcp) = build_tools(&config, &instance_id, &detach_slot).await;

    // 钩子要等 `GatewayState` 造好才找得到它，环用槽断开（`durable::assemble`）。
    let state_slot = durable::assemble::new_slot();
    let stream = options
        .stream
        .clone()
        .unwrap_or_else(|| state::build_stream(&snapshot, &config, &EffortCapabilities::builtin()));
    let core = durable::DurableCore::open(
        &home,
        Arc::clone(&clock),
        stream,
        durable::assemble::build_hooks(&state_slot),
        durable::assemble::build_orchestration(&state_slot),
        codemode.clone(),
    )
    .await
    .map_err(|error| ServiceError::Store(format!("durable.db：{error}")))?;

    let state = match GatewayState::assemble(Assembly {
        home: home.clone(),
        config: Arc::clone(&config),
        caps: EffortCapabilities::builtin(),
        db: db.clone(),
        durable: core.clone(),
        clock: Arc::clone(&clock),
        instance_id: instance_id.clone(),
        token: token.clone(),
        llm: options.llm,
        embeddings: options.embeddings,
        shared_home: options.shared_home.clone(),
        tools,
        codemode,
        mcp,
        channels: options.channels,
    })
    .await
    {
        Ok(state) => state,
        Err(error) => {
            let _ = core.close().await;
            return Err(ServiceError::Assemble(error.to_string()));
        }
    };

    if let Some(telemetry) = options.telemetry {
        let _ = state.telemetry.set(telemetry);
    }

    let dispatcher = Arc::new(Dispatcher::new(Arc::clone(&state)));
    let _ = state
        .inbound
        .set(Arc::clone(&dispatcher) as Arc<dyn Inbound>);
    let shutdown = Shutdown::new();
    // 把 `GatewayState` 交给钩子，然后才开始调度（编排走 harness，监听由 `MonitorRunner` 接）。
    durable::assemble::wire(&state_slot, &state, &detach_slot);
    // 重启时的订阅顺序（W4a）：**订阅与看客都在引擎恢复之前就位**，恢复里结算的 run（崩溃时在飞的）
    // 产生的终态帧才不会落在没人听的空档里。顺序有讲究：
    //   1. feed——提交 → SSE 帧（看客读的就是它推进 hub 的帧）；
    //   2. faulted 任务的提醒订阅（回放放后台任务里做）；
    //   3. blob 引用检查——恢复一开始 `execute` 阶段的任务会把读不到的输出当成"没有结果"重判，
    //      所以要先让这些任务 `faulted`；
    //   4. 看客与未结算输入的小任务（`wait_submission` 内部会 `resume`，所以排在 3 之后）。
    durable::feed::spawn(&core, &state.hub, &shutdown).await;
    durable::interventions::spawn_attention(&state, &shutdown);
    durable::lifecycle::boot(&state).await;
    durable::watch::reattach(&state).await;
    core.resume();
    // 窗口探测放后台，不挡 `Gateway 就绪`。
    state.spawn_window_probes();

    // 5. 绑监听——发现文件里要写真实地址，所以端口必须先定下来。
    let listen = options
        .listen
        .unwrap_or_else(|| snapshot.start_only.listen.clone());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|error| ServiceError::Listen {
            addr: listen.clone(),
            message: error.to_string(),
        })?;
    let addr = listener
        .local_addr()
        .map_err(|error| ServiceError::Listen {
            addr: listen.clone(),
            message: error.to_string(),
        })?;
    let base_url = format!("http://{addr}");

    // 6. 写发现文件（客户端按它找到这台实例并核对身份，§3 第 1–2 步）。
    let discovery = DiscoveryFile::write(
        &home,
        &GatewayDiscovery {
            instance_id: instance_id.clone(),
            base_url: base_url.clone(),
            protocol_version: komo_kernel::protocol::PROTOCOL_VERSION,
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            pid: Some(std::process::id()),
            data_dir: Some(home.display().to_string()),
            token: Some(token.clone()),
            started_at: Some(state.started_at),
        },
    )?;

    // 7. aux.db 的**第一次写**：只建不写的新文件，Turso 在同一进程里重开会报
    //    `database is locked`（实测；写过一次就好）。生产里重启是新进程，不受影响；测试里
    //    `stop` 后同一数据目录重起要靠这一下。
    if let Err(error) = state.warm_aux_db().await {
        tracing::warn!(%error, "aux.db 的首次写没做成");
    }

    // 8. 后台循环：feed 与看客已在 `core.resume()` 之前起好（上面），这里只起 resume 之后的。
    durable::background::spawn(&state, &shutdown);

    // 9. 渠道与 HTTP。
    //
    // **补发不在就绪之前**：它是网络 I/O，一条 pending 一个平台往返，积压多少就等多久。
    // 它曾经同步跑在 "Gateway 就绪" 之前，于是卡住的投递直接把重启拖长——10 条卡住的
    // 投递是 3 秒，几百条能拖过客户端 60 秒的就绪超时（§11.4 的补发等得起：投递记录先
    // 写后发，按 `DeliveryId` 幂等，晚几十毫秒没有代价）。
    state.supervisor.start_all(&state).await;
    let app = crate::http::router(Api::new(Arc::clone(&state)));
    let serving = shutdown.clone();
    tokio::spawn(async move {
        let graceful = axum::serve(listener, app).with_graceful_shutdown(async move {
            while !serving.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
        if let Err(error) = graceful.await {
            tracing::error!(%error, "HTTP 监听退出");
        }
    });

    tracing::info!(%base_url, instance = %instance_id, "Gateway 就绪");

    // 10. 补发积压的投递。**渠道全部登记之后**才有意义：`DeliveryLog::send_recorded`
    //     找不到发送口就把行原样留在 pending，所以在 `start_all` 之前冲刷等于什么都没干
    //     （W5 验收 BUG(3)）。这一步补的是"渠道都起完了"之后的那一遍，包括没有工厂、
    //     由别处登记发送口的情形。
    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            state.notifier.flush(None).await;
        });
    }

    Ok(Running {
        state,
        dispatcher,
        addr,
        base_url,
        shutdown,
        discovery,
        lock,
    })
}

/// Agent 配置在哪个文件里（`[agents.<id>]` 的出处，日志里要说清是"谁写的配置"）。
pub fn agent_config_file(config: &ConfigHolder) -> PathBuf {
    config.home().join("config.toml")
}

/// codemode 的 worker：当前这个可执行文件的 `__codemode-worker` 子命令，启动时解析一次
/// （`docs/codemode.md` §3）。测试进程自己不是 `komo`，用进程内 worker。
#[cfg(not(any(test, feature = "test-support")))]
fn codemode_worker() -> Option<komo_runtime::codemode::Worker> {
    match komo_runtime::codemode::Worker::current_exe() {
        Ok(worker) => Some(worker),
        Err(error) => {
            tracing::warn!(%error, "找不到自己的可执行文件：这台 Gateway 不挂 codemode");
            None
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
fn codemode_worker() -> Option<komo_runtime::codemode::Worker> {
    Some(komo_runtime::codemode::Worker::in_process())
}

/// 六个基础工具（§4）与那条编排操作。`codemode` 随 worker 一起装配（`docs/codemode.md` §3），worker 交给执行器。
/// MCP 的连接留在 [`komo_runtime::mcp::McpHub`] 里，后台连：连上的服务器受理 Run 时再把工具挂进来。
async fn build_tools(
    config: &Arc<ConfigHolder>,
    instance_id: &str,
    detach_slot: &Arc<komo_runtime::tools::detach::DetachSlot>,
) -> (
    Vec<Arc<dyn Tool>>,
    Option<komo_runtime::codemode::Worker>,
    Arc<komo_runtime::mcp::McpHub>,
) {
    use komo_runtime::tools::{
        BashTool, EditTool, MonitorStopTool, MonitorTool, ReadTool, RgTool, TaskTool, WriteTool,
    };

    let snapshot = config.current();
    let registry = Arc::new(komo_runtime::recovery::ChildRegistry::new(
        snapshot.paths.runtime_dir.join("children"),
    ));
    let registration = komo_runtime::tools::process::ChildRegistration {
        registry: Arc::clone(&registry),
        executor: komo_kernel::types::ids::ExecutorId::from_raw(instance_id.to_string()),
    };

    // §7 唯一的硬限制：komo 自己的状态不许被 write / edit 改，shell 里只读挂载（§8.10）。名单只依赖数据
    // 目录与 start-only 的路径，启动时算一次就够——热重载换不动它们。
    let protected = state::protected_paths(&snapshot, config.home());
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::new()),
        Arc::new(WriteTool::new().protecting(protected.clone())),
        Arc::new(EditTool::new().protecting(protected.clone())),
        Arc::new(
            BashTool::new()
                .registered(registration.clone())
                .protecting(protected.clone())
                .detaching(Arc::clone(detach_slot)),
        ),
        // 搜索是**进程内**的（`grep` + `ignore` 书库）：没有外部二进制，也就没有"这台机器
        // 上没装"这一种情况，不需要探测。
        Arc::new(RgTool::new()),
    ];
    // task：**编排**，不是第七个基础能力（§4），把耗时的事派给独立的任务会话
    // （`docs/background-tasks.md`）。放行之后由 executor 调 `TaskSpawner`——没有装配它时
    // （`ToolExecutor::set_spawner` 没被叫过）按"这台 Gateway 没有接任务分发"干净失败。
    // model / effort 在 `prepare` 里按当前配置快照校验，所以它拿着配置。
    tools.push(Arc::new(TaskTool::new(Arc::clone(config))));
    // monitor / monitor_stop：同一类编排操作，脚本的进程归 Gateway 的 `GatewayMonitors`
    // （`docs/monitor.md`）。
    tools.push(Arc::new(MonitorTool::new()));
    tools.push(Arc::new(MonitorStopTool::new()));
    // MCP（`docs/mcp.md` §2.2）：启动时每台服务器后台连，**不等握手**，这里一个 MCP 工具也不挂。
    // 连上的工具在受理 Run 时挂进同一份目录（`GatewayState::refresh_mcp`），之后与内置工具走
    // 同一条落账的路；被接续的 Run 调到时执行器自己等（`ToolExecutor::await_mcp`）。
    let mcp = Arc::new(komo_runtime::mcp::McpHub::start(
        &snapshot.mcp,
        &config.secrets(),
        config.home(),
    ));
    // 测试专用的工具（`gate` / `unsure`，见 `test_support`）：只在测试为这个数据目录装过
    // 时才有，生产构建里这一行不存在。
    #[cfg(any(test, feature = "test-support"))]
    tools.extend(test_support::test_tools(config.home()));

    // 脚本里的文件是只读的；komo 自己的状态除会话日志外连读也不行（`sessions/` 与 `read` 一样可读）。
    let unreadable = state::codemode_unreadable_paths(&snapshot, config.home());
    let codemode = codemode_worker().map(|worker| worker.protecting(unreadable));
    if codemode.is_some() {
        tools.push(Arc::new(komo_runtime::tools::CodemodeTool::new()));
    }
    (tools, codemode, mcp)
}

/// `komo gateway --foreground` 的主流程：起来，等停机信号，收尾。
pub async fn run(options: ServiceOptions) -> Result<(), ServiceError> {
    // 在起任何子进程之前：shell / 监听继承这个上限。
    match komo_runtime::limits::raise_open_files() {
        Ok(raised) if raised.after > raised.before => {
            tracing::info!(
                before = raised.before,
                after = raised.after,
                "抬高了能开的文件数"
            )
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "抬不了能开的文件数，子进程沿用服务管理器给的上限"),
    }
    let running = start(options).await?;
    wait_for_signal().await;
    tracing::info!("收到停机信号");
    running.stop().await;
    Ok(())
}

#[cfg(unix)]
async fn wait_for_signal() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(term) => term,
        Err(error) => {
            tracing::warn!(%error, "装不上 SIGTERM 处理器，只等 Ctrl-C");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
