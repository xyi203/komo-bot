//! codemode 的脚本引擎与 worker 协议（`docs/codemode.md`）。
//!
//! 这里只管“在 worker 里跑一段 TypeScript / JavaScript、把它的工具调用交出去”。工具调用怎么判、
//! 怎么执行在 executor / harness（`durable::harness::codemode`）——这里拿到的只是一个回调。
//!
//! 引擎是 `earendil-works/djs`（纯 Rust JS/TS 引擎）：语言本身零 I/O（没有文件、网络、子进程、
//! 环境变量），自带不可捕获堆上限与 Fuel 切片中断，不需要操作系统沙箱。每次调用仍起一个独立 worker
//! 进程（`komo __codemode-worker`，本模块的 [`worker`]），宿主管墙钟、RSS 看门狗与取消。

use std::collections::HashMap;
use std::future::Future;
use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub mod djs;
pub mod engine;
pub mod worker;

/// worker 协议版本：请求 / 消息 / 前置脚本（`prelude.js`）/ worker 行为任一变了就加一——握手只比它与
/// 引擎构建标识（[`engine_version`]），`komo` 版本号不同照常跑（[`Hello::compatible_with`]）。
/// 5：切换为 djs（JS/TS）引擎与流式 `wait`（先完先交付 + 交付序记账）。
pub const WORKER_PROTOCOL: u32 = 5;
/// 一次熵请求最多向宿主要这么多字节。
const MAX_URANDOM: usize = 1 << 16;

/// 脚本和 worker 合计的内存上限：既是 djs 的硬堆上限，也是宿主看门狗的杀线。
const DEFAULT_MEMORY_LIMIT: u64 = 1 << 30;
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(100);

/// 当前脚本引擎的构建标识：写进握手与 `codemode.start` 指纹。
pub fn engine_version() -> String {
    djs::build_id()
}

/// worker 启动后的第一行：它是哪一版。宿主与 worker 是同一个可执行文件，但磁盘上的二进制会在
/// Gateway 重启之前被换掉（`komo update` / `scripts/install-local.sh`），新起的 worker 就可能
/// 不是 Gateway 的那一版：协议或引擎构建号对不上就干净地失败，只差 `komo` 版本号照常跑
/// （`docs/codemode.md` §4）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub engine: String,
    pub komo: String,
}

impl Hello {
    pub fn current() -> Self {
        Self {
            protocol: WORKER_PROTOCOL,
            engine: engine_version(),
            komo: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// 能不能把脚本交给这个 worker：只看协议号与引擎构建标识。`komo` 版本不同（`komo update`
    /// 换了磁盘上的二进制、Gateway 还没重启）只要协议与引擎没变就照常跑——会改 worker 行为的变化
    /// 都要升 [`WORKER_PROTOCOL`]（`docs/optimization-design.md` §5.4）。
    pub fn compatible_with(&self, ours: &Hello) -> bool {
        self.protocol == ours.protocol && self.engine == ours.engine
    }
}

/// 脚本或流式调度需要宿主记账、恢复时原样回放的不确定事件：时钟、熵与流式 `wait` 的交付批次
/// （`docs/codemode.md` §7）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Nondet {
    /// 当前时刻，答 Unix 秒（f64）。
    Clock,
    /// `size` 个随机字节，答小写十六进制串。
    Urandom { size: usize },
    /// 流式 `wait` 在 `waiting` 上挂起时，宿主实际交付的那一批 id（用于多跳并发重放确定性）。
    Deliver { waiting: Vec<u32> },
}

/// 回答一次 [`Nondet`]。默认是真值（[`live`]）；harness 换成“先还录下来的、录完了再读真的并记账”。
pub type NondetHandler = Arc<dyn Fn(&Nondet) -> Result<serde_json::Value, String> + Send + Sync>;

/// 流式 `wait` 交付序的回放与记账接口：
/// - 回放中：`next_recorded(waiting)` 返回首次运行记下的那批交付 id，宿主等这批全齐再按同序交付；
/// - 越过记录之后：`next_recorded` 返回 `Ok(None)`，宿主等任一完成先交付并调 `record(waiting, delivered)`。
pub trait DeliverOracle: Send + Sync {
    fn next_recorded(&self, waiting: &[u32]) -> Result<Option<Vec<u32>>, String>;
    fn record(&self, waiting: &[u32], delivered: &[u32]) -> Result<(), String>;
}

/// 真实的时钟与熵。
pub fn live(request: &Nondet) -> Result<serde_json::Value, String> {
    match request {
        Nondet::Clock => {
            let micros = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| format!("系统时钟在 1970 年之前：{error}"))?
                .as_micros();
            Ok(serde_json::json!(micros as f64 / 1e6))
        }
        Nondet::Urandom { size } => {
            let mut bytes = vec![0u8; (*size).min(MAX_URANDOM)];
            std::fs::File::open("/dev/urandom")
                .and_then(|mut file| file.read_exact(&mut bytes))
                .map_err(|error| format!("读不到系统熵：{error}"))?;
            Ok(serde_json::Value::String(
                bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
            ))
        }
        Nondet::Deliver { .. } => Err("Deliver 不走 live()".to_owned()),
    }
}

