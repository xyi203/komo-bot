//! codemode 的脚本引擎与 worker 协议（`docs/codemode.md`）。
//!
//! 这里只管"在 worker 里跑一段 Python、把它的工具调用交出去"。工具调用怎么判、怎么执行在
//! executor（`run_codemode` / `nested_call`）——这里拿到的只是一个回调。
//!
//! 引擎是 pydantic Monty：语言本身没有文件、网络、子进程，所以不需要操作系统沙箱，也就没有
//! "沙箱起不来就不注册 codemode"这回事。每次调用起一个 worker 进程（`komo __codemode-worker`，
//! 本模块的 [`worker`]），宿主管墙钟、内存与取消，到限就 SIGKILL。

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

mod convert;
pub mod worker;

/// 脚本引擎的版本：写进 journal 的 `start` 指纹，升级后在途的 codemode 调用恢复时按分歧处理。
/// 与根 `Cargo.toml` 里 `monty` 的精确钉版本同步（有测试守着）。
pub const MONTY_VERSION: &str = "1.1.0";
/// worker 协议版本：请求 / 消息 / 脚本前置代码（`prelude.py`）任一变了就加一。
pub const WORKER_PROTOCOL: u32 = 1;

/// 脚本和 worker 合计的常驻内存上限：既是 Monty 的软限，也是宿主看门狗的杀线。
const DEFAULT_MEMORY_LIMIT: u64 = 1 << 30;
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(100);

/// 怎么起一个 worker。
#[derive(Debug, Clone)]
pub struct Worker {
    launch: Launch,
    memory_limit: u64,
}

#[derive(Debug, Clone)]
enum Launch {
    /// 起 `<exe> __codemode-worker`。
    Process(PathBuf),
    /// 测试用：同进程里的一个线程，经 socketpair 说同一套协议。杀不掉，只靠 Monty 自己的
    /// 时间限制收尾，所以死循环、内存看门狗这类要杀进程的验收走真进程（`crates/komo/tests`）。
    #[cfg(any(test, feature = "test-support"))]
    InProcess,
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
    /// 脚本抛出的异常（traceback）或 worker 层的失败。`None` = 正常跑完。
    pub error: Option<String>,
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
            memory_limit: DEFAULT_MEMORY_LIMIT,
        }
    }

    /// 当前进程的可执行文件，Gateway 启动时解析一次。
    pub fn current_exe() -> std::io::Result<Self> {
        std::env::current_exe().map(Self::exe)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn in_process() -> Self {
        Self {
            launch: Launch::InProcess,
            memory_limit: DEFAULT_MEMORY_LIMIT,
        }
    }

    pub fn with_memory_limit(mut self, bytes: u64) -> Self {
        self.memory_limit = bytes;
        self
    }

    /// 跑一段脚本。`on_call(name, args)` 回答脚本里的每一次工具调用；`catalog` 是脚本里
    /// `ALL_TOOLS` / `search_tools` / `describe_tool` / `describe_namespace` 要的说明
    /// （`docs/codemode.md` §6），发现在 worker 里本地做完，不经宿主、不进 journal。
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
                "limits": {
                    "max_memory": self.memory_limit,
                    "max_turn_ms": self.in_process_turn_limit(),
                },
            });
            if let Err(error) = write_line(&mut link.writer, &first).await {
                return Err(format!("脚本交不进 worker：{error}"));
            }
            let mut lines = (&mut link.reader).lines();
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => return Err("脚本进程提前退出".to_string()),
                    Err(error) => return Err(format!("读不到 worker 输出：{error}")),
                };
                let message: serde_json::Value = serde_json::from_str(&line)
                    .map_err(|error| format!("worker 输出不是协议：{error}"))?;
                if let Some(done) = message.get("done") {
                    return serde_json::from_value::<worker::Done>(done.clone())
                        .map_err(|error| format!("结束消息读不懂：{error}"));
                }
                let Some(name) = message.get("call").and_then(|name| name.as_str()) else {
                    return Err(format!("worker 输出不是协议：{line}"));
                };
                let args = message.get("args").cloned().unwrap_or_default();
                let reply = match on_call(name.to_string(), args).await {
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
                Ok(Err(error)) => failed(with_stderr_tail(error, stderr.as_mut()).await),
                Ok(Ok(done)) => ScriptOutcome {
                    outputs: done.outputs,
                    console: done.console,
                    error: done.error,
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
            Launch::InProcess => {
                let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
                ours.set_nonblocking(true)?;
                let input = theirs.try_clone()?;
                std::thread::Builder::new()
                    .stack_size(worker::STACK_SIZE)
                    .spawn(move || worker::serve(std::io::BufReader::new(input), theirs))?;
                let (reader, writer) = tokio::io::split(tokio::net::UnixStream::from_std(ours)?);
                Ok(Link {
                    reader: BufReader::new(Box::new(reader)),
                    writer: Box::new(writer),
                    child: None,
                })
            }
        }
    }

    /// 进程内的 worker 杀不掉，让 Monty 自己在时限后停下，免得死循环的线程在测试进程里
    /// 一直占着一个核。
    fn in_process_turn_limit(&self) -> Option<u64> {
        match self.launch {
            Launch::Process(_) => None,
            #[cfg(any(test, feature = "test-support"))]
            Launch::InProcess => Some(20_000),
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
