//! `App` 状态机的测试：按键序列 / 服务端事件 → 状态断言，不需要终端。

use super::*;
use crate::sse::ConnectionState;
use crate::tui::paste::{Input, InputEvent, PASTE_MIN_BYTES, PasteChip};
use crate::tui::test_support as fixture;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::http::{
    InterventionKind, InterventionVerdict, ModelMenuEntry, RunSummary, SessionSummary,
};
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::types::ids::{RunId, Seq};
use komo_kernel::types::model::Effort;
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::status::{RetryCause, RunState, SessionState, ToolCallState, WaitReason};

fn app() -> App {
    App::new(Some(fixture::session()), TuiMode::New, "seed")
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
}

fn feed(app: &mut App, events: &[Event]) {
    for event in events {
        app.apply(ServerEvent::Frame(Box::new(SseFrame {
            id: event.seq,
            session: event.session.clone(),
            event: SseEvent::Event(Box::new(event.clone())),
        })));
    }
}

/// 待处理清单按它**真正来的路**进来：`GET /v1/interventions` 的那一份（§7.5）。
///
/// 不是从本会话的事件折出来的那份——待处理可以属于别的会话，操作者在 TUI 里要看到的正是
/// "现在有谁在等我"，所以清单只能问服务端。
///
/// 给的是**句柄**（Run ID），每条都是结果不明。两类混排见 [`pending_mixed`]。
fn pending_list(handles: &[&str]) -> ServerEvent {
    ServerEvent::Pending(
        handles
            .iter()
            .map(|handle| {
                fixture::intervention_summary(
                    handle,
                    InterventionKind::Verify,
                    "上次那个调用发生了没有",
                )
            })
            .collect(),
    )
}

/// 两类各来一条：结果不明、阻塞。
fn pending_mixed() -> ServerEvent {
    ServerEvent::Pending(vec![
        fixture::intervention_summary("run-1", InterventionKind::Verify, "上次那个调用发生了没有"),
        fixture::intervention_summary(
            "run-2",
            InterventionKind::Blocked,
            "会话已删除，内容读不出来",
        ),
    ])
}

fn notices(app: &App) -> String {
    app.notices
        .iter()
        .map(|n| n.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- 输入与粘贴 ----

#[test]
fn enter_sends_and_the_text_goes_out_whole() {
    let mut app = app();
    type_text(&mut app, "你好");
    let effects = app.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        &effects[0],
        Effect::Submit { text, .. } if text == "你好"
    ));
    assert!(app.input.is_empty(), "发出去之后输入框清空");
    assert_eq!(
        app.pending_submissions.len(),
        1,
        "事件回声到达前也要有本地消息"
    );
    assert_eq!(app.pending_submissions[0].text, "你好");
}

#[test]
fn the_authoritative_run_accepted_replaces_the_local_submission() {
    let mut app = app();
    type_text(&mut app, "只显示一次");
    let effects = app.handle_key(key(KeyCode::Enter));
    let Effect::Submit { request_key, .. } = &effects[0] else {
        panic!("{effects:?}")
    };

    let mut accepted = fixture::conversation()[0].clone();
    if let EventPayload::RunAccepted(body) = &mut accepted.payload {
        body.request_key = request_key.clone();
        body.text = Some("只显示一次".into());
    } else {
        panic!("fixture 第一条必须是 run.accepted");
    }
    feed(&mut app, &[accepted]);

    assert!(
        app.pending_submissions.is_empty(),
        "权威事件到达后移除本地副本"
    );
    assert_eq!(
        app.messages()
            .iter()
            .filter_map(|message| message.text.as_deref())
            .collect::<Vec<_>>(),
        vec!["只显示一次"]
    );
}

#[test]
fn ctrl_j_and_shift_enter_are_newlines_not_sends() {
    let mut app = app();
    type_text(&mut app, "第一行");
    assert!(
        app.handle_key(with(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .is_empty()
    );
    type_text(&mut app, "第二行");
    assert!(
        app.handle_key(with(KeyCode::Enter, KeyModifiers::SHIFT))
            .is_empty()
    );
    assert!(
        app.handle_key(with(KeyCode::Enter, KeyModifiers::ALT))
            .is_empty()
    );
    assert_eq!(app.input.text(), "第一行\n第二行\n\n");
}

#[test]
fn a_four_line_paste_folds_but_is_still_sent_whole() {
    let mut app = app();
    let pasted = "一\n二\n三\n四";
    app.handle_input(InputEvent::Paste(pasted.into()));
    assert_eq!(app.input.chips().len(), 1);
    assert_eq!(app.input.display(), "[粘贴 4 行 · 15 B]");

    let effects = app.handle_key(key(KeyCode::Enter));
    let Effect::Submit { text, .. } = &effects[0] else {
        panic!("{effects:?}")
    };
    assert_eq!(text, pasted, "折的是显示，发的是全文");
}

#[test]
fn a_paste_over_ten_kilobytes_folds_even_on_one_line() {
    let mut app = app();
    app.handle_input(InputEvent::Paste("x".repeat(PASTE_MIN_BYTES + 1)));
    let chips: &[PasteChip] = app.input.chips();
    assert_eq!(chips.len(), 1);
    assert_eq!(chips[0].bytes, PASTE_MIN_BYTES + 1);
    assert!(
        app.input.display().contains("10.0 KB"),
        "{}",
        app.input.display()
    );
}

#[test]
fn the_input_history_walks_back_and_restores_the_draft() {
    let mut app = app();
    type_text(&mut app, "第一句");
    app.handle_key(key(KeyCode::Enter));
    type_text(&mut app, "第二句");
    app.handle_key(key(KeyCode::Enter));
    type_text(&mut app, "草稿");

    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.input.text(), "第二句");
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.input.text(), "第一句");
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.input.text(), "第二句");
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.input.text(), "草稿", "走回底部要把草稿还回来");
}

/// 翻历史来回一趟，草稿里折起来的粘贴不能变成摊开的一大段；翻到的那条发过的消息里有
/// 粘贴，也照样折着。
#[test]
fn history_keeps_folded_pastes_folded() {
    let mut app = app();
    app.handle_input(InputEvent::Paste("一\n二\n三\n四".into()));
    app.handle_key(key(KeyCode::Enter));
    type_text(&mut app, "草稿：");
    app.handle_input(InputEvent::Paste("a\nb\nc\nd".into()));

    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.input.display(), "[粘贴 4 行 · 15 B]");
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.input.display(), "草稿：[粘贴 4 行 · 7 B]");
    assert_eq!(app.input.text(), "草稿：a\nb\nc\nd");
}

