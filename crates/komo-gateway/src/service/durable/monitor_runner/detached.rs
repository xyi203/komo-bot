//! 转到后台的 `bash` 命令（`docs/monitor.md` §8）在 Durable 下：一个不发行事件、只在结束时交回
//! 一条消息的监听。进程是 `bash` 在对话会话里跑过 `detach_after_secs` 后交过来的活子进程
//! （[`Detacher`]）；这里把它**交回 Harness**：
//!
//! - 接手时 `Harness::register_monitor` 登记一条 `komo.monitors`（`id` 取那次 `bash` 调用的工具任务
//!   ID，`timeout_minutes = 0` 标记"转后台的命令"），之后 `monitor_stop #id`、停机与重启收尾都与
//!   监听同一套；
//! - 结束时 `Harness::finish_monitor`：登记 `ended` 与结束消息（`monitor-end:{id}` 的内部 steer
//!   输入）同一次提交。`monitor_stop` 停的不再交回消息（它已经把登记写成 `ended{stopped}`，
//!   模型自己知道）——这与旧实现里"停了也交回一条『已被 monitor_stop 停止』"不同；
//! - 重启：停机时登记留着 `running`，下一代启动时 [`MonitorRunner::boot`] 对每个没结束的转后台命令
//!   交回一条"结果不明"（带输出末尾与完整输出引用），不走 `reconcile_monitors` 那条通用的"已中断"。
//!
//! 转后台之后的输出写进这次尝试目录里的 `background.log`——`stdout.txt` / `stderr.txt` 在那次调用结算
//! 时已按哈希封口，不能再追加。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::durable::ConversationId;
use komo_kernel::traits::{OutputWriter, StoreError};
use komo_kernel::types::ids::{AttemptId, ExecutorId, RunId, SessionId, ToolCallId};
use komo_kernel::types::refs::AttemptRef;
use komo_kernel::types::resource::{OutputFile, ResourceUri};
use komo_kernel::types::tool::CancelToken;
use komo_runtime::durable::harness::orchestration::monitor;
use komo_runtime::durable::harness::{MonitorRecord, MonitorState};
use komo_runtime::recovery::ChildProcess;
use komo_runtime::tools::detach::{Detached, DetachedCall, Detacher};
use komo_runtime::tools::process::{ChildOutcome, Driven, LiveChild, drive};

use super::{
    Ctx, Ending, GatewayMonitorRunner, HEAD_LINES, Live, MAX_EVENT_LINES, STDERR_TAIL_LINES,
    session_of,
};

/// 结束消息里命令那一行最多留多少字符。
const COMMAND_CHARS: usize = 60;

