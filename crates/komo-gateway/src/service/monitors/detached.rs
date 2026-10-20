//! 转到后台的 `bash` 命令（`docs/monitor.md` §8）：一个不发行事件、只在结束时交回一条消息的
//! 监听。进程是 `bash` 在对话会话里跑过 `detach_after_secs` 后交过来的活子进程
//! （[`komo_runtime::tools::detach::Detacher`]），登记、短号、`monitor_stop`、重启收尾都与
//! 监听同一套（`GatewayMonitors` 的 `running` 表、`runtime/monitors/`）。
//!
//! 转后台之后的输出写进这次尝试目录里的 `background.log`——`stdout.txt` / `stderr.txt` 在那次
//! 调用结算时已按哈希封口，不能再追加。

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, StoreError};
use komo_kernel::types::chat::ChannelPeer;
use komo_kernel::types::ids::{ExecutorId, RequestKey, ToolCallId};
use komo_kernel::types::monitor;
use komo_kernel::types::refs::AttemptRef;
use komo_kernel::types::resource::{OutputFile, ResourceUri};
use komo_kernel::types::tool::CancelToken;
use komo_runtime::recovery::ChildProcess;
use komo_runtime::tools::detach::{Detached, DetachedCall, Detacher};
use komo_runtime::tools::process::{ChildOutcome, Driven, LiveChild, drive};
use serde::{Deserialize, Serialize};

use super::{
    Ending, GatewayMonitors, HEAD_LINES, Live, MAX_EVENT_LINES, Record, STDERR_TAIL_LINES,
};

/// 结束消息里命令那一行最多留多少字符。
const COMMAND_CHARS: usize = 60;

/// 登记里转后台命令独有的几格。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct DetachedInfo {
    /// 那次 `bash` 调用：结束消息的请求键 `bash-done:{call}`。
    pub call: ToolCallId,
    /// 转后台之后的输出文件。
    pub log: std::path::PathBuf,
    /// 它的 `artifact://` 入口。
    pub reference: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: time::OffsetDateTime,
}

pub(super) fn end_key(call: &ToolCallId) -> RequestKey {
    RequestKey::new(format!("bash-done:{call}"))
}

/// 转后台之后的输出：stdout 与 stderr 按到达顺序混写进一个文件。
struct LogSink {
    file: tokio::fs::File,
    written: u64,
    attempt: AttemptRef,
}

impl LogSink {
    async fn write(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        use tokio::io::AsyncWriteExt;
        self.file
            .write_all(chunk)
            .await
            .map_err(|error| StoreError::Io(format!("写 background.log 失败：{error}")))?;
        self.written += chunk.len() as u64;
        Ok(())
    }
}

#[async_trait]
impl OutputWriter for LogSink {
    async fn write_stdout(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.write(chunk).await
    }

    async fn write_stderr(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.write(chunk).await
    }

    fn attempt(&self) -> &AttemptRef {
        &self.attempt
    }

    fn bytes_written(&self) -> u64 {
        self.written
    }
}

#[async_trait]
impl Detacher for GatewayMonitors {
    async fn adopt(&self, call: DetachedCall) -> Result<Detached, String> {
        let DetachedCall {
            attempt,
            command,
            cwd: _,
            live,
        } = call;
        let result = self.adopt_inner(&attempt, &command, &live).await;
        match result {
            Ok((id, cancel, ending, info, peer)) => {
                let handle = Detached {
                    short_id: monitor::short_id(&id),
                    id: id.clone(),
                };
                let me = self.me.upgrade().expect("刚用过 self");
                tokio::spawn(me.watch_detached(id, attempt, info, peer, live, cancel, ending));
                Ok(handle)
            }
            Err(message) => {
                live.terminate().await;
                Err(message)
            }
        }
    }
}

type Adopted = (
    String,
    CancelToken,
    Arc<Mutex<Option<Ending>>>,
    DetachedInfo,
    Option<ChannelPeer>,
);

