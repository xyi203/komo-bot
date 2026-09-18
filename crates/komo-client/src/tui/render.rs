//! TUI 绘制与 Markdown 渲染（§13.4）。
//!
//! **渲染层薄**：这里没有一个 `if` 决定业务的事。它读 [`App`] 的状态，摆四块地方——
//! 身份行、消息面、状态行、输入框——需要时在上面盖一个审批弹窗。
//!
//! 布局：
//!
//! ```text
//! ┌ 身份行（会话 / 模式 / 工作目录）
//! │ 消息面（用户 / 助手 Markdown / 工具调用 / 提示）
//! ├ 状态行（Run 状态 · 本轮耗时 · 模型 · effort · 连接状态 · 待审批数）
//! │ 命令面板（只在输入以 `/` 开头时出现）
//! └ 输入框
//! ```

use komo_kernel::fold::SurfaceMessage;
use komo_kernel::types::status::{RunStatus, ToolCallState};
use komo_kernel::types::turn::Role;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};

use crate::tui::app::{App, SubmissionState, status_text};
use crate::tui::approval::approval_lines;
use crate::tui::markdown;

/// 画一帧。
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let palette = app.palette();
    let palette_height = if palette.is_empty() {
        0
    } else {
        (palette.len() as u16).min(6)
    };
    let input_height = input_height(app, area.width);

    let chunks = Layout::vertical([
        Constraint::Length(1),              // 身份行
        Constraint::Min(3),                 // 消息面
        Constraint::Length(1),              // 状态行
        Constraint::Length(palette_height), // 命令面板
        Constraint::Length(input_height),   // 输入框
    ])
    .split(area);

    frame.render_widget(identity_line(app), chunks[0]);
    frame.render_widget(transcript(app, chunks[1]), chunks[1]);
    frame.render_widget(status_line(app), chunks[2]);
    if palette_height > 0 {
        frame.render_widget(palette_block(&palette), chunks[3]);
    }
    frame.render_widget(input_block(app), chunks[4]);

    if app.approval.is_some() {
        draw_approval(frame, app, area);
    }
}

fn input_height(app: &App, width: u16) -> u16 {
    let text = app.input.display();
    let inner = width.saturating_sub(2).max(1) as usize;
    let lines: usize = text
        .split('\n')
        .map(|line| markdown::wrap_to_width(line, inner).len())
        .sum();
    (lines.max(1) as u16 + 2).clamp(3, 10)
}

