//! 子进程：自己的进程组、流式输出、输出上限、超时与取消回收（§4）。
//!
//! `bash` 要的是"起一个进程，把它的 stdout / stderr 边跑边写进 [`OutputWriter`]，
//! 超时或取消时**终止整个进程组并等待回收**"。Tokio 不会因为 handle 被丢掉就结束进程，而 `Child::kill` 只杀直接子进程
//! ——`sh -c 'sleep 30 &'` 的孙子进程会活下来，所以信号必须发给进程组。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use komo_kernel::traits::OutputWriter;
use komo_kernel::types::ids::ExecutorId;
use komo_kernel::types::tool::CancelToken;

use crate::recovery::{ChildProcess, ChildRegistry};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::executor::cancel::cancelled;

/// 取消或超时后，等进程组真正消失的上限。
const REAP_TIMEOUT: Duration = Duration::from_secs(5);
/// 预览保留的头、尾字节数。kernel 最终按工具类型取最多 50 KiB；这里两端都至少保留同样
/// 多的原始字节，避免工具在统一投影之前先把模型需要的那一端丢掉。
const HEAD_LIMIT: usize = 50 * 1024;
const TAIL_LIMIT: usize = 50 * 1024;

/// 谁给这次子进程登记在册（§8.7）。
///
/// 「异常退出可能遗留子进程」——启动子进程的那一方留一行，恢复扫描才核实得了"上一个
/// 执行实例真的停了吗"。`None` = 不登记：测试里没有 runtime 目录，而一个登记不了的
/// 子进程不该让命令跑不起来。
#[derive(Debug, Clone)]
pub struct ChildRegistration {
    pub registry: Arc<ChildRegistry>,
    pub executor: ExecutorId,
}

/// 起一个子进程要的全部东西。
#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// **明确的**环境变量集合——继承整个环境等于把凭证交给任意脚本。
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    /// 写进 [`OutputWriter`] 的字节上限；超出后停止转发并置 `truncated`。
    pub output_limit: u64,
    /// 在册登记（§8.7）。
    #[allow(clippy::doc_markdown)]
    pub register: Option<ChildRegistration>,
    /// 给操作者看的一句话，写进登记行。
    pub label: String,
}

/// 一次子进程执行的结果。
#[derive(Debug, Clone, Default)]
pub struct ChildOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    /// 输出超过预算，后面的没有写进 sink。
    pub truncated: bool,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    /// 首尾摘录，仅供预览；完整正文在 `ToolOutputStore` 里。
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProcessError {
    #[error("启动失败：{0}")]
    Spawn(String),
    #[error("读取输出失败：{0}")]
    Io(String),
    #[error("写入输出存储失败：{0}")]
    Sink(String),
}

/// 预览用的摘录：开头、结尾各留一段，不把整份输出搬进内存。
///
/// 只留尾巴时，分析脚本先打印的汇总、报错前的上下文都看不到，模型只能再去分页读完整
/// 输出（真实会话里为此多跑过两轮）。
#[derive(Debug, Default)]
struct Excerpt {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
}

