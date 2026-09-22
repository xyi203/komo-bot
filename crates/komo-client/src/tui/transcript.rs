//! 会话正文：哪些**已经定型**，哪些**还在动**。
//!
//! 这是 inline 视口的全部要点。终端底下留着一小块视口（状态行 + 输入框 + 一小段还在
//! 动的正文），视口**以上**的每一行都是终端自己的——它进了终端的回滚区，滚轮能滚、鼠标
//! 能选中复制，而且**只画一次**，不随每一帧重算。
//!
//! 于是正文分成两半：
//!
//! - **定型**（[`Emitted::take`]）：用户那条消息、助手那段回答、跑完的一次工具调用、
//!   一条提示。交出去一次，之后再也不动——所以判「定型」要保守，宁可多在视口里待一帧。
//! - **还在动**（[`Emitted::live`]）：还没收到 `run.accepted` 的那条提交、模型正在打字
//!   的草稿、还没跑完的工具调用。每一帧重画，永不交出去。
//!
//! 两边**不重叠**：一条东西定型的那一帧，它从「还在动」里消失、同时出现在回滚区上，所以
//! 屏幕上不会有两份。判重的记号在 [`Emitted`] 里，不在 [`App`] 里——那是终端这一侧的
//! 账，状态机不该知道。

use std::collections::BTreeSet;

use komo_kernel::fold::SurfaceMessage;
use komo_kernel::types::ids::{Seq, ToolCallId};
use komo_kernel::types::status::ToolCallState;
use komo_kernel::types::turn::Role;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use time::OffsetDateTime;

use crate::tui::app::{App, Notice, SubmissionState, ToolLine};
use crate::tui::{markdown, spinner};

/// 用户那一条的行首记号。
pub const USER_MARK: &str = "-> ";

/// 交到哪儿了。一次 TUI 一份，活在驱动里。
#[derive(Debug, Default)]
pub struct Emitted {
    /// `surface.messages` 交出去了多少条（消息只追加，前缀长度就够）。
    messages: usize,
    /// 交出去的工具调用。它们**不按顺序定型**（并发的只读调用谁先完谁先落地），所以记
    /// 的是名字不是前缀。
    tools: BTreeSet<ToolCallId>,
    /// 交出去的最后一条提示的编号。
    notices: u64,
}

impl Emitted {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取走**刚定型、还没交出去**的那些行，并记下已经交了。
    ///
    /// 顺序按**它是在哪一条事件上出现的**（`seq`）排，不按定型的先后：一次工具调用的
    /// `tool.planned` 排在请求它的那条助手消息之后、下一轮回答之前，所以哪怕
    /// `tool.result` 和下一条 `message.assistant` 在同一批里到达，回滚区上的顺序也仍是
    /// 「先答话、再跑工具、再答话」。
    pub fn take(&mut self, app: &App, width: u16) -> Vec<Line<'static>> {
        let mut items: Vec<(Seq, Vec<Line<'static>>)> = Vec::new();

        let messages = app.messages();
        for message in messages.iter().skip(self.messages) {
            let lines = message_lines(message, width);
            if !lines.is_empty() {
                items.push((message.seq, lines));
            }
        }
        self.messages = messages.len();

        for tool in app.tool_lines() {
            if self.tools.contains(&tool.call) || !self.settled(app, tool) {
                continue;
            }
            self.tools.insert(tool.call.clone());
            // 交出去的行冻在纸上：**不给钟**，否则那一格会定格在某一帧随机的样子。
            items.push((tool.seq, tool_lines(tool, app.tool_detail, width, None)));
        }

        items.sort_by_key(|(seq, _)| *seq);
        let mut out: Vec<Line<'static>> = items
            .into_iter()
            .flat_map(|(_, lines)| lines)
            .collect::<Vec<_>>();

        for notice in &app.notices {
            if notice.id <= self.notices {
                continue;
            }
            self.notices = notice.id;
            out.extend(notice_lines(notice, width));
        }
        out
    }

