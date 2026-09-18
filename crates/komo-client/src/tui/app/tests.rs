//! `App` 状态机的测试：按键序列 / 服务端事件 → 状态断言，不需要终端。

use super::*;
use crate::sse::ConnectionState;
use crate::tui::paste::{InputEvent, PASTE_MIN_BYTES, PasteChip};
use crate::tui::test_support as fixture;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::http::{ModelMenuEntry, RunSummary, SessionSummary};
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::types::ids::{ApprovalId, RunId, Seq, ShortId};
use komo_kernel::types::model::Effort;
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::status::{RunStatus, ToolCallState};

fn app() -> App {
    App::new(fixture::session(), TuiMode::New, "seed")
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

/// 待处理清单按它**真正来的路**进来：`GET /v1/approvals` 的那一份（§13.1）。
///
/// 不是从本会话的 `approval.requested` 折出来的那份——审批可以属于别的会话，操作者在
/// TUI 里要看到的正是"现在有谁在等我"，所以清单只能问服务端。
fn pending_list(records: &[(&str, &str)]) -> ServerEvent {
    ServerEvent::Pending(
        records
            .iter()
            .map(|(approval, short)| {
                let mut record = fixture::approval_record();
                record.approval = ApprovalId::from_raw(*approval);
                record.short_id = ShortId::parse(short).expect("短 ID");
                record
            })
            .collect(),
    )
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
    assert_eq!(app.run_status(), Some(RunStatus::Running));

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
    assert_eq!(app.run_status(), Some(RunStatus::Completed));
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
        Effect::Decide {
            approved: true,
            scope: ApprovalScope::Once,
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
        Effect::Decide {
            approved: true,
            scope: ApprovalScope::Run,
            ..
        }
    ));

    // 一条只可批本次的请求：`r` 不生效，而且说出为什么。
    let mut record = fixture::approval_record();
    record.scopes = vec![ApprovalScope::Once];
    let mut app = App::new(fixture::session(), TuiMode::New, "seed");
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
                Effect::Decide {
                    approved: false,
                    ..
                }
            ),
            "{code:?} 应当拒绝：{effects:?}"
        );
    }
}

#[test]
fn the_input_is_disabled_while_an_approval_is_open() {
    let mut app = with_modal();
    assert!(!app.input_enabled());
    assert!(app.input_hint().contains("批准"));
    // 打字不进输入框——y / n / r 才是这时候的键。
    type_text(&mut app, "abc");
    assert!(app.input.is_empty());
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
    // §13.1：详情去 GET /v1/approvals/{id} 取。
    let mut app = app();
    let effects = app.apply(ServerEvent::Frame(Box::new(SseFrame {
        id: Seq(20),
        session: fixture::session(),
        event: SseEvent::ApprovalPending {
            approval: ApprovalId::from_raw("appr-1"),
            short_id: ShortId::parse("7K2M").unwrap(),
        },
    })));
    assert_eq!(
        effects,
        vec![
            Effect::FetchApproval(ApprovalId::from_raw("appr-1")),
            // 顺带把清单问一遍：计数与 `a` 的名单要它。
            Effect::FetchPending,
        ]
    );
    assert!(app.approval.is_none(), "通知本身不足以弹窗");
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
    app.apply(pending_list(&[("appr-1", "7K2M")]));
    let effects = command(&mut app, "/approve run");
    assert!(matches!(
        &effects[0],
        Effect::Decide {
            approved: true,
            scope: ApprovalScope::Run,
            ..
        }
    ));

    // 两条待处理：要求指明。
    app.apply(pending_list(&[("appr-1", "7K2M"), ("appr-2", "9QRS")]));
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
fn every_run_status_has_a_name_and_they_are_all_different() {
    let all = [
        RunStatus::Ingesting,
        RunStatus::Queued,
        RunStatus::Running,
        RunStatus::WaitingApproval,
        RunStatus::WaitingRetry,
        RunStatus::Interrupted,
        RunStatus::NeedsAttention,
        RunStatus::Completed,
        RunStatus::Failed,
        RunStatus::Cancelled,
    ];
    let mut names: Vec<&str> = all.iter().map(|s| status_text(*s)).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all.len(), "十个状态如实显示，不合并");
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
    let mut app = App::new(fixture::session(), TuiMode::Resume, "seed");
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
    assert_eq!(app.run_status(), Some(RunStatus::Completed));

    // 这时候才发得出去。
    app.input.set("接着说");
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter))[0],
        Effect::Submit { .. }
    ));
}

#[test]
fn a_pending_approval_pops_the_moment_the_history_is_read() {
    let mut app = App::new(fixture::session(), TuiMode::Resume, "seed");
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
    let effects = app.apply(pending_list(&[("appr-1", "7K2M")]));
    assert_eq!(
        effects,
        vec![Effect::FetchApproval(ApprovalId::from_raw("appr-1"))],
        "打开即弹（§8.8）"
    );
    assert_eq!(app.pending_count(), 1);
}

#[test]
fn a_new_session_does_not_wait_for_a_backfill() {
    let app = App::new(fixture::session(), TuiMode::New, "seed");
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
                status: RunStatus::Queued,
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
                status: RunStatus::Queued,
                source: komo_kernel::types::plan::PlanSource::Interactive {
                    session: fixture::session(),
                },
                rounds: 0,
                created_at: fixture::T0,
                ended_at: None,
            },
        ],
        pending: vec![PendingItem::Approval(Box::new(fixture::approval_record()))],
    };
    assert_eq!(resume_summary(&response), "2 个任务已接续，1 个等待审批");
}

