//! 监听（`docs/monitor.md`）：`Monitors` 的生产实现。
//!
//! 一个监听是 Gateway 手里的一个子进程加一个泵：脚本按 `shell` 的规矩起（bash、数据目录
//! 只读挂载、明确的环境），stdout 逐行进泵，泵把连着出的行合成一条，以内部输入
//! `submit` 回挂它的会话——与后台任务交回结果同一条路（`tasks::report_to_parent`）。
//!
//! 几条不能松的规矩：
//!
//! - **同一个监听同时只有一条事件在飞。** 每条事件都是一条 Run、一轮模型；上一条没收尾时
//!   新出的行攒着，合进下一条。脚本再吵，排进会话的也只有一条。
//! - **不跨重启。** 进程只活在这一代 Gateway 里。停机时终止进程组；`runtime/monitors/`
//!   里留着的登记由下一代启动时收掉：遗留的进程组核对身份后终止，并告诉会话"中断了"。
//!   续跑的 `monitor` 调用认不到同一个键时答"中断了"，绝不自己重新起进程（§8.6）。
//! - **在册是自己的。** 子进程不登记进执行器的 `runtime/children/`：那里一行活着的进程会让
//!   恢复扫描把上一代的所有 Run 停下来（§8.7），而监听与任何一条 Run 都无关。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::traits::{MonitorError, Monitors, OutputWriter, StoreError};
use komo_kernel::types::chat::ChannelPeer;
use komo_kernel::types::ids::{AttemptId, ExecutorId, RequestKey, RunId, SessionId, ToolCallId};
use komo_kernel::types::monitor::{self, MonitorHandle, MonitorSpec, MonitorStopped};
use komo_kernel::types::refs::AttemptRef;
use komo_kernel::types::status::RunState;
use komo_kernel::types::tool::CancelToken;
use komo_runtime::recovery::{ChildRegistry, Liveness, ProcessProbe, SysProcessProbe};
use komo_runtime::tools::ShellTool;
use komo_runtime::tools::process::{
    ChildOutcome, ChildRegistration, ChildSpec, ProcessError, kill_process_group, run_child,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::state::GatewayState;

/// 第一行出来之后再等这么久，把连着出的行合进同一条事件。
const BATCH_QUIET: Duration = Duration::from_secs(1);
/// 泵看一眼"能不能交下一条"的间隔。
const TICK: Duration = Duration::from_millis(250);
/// 一条事件最多带几行：开头留 [`HEAD_LINES`] 行，其余留最新的，中间写明省了几行。
const MAX_EVENT_LINES: usize = 40;
const HEAD_LINES: usize = 10;
/// 一行最多留多少个字符。
const MAX_LINE_CHARS: usize = 1000;
/// 一条事件攒到这么多行还没交出去：脚本没有过滤，停掉它。
const FLOOD_LINES: usize = 2000;
/// 一个监听最多交回几条事件（不含结束那条）。每条都是一轮模型。
const MAX_EVENTS: u64 = 200;
/// 在飞的那条事件等这么久还没收尾，就不再等它（下一条照样排在它后面）。
const IN_FLIGHT_PATIENCE: Duration = Duration::from_secs(600);
/// 结束消息里带 stderr 的最后几行。
const STDERR_TAIL_LINES: usize = 20;
/// 停机时等泵收尾的上限（`run_child` 自己 TERM → KILL 各等 5 秒）。
const SHUTDOWN_WAIT: Duration = Duration::from_secs(12);

/// `runtime/monitors/<id>.json`：一个正在跑的监听。下一代启动时按它告诉会话"中断了"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    id: String,
    session: SessionId,
    title: String,
    command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peer: Option<String>,
}

/// 监听为什么结束了。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ending {
    /// 脚本自己退出。
    Exited(Option<i32>),
    /// 到了时限，进程组已终止。
    TimedOut(u64),
    /// 一条事件攒了太多行。
    Flood,
    /// 交回的事件到了上限。
    TooManyEvents,
    /// 起不来、读不了输出、交不回会话。
    Failed(String),
    /// 模型调了 `monitor_stop`：它自己知道，不再交回结束消息。
    Stopped,
    /// Gateway 停机：登记留给下一代启动时收。
    Shutdown,
}

