//! `App` 状态机的测试：按键序列 / 服务端事件 → 状态断言，不需要终端。

use super::*;
use crate::sse::ConnectionState;
use crate::tui::approval::ApprovalChoice;
use crate::tui::paste::{InputEvent, PASTE_MIN_BYTES, PasteChip};
use crate::tui::test_support as fixture;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::http::{
    InterventionKind, InterventionVerdict, ModelMenuEntry, RunSummary, SessionSummary,
};
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::types::ids::{ApprovalId, RunId, Seq, ShortId};
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
/// 不是从本会话的 `approval.requested` 折出来的那份——待处理可以属于别的会话，操作者在
/// TUI 里要看到的正是"现在有谁在等我"，所以清单只能问服务端。
///
/// 给的是**句柄**：审批的句柄就是短 ID（§11.3），所以 `"7K2M"` 与
/// `fixture::approval_record()` 是同一条。三类混排见 [`pending_mixed`]。
fn pending_list(handles: &[&str]) -> ServerEvent {
    ServerEvent::Pending(
        handles
            .iter()
            .map(|handle| {
                fixture::intervention_summary(
                    handle,
                    InterventionKind::Approval,
                    "放行 rm -rf build？",
                )
            })
            .collect(),
    )
}

/// 三类各来一条：审批、结果不明、阻塞。
fn pending_mixed() -> ServerEvent {
    ServerEvent::Pending(vec![
        fixture::intervention_summary("7K2M", InterventionKind::Approval, "放行 rm -rf build？"),
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

// ---- 审批弹窗 ----

fn with_modal() -> App {
    let mut app = app();
    app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));
    app
}

#[test]
fn y_approves_this_call_only() {
    let mut app = with_modal();
    let effects = app.handle_key(key(KeyCode::Char('y')));
    assert!(matches!(
        &effects[0],
        Effect::Answer {
            verdict: InterventionVerdict::Approve,
            scope: Some(ApprovalScope::Once),
            ..
        }
    ));
}

#[test]
fn r_approves_the_run_scope_only_when_policy_offered_it() {
    let mut app = with_modal();
    let effects = app.handle_key(key(KeyCode::Char('r')));
    assert!(matches!(
        &effects[0],
        Effect::Answer {
            verdict: InterventionVerdict::Approve,
            scope: Some(ApprovalScope::Run),
            ..
        }
    ));

    // 一条只可批本次的请求：`r` 不生效，而且说出为什么。
    let mut record = fixture::approval_record();
    record.scopes = vec![ApprovalScope::Once];
    let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
    app.apply(ServerEvent::Approval(Box::new(record)));
    assert!(app.handle_key(key(KeyCode::Char('r'))).is_empty());
    assert!(notices(&app).contains("不可范围化"), "{}", notices(&app));
}

#[test]
fn n_and_esc_both_reject() {
    for code in [KeyCode::Char('n'), KeyCode::Esc] {
        let mut app = with_modal();
        let effects = app.handle_key(key(code));
        assert!(
            matches!(
                &effects[0],
                Effect::Answer {
                    verdict: InterventionVerdict::Reject,
                    scope: None,
                    ..
                }
            ),
            "{code:?} 应当拒绝：{effects:?}"
        );
    }
}

/// **`Ctrl-C` 是暂停，弹窗开着也走得掉。**
///
/// 弹窗把按键全收走（`Esc` 在里面是拒绝），所以退出键必须在它之前处理——否则正卡在
/// 审批上的人只剩答完这一条才能走。暂停也不许替人答这条审批。
#[test]
fn ctrl_c_pauses_even_with_a_modal_up() {
    let mut app = with_modal();
    let effects = app.handle_key(with(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(matches!(effects.as_slice(), [Effect::Quit]), "{effects:?}");
    assert!(app.quit, "弹窗开着也要退得出去");
    assert!(app.approval.is_some(), "暂停不是替人答了这条审批");
}

#[test]
fn the_input_is_disabled_while_an_approval_is_open() {
    let mut app = with_modal();
    assert!(!app.input_enabled());
    // 提示行说出**怎么开这张菜单**：答案本身在弹窗的菜单里，这里不抄第二遍。
    let hint = app.input_hint();
    assert!(hint.contains("批准"), "{hint}");
    assert!(hint.contains("Enter 确认"), "{hint}");
    // 打字不进输入框——菜单才是这时候的输入。
    type_text(&mut app, "abc");
    assert!(app.input.is_empty());
}

/// 菜单的默认落点就是 `Enter` 的答案：能范围化时是「本次任务默认通过」，不能时是
/// 「只批准本次调用」——一次 `Enter` 永远落在看得见的那一行上。
#[test]
fn enter_confirms_the_highlighted_row() {
    let mut app = with_modal();
    let effects = app.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(
            &effects[0],
            Effect::Answer {
                verdict: InterventionVerdict::Approve,
                scope: Some(ApprovalScope::Run),
                ..
            }
        ),
        "{effects:?}"
    );

    // Policy 没标可范围化：菜单里没有那一行，默认落点跟着往下挪。
    let mut record = fixture::approval_record();
    record.scopes = vec![ApprovalScope::Once];
    let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
    app.apply(ServerEvent::Approval(Box::new(record)));
    let effects = app.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(
            &effects[0],
            Effect::Answer {
                verdict: InterventionVerdict::Approve,
                scope: Some(ApprovalScope::Once),
                ..
            }
        ),
        "{effects:?}"
    );
}

/// `↑` / `↓` 只动高亮：往下走到「拒绝」，`Enter` 就是拒绝；到两头停住，不绕回去。
#[test]
fn the_arrows_move_the_highlight_and_stop_at_the_ends() {
    let mut app = with_modal();
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.approval.as_ref().expect("弹窗").selected_index(1),
        1,
        "第二行是「只批准本次调用」"
    );
    app.handle_key(key(KeyCode::Down));
    let effects = app.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(
            &effects[0],
            Effect::Answer {
                verdict: InterventionVerdict::Reject,
                scope: None,
                ..
            }
        ),
        "{effects:?}"
    );

    // 到头了：再往下不动，往上回到第一行（`Enter` = 本次任务默认通过）。
    let mut app = with_modal();
    for _ in 0..9 {
        app.handle_key(key(KeyCode::Down));
    }
    assert_eq!(app.approval.as_ref().expect("弹窗").selected_index(1), 2);
    for _ in 0..9 {
        app.handle_key(key(KeyCode::Up));
    }
    assert_eq!(app.approval.as_ref().expect("弹窗").selected_index(1), 0);
}

