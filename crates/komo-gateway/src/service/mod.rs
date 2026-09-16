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
pub mod ledgers;
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
use komo_store::Db;

use crate::channels::ChannelFactory;
use crate::dispatcher::Dispatcher;
use crate::http::Api;
use crate::lock::{DiscoveryFile, GatewayDiscovery, InstanceLock, LockError, random_token};

use state::{Assembly, GatewayState, SystemClock};

/// Cron 的扫描节奏：一分钟一次（五字段表达式的精度就是分钟）。
const CRON_TICK: std::time::Duration = std::time::Duration::from_secs(60);
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
        db,
        clock: Arc::clone(&clock),
        instance_id: instance_id.clone(),
        token: token.clone(),
        llm: options.llm,
        tools: build_tools(&snapshot, &instance_id).await,
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
            if report.requeued() > 0 {
                state.waker().wake();
            }
        }
        Err(error) => tracing::error!(%error, "恢复扫描失败：新请求照常，未完成的任务等下一次扫描"),
    }

    // 上一次没送到的投递，现在补发（§11.4）。
    state.notifier.flush(None).await;

    // 8. 后台任务：调度器、Cron、配置轮询、SIGHUP。
    spawn_background(&state, &shutdown);

    // 9. 渠道与 HTTP。
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
                    Ok(tick) if !tick.fired.is_empty() => {
                        tracing::info!(fired = tick.fired.len(), "定时任务已入队");
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "Cron 这一轮扫描失败"),
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

/// 五个基础工具（§4）。`python` 要有一个跑得起来的解释器才挂。
async fn build_tools(
    snapshot: &komo_kernel::protocol::config::ConfigSnapshot,
    instance_id: &str,
) -> Vec<Arc<dyn Tool>> {
    use komo_runtime::tools::{EditTool, ReadTool, ShellTool, WriteTool};

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

    let python = komo_runtime::python_runtime::PythonEnvConfig::new(
        snapshot.start_only.python_env_root.clone(),
        snapshot.paths.workspaces_dir.clone(),
    );
    match komo_runtime::python_runtime::PythonRuntime::probe(python).await {
        Ok(runtime) => tools.push(Arc::new(komo_runtime::tools::PythonTool::new(Arc::new(
            runtime.registered(registration),
        )))),
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
