//! ratatui 聊天 TUI（§13.4、§14 阶段 1）。
//!
//! 分工是这个模块存在的形状：[`app`] 是**无终端的状态机**（按键 / 服务端事件 → 状态 +
//! [`Effect`]），[`render`] 只画，本模块是**驱动**——把终端事件喂进状态机，把状态机要的
//! 效果打到 HTTP 上，把服务端的回答喂回去。三者之间只有值，没有互相调用。

pub mod app;
pub mod approval;
pub mod command;
pub mod markdown;
pub mod paste;
pub mod render;
pub mod transcript;

#[cfg(test)]
pub mod test_support;

use std::io::{Stdout, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyEventKind,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::style::{
    Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{execute, queue};
use komo_kernel::protocol::http::{
    BoundaryRequest, CancelRunRequest, CreateSessionRequest, EventQuery, InterventionAnswerRequest,
    InterventionBatchAnswerRequest, InterventionDetail, InterventionListQuery, ResumeRequest,
    SubmitRunRequest,
};
use komo_kernel::protocol::sse::Cursor;
use komo_kernel::types::ids::{SessionId, uuid_v7_at};
use ratatui::backend::{CrosstermBackend, IntoCrossterm};
use ratatui::layout::{Rect, Size};
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

pub use app::{App, Effect, Phase, ServerEvent, TuiMode};

use crate::api::KomoClient;
use crate::error::ClientError;
use crate::sse::{self, ConnectionState, SseMessage};
use crate::tui::paste::{InputEvent, TimedKey, coalesce_rapid_keys};
use crate::tui::transcript::Emitted;

/// 补读历史时一页取多少条。
const BACKFILL_PAGE: u32 = 500;
/// 状态行上的耗时靠它走动。
const TICK: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("终端出错：{0}")]
    Terminal(#[from] std::io::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// 进入 TUI。
///
/// `session` 是**手里已经有的**会话：`komo resume` / `komo home` 直接给。`komo` 那条
/// 命令不带（`None`）——它**不在启动时建会话**，看一眼就退出不该在账本和磁盘上留下一个
/// 空壳；会话在**第一条消息送出去之前**才铸（[`open_session`]）。哪一种由 `mode` 说，
/// 它也决定要不要**先补读整段历史再进入交互**（§8.8）。
pub async fn run_tui(
    client: KomoClient,
    session: Option<SessionId>,
    mode: TuiMode,
) -> Result<(), TuiError> {
    let seed = uuid_v7_at(time::OffsetDateTime::now_utc()).to_string();
    let mut app = App::new(session.clone(), mode, seed);

    let (events_tx, mut events) = mpsc::channel::<ServerEvent>(256);

    // §8.8：resume 打开时先按游标补读全部历史，再进入交互。
    if mode.needs_backfill() {
        let session = session.as_ref().expect("要补读的两种打开方式都带着会话");
        backfill(&client, session, &events_tx).await;
    }
    let _ = events_tx.send(ServerEvent::HistoryDone).await;

    if mode == TuiMode::Resume {
        let session = session.as_ref().expect("resume 带着会话");
        // 恢复调度由 Gateway 自动进行；这一下只是问「现在到哪了」（§3 命令表）。
        match client.resume(session, &ResumeRequest::default()).await {
            Ok(response) => {
                let _ = events_tx
                    .send(ServerEvent::Resumed(Box::new(response)))
                    .await;
            }
            Err(error) => {
                let _ = events_tx
                    .send(ServerEvent::Failed(format!("resume 失败：{error}")))
                    .await;
            }
        }
    }

    // 一开机就问一次模型清单：`/model <id>` 与 `/effort <level>` 的校验靠它，而
    // §13.3 要求「不支持的 effort 在请求前拒绝」——拿不到清单只是退到内建白名单，
    // 不是让人在发出请求之后才发现。
    if let Ok(models) = client.models().await {
        let _ = events_tx.send(ServerEvent::ModelMenu(models.models)).await;
    }

    // 先把补读来的事件吃完，游标才是对的——订阅要从它之后开始。
    while let Ok(event) = events.try_recv() {
        let effects = app.apply(event);
        spawn_effects(&client, app.session.as_ref(), effects, &events_tx);
    }

    // 还没有会话（`komo` 裸命令）就没有订阅：它在第一条消息铸出会话之后才建立。
    let mut subscription =
        session.map(|session| sse::subscribe(&client, session, Cursor::after(app.cursor)));
    let mut just_opened: Option<SessionId> = None;
    let mut ended = false;

    // **先开终端，再起读键线程**：视口定位要问一次"光标现在在第几行"，而 crossterm 说得
    // 很清楚——另一个线程正在 `event::read` 时这一问会被它吃掉、然后超时。开场那一问
    // 因此必须发生在读键线程存在之前；之后的事这个界面自己记账，一次都不再问。
    let mut terminal = TerminalGuard::enter(&app)?;
    let stop = Arc::new(AtomicBool::new(false));
    let (mut input, reader) = spawn_input_reader(stop.clone());
    // 开场横幅先落进终端的回滚区：它是普通的两行输出，滚上去就看不见了，这正是它该有的
    // 分量（会话 Id 在这里闪过一次）。
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let _ = terminal.commit(render::banner(&app, &cwd));
    let mut emitted = Emitted::new();
    let mut ticker = tokio::time::interval(TICK);

    let outcome = loop {
        if app.quit {
            break Ok(());
        }
        app.now = Some(time::OffsetDateTime::now_utc());
        if let Err(error) = terminal.render(&app, &mut emitted) {
            break Err(TuiError::Terminal(error));
        }

        tokio::select! {
            batch = input.recv() => match batch {
                Some(batch) => {
                    for event in batch {
                        let effects = app.handle_input(event);
                        let (opened, effects) = open_session(&client, &mut app, effects).await;
                        just_opened = just_opened.or(opened);
                        spawn_effects(&client, app.session.as_ref(), effects, &events_tx);
                    }
                }
                None => break Ok(()),
            },
            event = next_stream(&mut subscription) => match event {
                Stream::Frame(SseMessage::Frame(frame)) => {
                    let effects = app.apply(ServerEvent::Frame(frame));
                    spawn_effects(&client, app.session.as_ref(), effects, &events_tx);
                }
                Stream::Frame(SseMessage::Skipped { id, reason }) => {
                    app.apply(ServerEvent::FrameSkipped { id, reason });
                }
                Stream::Connection(state) => {
                    app.apply(ServerEvent::Connection(state));
                }
                Stream::Ended => {
                    // 句柄在 `select!` 之外丢掉：`recv` 关掉之后恒返回 `None`，留着这条
                    // 分支会每一拍都立刻打转。
                    ended = true;
                    app.apply(ServerEvent::Notice("事件订阅已结束".into()));
                }
            },
            Some(event) = events.recv() => {
                let effects = app.apply(event);
                spawn_effects(&client, app.session.as_ref(), effects, &events_tx);
            }
            _ = ticker.tick() => {}
        }

        // 会话刚铸出来：订阅在这里建立，`select!` 的分支体里换它借不过编译器——那一
        // 路正借着 `subscription`。
        if let Some(session) = just_opened.take() {
            subscription = Some(sse::subscribe(&client, session, Cursor::after(app.cursor)));
        }
        if ended {
            subscription = None;
            ended = false;
        }
    };

    stop.store(true, Ordering::Relaxed);
    drop(reader);
    if let Some(subscription) = subscription {
        subscription.abort();
    }
    drop(terminal);
    // 退出即暂停：Run 留在 Gateway 里照跑，人回来靠的是**这一行命令**。会话 Id 只在身份
    // 行上闪过一次，退出这一刻正是需要它的时刻。会话还没铸出来（`komo` 一个字没发就退）
    // 就没有可 resume 的东西，什么都不印——那正是"不在账本上留空壳"的同一条规矩。
    if let Some(session) = app.session.as_ref() {
        println!("komo resume {session}");
    }
    outcome
}

/// 订阅这一路给驱动的东西。
enum Stream {
    /// 事件流上的一帧。
    Frame(SseMessage),
    /// 连接状态变了——状态行读它。
    Connection(ConnectionState),
    /// 订阅结束了。
    Ended,
}

/// 等订阅上的下一件事。
///
/// **还没有会话时永不就绪**：`select!` 的分支数由代码写死，没有订阅不等于少一条分支。
async fn next_stream(subscription: &mut Option<sse::SseHandle>) -> Stream {
    let Some(handle) = subscription.as_mut() else {
        return std::future::pending().await;
    };
    let mut connection = handle.state();
    tokio::select! {
        message = handle.recv() => match message {
            Some(message) => Stream::Frame(message),
            None => Stream::Ended,
        },
        Ok(()) = connection.changed() => Stream::Connection(connection.borrow().clone()),
    }
}

/// 第一条消息送出去之前把会话铸出来（`komo` 裸命令**不在启动时建会话**）。
///
/// 返回刚铸出来的那个 ID——订阅要在 `select!` 之外建立（见 [`run_tui`] 的循环）。铸不
/// 出来时那条消息**不假装送出去了**：`Submit` 从效果里摘掉，同一次失败按
/// [`ServerEvent::SubmitFailed`] 贴回它自己那条本地消息。
async fn open_session(
    client: &KomoClient,
    app: &mut App,
    mut effects: Vec<Effect>,
) -> (Option<SessionId>, Vec<Effect>) {
    let submitting = effects
        .iter()
        .any(|effect| matches!(effect, Effect::Submit { .. }));
    if app.session.is_some() || !submitting {
        return (None, effects);
    }
    match client
        .create_session(&CreateSessionRequest::default())
        .await
    {
        Ok(summary) => {
            // 会话 Id 在开场横幅上闪过一次（[`render::banner`]），而裸 `komo` 那条路上
            // 横幅印出来的时候它还不存在——补在这里，否则这个界面从头到尾说不出自己是谁。
            app.note(format!("会话 {}", summary.session));
            app.session = Some(summary.session.clone());
            (Some(summary.session), effects)
        }
        Err(error) => {
            let error = format!("建会话失败：{error}");
            effects.retain(|effect| match effect {
                Effect::Submit { request_key, .. } => {
                    app.apply(ServerEvent::SubmitFailed {
                        request_key: request_key.clone(),
                        error: error.clone(),
                    });
                    false
                }
                _ => true,
            });
            (None, effects)
        }
    }
}

/// 按游标一页一页读完整段历史。
async fn backfill(client: &KomoClient, session: &SessionId, events: &mpsc::Sender<ServerEvent>) {
    let mut query = EventQuery {
        from: komo_kernel::types::ids::Seq::ZERO,
        limit: Some(BACKFILL_PAGE),
    };
    loop {
        match client.events(session, &query).await {
            Ok(page) => {
                let more = page.more;
                query.from = page.next;
                let empty = page.events.is_empty();
                let _ = events.send(ServerEvent::HistoryPage(Box::new(page))).await;
                if !more || empty {
                    return;
                }
            }
            Err(error) => {
                let _ = events
                    .send(ServerEvent::Failed(format!("补读历史失败：{error}")))
                    .await;
                return;
            }
        }
    }
}

/// 把一串 [`Effect`] 打到 HTTP 上；回答走 `events` 回到状态机。
///
/// `session` 是 `None` 的只有一种情形：`komo` 裸命令里第一条消息之前的那些**不需要
/// 会话**的效果（待处理清单、模型清单）。需要会话的那几个由 [`App`] 在还没有会话时
/// 就不发（`/new`、`/status`），或者由 [`open_session`] 先铸出来（提交）。
fn spawn_effects(
    client: &KomoClient,
    session: Option<&SessionId>,
    effects: Vec<Effect>,
    events: &mpsc::Sender<ServerEvent>,
) {
    for effect in effects {
        let client = client.clone();
        let session = session.cloned();
        let events = events.clone();
        tokio::spawn(async move {
            let reply = run_effect(&client, session.as_ref(), effect).await;
            if let Some(reply) = reply {
                let _ = events.send(reply).await;
            }
        });
    }
}

async fn run_effect(
    client: &KomoClient,
    session: Option<&SessionId>,
    effect: Effect,
) -> Option<ServerEvent> {
    match effect {
        Effect::Submit {
            request_key,
            text,
            model,
            effort,
        } => {
            let reply_key = request_key.clone();
            let Some(session) = session else {
                // 铸会话那一步在 [`open_session`] 里，铸不出来时这条效果已经被摘掉；
                // 真漏到这里也要给那条本地消息一个结论，不能让它永远停在"发送中"。
                return Some(ServerEvent::SubmitFailed {
                    request_key: reply_key,
                    error: "还没有会话".into(),
                });
            };
            let request = SubmitRunRequest {
                request_key,
                text,
                model,
                effort,
            };
            match client.submit_run(session, &request).await {
                // HTTP 回执先更新本地 pending；`run.accepted` 回来后再由权威事件替换。
                Ok(response) => Some(ServerEvent::Submitted {
                    request_key: reply_key,
                    response: Box::new(response),
                }),
                Err(error) => Some(ServerEvent::SubmitFailed {
                    request_key: reply_key,
                    error: error.to_string(),
                }),
            }
        }
        Effect::Cancel { run, request_key } => {
            let request = CancelRunRequest {
                request_key: Some(request_key),
            };
            match client.cancel_run(&run, &request).await {
                Ok(response) => Some(ServerEvent::Notice(format!(
                    "{} 现在是 {}",
                    response.run,
                    app::status_text(response.state)
                ))),
                Err(error) => Some(ServerEvent::Failed(format!("取消失败：{error}"))),
            }
        }
        Effect::Boundary => match session {
            Some(session) => match client.boundary(session, &BoundaryRequest::default()).await {
                Ok(_) => None,
                Err(error) => Some(ServerEvent::Failed(format!("/new 失败：{error}"))),
            },
            None => Some(ServerEvent::Failed(
                "还没有会话：/new 先要有第一条消息".into(),
            )),
        },
        Effect::FetchApproval(handle) => match client.intervention(&handle).await {
            // 详情只有审批那一种会弹窗：另两类在清单、状态行与提示行里说得出路（§7.5）。
            Ok(InterventionDetail::Approval(record)) => Some(ServerEvent::Approval(record)),
            Ok(other) => Some(ServerEvent::Failed(format!(
                "{handle} 不是一条审批（{}），没有可打开的弹窗",
                match other {
                    InterventionDetail::Approval(_) => unreachable!("上面已经取走了"),
                    InterventionDetail::Verify { .. } => "结果不明",
                    InterventionDetail::Blocked { .. } => "阻塞",
                }
            ))),
            Err(error) => Some(ServerEvent::Failed(format!("取详情失败：{error}"))),
        },
        Effect::Answer {
            handle,
            verdict,
            scope,
            request_key,
        } => {
            let request = InterventionAnswerRequest {
                verdict,
                scope,
                request_key: Some(request_key),
            };
            match client.answer_intervention(&handle, &request).await {
                Ok(response) => Some(ServerEvent::Answered(Box::new(response))),
                Err(error) => Some(ServerEvent::Failed(format!("答复失败：{error}"))),
            }
        }
        Effect::AnswerMany {
            handles,
            approved,
            request_key,
        } => {
            let request = InterventionBatchAnswerRequest {
                handles,
                approved,
                request_key: Some(request_key),
            };
            match client.answer_interventions(&request).await {
                Ok(response) => Some(ServerEvent::BatchAnswered(Box::new(response))),
                Err(error) => Some(ServerEvent::Failed(format!("批量答复失败：{error}"))),
            }
        }
        Effect::FetchPending => match client
            .interventions(&InterventionListQuery::default())
            .await
        {
            Ok(response) => Some(ServerEvent::Pending(response.interventions)),
            Err(error) => Some(ServerEvent::Failed(format!("取待处理清单失败：{error}"))),
        },
        Effect::FetchStatus => match session {
            Some(session) => match client.session(session).await {
                Ok(detail) => Some(ServerEvent::Status(Box::new(detail))),
                Err(error) => Some(ServerEvent::Failed(format!("取状态失败：{error}"))),
            },
            None => Some(ServerEvent::Failed(
                "还没有会话：/status 先要有第一条消息".into(),
            )),
        },
        Effect::FetchModels => match client.models().await {
            Ok(response) => Some(ServerEvent::ModelMenu(response.models)),
            Err(error) => Some(ServerEvent::Failed(format!("取模型清单失败：{error}"))),
        },
        Effect::Quit => None,
    }
}

/// 终端事件读取：一个阻塞线程，成批送出。
///
/// **成批**是为了合并按键：没有括号粘贴的终端把一次粘贴拆成一串按键，只有拿到一批才
/// 看得出它们之间没有间隔（[`coalesce_rapid_keys`]）。
fn spawn_input_reader(
    stop: Arc<AtomicBool>,
) -> (mpsc::Receiver<Vec<InputEvent>>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Vec<InputEvent>>(64);
    let handle = std::thread::spawn(move || {
        let start = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            let mut raw: Vec<(TermEvent, u64)> = Vec::new();
            loop {
                match crossterm::event::read() {
                    Ok(event) => raw.push((event, start.elapsed().as_millis() as u64)),
                    Err(_) => return,
                }
                if !matches!(crossterm::event::poll(Duration::ZERO), Ok(true)) {
                    break;
                }
            }
            let batch = interpret(raw);
            if !batch.is_empty() && tx.blocking_send(batch).is_err() {
                return;
            }
        }
    });
    (rx, handle)
}

/// 一批终端事件 → 一批输入事件。按键成串时合并成一次粘贴。
fn interpret(raw: Vec<(TermEvent, u64)>) -> Vec<InputEvent> {
    let mut out = Vec::new();
    let mut keys: Vec<TimedKey> = Vec::new();
    let flush = |keys: &mut Vec<TimedKey>, out: &mut Vec<InputEvent>| {
        if !keys.is_empty() {
            out.extend(coalesce_rapid_keys(keys));
            keys.clear();
        }
    };
    for (event, at_ms) in raw {
        match event {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                keys.push(TimedKey::new(key, at_ms));
            }
            // 支持括号粘贴的终端直接给整段。
            TermEvent::Paste(text) => {
                flush(&mut keys, &mut out);
                out.push(InputEvent::Paste(text));
            }
            _ => flush(&mut keys, &mut out),
        }
    }
    flush(&mut keys, &mut out);
    out
}