/// 正文的滚动键与菜单无关：`PgUp` / `PgDn` 滚正文，高亮一动不动（正文可能比窗口长，
/// 高亮不该跟着滚走）。
#[test]
fn the_page_keys_scroll_the_body_without_moving_the_highlight() {
    let mut app = with_modal();
    app.handle_key(key(KeyCode::PageDown));
    app.handle_key(key(KeyCode::PageDown));
    let modal = app.approval.as_ref().expect("弹窗");
    assert_eq!(modal.scroll, 8, "滚的是正文");
    assert_eq!(modal.selected_index(1), 0, "高亮还在第一行");
    app.handle_key(key(KeyCode::PageUp));
    assert_eq!(app.approval.as_ref().expect("弹窗").scroll, 4);
}

/// 菜单里那行「全部批准」按下去和 `a` 是同一个答复：三条一起答，眼前这条排第一。
#[test]
fn the_batch_row_answers_the_visible_one_first() {
    let mut app = app();
    app.apply(pending_list(&["7K2M", "9QRS", "3TVW"]));
    app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));

    // 往下走三下到批量那一行（范围 → 本次 → 拒绝 → 全部批准）。
    for _ in 0..3 {
        app.handle_key(key(KeyCode::Down));
    }
    let rows = app.approval.as_ref().expect("弹窗").rows(3);
    assert_eq!(
        rows[app.approval.as_ref().expect("弹窗").selected_index(3)].choice,
        ApprovalChoice::AllPending
    );

    let effects = app.handle_key(key(KeyCode::Enter));
    let [Effect::AnswerMany { handles, .. }] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    assert_eq!(handles.len(), 3, "{handles:?}");
    assert_eq!(
        handles[0],
        fixture::approval_record().short_id.as_str(),
        "眼前这条排第一"
    );
}

