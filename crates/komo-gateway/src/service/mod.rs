//! Gateway 进程本体：§8.7 的启动顺序，一步不换位。
//!
//! ```text
//! 读配置 → 取得实例锁 → 打开 state.db → 绑监听 → 写发现文件
//!        → 恢复扫描（补发上一次没送到的结果）
//!        → 起调度器 / Cron / 配置轮询 → 起渠道与 HTTP → 等停机信号
//!        → 渠道停 → 等手上的跑完 → 删发现文件 → 放锁
//! ```
//!
//! 「Gateway 获得数据目录进程锁并完成存储校验后，**自动扫描未完成运行**；不必等用户
//! 打开 CLI 或发送 resume。恢复与新请求共用调度器，恢复扫描本身不等待全部旧任务完成
//! 才提供服务。」——所以恢复扫描在监听之前跑完它那一遍**索引**工作，真正的执行交给
//! 调度器，与新请求同一条路。

pub mod channels;
pub mod cron_watch;
pub mod ledgers;
pub mod run_watch;
pub mod segment;
pub mod state;
pub mod units;

// W5 恢复故障注入验收（§14）要在集成测试里包一层故障账本，所以它也在 feature 后面。
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use komo_kernel::traits::{Clock, Inbound, LlmClient, Shutdown, Tool};
use komo_kernel::types::chat::Outbound;
use komo_runtime::config::{ConfigHolder, EffortCapabilities, LoadOptions};
use komo_runtime::toolbox::Toolbox;
use komo_store::Db;
use time::OffsetDateTime;

use crate::channels::ChannelFactory;
use crate::dispatcher::Dispatcher;
use crate::http::Api;
use crate::lock::{DiscoveryFile, GatewayDiscovery, InstanceLock, LockError, random_token};

use state::{Assembly, GatewayState, SystemClock};