/// 终端这一侧：raw mode、视口、回滚区。**panic 时也要复原**。
///
/// 两层保险，因为它们在不同时刻起作用：`Drop` 管正常返回与 `?` 早退，panic hook 管
/// panic——默认 hook 会打印回溯，然后进程带着一个坏掉的终端退出。
///
/// **没有备用屏**。整屏接管换来的是滚轮失灵、选不中、退出之后什么也不剩；这里只占终端
/// 底下那几行，会话正文按行交还给终端自己的回滚区（[`TerminalGuard::commit`]）。
///
/// 视口的位置**由这里自己记账**，不交给 ratatui 的 `Viewport::Inline`：后者每次定位都要
/// 向终端问一次光标位置，而 crossterm 明写着「`event::read` / `event::poll` 正在进行时
/// 这一问会阻塞并超时」——这个界面有一个常驻的读键线程，两者不能共存。所以只有开场那
/// 一问（读键线程还没起来），之后全靠 [`TerminalGuard::view`] 与 [`TerminalGuard::gap`]
/// 这两笔账。
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    /// 视口占住的那一块。
    view: Rect,
    /// 视口正上方有几行是空的——缩高度腾出来的。下一次往回滚区写从那里开始写，于是
    /// 输入框缩回一行不会在历史与它之间留下一道再也填不上的空档。
    gap: u16,
    /// 上一次看到的终端尺寸。尺寸变了就把视口重新贴到底部。
    size: Size,
}