    /// 还在动的那些，每一帧重画。
    pub fn live(&self, app: &App, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();

        // Enter 之后立刻显示，不等 SSE 绕一圈。权威的 `run.accepted` 到达时状态机会移除
        // 对应项，那一刻它从这里消失、同时以折出来的用户消息落进回滚区。
        for pending in &app.pending_submissions {
            lines.extend(user_lines(&pending.text, width));
            let (label, style) = match &pending.state {
                SubmissionState::Sending => (
                    "  ↥ 发送中…".to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::DIM),
                ),
                SubmissionState::Submitted { deduplicated, .. } if *deduplicated => (
                    "  ✓ 已存在，等待事件同步…".to_string(),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                ),
                SubmissionState::Submitted { .. } => (
                    "  ✓ 已提交，等待事件同步…".to_string(),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                ),
                SubmissionState::Failed { error } => (
                    format!("  !! 发送失败：{error}"),
                    Style::default().fg(Color::Red),
                ),
            };
            for piece in markdown::wrap_to_width(&label, width as usize) {
                lines.push(Line::from(Span::styled(piece, style)));
            }
        }

        // 模型正在打字的那一段：**不是历史**（`assistant_delta` 只在 SSE 上，永不进
        // JSONL）。`message.assistant` 到达时它就地清掉，正文由那一条落进回滚区。
        if let Some(draft) = &app.draft {
            lines.extend(markdown::render(&draft.text, width));
            lines.push(Line::from(Span::styled(
                format!("{} 生成中", spinner::frame(app.now)),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
            )));
        }

        for tool in app.tool_lines() {
            if self.tools.contains(&tool.call) {
                continue;
            }
            lines.extend(tool_lines(tool, app.tool_detail, width, app.now));
        }
        lines
    }

    /// 这一次调用定型了没有。
    ///
    /// 跑完是一种，**这条 Run 已经结束**是另一种：取消掉的那一轮里有调用会永远停在
    /// `started` 上（§8.6 的 uncertain 也走这条），没有第二条它就永远留在视口里。
    fn settled(&self, app: &App, tool: &ToolLine) -> bool {
        if tool.state.is_terminal() {
            return true;
        }
        tool.run
            .as_ref()
            .and_then(|run| app.surface.runs.get(run))
            .is_some_and(|view| view.status.is_terminal())
    }
}

/// 一条消息的正文。**不含它请求的工具调用**：那些各自定型、各自落地。
fn message_lines(message: &SurfaceMessage, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match message.role {
        Role::User => {
            if let Some(text) = message.text.as_deref() {
                lines.extend(user_lines(text, width));
            } else if message.text_ref.is_some() {
                lines.push(dim(&format!("{USER_MARK}（正文已外置）")));
            }
        }
        Role::Assistant => {
            if let Some(text) = message.text.as_deref().filter(|t| !t.trim().is_empty()) {
                lines.extend(markdown::render(text, width));
            }
        }
        // 工具结果在调用那一行上显示，不另占一个消息节点。
        Role::Tool => {}
    }
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// 一次工具调用的那一行（`detail` 打开时再加参数与结果预览）。
///
/// `now` 是 `None` 就不转——那是要冻进回滚区的行。
pub fn tool_lines(
    tool: &ToolLine,
    detail: bool,
    width: u16,
    now: Option<OffsetDateTime>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let head = format!("  {} {} {}", tool.marker(now), tool.tool, tool.summary());
    lines.push(Line::from(Span::styled(
        markdown::truncate_to_width(&head, width as usize),
        Style::default().fg(state_colour(tool.state)),
    )));
    if tool.elapsed_ms > 0 && tool.state.is_terminal() {
        lines.push(dim(&format!("    {} ms", tool.elapsed_ms)));
    }
    if detail {
        let args = serde_json::to_string_pretty(&tool.args).unwrap_or_default();
        for line in args.lines() {
            lines.push(Line::from(Span::styled(
                markdown::truncate_to_width(&format!("    {line}"), width as usize),
                Style::default().fg(Color::DarkGray),
            )));
        }
        if let Some(preview) = &tool.preview {
            lines.push(dim("    ── 结果预览 ──"));
            for line in preview.lines() {
                lines.push(Line::from(Span::styled(
                    markdown::truncate_to_width(&format!("    {line}"), width as usize),
                    Style::default().fg(Color::Gray),
                )));
            }
        }
    }
    lines
}

fn notice_lines(notice: &Notice, width: u16) -> Vec<Line<'static>> {
    let style = if notice.is_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    // 折出来的续行对齐在记号下面——每行都顶一个 `·`，看上去就成了好几条提示。
    wrap_body(&notice.text, width.saturating_sub(2) as usize)
        .into_iter()
        .enumerate()
        .map(|(index, piece)| {
            let head = if index == 0 { "· " } else { "  " };
            Line::from(Span::styled(format!("{head}{piece}"), style))
        })
        .collect()
}