/// Cron 的扫描节奏：一分钟一次（五字段表达式的精度就是分钟）。
const CRON_TICK: std::time::Duration = std::time::Duration::from_secs(60);
/// 控制审计补写的节奏。补写是审计与顺序，不承担耐久性（§8.2），所以不必更密。
const AUDIT_TICK: std::time::Duration = std::time::Duration::from_secs(60);
/// 记忆后台队列的节奏（§9.3）。「run.completed …提交终态与 memory_work = pending →
/// **后台领取**」——领取是这一拍，不在 Run 的关键路径上。
const MEMORY_TICK: std::time::Duration = std::time::Duration::from_secs(30);
/// 停机时给手上的任务多少时间收尾（§8.7「给正在完成的工具短暂收尾时间」）。
const DRAIN: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Config(#[from] komo_runtime::config::ConfigError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("打不开 state.db：{0}")]
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
    /// 测试注入的向量后端；`None` = 按 `memory.embedding` alias 造。
    pub embeddings: Option<Arc<dyn komo_kernel::traits::EmbeddingClient>>,
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
        let _ = tokio::time::timeout(DRAIN, self.state.scheduler.wait_for_idle()).await;
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

    // 3. 打开 state.db（Turso 对 db 文件持进程独占锁，所以这一步也再证一次只有一个）。
    let db = Db::connect(&snapshot.start_only.db_path)
        .await
        .map_err(|error| ServiceError::Store(error.to_string()))?;

    // 4. 装配。
    let state = GatewayState::assemble(Assembly {
        home: home.clone(),
        config: Arc::clone(&config),
        caps: EffortCapabilities::builtin(),
        db: db.clone(),
        clock: Arc::clone(&clock),
        instance_id: instance_id.clone(),
        token: token.clone(),
        llm: options.llm,
        embeddings: options.embeddings,
        tools: build_tools(&config, &instance_id, &db, Arc::clone(&clock)).await,
        channels: options.channels,
    })
    .await
    .map_err(|error| ServiceError::Assemble(error.to_string()))?;

    let dispatcher = Arc::new(Dispatcher::new(Arc::clone(&state)));
    let _ = state
        .inbound
        .set(Arc::clone(&dispatcher) as Arc<dyn Inbound>);

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

    let shutdown = Shutdown::new();

    // 7. 恢复扫描。**只补索引与派生状态，不调用工具、不发外部请求**（§8.5）。
    match state.recovery().scan().await {
        Ok(report) => {
            tracing::info!(summary = %report.summary(), reclaimed = report.reclaimed, "恢复扫描完成");
            for outcome in report.to_redeliver() {
                // 「已保存最终结果，但客户端没有收到 → 补发或补读原结果，**不重新执行
                // 任务**」（§8.4）。
                let _ = state
                    .notifier
                    .deliver_home(Outbound::RunFinished {
                        session: outcome.session.clone(),
                        run: outcome.run.clone(),
                        summary: "这个任务在上次停机前就完成了，结果补发一次。".into(),
                    })
                    .await;
            }
            for group in report.corrupt_groups() {
                // 「停止受影响会话，**报告损坏**」（§8.4 / §8.5）——报告这一半就是这一条：
                // 一个读不出来的会话不会自己好起来，操作者得知道是哪一个、为什么。
                let affected = if group.runs.len() == 1 {
                    String::new()
                } else {
                    format!(
                        "\n\n同一会话有 {} 个未完成任务受影响：\n{}",
                        group.runs.len(),
                        group
                            .runs
                            .iter()
                            .map(|run| format!("- {run}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                let _ = state
                    .notifier
                    .deliver_home(Outbound::NeedsAttention {
                        session: group.session,
                        // Outbound 的兼容字段保留一个代表 Run；正文列出这一组的全部 Run。
                        run: group.runs[0].clone(),
                        reason: format!("恢复时停下了：{}{affected}", group.reason),
                    })
                    .await;
            }
            if report.requeued() > 0 {
                state.waker().wake();
            }
        }
        Err(error) => tracing::error!(%error, "恢复扫描失败：新请求照常，未完成的任务等下一次扫描"),
    }

    // 8. 补写控制审计 outbox（§8.7 的启动顺序第 3 步：恢复扫描之后、服务起来之前）。
    state.drain_audit().await;

    // 8b. 把卡在 `processing` 的记忆处理放回 pending（§9.3「进程崩溃后可重新领取」）。
    // 与恢复扫描同一个理由，也同一个位置：上一次没跑完的，这一次要能再领一遍。
    match state.requeue_memory_work().await {
        Ok(0) => {}
        Ok(requeued) => tracing::info!(requeued, "上次没跑完的记忆处理已放回队列"),
        Err(error) => tracing::warn!(%error, "记忆处理队列的复位没做成"),
    }

    // 9. 后台任务：调度器、Cron、配置轮询、SIGHUP。
    spawn_background(&state, &shutdown);

    // 10. 渠道与 HTTP。
    //
    // **补发在渠道登记之后**：`DeliveryLog::send_recorded` 找不到发送口就把行原样留在
    // pending，所以在 `start_all` 之前冲刷等于什么都没做（W5 验收 BUG(3)）。每个渠道起来
    // 时还会按自己的平台冲刷一次（`ChannelSupervisor::start`），这里补的是"渠道都起完了"
    // 之后的那一遍，包括没有工厂、由别处登记发送口的情形。
    state.supervisor.start_all(&state).await;
    state.notifier.flush(None).await;
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

fn spawn_background(state: &Arc<GatewayState>, shutdown: &Shutdown) {
    {
        let scheduler = Arc::clone(&state.scheduler);
        let shutdown = shutdown.clone();
        tokio::spawn(async move { scheduler.serve(shutdown).await });
    }
    {
        // Cron：一分钟一扫（五字段表达式的精度就是分钟）。
        let state = Arc::clone(state);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(CRON_TICK).await;
                if shutdown.is_cancelled() {
                    return;
                }
                match state.cron_scheduler().tick().await {
                    Ok(tick) => {
                        if !tick.fired.is_empty() {
                            tracing::info!(fired = tick.fired.len(), "定时任务已入队");
                        }
                        // 投出去之后要有人盯着（§10 的最后两步 + §7.4 的"投递到 home
                        // chat"）：Cron 没有来源会话，等待审批就只能从这里投出去。
                        state.watch_fired(&tick.fired).await;
                    }
                    Err(error) => tracing::warn!(%error, "Cron 这一轮扫描失败"),
                }
            }
        });
    }
    {
        // 控制审计的周期补写（§8.5 的反向顺序）。启动时补过一次；这一遍管的是运行期
        // 产生的那些——补写按 `event_id` 幂等，补不上的留在 outbox 里下次再来。
        let state = Arc::clone(state);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(AUDIT_TICK).await;
                if shutdown.is_cancelled() {
                    return;
                }
                state.drain_audit().await;
            }
        });
    }
    {
        // 记忆的后台队列（§9.3）。失败的 Run 放回 pending，下一拍重试；这一拍什么都没领
        // 到是常态，不记日志。
        let state = Arc::clone(state);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(MEMORY_TICK).await;
                if shutdown.is_cancelled() {
                    return;
                }
                match state
                    .memories
                    .process_pending(komo_runtime::memory::WORK_BATCH)
                    .await
                {
                    Ok(report)
                        if report.processed > 0 || report.failed > 0 || report.skipped > 0 =>
                    {
                        tracing::info!(
                            processed = report.processed,
                            applied = report.applied,
                            skipped = report.skipped,
                            failed = report.failed,
                            "记忆提取这一轮结束"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "记忆提取这一轮没跑成"),
                }
            }
        });
    }
    {
        let state = Arc::clone(state);
        let shutdown = shutdown.clone();
        tokio::spawn(async move { crate::reload::watch(state, shutdown).await });
    }
    {
        let state = Arc::clone(state);
        let shutdown = shutdown.clone();
        tokio::spawn(async move { crate::reload::on_sighup(state, shutdown).await });
    }
}