#[test]
fn answering_twice_only_sends_one_decision() {
    let mut app = with_modal();
    assert_eq!(app.handle_key(key(KeyCode::Char('y'))).len(), 1);
    assert!(
        app.handle_key(key(KeyCode::Char('y'))).is_empty(),
        "第二下不该再发一个请求"
    );
}

#[test]
fn a_failed_decision_lets_the_operator_answer_again() {
    let mut app = with_modal();
    app.handle_key(key(KeyCode::Char('y')));
    app.apply(ServerEvent::Failed("网络断了".into()));
    assert_eq!(app.handle_key(key(KeyCode::Char('y'))).len(), 1);
}

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

#[test]
fn an_approval_answered_elsewhere_closes_the_modal_here() {
    let mut app = with_modal();
    let approval = fixture::approval_record().approval;
    app.apply(ServerEvent::Frame(Box::new(SseFrame {
        id: Seq(99),
        session: fixture::session(),
        event: SseEvent::ApprovalDecided {
            approval,
            approved: true,
        },
    })));
    assert!(app.approval.is_none());
    assert!(notices(&app).contains("在别处已批准"), "{}", notices(&app));
}

#[test]
fn an_already_decided_approval_does_not_pop_up() {
    let mut app = app();
    let mut record = fixture::approval_record();
    record.decision = Some(komo_kernel::protocol::http::ApprovalDecisionRecord {
        approved: true,
        scope: ApprovalScope::Once,
        by: None,
        decided_at: fixture::T0,
        grant: None,
        consumed: true,
    });
    app.apply(ServerEvent::Approval(Box::new(record)));
    assert!(app.approval.is_none(), "点了没用的窗不该弹");
}

#[test]
fn an_approval_pending_notice_never_carries_the_authorization_itself() {
    // §13.1：详情去 `GET /v1/interventions/{handle}` 取，句柄由清单带回来。
    let mut app = app();
    let effects = app.apply(ServerEvent::Frame(Box::new(SseFrame {
        id: Seq(20),
        session: fixture::session(),
        event: SseEvent::ApprovalPending {
            approval: ApprovalId::from_raw("appr-1"),
            short_id: ShortId::parse("7K2M").unwrap(),
        },
    })));
    // 通知只说"有新的"，所以它只换来一次清单查询：句柄、计数、可答结论都在清单里。
    assert_eq!(effects, vec![Effect::FetchPending]);
    assert!(app.approval.is_none(), "通知本身不足以弹窗");
}