/// 脚本前置代码（`prelude.js`）的哈希：写进 `codemode.start` 指纹，它变了意味着同一段脚本的
/// 行为可能变了。
pub fn prelude_hash() -> komo_kernel::types::digest::ContentHash {
    komo_kernel::types::digest::ContentHash::of_str(djs::PRELUDE)
}

/// 怎么起一个 worker。
#[derive(Clone)]
pub struct Worker {
    launch: Launch,
    memory_limit: u64,
    /// 脚本的工作目录。
    cwd: Option<PathBuf>,
    nondet: NondetHandler,
    deliver: Option<Arc<dyn DeliverOracle>>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("launch", &self.launch)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
enum Launch {
    /// 起 `<exe> __codemode-worker`。
    Process(PathBuf),
    /// 测试用：同进程里的一个线程，经 socketpair 说同一套协议。djs 自带 Fuel 切片与硬内存限额，
    /// 进程内即可精确中断。
    #[cfg(any(test, feature = "test-support"))]
    InProcess(Hello),
}

/// 脚本里发现工具用的说明：这次能调的工具与它们所属的组。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ScriptCatalog {
    pub tools: Vec<ScriptTool>,
    pub namespaces: Vec<komo_kernel::types::tool::ToolNamespace>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScriptTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    /// 所属组的名字（[`komo_kernel::types::tool::ToolNamespace::name`]）。
    pub namespace: Option<String>,
}

/// 一段脚本跑完的样子。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptOutcome {
    pub outputs: Vec<String>,
    pub console: String,
    /// 脚本抛出的异常或 worker 层的失败。`None` = 正常跑完。
    pub error: Option<String>,
    /// worker 与宿主不是同一版（[`Hello`] 对不上）：脚本一行都没跑，宿主不该把它记成脚本的结果。
    pub incompatible: Option<String>,
}

/// 与 worker 对话中途停下的两种理由。
enum Stop {
    Failed(String),
    Incompatible(String),
}

impl From<String> for Stop {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// 与 worker 的两端，外加要在结束时收拾的东西。
struct Link {
    reader: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    child: Option<tokio::process::Child>,
}

impl Worker {
    pub fn exe(path: impl Into<PathBuf>) -> Self {
        Self {
            launch: Launch::Process(path.into()),
            ..Self::base()
        }
    }

    /// 当前进程的可执行文件，Gateway 启动时解析一次。
    pub fn current_exe() -> std::io::Result<Self> {
        std::env::current_exe().map(Self::exe)
    }