/// 这台 Gateway 的 toolbox（§5.3）。
///
/// 目录一定建出来，内置模块首次启动装进去（§5.5 的 memos）——「首次启动若 toolbox 里
/// 没有就写入并标已启用」，已经有同名模块就**不动**。
pub fn toolbox_of(snapshot: &komo_kernel::protocol::config::ConfigSnapshot) -> Arc<Toolbox> {
    let toolbox = Arc::new(Toolbox::new(snapshot.paths.toolbox_dir.clone()));
    if let Err(error) = toolbox.ensure_layout() {
        tracing::warn!(%error, "toolbox 目录建不出来");
    }
    match komo_runtime::toolbox::builtin::install(&toolbox, OffsetDateTime::now_utc()) {
        Ok(installed) => {
            for module in installed {
                tracing::info!(module = %module.module, version = %module.version, "装上内置 toolbox 模块");
            }
        }
        Err(error) => tracing::warn!(%error, "内置 toolbox 模块装不进去"),
    }
    toolbox
}

/// 凭证引用的解析口：**名字**问 toolbox（模块的 `__komo_env__`），**值**问
/// `ConfigHolder`（`.env`）。
///
/// 两边都是**每次现读**：`toolbox.inspect` 读的是磁盘上当前那一份，
/// `ConfigHolder::secrets()` 读的是 arc-swap 里当前那一份。所以启用一个新版本、或者改
/// 完 `.env` 跑一次 `komo config reload`，下一次调用就按新的来，不必重启（§3 第 2 步）。
///
/// **值到此为止**：它只在 `PythonRuntime::run` 里被放进那一次 spawn 的 env，不进
/// Gateway 自己的进程环境、不进计划、不进日志（§5.3、§7.2）。
struct ToolboxSecrets {
    toolbox: Arc<Toolbox>,
    config: Arc<ConfigHolder>,
}

impl komo_runtime::python_runtime::SecretResolver for ToolboxSecrets {
    fn names_for(&self, module: &str) -> Vec<String> {
        // 读不出这个模块（被停用、被删掉）就是"它没有声明任何凭证"——不猜一个名单。
        self.toolbox
            .inspect(module)
            .map(|info| info.env)
            .unwrap_or_default()
    }

    fn resolve(&self, name: &str) -> Option<String> {
        // 空串当作没配：一个空令牌只会在第一次请求时才失败，而那时说的是"401"，
        // 不是"没配"（`Secrets::has` 已经是这个约定）。
        self.config
            .secrets()
            .get(name)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
    }
}