impl Ending {
    fn headline(&self) -> String {
        match self {
            Ending::Exited(Some(code)) => format!("已结束：脚本退出（exit={code}）"),
            Ending::Exited(None) => "已结束：脚本被信号终止".into(),
            Ending::TimedOut(minutes) => format!("已结束：到了 {minutes} 分钟时限，进程组已终止"),
            Ending::Flood => format!(
                "已停止：一次攒了超过 {FLOOD_LINES} 行，脚本没有过滤输出（每一行都是一个事件）"
            ),
            Ending::TooManyEvents => format!("已停止：已经交回 {MAX_EVENTS} 条事件，到了上限"),
            Ending::Failed(reason) => format!("已停止：{reason}"),
            Ending::Stopped => "已停止".into(),
            Ending::Shutdown => "已中断：Gateway 停机".into(),
        }
    }
}

/// 正在跑的一个监听。
struct Live {
    key: RequestKey,
    session: SessionId,
    title: String,
    timeout_minutes: u64,
    cancel: CancelToken,
    /// 由外面决定的结束原因（停、停机、泵自己判的刷屏）。先写原因再取消。
    ending: Arc<Mutex<Option<Ending>>>,
}

impl Live {
    fn handle(&self, id: &str) -> MonitorHandle {
        MonitorHandle {
            id: id.to_string(),
            short_id: monitor::short_id(id),
            title: self.title.clone(),
            session: self.session.clone(),
            timeout_minutes: self.timeout_minutes,
        }
    }
}

fn end_with(ending: &Mutex<Option<Ending>>, cancel: &CancelToken, why: Ending) {
    let mut slot = ending.lock().expect("监听的结束原因");
    if slot.is_none() {
        *slot = Some(why);
    }
    drop(slot);
    cancel.cancel();
}

pub struct GatewayMonitors {
    state: Arc<GatewayState>,
    me: Weak<GatewayMonitors>,
    shell: ShellTool,
    /// `runtime/monitors`。
    dir: PathBuf,
    /// `runtime/monitors/children`：每个监听一份在册（executor 位上写监听 id）。
    children: ChildRegistry,
    running: Mutex<HashMap<String, Live>>,
    closing: std::sync::atomic::AtomicBool,
}