// ---- Esc 的两个意思 ----

#[test]
fn esc_while_a_run_is_going_cancels_it() {
    let mut app = app();
    feed(&mut app, &fixture::conversation()[..3]);
    assert_eq!(app.run_state(), Some(RunState::Running));

    let effects = app.handle_key(key(KeyCode::Esc));
    assert!(
        matches!(&effects[0], Effect::Cancel { run, .. } if run == &fixture::run()),
        "{effects:?}"
    );
}

#[test]
fn esc_while_idle_does_nothing_and_keeps_the_draft() {
    let mut app = app();
    type_text(&mut app, "半句话");
    let effects = app.handle_key(key(KeyCode::Esc));
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.input.text(), "半句话", "空闲时的 Esc 不动草稿");
}

#[test]
fn esc_after_a_run_finished_does_not_cancel_anything() {
    let mut app = app();
    feed(&mut app, &fixture::conversation());
    assert_eq!(app.run_state(), Some(RunState::Completed));
    assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
}

// ---- 订阅 ----

#[test]
fn a_subscription_failure_shows_its_reason_once_while_retrying() {
    let mut app = app();
    app.apply(ServerEvent::Connection(ConnectionState::Reconnecting {
        attempt: 1,
        reason: "connection reset".into(),
    }));
    app.apply(ServerEvent::Connection(ConnectionState::Reconnecting {
        attempt: 2,
        reason: "connection reset".into(),
    }));

    let errors: Vec<&Notice> = app
        .notices
        .iter()
        .filter(|notice| notice.is_error)
        .collect();
    assert_eq!(errors.len(), 1, "每次退避不重复刷同一条错误");
    assert!(errors[0].text.contains("connection reset"));
}

/// 断过又连上（多半是 Gateway 重启了）：说一句已恢复，并且问一次清单——重启时恢复扫描
/// 停下的 Run 只落在 state.db 里，不问就看不见。
#[test]
fn reconnecting_after_a_drop_asks_for_the_pending_list() {
    let mut app = app();
    app.apply(ServerEvent::Connection(ConnectionState::Connected));
    app.apply(ServerEvent::Connection(ConnectionState::Reconnecting {
        attempt: 1,
        reason: "Gateway 断开了连接".into(),
    }));
    let effects = app.apply(ServerEvent::Connection(ConnectionState::Connected));
    assert_eq!(effects, vec![Effect::FetchPending]);
    assert!(
        notices(&app).contains("事件订阅已恢复"),
        "{}",
        notices(&app)
    );
    assert!(app.connection.is_connected());

    // 第一次连上不算"恢复"。
    let mut fresh = self::app();
    assert!(
        fresh
            .apply(ServerEvent::Connection(ConnectionState::Connected))
            .is_empty()
    );
}

/// 事件流里那条 Run 还"在跑"，清单上却有它：它是停在等人上的，状态行照清单说。它动起来
/// 之后（任何状态帧）再问一次清单，免得那一条一直挂着。
#[test]
fn a_run_on_the_pending_list_reads_as_waiting_and_its_next_status_refreshes_the_list() {
    let mut app = app();
    // 只到 run.started：事件折出来是"在跑"。
    feed(&mut app, &fixture::conversation()[..3]);
    assert_eq!(app.run_state(), Some(RunState::Running));
    let run = app.current_run.clone().expect("有一条 Run");
    let mut item = fixture::intervention_summary("h", InterventionKind::Blocked, "旧执行者还在");
    item.run = Some(run.clone());
    app.apply(ServerEvent::Pending(vec![item.clone()]));
    assert_eq!(app.run_state(), Some(RunState::Waiting));

    // 已经结束的 Run 不被清单改写。
    let mut done = self::app();
    feed(&mut done, &fixture::conversation());
    done.apply(ServerEvent::Pending(vec![item]));
    assert_eq!(done.run_state(), Some(RunState::Completed));

    let effects = app.apply(ServerEvent::Frame(Box::new(SseFrame {
        id: Seq(40),
        session: fixture::session(),
        event: SseEvent::RunStatus {
            run,
            state: RunState::Queued,
        },
    })));
    assert_eq!(effects, vec![Effect::FetchPending]);
}

/// 旧 Gateway 还会推审批帧（2026-10-11 删除）：游标照走，但**不提示**——它不是
/// "格式对不上"，也不该换来一次清单查询。
#[test]
fn a_retired_frame_from_an_old_gateway_moves_the_cursor_quietly() {
    let mut app = app();
    let effects = app.apply(ServerEvent::FrameRetired { id: Seq(20) });
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.cursor, Seq(20));
    assert!(app.notices.is_empty(), "{}", notices(&app));
}

/// 停在等待上的状态帧**顺手问一次清单**：权威清单在 `GET /v1/interventions`，状态行的
/// 待处理条数要跟得上。
#[test]
fn a_run_stopping_to_wait_asks_for_the_pending_list() {
    let mut app = app();
    let effects = app.apply(ServerEvent::Frame(Box::new(SseFrame {
        id: Seq(21),
        session: fixture::session(),
        event: SseEvent::RunStatus {
            run: fixture::run(),
            state: RunState::Waiting,
        },
    })));
    assert_eq!(effects, vec![Effect::FetchPending]);

    // 别的状态不额外问一次（列表在答复、重连、`/pending` 时都会问）。
    let quiet = app.apply(ServerEvent::Frame(Box::new(SseFrame {
        id: Seq(22),
        session: fixture::session(),
        event: SseEvent::RunStatus {
            run: fixture::run(),
            state: RunState::Running,
        },
    })));
    assert!(quiet.is_empty(), "{quiet:?}");
}

/// 别处答掉的那条（另一个终端、聊天、模型在 shell 里替人答）不经过本会话的事件流：清单上
/// 挂着东西时定期悄悄再问，答掉的那条下一拍就从状态行上消失，问回来的清单不印。
#[test]
fn an_answer_given_elsewhere_leaves_the_status_line_on_the_next_poll() {
    let mut app = app();
    app.apply(ServerEvent::Connection(ConnectionState::Connected));
    assert!(app.poll_pending().is_empty(), "清单是空的不问");

    app.apply(pending_list(&["run-1"]));
    assert_eq!(app.poll_pending(), vec![Effect::RefreshPending]);

    let printed = app.notices.len();
    // 清单没变：照样不印。
    app.apply(ServerEvent::PendingRefreshed(vec![
        fixture::intervention_summary("run-1", InterventionKind::Verify, "上次那个调用发生了没有"),
    ]));
    assert_eq!(app.pending_count(), 1);
    assert_eq!(app.notices.len(), printed, "{}", notices(&app));
    // 那条在别处答掉了。
    let effects = app.apply(ServerEvent::PendingRefreshed(Vec::new()));
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.pending_count(), 0);
    assert_eq!(app.notices.len(), printed, "{}", notices(&app));
    assert!(app.poll_pending().is_empty());

    // Gateway 正在重启时不问：重连成功那一刻本来就会问一次。
    app.apply(pending_list(&["run-1"]));
    app.apply(ServerEvent::Connection(ConnectionState::Reconnecting {
        attempt: 1,
        reason: "Gateway 断开了连接".into(),
    }));
    assert!(app.poll_pending().is_empty());
}