impl GatewayMonitors {
    /// 登记、入表；任何一步失败都由调用方终止进程组。
    async fn adopt_inner(
        &self,
        attempt: &AttemptRef,
        command: &str,
        live: &LiveChild,
    ) -> Result<Adopted, String> {
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Gateway 正在停机".into());
        }
        let (session, peer_raw) = self
            .origin_of(&attempt.run)
            .await
            .map_err(|error| error.to_string())?;
        let id = komo_kernel::types::ids::uuid_v7_at(self.state.clock.now()).to_string();
        let dir =
            komo_store::SessionPaths::new(&self.state.snapshot().paths.sessions_dir, &session)
                .tool_output()
                .join(attempt.run.as_str())
                .join(attempt.call.as_str())
                .join(attempt.attempt.as_str());
        let info = DetachedInfo {
            call: attempt.call.clone(),
            log: dir.join(OutputFile::Background.file_name()),
            reference: ResourceUri::Output {
                run: attempt.run.clone(),
                call: attempt.call.clone(),
                attempt: attempt.attempt.clone(),
                file: OutputFile::Background,
            }
            .to_string(),
            started_at: live.started_at,
        };
        let title = command_title(command);
        let record = Record {
            id: id.clone(),
            session: session.clone(),
            title: title.clone(),
            command: command.to_string(),
            peer: peer_raw,
            resume: None,
            detached: Some(info.clone()),
        };
        self.write_record(&record)
            .map_err(|error| format!("登记写不进去：{error}"))?;
        if let Err(error) = self.children.record(
            &ExecutorId::from_raw(id.clone()),
            ChildProcess {
                pid: live.pgid,
                pgid: Some(live.pgid),
                started_at: Some(live.started_at),
                what: format!("bash 后台 #{} · {title}", monitor::short_id(&id)),
            },
        ) {
            tracing::warn!(%error, "转后台的子进程登记写不进去");
        }
        let cancel = CancelToken::new();
        let ending = Arc::new(Mutex::new(None));
        self.running.lock().expect("监听表").insert(
            id.clone(),
            Live {
                key: end_key(&attempt.call),
                session,
                title,
                timeout_minutes: 0,
                cancel: cancel.clone(),
                ending: Arc::clone(&ending),
            },
        );
        let peer = record.peer.as_deref().and_then(ChannelPeer::parse);
        Ok((id, cancel, ending, info, peer))
    }

    /// 收转后台命令的输出，等它收尾，交回结束消息。
    #[allow(clippy::too_many_arguments)]
    async fn watch_detached(
        self: Arc<Self>,
        id: String,
        attempt: AttemptRef,
        info: DetachedInfo,
        peer: Option<ChannelPeer>,
        live: LiveChild,
        cancel: CancelToken,
        ending: Arc<Mutex<Option<Ending>>>,
    ) {
        let (title, session) = {
            let running = self.running.lock().expect("监听表");
            let entry = running.get(&id).expect("入表之后才起泵");
            (entry.title.clone(), entry.session.clone())
        };
        let pid = live.pgid;
        let started_at = live.started_at;
        let driven = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&info.log)
            .await
        {
            Ok(file) => {
                let mut sink = LogSink {
                    file,
                    written: 0,
                    attempt,
                };
                drive(live, &mut sink, &cancel, None)
                    .await
                    .map_err(|error| error.to_string())
            }
            Err(error) => {
                live.terminate().await;
                Err(format!("background.log 打不开：{error}"))
            }
        };
        let decided = ending.lock().expect("结束原因").clone();
        let elapsed = (time::OffsetDateTime::now_utc() - started_at)
            .whole_seconds()
            .max(0) as u64;
        let (headline, tail) = match driven {
            Ok(Driven::Done(outcome)) => {
                let tail = outcome_tail(&outcome);
                (
                    match decided.clone() {
                        Some(Ending::Shutdown) => None,
                        Some(Ending::Stopped) => Some("已被 monitor_stop 停止".to_string()),
                        Some(other) => Some(other.headline()),
                        None if outcome.timed_out => Some("超时，进程组已终止".to_string()),
                        None if outcome.cancelled => Some("已停止".to_string()),
                        None => Some(match outcome.exit_code {
                            Some(code) => format!("退出码 {code}"),
                            None => "被信号终止".to_string(),
                        }),
                    },
                    tail,
                )
            }
            Ok(Driven::Detached(_)) => unreachable!("接手后不再给 detach_at"),
            Err(error) => (Some(format!("后台命令出错：{error}")), String::new()),
        };
        if let Some(headline) = headline {
            let text = done_text(&id, &title, &headline, elapsed, &tail, &info.reference);
            let key = end_key(&info.call);
            // 宿主可能拿到转后台回执后仍在跑；完成状态要在下一次工具边界让它看见，
            // 不能排到整条 Run 后面。沿用插话的模型快照/对端资格检查。
            if let Err(error) = self
                .state
                .submit_interjecting(&session, key, text, peer, None)
                .await
            {
                tracing::warn!(%error, command = %id, "后台命令的结束消息交不回会话");
            }
        }
        // 停机时登记留着：下一代启动时告诉会话"结果不明"。
        if decided != Some(Ending::Shutdown) {
            self.forget_record(&id);
        }
        let _ = self.children.forget(&ExecutorId::from_raw(id.clone()), pid);
        self.running.lock().expect("监听表").remove(&id);
    }

    /// 上一代留下的转后台命令：进程已经没了（或被核对身份后终止），结果不明。
    pub(super) fn interrupted_text(record: &Record, info: &DetachedInfo) -> String {
        let tail = read_tail(&info.log, MAX_EVENT_LINES);
        let elapsed = (time::OffsetDateTime::now_utc() - info.started_at)
            .whole_seconds()
            .max(0) as u64;
        done_text(
            &record.id,
            &record.title,
            "Gateway 重启，后台命令中断，结果不明",
            elapsed,
            &tail,
            &info.reference,
        )
    }
}