fn identity_line(app: &App) -> Paragraph<'static> {
    let spans = vec![
        Span::styled(
            format!(" komo · {} ", app.mode.label()),
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            app.session.to_string(),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    Paragraph::new(Line::from(spans))
}

/// 消息面。助手正文走 Markdown，工具调用逐条一行（可展开）。
fn transcript(app: &App, area: Rect) -> Paragraph<'static> {
    let width = area.width.saturating_sub(2).max(8);
    let mut lines: Vec<Line<'static>> = Vec::new();

    for message in app.messages() {
        match message.role {
            Role::User => {
                if let Some(text) = message.text.as_deref() {
                    for piece in markdown::wrap_to_width(text, width.saturating_sub(3) as usize) {
                        lines.push(Line::from(vec![
                            Span::styled("你 ", Style::default().fg(Color::Green).bold()),
                            Span::raw(piece),
                        ]));
                    }
                } else if message.text_ref.is_some() {
                    lines.push(dim("你 （正文已外置）"));
                }
                lines.push(Line::default());
            }
            Role::Assistant => {
                if let Some(text) = message.text.as_deref().filter(|t| !t.trim().is_empty()) {
                    lines.extend(markdown::render(text, width));
                }
                lines.extend(tool_lines_of(app, message, width));
                lines.push(Line::default());
            }
            // 工具结果在调用那一行上显示，不另占一个消息节点。
            Role::Tool => {}
        }
    }

    // Enter 后立刻显示，不等 SSE 绕一圈。权威 `run.accepted` 到达时状态机会移除对应项，
    // 因而不会和 JSONL 折出来的用户消息重复。
    for pending in &app.pending_submissions {
        for piece in markdown::wrap_to_width(&pending.text, width.saturating_sub(3) as usize) {
            lines.push(Line::from(vec![
                Span::styled("你 ", Style::default().fg(Color::Green).bold()),
                Span::raw(piece),
            ]));
        }
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
        lines.push(Line::default());
    }

    // 模型正在打字的那一段：接在历史后面，带一个「生成中」的记号，**不是历史的一部分**
    // （`message.assistant` 到了它就被那一条替换掉）。
    if let Some(draft) = &app.draft {
        lines.extend(markdown::render(&draft.text, width));
        lines.push(Line::from(Span::styled(
            "▌生成中…",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
        )));
        lines.push(Line::default());
    }

    if app.phase.is_backfilling() {
        lines.push(dim("正在补读历史……"));
    }
    for notice in &app.notices {
        let style = if notice.is_error {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        for piece in markdown::wrap_to_width(&notice.text, width.saturating_sub(2) as usize) {
            lines.push(Line::from(Span::styled(format!("· {piece}"), style)));
        }
    }

    // 贴底显示：只画放得下的最后那些行，`scroll` 往上挪。
    let height = area.height as usize;
    let end = lines.len().saturating_sub(app.scroll as usize);
    let start = end.saturating_sub(height);
    let window = lines[start.min(lines.len())..end.min(lines.len())].to_vec();

    Paragraph::new(window).block(Block::default().padding(Padding::horizontal(1)))
}

fn tool_lines_of(app: &App, message: &SurfaceMessage, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for request in &message.tool_calls {
        let Some(tool) = app.tool(&request.call_id) else {
            continue;
        };
        let (colour, marker) = (state_colour(tool.state), tool.marker());
        let head = format!("  {marker} {} {}", tool.tool, tool.summary());
        lines.push(Line::from(Span::styled(
            markdown::truncate_to_width(&head, width as usize),
            Style::default().fg(colour),
        )));
        if tool.elapsed_ms > 0 && tool.state.is_terminal() {
            lines.push(dim(&format!("     {} ms", tool.elapsed_ms)));
        }
        if tool.expanded {
            let args = serde_json::to_string_pretty(&tool.args).unwrap_or_default();
            for line in args.lines() {
                lines.push(Line::from(Span::styled(
                    markdown::truncate_to_width(&format!("     {line}"), width as usize),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if let Some(preview) = &tool.preview {
                lines.push(dim("     ── 结果预览 ──"));
                for line in preview.lines() {
                    lines.push(Line::from(Span::styled(
                        markdown::truncate_to_width(&format!("     {line}"), width as usize),
                        Style::default().fg(Color::Gray),
                    )));
                }
            }
        }
    }
    lines
}

fn state_colour(state: ToolCallState) -> Color {
    match state {
        ToolCallState::Planned => Color::DarkGray,
        ToolCallState::Started => Color::Yellow,
        ToolCallState::Completed => Color::Green,
        ToolCallState::Failed => Color::Red,
        // uncertain 不是失败的一种颜色——它是「不知道」，所以给它自己的颜色（§8.6）。
        ToolCallState::Uncertain => Color::Magenta,
    }
}

/// 状态行：Run 状态 · 本轮耗时 · 模型与 effort · 连接状态 · 待审批数。
fn status_line(app: &App) -> Paragraph<'static> {
    let mut parts: Vec<Span<'static>> = Vec::new();

    match app.run_status() {
        Some(status) => parts.push(Span::styled(
            format!(" {} ", status_text(status)),
            Style::default()
                .fg(Color::Black)
                .bg(run_colour(status))
                .add_modifier(Modifier::BOLD),
        )),
        None => parts.push(Span::styled(
            " 空闲 ",
            Style::default().fg(Color::Black).bg(Color::DarkGray),
        )),
    }

    if let Some(elapsed) = app.elapsed() {
        parts.push(Span::raw(format!(" {} ", pretty_duration(elapsed))));
    }

    let model = app
        .run_meta()
        .and_then(|meta| meta.model.clone())
        .or_else(|| app.model.clone())
        .unwrap_or_else(|| "默认模型".into());
    let effort = app
        .run_meta()
        .and_then(|meta| meta.effort.as_ref().and_then(|e| e.as_option().cloned()))
        .or_else(|| app.effort.clone())
        .map(|e| e.to_string())
        .unwrap_or_else(|| "服务端默认".into());
    parts.push(Span::styled(
        format!("· {model} / {effort} "),
        Style::default().fg(Color::Gray),
    ));

    let connection = &app.connection;
    parts.push(Span::styled(
        format!("· {} ", connection.label()),
        Style::default().fg(if connection.is_connected() {
            Color::Green
        } else {
            Color::Yellow
        }),
    ));

    if app.pending_count() > 0 {
        parts.push(Span::styled(
            format!("· 待审批 {} ", app.pending_count()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    Paragraph::new(Line::from(parts))
}

fn run_colour(status: RunStatus) -> Color {
    match status {
        RunStatus::Running | RunStatus::Queued | RunStatus::Ingesting => Color::Cyan,
        RunStatus::WaitingApproval | RunStatus::WaitingRetry | RunStatus::NeedsAttention => {
            Color::Yellow
        }
        RunStatus::Interrupted => Color::Magenta,
        RunStatus::Completed => Color::Green,
        RunStatus::Failed => Color::Red,
        RunStatus::Cancelled => Color::DarkGray,
    }
}

fn pretty_duration(duration: time::Duration) -> String {
    let seconds = duration.whole_seconds().max(0);
    if seconds < 60 {
        format!(
            "{}.{}s",
            seconds,
            (duration.subsec_milliseconds() / 100).abs()
        )
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

fn palette_block(matches: &[(&'static str, &'static str)]) -> Paragraph<'static> {
    let lines: Vec<Line<'static>> = matches
        .iter()
        .map(|(name, blurb)| {
            Line::from(vec![
                Span::styled(
                    format!(" {name} "),
                    Style::default().fg(Color::LightBlue).bold(),
                ),
                Span::styled((*blurb).to_string(), Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();
    Paragraph::new(lines).block(Block::default().padding(Padding::horizontal(1)))
}

fn input_block(app: &App) -> Paragraph<'static> {
    let enabled = app.input_enabled();
    let style = if enabled {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let body = if enabled {
        app.input.display()
    } else {
        String::new()
    };
    Paragraph::new(Span::styled(body, style)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(format!(" {} ", app.input_hint()))
            .border_style(Style::default().fg(if enabled {
                Color::DarkGray
            } else {
                Color::Yellow
            })),
    )
}

/// 审批弹窗：居中，盖住底下的内容（[`Clear`]），不动布局。
///
/// 按键提示放在**边框的底栏**而不是正文里：正文会滚动，而一个滚出屏幕的「y 批准 /
/// n 拒绝」等于没有提示——在窄终端上正文几乎一定滚。
fn draw_approval(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(modal) = app.approval.as_ref() else {
        return;
    };
    let popup = centered(area, 92, 86);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .title(" 需要批准 ")
        .title_bottom(Line::from(Span::styled(
            format!(" {} ", modal.keys_hint()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )))
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = approval_lines(&modal.record);
    // 弹窗里也不许有一行比它宽。
    for line in &mut lines {
        if line.width() > inner.width as usize {
            let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let style = line.spans.first().map(|s| s.style).unwrap_or_default();
            *line = Line::from(Span::styled(
                markdown::truncate_to_width(&flat, inner.width as usize),
                style,
            ));
        }
    }
    // 放不下就说还有多少行——滚动条之外最起码的一句话。
    let hidden = lines.len().saturating_sub(inner.height as usize);
    if hidden > 0 {
        let scrolled = (modal.scroll as usize).min(hidden);
        lines.insert(
            0,
            Line::from(Span::styled(
                format!("（还有 {} 行，↑/↓ 或 PgUp/PgDn 滚动）", hidden - scrolled),
                Style::default().fg(Color::DarkGray),
            )),
        );
    }

    frame.render_widget(Paragraph::new(lines).scroll((modal.scroll, 0)), inner);
}

fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
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
    use crate::sse::ConnectionState;
    use crate::tui::app::{App, Effect, ServerEvent, TuiMode};
    use crate::tui::test_support as fixture;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use komo_kernel::events::{Event, EventPayload, MessageAssistant};
    use komo_kernel::protocol::sse::{SseEvent, SseFrame};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// 一段中文 + 代码块 + 表格混排的长回复——§13.4 说窄终端也要能用。
    const LONG_REPLY: &str = "\
## 清理结果

我把 `build/` 清掉了，顺带看了一眼构建脚本。下面这段中文是故意写长的，为的是逼渲染层在八十列的终端里折行，而不是把整个布局撑破。

```rust
fn main() {
    println!(\"这一行故意写得非常非常非常非常非常非常非常非常非常非常非常非常长\");
}
```

| 文件 | 大小 | 说明 |
|---|---|---|
| build/app | 12 MB | 可执行文件 |
| build/deps | 340 MB | 依赖中间产物，占了绝大部分空间 |

> 下次要不要顺手加个 .gitignore？

1. 已删除 12 个文件
2. 释放 352 MB
";

    fn conversation_app() -> App {
        let mut app = App::new(fixture::session(), TuiMode::New, "seed");
        let mut events = fixture::conversation();
        // 把最后一轮的回复换成那段长 Markdown。
        if let EventPayload::MessageAssistant(body) = &mut events[7].payload {
            *body = MessageAssistant {
                round: 2,
                text: Some(LONG_REPLY.into()),
                text_ref: None,
                tool_calls: vec![],
                provider_blocks: None,
                input_tokens: Some(200),
                output_tokens: Some(20),
            };
        }
        feed(&mut app, &events);
        app.apply(ServerEvent::Connection(ConnectionState::Connected));
        app.apply(ServerEvent::Tick(fixture::T0 + time::Duration::seconds(30)));
        app
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

    fn snapshot(app: &App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("能建终端");
        terminal.draw(|frame| draw(frame, app)).expect("能画一帧");
        terminal.backend().buffer().clone()
    }

    /// 缓冲区的每一行，右侧空白已去掉。
    ///
    /// 一个宽字符占两格，第二格是它的延续，不是另一个字符——按显示宽度跳过去，否则
    /// 重建出来的行会比屏幕上的宽。
    fn rows(buffer: &Buffer) -> Vec<String> {
        let area = buffer.area();
        (0..area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0u16;
                while x < area.width {
                    let symbol = buffer[(x, y)].symbol();
                    let width = crate::tui::markdown::display_width(symbol).max(1) as u16;
                    row.push_str(symbol);
                    x += width;
                }
                row.trim_end().to_string()
            })
            .collect()
    }

    #[test]
    fn a_submitted_message_is_visible_before_the_event_stream_echoes_it() {
        let mut app = App::new(fixture::session(), TuiMode::New, "seed");
        app.input.set("刚发出去的消息");
        let effects = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(effects.as_slice(), [Effect::Submit { .. }]));

        let screen = rows(&snapshot(&app, 80, 24)).join("\n");
        assert!(screen.contains("刚发出去的消息"), "{screen}");
        assert!(screen.contains("发送中"), "{screen}");
    }

    #[test]
    fn a_submit_failure_is_attached_to_the_message_that_failed() {
        let mut app = App::new(fixture::session(), TuiMode::New, "seed");
        app.input.set("这条没有送到");
        let effects = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Effect::Submit { request_key, .. } = &effects[0] else {
            panic!("{effects:?}")
        };
        app.apply(ServerEvent::SubmitFailed {
            request_key: request_key.clone(),
            error: "Gateway 不可用".into(),
        });

        let screen = rows(&snapshot(&app, 80, 24)).join("\n");
        assert!(screen.contains("这条没有送到"), "{screen}");
        assert!(screen.contains("发送失败：Gateway 不可用"), "{screen}");
    }

    #[test]
    fn a_run_failure_shows_its_reason_in_the_transcript() {
        let mut app = App::new(fixture::session(), TuiMode::New, "seed");
        let events = vec![
            fixture::conversation()[0].clone(),
            fixture::event(
                2,
                Some(fixture::run()),
                EventPayload::RunFailed(komo_kernel::events::RunFailed {
                    reason: "上游拒绝了这次请求".into(),
                }),
            ),
        ];
        feed(&mut app, &events);

        let screen = rows(&snapshot(&app, 80, 24)).join("\n");
        assert!(screen.contains("上游拒绝了这次请求"), "{screen}");
    }

    /// 每一行都恰好占满缓冲区的宽度——没有一格越界，也没有一行被截短。
    fn assert_within(buffer: &Buffer, width: u16, height: u16) {
        let area = buffer.area();
        assert_eq!(area.width, width);
        assert_eq!(area.height, height);
        for y in 0..height {
            for x in 0..width {
                // 越界访问会 panic；这里遍历一遍就是在断言「每一格都在里面」。
                let _ = buffer[(x, y)].symbol();
            }
        }
    }

    #[test]
    fn a_narrow_eighty_column_terminal_renders_within_its_bounds() {
        let app = conversation_app();
        let buffer = snapshot(&app, 80, 24);
        assert_within(&buffer, 80, 24);
        let rows = rows(&buffer);
        for (index, row) in rows.iter().enumerate() {
            assert!(
                crate::tui::markdown::display_width(row) <= 80,
                "第 {index} 行宽 {}：{row}",
                crate::tui::markdown::display_width(row)
            );
        }
        let screen = rows.join("\n");
        // 身份行、状态行、输入框都在。
        assert!(screen.contains("komo · 新会话"), "{screen}");
        assert!(screen.contains("已完成"), "{screen}");
        assert!(screen.contains("chat-a"), "{screen}");
        assert!(screen.contains("high"), "{screen}");
        assert!(screen.contains("已连接"), "{screen}");
        assert!(screen.contains("Enter 发送"), "{screen}");
    }

    #[test]
    fn a_wide_two_hundred_column_terminal_renders_within_its_bounds() {
        let app = conversation_app();
        let buffer = snapshot(&app, 200, 50);
        assert_within(&buffer, 200, 50);
        let screen = rows(&buffer).join("\n");
        assert!(screen.contains("清理结果"), "{screen}");
        // 宽屏放得下表格与代码块。
        assert!(screen.contains("build/deps"), "{screen}");
        assert!(screen.contains("fn main()"), "{screen}");
        assert!(screen.contains("│"), "引用与表格的竖线：{screen}");
    }

    #[test]
    fn the_tool_line_shows_the_tool_its_summary_and_its_state() {
        let app = conversation_app();
        let screen = rows(&snapshot(&app, 100, 40)).join("\n");
        assert!(screen.contains("shell rm -rf build"), "{screen}");
        assert!(screen.contains("ok shell"), "{screen}");
    }

    #[test]
    fn an_uncertain_call_shows_two_question_marks_on_screen() {
        let mut app = App::new(fixture::session(), TuiMode::New, "seed");
        let mut events = fixture::conversation();
        if let EventPayload::ToolResult(body) = &mut events[6].payload {
            body.status = komo_kernel::types::refs::ToolResultStatus::Uncertain;
        }
        feed(&mut app, &events[..7]);
        let screen = rows(&snapshot(&app, 100, 30)).join("\n");
        assert!(screen.contains("?? shell"), "{screen}");
    }

    #[test]
    fn the_approval_popup_shows_the_five_items_and_does_not_overflow() {
        let mut app = App::new(fixture::session(), TuiMode::New, "seed");
        feed(&mut app, &fixture::conversation()[..4]);
        app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));

        // 够高的窗口：五项都在屏幕上。
        for (width, height) in [(80u16, 44u16), (200, 50)] {
            let buffer = snapshot(&app, width, height);
            assert_within(&buffer, width, height);
            let rows = rows(&buffer);
            for row in &rows {
                assert!(
                    crate::tui::markdown::display_width(row) <= width as usize,
                    "{width} 列下有一行溢出：{row}"
                );
            }
            let screen = rows.join("\n");
            assert!(screen.contains("需要批准"), "{screen}");
            assert!(screen.contains("7K2M"), "一、短 ID：{screen}");
            assert!(screen.contains("动作"), "二、动作：{screen}");
            assert!(screen.contains("shell"), "工具：{screen}");
            assert!(screen.contains("rm -rf build"), "命令：{screen}");
            assert!(
                screen.contains("/home/u/project/build"),
                "真实目标路径：{screen}"
            );
            assert!(screen.contains("cwd"), "cwd：{screen}");
            assert!(screen.contains("版本"), "版本：{screen}");
            assert!(screen.contains("改动"), "三、改动：{screen}");
            assert!(screen.contains("+新的一行"), "diff：{screen}");
            assert!(screen.contains("原因"), "四、原因：{screen}");
            assert!(screen.contains("范围"), "五、范围：{screen}");
            // 待审批时输入框禁用并提示。
            assert!(screen.contains("先 y / r 批准"), "输入框提示：{screen}");
        }
    }

    #[test]
    fn the_popup_keys_stay_visible_even_when_the_body_does_not_fit() {
        let mut app = App::new(fixture::session(), TuiMode::New, "seed");
        feed(&mut app, &fixture::conversation()[..4]);
        app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));

        // 一个放不下正文的窄窗口：按键提示在边框上，滚不走。
        let buffer = snapshot(&app, 80, 24);
        assert_within(&buffer, 80, 24);
        let rows = rows(&buffer);
        for row in &rows {
            assert!(
                crate::tui::markdown::display_width(row) <= 80,
                "80 列下有一行溢出：{row}"
            );
        }
        let screen = rows.join("\n");
        assert!(screen.contains("y 批准本次"), "{screen}");
        assert!(screen.contains("n / Esc 拒绝"), "{screen}");
        assert!(screen.contains("滚动"), "放不下要说还有多少行：{screen}");
        // 输入框在这么窄的窗口里被弹窗盖住了；它禁用这件事由状态说了算。
        assert!(!app.input_enabled());
    }

    #[test]
    fn a_draft_shows_on_screen_with_a_generating_marker_and_then_goes_away() {
        let mut app = App::new(fixture::session(), TuiMode::New, "seed");
        feed(&mut app, &fixture::conversation()[..3]);
        app.apply(ServerEvent::Frame(Box::new(SseFrame {
            id: komo_kernel::types::ids::Seq(100),
            session: fixture::session(),
            event: SseEvent::AssistantDelta {
                run: fixture::run(),
                round: 1,
                text: "我来清一".into(),
            },
        })));
        let screen = rows(&snapshot(&app, 80, 24)).join("\n");
        assert!(screen.contains("我来清一"), "{screen}");
        assert!(screen.contains("生成中"), "{screen}");

        // 正式回复到了，草稿与记号一起消失，屏幕上只剩那一条。
        feed(&mut app, &fixture::conversation()[3..4]);
        let screen = rows(&snapshot(&app, 80, 24)).join("\n");
        assert!(!screen.contains("生成中"), "{screen}");
        assert_eq!(screen.matches("我来清一下。").count(), 1, "{screen}");
    }

    #[test]
    fn a_reconnecting_subscription_says_so_on_the_status_line() {
        let mut app = conversation_app();
        app.apply(ServerEvent::Connection(ConnectionState::Reconnecting {
            attempt: 2,
            reason: "connection reset".into(),
        }));
        let screen = rows(&snapshot(&app, 80, 20)).join("\n");
        assert!(screen.contains("重连中 #2"), "{screen}");
    }

    #[test]
    fn the_command_palette_appears_while_a_slash_command_is_being_typed() {
        let mut app = App::new(fixture::session(), TuiMode::New, "seed");
        app.input.set("/ap");
        let screen = rows(&snapshot(&app, 80, 20)).join("\n");
        assert!(screen.contains("/approve"), "{screen}");
        assert!(screen.contains("本次 Run 范围"), "{screen}");
    }

    #[test]
    fn a_folded_paste_shows_its_chip_not_its_content() {
        let mut app = App::new(fixture::session(), TuiMode::New, "seed");
        app.handle_input(crate::tui::paste::InputEvent::Paste(
            "机密第一行\n机密第二行\n机密第三行\n机密第四行".into(),
        ));
        let screen = rows(&snapshot(&app, 80, 20)).join("\n");
        assert!(screen.contains("[粘贴 4 行"), "{screen}");
        assert!(
            !screen.contains("机密第二行"),
            "折起来的内容不该画出来：{screen}"
        );
    }

    #[test]
    fn a_twenty_column_terminal_still_draws_without_panicking() {
        // 不是要好看，是要**不崩**。
        let app = conversation_app();
        let buffer = snapshot(&app, 20, 10);
        assert_within(&buffer, 20, 10);
    }
}