/// 本会话的 Run 结束、清单上挂着别的会话的：这一轮里多半答过其中一条，立刻悄悄问一次。
#[test]
fn a_run_ending_here_refreshes_a_pending_list_it_is_not_on() {
    let mut app = app();
    let here = RunId::from_raw("run-here");
    let ended = |app: &mut App, state| {
        app.apply(ServerEvent::Frame(Box::new(SseFrame {
            id: Seq(30),
            session: fixture::session(),
            event: SseEvent::RunStatus {
                run: here.clone(),
                state,
            },
        })))
    };
    assert!(
        ended(&mut app, RunState::Completed).is_empty(),
        "清单是空的不问"
    );

    app.apply(pending_list(&["run-1"]));
    assert_eq!(
        ended(&mut app, RunState::Completed),
        vec![Effect::RefreshPending]
    );
    assert_eq!(
        ended(&mut app, RunState::Failed),
        vec![Effect::RefreshPending]
    );
    assert!(ended(&mut app, RunState::Running).is_empty(), "还在跑不问");
}

// ---- 命令 ----

fn command(app: &mut App, text: &str) -> Vec<Effect> {
    app.input.set(text);
    app.handle_key(key(KeyCode::Enter))
}

#[test]
fn new_and_status_and_pending_each_produce_their_call() {
    let mut app = app();
    assert_eq!(command(&mut app, "/new"), vec![Effect::Boundary]);
    assert_eq!(command(&mut app, "/status"), vec![Effect::FetchStatus]);
    assert_eq!(command(&mut app, "/pending"), vec![Effect::FetchPending]);
}

#[test]
fn an_unknown_command_is_reported_not_sent_as_a_message() {
    let mut app = app();
    assert!(command(&mut app, "/aprove").is_empty());
    assert!(
        notices(&app).contains("没有 /aprove 这个命令"),
        "{}",
        notices(&app)
    );
    assert!(app.notices.last().unwrap().is_error);
}

#[test]
fn a_leading_path_is_sent_as_a_message() {
    let mut app = app();
    let effects = command(&mut app, "/api/checkout/paypal 接口逻辑");
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit { text, .. }] if text == "/api/checkout/paypal 接口逻辑"
        ),
        "{effects:?}"
    );
}

#[test]
fn an_illegal_effort_is_refused_with_the_options_spelled_out() {
    let mut app = app();
    assert!(command(&mut app, "/effort hgih").is_empty());
    let printed = notices(&app);
    assert!(printed.contains("hgih"), "{printed}");
    for level in command::EFFORT_LEVELS {
        assert!(printed.contains(level), "{printed}");
    }
    assert!(app.effort.is_none(), "非法值不会被悄悄接受");
}

#[test]
fn a_legal_effort_rides_on_the_next_submit() {
    let mut app = app();
    command(&mut app, "/effort high");
    command(&mut app, "/model gpt-x");
    let effects = command(&mut app, "跑一下");
    let Effect::Submit { model, effort, .. } = &effects[0] else {
        panic!("{effects:?}")
    };
    assert_eq!(model.as_deref(), Some("gpt-x"));
    assert_eq!(
        effort.as_ref().map(|e| e.to_string()).as_deref(),
        Some("high")
    );
}

/// `GET /v1/models` 回来的一份菜单。
fn menu() -> Vec<ModelMenuEntry> {
    vec![
        ModelMenuEntry {
            id: "gpt-x".into(),
            provider: "openai_compatible".into(),
            efforts: vec![Effort::new("low"), Effort::new("high")],
            default: true,
            ..ModelMenuEntry::default()
        },
        ModelMenuEntry {
            id: "claude-y".into(),
            provider: "anthropic".into(),
            efforts: vec![],
            default: false,
            ..ModelMenuEntry::default()
        },
    ]
}

#[test]
fn model_with_no_argument_goes_and_asks_then_opens_a_picker() {
    let mut app = app();
    // 清单是去问来的，不是启动时抄下来就不再更新的一份。
    assert_eq!(command(&mut app, "/model"), vec![Effect::FetchModels]);
    assert!(app.model_picker.is_none());
    app.apply(ServerEvent::ModelMenu(menu()));
    let picker = app.model_picker.as_ref().expect("清单回来了就该打开菜单");
    assert_eq!(picker.entries.len(), 2);
    // 没设过模型 ⇒ 高亮落在网关的 default 上。
    assert_eq!(picker.selected, 0);
    // 菜单开着，输入框让位。
    assert!(!app.input_enabled());
}

#[test]
fn the_picker_moves_and_enter_sets_the_model() {
    let mut app = app();
    command(&mut app, "/model");
    app.apply(ServerEvent::ModelMenu(menu()));
    assert!(app.handle_key(key(KeyCode::Down)).is_empty());
    assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
    assert!(app.model_picker.is_none());
    assert_eq!(app.model.as_deref(), Some("claude-y"));
    // 选中了什么看状态行，不另印一行。
    assert_eq!(app.model_label(), "claude-y");
    assert!(app.notices.is_empty(), "{:?}", app.notices);
    assert!(app.input_enabled());
}

#[test]
fn the_status_line_shows_the_next_runs_model_and_effort_when_idle() {
    let mut app = app();
    app.apply(ServerEvent::ModelMenu(menu()));
    // 什么都没设 ⇒ 网关的 default，effort 跟随 Gateway 就不印。
    assert_eq!(app.model_label(), "gpt-x");
    command(&mut app, "/effort high");
    assert_eq!(app.model_label(), "gpt-x · high");
    // 上一轮跑完了，再选模型 ⇒ 立刻印新选的，而不是上一轮那个。
    feed(&mut app, &fixture::conversation());
    command(&mut app, "/model claude-y");
    assert_eq!(app.model_label(), "claude-y");
}

#[test]
fn the_status_line_shows_the_running_runs_own_model() {
    let mut app = app();
    command(&mut app, "/model claude-y");
    feed(&mut app, &fixture::conversation()[..3]);
    assert_eq!(app.model_label(), "chat-a · high");
}