impl GatewayMonitors {
    pub fn new(state: Arc<GatewayState>, shell: ShellTool, runtime_dir: &Path) -> Arc<Self> {
        let dir = runtime_dir.join("monitors");
        Arc::new_cyclic(|me| Self {
            state,
            me: me.clone(),
            shell,
            children: ChildRegistry::new(dir.join("children")),
            dir,
            running: Mutex::new(HashMap::new()),
            closing: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn write_record(&self, record: &Record) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let text = serde_json::to_string_pretty(record)?;
        std::fs::write(self.record_path(&record.id), text)
    }

    fn forget_record(&self, id: &str) {
        let path = self.record_path(id);
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), %error, "监听登记删不掉");
        }
    }

    /// 这条 Run 属于哪个会话、该投给哪个渠道对端。
    async fn origin_of(&self, from: &RunId) -> Result<(SessionId, Option<String>), MonitorError> {
        let run = komo_store::repos::runs::get(&self.state.db, from)
            .await
            .map_err(|error| MonitorError::Failed(format!("读不到挂它的那条 Run：{error}")))?
            .ok_or_else(|| MonitorError::Failed(format!("挂它的那条 Run {from} 不在账本里")))?;
        Ok((run.session, run.peer))
    }

    /// 停机：终止所有监听的进程组，等泵收尾。登记留着，下一代启动时告诉会话"中断了"。
    pub async fn shutdown(&self) {
        self.closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let running = self.running.lock().expect("监听表");
            for live in running.values() {
                end_with(&live.ending, &live.cancel, Ending::Shutdown);
            }
        }
        let deadline = Instant::now() + SHUTDOWN_WAIT;
        while Instant::now() < deadline && !self.running.lock().expect("监听表").is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 启动时收掉上一代留下的监听：遗留的进程组核对身份后终止，登记过的会话各收到一条
    /// "中断了"。在 `service::start` 里、调度器起来之前调。
    pub async fn recover_interrupted(&self) {
        self.reap_leftover_children().await;
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let record = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<Record>(&text).ok());
            let Some(record) = record else {
                tracing::warn!(path = %path.display(), "监听登记读不出来，删掉");
                let _ = std::fs::remove_file(&path);
                continue;
            };
            let text = end_text(
                &record.id,
                &record.title,
                "已中断：Gateway 重启了，脚本没有在跑，也不会自动重挂",
                &[],
                0,
                "",
            );
            let peer = record.peer.as_deref().and_then(ChannelPeer::parse);
            let key = RequestKey::new(format!("monitor-end:{}", record.id));
            if let Err(error) = self
                .state
                .submit(&record.session, key, text, peer, None)
                .await
            {
                tracing::warn!(%error, monitor = %record.id, "监听中断的消息交不回会话");
            }
            self.forget_record(&record.id);
        }
    }

    /// 上一代的监听进程组：还活着、而且起来的时刻对得上登记，才终止它。pid 会被复用，
    /// 对不上时宁可留着一个孤儿，也不误杀别人的进程组。
    async fn reap_leftover_children(&self) {
        let Ok(entries) = std::fs::read_dir(self.children.dir()) else {
            return;
        };
        let now = time::OffsetDateTime::now_utc();
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let executor = ExecutorId::from_raw(id.to_string());
            for child in self.children.children(&executor) {
                if SysProcessProbe.probe(&child) != Liveness::Alive {
                    continue;
                }
                let same = child.started_at.is_some_and(|started| {
                    elapsed_secs(child.pid).is_some_and(|elapsed| {
                        let then = now - time::Duration::seconds(elapsed as i64);
                        (then - started).whole_seconds().abs() <= 10
                    })
                });
                if !same {
                    tracing::warn!(pid = child.pid, what = %child.what, "上一代的监听进程核对不了身份，没有终止它");
                    continue;
                }
                let pgid = child.pgid.unwrap_or(child.pid);
                tracing::info!(pgid, what = %child.what, "终止上一代留下的监听进程组");
                kill_process_group(pgid, "TERM").await;
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    /// 泵：收行、合批、交回，脚本收尾后交回结束消息并销号。
    async fn pump(
        self: Arc<Self>,
        id: String,
        session: SessionId,
        peer: Option<ChannelPeer>,
        spec: ChildSpec,
        attempt: AttemptRef,
    ) {
        let (title, cancel, ending, timeout_minutes) = {
            let running = self.running.lock().expect("监听表");
            let live = running.get(&id).expect("泵起来之前已经登记");
            (
                live.title.clone(),
                live.cancel.clone(),
                Arc::clone(&live.ending),
                live.timeout_minutes,
            )
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let child_cancel = cancel.clone();
        let child = tokio::spawn(async move {
            let mut sink = LineSink::new(tx, attempt);
            let outcome = run_child(spec, &mut sink, &child_cancel).await;
            sink.flush();
            outcome
        });

        let mut batch = Batch::default();
        let mut events = 0u64;
        let mut in_flight: Option<(RunId, Instant)> = None;
        let mut ticker = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                line = rx.recv() => {
                    let Some(line) = line else { break };
                    batch.push(line, Instant::now());
                    if batch.total >= FLOOD_LINES {
                        end_with(&ending, &cancel, Ending::Flood);
                    }
                }
                _ = ticker.tick() => {
                    if let Some((run, since)) = &in_flight
                        && (self.settled(run).await || since.elapsed() >= IN_FLIGHT_PATIENCE)
                    {
                        in_flight = None;
                    }
                    if cancel.is_cancelled() || in_flight.is_some() || !batch.ready(Instant::now()) {
                        continue;
                    }
                    if events >= MAX_EVENTS {
                        end_with(&ending, &cancel, Ending::TooManyEvents);
                        continue;
                    }
                    events += 1;
                    let text = event_text(&id, &title, &batch.lines(), batch.omitted());
                    batch = Batch::default();
                    let key = RequestKey::new(format!("monitor:{id}:{events}"));
                    match self.state.submit(&session, key, text, peer.clone(), None).await {
                        Ok(response) => in_flight = Some((response.run, Instant::now())),
                        Err(error) => end_with(
                            &ending,
                            &cancel,
                            Ending::Failed(format!("事件交不回会话：{error}")),
                        ),
                    }
                }
            }
        }

        let outcome = child.await;
        let decided = ending.lock().expect("监听的结束原因").clone();
        let (end, stderr) = settle(decided, outcome, timeout_minutes);
        match end {
            Ending::Stopped | Ending::Shutdown => {}
            ref end => {
                let text = end_text(
                    &id,
                    &title,
                    &end.headline(),
                    &batch.lines(),
                    batch.omitted(),
                    &stderr,
                );
                let key = RequestKey::new(format!("monitor-end:{id}"));
                if let Err(error) = self.state.submit(&session, key, text, peer, None).await {
                    tracing::warn!(%error, monitor = %id, "监听的结束消息交不回会话");
                }
            }
        }
        // 停机时登记留着：下一代启动时由它告诉会话"中断了"。
        if end != Ending::Shutdown {
            self.forget_record(&id);
        }
        self.running.lock().expect("监听表").remove(&id);
    }

    /// 在飞的那条事件收尾了吗：终态，或者停在要人处理的介入上（后面的事件反正排在它后面）。
    async fn settled(&self, run: &RunId) -> bool {
        match komo_store::repos::runs::get(&self.state.db, run).await {
            Ok(Some(record)) => {
                record.state.is_terminal()
                    || (record.state == RunState::Waiting
                        && record
                            .wait
                            .as_ref()
                            .is_some_and(|wait| wait.needs_a_person()))
            }
            Ok(None) => true,
            // 读不出来就当它还在跑：等满 IN_FLIGHT_PATIENCE 或下一拍再看。
            Err(_) => false,
        }
    }
}