impl Excerpt {
    fn push(&mut self, mut chunk: &[u8]) {
        self.total += chunk.len();
        if self.head.len() < HEAD_LIMIT {
            let take = chunk.len().min(HEAD_LIMIT - self.head.len());
            self.head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > TAIL_LIMIT {
            let cut = self.tail.len() - TAIL_LIMIT;
            self.tail.drain(..cut);
        }
    }

    fn to_string_lossy(&self) -> String {
        let head = String::from_utf8_lossy(&self.head);
        let tail = String::from_utf8_lossy(&self.tail);
        let omitted = self.total - self.head.len() - self.tail.len();
        if omitted == 0 {
            format!("{head}{tail}")
        } else {
            format!("{head}\n…（中间省略 {omitted} 字节）…\n{tail}")
        }
    }

    fn into_string(self) -> String {
        self.to_string_lossy()
    }
}

/// 起进程、流式收输出、按需终止进程组。
pub async fn run_child(
    spec: ChildSpec,
    sink: &mut dyn OutputWriter,
    cancel: &CancelToken,
) -> Result<ChildOutcome, ProcessError> {
    match run_child_detachable(spec, sink, cancel, None).await? {
        Driven::Done(outcome) => Ok(outcome),
        Driven::Detached(_) => unreachable!("没有给 detach_after 就不会转后台"),
    }
}

/// 一个跑着、已脱离原调用的子进程：管道、进程组、已经收到的字节数与摘录、从调用起点算
/// 的截止时刻，都带着，由接手的一方（Gateway）用 [`drive`] 接着收。
#[derive(Debug)]
pub struct LiveChild {
    child: tokio::process::Child,
    pub pgid: u32,
    /// 进程起来的时刻，给 pid 复用核对用（与在册登记同一个事实）。
    pub started_at: time::OffsetDateTime,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    outcome: ChildOutcome,
    stdout_excerpt: Excerpt,
    stderr_excerpt: Excerpt,
    exit_status: Option<std::process::ExitStatus>,
    /// 从**调用开始**算的截止（转后台不重置时限）。
    pub deadline: tokio::time::Instant,
    output_limit: u64,
    /// 执行器那一份在册；转后台时销掉，由接手的一方登记自己的（`docs/monitor.md` §8）。
    forget: Option<ForgetOnDrop>,
}

impl LiveChild {
    /// 到目前为止收到的输出摘录与字节数，给"已转后台"的结果用。
    pub fn so_far(&self) -> (u64, u64, String, String) {
        (
            self.outcome.stdout_bytes,
            self.outcome.stderr_bytes,
            self.stdout_excerpt.to_string_lossy(),
            self.stderr_excerpt.to_string_lossy(),
        )
    }

    /// 终止整个进程组并等回收。接手失败、不再需要它时用。
    pub async fn terminate(mut self) {
        reap(&mut self.child, self.pgid).await;
    }
}

/// [`run_child_detachable`] 的两种结局。
#[derive(Debug)]
pub enum Driven {
    Done(ChildOutcome),
    Detached(Box<LiveChild>),
}

/// 与 [`run_child`] 一样，但 `detach_after` 到点进程还在跑就**不杀它**：连同管道交出去。
pub async fn run_child_detachable(
    spec: ChildSpec,
    sink: &mut dyn OutputWriter,
    cancel: &CancelToken,
    detach_after: Option<Duration>,
) -> Result<Driven, ProcessError> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 自己的进程组：取消时一个信号带走整棵子树。
        .process_group(0)
        .kill_on_drop(true);

    // cwd 不在时 spawn 的 ENOENT 会挂在**程序名**上（`/bin/sh: No such file or
    // directory`），真正不在的是工作目录。这一条在现场把人引偏过一次，所以单独认出来。
    if !spec.cwd.is_dir() {
        return Err(ProcessError::Spawn(format!(
            "工作目录不存在：{}",
            spec.cwd.display()
        )));
    }

    let mut child = command
        .spawn()
        .map_err(|error| ProcessError::Spawn(format!("{}: {error}", spec.program)))?;
    let pgid = child
        .id()
        .ok_or_else(|| ProcessError::Spawn("子进程没有 pid".into()))?;
    let started_at = time::OffsetDateTime::now_utc();

    // 在册：留下 pid、进程组与起来的时刻。`started_at` 走真实时钟——它是给 pid 复用
    // 核对用的事实，不是一个判决输入。登记失败只是少了一条恢复线索，不该让命令跑不起来。
    if let Some(registration) = &spec.register
        && let Err(error) = registration.registry.record(
            &registration.executor,
            ChildProcess {
                pid: pgid,
                pgid: Some(pgid),
                started_at: Some(started_at),
                what: spec.label.clone(),
            },
        )
    {
        tracing::warn!(pid = pgid, %error, "子进程登记写不进去");
    }
    // 无论怎么收尾都要销号：正常退出、超时、取消、读输出出错，都从这个函数出去。
    let forget = ForgetOnDrop {
        registration: spec.register.clone(),
        pid: pgid,
    };

    let now = tokio::time::Instant::now();
    let live = LiveChild {
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
        child,
        pgid,
        started_at,
        outcome: ChildOutcome::default(),
        stdout_excerpt: Excerpt::default(),
        stderr_excerpt: Excerpt::default(),
        exit_status: None,
        deadline: now + spec.timeout,
        output_limit: spec.output_limit,
        forget: Some(forget),
    };
    drive(live, sink, cancel, detach_after.map(|after| now + after)).await
}

