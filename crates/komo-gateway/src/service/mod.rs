//! Gateway 进程本体：§8.7 的启动顺序，一步不换位。
//!
//! ```text
//! 读配置 → 取得实例锁 → 打开 state.db（Durable 模式：durable.db + aux.db）→ 绑监听 → 写发现文件
//!        → 恢复扫描（补发上一次没送到的结果）
//!        → 起调度器 / Cron / 配置轮询 → 起渠道与 HTTP → 等停机信号
//!        → 渠道停 → 等手上的跑完 → 删发现文件 → 放锁
//! ```
//!
//! 「Gateway 获得数据目录进程锁并完成存储校验后，**自动扫描未完成运行**；不必等用户
//! 打开 CLI 或发送 resume。恢复与新请求共用调度器，恢复扫描本身不等待全部旧任务完成
//! 才提供服务。」——所以恢复扫描在监听之前跑完它那一遍**索引**工作，真正的执行交给
//! 调度器，与新请求同一条路。
//!
//! **两种存储模式**（`docs/storage-durable-d5.md` §1）：[`StorageMode::Legacy`] 是上面这条
//! 路；[`StorageMode::Durable`] 打开 `durable.db` 与 `aux.db`、**不打开 `state.db`**，跳过
//! 旧的调度器 / 对账 / 审计补写 / 重挂看客，后台循环换成 [`durable::background`]。

pub mod channels;
pub(crate) mod compaction;
pub(crate) mod context_sources;
pub mod cron_watch;
pub mod durable;
pub mod interventions;
pub mod ledgers;
pub mod lifecycle;
pub mod monitors;
pub(crate) mod project_files;
pub mod run_watch;
pub mod segment;
pub mod state;
pub mod tasks;
pub mod titles;
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
    #[error("打不开数据库：{0}")]
    Store(String),
    #[error("监听 {addr} 失败：{message}")]
    Listen { addr: String, message: String },
    #[error("装配失败：{0}")]
    Assemble(String),
}