/// 结束原因：外面定过的优先，否则看进程怎么收尾的。第二个值是 stderr 的最后几行。
fn settle(
    decided: Option<Ending>,
    outcome: Result<Result<ChildOutcome, ProcessError>, tokio::task::JoinError>,
    timeout_minutes: u64,
) -> (Ending, String) {
    let (from_process, stderr) = match outcome {
        Err(error) => (
            Ending::Failed(format!("监听任务崩了：{error}")),
            String::new(),
        ),
        Ok(Err(error)) => (Ending::Failed(error.to_string()), String::new()),
        Ok(Ok(outcome)) => {
            let stderr = last_lines(&outcome.stderr_excerpt, STDERR_TAIL_LINES);
            let ending = if outcome.timed_out {
                Ending::TimedOut(timeout_minutes)
            } else if outcome.cancelled {
                Ending::Stopped
            } else {
                Ending::Exited(outcome.exit_code)
            };
            (ending, stderr)
        }
    };
    (decided.unwrap_or(from_process), stderr)
}

#[async_trait]
impl Monitors for GatewayMonitors {
    async fn start(
        &self,
        from: &RunId,
        request_key: RequestKey,
        spec: MonitorSpec,
        fresh: bool,
    ) -> Result<MonitorHandle, MonitorError> {
        {
            let running = self.running.lock().expect("监听表");
            if let Some((id, live)) = running.iter().find(|(_, live)| live.key == request_key) {
                return Ok(live.handle(id));
            }
        }
        if !fresh {
            return Err(MonitorError::Interrupted(
                "上一次挂的监听随 Gateway 重启中断了，没有自动重挂；要接着盯就重新调用 monitor"
                    .into(),
            ));
        }
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(MonitorError::Failed("Gateway 正在停机".into()));
        }
        if !spec.cwd.is_dir() {
            return Err(MonitorError::Failed(format!(
                "工作目录不存在：{}",
                spec.cwd.display()
            )));
        }
        let me = self
            .me
            .upgrade()
            .ok_or_else(|| MonitorError::Failed("Gateway 正在停机".into()))?;
        let (session, peer_raw) = self.origin_of(from).await?;
        let id = komo_kernel::types::ids::uuid_v7_at(self.state.clock.now()).to_string();
        self.write_record(&Record {
            id: id.clone(),
            session: session.clone(),
            title: spec.title.clone(),
            command: spec.command.clone(),
            peer: peer_raw.clone(),
        })
        .map_err(|error| MonitorError::Failed(format!("监听登记写不进去：{error}")))?;

