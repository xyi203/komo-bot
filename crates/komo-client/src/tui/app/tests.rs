//! `App` 状态机的测试：按键序列 / 服务端事件 → 状态断言，不需要终端。

use super::*;
use crate::tui::paste::{InputEvent, PASTE_MIN_BYTES, PasteChip};
use crate::tui::test_support as fixture;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::http::{RunSummary, SessionSummary};
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::types::ids::{ApprovalId, RunId, Seq, ShortId};
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
        vec![Effect::FetchApproval(ApprovalId::from_raw("appr-1"))]
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

#[test]
fn model_with_no_argument_lists_the_menu() {
    let mut app = app();
    app.apply(ServerEvent::ModelMenu(vec![
        "gpt-x".into(),
        "claude-y".into(),
    ]));
    command(&mut app, "/model");
    let printed = notices(&app);
    assert!(
        printed.contains("gpt-x") && printed.contains("claude-y"),
        "{printed}"
    );
}

#[test]
fn a_model_outside_the_menu_is_refused_with_the_menu_spelled_out() {
    let mut app = app();
    app.apply(ServerEvent::ModelMenu(vec!["gpt-x".into()]));
    assert!(command(&mut app, "/model gpt-z").is_empty());
    assert!(notices(&app).contains("gpt-x"), "{}", notices(&app));
    assert!(app.model.is_none());
}

#[test]
fn model_says_so_when_the_gateway_offered_no_menu() {
    let mut app = app();
    command(&mut app, "/model");
    assert!(
        notices(&app).contains("没有提供模型清单"),
        "{}",
        notices(&app)
    );
}

#[test]
fn effort_with_no_argument_lists_its_options() {
    let mut app = app();
    command(&mut app, "/effort");
    let printed = notices(&app);
    for level in command::EFFORT_LEVELS {
        assert!(printed.contains(level), "{printed}");
    }
}

#[test]
fn approve_without_an_id_needs_exactly_one_pending_request() {
    let mut app = app();
    assert!(command(&mut app, "/approve").is_empty());
    assert!(notices(&app).contains("没有待处理的审批"));

    // 一条待处理：无 ID 生效。
    feed(
        &mut app,
        &[fixture::event(
            10,
            Some(fixture::run()),
            EventPayload::ApprovalRequested(komo_kernel::events::ApprovalRequested {
                approval: ApprovalId::from_raw("appr-1"),
                short_id: ShortId::parse("7K2M").unwrap(),
                plan_hash: fixture::plan().plan_hash(),
                call_id: Some(fixture::call()),
                reason: "要人看一眼".into(),
                scopes: vec![ApprovalScope::Once, ApprovalScope::Run],
            }),
        )],
    );
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
    feed(
        &mut app,
        &[fixture::event(
            11,
            Some(fixture::run()),
            EventPayload::ApprovalRequested(komo_kernel::events::ApprovalRequested {
                approval: ApprovalId::from_raw("appr-2"),
                short_id: ShortId::parse("9QRS").unwrap(),
                plan_hash: fixture::plan().plan_hash(),
                call_id: None,
                reason: "另一件".into(),
                scopes: vec![ApprovalScope::Once],
            }),
        )],
    );
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
    let effects = app.apply(ServerEvent::HistoryDone);
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
        pending_approvals: vec![fixture::approval_record()],
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