/// 登记里转后台命令的标记：监听总有时限（≥ 1 分钟），转后台的命令没有。
pub(crate) fn is_detached(record: &MonitorRecord) -> bool {
    record.timeout_minutes == 0
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

/// 这次尝试的目录：`tool-output/{conversation}/{tool_task}/{attempt}/`（Durable 的 `RunId` 是
/// conversation，`ToolCallId` 是工具任务）。
fn attempt_dir(sessions_dir: &Path, attempt: &AttemptRef) -> PathBuf {
    komo_store::SessionPaths::new(sessions_dir, &attempt.session)
        .tool_output()
        .join(attempt.run.as_str())
        .join(attempt.call.as_str())
        .join(attempt.attempt.as_str())
}

fn reference_of(attempt: &AttemptRef) -> String {
    ResourceUri::Output {
        run: attempt.run.clone(),
        call: attempt.call.clone(),
        attempt: attempt.attempt.clone(),
        file: OutputFile::Background,
    }
    .to_string()
}

struct Adopted {
    id: u64,
    cancel: CancelToken,
    ending: Arc<Mutex<Option<Ending>>>,
    log: PathBuf,
    reference: String,
    started_at: time::OffsetDateTime,
    title: String,
}

#[async_trait]
impl Detacher for GatewayMonitorRunner {
    async fn adopt(&self, call: DetachedCall) -> Result<Detached, String> {
        let DetachedCall {
            attempt,
            command,
            cwd,
            live,
        } = call;
        match self.adopt_inner(&attempt, &command, cwd, &live).await {
            Ok(adopted) => {
                let handle = Detached {
                    id: adopted.id.to_string(),
                    short_id: adopted.id.to_string(),
                };
                let me = self.me.upgrade().expect("刚用过 self");
                let ctx = self.ctx().map_err(|error| error.to_string())?;
                tokio::spawn(me.watch_detached(ctx, attempt, adopted, live));
                Ok(handle)
            }
            Err(message) => {
                live.terminate().await;
                Err(message)
            }
        }
    }
}

impl GatewayMonitorRunner {
    /// 登记、入表；任何一步失败都由调用方终止进程组。
    async fn adopt_inner(
        &self,
        attempt: &AttemptRef,
        command: &str,
        cwd: PathBuf,
        live: &LiveChild,
    ) -> Result<Adopted, String> {
        if self.is_closing() {
            return Err("Gateway 正在停机".into());
        }
        let ctx = self.ctx().map_err(|error| error.to_string())?;
        let parse = |raw: &str, what: &str| {
            raw.parse::<u64>()
                .map_err(|_| format!("转后台的调用身份不是 Durable 的（{what} = {raw}）"))
        };
        let conversation = ConversationId::new(parse(attempt.run.as_str(), "conversation")?);
        let id = parse(attempt.call.as_str(), "工具任务")?;
        let sessions_dir = ctx.state.snapshot().paths.sessions_dir.clone();
        let title = command_title(command);
        let now = ctx.core.engine.line().now_ms();
        let record = MonitorRecord {
            id,
            conversation,
            title: title.clone(),
            command: command.to_owned(),
            cwd,
            timeout_minutes: 0,
            resume_after_restart: false,
            started_at: now,
            deadline: now,
            state: MonitorState::Running,
        };
        ctx.core
            .harness
            .register_monitor(&ctx.core.engine, record)
            .await
            .map_err(|error| format!("登记写不进去：{error}"))?;
        if let Err(error) = ctx.children.record(
            &ExecutorId::from_raw(id.to_string()),
            ChildProcess {
                pid: live.pgid,
                pgid: Some(live.pgid),
                started_at: Some(live.started_at),
                what: format!("bash 后台 #{id} · {title}"),
            },
        ) {
            tracing::warn!(%error, "转后台的子进程登记写不进去");
        }
        let cancel = CancelToken::new();
        let ending = Arc::new(Mutex::new(None));
        self.insert(
            id,
            Live {
                title: title.clone(),
                conversation,
                timeout_minutes: 0,
                cancel: cancel.clone(),
                ending: Arc::clone(&ending),
            },
        );
        Ok(Adopted {
            id,
            cancel,
            ending,
            log: attempt_dir(&sessions_dir, attempt).join(OutputFile::Background.file_name()),
            reference: reference_of(attempt),
            started_at: live.started_at,
            title,
        })
    }

    /// 收转后台命令的输出，等它收尾，交回结束消息。
    async fn watch_detached(
        self: Arc<Self>,
        ctx: Ctx,
        attempt: AttemptRef,
        adopted: Adopted,
        live: LiveChild,
    ) {
        let Adopted {
            id,
            cancel,
            ending,
            log,
            reference,
            started_at,
            title,
        } = adopted;
        let pid = live.pgid;
        let driven = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .await
        {
            Ok(file) => {
                let mut sink = LogSink {
                    file,
                    written: 0,
                    attempt,
                };
                drive(live, &mut sink, &cancel, None, None)
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
        // `(reason, headline)`；没有 = 不交回（停机、`monitor_stop`）。
        let (finished, tail) = match driven {
            Ok(Driven::Done(outcome)) => {
                let tail = outcome_tail(&outcome);
                let finished = match decided.clone() {
                    Some(Ending::Shutdown | Ending::Stopped) => None,
                    Some(other) => Some((other.reason(), other.headline())),
                    None if outcome.timed_out => {
                        Some(("timeout", "超时，进程组已终止".to_string()))
                    }
                    None if outcome.cancelled => Some(("stopped", "已停止".to_string())),
                    None => Some((
                        "exit",
                        match outcome.exit_code {
                            Some(code) => format!("退出码 {code}"),
                            None => "被信号终止".to_string(),
                        },
                    )),
                };
                (finished, tail)
            }
            Ok(Driven::Detached(_)) => unreachable!("接手后不再给 detach_at"),
            Err(error) => (
                Some(("failed", format!("后台命令出错：{error}"))),
                String::new(),
            ),
        };
        if let Some((reason, headline)) = finished {
            let text = done_text(id, &title, &headline, elapsed, &tail, &reference);
            let detail = format!("已结束（{headline}）；结束消息会单独交回会话（可能已经到了）");
            if let Err(error) = ctx
                .core
                .harness
                .finish_monitor(&ctx.core.engine, id, reason, Some(&detail), text)
                .await
            {
                tracing::warn!(%error, command = id, "后台命令的结束消息交不回会话");
            }
        }
        let _ = ctx
            .children
            .forget(&ExecutorId::from_raw(id.to_string()), pid);
        self.retire(id);
    }

    /// 启动时收掉上一代留下的转后台命令：进程已经没了（[`Self::ensure_reaped`] 核对身份后终止），
    /// 每个没结束的交回一条"结果不明"。登记不是转后台命令的留给 `reconcile_monitors`。
    pub(super) async fn finish_leftover_detached(&self, ctx: &Ctx) {
        let records = match monitor::list(&*ctx.core.storage).await {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "监听登记读不出来，转后台命令的收尾留给对账");
                return;
            }
        };
        let sessions_dir = ctx.state.snapshot().paths.sessions_dir.clone();
        let now = ctx.core.engine.line().now_ms();
        for record in records {
            if !is_detached(&record) || record.state != MonitorState::Running {
                continue;
            }
            if self.is_running(record.id) {
                continue;
            }
            let session = session_of(&*ctx.core.storage, record.conversation).await;
            let (log, reference) = leftover_log(&sessions_dir, &session, &record);
            let tail = read_tail(&log, MAX_EVENT_LINES);
            let elapsed = ((now - record.started_at).max(0) / 1000) as u64;
            let headline = "Gateway 重启，后台命令中断，结果不明";
            let text = done_text(
                record.id,
                &record.title,
                headline,
                elapsed,
                &tail,
                &reference,
            );
            if let Err(error) = ctx
                .core
                .harness
                .finish_monitor(
                    &ctx.core.engine,
                    record.id,
                    "interrupted",
                    Some(headline),
                    text,
                )
                .await
            {
                tracing::warn!(%error, command = record.id, "后台命令中断的消息交不回会话");
            }
        }
    }
}

/// 重启后找那条转后台命令的 `background.log`：尝试号登记里没有，在工具任务的目录下找最新的一份。
fn leftover_log(
    sessions_dir: &Path,
    session: &SessionId,
    record: &MonitorRecord,
) -> (PathBuf, String) {
    let call_dir = komo_store::SessionPaths::new(sessions_dir, session)
        .tool_output()
        .join(record.conversation.to_string())
        .join(record.id.to_string());
    let mut attempts: Vec<u64> = std::fs::read_dir(&call_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .join(OutputFile::Background.file_name())
                .exists()
        })
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    attempts.sort_unstable();
    let attempt = attempts.last().copied().unwrap_or(1);
    let log = call_dir
        .join(attempt.to_string())
        .join(OutputFile::Background.file_name());
    let reference = ResourceUri::Output {
        run: RunId::from_raw(record.conversation.to_string()),
        call: ToolCallId::from_raw(record.id.to_string()),
        attempt: AttemptId::from_raw(attempt.to_string()),
        file: OutputFile::Background,
    }
    .to_string();
    (log, reference)
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
    id: u64,
    title: &str,
    headline: &str,
    elapsed_secs: u64,
    tail: &str,
    reference: &str,
) -> String {
    let mut text = format!(
        "[后台命令 #{id}「{title}」结束] {headline}，用时 {}",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_done_text_names_the_handle_the_exit_the_tail_and_the_full_output() {
        let text = done_text(
            77,
            "make",
            "退出码 4",
            65,
            "after-line",
            "artifact://x/background",
        );
        assert!(
            text.starts_with("[后台命令 #77「make」结束] 退出码 4，用时 1 分 5 秒"),
            "{text}"
        );
        assert!(text.contains("输出末尾：\nafter-line"), "{text}");
        assert!(text.contains("artifact://x/background"), "{text}");
    }

    #[test]
    fn a_long_command_line_is_cut_to_a_title() {
        let title = command_title(&format!("{}\nsecond", "x".repeat(100)));
        assert_eq!(title.chars().count(), COMMAND_CHARS + 1);
        assert!(title.ends_with('…'));
        assert_eq!(command_title("echo hi\nsecond"), "echo hi");
    }

    #[test]
    fn only_zero_timeout_records_count_as_detached() {
        let mut record = MonitorRecord {
            id: 1,
            conversation: ConversationId::new(2),
            title: "t".into(),
            command: "c".into(),
            cwd: ".".into(),
            timeout_minutes: 0,
            resume_after_restart: false,
            started_at: 0,
            deadline: 0,
            state: MonitorState::Running,
        };
        assert!(is_detached(&record));
        record.timeout_minutes = 5;
        assert!(!is_detached(&record));
    }
}