#[test]
fn a_digit_in_the_picker_picks_that_row_directly() {
    let mut app = app();
    command(&mut app, "/model");
    app.apply(ServerEvent::ModelMenu(menu()));
    app.handle_key(key(KeyCode::Char('2')));
    assert_eq!(app.model.as_deref(), Some("claude-y"));
    assert!(app.model_picker.is_none());
}

#[test]
fn escape_closes_the_picker_without_changing_anything() {
    let mut app = app();
    command(&mut app, "/model");
    app.apply(ServerEvent::ModelMenu(menu()));
    app.handle_key(key(KeyCode::Down));
    // 菜单开着时 Esc 只关菜单，不是取消 Run。
    assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
    assert!(app.model_picker.is_none());
    assert!(app.model.is_none());
}

#[test]
fn the_picker_reopens_on_the_model_already_chosen() {
    let mut app = app();
    app.apply(ServerEvent::ModelMenu(menu()));
    command(&mut app, "/model claude-y");
    command(&mut app, "/model");
    app.apply(ServerEvent::ModelMenu(menu()));
    assert_eq!(app.model_picker.as_ref().map(|p| p.selected), Some(1));
}

#[test]
fn switching_to_a_model_that_refuses_the_chosen_effort_drops_it() {
    let mut app = app();
    app.apply(ServerEvent::ModelMenu(menu()));
    command(&mut app, "/effort high");
    command(&mut app, "/model claude-y");
    // 留着它，下一次提交就会被网关拒掉。
    assert!(app.effort.is_none());
    assert!(
        notices(&app).contains("不收 effort high"),
        "{}",
        notices(&app)
    );
    // 换回一个收这一档的模型，再设的时候照常。
    command(&mut app, "/model gpt-x");
    command(&mut app, "/effort high");
    command(&mut app, "/model gpt-x");
    assert_eq!(
        app.effort.as_ref().map(|e| e.to_string()).as_deref(),
        Some("high")
    );
}

#[test]
fn a_menu_arriving_on_its_own_does_not_print_anything() {
    let mut app = app();
    app.apply(ServerEvent::ModelMenu(menu()));
    assert!(app.notices.is_empty(), "{:?}", app.notices);
    assert_eq!(app.model_options(), vec!["gpt-x", "claude-y"]);
}

#[test]
fn a_model_outside_the_menu_is_refused_with_the_menu_spelled_out() {
    let mut app = app();
    app.apply(ServerEvent::ModelMenu(menu()));
    assert!(command(&mut app, "/model gpt-z").is_empty());
    assert!(notices(&app).contains("gpt-x"), "{}", notices(&app));
    assert!(app.model.is_none());
}

#[test]
fn model_says_so_when_the_gateway_offered_no_menu() {
    let mut app = app();
    command(&mut app, "/model");
    // 还没回来，先什么都不印；空菜单回来了才说「没有报出模型清单」。
    app.apply(ServerEvent::ModelMenu(vec![]));
    assert!(
        notices(&app).contains("没有报出模型清单"),
        "{}",
        notices(&app)
    );
}

#[test]
fn the_effort_options_come_from_the_menus_entry_for_the_current_model() {
    let mut app = app();
    app.apply(ServerEvent::ModelMenu(menu()));
    // 没选模型 ⇒ 菜单里标了 default 的那一个。
    assert_eq!(app.effort_options(), vec!["low", "high"]);
    assert!(command(&mut app, "/effort medium").is_empty());
    assert!(notices(&app).contains("low · high"), "{}", notices(&app));
    assert!(app.effort.is_none());
    assert!(command(&mut app, "/effort high").is_empty());
    assert_eq!(
        app.effort.as_ref().map(|e| e.to_string()).as_deref(),
        Some("high")
    );
}

#[test]
fn a_model_that_takes_no_explicit_effort_refuses_every_level() {
    let mut app = app();
    app.apply(ServerEvent::ModelMenu(menu()));
    command(&mut app, "/model claude-y");
    // kernel：空表就是「一档都不支持」，不是「还不知道」。
    assert!(app.effort_options().is_empty());
    assert!(command(&mut app, "/effort low").is_empty());
    assert!(
        notices(&app).contains("不接受显式 effort"),
        "{}",
        notices(&app)
    );
    assert!(app.effort.is_none());
}

#[test]
fn without_a_menu_the_built_in_effort_whitelist_stands_in() {
    let app = app();
    assert_eq!(app.effort_options(), command::EFFORT_LEVELS);
}

#[test]
fn the_command_palette_filters_while_the_slash_word_is_being_typed() {
    let mut app = app();
    type_text(&mut app, "/an");
    assert_eq!(app.palette().len(), 1);
    assert_eq!(app.palette()[0].0, "/answer");
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.input.text(), "/answer");
}

#[test]
fn a_request_key_is_different_for_every_operation() {
    let mut app = app();
    let first = command(&mut app, "一");
    let second = command(&mut app, "二");
    let (Effect::Submit { request_key: a, .. }, Effect::Submit { request_key: b, .. }) =
        (&first[0], &second[0])
    else {
        panic!()
    };
    assert_ne!(a, b);
}

// ---- 折叠出来的视图 ----

#[test]
fn the_conversation_folds_into_messages_and_tool_lines() {
    let mut app = app();
    feed(&mut app, &fixture::conversation());

    let texts: Vec<&str> = app
        .messages()
        .iter()
        .filter_map(|m| m.text.as_deref())
        .collect();
    assert_eq!(
        texts,
        vec![
            "把 build 目录清掉",
            "我来清一下。",
            "清完了，删掉 12 个文件。"
        ]
    );

    let tools = app.tool_lines();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].tool, "bash");
    assert_eq!(tools[0].summary(), "rm -rf build");
    assert_eq!(tools[0].state, ToolCallState::Completed);
    assert_eq!(tools[0].marker(None), "✔");
    assert_eq!(tools[0].preview.as_deref(), Some("removed 12 files"));
    assert_eq!(tools[0].elapsed_ms, 340);
    assert_eq!(tools[0].attempts, 1);
}

