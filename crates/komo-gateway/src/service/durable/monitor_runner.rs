//! 监听的进程那一半（`docs/monitor.md`；D5 W2c）：[`MonitorRunner`] 的生产实现。
//!
//! runtime 管**登记**（Session 级 document `komo.monitors`：谁挂的、命令、时限、截止时刻、
//! 是否 `resume_after_restart`、是否已结束）与 `monitor` / `monitor_stop` 工具任务的"登记 → 起进程 →
//! 落结果"三明治；这里只剩进程：脚本按 `bash` 的规矩起（bash、数据目录只读挂载、明确的环境），stdout
//! 逐行进泵，泵把连着出的行合成一条，经 `Harness::submit(monitor::event_draft(..))` 交回挂它的
//! conversation；进程结束时调 `Harness::finish_monitor`（登记 `ended` 与结束消息同一次提交，只有
//! 第一个结束它的人赢）。**登记状态不再写 JSON**——旧 `runtime/monitors/<id>.json` 没有了，留在
//! `runtime/monitors/children/` 的只有进程身份（pid / 进程组 / 起来的时刻）。
//!
//! 几条不能松的规矩（与旧 `monitors.rs` 一致）：
//!
//! - **同一个监听同时只有一条事件在飞。** 每条事件都是一轮模型；上一条没收尾时新出的行攒着，合进
//!   下一条。"收尾"= 那条输入 submission 已结算，或者它并进了别的输入开的 run（`Placed` 且不是
//!   `komo.live.run` 的首个输入）——并进去的已经在模型手里了，旧实现里的 `absorbed` 同理。
//! - **默认不跨重启。** 进程只活在这一代 Gateway 里。停机时终止进程组、登记**原样留着**
//!   （`running`）：下一代启动时 [`MonitorRunner::boot`] 先核对身份、终止遗留的进程组，再由
//!   `Harness::reconcile_monitors` 判重挂（声明了 `resume_after_restart` 且还剩 ≥ 1 分钟）或交回
//!   "已中断"。续跑的 `monitor` 调用认不到进程时答 `Interrupted`，绝不自己重新起进程（§8.6）。
//! - **在册是自己的。** 子进程不登记进执行器的 `runtime/children/`，而是 `runtime/monitors/children/`，
//!   以监听 id 为 executor：那里一行活着的进程会让恢复扫描把上一代的所有 Run 停下来（§8.7）。
//! - **幂等。** 同一个 id 已经在跑，`start` 就是认回（什么都不做）；`stop` 对不在的 id 也算成功。
//!
//! `bash` 转后台的命令（[`detached`]）与监听同一套：登记（`Harness::register_monitor`）、
//! `monitor_stop`、停机与重启收尾都一样，只是不发行事件、只在结束时交回一条消息。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::durable::{ConversationId, Storage, SubmissionId, SubmissionStatus};
use komo_kernel::traits::{OutputWriter, StoreError};
use komo_kernel::types::ids::{AttemptId, ExecutorId, RunId, SessionId, ToolCallId};
use komo_kernel::types::refs::AttemptRef;
use komo_kernel::types::tool::CancelToken;
use komo_runtime::durable::harness::orchestration::monitor;
use komo_runtime::durable::harness::{
    Harness, LaunchMode, MonitorError, MonitorLaunch, MonitorRecord,
};
use komo_runtime::recovery::{ChildRegistry, Liveness, ProcessProbe, SysProcessProbe};
use komo_runtime::tools::BashTool;
use komo_runtime::tools::detach::Detacher;
use komo_runtime::tools::process::{
    ChildOutcome, ChildRegistration, ChildSpec, ProcessError, kill_process_group, run_child,
};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::DurableCore;
use super::assemble::StateSlot;
use super::hooks::state_of;
use super::orchestration::MonitorRunner;
use crate::service::state::{GatewayState, protected_paths};

pub mod detached;