impl TerminalGuard {
    fn enter(app: &App) -> Result<Self, std::io::Error> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            previous(info);
        }));
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnableBracketedPaste)?;
        // kitty 键盘协议是 Shift-Enter / Alt-Enter **能被区分开**的唯一办法：没有它，
        // 终端把这三种回车发成同一个字节。不支持就算了，Ctrl-J 一直在。
        // **推过才弹**：没推过还发一条 pop，在不认这套协议的终端上就是屏幕上凭空多出来
        // 的几个字符。推没推成功只有这里知道，所以记在进程里。
        if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
            && execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .is_ok()
        {
            KEYBOARD_ENHANCED.store(true, Ordering::Relaxed);
        }

        let (width, height) = crossterm::terminal::size()?;
        let size = Size::new(width, height);
        let want = render::viewport_height(app, width, max_viewport(size), 0).max(1);
        // 全程唯一一次问终端"光标在第几行"（见类型上的那段注释）。
        let row = crossterm::cursor::position()?.1;
        let view = reserve(row, want, size)?;

        let terminal = Terminal::with_options(
            CrosstermBackend::new(stdout),
            TerminalOptions {
                viewport: Viewport::Fixed(view),
            },
        )?;
        Ok(TerminalGuard {
            terminal,
            view,
            gap: 0,
            size,
        })
    }

    /// 一帧：先把定型的正文交给回滚区，再按还在动的那一段调整视口高度，最后画。
    ///
    /// 顺序不能反。先交后画，是因为交正文会把视口整块往下推，画在它前面等于画在一个
    /// 马上要挪走的位置上。
    fn render(&mut self, app: &App, emitted: &mut Emitted) -> Result<(), std::io::Error> {
        self.follow_terminal_size()?;
        self.commit(emitted.take(app, self.size.width))?;

        let live = emitted.live(app, self.size.width);
        let want =
            render::viewport_height(app, self.size.width, max_viewport(self.size), live.len())
                .max(1);
        self.set_height(want)?;
        self.draw(app, &live)
    }

    /// 终端被拉大拉小了：把视口重新贴到底部。
    ///
    /// 上面那些历史怎么重排是终端自己的事——它们已经交出去了，这里不该、也没法再动它们。
    fn follow_terminal_size(&mut self) -> Result<(), std::io::Error> {
        let (width, height) = crossterm::terminal::size()?;
        let size = Size::new(width, height);
        if size == self.size {
            return Ok(());
        }
        self.size = size;
        let view_height = self.view.height.clamp(1, height.max(1));
        self.view = Rect::new(0, height.saturating_sub(view_height), width, view_height);
        self.gap = 0;
        self.terminal.resize(self.view)
    }

    /// 把这些行交给终端的回滚区。交出去之后它们就是终端的了——谁也改不动。
    ///
    /// 写法是"从我们这一块的顶上开始，正常地打印"：每行一个换行，打到屏幕底部时终端自己
    /// 往上滚，滚走的那些进它的回滚区。最后再打 `view.height` 个换行，把视口那块位置重新
    /// 腾出来。
    fn commit(&mut self, lines: Vec<Line<'static>>) -> Result<(), std::io::Error> {
        if lines.is_empty() {
            return Ok(());
        }
        let start = self.view.y.saturating_sub(self.gap);
        let width = self.size.width;
        let out = self.terminal.backend_mut();
        queue!(out, MoveTo(0, start), Clear(ClearType::FromCursorDown))?;
        for line in &lines {
            write_line(out, line, width)?;
        }
        // **`height - 1` 个换行，不是 `height` 个**：最后一个换行会把光标推到视口下面
        // 一行，屏幕跟着多滚一次，视口上面于是留下一条再也填不回去的空行。打到视口的
        // 最后一行为止就够了。
        for _ in 1..self.view.height {
            queue!(out, Print("\r\n"))?;
        }
        out.flush()?;

        self.gap = 0;
        let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.view.y = start
            .saturating_add(rows)
            .min(self.size.height.saturating_sub(self.view.height));
        // `Fixed` 的 resize 只换区域、清视口、把后备缓冲清零逼下一帧全画——**不问光标**。
        self.terminal.resize(self.view)
    }

    /// 改视口高度。
    ///
    /// 长高：先用底下剩的地方，再用 [`Self::gap`] 那几行空档，都不够才让屏幕往上滚。
    /// 变矮：视口原地下移，让出来的行记成空档，下一次往回滚区写正文时正好填回去。
    fn set_height(&mut self, want: u16) -> Result<(), std::io::Error> {
        let want = want.clamp(1, self.size.height.max(1));
        if want == self.view.height {
            return Ok(());
        }
        let out = self.terminal.backend_mut();
        if want < self.view.height {
            let shrink = self.view.height - want;
            queue!(
                out,
                MoveTo(0, self.view.y),
                Clear(ClearType::FromCursorDown)
            )?;
            out.flush()?;
            self.gap += shrink;
            self.view.y += shrink;
        } else {
            let grow = want - self.view.height;
            let below = self
                .size
                .height
                .saturating_sub(self.view.y + self.view.height);
            let rest = grow.saturating_sub(below);
            let from_gap = rest.min(self.gap);
            self.gap -= from_gap;
            self.view.y -= from_gap;
            let scroll = rest - from_gap;
            if scroll > 0 {
                queue!(out, MoveTo(0, self.size.height.saturating_sub(1)))?;
                for _ in 0..scroll {
                    queue!(out, Print("\r\n"))?;
                }
                out.flush()?;
                self.view.y = self.view.y.saturating_sub(scroll);
            }
        }
        self.view.height = want;
        self.terminal.resize(self.view)
    }

    /// 画一帧，整帧包在**同步输出**里（DECSET 2026）。
    ///
    /// 支持它的终端会等整帧到齐再上屏：光标、边框、正在打字的那一行不会各自闪一下。不
    /// 支持的终端把这两个序列当无事发生。
    fn draw(&mut self, app: &App, live: &[Line<'static>]) -> Result<(), std::io::Error> {
        queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let drawn = self
            .terminal
            .draw(|frame| render::draw(frame, app, live))
            .map(|_| ());
        queue!(self.terminal.backend_mut(), EndSynchronizedUpdate)?;
        self.terminal.backend_mut().flush()?;
        drawn
    }
}