/// 结果正文取 `body` 里那段完整的，不取顶层截在 1 KiB 的预览；失败时 body 只有原因也照
/// 样取到。没有 body 的旧事件才退回顶层那份（上一个测试）。
#[test]
fn a_tool_result_shows_the_full_body_not_the_one_kib_preview() {
    let full = "一行输出\n".repeat(400);
    for (preview, error, want) in [
        (Some(full.clone()), None, full.as_str()),
        (None, Some("exit=2".to_string()), "exit=2"),
    ] {
        let mut app = app();
        let mut events = fixture::conversation();
        for event in &mut events {
            if let EventPayload::ToolResult(body) = &mut event.payload {
                body.body = Some(Box::new(komo_kernel::types::refs::ToolResultBody {
                    status: ToolResultStatus::Completed,
                    result: serde_json::Value::Null,
                    error: error.clone(),
                    exit_code: None,
                    artifacts: vec![],
                    images: vec![],
                    preview: preview.clone(),
                }));
            }
        }
        feed(&mut app, &events);
        assert_eq!(app.tool_lines()[0].preview.as_deref(), Some(want));
    }
}

/// 转到后台的 bash 在事件上是 completed，界面上不是成功：认的是结果里的 `detached`。
#[test]
fn a_result_handed_to_the_background_is_marked_detached() {
    for (result, want) in [
        (serde_json::json!({"detached": "0199-abcd"}), "↗"),
        (serde_json::json!({"detached": null, "exit_code": 0}), "✔"),
    ] {
        let mut app = app();
        let mut events = fixture::conversation();
        for event in &mut events {
            if let EventPayload::ToolResult(body) = &mut event.payload {
                body.body = Some(Box::new(komo_kernel::types::refs::ToolResultBody {
                    status: ToolResultStatus::Completed,
                    result: result.clone(),
                    error: None,
                    exit_code: None,
                    artifacts: vec![],
                    images: vec![],
                    preview: Some("已转后台 #abcd".into()),
                }));
            }
        }
        feed(&mut app, &events);
        assert_eq!(app.tool_lines()[0].marker(None), want, "{result}");
    }
}

/// `Ctrl-T` 是一个**模式**，管的是整段会话：切一次，下一帧就整段重画——已经落进回滚区
/// 的那些也跟着展开或收起。
#[test]
fn ctrl_t_turns_tool_detail_on_and_off() {
    let mut app = app();
    feed(&mut app, &fixture::conversation());
    assert!(!app.tool_detail);
    assert!(!app.repaint);
    app.handle_key(with(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert!(app.tool_detail, "展开看参数与结果预览");
    assert!(
        std::mem::take(&mut app.repaint),
        "已经印出去的那些也要跟着展开"
    );
    app.handle_key(with(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert!(!app.tool_detail);
    assert!(app.repaint, "收起来也一样");
}

// ---- 打字机效果（`assistant_delta`） ----

fn delta(app: &mut App, seq: u64, round: u32, text: &str) {
    app.apply(ServerEvent::Frame(Box::new(SseFrame {
        id: Seq(seq),
        session: fixture::session(),
        event: SseEvent::AssistantDelta {
            run: fixture::run(),
            round,
            text: text.into(),
        },
    })));
}

#[test]
fn deltas_accumulate_then_the_real_message_replaces_the_draft() {
    let mut app = app();
    feed(&mut app, &fixture::conversation()[..3]);
    let history_before = app.messages().len();

    delta(&mut app, 100, 1, "我来");
    delta(&mut app, 101, 1, "清一");
    delta(&mut app, 102, 1, "下。");
    assert_eq!(
        app.draft.as_ref().map(|d| d.text.as_str()),
        Some("我来清一下。")
    );
    // 草稿不是历史：`Surface` 一条都没多。
    assert_eq!(app.messages().len(), history_before);

    // 正式回复到了。
    feed(&mut app, &fixture::conversation()[3..4]);
    assert!(app.draft.is_none(), "草稿让位给正式消息");
    let assistant: Vec<&str> = app
        .messages()
        .iter()
        .filter(|m| m.role == komo_kernel::types::turn::Role::Assistant)
        .filter_map(|m| m.text.as_deref())
        .collect();
    // 历史上只有一条，不是「草稿一条 + 正式一条」。
    assert_eq!(assistant, vec!["我来清一下。"]);
}

#[test]
fn a_delta_for_a_new_round_starts_a_fresh_draft() {
    let mut app = app();
    delta(&mut app, 100, 1, "第一轮");
    delta(&mut app, 101, 2, "第二轮");
    let draft = app.draft.as_ref().unwrap();
    assert_eq!(draft.round, 2);
    assert_eq!(draft.text, "第二轮", "换轮不往上一段后面接");
}

#[test]
fn a_delta_never_reaches_the_replay_window() {
    let mut app = app();
    feed(&mut app, &fixture::conversation()[..3]);
    delta(&mut app, 100, 1, "还没说完");
    // 它只在 SSE 上，JSONL 里没有它——所以 fold 出来的 applied_seq 不因它前进。
    assert_eq!(app.surface.applied_seq, Seq(3));
    assert!(
        app.surface
            .messages
            .iter()
            .all(|m| m.text.as_deref() != Some("还没说完"))
    );
}

#[test]
fn a_run_that_ends_without_a_final_message_still_clears_the_draft() {
    let mut app = app();
    feed(&mut app, &fixture::conversation()[..3]);
    delta(&mut app, 100, 1, "打了一半");
    feed(
        &mut app,
        &[fixture::event(
            4,
            Some(fixture::run()),
            EventPayload::RunCancelled(komo_kernel::events::RunCancelled { by: None }),
        )],
    );
    assert!(app.draft.is_none(), "没人在打字了就别一直显示生成中");
}

/// 插话（§6）：打的话一律要求插话（轮不轮得上由服务端判）；`!` 命令不对话，不要求。
#[test]
fn typed_text_asks_to_interject_and_a_bang_command_does_not() {
    let mut app = app();
    type_text(&mut app, "顺便看下 dist");
    let effects = app.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit {
                interject: true,
                ..
            }]
        ),
        "{effects:?}"
    );
    type_text(&mut app, "!ls");
    let effects = app.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Submit {
                interject: false,
                shell: true,
                ..
            }]
        ),
        "{effects:?}"
    );
}

/// 在跑的时候又来一条（打的话、监听事件）：状态行与 Esc 仍对着在跑的那条；那条被并走
/// 之后也还是宿主——回答在宿主那里。
#[test]
fn a_queued_or_absorbed_run_never_takes_the_status_line_from_the_running_one() {
    let mut app = app();
    let events = fixture::interjected();
    feed(&mut app, &events[..9]);
    assert_eq!(app.current_run.as_ref(), Some(&fixture::run()));
    assert_eq!(app.run_state(), Some(RunState::Running));
    let cancel = app.handle_key(key(KeyCode::Esc));
    assert!(
        matches!(cancel.as_slice(), [Effect::Cancel { run, .. }] if *run == fixture::run()),
        "{cancel:?}"
    );

    feed(&mut app, &events[9..10]);
    assert_eq!(app.current_run.as_ref(), Some(&fixture::run()));
    assert_eq!(app.run_state(), Some(RunState::Running));
    assert_eq!(
        app.surface.runs[&RunId::from_raw("run-2")].status,
        RunState::Absorbed
    );
}