    fn base() -> Self {
        Self {
            launch: Launch::Process(PathBuf::new()),
            memory_limit: DEFAULT_MEMORY_LIMIT,
            cwd: None,
            nondet: Arc::new(live),
            deliver: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn in_process() -> Self {
        Self::in_process_as(Hello::current())
    }

    /// 进程内 worker，自称是另一版（握手测试用）。
    #[cfg(any(test, feature = "test-support"))]
    pub fn in_process_as(hello: Hello) -> Self {
        Self {
            launch: Launch::InProcess(hello),
            ..Self::base()
        }
    }

    /// 这一次运行的工作目录。
    pub fn in_dir(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// 这一次运行怎么回答时钟与熵。
    pub fn with_nondet(mut self, handler: NondetHandler) -> Self {
        self.nondet = handler;
        self
    }

    /// 流式 `wait` 的交付序记账/回放口（harness 注入）。
    pub fn with_deliver(mut self, oracle: Arc<dyn DeliverOracle>) -> Self {
        self.deliver = Some(oracle);
        self
    }

    pub fn with_memory_limit(mut self, bytes: u64) -> Self {
        self.memory_limit = bytes;
        self
    }

    /// 跑一段脚本。`on_call(name, args)` 回答脚本里的每一次工具调用，按收到的顺序同步地调它
    /// （编号之类要在这里定），返回的 future 可与别的调用并发跑；脚本结束时还在跑的也等它们
    /// 跑完再返回。`catalog` 是脚本里 `ALL_TOOLS` / `searchTools` / `describeTool` /
    /// `describeNamespace` 要的说明（`docs/codemode.md` §6），发现在 worker 里本地做完。
    ///
    /// 时限到了、内存超了都 SIGKILL worker；外层 future 被丢掉（取消）时进程随之被杀
    /// （`kill_on_drop`）。
    pub async fn run<F, Fut>(
        &self,
        code: &str,
        tools: &[String],
        catalog: &ScriptCatalog,
        timeout: Duration,
        mut on_call: F,
    ) -> ScriptOutcome
    where
        F: FnMut(String, serde_json::Value) -> Fut,
        Fut: Future<Output = Result<serde_json::Value, String>>,
    {
        let mut link = match self.spawn() {
            Ok(link) => link,
            Err(error) => return failed(format!("脚本进程起不来：{error}")),
        };
        let pid = link.child.as_ref().and_then(|child| child.id());
        let mut stderr = link.child.as_mut().and_then(|child| child.stderr.take());

        let conversation = async {
            let first = serde_json::json!({
                "code": code,
                "tools": tools,
                "catalog": catalog,
                "cwd": self.cwd,
                "limits": {
                    "max_memory": self.memory_limit,
                    "max_turn_ms": self.in_process_turn_limit(),
                },
            });
            let mut lines = (&mut link.reader).lines();
            let hello = match lines.next_line().await {
                Ok(Some(line)) => serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|message| message.get("hello").cloned())
                    .and_then(|hello| serde_json::from_value::<Hello>(hello).ok())
                    .ok_or_else(|| format!("worker 第一行不是握手：{line}"))?,
                Ok(None) => return Err("脚本进程提前退出".to_string().into()),
                Err(error) => return Err(format!("读不到 worker 输出：{error}").into()),
            };
            let ours = Hello::current();
            if !hello.compatible_with(&ours) {
                return Err(Stop::Incompatible(format!(
                    "codemode 的 worker 与 Gateway 不是同一版（worker：协议 {} / 引擎 {} / komo {}；Gateway：协议 {} / 引擎 {} / komo {}）。\
                     磁盘上的 komo 在 Gateway 启动之后被换过，需要重启 Gateway（komo gateway restart）；脚本没有运行。",
                    hello.protocol, hello.engine, hello.komo, ours.protocol, ours.engine, ours.komo
                )));
            }
            if hello.komo != ours.komo {
                tracing::warn!(
                    worker = %hello.komo,
                    gateway = %ours.komo,
                    "codemode worker 与 Gateway 的 komo 版本不同（协议与引擎一致，照常运行）；磁盘上的 komo 已更新，重启 Gateway 才用上新版"
                );
            }
            if let Err(error) = write_line(&mut link.writer, &first).await {
                return Err(format!("脚本交不进 worker：{error}").into());
            }
            // 异步调用：在跑的与跑完还没交给 worker 的。等 worker 下一行、等同步调用时，
            // 它们都照样往前跑。
            let mut inflight = FuturesUnordered::new();
            let mut finished: HashMap<u32, Result<serde_json::Value, String>> = HashMap::new();
            loop {
                let next = tokio::select! {
                    line = lines.next_line() => line,
                    Some((id, result)) = inflight.next(), if !inflight.is_empty() => {
                        finished.insert(id, result);
                        continue;
                    }
                };
                let line = match next {
                    Ok(Some(line)) => line,
                    Ok(None) => return Err("脚本进程提前退出".to_string().into()),
                    Err(error) => return Err(format!("读不到 worker 输出：{error}").into()),
                };
                let message: serde_json::Value = serde_json::from_str(&line)
                    .map_err(|error| format!("worker 输出不是协议：{error}"))?;
                if let Some(done) = message.get("done") {
                    // 脚本发出去没等的调用也已经在做了：等它们落下结果。
                    while inflight.next().await.is_some() {}
                    return serde_json::from_value::<worker::Done>(done.clone())
                        .map_err(|error| format!("结束消息读不懂：{error}").into());
                }
                if let Some(ids) = message.get("wait") {
                    let ids: Vec<u32> = serde_json::from_value(ids.clone())
                        .map_err(|error| format!("wait 读不懂：{error}"))?;
                    let deliver_ids = match self
                        .deliver
                        .as_ref()
                        .map(|oracle| oracle.next_recorded(&ids))
                        .transpose()
                    {
                        Ok(Some(Some(recorded))) => {
                            // 回放模式：等录下的这一批全部就绪，再按原顺序交付。
                            while !recorded.iter().all(|id| finished.contains_key(id)) {
                                match inflight.next().await {
                                    Some((id, result)) => {
                                        finished.insert(id, result);
                                    }
                                    None => break,
                                }
                            }
                            recorded
                        }
                        Ok(Some(None)) | Ok(None) => {
                            // 流式模式：只要 ids 里有至少 1 个已完成就立刻交付，不卡在慢调用上。
                            while !ids.iter().any(|id| finished.contains_key(id)) {
                                match inflight.next().await {
                                    Some((id, result)) => {
                                        finished.insert(id, result);
                                    }
                                    None => break,
                                }
                            }
                            // 同一拍已经一起跑完的顺手捎上，减少往返次数。
                            while let Some(Some((id, result))) = inflight.next().now_or_never() {
                                finished.insert(id, result);
                            }
                            let ready: Vec<u32> = ids
                                .iter()
                                .copied()
                                .filter(|id| finished.contains_key(id))
                                .collect();
                            let ready = if ready.is_empty() { ids.clone() } else { ready };
                            if let Some(oracle) = &self.deliver
                                && let Err(error) = oracle.record(&ids, &ready)
                            {
                                let results: Vec<serde_json::Value> = ids
                                    .iter()
                                    .map(|id| serde_json::json!({ "id": id, "error": error }))
                                    .collect();
                                write_line(
                                    &mut link.writer,
                                    &serde_json::json!({ "results": results }),
                                )
                                .await
                                .map_err(|e| format!("答复交不进 worker：{e}"))?;
                                continue;
                            }
                            ready
                        }
                        Err(error) => {
                            let results: Vec<serde_json::Value> = ids
                                .iter()
                                .map(|id| serde_json::json!({ "id": id, "error": error }))
                                .collect();
                            write_line(
                                &mut link.writer,
                                &serde_json::json!({ "results": results }),
                            )
                            .await
                            .map_err(|e| format!("答复交不进 worker：{e}"))?;
                            continue;
                        }
                    };
                    let results: Vec<serde_json::Value> = deliver_ids
                        .iter()
                        .map(|id| match finished.remove(id) {
                            Some(Ok(value)) => serde_json::json!({ "id": id, "ok": value }),
                            Some(Err(error)) => serde_json::json!({ "id": id, "error": error }),
                            None => serde_json::json!({ "id": id, "error": "宿主没有这次调用" }),
                        })
                        .collect();
                    write_line(&mut link.writer, &serde_json::json!({ "results": results }))
                        .await
                        .map_err(|error| format!("答复交不进 worker：{error}"))?;
                    continue;
                }
                if let Some(request) = message.get("os") {
                    let reply = match serde_json::from_value::<Nondet>(request.clone()) {
                        Ok(request) => match (self.nondet)(&request) {
                            Ok(value) => serde_json::json!({ "ok": value }),
                            Err(error) => serde_json::json!({ "error": error }),
                        },
                        Err(error) => {
                            serde_json::json!({ "error": format!("os 请求读不懂：{error}") })
                        }
                    };
                    write_line(&mut link.writer, &reply)
                        .await
                        .map_err(|error| format!("答复交不进 worker：{error}"))?;
                    continue;
                }
                let Some(name) = message.get("call").and_then(|name| name.as_str()) else {
                    return Err(format!("worker 输出不是协议：{line}").into());
                };
                let args = message.get("args").cloned().unwrap_or_default();
                let call = on_call(name.to_string(), args);
                if message.get("defer").and_then(|defer| defer.as_bool()) == Some(true) {
                    let Some(id) = message.get("id").and_then(|id| id.as_u64()) else {
                        return Err(format!("延后调用没有 id：{line}").into());
                    };
                    inflight.push(call.map(move |result| (id as u32, result)));
                    continue;
                }
                // 同步调用：worker 在等这一个，在跑的异步调用照样往前走。
                let mut call = std::pin::pin!(call);
                let result = loop {
                    tokio::select! {
                        result = &mut call => break result,
                        Some((id, result)) = inflight.next(), if !inflight.is_empty() => {
                            finished.insert(id, result);
                        }
                    }
                };
                let reply = match result {
                    Ok(value) => serde_json::json!({ "ok": value }),
                    Err(error) => serde_json::json!({ "error": error }),
                };
                write_line(&mut link.writer, &reply)
                    .await
                    .map_err(|error| format!("答复交不进 worker：{error}"))?;
            }
        };

        let outcome = tokio::select! {
            result = tokio::time::timeout(timeout, conversation) => match result {
                Err(_elapsed) => failed(format!("脚本超时（{}s），已终止", timeout.as_secs())),
                Ok(Err(Stop::Failed(error))) => {
                    failed(with_stderr_tail(error, stderr.as_mut()).await)
                }
                Ok(Err(Stop::Incompatible(message))) => ScriptOutcome {
                    incompatible: Some(message),
                    ..ScriptOutcome::default()
                },
                Ok(Ok(done)) => ScriptOutcome {
                    outputs: done.outputs,
                    console: done.console,
                    error: done.error,
                    incompatible: None,
                },
            },
            resident = over_memory(pid, self.memory_limit) => failed(format!(
                "脚本内存超限（{} MiB > {} MiB），已终止",
                resident >> 20,
                self.memory_limit >> 20
            )),
        };
        if let Some(child) = link.child.as_mut() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        }
        outcome
    }

    fn spawn(&self) -> std::io::Result<Link> {
        match &self.launch {
            Launch::Process(exe) => {
                let mut child = tokio::process::Command::new(exe)
                    .arg("__codemode-worker")
                    .env_clear()
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .process_group(0)
                    .spawn()?;
                let writer = child.stdin.take().expect("stdin 是 piped");
                let reader = child.stdout.take().expect("stdout 是 piped");
                Ok(Link {
                    reader: BufReader::new(Box::new(reader)),
                    writer: Box::new(writer),
                    child: Some(child),
                })
            }
            #[cfg(any(test, feature = "test-support"))]
            Launch::InProcess(hello) => {
                let hello = hello.clone();
                let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
                ours.set_nonblocking(true)?;
                let input = theirs.try_clone()?;
                std::thread::Builder::new()
                    .stack_size(worker::STACK_SIZE)
                    .spawn(move || worker::serve(std::io::BufReader::new(input), theirs, hello))?;
                let (reader, writer) = tokio::io::split(tokio::net::UnixStream::from_std(ours)?);
                Ok(Link {
                    reader: BufReader::new(Box::new(reader)),
                    writer: Box::new(writer),
                    child: None,
                })
            }
        }
    }

    /// 进程内的 worker 杀不掉，让 djs 自己在时限后停下，免得死循环的线程在测试进程里一直占着一个核。
    fn in_process_turn_limit(&self) -> Option<u64> {
        match self.launch {
            Launch::Process(_) => None,
            #[cfg(any(test, feature = "test-support"))]
            Launch::InProcess(_) => Some(20_000),
        }
    }
}

/// worker 的常驻内存超过 `limit` 时返回读数。拿不到 pid（进程内 worker）就永远不返回。
async fn over_memory(pid: Option<u32>, limit: u64) -> u64 {
    let Some(pid) = pid else {
        return std::future::pending().await;
    };
    let mut interval = tokio::time::interval(WATCHDOG_INTERVAL);
    loop {
        interval.tick().await;
        if let Some(resident) = crate::limits::resident_bytes(pid)
            && resident > limit
        {
            return resident;
        }
    }
}

async fn with_stderr_tail(
    error: String,
    stderr: Option<&mut tokio::process::ChildStderr>,
) -> String {
    let Some(stderr) = stderr else {
        return error;
    };
    let mut tail = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(1), stderr.read_to_string(&mut tail)).await;
    let tail = tail.trim();
    if tail.is_empty() {
        error
    } else {
        format!("{error}\n{tail}")
    }
}

fn failed(error: String) -> ScriptOutcome {
    ScriptOutcome {
        error: Some(error),
        ..ScriptOutcome::default()
    }
}

async fn write_line(
    writer: &mut (dyn AsyncWrite + Unpin + Send),
    value: &serde_json::Value,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests;
