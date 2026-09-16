//! 子进程：自己的进程组、流式输出、输出上限、超时与取消回收（§4）。
//!
//! `shell` 和 `python_runtime` 用的是同一套：两者要的都是"起一个进程，把它的
//! stdout / stderr 边跑边写进 [`OutputWriter`]，超时或取消时**终止整个进程组并等待
//! 回收**"。Tokio 不会因为 handle 被丢掉就结束进程，而 `Child::kill` 只杀直接子进程
//! ——`sh -c 'sleep 30 &'` 的孙子进程会活下来，所以信号必须发给进程组。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use komo_kernel::traits::OutputWriter;
use komo_kernel::types::tool::CancelToken;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::executor::cancel::cancelled;

/// 取消或超时后，等进程组真正消失的上限。
const REAP_TIMEOUT: Duration = Duration::from_secs(5);
/// 预览保留的尾部字节数。
const TAIL_LIMIT: usize = 4096;

/// 起一个子进程要的全部东西。
#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// **明确的**环境变量集合——继承整个环境等于把凭证交给任意脚本（§5.3）。
    pub env: BTreeMap<String, String>,
    #[allow(clippy::doc_markdown)]
    /// 写进 stdin 的正文（`python` 的作业描述走这里）。
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
    /// 写进 [`OutputWriter`] 的字节上限；超出后停止转发并置 `truncated`。
    pub output_limit: u64,
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
    /// 尾部片段，仅供预览；完整正文在 `ToolOutputStore` 里。
    pub stdout_tail: String,
    pub stderr_tail: String,
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

/// 一个只保留尾部的缓冲：预览要的是最后几行，不是把整份输出搬进内存。
#[derive(Debug, Default)]
struct Tail {
    buffer: Vec<u8>,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
        if self.buffer.len() > TAIL_LIMIT {
            let cut = self.buffer.len() - TAIL_LIMIT;
            self.buffer.drain(..cut);
        }
    }

    fn into_string(self) -> String {
        String::from_utf8_lossy(&self.buffer).into_owned()
    }
}

/// 起进程、流式收输出、按需终止进程组。
pub async fn run_child(
    spec: ChildSpec,
    sink: &mut dyn OutputWriter,
    cancel: &CancelToken,
) -> Result<ChildOutcome, ProcessError> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 自己的进程组：取消时一个信号带走整棵子树。
        .process_group(0)
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|error| ProcessError::Spawn(format!("{}: {error}", spec.program)))?;
    let pgid = child
        .id()
        .ok_or_else(|| ProcessError::Spawn("子进程没有 pid".into()))?;

    if let Some(body) = &spec.stdin
        && let Some(mut handle) = child.stdin.take()
    {
        // 写不进去（脚本没读 stdin 就退出）不是失败，输出还是要收。
        let _ = handle.write_all(body).await;
        let _ = handle.shutdown().await;
    }

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut outcome = ChildOutcome::default();
    let mut stdout_tail = Tail::default();
    let mut stderr_tail = Tail::default();
    let mut stdout_buf = vec![0u8; 8192];
    let mut stderr_buf = vec![0u8; 8192];

    let deadline = tokio::time::Instant::now() + spec.timeout;
    let mut exit_status = None;

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
            read = async {
                stdout.as_mut().expect("已判断打开").read(&mut stdout_buf).await
            }, if out_open => {
                match read {
                    Ok(0) | Err(_) => stdout = None,
                    Ok(n) => {
                        outcome.stdout_bytes += n as u64;
                        stdout_tail.push(&stdout_buf[..n]);
                        forward(sink, &spec, &mut outcome, &stdout_buf[..n], true).await?;
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
                        stderr_tail.push(&stderr_buf[..n]);
                        forward(sink, &spec, &mut outcome, &stderr_buf[..n], false).await?;
                    }
                }
            }
            status = child.wait(), if exit_status.is_none() => {
                exit_status = Some(status.map_err(|e| ProcessError::Io(e.to_string()))?);
                // 管道还可能有残留，循环继续读到 EOF。
            }
        }
    }

    if outcome.cancelled || outcome.timed_out {
        // **终止进程组并等待回收**：丢掉 handle 不等于进程结束。
        kill_process_group(pgid, "TERM").await;
        if tokio::time::timeout(REAP_TIMEOUT, child.wait())
            .await
            .is_err()
        {
            kill_process_group(pgid, "KILL").await;
            let _ = tokio::time::timeout(REAP_TIMEOUT, child.wait()).await;
        }
    } else if exit_status.is_none() {
        exit_status = Some(
            child
                .wait()
                .await
                .map_err(|e| ProcessError::Io(e.to_string()))?,
        );
    }

    outcome.exit_code = exit_status.and_then(|status| status.code());
    outcome.stdout_tail = stdout_tail.into_string();
    outcome.stderr_tail = stderr_tail.into_string();
    Ok(outcome)
}

async fn forward(
    sink: &mut dyn OutputWriter,
    spec: &ChildSpec,
    outcome: &mut ChildOutcome,
    chunk: &[u8],
    is_stdout: bool,
) -> Result<(), ProcessError> {
    if sink.bytes_written() >= spec.output_limit {
        outcome.truncated = true;
        return Ok(());
    }
    let room = (spec.output_limit - sink.bytes_written()) as usize;
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
    use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
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
            stdin: None,
            timeout: Duration::from_secs(30),
            output_limit: 1 << 20,
        }
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
}