/// 前面那条已经结束了，新来的就是状态行该说的那条。
#[test]
fn a_new_run_after_the_last_one_ended_takes_the_status_line() {
    let mut app = app();
    feed(&mut app, &fixture::conversation());
    let mut next = fixture::interjected()[7].clone();
    next.seq = Seq(10);
    feed(&mut app, &[next]);
    assert_eq!(app.current_run.as_ref(), Some(&RunId::from_raw("run-2")));
    assert_eq!(app.run_state(), Some(RunState::Accepted));
}

#[test]
fn a_delta_that_arrives_before_anything_else_still_names_the_run() {
    let mut app = app();
    delta(&mut app, 1, 1, "嗯");
    assert_eq!(app.current_run.as_ref(), Some(&fixture::run()));
}

#[test]
fn an_uncertain_call_is_marked_with_two_question_marks() {
    let mut app = app();
    let mut events = fixture::conversation();
    // 把那次调用的结果改成「不知道」。
    if let EventPayload::ToolResult(body) = &mut events[6].payload {
        body.status = ToolResultStatus::Uncertain;
    }
    feed(&mut app, &events[..7]);
    let tools = app.tool_lines();
    assert_eq!(tools[0].state, ToolCallState::Uncertain);
    assert_eq!(tools[0].marker(None), "?", "既不是成功也不是失败那个符号");
}

#[test]
fn the_status_line_reads_the_model_and_effort_the_run_was_fixed_on() {
    let mut app = app();
    feed(&mut app, &fixture::conversation()[..1]);
    let meta = app.run_meta().unwrap();
    assert_eq!(meta.model.as_deref(), Some("chat-a"));
    assert_eq!(
        meta.effort
            .as_ref()
            .and_then(|e| e.as_option())
            .map(|e| e.to_string()),
        Some("high".to_string())
    );
}

#[test]
fn the_elapsed_time_freezes_when_the_run_ends() {
    let mut app = app();
    feed(&mut app, &fixture::conversation());
    app.apply(ServerEvent::Tick(fixture::T0 + time::Duration::hours(1)));
    // run.accepted 在 T0+1s，run.completed 在 T0+9s。
    assert_eq!(app.elapsed(), Some(time::Duration::seconds(8)));
}

#[test]
fn a_run_still_going_measures_against_the_clock_the_driver_gave() {
    let mut app = app();
    feed(&mut app, &fixture::conversation()[..3]);
    app.apply(ServerEvent::Tick(fixture::T0 + time::Duration::seconds(31)));
    assert_eq!(app.elapsed(), Some(time::Duration::seconds(30)));
}

#[test]
fn every_run_state_has_a_name_and_they_are_all_different() {
    let all = [
        RunState::Accepted,
        RunState::Queued,
        RunState::Running,
        RunState::Waiting,
        RunState::Completed,
        RunState::Failed,
        RunState::Cancelled,
        RunState::Abandoned,
    ];
    let mut names: Vec<&str> = all.iter().map(|s| status_text(*s)).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all.len(), "八个状态如实显示，不合并");
    // 停着的那一个不自己编"为什么"：理由由 `wait_text` 说（§8.4 的第二个维度）。
    assert_eq!(status_text(RunState::Waiting), "等待中");
}

/// §8.4：**状态只说"能不能跑"，理由说"在等谁、等到什么时候"**。没有理由那一格，
/// 状态行上的"等待中"就是一句等于没说的话——"排队二十分钟不知道为什么"正是它要堵的坑。
#[test]
fn the_wait_reason_says_what_it_is_waiting_for() {
    let now = fixture::T0;
    // 2026-10-11 之前的旧行：Gateway 启动时直接续跑它，不说"等审批"。
    let approval = WaitReason::Approval {
        approval: "appr-1".into(),
    };
    assert_eq!(
        wait_text(&approval, Some(now)),
        "旧的审批等待，Gateway 会直接续跑"
    );

    let retry = WaitReason::Retry {
        attempts: 2,
        not_before: now + time::Duration::seconds(30),
        cause: RetryCause::RateLimited,
    };
    assert_eq!(wait_text(&retry, Some(now)), "30s 后重试（限流，第 2 次）");

    // 到点了还停着：别报一个负数的"还要等"。
    assert_eq!(
        wait_text(&retry, Some(now + time::Duration::seconds(90))),
        "马上重试（限流，第 2 次）"
    );

    let dependency = WaitReason::Dependency {
        run: RunId::from_raw("run-9"),
    };
    assert_eq!(wait_text(&dependency, Some(now)), "在等 Run run-9");
}

#[test]
fn a_boundary_moves_the_replay_window_without_losing_the_transcript() {
    let mut app = app();
    feed(&mut app, &fixture::conversation());
    feed(
        &mut app,
        &[fixture::event(
            10,
            None,
            EventPayload::ConversationBoundary(komo_kernel::events::ConversationBoundary {
                by: None,
            }),
        )],
    );
    // 人读的历史一个字都没少……
    assert_eq!(app.messages().len(), 4);
    // ……交给模型回放的窗口从这一刀之后开始。
    assert!(app.surface.replay().is_empty());
}

// ---- 补读历史 ----

#[test]
fn a_resume_reads_the_whole_history_before_it_becomes_interactive() {
    let mut app = App::new(Some(fixture::session()), TuiMode::Resume, "seed");
    assert!(app.phase.is_backfilling());

    let events = fixture::conversation();
    app.apply(ServerEvent::HistoryPage(Box::new(fixture::page(
        events[..4].to_vec(),
        true,
    ))));
    assert!(app.phase.is_backfilling(), "还有下一页，别急着交互");

    // 补读期间敲回车不会提交——那条消息会插到一段还没读完的上下文前面。
    app.input.set("等不及了");
    assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
    assert!(notices(&app).contains("正在补读历史"));

    app.apply(ServerEvent::HistoryPage(Box::new(fixture::page(
        events[4..].to_vec(),
        false,
    ))));
    app.apply(ServerEvent::HistoryDone);

    assert_eq!(app.phase, Phase::Interactive);
    assert_eq!(app.cursor, Seq(9), "游标停在最后一条事件上");
    // 三条正文消息 + 一个承载工具结果的节点。
    assert_eq!(app.messages().len(), 4);
    assert_eq!(app.run_state(), Some(RunState::Completed));

    // 这时候才发得出去。
    app.input.set("接着说");
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter))[0],
        Effect::Submit { .. }
    ));
}

