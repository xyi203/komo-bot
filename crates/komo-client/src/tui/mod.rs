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

#[cfg(test)]
pub mod test_support;

use std::io::Stdout;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyEventKind,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use komo_kernel::protocol::http::{
    BoundaryRequest, CancelRunRequest, CreateSessionRequest, EventQuery, InterventionAnswerRequest,
    InterventionBatchAnswerRequest, InterventionDetail, InterventionListQuery, ResumeRequest,
    SubmitRunRequest,
};
use komo_kernel::protocol::sse::Cursor;
use komo_kernel::types::ids::{SessionId, uuid_v7_at};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

pub use app::{App, Effect, Phase, ServerEvent, TuiMode};

use crate::api::KomoClient;
use crate::error::ClientError;
use crate::sse::{self, ConnectionState, SseMessage};
use crate::tui::paste::{InputEvent, TimedKey, coalesce_rapid_keys};

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

    let stop = Arc::new(AtomicBool::new(false));
    let (mut input, reader) = spawn_input_reader(stop.clone());

    let mut terminal = TerminalGuard::enter()?;
    let mut ticker = tokio::time::interval(TICK);

    let outcome = loop {
        if app.quit {
            break Ok(());
        }
        app.now = Some(time::OffsetDateTime::now_utc());
        if let Err(error) = terminal.draw(&app) {
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
        println!("\nkomo resume {session}");
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

/// 终端的 raw mode 与备用屏：**panic 时也要复原**。
///
/// 两层保险，因为它们在不同时刻起作用：`Drop` 管正常返回与 `?` 早退，panic hook 管
/// panic——默认 hook 会在备用屏里打印回溯，然后进程带着一个坏掉的终端退出。
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self, std::io::Error> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            previous(info);
        }));
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
        // kitty 键盘协议是 Shift-Enter / Alt-Enter **能被区分开**的唯一办法：没有它，
        // 终端把这三种回车发成同一个字节。不支持就算了，Ctrl-J 一直在。
        if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
            let _ = execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            );
        }
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(TerminalGuard { terminal })
    }

    fn draw(&mut self, app: &App) -> Result<(), std::io::Error> {
        self.terminal.draw(|frame| render::draw(frame, app))?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

fn restore_terminal() -> Result<(), std::io::Error> {
    let mut stdout = std::io::stdout();
    // Pop 在没 Push 过时是无害的；两边都用 `let _` 因为复原路径不许因此失败。
    let _ = execute!(
        stdout,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    disable_raw_mode()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
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