/// 停在等待上的状态帧**顺手问一次清单**：弹窗等的是 `approval.requested`（补写的审计
/// 副本，§8.5），而它的到达时间不由界面决定；权威清单在 `GET /v1/interventions`。
#[test]
fn a_run_stopping_for_approval_asks_for_the_pending_list() {
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
fn model_with_no_argument_goes_and_asks_then_lists_what_came_back() {
    let mut app = app();
    // 清单是去问来的，不是启动时抄下来就不再更新的一份。
    assert_eq!(command(&mut app, "/model"), vec![Effect::FetchModels]);
    app.apply(ServerEvent::ModelMenu(menu()));
    let printed = notices(&app);
    assert!(
        printed.contains("gpt-x") && printed.contains("claude-y"),
        "{printed}"
    );
    // 顺带把当前模型能选的 effort 也印出来。
    assert!(printed.contains("low · high"), "{printed}");
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
fn approve_without_an_id_needs_exactly_one_pending_request() {
    let mut app = app();
    assert!(command(&mut app, "/approve").is_empty());
    assert!(notices(&app).contains("没有待处理的审批"));

    // 一条待处理：无 ID 生效。
    app.apply(pending_list(&["7K2M"]));
    let effects = command(&mut app, "/approve run");
    assert!(matches!(
        &effects[0],
        Effect::Answer {
            verdict: InterventionVerdict::Approve,
            scope: Some(ApprovalScope::Run),
            ..
        }
    ));

    // 两条待处理：要求指明。
    app.apply(pending_list(&["7K2M", "9QRS"]));
    assert!(command(&mut app, "/approve").is_empty());
    assert!(notices(&app).contains("请指明短 ID"), "{}", notices(&app));
    assert!(notices(&app).contains("9QRS"));
}

#[test]
fn the_command_palette_filters_while_the_slash_word_is_being_typed() {
    let mut app = app();
    type_text(&mut app, "/ap");
    assert_eq!(app.palette().len(), 1);
    assert_eq!(app.palette()[0].0, "/approve");
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.input.text(), "/approve");
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
    assert_eq!(tools[0].tool, "shell");
    assert_eq!(tools[0].summary(), "rm -rf build");
    assert_eq!(tools[0].state, ToolCallState::Completed);
    assert_eq!(tools[0].marker(), "ok");
    assert_eq!(tools[0].preview.as_deref(), Some("removed 12 files"));
    assert_eq!(tools[0].elapsed_ms, 340);
    assert_eq!(tools[0].attempts, 1);
}

#[test]
fn ctrl_t_expands_and_collapses_the_tool_calls() {
    let mut app = app();
    feed(&mut app, &fixture::conversation());
    assert!(!app.tool_lines()[0].expanded);
    app.handle_key(with(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert!(app.tool_lines()[0].expanded, "展开看参数与结果预览");
    app.handle_key(with(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert!(!app.tool_lines()[0].expanded);
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
    assert_eq!(tools[0].marker(), "??", "既不是 ok 也不是 !!");
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
    let approval = WaitReason::Approval {
        approval: ApprovalId::from_raw("appr-1"),
    };
    assert_eq!(wait_text(&approval, Some(now)), "等审批答复");

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
fn a_pending_approval_pops_the_moment_the_history_is_read() {
    let mut app = App::new(Some(fixture::session()), TuiMode::Resume, "seed");
    let mut events = fixture::conversation()[..4].to_vec();
    events.push(fixture::event(
        5,
        Some(fixture::run()),
        EventPayload::ApprovalRequested(komo_kernel::events::ApprovalRequested {
            approval: ApprovalId::from_raw("appr-1"),
            short_id: ShortId::parse("7K2M").unwrap(),
            plan_hash: fixture::plan().plan_hash(),
            call_id: Some(fixture::call()),
            reason: "要人看一眼".into(),
            scopes: vec![ApprovalScope::Once],
        }),
    ));
    app.apply(ServerEvent::HistoryPage(Box::new(fixture::page(
        events, false,
    ))));
    // 补读完之后**问一次清单**：待处理的可以属于别的会话，本会话的事件流里没有它们。
    assert_eq!(
        app.apply(ServerEvent::HistoryDone),
        vec![Effect::FetchPending]
    );

    // 清单回来了，第一条就弹（§8.8「打开即弹」）。
    let effects = app.apply(pending_list(&["7K2M"]));
    assert_eq!(
        effects,
        vec![Effect::FetchApproval("7K2M".into())],
        "打开即弹（§8.8）"
    );
    assert_eq!(app.pending_count(), 1);
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
            "7K2M",
            InterventionKind::Approval,
            "放行 rm -rf build？",
        )],
    };
    assert_eq!(resume_summary(&response), "2 个任务已接续，1 个等待审批");
}

#[test]
fn a_status_reply_says_what_is_running_and_how_many_wait() {
    let mut app = app();
    // 待处理数读的是**清单那一份**（三类合计、全部会话），不是会话详情里本会话那几条。
    app.apply(pending_list(&["7K2M"]));
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
// ---- 一次答一批（§11.3 的 `/approve all`）----

/// 待处理三条、弹窗开着时按 `a`：**一个请求答三条**，名单里第一条是眼前这条。
///
/// 这是"一个任务里连着几个命令"在界面上的样子：逐条答要按 N 次 `y`、等 N 次续跑。批量
/// 省的是按键——每一条仍然各自落一条决定（网关侧，§7.4）。
#[test]
fn a_batch_key_answers_everything_pending() {
    let mut app = app();
    app.apply(pending_list(&["7K2M", "9QRS", "3TVW"]));
    app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));
    assert_eq!(app.pending_count(), 3, "三条待处理");

    let effects = app.handle_key(key(KeyCode::Char('a')));
    let [
        Effect::AnswerMany {
            handles,
            approved: true,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("`a` 应当一次答一批：{effects:?}");
    };
    assert_eq!(handles.len(), 3, "{handles:?}");
    assert_eq!(
        handles[0],
        fixture::approval_record().short_id.as_str(),
        "眼前这条排第一：它就是弹窗上那一条"
    );

    // 再按一次不发第二个请求（已经答过，在等服务端回执）。
    assert!(app.handle_key(key(KeyCode::Char('a'))).is_empty());
}

/// 只有一条待处理时 `a` 仍然可用（和 `y` 同义），但**菜单里不列它**——列一行不必要
/// 的答案会让人以为还有什么没批。
#[test]
fn the_batch_key_is_harmless_when_only_one_is_waiting() {
    let mut app = app();
    app.apply(pending_list(&["7K2M"]));
    app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));

    let effects = app.handle_key(key(KeyCode::Char('a')));
    let [Effect::AnswerMany { handles, .. }] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    assert_eq!(handles.len(), 1, "{handles:?}");
    let rows = app.approval.as_ref().expect("弹窗").rows(1);
    assert!(
        !rows
            .iter()
            .any(|row| row.choice == ApprovalChoice::AllPending),
        "{rows:?}"
    );
}

/// `/approve all` 与 `/reject all`：命令行走同一条路，名单来自**服务端那份清单**。
#[test]
fn the_all_commands_go_through_the_same_batch() {
    let mut app = app();
    app.apply(pending_list(&["7K2M", "9QRS"]));
    app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));
    app.approval = None;

    let effects = command(&mut app, "/approve all");
    let [
        Effect::AnswerMany {
            approved: true,
            handles,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("{effects:?}");
    };
    assert_eq!(handles.len(), 2, "{handles:?}");

    let effects = command(&mut app, "/reject all");
    let [
        Effect::AnswerMany {
            approved: false,
            handles,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("{effects:?}");
    };
    assert_eq!(handles.len(), 2, "{handles:?}");
}

/// **自己答的那一下不能被说成"在别处批准了"。**
///
/// 决定会以 `approval_decided` 从 SSE 回来，它和"别人在别处答的"长得一模一样。少了这层
/// 区分，用户按下 `y` 的下一秒会看到"这条审批在别处批准了"——一句假话，刚好盖住他要看的
/// 回执。
#[test]
fn our_own_decision_is_not_reported_as_someone_elses() {
    let mut app = with_modal();
    let approval = fixture::approval_record().approval;
    let effects = app.handle_key(key(KeyCode::Char('y')));
    assert!(matches!(&effects[0], Effect::Answer { .. }));

    app.apply(ServerEvent::Frame(Box::new(SseFrame {
        id: Seq(50),
        session: fixture::session(),
        event: SseEvent::ApprovalDecided {
            approval,
            approved: true,
        },
    })));
    assert!(
        !notices(&app).contains("在别处"),
        "自己答的不能说成别处答的：{}",
        notices(&app)
    );
    assert!(app.approval.is_none(), "回执到了就收窗");
}

/// 别人在别处答的，仍然要说出来。
#[test]
fn someone_elses_decision_is_still_reported() {
    let mut app = with_modal();
    let approval = fixture::approval_record().approval;
    app.apply(ServerEvent::Frame(Box::new(SseFrame {
        id: Seq(51),
        session: fixture::session(),
        event: SseEvent::ApprovalDecided {
            approval,
            approved: false,
        },
    })));
    assert!(notices(&app).contains("在别处已拒绝"), "{}", notices(&app));
}

/// 待处理但**没有弹窗**时，输入框的提示要说出路——那是审批在界面上唯一还看得见的地方。
#[test]
fn a_pending_approval_without_a_popup_still_tells_you_how_to_answer() {
    let mut app = app();
    app.apply(pending_list(&["7K2M"]));
    // 详情取不回来的情形：弹窗没开，但清单里有它。
    app.approval = None;
    let hint = app.input_hint();
    assert!(hint.contains("待处理"), "{hint}");
    assert!(hint.contains("/approve"), "{hint}");
    assert!(hint.contains("/approve all"), "{hint}");
}

// ---- 三类共用一张清单（§7.5）----

/// `/pending` 的每一行都要**写清这一条该答什么**（§11.3）：句柄、种类、问题、可答结论。
#[test]
fn the_pending_list_prints_handle_kind_question_and_verdicts() {
    let mut app = app();
    app.apply(pending_mixed());
    let printed = notices(&app);
    assert!(printed.contains("7K2M"), "{printed}");
    assert!(printed.contains("审批"), "{printed}");
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
    assert!(printed.contains("放行 rm -rf build？"), "{printed}");
    // 三类合计进状态行/提示行。
    assert_eq!(app.pending_count(), 3);
    assert_eq!(app.pending_breakdown(), (1, 1, 1));
}

/// 清单里只有 `verify` / `blocked` 时**不弹窗**：弹窗是审批的主界面，另两类的出路是
/// `/pending` 与 `/answer`。
#[test]
fn only_an_approval_opens_the_popup() {
    let mut app = app();
    let effects = app.apply(ServerEvent::Pending(vec![
        fixture::intervention_summary("run-1", InterventionKind::Verify, "上次那个调用发生了没有"),
        fixture::intervention_summary("run-2", InterventionKind::Blocked, "会话已删除"),
    ]));
    assert!(effects.is_empty(), "没有审批可取详情：{effects:?}");
    assert!(app.approval.is_none());
}

/// 只有 `verify` / `blocked` 待处理时，提示行**用句柄与结论说出路**，不是只说"等待审批"。
#[test]
fn a_verify_pending_tells_you_the_handle_and_the_verdicts() {
    let mut app = app();
    app.apply(ServerEvent::Pending(vec![fixture::intervention_summary(
        "run-1",
        InterventionKind::Verify,
        "上次那个调用发生了没有",
    )]));
    let hint = app.input_hint();
    assert!(hint.contains("/answer <句柄> <结论>"), "{hint}");
    assert!(hint.contains("satisfied"), "{hint}");
    assert!(hint.contains("not_performed"), "{hint}");
    assert!(hint.contains("resolve"), "{hint}");
    assert!(
        !hint.contains("/approve"),
        "没有审批可批时别提 /approve：{hint}"
    );
}

/// 弹窗开着而清单上**还有别的类**时，提示行要说得出它们也有出路——那是它们在界面上
/// 唯一还看得见的地方。
#[test]
fn the_popup_hint_mentions_the_other_kinds() {
    let mut app = app();
    app.apply(pending_mixed());
    app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));
    let hint = app.input_hint();
    assert!(hint.contains("待批准 1 条"), "{hint}");
    assert!(hint.contains("另有 2 条"), "{hint}");
    assert!(hint.contains("/answer"), "{hint}");
    assert!(hint.contains("Enter 确认"), "{hint}");
}