#[test]
fn the_pending_list_is_asked_for_the_moment_the_history_is_read() {
    let mut app = App::new(Some(fixture::session()), TuiMode::Resume, "seed");
    app.apply(ServerEvent::HistoryPage(Box::new(fixture::page(
        fixture::conversation()[..4].to_vec(),
        false,
    ))));
    // 补读完之后**问一次清单**：待处理的可以属于别的会话，本会话的事件流里没有它们。
    assert_eq!(
        app.apply(ServerEvent::HistoryDone),
        vec![Effect::FetchPending]
    );

    // 清单回来了：进状态行与提示行，没有弹窗、也不再去取详情。
    let effects = app.apply(pending_list(&["run-1"]));
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.pending_count(), 1);
    assert!(app.input_enabled(), "介入不挡输入框");
}

#[test]
fn a_new_session_does_not_wait_for_a_backfill() {
    let app = App::new(Some(fixture::session()), TuiMode::New, "seed");
    assert_eq!(app.phase, Phase::Interactive);
}

#[test]
fn the_resume_summary_reads_like_the_design_doc_sentence() {
    let response = ResumeResponse {
        session: fixture::session(),
        resumed: vec![
            RunSummary {
                run: RunId::from_raw("run-1"),
                session: fixture::session(),
                state: RunState::Queued,
                wait: None,
                source: komo_kernel::types::plan::PlanSource::Interactive {
                    session: fixture::session(),
                },
                rounds: 0,
                created_at: fixture::T0,
                ended_at: None,
            },
            RunSummary {
                run: RunId::from_raw("run-2"),
                session: fixture::session(),
                state: RunState::Queued,
                wait: None,
                source: komo_kernel::types::plan::PlanSource::Interactive {
                    session: fixture::session(),
                },
                rounds: 0,
                created_at: fixture::T0,
                ended_at: None,
            },
        ],
        pending: vec![fixture::intervention_summary(
            "run-3",
            InterventionKind::Verify,
            "上次那个调用发生了没有",
        )],
    };
    assert_eq!(
        resume_summary(&response).as_deref(),
        Some("2 个任务已接续，1 个结果不明")
    );

    // 没有未完成的任务是**默认情况**，不占一行——打开 TUI 先读一句"没事"没有意义。
    let quiet = ResumeResponse {
        session: fixture::session(),
        resumed: vec![],
        pending: vec![],
    };
    assert_eq!(resume_summary(&quiet), None);
}

#[test]
fn a_status_reply_says_what_is_running_and_how_many_wait() {
    let mut app = app();
    // 待处理数读的是**清单那一份**（两类合计、全部会话），不是会话详情里本会话那几条。
    app.apply(pending_list(&["run-1"]));
    app.apply(ServerEvent::Status(Box::new(SessionDetail {
        summary: SessionSummary {
            session: fixture::session(),
            title: "清理".into(),
            state: SessionState::Active,
            workdir: None,
            current_run: Some(fixture::run()),
            current_state: Some(RunState::Waiting),
            current_wait: None,
            applied_seq: Seq(9),
            created_at: fixture::T0,
            updated_at: fixture::T0,
            kind: "normal".into(),
            home: None,
        },
        unfinished: vec![],
        pending: vec![],
    })));
    assert!(notices(&app).contains("等待中"), "{}", notices(&app));
    assert!(notices(&app).contains("待处理 1"), "{}", notices(&app));
}

#[test]
fn a_frame_this_build_cannot_read_still_moves_the_cursor() {
    let mut app = app();
    app.apply(ServerEvent::FrameSkipped {
        id: Seq(77),
        reason: "未来的事件类型".into(),
    });
    assert_eq!(app.cursor, Seq(77));
}
// ---- 两类共用一张清单，一次答一条（§7.5、§11.3）----

/// `/pending` 的每一行都要**写清这一条该答什么**（§11.3）：句柄、种类、问题、可答结论。
#[test]
fn the_pending_list_prints_handle_kind_question_and_verdicts() {
    let mut app = app();
    app.apply(pending_mixed());
    let printed = notices(&app);
    assert!(printed.contains("run-1"), "{printed}");
    assert!(printed.contains("结果不明"), "{printed}");
    assert!(printed.contains("run-2"), "{printed}");
    assert!(printed.contains("阻塞"), "{printed}");
    // 「能答什么」来自清单自带的那一份，不是界面自己推的。
    assert!(
        printed.contains("satisfied / not_performed / abandon"),
        "{printed}"
    );
    assert!(printed.contains("resolve / abandon"), "{printed}");
    assert!(printed.contains("会话已删除"), "{printed}");
    // 清单之后跟一句答法：一次答一条。
    assert!(
        printed.contains("答一条：/answer <句柄> <结论>"),
        "{printed}"
    );
    // 两类合计进状态行/提示行。
    assert_eq!(app.pending_count(), 2);
    assert_eq!(app.pending_breakdown(), (1, 1));
}

/// 有待处理时，提示行**说出路**：`/pending` 看清单，`/answer` 答一条（§11.3）。
#[test]
fn a_verify_pending_tells_you_how_to_answer() {
    let mut app = app();
    app.apply(ServerEvent::Pending(vec![fixture::intervention_summary(
        "run-1",
        InterventionKind::Verify,
        "上次那个调用发生了没有",
    )]));
    let hint = app.input_hint();
    assert!(hint.contains("有 1 条待处理"), "{hint}");
    assert!(hint.contains("结果不明 1"), "{hint}");
    assert!(hint.contains("/pending"), "{hint}");
    assert!(hint.contains("/answer <句柄> <结论>"), "{hint}");
    assert!(!hint.contains("/approve"), "{hint}");
    // 没有弹窗：输入框照常可用，答复就是一条命令。
    assert!(app.input_enabled());
}

/// `/answer <句柄> <结论>`：结论按种类分派（§7.5），一次一条。
#[test]
fn answer_sends_the_verdict_for_the_handle_it_was_given() {
    let mut app = app();
    app.apply(pending_mixed());

    let effects = command(&mut app, "/answer run-1 satisfied");
    let [
        Effect::Answer {
            handle, verdict, ..
        },
    ] = effects.as_slice()
    else {
        panic!("{effects:?}");
    };
    assert_eq!(handle, "run-1");
    assert_eq!(*verdict, InterventionVerdict::Satisfied);

    let effects = command(&mut app, "/answer run-2 resolve");
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Answer {
                handle,
                verdict: InterventionVerdict::Resolve,
                ..
            }] if handle == "run-2"
        ),
        "{effects:?}"
    );
}