#[test]
fn a_status_reply_says_what_is_running_and_how_many_wait() {
    let mut app = app();
    // 待审批数读的是**清单那一份**（全部会话），不是会话详情里本会话那几条。
    app.apply(pending_list(&[("appr-1", "7K2M")]));
    app.apply(ServerEvent::Status(Box::new(SessionDetail {
        summary: SessionSummary {
            session: fixture::session(),
            title: "清理".into(),
            workdir: None,
            current_run: Some(fixture::run()),
            current_status: Some(RunStatus::WaitingApproval),
            applied_seq: Seq(9),
            created_at: fixture::T0,
            updated_at: fixture::T0,
        },
        unfinished: vec![],
        pending_approvals: vec![],
    })));
    assert!(notices(&app).contains("等待审批"), "{}", notices(&app));
    assert!(notices(&app).contains("待审批 1"), "{}", notices(&app));
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
    app.apply(pending_list(&[
        ("appr-1", "7K2M"),
        ("appr-2", "9QRS"),
        ("appr-3", "3TVW"),
    ]));
    app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));
    assert_eq!(app.pending_count(), 3, "三条待处理");

    let effects = app.handle_key(key(KeyCode::Char('a')));
    let [
        Effect::DecideMany {
            approvals,
            approved: true,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("`a` 应当一次答一批：{effects:?}");
    };
    assert_eq!(approvals.len(), 3, "{approvals:?}");
    assert_eq!(
        approvals[0].as_str(),
        fixture::approval_record().approval.as_str(),
        "眼前这条排第一：它就是弹窗上那一条"
    );

    // 再按一次不发第二个请求（已经答过，在等服务端回执）。
    assert!(app.handle_key(key(KeyCode::Char('a'))).is_empty());
}

/// 只有一条待处理时 `a` 仍然可用（和 `y` 同义），但**按键提示里不列它**——列一个不必
/// 要的键会让人以为还有什么没批。
#[test]
fn the_batch_key_is_harmless_when_only_one_is_waiting() {
    let mut app = app();
    app.apply(pending_list(&[("appr-1", "7K2M")]));
    app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));

    let effects = app.handle_key(key(KeyCode::Char('a')));
    let [Effect::DecideMany { approvals, .. }] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    let hint = app.approval.as_ref().expect("弹窗").keys_hint(1);
    assert!(!hint.contains("全部批准"), "{hint}");
}

/// `/approve all` 与 `/reject all`：命令行走同一条路，名单来自**服务端那份清单**。
#[test]
fn the_all_commands_go_through_the_same_batch() {
    let mut app = app();
    app.apply(pending_list(&[("appr-1", "7K2M"), ("appr-2", "9QRS")]));
    app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));
    app.approval = None;

    let effects = command(&mut app, "/approve all");
    let [
        Effect::DecideMany {
            approved: true,
            approvals,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("{effects:?}");
    };
    assert_eq!(approvals.len(), 2, "{approvals:?}");

    let effects = command(&mut app, "/reject all");
    let [
        Effect::DecideMany {
            approved: false,
            approvals,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("{effects:?}");
    };
    assert_eq!(approvals.len(), 2, "{approvals:?}");
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
    assert!(matches!(&effects[0], Effect::Decide { .. }));

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

/// 待批复但**没有弹窗**时，输入框的提示要说出路——那是审批在界面上唯一还看得见的地方。
#[test]
fn a_pending_approval_without_a_popup_still_tells_you_how_to_answer() {
    let mut app = app();
    app.apply(pending_list(&[("appr-1", "7K2M")]));
    // 详情取不回来的情形：弹窗没开，但清单里有它。
    app.approval = None;
    let hint = app.input_hint();
    assert!(hint.contains("待批复"), "{hint}");
    assert!(hint.contains("/approve"), "{hint}");
    assert!(hint.contains("/approve all"), "{hint}");
}

/// **别的会话的待审批，打开 TUI 也看得见。**
///
/// 审批可以属于定时任务那条会话、聊天里那条、别的 TUI 里开的那个，而本会话的事件流里
/// 没有它们——清单只能问服务端。少了这一步，"打开 TUI 一条待审批也看不到"与
/// "`/approve all` 说没有待处理、而 `GET /v1/approvals` 列着三条"会同时成立。
#[test]
fn approvals_from_other_sessions_are_visible_here() {
    let mut app = App::new(fixture::session(), TuiMode::New, "seed");
    assert_eq!(app.pending_count(), 0);

    // 本会话一条 `approval.requested` 都没有——清单照样把三条带回来。
    let effects = app.apply(pending_list(&[
        ("appr-1", "7K2M"),
        ("appr-2", "9QRS"),
        ("appr-3", "3TVW"),
    ]));
    assert_eq!(app.pending_count(), 3);
    assert!(
        matches!(effects.as_slice(), [Effect::FetchApproval(_)]),
        "清单回来就该弹第一条：{effects:?}"
    );
    let hint = app.input_hint();
    assert!(hint.contains('3'), "提示里要写清有几条：{hint}");
}