/// 命令第一行，截断。
fn command_title(command: &str) -> String {
    let first = command.lines().next().unwrap_or("").trim();
    if first.chars().count() > COMMAND_CHARS {
        let cut: String = first.chars().take(COMMAND_CHARS).collect();
        format!("{cut}…")
    } else {
        first.to_string()
    }
}

/// 结束消息：`[后台命令 #id「命令」结束] 退出码 N，用时 …`、输出尾部、完整输出引用。
fn done_text(
    id: &str,
    title: &str,
    headline: &str,
    elapsed_secs: u64,
    tail: &str,
    reference: &str,
) -> String {
    let short = monitor::short_id(id);
    let mut text = format!(
        "[后台命令 #{short}「{title}」结束] {headline}，用时 {}",
        duration(elapsed_secs)
    );
    if !tail.trim().is_empty() {
        text.push_str("\n\n输出末尾：\n");
        text.push_str(tail.trim_end());
    }
    text.push_str(&format!(
        "\n\n转到后台之后的完整输出：{reference}（转后台之前的输出在那次 bash 调用的结果里）\n\n\
         （这是系统消息，不是用户发来的。需要用户知道的由你转告。）"
    ));
    text
}

fn duration(secs: u64) -> String {
    match (secs / 3600, secs % 3600 / 60, secs % 60) {
        (0, 0, s) => format!("{s} 秒"),
        (0, m, s) => format!("{m} 分 {s} 秒"),
        (h, m, s) => format!("{h} 小时 {m} 分 {s} 秒"),
    }
}

/// stdout 行：不超过 [`MAX_EVENT_LINES`] 全留，否则开头 [`HEAD_LINES`] + 最新的，中间写明省略；
/// stderr 另带最后 [`STDERR_TAIL_LINES`] 行。
fn outcome_tail(outcome: &ChildOutcome) -> String {
    let lines: Vec<&str> = outcome.stdout_excerpt.lines().collect();
    let mut text = if lines.len() <= MAX_EVENT_LINES {
        lines.join("\n")
    } else {
        let tail_from = lines.len() - (MAX_EVENT_LINES - HEAD_LINES);
        format!(
            "{}\n…（中间省略 {} 行）…\n{}",
            lines[..HEAD_LINES].join("\n"),
            tail_from - HEAD_LINES,
            lines[tail_from..].join("\n")
        )
    };
    let err: Vec<&str> = outcome.stderr_excerpt.lines().collect();
    if !err.is_empty() {
        text.push_str("\n[stderr 末尾]\n");
        text.push_str(&err[err.len().saturating_sub(STDERR_TAIL_LINES)..].join("\n"));
    }
    text
}

/// 文件最后一段的最后 `n` 行；没有文件就是空。
fn read_tail(path: &Path, n: usize) -> String {
    const WINDOW: u64 = 64 * 1024;
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(WINDOW)));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
