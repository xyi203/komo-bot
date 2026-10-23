//! ratatui 聊天 TUI（§13.4、§14 阶段 1）。
//!
//! 分工是这个模块存在的形状：[`app`] 是**无终端的状态机**（按键 / 服务端事件 → 状态 +
//! [`Effect`]），[`render`] 只画，本模块是**驱动**——把终端事件喂进状态机，把状态机要的
//! 效果打到 HTTP 上，把服务端的回答喂回去。三者之间只有值，没有互相调用。

pub mod app;
pub mod command;
pub mod markdown;
pub mod model_picker;
pub mod paste;
pub mod relaunch;
pub mod render;
pub mod shell;
pub mod spinner;
pub mod tool_view;
pub mod transcript;
pub mod tree_picker;

#[cfg(test)]
pub mod test_support;

use std::cell::Cell;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyEventKind,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::style::{
    Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, SetTitle, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{execute, queue};
use komo_kernel::protocol::http::{
    BoundaryRequest, CancelRunRequest, CreateSessionRequest, EditSessionContextRequest, EventQuery,
    ForkSessionRequest, InterventionAnswerRequest, InterventionListQuery, LabelSessionRequest,
    NameSessionRequest, NavigateSessionRequest, ResumeRequest, SubmitRunRequest,
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
use crate::sse::{self, ConnectionState, SseMessage, SseUpdate as Stream};
use crate::tui::paste::{InputEvent, TimedKey, coalesce_rapid_keys};
use crate::tui::transcript::Emitted;

/// 补读历史时一页取多少条。
const BACKFILL_PAGE: u32 = 500;
/// 状态行上的耗时与转圈靠它走动。
///
/// 比一帧（[`spinner::FRAME_MS`]）快一点，转圈才不会一卡一卡的。一拍只重画底下那一小块
/// 视口，而且 ratatui 只写真的变了的格子——没在跑的时候这一拍只发同步标记与光标定位那
/// 几十个字节，屏幕上什么都不变。
const TICK: Duration = Duration::from_millis(80);
/// 清单上挂着东西时多久再问一次（[`App::poll_pending`]）：别处答掉的那条最多挂这么久。
const PENDING_POLL: Duration = Duration::from_secs(5);
/// 终端尺寸多久不再变，才算拖完（[`ResizeGate`]）。拖动时尺寸大约每 16ms 变一次。
const RESIZE_SETTLE: Duration = Duration::from_millis(100);

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
        // **这一问答上了就是连上了。** 事件流要有会话才订得了，而裸 `komo` 的第一条消息
        // 之前没有会话——状态行上那句"连接中"于是会一直挂到人发出第一句话，说的却是一件
        // 早就成立的事：网关就在那儿，刚刚还答了话。
        let _ = events_tx
            .send(ServerEvent::Connection(ConnectionState::Connected))
            .await;
        let _ = events_tx.send(ServerEvent::ModelMenu(models.models)).await;
    }

    // 先把补读来的事件吃完，游标才是对的——订阅要从它之后开始。
    while let Ok(event) = events.try_recv() {
        let effects = app.apply(event);
        spawn_effects(&client, app.session.as_ref(), effects, &events_tx);
    }

    if let Some(session) = app.session.as_ref() {
        match client.session_tree(session).await {
            Ok(response) => {
                app.apply(ServerEvent::Tree {
                    response: Box::new(response),
                    generation: 0,
                });
            }
            Err(error) => {
                app.apply(ServerEvent::TreeFailed {
                    error: error.to_string(),
                    generation: 0,
                });
            }
        }
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
    // 标签页标题上次设成了什么：Run 在跑时它随转圈换帧，**变了才发**——空闲时一帧照旧
    // 一个字节不写（[`FrameWriter`]）。
    let mut shown_title = String::new();
    // 终端自己在标签页上转时（OSC 9;4），上次告诉它的是转还是停——同样变了才发。
    let tab_spinner = render::TabSpinner::detect();
    let mut shown_busy = false;
    if let Some(session) = app.session.as_ref() {
        terminal.arm_relaunch(session, mode, &cwd);
    }
    let _ = terminal.commit(render::banner(&app, &cwd));
    let mut emitted = Emitted::new();
    let mut ticker = tokio::time::interval(TICK);
    let mut pending_poll = tokio::time::interval(PENDING_POLL);
    // 卡住一阵之后不补拍：补回来的几次问的是同一件事。
    pending_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let outcome = loop {
        if app.quit {
            break Ok(());
        }
        app.now = Some(time::OffsetDateTime::now_utc());
        let title = render::tab_title(&app, &cwd, tab_spinner);
        if title != shown_title {
            // 只排进这一帧，跟着下面的 `render` 一起写出去。
            let _ = queue!(terminal.terminal.backend_mut(), SetTitle(&title));
            shown_title = title;
        }
        let busy = render::busy(&app);
        if tab_spinner == render::TabSpinner::Native && busy != shown_busy {
            let _ = queue!(
                terminal.terminal.backend_mut(),
                Print(render::tab_progress(busy))
            );
            shown_busy = busy;
        }
        // 终端被拉大拉小：照 pi，整段重画。已经画在屏幕上的视口会被终端按新宽度折行、
        // 挪位，残骸留在视口上面；交出去的正文也是按旧宽度截的。只把视口贴回底部收拾
        // 不了这两样。整段重画是把整段会话再写一遍，拖动时不能每变一次就来一遍
        // （[`ResizeGate`]）。
        let step = match terminal.resize_step(Instant::now()) {
            Ok(step) => step,
            Err(error) => break Err(TuiError::Terminal(error)),
        };
        let drawn = match step {
            ResizeStep::Hold(_) => Ok(()),
            ResizeStep::Repaint => {
                app.repaint = false;
                terminal.repaint(&app, &mut emitted, render::banner(&app, &cwd))
            }
            ResizeStep::Frame if std::mem::take(&mut app.repaint) => {
                terminal.repaint(&app, &mut emitted, render::banner(&app, &cwd))
            }
            ResizeStep::Frame => terminal.render(&app, &mut emitted),
        };
        if let Err(error) = drawn {
            break Err(TuiError::Terminal(error));
        }
        let settle_at = match step {
            ResizeStep::Hold(until) => Some(until),
            _ => None,
        };

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
                Stream::Frame(SseMessage::Retired { id }) => {
                    app.apply(ServerEvent::FrameRetired { id });
                }
                Stream::Connection(state) => {
                    let effects = app.apply(ServerEvent::Connection(state));
                    spawn_effects(&client, app.session.as_ref(), effects, &events_tx);
                }
                Stream::Ended => {
                    // 句柄在 `select!` 之外丢掉：`recv` 关掉之后恒返回 `None`，留着这条
                    // 分支会每一拍都立刻打转。
                    ended = true;
                    app.apply(ServerEvent::Notice("事件订阅已结束".into()));
                }
            },
            Some(event) = events.recv() => {
                let previous = app.session.clone();
                let effects = app.apply(event);
                if app.session != previous {
                    if let Some(old) = subscription.take() { old.abort(); }
                    just_opened = app.session.clone();
                    ended = false;
                }
                spawn_effects(&client, app.session.as_ref(), effects, &events_tx);
            }
            _ = ticker.tick() => {}
            // 拖完了：到点回来补那一次整段重画。
            _ = tokio::time::sleep_until(
                tokio::time::Instant::from_std(settle_at.unwrap_or_else(Instant::now))
            ), if settle_at.is_some() => {}
            _ = pending_poll.tick() => {
                let effects = app.poll_pending();
                spawn_effects(&client, app.session.as_ref(), effects, &events_tx);
            }
        }

        // 会话刚铸出来：订阅在这里建立，`select!` 的分支体里换它借不过编译器——那一
        // 路正借着 `subscription`。
        if let Some(session) = just_opened.take() {
            terminal.arm_relaunch(&session, app.mode, &cwd);
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
    // 退出时 Run 可能还在跑：标题不许冻在某一帧星上。去掉星，跟着 `drop` 里最后那次
    // 冲刷写出去。
    let _ = queue!(
        terminal.terminal.backend_mut(),
        SetTitle(render::title(&cwd))
    );
    // 终端那边的转圈同理：不清掉，komo 退出之后标签页还在转。
    if shown_busy {
        let _ = queue!(
            terminal.terminal.backend_mut(),
            Print(render::tab_progress(false))
        );
    }
    drop(terminal);
    // 人自己退出的才撤回重启声明；读键那一路断了（终端没了）不算，留着声明等终端拉回来。
    if app.quit {
        print!("{}", relaunch::CLEAR);
    }
    // 退出即暂停：Run 留在 Gateway 里照跑，人回来靠的是**这一行命令**。会话 Id 只在身份
    // 行上闪过一次，退出这一刻正是需要它的时刻。会话还没铸出来（`komo` 一个字没发就退）
    // 就没有可 resume 的东西，什么都不印——那正是"不在账本上留空壳"的同一条规矩。
    if let Some(session) = app.session.as_ref() {
        println!("komo resume {session}");
    }
    outcome
}

/// 等订阅上的下一件事。
///
/// **还没有会话时永不就绪**：`select!` 的分支数由代码写死，没有订阅不等于少一条分支。
async fn next_stream(subscription: &mut Option<sse::SseHandle>) -> Stream {
    let Some(handle) = subscription.as_mut() else {
        return std::future::pending().await;
    };
    handle.next().await
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
    // 在哪个目录敲的 `komo`，这个会话就在哪个目录干活；取不到才落回 Gateway 的
    // `workspaces_dir`。
    let request = CreateSessionRequest {
        workdir: std::env::current_dir()
            .ok()
            .map(|dir| dir.display().to_string()),
        ..Default::default()
    };
    match client.create_session(&request).await {
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
            shell,
            interject,
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
                shell,
                interject,
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
        Effect::Answer {
            handle,
            verdict,
            request_key,
        } => {
            let request = InterventionAnswerRequest {
                verdict,
                request_key: Some(request_key),
            };
            match client.answer_intervention(&handle, &request).await {
                Ok(response) => Some(ServerEvent::Answered(Box::new(response))),
                Err(error) => Some(ServerEvent::Failed(format!("答复失败：{error}"))),
            }
        }
        Effect::FetchPending => match client
            .interventions(&InterventionListQuery::default())
            .await
        {
            Ok(response) => Some(ServerEvent::Pending(response.interventions)),
            Err(error) => Some(ServerEvent::Failed(format!("取待处理清单失败：{error}"))),
        },
        Effect::RefreshPending => match client
            .interventions(&InterventionListQuery::default())
            .await
        {
            Ok(response) => Some(ServerEvent::PendingRefreshed(response.interventions)),
            // 每 5 秒一条同样的失败是刷屏；下一拍还会再问，`/pending` 问的那次照样会说。
            Err(error) => {
                tracing::debug!(%error, "悄悄再取待处理清单失败");
                None
            }
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
        Effect::FetchTree { generation } => {
            let result = match session {
                Some(session) => client.session_tree(session).await,
                None => return None,
            };
            Some(match result {
                Ok(response) => ServerEvent::Tree {
                    response: Box::new(response),
                    generation,
                },
                Err(error) => ServerEvent::TreeFailed {
                    error: error.to_string(),
                    generation,
                },
            })
        }
        Effect::Navigate { target, summarize } => {
            let session = session?;
            Some(
                match client
                    .navigate_session(
                        session,
                        &NavigateSessionRequest {
                            target_id: target,
                            summarize,
                            instructions: None,
                            label: None,
                        },
                    )
                    .await
                {
                    Ok(response) => ServerEvent::TreeChanged(Box::new(response)),
                    Err(error) => ServerEvent::TreeMutationFailed(format!("导航失败：{error}")),
                },
            )
        }
        Effect::Fork { target, clone } => {
            let session = session?;
            let editor_text = if !clone {
                if let Some(target) = &target {
                    match client.session_tree(session).await {
                        Ok(tree) => tree.entries.iter().find(|e| &e.id == target).and_then(|e| {
                            match &e.kind {
                                komo_kernel::session::EntryKind::Message { message }
                                    if message["role"] == "user" =>
                                {
                                    Some(tree_picker::content_text(&message["content"]))
                                }
                                _ => None,
                            }
                        }),
                        Err(error) => {
                            return Some(ServerEvent::TreeMutationFailed(format!(
                                "读取 fork 输入失败：{error}"
                            )));
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            };
            Some(
                match client
                    .fork_session(
                        session,
                        &ForkSessionRequest {
                            target_id: target,
                            clone,
                        },
                    )
                    .await
                {
                    Ok(response) => ServerEvent::Forked {
                        session: response.session,
                        editor_text,
                    },
                    Err(error) => {
                        ServerEvent::TreeMutationFailed(format!("fork/clone 失败：{error}"))
                    }
                },
            )
        }
        Effect::Compact { instructions } => {
            let session = session?;
            Some(match client.compact_session(session, instructions).await {
                Ok(response) => ServerEvent::TreeChanged(Box::new(response)),
                Err(error) => ServerEvent::TreeMutationFailed(format!("压缩失败：{error}")),
            })
        }
        effect @ (Effect::ContextEdit { .. } | Effect::Label { .. } | Effect::Name(_)) => {
            let session = session?;
            let result = match effect {
                Effect::ContextEdit {
                    target,
                    replacement,
                } => {
                    client
                        .edit_session_context(
                            session,
                            &EditSessionContextRequest {
                                target_id: target,
                                replacement,
                            },
                        )
                        .await
                }
                Effect::Label { target, label } => {
                    client
                        .label_session(
                            session,
                            &LabelSessionRequest {
                                target_id: target,
                                label,
                            },
                        )
                        .await
                }
                Effect::Name(name) => {
                    client
                        .name_session(session, &NameSessionRequest { name })
                        .await
                }
                _ => unreachable!(),
            };
            Some(match result {
                Ok(response) if response.cancelled => ServerEvent::TreeChanged(Box::new(response)),
                Ok(_) => ServerEvent::TreeUpdated,
                Err(error) => ServerEvent::TreeMutationFailed(format!("更新会话失败：{error}")),
            })
        }
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
            // 尺寸变了不是输入，但要叫醒主循环：空的一批也送过去，它醒来自己去量尺寸，不必
            // 等下一拍。
            let resized = raw
                .iter()
                .any(|(event, _)| matches!(event, TermEvent::Resize(..)));
            let batch = interpret(raw);
            if (!batch.is_empty() || resized) && tx.blocking_send(batch).is_err() {
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
    terminal: Terminal<CrosstermBackend<FrameWriter>>,
    /// 视口占住的那一块。
    view: Rect,
    /// 视口正上方有几行是空的——缩高度腾出来的。下一次往回滚区写从那里开始写，于是
    /// 输入框缩回一行不会在历史与它之间留下一道再也填不上的空档。
    gap: u16,
    /// 上一次看到的终端尺寸。尺寸变了就把视口重新贴到底部。
    size: Size,
    /// 与 [`FrameWriter`] 共用的那个开关：只有 [`TerminalGuard::present`] 的冲刷才真写。
    present: Rc<Cell<bool>>,
    /// 尺寸变了什么时候整段重画。
    resize: ResizeGate,
}

/// 这一拍按尺寸该怎么画。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeStep {
    /// 照常一帧。
    Frame,
    /// 整段重画。
    Repaint,
    /// 还在拖：这一拍什么都不画，到这个时刻再看。
    Hold(Instant),
}

/// 尺寸变了的整段重画，拖动时只来两次：变的第一下立刻画（只拉一下的，比如最大化，不必
/// 等），之后尺寸还在变就先不画，停下 [`RESIZE_SETTLE`] 再按最后的尺寸补画一次。
///
/// 起因（2026-10-19）：伪终端里 0.34 秒内改 20 次尺寸，原来每次都整段重画，19 次共写出
/// 4.7 MB——komo 自己画得不慢，是终端要在拖动途中把这么多字节解析、折行一遍。
#[derive(Debug)]
struct ResizeGate {
    /// 上一拍量到的尺寸：拿来分辨"又变了"与"停在那儿"。
    seen: Size,
    /// 上一次整段重画之后的静默期截止时刻。
    quiet_until: Option<Instant>,
    /// 静默期里尺寸又变过：到点必须补画，哪怕最后拉回了原来的尺寸——中间那几次折行的
    /// 残骸还在屏幕上。
    held: bool,
}

impl ResizeGate {
    fn new(size: Size) -> Self {
        ResizeGate {
            seen: size,
            quiet_until: None,
            held: false,
        }
    }

    /// `painted` 是上一次画的时候的尺寸，`current` 是终端现在的尺寸。
    fn step(&mut self, painted: Size, current: Size, now: Instant) -> ResizeStep {
        let moved = current != self.seen;
        self.seen = current;
        match self.quiet_until {
            Some(until) if now < until => {
                if moved {
                    self.held = true;
                    let until = now + RESIZE_SETTLE;
                    self.quiet_until = Some(until);
                    ResizeStep::Hold(until)
                } else if self.held {
                    ResizeStep::Hold(until)
                } else {
                    ResizeStep::Frame
                }
            }
            _ if self.held || current != painted => {
                self.held = false;
                self.quiet_until = Some(now + RESIZE_SETTLE);
                ResizeStep::Repaint
            }
            _ => {
                self.quiet_until = None;
                ResizeStep::Frame
            }
        }
    }
}

impl TerminalGuard {
    /// 告诉终端重启后怎么把这个会话拉回来（[`relaunch`]）。与终端标题一样只排进这一帧。
    fn arm_relaunch(&mut self, session: &SessionId, mode: TuiMode, cwd: &str) {
        if let Some(sequence) = relaunch::arm(session, mode, cwd) {
            let _ = queue!(self.terminal.backend_mut(), Print(sequence));
        }
    }

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
        let (view, gap) = reserve(row, want, size)?;

        let present = Rc::new(Cell::new(false));
        let terminal = Terminal::with_options(
            CrosstermBackend::new(FrameWriter::new(&stdout, present.clone())?),
            TerminalOptions {
                viewport: Viewport::Fixed(view),
            },
        )?;
        Ok(TerminalGuard {
            terminal,
            view,
            gap,
            size,
            present,
            resize: ResizeGate::new(size),
        })
    }

    /// 一帧：先把定型的正文交给回滚区，再按还在动的那一段调整视口高度，最后画。
    ///
    /// 顺序不能反。先交后画，是因为交正文会把视口整块往下推，画在它前面等于画在一个
    /// 马上要挪走的位置上。
    ///
    /// **这三步是同一帧**：整段包在同步输出里（DECSET 2026），字节攒在 [`FrameWriter`]
    /// 里到最后一次写给终端。交正文与改高度都会先擦掉视口再整块重画——擦与画要是分几次
    /// 写出去，本机上快得看不见，隔着 SSH 就是每交一行正文输入框闪一下空白。支持同步
    /// 输出的终端等整帧到齐再上屏；不支持的终端也只收到一次写，擦和画落在同一次读里。
    fn render(&mut self, app: &App, emitted: &mut Emitted) -> Result<(), std::io::Error> {
        self.synchronized(|this| this.frame(app, emitted))
    }

    /// 整段重画（`Ctrl-T`，以及终端尺寸变了）：擦掉屏幕与终端的回滚区，从开场横幅起把
    /// 整段会话按现在的样子、现在的宽度重新交一遍。
    ///
    /// 交出去的行终端不让改，要改只能整段收回——pi 的 Ctrl-O 与 resize 也是这么做的。代价
    /// 是进 TUI 之前终端里的那些也一起擦掉了。与平常的一帧一样包在同步输出里，擦与重画
    /// 一次上屏。
    fn repaint(
        &mut self,
        app: &App,
        emitted: &mut Emitted,
        banner: Vec<Line<'static>>,
    ) -> Result<(), std::io::Error> {
        self.synchronized(|this| {
            // 先认下新尺寸：横幅与正文要按新宽度交。
            this.follow_terminal_size()?;
            wipe(this.terminal.backend_mut())?;
            // 视口还贴着底边，它上面整块都空了：下一次往回滚区写从第一行开始。
            this.gap = this.view.y;
            *emitted = Emitted::new();
            this.commit(banner)?;
            this.frame(app, emitted)
        })
    }

    /// 一帧包在一对同步输出标记里，攒完一次写给终端。
    fn synchronized(
        &mut self,
        draw: impl FnOnce(&mut Self) -> Result<(), std::io::Error>,
    ) -> Result<(), std::io::Error> {
        // 画的时候光标要在屏幕上到处跳（交正文、擦视口、逐格画），先藏起来；画完 ratatui
        // 会把它显示回输入框里。不支持同步输出的终端上，这就是"光标一闪一闪"的那一跳。
        queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate, Hide)?;
        let drawn = draw(self);
        // 出错也要把同步标记收回去：不收，支持它的终端会冻在这一帧上直到它自己超时。
        let ended = queue!(self.terminal.backend_mut(), EndSynchronizedUpdate);
        let presented = self.present();
        drawn.and(ended).and(presented)
    }

    /// 把攒下的一帧一次写给终端（见 [`FrameWriter`]）。
    fn present(&mut self) -> Result<(), std::io::Error> {
        self.present.set(true);
        let written = self.terminal.backend_mut().flush();
        self.present.set(false);
        written
    }

    fn frame(&mut self, app: &App, emitted: &mut Emitted) -> Result<(), std::io::Error> {
        // 尺寸不在这里认：认了，这一拍屏幕上的残骸没擦，下一拍 [`Self::resized`] 也就看不出
        // 变过。画到一半终端被拉了，下一拍整段重画。
        self.commit(emitted.take(app, self.size.width))?;

        let live = emitted.live(app, self.size.width);
        let want =
            render::viewport_height(app, self.size.width, max_viewport(self.size), live.len())
                .max(1);
        self.set_height(want)?;
        self.terminal
            .draw(|frame| render::draw(frame, app, &live))
            .map(|_| ())
    }

    /// 量一次终端尺寸，问 [`ResizeGate`] 这一拍怎么画。
    fn resize_step(&mut self, now: Instant) -> Result<ResizeStep, std::io::Error> {
        let (width, height) = crossterm::terminal::size()?;
        Ok(self.resize.step(self.size, Size::new(width, height), now))
    }

    /// 认下终端的新尺寸：把视口重新贴到底部。
    ///
    /// 只管视口这一块的账，只从 [`Self::repaint`] 进来——屏幕上已有的东西由它擦掉重交。
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

    /// 把这些行交给终端的回滚区。交出去之后它们就是终端的了——谁也改不动，要改只能整段
    /// 擦掉重来（[`Self::repaint`]）。
    ///
    /// 只排进 [`FrameWriter`]，跟着下一帧一起写出去。
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
        // 正文之后补到底边为止，视口于是还在它该在的地方（贴底）。正文短得够不着底时，
        // 中间空出来的那几行记成空档——**不是丢掉**：下一段正文从空档的顶上开始写，历史
        // 与输入框之间不会留一道越来越宽的缝。
        //
        // 只打到视口的**最后一行**为止（`height - 1` 个换行，不是 `height` 个）：多打
        // 一个，光标会被推到视口下面一行，屏幕跟着多滚一次。
        let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let top = self.size.height.saturating_sub(self.view.height);
        let gap = top.saturating_sub(start.saturating_add(rows));
        for _ in 1..self.view.height.saturating_add(gap) {
            queue!(out, Print("\r\n"))?;
        }

        self.gap = gap;
        self.view.y = top;
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
            self.gap += shrink;
            self.view.y += shrink;
        } else {
            // 视口贴着底边（[`reserve`] 与 [`Self::commit`] 都维持这一条），所以底下
            // 没有空地可用：长高只能往上要——先要空档，空档不够才让屏幕往上滚。
            let grow = want - self.view.height;
            let from_gap = grow.min(self.gap);
            self.gap -= from_gap;
            self.view.y -= from_gap;
            let scroll = grow - from_gap;
            if scroll > 0 {
                queue!(out, MoveTo(0, self.size.height.saturating_sub(1)))?;
                for _ in 0..scroll {
                    queue!(out, Print("\r\n"))?;
                }
                self.view.y = self.view.y.saturating_sub(scroll);
            }
        }
        self.view.height = want;
        self.terminal.resize(self.view)
    }
}

/// 擦掉整屏与终端的回滚区，光标回到左上角。
///
/// 顺序照 pi：先擦屏、再擦回滚区——有的终端擦屏时把屏上那一页推进回滚区，反过来擦就
/// 留下这一页。
fn wipe(out: &mut impl Write) -> Result<(), std::io::Error> {
    queue!(
        out,
        Clear(ClearType::All),
        MoveTo(0, 0),
        Clear(ClearType::Purge)
    )
}

/// 视口最多占到哪：**总要给上文留一行**，否则人再也看不见刚说过的那句话。
fn max_viewport(size: Size) -> u16 {
    size.height.saturating_sub(1).max(1)
}

/// 从光标所在行往下留出 `height` 行，返回视口占住的那一块。
///
/// 下面放不下就让终端往上滚——滚走的是终端自己的历史，进它的回滚区，一行都没丢。这与
/// ratatui 给 inline 视口算位置的做法是同一套算术，只是这里算完之后**位置归我们记**。
fn reserve(row: u16, height: u16, size: Size) -> Result<(Rect, u16), std::io::Error> {
    let height = height.clamp(1, size.height.max(1));
    let top = size.height - height;
    let mut out = std::io::stdout();
    let gap = if row > top {
        // 光标已经在视口该待的地方以下了：让终端往上滚，滚走的进它的回滚区，一行不丢。
        queue!(out, MoveTo(0, size.height.saturating_sub(1)))?;
        for _ in 0..(row - top) {
            queue!(out, Print("\r\n"))?;
        }
        out.flush()?;
        0
    } else {
        // 上面的内容够不着底：中间那几行先空着，记成空档——下一段正文落地时正好填回去。
        top - row
    };
    Ok((Rect::new(0, top, size.width, height), gap))
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
        // 还没写出去的那一帧先写完，下面直接写 stdout 的几条才排在它后面。
        let _ = self.present();
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

/// 视口这一侧的字节先攒着，[`FrameWriter::present`] 时一次写给终端。
///
/// 不能直接用 `Stdout`：它是按行缓冲的，交给回滚区的每一行一个换行就是一次写，一帧被
/// 切成几十段；`BufWriter` 套在它外面也没用，冲刷时照样在最后一个换行处断成两次。这里
/// 写的是 stdout 的一个 dup，不经过那层行缓冲。
///
/// 平常的 `flush` 什么都不做：ratatui 画完一帧就会冲刷，那时同步输出的结束标记还没排
/// 进来。只有 `present` 开关打开时的那次冲刷才真写——开关由 [`TerminalGuard::present`]
/// 拨（ratatui 不给稳定的办法从后端里取回 writer，所以靠这个共用的开关）。
///
/// **与上一帧一字不差的一帧不写。** 主循环每一拍都画，空闲时 ratatui 的差分什么格子都不
/// 出，可它每帧照样发一次"显示光标、移到输入框"——再加上帧头藏光标，终端每 80ms 被
/// 拨一次光标，闪烁的相位跟着重来，看上去就是光标在乱闪；隔着 SSH 还是白白的流量。
/// 屏幕上没有变化，这一帧的字节就与上一帧相同，跳过它终端停在的正是同一个状态。
struct FrameWriter {
    pending: Vec<u8>,
    /// 上一次真写出去的那一帧。
    last: Vec<u8>,
    out: File,
    present: Rc<Cell<bool>>,
}

impl FrameWriter {
    fn new(stdout: &std::io::Stdout, present: Rc<Cell<bool>>) -> Result<Self, std::io::Error> {
        Ok(FrameWriter {
            pending: Vec::with_capacity(16 * 1024),
            last: Vec::new(),
            out: File::from(stdout.as_fd().try_clone_to_owned()?),
            present,
        })
    }
}

impl Write for FrameWriter {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        self.pending.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        if !self.present.get() || self.pending.is_empty() {
            return Ok(());
        }
        if self.pending == self.last {
            self.pending.clear();
            return Ok(());
        }
        let written = self.out.write_all(&self.pending);
        std::mem::swap(&mut self.pending, &mut self.last);
        self.pending.clear();
        written
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

    #[tokio::test]
    async fn fork_effect_opens_independent_session_and_restores_selected_input() {
        use crate::test_server::{FakeGateway, Reply};
        use serde_json::json;
        let server = FakeGateway::spawn(|request, _| {
            if request.path.ends_with("/fork") { return Reply::ok(json!({"session":"forked"})); }
            let entries = vec![
                json!({"id":"root","parentId":null,"timestamp":"now","type":"message","message":{"role":"assistant","content":"shared ancestor"}}),
                json!({"id":"selected","parentId":"root","timestamp":"now","type":"message","message":{"role":"user","content":[{"type":"text","text":"edit this question"}]}}),
            ];
            Reply::ok(json!({"entries":entries,"leafId":"root"}))
        }).await;
        let client = server.client();
        let source = SessionId::from_raw("source");
        let mut app = App::new(Some(source.clone()), TuiMode::New, "fork-effect");
        app.cursor = komo_kernel::types::ids::Seq(99);
        let reply = run_effect(
            &client,
            Some(&source),
            Effect::Fork {
                target: Some("selected".into()),
                clone: false,
            },
        )
        .await
        .unwrap();
        let effects = app.apply(reply);
        assert_eq!(app.session.as_ref().unwrap().as_str(), "forked");
        assert_eq!(app.cursor, komo_kernel::types::ids::Seq::ZERO);
        assert_eq!(app.input.text(), "edit this question");
        let tree = run_effect(
            &client,
            app.session.as_ref(),
            effects.into_iter().next().unwrap(),
        )
        .await
        .unwrap();
        app.apply(tree);
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].text.as_deref(), Some("shared ancestor"));
        assert!(app.input_enabled());
        assert!(app.repaint);
        let requests = server.requests();
        assert_eq!(
            requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            [
                "/v1/sessions/source/tree",
                "/v1/sessions/source/fork",
                "/v1/sessions/forked/tree"
            ]
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&requests[1].body).unwrap(),
            json!({"targetId":"selected","clone":false})
        );
    }

    /// 同一次连接状态只消费一次；时钟取消等待后，再等仍须挂起。
    #[tokio::test]
    async fn idle_sse_state_is_consumed_once() {
        use crate::test_server::{FakeGateway, Reply};

        let server = FakeGateway::spawn(|_, _| Reply::SseOpen(vec![])).await;
        let mut subscription = Some(sse::subscribe(
            &server.client(),
            SessionId::from_raw("sess-idle"),
            Cursor::default(),
        ));
        let connected =
            tokio::time::timeout(Duration::from_secs(2), next_stream(&mut subscription))
                .await
                .expect("能收到连接状态");
        assert!(matches!(
            connected,
            Stream::Connection(ConnectionState::Connected)
        ));

        for _ in 0..2 {
            assert!(
                tokio::time::timeout(Duration::from_millis(25), next_stream(&mut subscription),)
                    .await
                    .is_err(),
                "没有新事件时必须等待，不能重复消费已连接状态并重绘"
            );
        }
        subscription.take().expect("订阅还在").abort();
    }

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    fn ms(at: Instant, millis: u64) -> Instant {
        at + Duration::from_millis(millis)
    }

    /// 只拉一下（比如最大化）：当场整段重画，不等静默期。
    #[test]
    fn a_single_resize_repaints_at_once() {
        let t0 = Instant::now();
        let (a, b) = (Size::new(140, 40), Size::new(100, 40));
        let mut gate = ResizeGate::new(a);
        assert_eq!(gate.step(a, a, t0), ResizeStep::Frame);
        assert_eq!(gate.step(a, b, ms(t0, 10)), ResizeStep::Repaint);
        assert_eq!(
            gate.step(b, b, ms(t0, 20)),
            ResizeStep::Frame,
            "画过了就照常一帧"
        );
        assert_eq!(gate.step(b, b, ms(t0, 500)), ResizeStep::Frame);
    }

    /// 拖动：第一下画，拖动途中一次都不画，停下静默期之后按最后的尺寸再画一次。
    #[test]
    fn dragging_repaints_once_at_the_start_and_once_after_it_settles() {
        let t0 = Instant::now();
        let start = Size::new(140, 40);
        let mut gate = ResizeGate::new(start);
        let mut painted = start;
        let mut repaints = 0;
        let mut last = t0;
        for i in 1..=20u16 {
            last = ms(t0, 16 * u64::from(i));
            let current = Size::new(140 - 2 * i, 40);
            match gate.step(painted, current, last) {
                ResizeStep::Repaint => {
                    repaints += 1;
                    painted = current;
                }
                ResizeStep::Hold(until) => assert_eq!(until, last + RESIZE_SETTLE),
                ResizeStep::Frame => panic!("拖动途中尺寸一直在变，不该照常画"),
            }
        }
        assert_eq!(repaints, 1, "拖动途中只有第一下画了");

        let end = Size::new(100, 40);
        assert!(matches!(
            gate.step(painted, end, ms(last, 50)),
            ResizeStep::Hold(_)
        ));
        assert_eq!(
            gate.step(painted, end, last + RESIZE_SETTLE),
            ResizeStep::Repaint
        );
        assert_eq!(gate.step(end, end, ms(last, 120)), ResizeStep::Frame);
    }

    /// 拖了一圈又回到原来的尺寸：中间折行的残骸还在，照样补画。
    #[test]
    fn dragging_back_to_the_painted_size_still_repaints() {
        let t0 = Instant::now();
        let (a, b) = (Size::new(140, 40), Size::new(120, 40));
        let mut gate = ResizeGate::new(a);
        assert_eq!(gate.step(a, b, t0), ResizeStep::Repaint);
        assert!(matches!(gate.step(b, a, ms(t0, 16)), ResizeStep::Hold(_)));
        assert!(matches!(gate.step(b, b, ms(t0, 32)), ResizeStep::Hold(_)));
        assert_eq!(
            gate.step(b, b, ms(t0, 32) + RESIZE_SETTLE),
            ResizeStep::Repaint
        );
    }

    /// **一帧只写一次。** ratatui 自己的冲刷、正文行里的换行都不许把半帧送出去——隔着
    /// SSH，擦掉视口与重画它分成两次到达，就是一次闪烁。
    #[test]
    fn a_frame_reaches_the_terminal_in_one_write_only_when_presented() {
        let file = tempfile::NamedTempFile::new().expect("能建临时文件");
        let present = Rc::new(Cell::new(false));
        let mut writer = FrameWriter {
            pending: Vec::new(),
            last: Vec::new(),
            out: file.reopen().expect("能打开"),
            present: present.clone(),
        };
        let written = || std::fs::read(file.path()).expect("能读");

        writer.write_all(b"\x1b[J line one\r\n").expect("能写");
        writer.flush().expect("能冲刷");
        writer.write_all(b"redraw").expect("能写");
        writer.flush().expect("能冲刷");
        assert!(written().is_empty(), "开关没开，冲刷不该写出任何东西");

        present.set(true);
        writer.flush().expect("能冲刷");
        assert_eq!(written(), b"\x1b[J line one\r\nredraw");
        assert!(writer.pending.is_empty());
    }

    /// **屏幕没变的一帧一个字节都不写。** 空闲时每一拍都画，每一拍都拨一次光标，光标就
    /// 一直在闪。
    #[test]
    fn an_unchanged_frame_is_not_written_again() {
        let file = tempfile::NamedTempFile::new().expect("能建临时文件");
        let present = Rc::new(Cell::new(true));
        let mut writer = FrameWriter {
            pending: Vec::new(),
            last: Vec::new(),
            out: file.reopen().expect("能打开"),
            present,
        };
        let written = || std::fs::read(file.path()).expect("能读");

        for frame in [
            &b"\x1b[?25l a \x1b[?25h"[..],
            b"\x1b[?25l a \x1b[?25h",
            b"b",
            b"b",
            b"a",
        ] {
            writer.write_all(frame).expect("能写");
            writer.flush().expect("能冲刷");
        }
        assert_eq!(written(), b"\x1b[?25l a \x1b[?25hba");
        assert!(writer.pending.is_empty());
    }

    /// `Ctrl-T` 的整段重画先擦屏、再擦回滚区：反过来，有的终端擦屏时推进回滚区的那一页
    /// 就留下了。
    #[test]
    fn a_repaint_wipes_the_screen_before_the_scrollback() {
        let mut out: Vec<u8> = Vec::new();
        wipe(&mut out).expect("写得出去");
        assert_eq!(out, b"\x1b[2J\x1b[1;1H\x1b[3J");
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