/// `/answer <句柄> <结论>` 答另外两类：结论按种类分派（§7.5），范围只有审批用得上。
#[test]
fn answer_sends_the_verdict_for_the_handle_it_was_given() {
    let mut app = app();
    app.apply(pending_mixed());
    // 弹窗别挡着输入（这一条测的是命令行，不是弹窗）。
    app.approval = None;

    let effects = command(&mut app, "/answer run-1 satisfied");
    let [
        Effect::Answer {
            handle,
            verdict,
            scope,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("{effects:?}");
    };
    assert_eq!(handle, "run-1");
    assert_eq!(*verdict, InterventionVerdict::Satisfied);
    assert_eq!(*scope, None, "范围是授权的事，只有 approve 用得上");

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

    // 审批走同一条路（`/approve` 就是它的两个结论）。
    let effects = command(&mut app, "/answer 7K2M approve");
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Answer {
                verdict: InterventionVerdict::Approve,
                ..
            }]
        ),
        "{effects:?}"
    );
}

/// 给错种类的结论**说出来**，而不是发一个服务端只能拒绝的请求（§7.5：结论按种类分派）。
#[test]
fn a_verdict_that_does_not_belong_to_the_kind_is_refused_locally() {
    let mut app = app();
    app.apply(pending_mixed());
    app.approval = None;

    // `approve` 是审批的结论，答不了 `verify`。
    assert!(command(&mut app, "/answer run-1 approve").is_empty());
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
    app.approval = None;
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

/// 批量**只批审批**：一批互不相干的事项共用一个结论只能是替操作者猜（§11.3）。
#[test]
fn the_all_command_answers_only_the_approvals() {
    let mut app = app();
    app.apply(pending_mixed());
    app.approval = None;

    let effects = command(&mut app, "/approve all");
    let [Effect::AnswerMany { handles, .. }] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    assert_eq!(handles, &vec!["7K2M".to_string()], "只有审批那一条");

    // 一条审批都没有时，明说没有——别静默什么都不做。
    let mut only_verify = App::new(Some(fixture::session()), TuiMode::New, "seed");
    only_verify.apply(ServerEvent::Pending(vec![fixture::intervention_summary(
        "run-1",
        InterventionKind::Verify,
        "上次那个调用发生了没有",
    )]));
    assert!(command(&mut only_verify, "/approve all").is_empty());
    assert!(
        notices(&only_verify).contains("没有待处理的审批"),
        "{}",
        notices(&only_verify)
    );
}

/// 只答一条时，`/approve` 找的是**审批类**：短 ID 是审批的句柄，它管不到另外两类。
#[test]
fn a_short_id_that_belongs_to_another_kind_is_not_an_approval() {
    let mut app = app();
    app.apply(ServerEvent::Pending(vec![fixture::intervention_summary(
        "run-1",
        InterventionKind::Verify,
        "上次那个调用发生了没有",
    )]));
    app.approval = None;
    // `run-1` 是句柄，不是短 ID，而且它那一类也答不了 approve。
    assert!(command(&mut app, "/approve run-1").is_empty());
    assert!(notices(&app).contains("短 ID"), "{}", notices(&app));
}

/// 自己答过的那一条之后收到的 `approval_decided`，仍然不能被说成"在别处答的"。
///
/// SSE 帧里只有审批 id、没有句柄，所以"别处答掉的那一条"没法直接从清单里摘掉——问一次
/// 清单，让权威那一份说话。
#[test]
fn a_decided_frame_for_something_else_refreshes_the_list() {
    let mut app = app();
    app.apply(pending_mixed());
    app.approval = None;
    let effects = app.apply(ServerEvent::Frame(Box::new(SseFrame {
        id: Seq(60),
        session: fixture::session(),
        event: SseEvent::ApprovalDecided {
            approval: ApprovalId::from_raw("appr-other"),
            approved: true,
        },
    })));
    assert_eq!(effects, vec![Effect::FetchPending]);
}

/// 回执那句话由服务端给（`note`）：同一句结论的措辞不该在四个界面里各写一遍。
#[test]
fn an_answer_receipt_prints_the_note_and_drops_it_from_the_list() {
    let mut app = app();
    app.apply(pending_mixed());
    app.approval = None;

    app.apply(ServerEvent::Answered(Box::new(
        komo_kernel::protocol::http::InterventionAnswerResponse {
            handle: "run-1".into(),
            kind: InterventionKind::Verify,
            verdict: InterventionVerdict::Satisfied,
            decision: None,
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
    assert_eq!(app.pending_count(), 2, "答过的那条要从清单里下去");
    assert!(app.pending_item("run-1").is_none());

    // 早就答过的那一条：回执照样说清楚这次没改变什么。
    app.apply(ServerEvent::Answered(Box::new(
        komo_kernel::protocol::http::InterventionAnswerResponse {
            handle: "run-2".into(),
            kind: InterventionKind::Blocked,
            verdict: InterventionVerdict::Resolve,
            decision: None,
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

/// **别的会话的待审批，打开 TUI 也看得见。**
///
/// 审批可以属于定时任务那条会话、聊天里那条、别的 TUI 里开的那个，而本会话的事件流里
/// 没有它们——清单只能问服务端。少了这一步，"打开 TUI 一条待审批也看不到"与
/// "`/approve all` 说没有待处理、而 `GET /v1/approvals` 列着三条"会同时成立。
#[test]
fn approvals_from_other_sessions_are_visible_here() {
    let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
    assert_eq!(app.pending_count(), 0);

    // 本会话一条 `approval.requested` 都没有——清单照样把三条带回来。
    let effects = app.apply(pending_list(&["7K2M", "9QRS", "3TVW"]));
    assert_eq!(app.pending_count(), 3);
    assert!(
        matches!(effects.as_slice(), [Effect::FetchApproval(_)]),
        "清单回来就该弹第一条：{effects:?}"
    );
    let hint = app.input_hint();
    assert!(hint.contains('3'), "提示里要写清有几条：{hint}");
}