/// 第一行出来之后再等这么久，把连着出的行合进同一条事件。
const BATCH_QUIET: Duration = Duration::from_secs(1);
/// 泵看一眼"能不能交下一条"的间隔。
const TICK: Duration = Duration::from_millis(250);
/// 一条事件最多带几行：开头留 [`HEAD_LINES`] 行，其余留最新的，中间写明省了几行。
pub(crate) const MAX_EVENT_LINES: usize = 40;
pub(crate) const HEAD_LINES: usize = 10;
/// 一行最多留多少个字符。
const MAX_LINE_CHARS: usize = 1000;
/// 一条事件攒到这么多行还没交出去：脚本没有过滤，停掉它。
const FLOOD_LINES: usize = 2000;
/// 一个监听最多交回几条事件（不含结束那条）。每条都是一轮模型。
const MAX_EVENTS: u64 = 200;
/// 在飞的那条事件等这么久还没收尾，就不再等它（下一条照样排在它后面）。
const IN_FLIGHT_PATIENCE: Duration = Duration::from_secs(600);
/// 结束消息里带 stderr 的最后几行。
pub(crate) const STDERR_TAIL_LINES: usize = 20;
/// 停机时等泵收尾的上限（`run_child` 自己 TERM → KILL 各等 5 秒）。
const SHUTDOWN_WAIT: Duration = Duration::from_secs(12);

/// 监听为什么结束了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ending {
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
    /// 模型调了 `monitor_stop`：登记已经是 `ended{stopped}`（`monitor_stop` 先写意图再停进程），
    /// 它自己知道，不再交回结束消息。
    Stopped,
    /// Gateway 停机：登记留给下一代启动时对账。
    Shutdown,
}

impl Ending {
    pub(crate) fn headline(&self) -> String {
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

    /// 登记里 `ended.reason` 的词。
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Ending::Exited(_) => "exit",
            Ending::TimedOut(_) => "timeout",
            Ending::Flood => "flood",
            Ending::TooManyEvents => "limit",
            Ending::Failed(_) => "failed",
            Ending::Stopped => "stopped",
            Ending::Shutdown => "shutdown",
        }
    }
}

/// 正在跑的一个监听（或转后台的命令）。
pub(crate) struct Live {
    pub title: String,
    pub conversation: ConversationId,
    pub timeout_minutes: u64,
    pub cancel: CancelToken,
    /// 由外面决定的结束原因（停、停机、泵自己判的刷屏）。先写原因再取消。
    pub ending: Arc<Mutex<Option<Ending>>>,
}

pub(crate) fn end_with(ending: &Mutex<Option<Ending>>, cancel: &CancelToken, why: Ending) {
    let mut slot = ending.lock().expect("监听的结束原因");
    if slot.is_none() {
        *slot = Some(why);
    }
    drop(slot);
    cancel.cancel();
}

/// 一次调用要用的东西（从 `GatewayState` 现取，不在构造时抓：构造时它还不存在）。
pub(crate) struct Ctx {
    pub state: Arc<GatewayState>,
    pub core: Arc<DurableCore>,
    /// `runtime/monitors/children`：每个监听一份在册（executor 位上写监听 id）。
    pub children: ChildRegistry,
}

pub struct GatewayMonitorRunner {
    slot: StateSlot,
    pub(crate) me: Weak<GatewayMonitorRunner>,
    running: Mutex<HashMap<u64, Live>>,
    closing: AtomicBool,
    /// 上一代遗留进程的核对只做一次，而且在任何新进程起来之前（它会清掉在册文件）。
    reaped: tokio::sync::OnceCell<()>,
}

