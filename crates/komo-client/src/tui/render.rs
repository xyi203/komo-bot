//! 视口绘制（§13.4）。
//!
//! **这里画的不是一屏，是终端底下那一小块。** 会话正文交给终端自己的回滚区（见
//! [`crate::tui::transcript`]），所以滚轮、选中复制、退出之后留在屏幕上的那段记录，都
//! 是终端原本就会做的事，不是这里重新实现一遍的东西。
//!
//! 视口从上到下：
//!
//! ```text
//! │ 还在动的那一小段（草稿 / 在跑的工具 / 刚按下 Enter 的那条）  ← 高度有上限，只画尾巴
//! ├ 状态行（Run 状态 · 本轮耗时 · 模型 · effort · 连接 · 待处理 · 模式）
//! │ 命令面板（只在输入以 `/` 开头时出现）
//! ╰ 输入框（会折行，光标摆在它真正在的那一格上）
//! ```
//!
//! 审批弹窗来的时候它**占满视口**：输入框本来就该禁用（§11.3 先把眼前这件事答了），
//! 留一个画得出来却按不动的框只会骗人。
//!
//! [`viewport_height`] 与 [`draw`] 必须算出同一个高度——驱动照前者开视口、照后者画，
//! 两边差一行，输入框就会被切掉一条边。所以两边共用底下那几个 `*_height`。

use komo_kernel::types::status::RunState;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

use crate::tui::app::{App, status_text};
use crate::tui::approval::{ApprovalRow, approval_lines};
pub(crate) use crate::tui::markdown;

/// 还在动的那一段最多占几行。
///
/// 它有上限是因为**视口高度一变，底下那块就要重开一次**：不封顶的话，模型每吐出一行
/// 都是一次重开。封了顶之后，一轮对话里最多长这么多次，长到头就不动了。
pub const LIVE_MAX: u16 = 8;
/// 输入框里最多显示几行（正文再长也只滚动，不把视口顶穿）。
pub const INPUT_MAX_ROWS: u16 = 8;
/// 命令面板最多列几条。
const PALETTE_MAX: u16 = 6;

/// 视口要多高。`max` 是终端高度减去留给上文的那一行。
pub fn viewport_height(app: &App, width: u16, max: u16, live_lines: usize) -> u16 {
    let max = max.max(1);
    if app.approval.is_some() {
        return modal_height(app, max);
    }
    let live = (live_lines as u16).min(LIVE_MAX);
    (live + 1 + palette_height(app) + input_height(app, width)).min(max)
}

/// 画一帧。`live` 是 [`crate::tui::transcript::Emitted::live`] 给的那一段。
pub fn draw(frame: &mut Frame<'_>, app: &App, live: &[Line<'static>]) {
    let area = frame.area();
    if app.approval.is_some() {
        draw_modal(frame, app, area);
        return;
    }

    let palette = app.palette();
    let palette_height = palette_height(app);
    let input_height = input_height(app, area.width);
    let live_height = area
        .height
        .saturating_sub(1 + palette_height + input_height);

    let chunks = Layout::vertical([
        Constraint::Length(live_height),
        Constraint::Length(1),
        Constraint::Length(palette_height),
        Constraint::Length(input_height),
    ])
    .split(area);

    if live_height > 0 {
        frame.render_widget(live_block(live, live_height), chunks[0]);
    }
    frame.render_widget(status_line(app), chunks[1]);
    if palette_height > 0 {
        frame.render_widget(palette_block(&palette), chunks[2]);
    }
    draw_input(frame, app, chunks[3]);
}