        let mut env: BTreeMap<String, String> = komo_runtime::tools::shell::base_environment();
        env.insert("KOMO_SESSION".into(), session.to_string());
        env.insert("KOMO_MONITOR".into(), id.clone());
        let mut child = self.shell.child_spec(
            &spec.command,
            spec.cwd.clone(),
            env,
            Duration::from_secs(spec.timeout_minutes * 60),
            format!("monitor #{} · {}", monitor::short_id(&id), spec.title),
        );
        child.register = Some(ChildRegistration {
            registry: Arc::new(self.children.clone()),
            executor: ExecutorId::from_raw(id.clone()),
        });

        let live = Live {
            key: request_key,
            session: session.clone(),
            title: spec.title.clone(),
            timeout_minutes: spec.timeout_minutes,
            cancel: CancelToken::new(),
            ending: Arc::new(Mutex::new(None)),
        };
        let handle = live.handle(&id);
        self.running
            .lock()
            .expect("监听表")
            .insert(id.clone(), live);
        let attempt = AttemptRef {
            session: session.clone(),
            run: from.clone(),
            call: ToolCallId::from_raw(format!("monitor-{id}")),
            attempt: AttemptId::from_raw(id.clone()),
        };
        let peer = peer_raw.as_deref().and_then(ChannelPeer::parse);
        tracing::info!(monitor = %id, %session, title = %spec.title, "挂上监听");
        tokio::spawn(me.pump(id, session, peer, child, attempt));
        Ok(handle)
    }

    async fn stop(&self, from: &RunId, monitor_id: &str) -> Result<MonitorStopped, MonitorError> {
        let (session, _) = self.origin_of(from).await?;
        let running = self.running.lock().expect("监听表");
        let mine: Vec<(&String, &Live)> = running
            .iter()
            .filter(|(_, live)| live.session == session)
            .collect();
        let matched: Vec<&(&String, &Live)> = mine
            .iter()
            .filter(|(id, _)| monitor::matches_short_id(id, monitor_id))
            .collect();
        match matched.as_slice() {
            [] => {
                let listed = mine
                    .iter()
                    .map(|(id, live)| format!("#{}「{}」", monitor::short_id(id), live.title))
                    .collect::<Vec<_>>();
                Err(MonitorError::Unknown(if listed.is_empty() {
                    format!("#{monitor_id} 不在跑：这个会话现在没有挂着的监听")
                } else {
                    format!(
                        "#{monitor_id} 不在跑；这个会话现在挂着：{}",
                        listed.join("、")
                    )
                }))
            }
            [(id, live)] => {
                end_with(&live.ending, &live.cancel, Ending::Stopped);
                Ok(MonitorStopped {
                    short_id: monitor::short_id(id),
                    title: live.title.clone(),
                })
            }
            many => {
                let listed = many
                    .iter()
                    .map(|(id, _)| format!("#{}", &id[id.len().saturating_sub(6)..]))
                    .collect::<Vec<_>>()
                    .join("、");
                Err(MonitorError::Ambiguous(format!(
                    "#{monitor_id} 有歧义，候选：{listed}"
                )))
            }
        }
    }
}

/// 攒着还没交出去的行：开头几行与最新的那些，中间只记个数。
#[derive(Debug, Default)]
struct Batch {
    head: Vec<String>,
    tail: VecDeque<String>,
    total: usize,
    first_at: Option<Instant>,
}

impl Batch {
    fn push(&mut self, line: String, now: Instant) {
        self.total += 1;
        self.first_at.get_or_insert(now);
        if self.head.len() < HEAD_LINES {
            self.head.push(line);
            return;
        }
        self.tail.push_back(line);
        if self.tail.len() > MAX_EVENT_LINES - HEAD_LINES {
            self.tail.pop_front();
        }
    }

    /// 有行，而且第一行之后已经安静了 [`BATCH_QUIET`]。
    fn ready(&self, now: Instant) -> bool {
        self.first_at
            .is_some_and(|first| now.duration_since(first) >= BATCH_QUIET)
    }