/// 接着收一个 [`LiveChild`] 的输出直到它收尾（或，给了 `detach_at` 时，到点交出去）。
pub async fn drive(
    live: LiveChild,
    sink: &mut dyn OutputWriter,
    cancel: &CancelToken,
    detach_at: Option<tokio::time::Instant>,
) -> Result<Driven, ProcessError> {
    let LiveChild {
        mut child,
        pgid,
        started_at,
        mut stdout,
        mut stderr,
        mut outcome,
        mut stdout_excerpt,
        mut stderr_excerpt,
        mut exit_status,
        deadline,
        output_limit,
        forget,
    } = live;
    let mut stdout_buf = vec![0u8; 8192];
    let mut stderr_buf = vec![0u8; 8192];
    let mut detached = false;

    loop {
        if cancel.is_cancelled() {
            outcome.cancelled = true;
            break;
        }
        let out_open = stdout.is_some();
        let err_open = stderr.is_some();
        if !out_open && !err_open && exit_status.is_some() {
            break;
        }

        tokio::select! {
            biased;

            () = cancelled(cancel) => {
                outcome.cancelled = true;
                break;
            }
            () = tokio::time::sleep_until(deadline) => {
                outcome.timed_out = true;
                break;
            }
            () = async { tokio::time::sleep_until(detach_at.expect("已判断")).await }, if detach_at.is_some() => {
                detached = true;
                break;
            }
            read = async {
                stdout.as_mut().expect("已判断打开").read(&mut stdout_buf).await
            }, if out_open => {
                match read {
                    Ok(0) | Err(_) => stdout = None,
                    Ok(n) => {
                        outcome.stdout_bytes += n as u64;
                        stdout_excerpt.push(&stdout_buf[..n]);
                        forward(sink, output_limit, &mut outcome, &stdout_buf[..n], true).await?;
                    }
                }
            }
            read = async {
                stderr.as_mut().expect("已判断打开").read(&mut stderr_buf).await
            }, if err_open => {
                match read {
                    Ok(0) | Err(_) => stderr = None,
                    Ok(n) => {
                        outcome.stderr_bytes += n as u64;
                        stderr_excerpt.push(&stderr_buf[..n]);
                        forward(sink, output_limit, &mut outcome, &stderr_buf[..n], false).await?;
                    }
                }
            }
            status = child.wait(), if exit_status.is_none() => {
                exit_status = Some(status.map_err(|e| ProcessError::Io(e.to_string()))?);
                // 管道还可能有残留，循环继续读到 EOF。
            }
        }
    }

    if detached {
        // 执行器那一份在册销掉（`forget` 在这里落地）；接手的一方登记自己的。
        drop(forget);
        return Ok(Driven::Detached(Box::new(LiveChild {
            child,
            pgid,
            started_at,
            stdout,
            stderr,
            outcome,
            stdout_excerpt,
            stderr_excerpt,
            exit_status,
            deadline,
            output_limit,
            forget: None,
        })));
    }

    if outcome.cancelled || outcome.timed_out {
        // **终止进程组并等待回收**：丢掉 handle 不等于进程结束。
        reap(&mut child, pgid).await;
    } else if exit_status.is_none() {
        exit_status = Some(
            child
                .wait()
                .await
                .map_err(|e| ProcessError::Io(e.to_string()))?,
        );
    }

    outcome.exit_code = exit_status.and_then(|status| status.code());
    outcome.stdout_excerpt = stdout_excerpt.into_string();
    outcome.stderr_excerpt = stderr_excerpt.into_string();
    drop(forget);
    Ok(Driven::Done(outcome))
}

async fn reap(child: &mut tokio::process::Child, pgid: u32) {
    kill_process_group(pgid, "TERM").await;
    if tokio::time::timeout(REAP_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        kill_process_group(pgid, "KILL").await;
        let _ = tokio::time::timeout(REAP_TIMEOUT, child.wait()).await;
    }
}

async fn forward(
    sink: &mut dyn OutputWriter,
    output_limit: u64,
    outcome: &mut ChildOutcome,
    chunk: &[u8],
    is_stdout: bool,
) -> Result<(), ProcessError> {
    if sink.bytes_written() >= output_limit {
        outcome.truncated = true;
        return Ok(());
    }
    let room = (output_limit - sink.bytes_written()) as usize;
    let slice = if chunk.len() > room {
        outcome.truncated = true;
        &chunk[..room]
    } else {
        chunk
    };
    if is_stdout {
        sink.write_stdout(slice).await
    } else {
        sink.write_stderr(slice).await
    }
    .map_err(|error| ProcessError::Sink(error.to_string()))
}