/// 视口最多占到哪：**总要给上文留一行**，否则人再也看不见刚说过的那句话。
fn max_viewport(size: Size) -> u16 {
    size.height.saturating_sub(1).max(1)
}

/// 从光标所在行往下留出 `height` 行，返回视口占住的那一块。
///
/// 下面放不下就让终端往上滚——滚走的是终端自己的历史，进它的回滚区，一行都没丢。这与
/// ratatui 给 inline 视口算位置的做法是同一套算术，只是这里算完之后**位置归我们记**。
fn reserve(row: u16, height: u16, size: Size) -> Result<Rect, std::io::Error> {
    let height = height.clamp(1, size.height.max(1));
    let mut out = std::io::stdout();
    let after = height - 1;
    for _ in 0..after {
        queue!(out, Print("\r\n"))?;
    }
    out.flush()?;
    let available = size.height.saturating_sub(row).saturating_sub(1);
    let missing = after.saturating_sub(available);
    Ok(Rect::new(
        0,
        row.saturating_sub(missing),
        size.width,
        height,
    ))
}

/// 把一行带样式的正文写进终端。
///
/// 交给回滚区的行不经过 ratatui 的缓冲区（那块只管视口），所以颜色与粗细要在这里自己
/// 发出去。**一行只占一行**：超宽先截断，否则终端自动折行，回滚区上的行数就与这里数的
/// 对不上，视口的位置跟着算错。
fn write_line(out: &mut impl Write, line: &Line<'_>, width: u16) -> Result<(), std::io::Error> {
    let mut used = 0usize;
    for span in &line.spans {
        let room = (width as usize).saturating_sub(used);
        if room == 0 {
            break;
        }
        // 控制字符一个都不许原样发出去：一个裸换行就是一级楼梯，一个回车能把半行吞
        // 掉，而这一段是**直接写给终端的**，没有缓冲区替它把关。
        let clean: String = span
            .content
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        let text = render::markdown::truncate_to_width(&clean, room);
        if text.is_empty() {
            continue;
        }
        used += render::markdown::display_width(&text);
        let style = line.style.patch(span.style);
        queue!(out, SetAttribute(Attribute::Reset))?;
        for (modifier, attribute) in [
            (Modifier::BOLD, Attribute::Bold),
            (Modifier::DIM, Attribute::Dim),
            (Modifier::ITALIC, Attribute::Italic),
            (Modifier::UNDERLINED, Attribute::Underlined),
            (Modifier::REVERSED, Attribute::Reverse),
            (Modifier::CROSSED_OUT, Attribute::CrossedOut),
        ] {
            if style.add_modifier.contains(modifier) {
                queue!(out, SetAttribute(attribute))?;
            }
        }
        queue!(
            out,
            SetForegroundColor(style.fg.unwrap_or(Color::Reset).into_crossterm()),
            SetBackgroundColor(style.bg.unwrap_or(Color::Reset).into_crossterm()),
        )?;
        queue!(out, Print(text))?;
    }
    queue!(
        out,
        SetAttribute(Attribute::Reset),
        ResetColor,
        Print("\r\n")
    )
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // 视口那一块是这个进程借来的，还回去时要擦干净：光标回到我们这一块的顶上、清掉
        // 往下的内容，退出后那一行 `komo resume` 就印在它原来的位置上，不会留下半个输入框。
        let mut stdout = std::io::stdout();
        let _ = execute!(
            stdout,
            MoveTo(0, self.view.y.saturating_sub(self.gap)),
            Clear(ClearType::FromCursorDown),
            Show
        );
        let _ = restore_terminal();
    }
}