impl GatewayMonitorRunner {
    pub fn new(slot: StateSlot) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            slot,
            me: me.clone(),
            running: Mutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
            reaped: tokio::sync::OnceCell::new(),
        })
    }

    pub(crate) fn ctx(&self) -> Result<Ctx, MonitorError> {
        let state = state_of(&self.slot).map_err(|e| MonitorError::Failed(e.to_string()))?;
        let core = Arc::clone(
            state
                .core()
                .map_err(|e| MonitorError::Failed(e.to_string()))?,
        );
        let dir = state.snapshot().paths.runtime_dir.join("monitors");
        Ok(Ctx {
            state,
            core,
            children: ChildRegistry::new(dir.join("children")),
        })
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    pub(crate) fn is_running(&self, id: u64) -> bool {
        self.running.lock().expect("监听表").contains_key(&id)
    }

    pub(crate) fn insert(&self, id: u64, live: Live) {
        self.running.lock().expect("监听表").insert(id, live);
    }

    /// 泵收尾：从在跑表里摘掉。
    pub(crate) fn retire(&self, id: u64) {
        self.running.lock().expect("监听表").remove(&id);
    }

    async fn ensure_reaped(&self, ctx: &Ctx) {
        self.reaped
            .get_or_init(|| async { reap_leftover_children(&ctx.children).await })
            .await;
    }

    /// 起一个监听的进程并交给泵。新挂（`Fresh`）与重挂（`Resume`）都走这里；`limit` 是剩下的时限。
    async fn launch(
        &self,
        ctx: &Ctx,
        record: &MonitorRecord,
        limit: Duration,
        restarted: bool,
    ) -> Result<(), MonitorError> {
        let me = self
            .me
            .upgrade()
            .ok_or_else(|| MonitorError::Failed("Gateway 正在停机".into()))?;
        if self.is_closing() {
            return Err(MonitorError::Failed("Gateway 正在停机".into()));
        }
        if !record.cwd.is_dir() {
            return Err(MonitorError::Failed(format!(
                "工作目录不存在：{}",
                record.cwd.display()
            )));
        }
        let id = record.id;
        let session = session_of(&*ctx.core.storage, record.conversation).await;

        let mut env: BTreeMap<String, String> = komo_runtime::tools::bash::base_environment();
        env.insert("KOMO_SESSION".into(), session.to_string());
        env.insert("KOMO_MONITOR".into(), id.to_string());
        let snapshot = ctx.state.snapshot();
        let shell = BashTool::new().protecting(protected_paths(&snapshot, &ctx.state.home));
        let mut child = shell.child_spec(
            &record.command,
            record.cwd.clone(),
            env,
            limit,
            format!("monitor #{id} · {}", record.title),
        );
        child.register = Some(ChildRegistration {
            registry: Arc::new(ctx.children.clone()),
            executor: ExecutorId::from_raw(id.to_string()),
        });

        let live = Live {
            title: record.title.clone(),
            conversation: record.conversation,
            timeout_minutes: record.timeout_minutes,
            cancel: CancelToken::new(),
            ending: Arc::new(Mutex::new(None)),
        };
        self.insert(id, live);
        let attempt = AttemptRef {
            session,
            run: RunId::from_raw(record.conversation.to_string()),
            call: ToolCallId::from_raw(format!("monitor-{id}")),
            attempt: AttemptId::from_raw(id.to_string()),
        };
        tracing::info!(monitor = id, title = %record.title, restarted, "挂上监听");
        tokio::spawn(me.pump(Arc::clone(&ctx.core), id, child, attempt, restarted));
        Ok(())
    }

    /// 在飞的那条事件收尾了吗：输入已结算，或者并进了别的输入开的 run（后面的事件反正会在下一个
    /// boundary 并进去）。
    async fn settled(
        core: &DurableCore,
        conversation: ConversationId,
        input: SubmissionId,
    ) -> bool {
        match core.storage.submission(input).await {
            Ok(Some(record)) => match record.status() {
                SubmissionStatus::Done | SubmissionStatus::Unanswered => true,
                SubmissionStatus::Queued => false,
                SubmissionStatus::Placed => {
                    match Harness::live(&*core.storage, conversation).await {
                        // 它自己开的 run 还在跑：没收尾。
                        Ok(live) => !live
                            .run
                            .is_some_and(|run| run.inputs.first() == Some(&input)),
                        Err(_) => false,
                    }
                }
            },
            Ok(None) => true,
            // 读不出来就当它还在跑：等满 IN_FLIGHT_PATIENCE 或下一拍再看。
            Err(_) => false,
        }
    }

    /// 泵：收行、合批、交回，脚本收尾后交回结束消息。
    async fn pump(
        self: Arc<Self>,
        core: Arc<DurableCore>,
        id: u64,
        spec: ChildSpec,
        attempt: AttemptRef,
        restarted: bool,
    ) {
        let (title, conversation, cancel, ending, timeout_minutes) = {
            let running = self.running.lock().expect("监听表");
            let live = running.get(&id).expect("泵起来之前已经登记");
            (
                live.title.clone(),
                live.conversation,
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
        // 重挂的监听：第一条事件注明中间断过（断开那几秒里打的行没人收到）。
        let mut gap_note = restarted;
        // 事件的请求键 `monitor:{id}:{suffix}`：重挂后计数从头来，后缀里得多一段这一代独有的，
        // 否则撞上上一代交过的 `monitor:{id}:1`，被当成同一个请求吞掉。
        let generation =
            restarted.then(|| komo_kernel::types::ids::uuid_v7_at(core.clock.now()).to_string());
        let mut in_flight: Option<(SubmissionId, Instant)> = None;
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
                    if let Some((input, since)) = &in_flight
                        && (Self::settled(&core, conversation, *input).await
                            || since.elapsed() >= IN_FLIGHT_PATIENCE)
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
                    let mut text = event_text(id, &title, &batch.lines(), batch.omitted());
                    if std::mem::take(&mut gap_note) {
                        text.push_str(RESTART_NOTE);
                    }
                    batch = Batch::default();
                    let suffix = match &generation {
                        Some(generation) => format!("{generation}:{events}"),
                        None => events.to_string(),
                    };
                    // steer：宿主还在跑时在下一个工具边界并进去，不排到整条 Run 后面。
                    let draft = monitor::event_draft(id, &title, &suffix, text);
                    match core.harness.submit(&core.engine, conversation, draft).await {
                        Ok(input) => in_flight = Some((input, Instant::now())),
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
            // 停：登记已经是 `ended{stopped}`；停机：登记留给下一代对账。都不交回。
            Ending::Stopped | Ending::Shutdown => {}
            ref end => {
                let headline = end.headline();
                let text = end_text(
                    id,
                    &title,
                    &headline,
                    &batch.lines(),
                    batch.omitted(),
                    &stderr,
                );
                let detail = format!("{headline}；结束消息会单独交回会话（可能已经到了）");
                if let Err(error) = core
                    .harness
                    .finish_monitor(&core.engine, id, end.reason(), Some(&detail), text)
                    .await
                {
                    tracing::warn!(%error, monitor = id, "监听的结束消息交不回会话");
                }
            }
        }
        self.retire(id);
    }
}

/// conversation 属于哪个 Session（`KOMO_SESSION` 与泵的输出身份用）；没登记就是 `c{conversation}`。
pub(crate) async fn session_of(storage: &dyn Storage, conversation: ConversationId) -> SessionId {
    match storage.conversation(conversation).await {
        Ok(Some(record)) => match record.session {
            Some(session) => SessionId::from_raw(session),
            None => SessionId::from_raw(format!("c{conversation}")),
        },
        _ => SessionId::from_raw(format!("c{conversation}")),
    }
}

/// 上一代的监听进程组：还活着、而且起来的时刻对得上登记，才终止它。pid 会被复用，对不上时宁可留着
/// 一个孤儿，也不误杀别人的进程组。登记文件随后删掉。
async fn reap_leftover_children(children: &ChildRegistry) {
    let Ok(entries) = std::fs::read_dir(children.dir()) else {
        return;
    };
    let now = time::OffsetDateTime::now_utc();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let executor = ExecutorId::from_raw(id.to_string());
        for child in children.children(&executor) {
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

#[async_trait]
impl MonitorRunner for GatewayMonitorRunner {
    async fn start(&self, launch: &MonitorLaunch) -> Result<(), MonitorError> {
        let ctx = self.ctx()?;
        self.ensure_reaped(&ctx).await;
        let record = &launch.record;
        // 同一个 id 已经在跑：认回（工具任务重放、对账再问一遍都落在这里）。
        if self.is_running(record.id) {
            return Ok(());
        }
        match launch.mode {
            // 认不到就是中断了，**绝不起新进程**（§8.6）；runtime 据此判重挂还是登记中断。
            LaunchMode::Recover => Err(MonitorError::Interrupted(String::new())),
            LaunchMode::Fresh => {
                let limit = Duration::from_secs(record.timeout_minutes * 60);
                self.launch(&ctx, record, limit, false).await
            }
            LaunchMode::Resume => {
                let limit = Duration::from_millis(launch.remaining_ms.max(0) as u64);
                self.launch(&ctx, record, limit, true).await
            }
        }
    }

    async fn stop(&self, id: u64) -> Result<(), MonitorError> {
        if let Some(live) = self.running.lock().expect("监听表").get(&id) {
            end_with(&live.ending, &live.cancel, Ending::Stopped);
        }
        Ok(())
    }

    async fn boot(&self) {
        let Ok(ctx) = self.ctx() else { return };
        self.ensure_reaped(&ctx).await;
        self.finish_leftover_detached(&ctx).await;
    }

    async fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
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

    fn detacher(&self) -> Option<Arc<dyn Detacher>> {
        self.me.upgrade().map(|runner| runner as Arc<dyn Detacher>)
    }
}

/// 监听已经结束时给 `Ending` 一个结算：外面定过的优先，否则看进程怎么收尾的。第二个值是 stderr 的
/// 最后几行。
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

/// 攒着还没交出去的行：开头几行与最新的那些，中间只记个数。
#[derive(Debug, Default)]
pub(crate) struct Batch {
    head: Vec<String>,
    tail: VecDeque<String>,
    pub(crate) total: usize,
    first_at: Option<Instant>,
}

impl Batch {
    pub(crate) fn push(&mut self, line: String, now: Instant) {
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
    pub(crate) fn ready(&self, now: Instant) -> bool {
        self.first_at
            .is_some_and(|first| now.duration_since(first) >= BATCH_QUIET)
    }

    pub(crate) fn lines(&self) -> Vec<String> {
        self.head.iter().chain(self.tail.iter()).cloned().collect()
    }

    pub(crate) fn omitted(&self) -> usize {
        self.total - self.head.len() - self.tail.len()
    }
}

/// 重挂之后第一条事件的附注。
const RESTART_NOTE: &str =
    "\n\n（中间 Gateway 重启过，这个监听已按原句柄自动重挂；断开那几秒里脚本打的行没有收到。）";

/// 一条事件的正文。句柄是监听 id（`#id`，`monitor_stop` 用它）。
pub(crate) fn event_text(id: u64, title: &str, lines: &[String], omitted: usize) -> String {
    format!(
        "[监听 #{id}「{title}」]\n\n{}\n\n（这是监听脚本 #{id} 的输出，不是用户发来的消息，\
         用户看不到上面这段。需要用户知道的由你转告；不用再盯了就调 monitor_stop。）",
        body(lines, omitted)
    )
}

/// 结束消息的正文。
pub(crate) fn end_text(
    id: u64,
    title: &str,
    headline: &str,
    lines: &[String],
    omitted: usize,
    stderr: &str,
) -> String {
    let mut text = format!("[监听 #{id}「{title}」{headline}]");
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

impl std::fmt::Debug for GatewayMonitorRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayMonitorRunner")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt() -> AttemptRef {
        AttemptRef {
            session: SessionId::from_raw("s"),
            run: RunId::from_raw("1"),
            call: ToolCallId::from_raw("2"),
            attempt: AttemptId::from_raw("3"),
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

        let text = event_text(4242, "流水线", &lines, batch.omitted());
        assert!(text.starts_with("[监听 #4242「流水线」]"), "{text}");
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
            4242,
            "流水线",
            &Ending::Exited(Some(1)).headline(),
            &["job deploy: failed".to_string()],
            0,
            "glab: 401 Unauthorized\n",
        );
        assert!(
            text.starts_with("[监听 #4242「流水线」已结束：脚本退出（exit=1）]"),
            "{text}"
        );
        assert!(text.contains("job deploy: failed"), "{text}");
        assert!(
            text.contains("stderr 末尾：\nglab: 401 Unauthorized"),
            "{text}"
        );
    }

    #[test]
    fn every_ending_has_a_registry_reason() {
        for (end, reason) in [
            (Ending::Exited(Some(0)), "exit"),
            (Ending::TimedOut(5), "timeout"),
            (Ending::Flood, "flood"),
            (Ending::TooManyEvents, "limit"),
            (Ending::Failed("x".into()), "failed"),
            (Ending::Stopped, "stopped"),
            (Ending::Shutdown, "shutdown"),
        ] {
            assert_eq!(end.reason(), reason);
        }
    }
}