/// 这台 Gateway 用哪套存储（`docs/storage-durable-d5.md` §1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageMode {
    /// 旧的 `state.db` + JSONL 日志。D5 期间的生产默认，W5 翻成 `Durable`，D7 删。
    #[default]
    Legacy,
    /// 新的 `durable.db`（harness 的 entry / task / submission），cron / 投递 / memory 等辅助表在
    /// `aux.db`。
    Durable,
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
    /// 存储模式；默认 [`StorageMode::Legacy`]。
    pub storage: StorageMode,
    /// 测试注入的模型后端。
    pub llm: Option<Arc<dyn LlmClient>>,
    /// 测试注入的无状态模型后端（只有 Durable 的 harness 用它）；`None` = 按配置造
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
            .field("storage", &self.storage)
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
        // 监听的进程组不跨停机（`docs/monitor.md`）：先终止，登记留给下一代启动时收。
        if let Some(monitors) = self.state.monitors.get() {
            monitors.shutdown().await;
        }
        self.shutdown.cancel();
        if let Some(core) = &self.state.durable {
            // 引擎收尾：封住受理、等 invocation 走完、关库（放掉 Turso 的文件锁）。
            match tokio::time::timeout(DRAIN, core.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "durable.db 没有干净地关掉"),
                Err(_) => tracing::warn!("durable.db 收尾超时，下次启动从 checkpoint 继续"),
            }
        }
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

    // 3. 打开 state.db（Turso 对 db 文件持进程独占锁，所以这一步也再证一次只有一个）。
    //    Durable 模式**不碰 state.db**：同一套 toasty 模型与 migration 开在 `aux.db` 上，装 cron /
    //    投递 / memory / 幂等键这些辅助表（决定 4）；`durable.db` 在下面随工具一起打开。
    let durable_mode = options.storage == StorageMode::Durable;
    let db_path = if durable_mode {
        home.join(state::AUX_DB_FILE)
    } else {
        snapshot.start_only.db_path.clone()
    };
    let db = Db::connect(&db_path)
        .await
        .map_err(|error| ServiceError::Store(format!("{}：{error}", db_path.display())))?;

    // 4. 装配。
    // 转后台的接手方要等整份状态才造得出来：`bash` 先拿着空槽，`wire_monitors` 再接上。
    let detach_slot = Arc::new(komo_runtime::tools::detach::DetachSlot::default());
    let (tools, codemode, mcp) = build_tools(&config, &instance_id, &detach_slot).await;

    // Durable：钩子要等 `GatewayState` 造好才找得到它，环用槽断开（`durable::assemble`）。
    let state_slot = durable::assemble::new_slot();
    let durable = if durable_mode {
        let stream = options.stream.clone().unwrap_or_else(|| {
            state::build_stream(&snapshot, &config, &EffortCapabilities::builtin())
        });
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
        Some(core)
    } else {
        None
    };

    let state = match GatewayState::assemble(Assembly {
        home: home.clone(),
        config: Arc::clone(&config),
        caps: EffortCapabilities::builtin(),
        db: db.clone(),
        durable: durable.clone(),
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
            if let Some(core) = &durable {
                let _ = core.close().await;
            }
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
    if let Some(core) = &durable {
        // Durable：把 `GatewayState` 交给钩子，然后才开始调度。旧的 `task` / `monitor` 接缝
        // 不接（编排走 harness；W2c 起监听由 `MonitorRunner` 接）。
        durable::assemble::wire(&state_slot, &state, &detach_slot);
        core.resume();
    } else {
        // `task` 的接缝：`GatewayTaskSpawner` 自己要一份 `Arc<GatewayState>`
        // （建会话、`submit`），与上面这行同一个"先有整份状态才能自己引用自己"的理由
        // （`docs/background-tasks.md`）。
        state.wire_task_spawner();
        // `monitor` / `monitor_stop` 的接缝，同一个接法（`docs/monitor.md`）。
        state.wire_monitors(&detach_slot);
    }
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

    let shutdown = Shutdown::new();

    // 7–8. 启动对账、审计补写、记忆队列复位、重挂看客、遗留监听：Legacy 的恢复链。
    //      Durable 一步不做——恢复由引擎开库时的 reconciliation 与 W4a 的启动补做负责。
    if durable_mode {
        // 把卡在 `processing` 的记忆处理放回 pending（aux.db 里的旧表，W3c 换成新游标的复位）。
        // 它同时是 aux.db 的**第一次写**：只建不写的新文件，Turso 在同一进程里重开会报
        // `database is locked`（实测；写过一次就好）。生产里重启是新进程，不受影响；测试里
        // `stop` 后同一数据目录重起要靠这一下。
        if let Err(error) = state.requeue_memory_work().await {
            tracing::warn!(%error, "记忆处理队列的复位没做成");
        }
        // W4a：做完停在 `closing` 的删除；W3a：重挂看客。
    } else {
        legacy_recovery(&state, &home).await;
    }

    // 9. 后台任务：调度器、Cron、配置轮询、SIGHUP（Durable：只有配置轮询与 SIGHUP，其余各包再加）。
    if durable_mode {
        durable::background::spawn(&state, &shutdown);
        // 提交订阅 → 旧事件词汇 → `EventHub`（SSE 与渠道看客都从这里读）。返回时订阅已生效。
        if let Some(core) = state.durable.as_ref() {
            durable::feed::spawn(core, &state.hub, &shutdown).await;
        }
    } else {
        spawn_background(&state, &shutdown);
    }

    // 10. 渠道与 HTTP。
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

    // 11. 补发积压的投递。**渠道全部登记之后**才有意义：`DeliveryLog::send_recorded`
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

/// Legacy 的启动恢复链（§8.7 的第 7、8 步及 8b–8d）：对账、审计补写、记忆队列复位、重挂
/// 看客、遗留监听收尾。Durable 模式不走这里。
async fn legacy_recovery(state: &Arc<GatewayState>, home: &std::path::Path) {
    // 7. 启动对账（§8.7 的第 7 步就是 §8.9 的 reconcile）：**只补索引与派生状态，不调用
    //    工具、不发外部请求**（§8.5）。它同时把"输入已在 JSONL 里而索引落后"的补齐、
    //    把没主人的 `running` 判成 `queued` 或 `waiting + intervention`、把 `closing`
    //    且已无未完成 Run 的会话推进到 `deleted`、把墓碑已落而内容还在的补齐回收。
    //
    //    2026-10-11 之前停在审批上的 Run（§7 删掉审批之后不再产生）走同一条恢复路径：
    //    那次调用 planned 而没 started，按 §8.4 自动执行，不另做什么。
    //    （**不要**在这一步之前另读一遍 `legacy_approval_waits()`：实测那一次读会让同一个
    //    池里随后的读拿到旧快照——Run 行读成 `None`、新会话 404。见报告。）
    match state.reconcile_reporting().await {
        Ok((report, scan)) => {
            tracing::info!(
                checked = report.checked,
                resumed = report.resumed,
                blocked = report.blocked,
                reclaimed = report.reclaimed,
                closed = report.closed,
                purged = report.purged,
                summary = %scan.summary(),
                "启动对账完成"
            );
            // 2026-10-12 之前停在"结果不明"上等人核对的 Run：结果不明现在交给模型判断，
            // 放回队列，模型在回放里看见那条 uncertain 结果，自己核对再决定（§8.6）。
            requeue_legacy_verify(state, home).await;
            // §8.7：「就绪之后」才做两件不影响调度事实的事——补发与报告。这两步在这里
            // 之外没有别的入口，而它们都是网络 / 界面的事，所以对账本身不发请求。
            for outcome in scan.to_redeliver() {
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
            for group in scan.corrupt_groups() {
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
                // 与周期兜底共用权威 Intervention 的身份，重启不再生成另一行。
                let key = match state.intervention(group.runs[0].as_str()).await {
                    Ok(Some(detail)) => crate::notifier::detail_key(&detail),
                    _ => None,
                };
                let message = Outbound::NeedsAttention {
                    session: group.session,
                    run: group.runs[0].clone(),
                    reason: format!("恢复时停下了：{}{affected}", group.reason),
                };
                if let Some(key) = key {
                    let _ = state.notifier.deliver_home_once(message, &key).await;
                } else {
                    let _ = state.notifier.deliver_home(message).await;
                }
            }
            if scan.requeued() > 0 {
                state.waker().wake();
            }
        }
        Err(error) => tracing::error!(%error, "启动对账失败：新请求照常，未完成的任务等下一拍"),
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

    // 8c. 把跨重启还在跑的交互 Run 与后台任务 Run 重新挂上看客：看客只活在内存里，重启后没人订阅就没人能把最终回复投回聊天来源。
    // **必须在下一步调度器起来之前**：调度器一起，这些 Run 可能立刻继续往下跑。
    run_watch::reattach_unfinished(state).await;

    // 8d. 上一代留下的监听：遗留进程组核对身份后终止，挂它的会话各收到一条"中断了"
    // （`docs/monitor.md`）。监听不跨重启，这里只收尾、不重挂。
    if let Some(monitors) = state.monitors.get() {
        monitors.recover_interrupted().await;
    }
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
                        // chat"）：Cron 没有来源会话，要人处理的就只能从这里投出去。
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
        //
        // **不只靠这一拍**：刚排进 outbox 的审计事件（会话生命周期）会按 `audit_wake`
        // 立刻叫它。周期留下当兜底：漏掉的、别处写进去的，一拍之内照样会补上。
        let state = Arc::clone(state);
        let shutdown = shutdown.clone();
        let wake = Arc::clone(&state.audit_wake);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(AUDIT_TICK) => {}
                    _ = wake.notified() => {}
                }
                if shutdown.is_cancelled() {
                    return;
                }
                state.drain_audit().await;
                // 同一拍里顺手兜底：有人看着的介入只弹在他面前，而看着的人可能已经
                // 关掉界面走了——挂着没人知道的介入是 §10 明确要挡的状态。
                state.sweep_unseen_interventions().await;
                // §8.9 的第三个触发点：`AUDIT_TICK` 那一拍的周期对账（另外两个是启动与
                // `POST /v1/reconcile`）。「它可以比补写粗一点，比如每拍最多干一次」——
                // 它本来就是六十秒一拍，所以每拍一次就够；代价是几十条 SELECT。
                //
                // 它只**写状态**：不调工具、不发外部请求、不消费授权（§8.9）。
                if let Err(error) = state.reconcile().await {
                    tracing::warn!(%error, "这一拍的周期对账没跑成");
                }
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
                        if report.processed > 0
                            || report.failed > 0
                            || report.skipped > 0
                            || report.abandoned > 0 =>
                    {
                        tracing::info!(
                            processed = report.processed,
                            applied = report.applied,
                            skipped = report.skipped,
                            failed = report.failed,
                            abandoned = report.abandoned,
                            "记忆提取这一轮结束"
                        );
                        // 写进了变更就补向量（§9.3「标记相关索引待更新 → 后台生成向量」）。
                        // 没有向量后端时什么都不做：关键词索引已经随正文写好了。
                        if report.applied > 0
                            && state.memories.space().is_some()
                            && let Err(error) = state.memories.schedule_index()
                        {
                            tracing::warn!(%error, "索引补齐没提交上，新记忆先只靠关键词命中");
                        }
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

/// 把升级前停在"结果不明"上等人的 Run 放回队列。失败只记日志：它们仍在清单里，人还能答。
async fn requeue_legacy_verify(state: &GatewayState, home: &std::path::Path) {
    #[cfg(any(test, feature = "test-support"))]
    if !test_support::requeues_legacy_verify(home) {
        return;
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = home;
    let runs = match state.recovery_store.legacy_verify_waits().await {
        Ok(runs) => runs,
        Err(error) => {
            tracing::warn!(%error, "读不出停在结果不明上的旧 Run，留在清单里");
            return;
        }
    };
    for run in runs {
        match state.wake_run(&run).await {
            Ok(()) => tracing::info!(run = %run, "旧的结果不明交给模型判断，放回队列"),
            Err(error) => tracing::warn!(run = %run, %error, "旧的结果不明放不回队列，留在清单里"),
        }
    }
}