/// 给错种类的结论**说出来**，而不是发一个服务端只能拒绝的请求（§7.5：结论按种类分派）。
#[test]
fn a_verdict_that_does_not_belong_to_the_kind_is_refused_locally() {
    let mut app = app();
    app.apply(pending_mixed());

    // `resolve` 是阻塞的结论，答不了 `verify`。
    assert!(command(&mut app, "/answer run-1 resolve").is_empty());
    let printed = notices(&app);
    assert!(printed.contains("结果不明"), "{printed}");
    assert!(printed.contains("satisfied"), "{printed}");
    // 也没发出去过。
    assert!(!printed.contains("run-1 不在待处理清单里"), "{printed}");

    // 句柄不在清单上时，说出来而不是静默。
    assert!(command(&mut app, "/answer run-9 resolve").is_empty());
    assert!(
        notices(&app).contains("不在待处理清单里"),
        "{}",
        notices(&app)
    );
}

/// 拼错的结论要列出可选值——打字的人还在屏幕前。
#[test]
fn a_bad_verdict_word_lists_the_options() {
    let mut app = app();
    app.apply(pending_mixed());
    assert!(command(&mut app, "/answer run-1 satisfiedd").is_empty());
    let printed = notices(&app);
    assert!(printed.contains("satisfiedd"), "{printed}");
    for word in command::VERDICT_WORDS {
        assert!(printed.contains(word.as_str()), "{printed}");
    }

    // 少给一个参数也要说清楚它要两个。
    assert!(command(&mut app, "/answer run-1").is_empty());
    assert!(notices(&app).contains("要两个参数"), "{}", notices(&app));
}

/// 回执那句话由服务端给（`note`）：同一句结论的措辞不该在四个界面里各写一遍。
#[test]
fn an_answer_receipt_prints_the_note_and_drops_it_from_the_list() {
    let mut app = app();
    app.apply(pending_mixed());

    app.apply(ServerEvent::Answered(Box::new(
        komo_kernel::protocol::http::InterventionAnswerResponse {
            handle: "run-1".into(),
            kind: InterventionKind::Verify,
            verdict: InterventionVerdict::Satisfied,
            run_state: Some(RunState::Queued),
            note: "核对后目标已满足，原 Run 继续".into(),
            already_answered: false,
        },
    )));
    assert!(
        notices(&app).contains("核对后目标已满足"),
        "{}",
        notices(&app)
    );
    assert_eq!(app.pending_count(), 1, "答过的那条要从清单里下去");
    assert!(app.pending_item("run-1").is_none());

    // 早就答过的那一条：回执照样说清楚这次没改变什么。
    app.apply(ServerEvent::Answered(Box::new(
        komo_kernel::protocol::http::InterventionAnswerResponse {
            handle: "run-2".into(),
            kind: InterventionKind::Blocked,
            verdict: InterventionVerdict::Resolve,
            run_state: None,
            note: "已重新观察".into(),
            already_answered: true,
        },
    )));
    assert!(
        notices(&app).contains("早已答复，这次没有改变什么"),
        "{}",
        notices(&app)
    );
}

/// **别的会话的待处理，打开 TUI 也看得见。**
///
/// 介入可以属于定时任务那条会话、聊天里那条、别的 TUI 里开的那个，而本会话的事件流里
/// 没有它们——清单只能问服务端。
#[test]
fn interventions_from_other_sessions_are_visible_here() {
    let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
    assert_eq!(app.pending_count(), 0);

    // 本会话的事件流里一条都没有——清单照样把三条带回来。
    let effects = app.apply(pending_list(&["run-7", "run-8", "run-9"]));
    assert_eq!(app.pending_count(), 3);
    assert!(effects.is_empty(), "{effects:?}");
    let hint = app.input_hint();
    assert!(hint.contains('3'), "提示里要写清有几条：{hint}");
}

// ---- `!`：直接跑 shell ----

#[test]
fn a_bang_draft_is_shell_mode_and_backspacing_the_bang_leaves_it() {
    let mut app = app();
    type_text(&mut app, "!");
    assert!(app.shell_mode());
    assert_eq!(app.input_hint(), "", "终端模式不给文字提示");
    app.handle_key(key(KeyCode::Backspace));
    assert!(!app.shell_mode(), "删掉 ! 就回到普通模式");
    assert!(!app.input_hint().is_empty());
}

#[test]
fn enter_on_a_bang_sends_the_command_without_the_model() {
    let mut app = app();
    type_text(&mut app, "!ls -la");
    let effects = app.handle_key(key(KeyCode::Enter));
    let [
        Effect::Submit {
            text, shell, model, ..
        },
    ] = effects.as_slice()
    else {
        panic!("{effects:?}");
    };
    assert_eq!(text, "ls -la", "发出去的是 ! 后面那条命令");
    assert!(*shell);
    assert!(model.is_none(), "不经模型，不带模型覆盖");
    assert_eq!(
        app.pending_submissions[0].text, "!ls -la",
        "本地那条照原样显示"
    );
    assert_eq!(app.history.last().map(Input::text), Some("!ls -la"));
}

#[test]
fn a_bang_command_that_needs_a_terminal_is_refused_and_the_draft_stays() {
    let mut app = app();
    type_text(&mut app, "!vim notes.md");
    let effects = app.handle_key(key(KeyCode::Enter));
    assert!(effects.is_empty(), "{effects:?}");
    assert!(notices(&app).contains("没有 TTY"), "{}", notices(&app));
    assert_eq!(app.input.text(), "!vim notes.md", "草稿留在框里改");

    let mut app = super::App::new(Some(fixture::session()), TuiMode::New, "seed");
    type_text(&mut app, "!");
    assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
    assert!(
        notices(&app).contains("!ls -la"),
        "空命令给用法：{}",
        notices(&app)
    );
}

/// 终端模式跑完不补说明：撞上只读也只有结果本身。
#[test]
fn a_bang_result_gets_no_hint() {
    let mut app = app();
    let mut events = fixture::conversation();
    for event in &mut events {
        if let EventPayload::ToolResult(body) = &mut event.payload {
            body.status = ToolResultStatus::Failed;
            body.preview = Some(
                "exit=1 stdout=0B stderr=48B\nrm: cannot remove 'state.db': Read-only file system"
                    .into(),
            );
        }
    }
    let before = notices(&app);
    feed(&mut app, &events);
    assert_eq!(notices(&app), before);
}