pub fn state_colour(state: ToolCallState) -> Color {
    match state {
        ToolCallState::Planned => Color::DarkGray,
        ToolCallState::Started => Color::Yellow,
        ToolCallState::Completed => Color::Green,
        ToolCallState::Failed => Color::Red,
        // uncertain 不是失败的一种颜色——它是「不知道」，所以给它自己的颜色（§8.6）。
        ToolCallState::Uncertain => Color::Magenta,
    }
}

/// 用户那一条：行首一个 `->`，折出来的续行对齐在它下面。
fn user_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let mark = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    wrap_body(text, body_width(width))
        .into_iter()
        .enumerate()
        .map(|(index, piece)| {
            let head = if index == 0 {
                Span::styled(USER_MARK, mark)
            } else {
                Span::raw(" ".repeat(markdown::display_width(USER_MARK)))
            };
            Line::from(vec![head, Span::raw(piece)])
        })
        .collect()
}

/// 折行，**先按真正的换行切开**。
///
/// 少了第一步，一段多行正文会被当成一整行拿去折，换行原样留在行里——交给终端就是一级
/// 一级往右掉的楼梯（回滚区那一段不经过 ratatui 的缓冲区，没人替它吃掉那个换行）。
fn wrap_body(text: &str, width: usize) -> Vec<String> {
    text.split('\n')
        .flat_map(|line| markdown::wrap_to_width(line, width))
        .collect()
}

/// 正文的可用宽度：行首那个记号占掉的列不算。
fn body_width(width: u16) -> usize {
    (width as usize).saturating_sub(markdown::display_width(USER_MARK))
}