/// 进来的时候有没有真的把 kitty 键盘协议推上去——[`restore_terminal`] 据此决定弹不弹。
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

fn restore_terminal() -> Result<(), std::io::Error> {
    let mut stdout = std::io::stdout();
    // 复原路径不许因为其中一步失败就停下，所以每一条都 `let _`。
    if KEYBOARD_ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout, DisableBracketedPaste, Show);
    disable_raw_mode()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    /// **交给回滚区的一行，在终端上就得正好占一行。**
    ///
    /// 这一段不经过 ratatui 的缓冲区——是直接写给终端的字节。行里混进一个换行，屏幕上
    /// 就是一级楼梯；超出宽度不截断，终端自己折行，于是这边数的行数与屏幕上的对不上，
    /// 视口的位置跟着算错。
    #[test]
    fn a_line_handed_to_the_scrollback_occupies_exactly_one_row() {
        use ratatui::style::{Color, Style};
        use ratatui::text::Span;

        let mut out: Vec<u8> = Vec::new();
        let line = Line::from(vec![
            Span::styled("头", Style::default().fg(Color::Green)),
            // 正文里混进来的控制字符，以及一段远超宽度的文字。
            Span::raw("一\n二\r三\t四"),
            Span::raw("很长很长很长很长很长很长很长很长很长很长"),
        ]);
        write_line(&mut out, &line, 20).expect("写得出去");
        let text = String::from_utf8(out).expect("是 UTF-8");

        assert_eq!(
            text.matches('\n').count(),
            1,
            "只有行尾那一个换行：{text:?}"
        );
        assert!(text.ends_with("\r\n"), "{text:?}");
        // 去掉转义序列之后，可见部分不超过 20 列。
        let visible = strip_escapes(text.trim_end_matches("\r\n"));
        assert!(
            crate::tui::markdown::display_width(&visible) <= 20,
            "宽 {}：{visible:?}",
            crate::tui::markdown::display_width(&visible)
        );
    }

    /// 掐掉 CSI 序列，只留会占格子的那些字符。
    fn strip_escapes(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\u{1b}' {
                out.push(ch);
                continue;
            }
            if chars.peek() == Some(&'[') {
                chars.next();
                for ch in chars.by_ref() {
                    if ch.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        }
        out
    }

    #[test]
    fn a_bracketed_paste_arrives_whole() {
        let out = interpret(vec![(TermEvent::Paste("一大段\n文字".into()), 0)]);
        assert_eq!(out, vec![InputEvent::Paste("一大段\n文字".into())]);
    }

    #[test]
    fn a_burst_of_keys_from_a_terminal_without_bracketed_paste_becomes_a_paste() {
        let raw: Vec<(TermEvent, u64)> = "hello world"
            .chars()
            .enumerate()
            .map(|(index, ch)| (TermEvent::Key(key(ch)), index as u64))
            .collect();
        assert_eq!(
            interpret(raw),
            vec![InputEvent::Paste("hello world".into())]
        );
    }

    #[test]
    fn key_releases_are_dropped_before_they_reach_the_state_machine() {
        let mut released = key('a');
        released.kind = KeyEventKind::Release;
        let out = interpret(vec![
            (TermEvent::Key(key('a')), 0),
            (TermEvent::Key(released), 1),
        ]);
        assert_eq!(out, vec![InputEvent::Key(key('a'))]);
    }

    /// 假网关：只答 `/v1/sessions`（建会话）与 `/v1/interventions`（不需要会话的那一类）。
    async fn fake_gateway() -> crate::test_server::FakeGateway {
        crate::test_server::install_crypto_provider();
        crate::test_server::FakeGateway::spawn(|request, _| {
            if request.path.ends_with("/v1/sessions") {
                return crate::test_server::Reply::ok(serde_json::json!({
                    "session": "sess-new",
                    "title": "",
                    "state": "active",
                    "applied_seq": 0,
                    "created_at": "2026-09-20T00:00:00Z",
                    "updated_at": "2026-09-20T00:00:00Z",
                }));
            }
            crate::test_server::Reply::ok(serde_json::json!({ "interventions": [] }))
        })
        .await
    }

    fn creates(gateway: &crate::test_server::FakeGateway) -> usize {
        gateway
            .requests()
            .into_iter()
            .filter(|request| request.path.ends_with("/v1/sessions") && request.method == "POST")
            .count()
    }

    /// **一个字都没发就直接退出的那条路上，一次 `POST /v1/sessions` 都不该有。**
    ///
    /// 会话是第一条消息送出去之前才铸的：`komo` 看一眼就退出，不该在账本和磁盘上留下
    /// 一个空壳。
    #[tokio::test]
    async fn nothing_is_created_until_the_first_message() {
        let gateway = fake_gateway().await;
        let client = gateway.client();
        let mut app = App::new(None, TuiMode::New, "seed");

        // 按 Ctrl-C、敲字、翻历史、问模型清单——没有一条会碰到会话。
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        for ch in "写着玩一会儿".chars() {
            let effects = app.handle_key(key(ch));
            let (opened, effects) = open_session(&client, &mut app, effects).await;
            assert!(opened.is_none());
            spawn_effects(&client, app.session.as_ref(), effects, &events_channel());
        }
        assert_eq!(creates(&gateway), 0, "一个字都没发就不该建会话");
        assert!(app.session.is_none());
    }

    /// 第一条消息：会话在这里铸出来，消息照常发出去，而且**只铸一次**。
    #[tokio::test]
    async fn the_first_message_mints_the_session_once() {
        let gateway = fake_gateway().await;
        let client = gateway.client();
        let mut app = App::new(None, TuiMode::New, "seed");

        let (opened, effects) = submit(&client, &mut app, "把构建目录清一下").await;
        assert_eq!(opened, Some(SessionId::from_raw("sess-new")));
        assert_eq!(creates(&gateway), 1);
        assert!(
            matches!(effects.as_slice(), [Effect::Submit { .. }]),
            "铸完会话那条消息还要发：{effects:?}"
        );

        let (again, _) = submit(&client, &mut app, "再来一条").await;
        assert!(again.is_none(), "第二次提交不再铸会话");
        assert_eq!(creates(&gateway), 1);
    }

    /// 建会话失败时那条消息**不假装送出去了**：`Submit` 被摘掉，失败贴回它自己那条。
    #[tokio::test]
    async fn a_message_is_not_sent_when_the_session_cannot_be_created() {
        crate::test_server::install_crypto_provider();
        let gateway = crate::test_server::FakeGateway::spawn(|_, _| {
            crate::test_server::Reply::error(500, "internal", "建不出来")
        })
        .await;
        let client = gateway.client();
        let mut app = App::new(None, TuiMode::New, "seed");

        let (opened, effects) = submit(&client, &mut app, "这条送不出去").await;
        assert!(opened.is_none());
        assert!(effects.is_empty(), "没铸出会话就不发提交：{effects:?}");
        assert!(
            app.pending_submissions[0].state != crate::tui::app::SubmissionState::Sending,
            "那条本地消息要有结论，不能永远停在发送中"
        );
    }

    /// 走一条真实的提交：按键 → 状态机 → 铸会话（需要的话）→ 效果。
    async fn submit(
        client: &KomoClient,
        app: &mut App,
        text: &str,
    ) -> (Option<SessionId>, Vec<Effect>) {
        app.input.set(text);
        let effects = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        open_session(client, app, effects).await
    }

    fn events_channel() -> mpsc::Sender<ServerEvent> {
        mpsc::channel(1).0
    }
}
