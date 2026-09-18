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
    ApprovalListQuery, BoundaryRequest, CancelRunRequest, EventQuery, ResumeRequest,
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
use crate::sse::{self, SseMessage};
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
/// `session` 已经存在——`komo` 那条命令先 `POST /v1/sessions`，`komo resume` 直接给
/// 会话 ID。哪一种由 `mode` 说，它也决定要不要**先补读整段历史再进入交互**（§8.8）。
pub async fn run_tui(
    client: KomoClient,
    session: SessionId,
    mode: TuiMode,
) -> Result<(), TuiError> {
    let seed = uuid_v7_at(time::OffsetDateTime::now_utc()).to_string();
    let mut app = App::new(session.clone(), mode, seed);

    let (events_tx, mut events) = mpsc::channel::<ServerEvent>(256);

    // §8.8：resume 打开时先按游标补读全部历史，再进入交互。
    if mode.needs_backfill() {
        backfill(&client, &session, &events_tx).await;
    }
    let _ = events_tx.send(ServerEvent::HistoryDone).await;

    if mode == TuiMode::Resume {
        // 恢复调度由 Gateway 自动进行；这一下只是问「现在到哪了」（§3 命令表）。
        match client.resume(&session, &ResumeRequest::default()).await {
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
        spawn_effects(&client, &session, effects, &events_tx);
    }

    let mut subscription = sse::subscribe(&client, session.clone(), Cursor::after(app.cursor));
    let mut connection = subscription.state();

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
                        spawn_effects(&client, &session, effects, &events_tx);
                    }
                }
                None => break Ok(()),
            },
            message = subscription.recv() => match message {
                Some(SseMessage::Frame(frame)) => {
                    let effects = app.apply(ServerEvent::Frame(frame));
                    spawn_effects(&client, &session, effects, &events_tx);
                }
                Some(SseMessage::Skipped { id, reason }) => {
                    app.apply(ServerEvent::FrameSkipped { id, reason });
                }
                None => {
                    app.apply(ServerEvent::Notice("事件订阅已结束".into()));
                }
            },
            Ok(()) = connection.changed() => {
                let state = connection.borrow().clone();
                app.apply(ServerEvent::Connection(state));
            }
            Some(event) = events.recv() => {
                let effects = app.apply(event);
                spawn_effects(&client, &session, effects, &events_tx);
            }
            _ = ticker.tick() => {}
        }
    };

    stop.store(true, Ordering::Relaxed);
    drop(reader);
    subscription.abort();
    drop(terminal);
    outcome
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
fn spawn_effects(
    client: &KomoClient,
    session: &SessionId,
    effects: Vec<Effect>,
    events: &mpsc::Sender<ServerEvent>,
) {
    for effect in effects {
        let client = client.clone();
        let session = session.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let reply = run_effect(&client, &session, effect).await;
            if let Some(reply) = reply {
                let _ = events.send(reply).await;
            }
        });
    }
}

async fn run_effect(
    client: &KomoClient,
    session: &SessionId,
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
                    app::status_text(response.status)
                ))),
                Err(error) => Some(ServerEvent::Failed(format!("取消失败：{error}"))),
            }
        }
        Effect::Boundary => match client.boundary(session, &BoundaryRequest::default()).await {
            Ok(_) => None,
            Err(error) => Some(ServerEvent::Failed(format!("/new 失败：{error}"))),
        },
        Effect::FetchApproval(approval) => match client.approval(&approval).await {
            Ok(record) => Some(ServerEvent::Approval(Box::new(record))),
            Err(error) => Some(ServerEvent::Failed(format!("取审批详情失败：{error}"))),
        },
        Effect::Decide {
            approval,
            approved,
            scope,
            request_key,
        } => {
            let request = komo_kernel::protocol::http::ApprovalDecisionRequest {
                approved,
                scope,
                request_key: Some(request_key),
            };
            match client.decide_approval(&approval, &request).await {
                Ok(response) => Some(ServerEvent::ApprovalSettled {
                    approval,
                    approved: response.decision.approved,
                    already_decided: response.already_decided,
                }),
                Err(error) => Some(ServerEvent::Failed(format!("答复失败：{error}"))),
            }
        }
        Effect::FetchPending => match client.approvals(&ApprovalListQuery::default()).await {
            Ok(response) => Some(ServerEvent::Pending(response.approvals)),
            Err(error) => Some(ServerEvent::Failed(format!("取待审批失败：{error}"))),
        },
        Effect::FetchStatus => match client.session(session).await {
            Ok(detail) => Some(ServerEvent::Status(Box::new(detail))),
            Err(error) => Some(ServerEvent::Failed(format!("取状态失败：{error}"))),
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
}