/// 收尾时把在册的那一行销掉。放在守卫里是因为 `run_child` 有好几条出口。
#[derive(Debug)]
struct ForgetOnDrop {
    registration: Option<ChildRegistration>,
    pid: u32,
}

impl Drop for ForgetOnDrop {
    fn drop(&mut self) {
        if let Some(registration) = &self.registration
            && let Err(error) = registration
                .registry
                .forget(&registration.executor, self.pid)
        {
            tracing::warn!(pid = self.pid, %error, "子进程销号写不进去");
        }
    }
}

/// 给整个进程组发信号。
///
/// 没有 `libc` 依赖（§13.4 的依赖清单里没有它，而 Cargo.toml 不归运行时改），所以走
/// `kill(1)`：`kill -TERM -- -<pgid>` 的负数 PID 就是"这个进程组"。
// TODO(decide: 想要 libc::killpg 就要在 komo-runtime 的 Cargo.toml 里加 libc——
// 编排者拍板前用 kill(1)，行为一致，代价是每次取消多起一个进程)。
pub async fn kill_process_group(pgid: u32, signal: &str) {
    for program in ["/usr/bin/kill", "/bin/kill", "kill"] {
        let spawned = Command::new(program)
            .arg(format!("-{signal}"))
            .arg("--")
            .arg(format!("-{pgid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut killer) = spawned {
            let _ = killer.wait().await;
            return;
        }
    }
    tracing::warn!(pgid, signal, "找不到 kill(1)，进程组没能收到信号");
}

/// 一个进程组还在不在——取消测试断言的就是它。
pub fn process_group_alive(pgid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "pid=", "-g", &pgid.to_string()])
        .output()
        .map(|out| !out.stdout.iter().all(u8::is_ascii_whitespace))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::test_support::MemOutputWriter;
    use komo_kernel::types::ids::{AttemptId, ExecutorId, RunId, SessionId, ToolCallId};
    use komo_kernel::types::refs::AttemptRef;

    fn writer() -> MemOutputWriter {
        MemOutputWriter::new(AttemptRef {
            session: SessionId::from_raw("s"),
            run: RunId::from_raw("r"),
            call: ToolCallId::from_raw("c"),
            attempt: AttemptId::from_raw("a"),
        })
    }

    fn spec(command: &str) -> ChildSpec {
        ChildSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), command.into()],
            cwd: std::env::temp_dir(),
            env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
            timeout: Duration::from_secs(30),
            output_limit: 1 << 20,
            register: None,
            label: "测试".into(),
        }
    }

    /// 工作目录不存在时，报的必须是**工作目录**，不是程序名。
    ///
    /// spawn 的 ENOENT 挂在程序名上（`/bin/sh: No such file or directory`），而 `/bin/sh`
    /// 明明在。这条已在现场把人引偏过一次（每个 shell 调用都"起不来"，真正
    /// 不在的是会话 cwd）。
    #[tokio::test]
    async fn a_missing_cwd_is_reported_as_such() {
        let mut spec = spec("echo hi");
        spec.cwd = std::env::temp_dir().join("komo-没有这个目录-9f3a");
        let error = run_child(spec, &mut writer(), &CancelToken::new())
            .await
            .expect_err("起不来");
        let ProcessError::Spawn(message) = error else {
            panic!("该报 Spawn：{error}");
        };
        assert!(message.contains("工作目录不存在"), "{message}");
        assert!(message.contains("komo-没有这个目录-9f3a"), "{message}");
        assert!(!message.contains("/bin/sh"), "别把程序名当原因：{message}");
    }

    /// 长输出的预览同时留着开头的汇总与结尾，中间写明省了多少。
    #[tokio::test]
    async fn a_long_output_keeps_both_its_head_and_its_tail() {
        let outcome = run_child(
            spec("echo SUMMARY; head -c 120000 /dev/zero | tr '\\0' x; echo; echo LAST"),
            &mut writer(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        let excerpt = &outcome.stdout_excerpt;
        assert!(excerpt.starts_with("SUMMARY\n"), "{excerpt}");
        assert!(excerpt.ends_with("LAST\n"), "{excerpt}");
        let omitted = outcome.stdout_bytes as usize - HEAD_LIMIT - TAIL_LIMIT;
        assert!(
            excerpt.contains(&format!("…（中间省略 {omitted} 字节）…")),
            "{excerpt}"
        );
    }

    #[test]
    fn a_short_output_is_kept_whole() {
        let mut excerpt = Excerpt::default();
        excerpt.push(b"a\n");
        excerpt.push(b"b\n");
        assert_eq!(excerpt.into_string(), "a\nb\n");
    }

    #[tokio::test]
    async fn stdout_and_stderr_reach_the_sink_separately() {
        let mut sink = writer();
        let outcome = run_child(
            spec("echo out; echo err 1>&2"),
            &mut sink,
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(String::from_utf8_lossy(&sink.stdout).trim(), "out");
        assert_eq!(String::from_utf8_lossy(&sink.stderr).trim(), "err");
    }

    #[tokio::test]
    async fn output_beyond_the_budget_is_dropped_and_marked() {
        let mut sink = writer();
        let mut spec = spec("head -c 100000 /dev/zero | tr '\\0' 'x'");
        spec.output_limit = 256;
        let outcome = run_child(spec, &mut sink, &CancelToken::new())
            .await
            .unwrap();
        assert!(outcome.truncated, "{outcome:?}");
        assert!(
            sink.stdout.len() <= 256,
            "写进去 {} 字节",
            sink.stdout.len()
        );
    }

    #[tokio::test]
    async fn a_timeout_kills_the_whole_process_group() {
        let mut sink = writer();
        let mut spec = spec("sleep 30 & wait");
        spec.timeout = Duration::from_millis(200);
        let outcome = run_child(spec, &mut sink, &CancelToken::new())
            .await
            .unwrap();
        assert!(outcome.timed_out);
    }

    /// 在册登记：跑着的时候有一行，收尾之后没有（§8.7）。
    #[tokio::test]
    async fn a_child_is_registered_while_it_runs_and_forgotten_when_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(ChildRegistry::new(dir.path().join("children")));
        let executor = ExecutorId::from_raw("exec-1");
        let registration = ChildRegistration {
            registry: registry.clone(),
            executor: executor.clone(),
        };

        // 子进程一直跑到我们看过登记文件、放下 `stop` 为止。由看的这一方放下标记而不是
        // 删掉子进程自己建的：后者在负载下会赶在子进程建它之前删，子进程就一直跑到超时。
        let stop = dir.path().join("stop");
        let mut running = spec(&format!(
            "while [ ! -e {} ]; do sleep 0.05; done",
            stop.display()
        ));
        running.register = Some(registration);
        running.label = "测试命令".into();

        let mut sink = writer();
        let watcher = registry.clone();
        let watched = executor.clone();
        let stop_path = stop.clone();
        let peek = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..100 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let children = watcher.children(&watched);
                if !children.is_empty() {
                    seen = children;
                    break;
                }
            }
            std::fs::write(&stop_path, "").expect("放下 stop");
            seen
        });

        let outcome = run_child(running, &mut sink, &CancelToken::new())
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, Some(0));

        let seen = peek.await.unwrap();
        assert_eq!(seen.len(), 1, "跑着的时候在册");
        assert_eq!(seen[0].what, "测试命令");
        assert_eq!(seen[0].pgid, Some(seen[0].pid), "登记的是它自己的进程组");
        assert!(seen[0].started_at.is_some());

        assert!(
            registry.children(&executor).is_empty(),
            "收尾之后销号，不然恢复扫描会以为上一代还留着子进程"
        );
    }

    /// 取消也要销号——`run_child` 有好几条出口，不能只有正常那条记得。
    #[tokio::test]
    async fn a_cancelled_child_is_forgotten_too() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(ChildRegistry::new(dir.path().join("children")));
        let executor = ExecutorId::from_raw("exec-1");
        let mut running = spec("sleep 30");
        running.register = Some(ChildRegistration {
            registry: registry.clone(),
            executor: executor.clone(),
        });
        running.timeout = Duration::from_millis(200);

        let mut sink = writer();
        let outcome = run_child(running, &mut sink, &CancelToken::new())
            .await
            .unwrap();
        assert!(outcome.timed_out);
        assert!(registry.children(&executor).is_empty());
    }
}