fn dim(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(Color::DarkGray),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Effect, ServerEvent, TuiMode};
    use crate::tui::test_support as fixture;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use komo_kernel::events::Event;
    use komo_kernel::protocol::sse::{SseEvent, SseFrame};

    fn feed(app: &mut App, events: &[Event]) {
        for event in events {
            app.apply(ServerEvent::Frame(Box::new(SseFrame {
                id: event.seq,
                session: event.session.clone(),
                event: SseEvent::Event(Box::new(event.clone())),
            })));
        }
    }

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 提问用 `->` 开头，不是「你」——这是一行命令提示符，不是一句对白。
    #[test]
    fn a_question_is_marked_with_an_arrow() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        feed(&mut app, &fixture::conversation());
        let mut emitted = Emitted::new();
        let settled = text_of(&emitted.take(&app, 80));
        assert!(settled.contains("-> 把 build 目录清掉"), "{settled}");
        assert!(!settled.contains("你 "), "{settled}");
    }

    /// 一段多行正文要折成**一行一条**。
    ///
    /// 交给回滚区的行是直接写给终端的，行里留一个换行，屏幕上就是一级一级往右掉的楼梯。
    #[test]
    fn a_multi_line_question_becomes_one_line_each() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        app.input.set("第一行\n第二行\n第三行");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let emitted = Emitted::new();
        let live = emitted.live(&app, 80);
        let body: Vec<&Line<'static>> = live
            .iter()
            .filter(|line| text_of(std::slice::from_ref(*line)).contains('行'))
            .collect();
        assert_eq!(body.len(), 3, "三行正文三条：{:?}", text_of(&live));
        for line in live.iter() {
            for span in &line.spans {
                assert!(
                    !span.content.contains('\n'),
                    "行里不许留换行：{:?}",
                    span.content
                );
            }
        }
        // 第一行带记号，续行对齐在它下面。
        assert!(text_of(std::slice::from_ref(body[0])).starts_with(USER_MARK));
        assert!(text_of(std::slice::from_ref(body[1])).starts_with("   第二行"));
    }

    /// **交出去一次就不再交第二次**：回滚区上的行是终端的，重复一次就是屏幕上出现两份。
    #[test]
    fn nothing_is_handed_to_the_scrollback_twice() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        feed(&mut app, &fixture::conversation());
        let mut emitted = Emitted::new();
        let first = emitted.take(&app, 80);
        assert!(!first.is_empty());
        assert!(
            emitted.take(&app, 80).is_empty(),
            "同一段内容第二次还交，屏幕上就有两份"
        );
    }

    /// 定型的那一帧，它要**同时**从视口里消失。两边都画 = 屏幕上两份。
    #[test]
    fn a_finished_tool_call_leaves_the_live_area_the_moment_it_lands() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        let events = fixture::conversation();
        let mut emitted = Emitted::new();

        // 先喂到「调用开跑」为止：它这时还在动，只该出现在视口里。
        feed(&mut app, &events[..5]);
        let _ = emitted.take(&app, 80);
        let live = text_of(&emitted.live(&app, 80));
        assert!(live.contains("shell"), "跑着的调用要在视口里：{live}");

        feed(&mut app, &events[5..]);
        let settled = text_of(&emitted.take(&app, 80));
        assert!(settled.contains("shell"), "跑完了要落进回滚区：{settled}");
        let live = text_of(&emitted.live(&app, 80));
        assert!(!live.contains("shell"), "落地之后不该还在视口里：{live}");
    }

    /// 取消掉的那一轮里，停在 `started` 上的调用也要落地——否则它永远占着视口。
    #[test]
    fn a_call_left_hanging_by_a_cancelled_run_still_lands() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        let events = fixture::conversation();
        feed(&mut app, &events[..5]);
        let mut emitted = Emitted::new();
        let _ = emitted.take(&app, 80);
        assert!(!emitted.live(&app, 80).is_empty());

        feed(
            &mut app,
            &[fixture::event(
                99,
                Some(fixture::run()),
                komo_kernel::events::EventPayload::RunCancelled(
                    komo_kernel::events::RunCancelled { by: None },
                ),
            )],
        );
        assert!(
            !emitted.take(&app, 80).is_empty(),
            "Run 结束了，停在半路的调用也该落地"
        );
        assert!(emitted.live(&app, 80).is_empty());
    }

    /// 还没收到回执的那条提交只在视口里；回执到了由折出来的那条落地，不重复。
    #[test]
    fn a_pending_submission_lives_only_in_the_viewport() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        app.input.set("刚发出去的消息");
        let effects = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(effects.as_slice(), [Effect::Submit { .. }]));

        let mut emitted = Emitted::new();
        assert!(emitted.take(&app, 80).is_empty());
        let live = text_of(&emitted.live(&app, 80));
        assert!(live.contains("刚发出去的消息"), "{live}");
        assert!(live.contains("发送中"), "{live}");
    }

    /// 提示按编号交，**裁表不会让它重来一遍**：`notices` 留最后 50 条，下标会错位。
    #[test]
    fn notices_are_handed_over_by_id_so_trimming_cannot_replay_them() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        let mut emitted = Emitted::new();
        for index in 0..60 {
            app.note(format!("第 {index} 条"));
        }
        let first = text_of(&emitted.take(&app, 80));
        assert!(first.contains("第 59 条"), "{first}");
        app.note("再来一条");
        let second = text_of(&emitted.take(&app, 80));
        assert_eq!(second.matches("再来一条").count(), 1);
        assert!(!second.contains("第 59 条"), "交过的不再交：{second}");
    }
}