    fn lines(&self) -> Vec<String> {
        self.head.iter().chain(self.tail.iter()).cloned().collect()
    }

    fn omitted(&self) -> usize {
        self.total - self.head.len() - self.tail.len()
    }
}

/// 一条事件的正文。
fn event_text(id: &str, title: &str, lines: &[String], omitted: usize) -> String {
    let short = monitor::short_id(id);
    format!(
        "[监听 #{short}「{title}」]\n\n{}\n\n（这是监听脚本 #{short} 的输出，不是用户发来的消息，\
         用户看不到上面这段。需要用户知道的由你转告；不用再盯了就调 monitor_stop。）",
        body(lines, omitted)
    )
}

/// 结束消息的正文。
fn end_text(
    id: &str,
    title: &str,
    headline: &str,
    lines: &[String],
    omitted: usize,
    stderr: &str,
) -> String {
    let short = monitor::short_id(id);
    let mut text = format!("[监听 #{short}「{title}」{headline}]");
    if !lines.is_empty() {
        text.push_str("\n\n最后的输出：\n");
        text.push_str(&body(lines, omitted));
    }
    if !stderr.trim().is_empty() {
        text.push_str("\n\nstderr 末尾：\n");
        text.push_str(stderr.trim_end());
    }
    text.push_str(
        "\n\n（这是系统消息，不是用户发来的；这个监听不会再有事件了。需要用户知道的由你转告。）",
    );
    text
}

fn body(lines: &[String], omitted: usize) -> String {
    if omitted == 0 || lines.len() <= HEAD_LINES {
        return lines.join("\n");
    }
    format!(
        "{}\n…（中间省略 {omitted} 行）…\n{}",
        lines[..HEAD_LINES].join("\n"),
        lines[HEAD_LINES..].join("\n")
    )
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// `ps -o etime=` 的那一格：`[[dd-]hh:]mm:ss`，换成秒。
fn parse_etime(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let (days, clock) = match raw.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, raw),
    };
    let parts: Vec<u64> = clock
        .split(':')
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<_>>()?;
    let seconds = match parts.as_slice() {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(days * 86_400 + seconds)
}

/// 这个 pid 已经跑了多少秒（`ps -o etime=`；Linux 与 macOS 都有）。
fn elapsed_secs(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_etime(&String::from_utf8_lossy(&output.stdout))
}

/// 把 stdout 切成行送进泵；stderr 只计数（结束消息从 `ChildOutcome` 取它的末尾）。
struct LineSink {
    tx: mpsc::UnboundedSender<String>,
    partial: Vec<u8>,
    written: u64,
    attempt: AttemptRef,
}

/// 没有换行的一段最多攒这么多字节，超了当作一行交出去。
const MAX_PARTIAL_BYTES: usize = 64 * 1024;

impl LineSink {
    fn new(tx: mpsc::UnboundedSender<String>, attempt: AttemptRef) -> Self {
        Self {
            tx,
            partial: Vec::new(),
            written: 0,
            attempt,
        }
    }

    fn emit(&self, raw: &[u8]) {
        let text = String::from_utf8_lossy(raw);
        let line = text.trim_end_matches(['\r', '\n']).trim_end();
        if line.trim().is_empty() {
            return;
        }
        let line: String = if line.chars().count() > MAX_LINE_CHARS {
            let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
            format!("{cut}…")
        } else {
            line.to_string()
        };
        // 泵已经不收了（它在收尾）：丢掉就是了。
        let _ = self.tx.send(line);
    }

    fn flush(&mut self) {
        if !self.partial.is_empty() {
            let rest = std::mem::take(&mut self.partial);
            self.emit(&rest);
        }
    }
}