/// 受管理解释器的配置（§5.1）。
///
/// 三件与 toolbox 相关的事在这里接上：`toolbox_parent` 让 `import toolbox.x` 成立，
/// `denied_imports` 让候选与历史快照 import 不到（§7.3），`secrets` 让已授权模块在
/// **每次 spawn 时**按名拿到它声明的那几个变量（§5.3）。
pub fn python_env(
    config: &Arc<ConfigHolder>,
    toolbox: &Arc<Toolbox>,
) -> komo_runtime::python_runtime::PythonEnvConfig {
    let snapshot = config.current();
    let mut python = komo_runtime::python_runtime::PythonEnvConfig::new(
        snapshot.start_only.python_env_root.clone(),
        snapshot.paths.workspaces_dir.clone(),
    );
    // 受管理环境还没建出来时**退回系统解释器**，并且说出来。
    //
    // 不静默：`env_version` 会变成系统解释器的版本 + `no-lock`，那正是"这台机器上跑的
    // 是哪一个解释器、锁没锁依赖"的诚实答案（§5.1）。反过来，让整个 `python` 工具因为
    // 一个还没 `python -m venv` 过的目录而消失，等于把一句诊断换成一个空白。
    if !python.interpreter_path().exists() {
        tracing::warn!(
            env_root = %python.env_root.display(),
            "受管理的 Python 环境还没建出来，这次退回系统 python3"
        );
        python.interpreter = Some(std::path::PathBuf::from("python3"));
    }
    python.toolbox_parent = Some(toolbox.layout().parent());
    python.denied_imports = toolbox.layout().denied_import_roots();
    python.secrets = Some(Arc::new(ToolboxSecrets {
        toolbox: Arc::clone(toolbox),
        config: Arc::clone(config),
    }));
    python
}

/// 五个基础工具（§4）。`python` 要有一个跑得起来的解释器才挂。
async fn build_tools(
    config: &Arc<ConfigHolder>,
    instance_id: &str,
    db: &komo_store::Db,
    clock: Arc<dyn Clock>,
) -> Vec<Arc<dyn Tool>> {
    use komo_runtime::tools::{EditTool, ReadTool, ShellTool, WriteTool};

    let snapshot = config.current();
    let registry = Arc::new(komo_runtime::recovery::ChildRegistry::new(
        snapshot.paths.runtime_dir.join("children"),
    ));
    let registration = komo_runtime::tools::process::ChildRegistration {
        registry: Arc::clone(&registry),
        executor: komo_kernel::types::ids::ExecutorId::from_raw(instance_id.to_string()),
    };

    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::new()),
        Arc::new(WriteTool::new()),
        Arc::new(EditTool::new()),
        Arc::new(ShellTool::new().registered(registration.clone())),
    ];

    let toolbox = toolbox_of(&config.current());
    let python = python_env(config, &toolbox);
    match komo_runtime::python_runtime::PythonRuntime::probe(python).await {
        Ok(runtime) => {
            // 核对函数的那道门（§8.6）。它自己带一份 Policy 与授权表：核对是一次**新的**
            // 执行计划（`PlanSource::Verification`），要按当时的规则重新判一次。
            let gate = komo_runtime::tools::python::VerificationGate {
                policy: komo_runtime::policy::PolicyEngine::from_rules(snapshot.policy.clone()),
                approvals: Arc::new(komo_store::TursoApprovalRepo::new(db.clone())),
                clock,
            };
            tools.push(Arc::new(
                komo_runtime::tools::PythonTool::new(Arc::new(runtime.registered(registration)))
                    .with_toolbox(toolbox)
                    .with_verification(gate),
            ));
        }
        Err(error) => {
            // 「没有解释器」不该让整台 Gateway 起不来——别的四个工具照常。
            tracing::warn!(%error, "Python 环境探测不到：这台 Gateway 不挂 python 工具");
        }
    }
    tools
}

/// `komo gateway --foreground` 的主流程：起来，等停机信号，收尾。
pub async fn run(options: ServiceOptions) -> Result<(), ServiceError> {
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