/// 还在动的那一段：放不下就**只画尾巴**，因为在动的总是最后那几行。
fn live_block(lines: &[Line<'static>], height: u16) -> Paragraph<'static> {
    let start = lines.len().saturating_sub(height as usize);
    Paragraph::new(lines[start..].to_vec()).block(Block::default().padding(Padding::horizontal(1)))
}

fn palette_height(app: &App) -> u16 {
    (app.palette().len() as u16).min(PALETTE_MAX)
}

/// 输入框连边框一共几行。
fn input_height(app: &App, width: u16) -> u16 {
    (input_rows(app, width).len() as u16).clamp(1, INPUT_MAX_ROWS) + 2
}

/// 输入框里折好的每一行，附它在 [`crate::tui::paste::Input::display`] 那份文本里的起始
/// 偏移——光标靠它落位。
fn input_rows(app: &App, width: u16) -> Vec<(usize, String)> {
    let inner = width.saturating_sub(2).max(1) as usize;
    let text = if app.input_enabled() {
        app.input.display()
    } else {
        String::new()
    };
    let mut rows = Vec::new();
    let mut base = 0usize;
    for line in text.split('\n') {
        for (at, piece) in markdown::wrap_with_offsets(line, inner) {
            rows.push((base + at, piece));
        }
        // `split` 吃掉的那个换行也占一个字节。
        base += line.len() + 1;
    }
    rows
}

/// 输入框，外加**把光标摆在它真正在的那一格上**。
///
/// 不摆这一下，ratatui 每一帧都会把光标藏起来：人在框里打字，插入符却不在框里——这就是
/// "初始输入位置对不上"。宽字符一个占两列，所以列数按显示宽度算，不按字符数。
fn draw_input(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let enabled = app.input_enabled();
    let rows = input_rows(app, area.width);
    let (caret_row, caret_column) = markdown::caret_at(&rows, app.input.display_cursor());

    // 框里放得下几行，以及从第几行开始画——光标跑出框外时跟着滚，人不会打着打着就看不见
    // 自己写到哪了。
    let visible = area.height.saturating_sub(2).max(1) as usize;
    let offset = (caret_row + 1).saturating_sub(visible);

    let style = if enabled {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let body: Vec<Line<'static>> = rows
        .iter()
        .skip(offset)
        .take(visible)
        .map(|(_, line)| Line::from(Span::styled(line.clone(), style)))
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(" {} ", app.input_hint()))
        .border_style(Style::default().fg(if enabled {
            Color::DarkGray
        } else {
            Color::Yellow
        }));
    let inner = block.inner(area);
    frame.render_widget(Paragraph::new(body).block(block), area);

    if enabled && inner.width > 0 && inner.height > 0 {
        frame.set_cursor_position(Position {
            x: inner.x + (caret_column as u16).min(inner.width - 1),
            y: inner.y + (caret_row.saturating_sub(offset) as u16).min(inner.height - 1),
        });
    }
}

/// 状态行：Run 状态（等什么就说出什么） · 本轮耗时 · 模型与 effort · 连接状态 ·
/// 待处理数 · 这个界面是怎么打开的。
fn status_line(app: &App) -> Paragraph<'static> {
    let mut parts: Vec<Span<'static>> = Vec::new();

    if app.phase.is_backfilling() {
        parts.push(Span::styled(
            " 补读历史 ",
            Style::default().fg(Color::Black).bg(Color::LightBlue),
        ));
    }

    match app.run_state() {
        Some(state) => {
            // 还在跑的那一格是动的：一块静止的文字分不出还在跑还是卡住了。
            if !state.is_terminal() {
                parts.push(Span::styled(
                    format!(" {}", crate::tui::spinner::frame(app.now)),
                    Style::default().fg(run_colour(state)),
                ));
            }
            // **一切正常的两个状态不写出来**：跑着有转圈，跑完了屏幕上就是那段回答——
            // "运行中" / "已完成" 都是让人读一件他正看着的事。剩下的几个留着，它们说的
            // 是转圈与回答都说不出的事：在排队、在等人、失败了、被取消了。
            if !matches!(state, RunState::Running | RunState::Completed) {
                parts.push(Span::styled(
                    format!(" {} ", status_text(state)),
                    Style::default()
                        .fg(Color::Black)
                        .bg(run_colour(state))
                        .add_modifier(Modifier::BOLD),
                ));
            }
            // §8.4：状态只说"能不能跑"，理由说"在等谁、等到什么时候"。**排队二十分钟
            // 不知道为什么**就是少了这一格；窄终端下行会被截，但截掉的是一句话的后半段，
            // 不是全部。
            if let Some(reason) = app.run_wait() {
                parts.push(Span::styled(
                    format!("{} ", crate::tui::app::wait_text(reason, app.now)),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
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
        // 三类合计（§7.5）：状态行只报"有几条在等人"，哪一种在哪一条由清单与提示行说。
        parts.push(Span::styled(
            format!("· 待处理 {} ", app.pending_count()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // 会话 Id 只在开场那条横幅上（[`banner`]）与退出那行命令里出现，这里只留"怎么打开
    // 的"——一行状态挤不下一个 UUID，挤进去只会把前面那些真会变的东西顶掉。
    parts.push(Span::styled(
        format!("· komo · {}", app.mode.label()),
        Style::default().fg(Color::DarkGray),
    ));

    Paragraph::new(Line::from(parts))
}

/// 开场横幅：进 TUI 时印一次，之后它就是终端回滚区里普通的两行。
///
/// 会话 Id 在这里"闪过一次"；`komo` 裸命令这时还没有会话，等第一条消息把它铸出来时由
/// 一条提示补上。
pub fn banner(app: &App, cwd: &str) -> Vec<Line<'static>> {
    let mut head = vec![
        Span::styled(
            format!(" komo · {} ", app.mode.label()),
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(cwd.to_string(), Style::default().fg(Color::DarkGray)),
    ];
    if let Some(session) = &app.session {
        head.push(Span::styled(
            format!(" · {session}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    vec![Line::from(head), Line::default()]
}

fn run_colour(state: RunState) -> Color {
    match state {
        RunState::Accepted | RunState::Queued | RunState::Running => Color::Cyan,
        // 等待：**停着**，所以是黄色——但它是哪一种等待，由状态行后面那句理由说。
        RunState::Waiting => Color::Yellow,
        RunState::Completed => Color::Green,
        RunState::Failed => Color::Red,
        RunState::Cancelled | RunState::Abandoned => Color::DarkGray,
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
                    Style::default()
                        .fg(Color::LightBlue)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled((*blurb).to_string(), Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();
    Paragraph::new(lines).block(Block::default().padding(Padding::horizontal(1)))
}

/// 审批弹窗要多高：正文 + 菜单 + 边框 + 状态行，放不下就按 `max` 截。
fn modal_height(app: &App, max: u16) -> u16 {
    let Some(modal) = app.approval.as_ref() else {
        return max;
    };
    let body = approval_lines(&modal.record).len() as u16;
    let menu = modal.rows(app.pending_count()).len() as u16;
    body.saturating_add(menu).saturating_add(3).min(max)
}

/// 审批弹窗：占满视口，底下留一行状态行。
///
/// 弹窗里有两块地方**不滚**：
///
/// ```text
/// ├ 正文（五项：短 ID / 动作 / 改动 / 原因 / 范围）   ← 会滚，PgUp / PgDn
/// ├ 答案菜单（每一行是一个答案，↑ / ↓ 选、Enter 确认） ← 钉在底部
/// └ 边框底栏（怎么开这张菜单）
/// ```
///
/// 钉住是因为正文在窄终端上几乎一定放不下，而一个滚出屏幕的「Enter 确认」等于没有提示
/// ——这个弹窗上所有的答案都在菜单里。
fn draw_modal(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(modal) = app.approval.as_ref() else {
        return;
    };
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(area);
    let popup = chunks[0];

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

    // 菜单钉在底部：正文多长都不许把它挤出窗口。这里手工切 `Rect` 而不是用 `Layout`
    // ——窗口矮到装不下菜单时，`Length` 约束怎么取舍是布局库的事，而这里要的是"菜单
    // 优先，正文归零"这一条明确的规矩。
    let pending = app.pending_count();
    let rows = modal.rows(pending);
    let menu_height = (rows.len() as u16).min(inner.height);
    let menu = Rect {
        y: inner.y + inner.height - menu_height,
        height: menu_height,
        ..inner
    };
    let body = Rect {
        height: inner.height - menu_height,
        ..inner
    };

    if body.height > 0 {
        let mut lines = approval_lines(&modal.record);
        // 弹窗里也不许有一行比它宽。
        for line in &mut lines {
            if line.width() > body.width as usize {
                let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let style = line.spans.first().map(|s| s.style).unwrap_or_default();
                *line = Line::from(Span::styled(
                    markdown::truncate_to_width(&flat, body.width as usize),
                    style,
                ));
            }
        }
        // 放不下就说还有多少行——滚动条之外最起码的一句话。
        let hidden = lines.len().saturating_sub(body.height as usize);
        if hidden > 0 {
            let scrolled = (modal.scroll as usize).min(hidden);
            lines.insert(
                0,
                Line::from(Span::styled(
                    format!("（还有 {} 行，PgUp/PgDn 滚动）", hidden - scrolled),
                    Style::default().fg(Color::DarkGray),
                )),
            );
        }
        frame.render_widget(Paragraph::new(lines).scroll((modal.scroll, 0)), body);
    }

    if menu_height > 0 {
        let lines = menu_lines(
            &rows,
            modal.selected_index(pending),
            modal.answering,
            menu.width,
        );
        frame.render_widget(Paragraph::new(lines), menu);
    }

    frame.render_widget(status_line(app), chunks[1]);
}

/// 菜单的每一行：高亮那一行是实心色块，其余是灰的；行尾挂着它的直通键。
///
/// 每一行按 `width` 自己裁、自己补空格——高亮要是一条通到底的色块，而"超宽就截断"那
/// 套循环作用在正文的 `Vec<Line>` 上，够不着这里。
fn menu_lines(
    rows: &[ApprovalRow],
    selected: usize,
    answering: bool,
    width: u16,
) -> Vec<Line<'static>> {
    // "▸ " + 标签 + 标签到直通键之间的空格 + 直通键。
    let label_width = (width as usize).saturating_sub(4);
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let chosen = index == selected && !answering;
            let row_style = if answering {
                Style::default().fg(Color::DarkGray)
            } else if chosen {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let label = markdown::truncate_to_width(&row.label, label_width);
            let fill = label_width.saturating_sub(markdown::display_width(&label)) + 1;
            Line::from(vec![
                Span::styled(if chosen { "▸ " } else { "  " }, row_style),
                Span::styled(label, row_style),
                Span::styled(" ".repeat(fill), row_style),
                Span::styled(
                    row.key.to_string(),
                    if chosen {
                        row_style
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::ConnectionState;
    use crate::tui::app::{Effect, ServerEvent, TuiMode};
    use crate::tui::test_support as fixture;
    use crate::tui::transcript::Emitted;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use komo_kernel::events::{Event, EventPayload, MessageAssistant, RunWaiting};
    use komo_kernel::protocol::http::InterventionKind;
    use komo_kernel::protocol::sse::{SseEvent, SseFrame};
    use komo_kernel::types::status::{RetryCause, WaitReason};
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
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
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

    /// 人**看得见的那一屏**：终端回滚区上那一段（尾巴）加底下的视口。
    ///
    /// 驱动把正文交给终端、只画视口，所以单画视口的断言会漏掉一半界面；这个助手把两边
    /// 按真实的高度拼回去，测试问的于是还是「屏幕上有没有这句话」。
    fn screen_rows(app: &App, width: u16, height: u16) -> Vec<String> {
        let mut emitted = Emitted::new();
        let settled = emitted.take(app, width);
        let live = emitted.live(app, width);
        let viewport = viewport_height(app, width, height.saturating_sub(1).max(1), live.len());
        let above = height.saturating_sub(viewport);

        let mut out = Vec::new();
        if above > 0 {
            let start = (settled.len() as u16).saturating_sub(above);
            let mut terminal = Terminal::new(TestBackend::new(width, above)).expect("能建终端");
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new(settled.clone()).scroll((start, 0)),
                        frame.area(),
                    )
                })
                .expect("能画一帧");
            out.extend(rows(terminal.backend().buffer()));
        }
        let mut terminal = Terminal::new(TestBackend::new(width, viewport)).expect("能建终端");
        terminal
            .draw(|frame| draw(frame, app, &live))
            .expect("能画一帧");
        out.extend(rows(terminal.backend().buffer()));
        out
    }

    fn screen(app: &App, width: u16, height: u16) -> String {
        screen_rows(app, width, height).join("\n")
    }

    /// 只画视口那一块。光标落在哪一格要问它。
    fn viewport(app: &App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut emitted = Emitted::new();
        let _ = emitted.take(app, width);
        let live = emitted.live(app, width);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("能建终端");
        terminal
            .draw(|frame| draw(frame, app, &live))
            .expect("能画一帧");
        terminal
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
                    let width = markdown::display_width(symbol).max(1) as u16;
                    row.push_str(symbol);
                    x += width;
                }
                row.trim_end().to_string()
            })
            .collect()
    }

    /// **光标要落在输入框里、落在刚打完那个字的后面。** 不摆这一下，人在框里打字、插入符
    /// 却停在屏幕左上角——那正是"初始输入位置对不上"。
    #[test]
    fn the_caret_sits_where_the_next_character_will_go() {
        let mut app = App::new(None, TuiMode::New, "seed");
        let height = viewport_height(&app, 60, 20, 0);

        // 一个字都没打：光标在框内第一格。
        let mut terminal = viewport(&app, 60, height);
        let empty = terminal.get_cursor_position().expect("有光标");
        assert_eq!(empty.x, 1, "框的左边框占掉第 0 列");
        assert_eq!(empty.y, height - 2, "框内第一行");

        // 中文一个字占两列，所以四个字之后是第 8 列，不是第 4 列。
        app.input.set("清理一下");
        let mut terminal = viewport(&app, 60, height);
        let typed = terminal.get_cursor_position().expect("有光标");
        assert_eq!(typed.x, 1 + 8, "宽字符按显示宽度算");
        assert_eq!(typed.y, empty.y);
    }

    /// 一行写不下就折到下一行，而且**光标跟着折**——框长高了却只画得下第一行，超出的那
    /// 半句就等于没写。
    #[test]
    fn a_long_line_wraps_inside_the_box_and_the_caret_follows() {
        let mut app = App::new(None, TuiMode::New, "seed");
        let long = "这是一句很长的输入会超过一行宽度继续写下去";
        app.input.set(long);

        let height = viewport_height(&app, 30, 20, 0);
        assert!(height > 4, "折行之后输入框要长高：{height}");
        let mut terminal = viewport(&app, 30, height);
        let screen = rows(terminal.backend().buffer()).join("\n");
        assert!(screen.contains("这是一句很长的输入"), "{screen}");
        assert!(
            screen.contains("继续写下去"),
            "折到下一行的那半句也要画出来：{screen}"
        );

        let caret = terminal.get_cursor_position().expect("有光标");
        assert!(caret.y > height - 3, "光标跟着折到后面的行：{caret:?}");
    }

    /// 弹窗开着时输入框禁用，光标就不该还留在屏幕上晃。
    #[test]
    fn the_caret_goes_away_while_the_approval_popup_is_open() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));
        let terminal = viewport(&app, 80, 24);
        assert_eq!(
            terminal.backend().buffer().area().height,
            24,
            "弹窗占满视口"
        );
        assert!(!app.input_enabled());
    }

    #[test]
    fn a_submitted_message_is_visible_before_the_event_stream_echoes_it() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        app.input.set("刚发出去的消息");
        let effects = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(effects.as_slice(), [Effect::Submit { .. }]));

        let screen = screen(&app, 80, 24);
        assert!(screen.contains("刚发出去的消息"), "{screen}");
        assert!(screen.contains("发送中"), "{screen}");
    }

    #[test]
    fn a_submit_failure_is_attached_to_the_message_that_failed() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        app.input.set("这条没有送到");
        let effects = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Effect::Submit { request_key, .. } = &effects[0] else {
            panic!("{effects:?}")
        };
        app.apply(ServerEvent::SubmitFailed {
            request_key: request_key.clone(),
            error: "Gateway 不可用".into(),
        });

        let screen = screen(&app, 80, 24);
        assert!(screen.contains("这条没有送到"), "{screen}");
        assert!(screen.contains("发送失败：Gateway 不可用"), "{screen}");
    }

    #[test]
    fn a_run_failure_shows_its_reason_on_screen() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
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

        let screen = screen(&app, 80, 24);
        assert!(screen.contains("上游拒绝了这次请求"), "{screen}");
    }

    /// 每一行都恰好占满缓冲区的宽度——没有一格越界，也没有一行被截短。
    fn assert_within(rows: &[String], width: u16) {
        for (index, row) in rows.iter().enumerate() {
            assert!(
                markdown::display_width(row) <= width as usize,
                "第 {index} 行宽 {}：{row}",
                markdown::display_width(row)
            );
        }
    }

    #[test]
    fn a_narrow_eighty_column_terminal_renders_within_its_bounds() {
        let app = conversation_app();
        let rows = screen_rows(&app, 80, 24);
        assert_eq!(rows.len(), 24);
        assert_within(&rows, 80);
        let screen = rows.join("\n");
        assert!(screen.contains("chat-a"), "{screen}");
        assert!(screen.contains("high"), "{screen}");
        assert!(screen.contains("已连接"), "{screen}");
        assert!(
            screen.contains("新会话"),
            "状态行上说得出这个界面是怎么开的：{screen}"
        );
        assert!(screen.contains("Enter 发送"), "{screen}");
    }

    #[test]
    fn a_wide_two_hundred_column_terminal_renders_within_its_bounds() {
        let app = conversation_app();
        let rows = screen_rows(&app, 200, 50);
        assert_eq!(rows.len(), 50);
        assert_within(&rows, 200);
        let screen = rows.join("\n");
        assert!(screen.contains("清理结果"), "{screen}");
        // 宽屏放得下表格与代码块。
        assert!(screen.contains("build/deps"), "{screen}");
        assert!(screen.contains("fn main()"), "{screen}");
        assert!(screen.contains("│"), "引用与表格的竖线：{screen}");
    }

    #[test]
    fn the_tool_line_shows_the_tool_its_summary_and_its_state() {
        let app = conversation_app();
        let screen = screen(&app, 100, 40);
        assert!(screen.contains("shell rm -rf build"), "{screen}");
        assert!(screen.contains("✔ shell"), "跑完了定住一个符号：{screen}");
    }

    #[test]
    fn an_uncertain_call_gets_its_own_mark_not_the_failure_one() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        let mut events = fixture::conversation();
        if let EventPayload::ToolResult(body) = &mut events[6].payload {
            body.status = komo_kernel::types::refs::ToolResultStatus::Uncertain;
        }
        feed(&mut app, &events[..7]);
        let screen = screen(&app, 100, 30);
        assert!(screen.contains("? shell"), "{screen}");
        assert!(!screen.contains("✖ shell"), "uncertain 不是失败：{screen}");
    }

    /// **在跑的东西要动。** 一块静止的"运行中"分不出还在跑还是卡住了；转圈不用读就知道
    /// 它还活着。这里断言的是"它真的会变"，不是它长什么样。
    #[test]
    fn a_running_turn_animates_on_the_status_line_and_the_tool_row() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        // 喂到「调用开跑」为止：Run 在跑，调用也在跑。
        feed(&mut app, &fixture::conversation()[..5]);

        let at = |app: &mut App, offset_ms: i64| {
            app.apply(ServerEvent::Tick(
                fixture::T0 + time::Duration::milliseconds(offset_ms),
            ));
            screen(app, 100, 20)
        };
        let first = at(&mut app, 0);
        let next = at(&mut app, crate::tui::spinner::FRAME_MS as i64);
        assert_ne!(first, next, "过了一帧，屏幕该变一下");

        // 转一圈回到原处——变的只是那一格，不是整屏在抖。
        let full = at(
            &mut app,
            crate::tui::spinner::FRAME_MS as i64 * crate::tui::spinner::FRAMES.len() as i64,
        );
        assert_eq!(first, full);
    }

    /// **一切正常的时候状态行不说话。**
    ///
    /// 跑着有转圈，跑完了屏幕上就是那段回答——"运行中" / "已完成" 都是让人读一件他正看
    /// 着的事。真有事的那几个状态照旧说出来。
    #[test]
    fn the_status_line_keeps_quiet_while_nothing_is_wrong() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        feed(&mut app, &fixture::conversation()[..3]);
        app.apply(ServerEvent::Tick(fixture::T0));
        let running = screen(&app, 100, 20);
        assert!(!running.contains("运行中"), "{running}");
        assert!(
            crate::tui::spinner::FRAMES
                .iter()
                .any(|frame| running.contains(frame)),
            "那一格得在转：{running}"
        );

        feed(&mut app, &fixture::conversation()[3..]);
        let done = screen(&app, 100, 20);
        assert!(!done.contains("已完成"), "{done}");
        for frame in crate::tui::spinner::FRAMES {
            assert!(!done.contains(frame), "跑完了就不转了：{done}");
        }
    }

    /// 落进回滚区的那一行**定住**：它是终端的了，不能带着某一帧随机的样子留在那里。
    #[test]
    fn a_finished_tool_row_stops_moving_once_it_lands() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        feed(&mut app, &fixture::conversation());

        let mut emitted = crate::tui::transcript::Emitted::new();
        app.apply(ServerEvent::Tick(fixture::T0));
        let settled = emitted.take(&app, 100);
        let text: String = settled
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(text.contains("✔ shell"), "跑完了是一个定住的符号：{text}");
        for frame in crate::tui::spinner::FRAMES {
            assert!(!text.contains(frame), "回滚区上不许有转圈的那一格：{text}");
        }
    }

    /// 开场横幅把「这是谁、在哪、哪个会话」说一次，然后它就是回滚区里普通的一行。
    #[test]
    fn the_banner_says_which_session_this_is() {
        let app = App::new(Some(fixture::session()), TuiMode::Resume, "seed");
        let text: String = banner(&app, "/home/u/project")
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(text.contains("续接会话"), "{text}");
        assert!(text.contains("/home/u/project"), "{text}");
        assert!(text.contains("sess-1"), "{text}");
    }

    #[test]
    fn the_approval_popup_shows_the_five_items_and_does_not_overflow() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        feed(&mut app, &fixture::conversation()[..4]);
        app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));

        // 够高的窗口：五项都在屏幕上。
        for (width, height) in [(80u16, 44u16), (200, 50)] {
            let rows = screen_rows(&app, width, height);
            assert_within(&rows, width);
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
            // 答案一行行列在弹窗底部的菜单里，行的尾巴上挂着它的直通键。
            assert!(
                screen.contains("本次任务默认通过（本次 Run 范围）"),
                "菜单：{screen}"
            );
            assert!(screen.contains("只批准本次调用"), "菜单：{screen}");
            assert!(screen.contains("拒绝（本次不执行）"), "菜单：{screen}");
            assert!(screen.contains("Enter 确认"), "底栏：{screen}");
        }
    }

    #[test]
    fn the_popup_keys_stay_visible_even_when_the_body_does_not_fit() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        feed(&mut app, &fixture::conversation()[..4]);
        app.apply(ServerEvent::Approval(Box::new(fixture::approval_record())));

        // 一个放不下正文的窄窗口：菜单与底栏钉在弹窗底部，正文滚不走它们。
        let rows = screen_rows(&app, 80, 24);
        assert_within(&rows, 80);
        let screen = rows.join("\n");
        assert!(screen.contains("本次任务默认通过"), "菜单：{screen}");
        assert!(screen.contains("拒绝（本次不执行）"), "菜单：{screen}");
        assert!(screen.contains("Enter 确认"), "底栏：{screen}");
        assert!(screen.contains("滚动"), "放不下要说还有多少行：{screen}");
        // 输入框在这么窄的窗口里被弹窗盖住了；它禁用这件事由状态说了算。
        assert!(!app.input_enabled());
    }

    #[test]
    fn a_draft_shows_on_screen_with_a_generating_marker_and_then_goes_away() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
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
        let drafting = screen(&app, 80, 24);
        assert!(drafting.contains("我来清一"), "{drafting}");
        assert!(drafting.contains("生成中"), "{drafting}");

        // 正式回复到了，草稿与记号一起消失，屏幕上只剩那一条。
        feed(&mut app, &fixture::conversation()[3..4]);
        let screen = screen(&app, 80, 24);
        assert!(!screen.contains("生成中"), "{screen}");
        assert_eq!(screen.matches("我来清一下。").count(), 1, "{screen}");
    }

    /// §8.4：状态行只说"等待中"等于没说——**理由要一起印出来**，否则"排队二十分钟不知道
    /// 为什么"还是没人答得上。理由住在 fold 的第二个维度里（`wait`），不在状态里。
    #[test]
    fn the_status_line_says_what_a_waiting_run_is_waiting_for() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        feed(&mut app, &fixture::conversation()[..3]);
        feed(
            &mut app,
            &[fixture::event(
                4,
                Some(fixture::run()),
                EventPayload::RunWaiting(RunWaiting {
                    reason: WaitReason::Retry {
                        attempts: 2,
                        not_before: fixture::T0 + time::Duration::seconds(30),
                        cause: RetryCause::RateLimited,
                    },
                }),
            )],
        );
        app.apply(ServerEvent::Tick(fixture::T0 + time::Duration::seconds(10)));

        let screen = screen(&app, 120, 20);
        assert!(screen.contains("等待中"), "{screen}");
        assert!(screen.contains("20s 后重试"), "理由要印出来：{screen}");
        assert!(screen.contains("限流"), "{screen}");
        assert!(screen.contains("第 2 次"), "{screen}");
    }

    /// 状态行上的条数是**三类合计**（§7.5）：只有审批那一种会被漏掉另外两条的等待。
    #[test]
    fn the_status_line_counts_every_kind_waiting_on_a_person() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        app.apply(ServerEvent::Pending(vec![
            fixture::intervention_summary(
                "7K2M",
                InterventionKind::Approval,
                "放行 rm -rf build？",
            ),
            fixture::intervention_summary(
                "run-1",
                InterventionKind::Verify,
                "上次那个调用发生了没有",
            ),
            fixture::intervention_summary("run-2", InterventionKind::Blocked, "会话已删除"),
        ]));
        let screen = screen(&app, 120, 20);
        assert!(screen.contains("待处理 3"), "{screen}");
    }

    #[test]
    fn a_reconnecting_subscription_says_so_on_the_status_line() {
        let mut app = conversation_app();
        app.apply(ServerEvent::Connection(ConnectionState::Reconnecting {
            attempt: 2,
            reason: "connection reset".into(),
        }));
        let screen = screen(&app, 80, 20);
        assert!(screen.contains("重连中 #2"), "{screen}");
    }

    #[test]
    fn the_command_palette_appears_while_a_slash_command_is_being_typed() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        app.input.set("/ap");
        let screen = screen(&app, 80, 20);
        assert!(screen.contains("/approve"), "{screen}");
        assert!(screen.contains("本次 Run 范围"), "{screen}");
    }

    #[test]
    fn a_folded_paste_shows_its_chip_not_its_content() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        app.handle_input(crate::tui::paste::InputEvent::Paste(
            "机密第一行\n机密第二行\n机密第三行\n机密第四行".into(),
        ));
        let screen = screen(&app, 80, 20);
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
        let rows = screen_rows(&app, 20, 10);
        assert_eq!(rows.len(), 10);
        assert_within(&rows, 20);
    }

    /// 视口不许把整个终端占满：上面总要留得下前面说过的话。
    #[test]
    fn the_viewport_never_eats_the_whole_terminal() {
        let mut app = conversation_app();
        app.input.set("很长的一段草稿".repeat(40));
        for height in [10u16, 24, 50] {
            let want = viewport_height(&app, 80, height.saturating_sub(1), 40);
            assert!(want < height, "{height} 行的终端要给上文留位置：{want}");
        }
    }
}