#[async_trait]
impl OutputWriter for LineSink {
    async fn write_stdout(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.written += chunk.len() as u64;
        self.partial.extend_from_slice(chunk);
        while let Some(pos) = self.partial.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=pos).collect();
            self.emit(&line);
        }
        if self.partial.len() > MAX_PARTIAL_BYTES {
            self.flush();
        }
        Ok(())
    }

    async fn write_stderr(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.written += chunk.len() as u64;
        Ok(())
    }

    fn attempt(&self) -> &AttemptRef {
        &self.attempt
    }

    fn bytes_written(&self) -> u64 {
        self.written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt() -> AttemptRef {
        AttemptRef {
            session: SessionId::from_raw("s"),
            run: RunId::from_raw("r"),
            call: ToolCallId::from_raw("c"),
            attempt: AttemptId::from_raw("a"),
        }
    }

    #[tokio::test]
    async fn the_sink_splits_stdout_into_lines_across_chunks_and_drops_blank_ones() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sink = LineSink::new(tx, attempt());
        sink.write_stdout(b"job build: succ").await.unwrap();
        sink.write_stdout(b"ess\r\n\n  \njob test: fa")
            .await
            .unwrap();
        sink.write_stderr(b"noise\n").await.unwrap();
        sink.write_stdout(b"iled").await.unwrap();
        sink.flush();
        drop(sink);

        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        assert_eq!(lines, ["job build: success", "job test: failed"]);
    }

    #[test]
    fn a_batch_keeps_its_head_and_newest_lines_and_counts_the_rest() {
        let now = Instant::now();
        let mut batch = Batch::default();
        for i in 0..100 {
            batch.push(format!("l{i}"), now);
        }
        let lines = batch.lines();
        assert_eq!(lines.len(), MAX_EVENT_LINES);
        assert_eq!(lines[0], "l0");
        assert_eq!(lines[HEAD_LINES - 1], "l9");
        assert_eq!(lines[HEAD_LINES], "l70");
        assert_eq!(lines.last().unwrap(), "l99");
        assert_eq!(batch.omitted(), 60);

        let text = event_text("0199-abcd", "流水线", &lines, batch.omitted());
        assert!(text.starts_with("[监听 #abcd「流水线」]"), "{text}");
        assert!(text.contains("l9\n…（中间省略 60 行）…\nl70"), "{text}");
    }

    #[test]
    fn a_batch_waits_for_a_quiet_moment_after_its_first_line() {
        let start = Instant::now();
        let mut batch = Batch::default();
        assert!(!batch.ready(start + BATCH_QUIET * 5), "空的不交");
        batch.push("a".into(), start);
        assert!(!batch.ready(start));
        assert!(batch.ready(start + BATCH_QUIET));
    }

    #[test]
    fn a_decided_ending_wins_over_how_the_process_went() {
        let cancelled = ChildOutcome {
            cancelled: true,
            ..ChildOutcome::default()
        };
        let (end, _) = settle(Some(Ending::Flood), Ok(Ok(cancelled.clone())), 30);
        assert_eq!(end, Ending::Flood);
        let (end, _) = settle(None, Ok(Ok(cancelled)), 30);
        assert_eq!(end, Ending::Stopped);

        let timed_out = ChildOutcome {
            timed_out: true,
            stderr_excerpt: (1..=30).map(|i| format!("e{i}\n")).collect(),
            ..ChildOutcome::default()
        };
        let (end, stderr) = settle(None, Ok(Ok(timed_out)), 45);
        assert_eq!(end, Ending::TimedOut(45));
        assert!(
            stderr.starts_with("e11\n") && stderr.ends_with("e30"),
            "{stderr}"
        );
    }

    #[test]
    fn etime_parses_every_shape_ps_prints() {
        assert_eq!(parse_etime("  00:07"), Some(7));
        assert_eq!(parse_etime("12:34"), Some(754));
        assert_eq!(parse_etime("01:00:00"), Some(3600));
        assert_eq!(parse_etime("2-00:00:01"), Some(172_801));
        assert_eq!(parse_etime("garbage"), None);
    }

    #[test]
    fn the_end_text_carries_the_reason_the_last_lines_and_the_stderr_tail() {
        let text = end_text(
            "0199-abcd",
            "流水线",
            &Ending::Exited(Some(1)).headline(),
            &["job deploy: failed".to_string()],
            0,
            "glab: 401 Unauthorized\n",
        );
        assert!(
            text.starts_with("[监听 #abcd「流水线」已结束：脚本退出（exit=1）]"),
            "{text}"
        );
        assert!(text.contains("job deploy: failed"), "{text}");
        assert!(
            text.contains("stderr 末尾：\nglab: 401 Unauthorized"),
            "{text}"
        );
    }
}
