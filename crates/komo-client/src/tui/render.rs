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
//! │ 命令面板（只在输入以 `/` 开头时出现）/ `/model` 的选择菜单
//! │ 输入框：上下两条横线夹着（照 Claude Code），会折行，光标摆在它真正在的那一格上
//! ╰ 操作提示
//! ```
//!
//! [`viewport_height`] 与 [`draw`] 必须算出同一个高度——驱动照前者开视口、照后者画，
//! 两边差一行，输入框就会被切掉一条边。所以两边共用底下那几个 `*_height`。

use komo_kernel::types::status::RunState;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use crate::tui::app::{App, TokenTally, status_text};
pub(crate) use crate::tui::markdown;
use crate::tui::model_picker::{ModelPicker, entry_detail};

/// 还在动的那一段最多占几行。
///
/// 它有上限是因为**视口高度一变，底下那块就要重开一次**：不封顶的话，模型每吐出一行
/// 都是一次重开。封了顶之后，一轮对话里最多长这么多次，长到头就不动了。
pub const LIVE_MAX: u16 = 8;
/// 输入框里最多显示几行（正文再长也只滚动，不把视口顶穿）。
pub const INPUT_MAX_ROWS: u16 = 8;
/// 命令面板最多列几条。
const PALETTE_MAX: u16 = 6;
/// 模型选择菜单最多列几行（再多就跟着高亮滚）。
const PICKER_MAX: u16 = 8;
/// Shell 模式（`!`）的强调色。
const SHELL_COLOUR: Color = Color::Magenta;

/// 视口要多高。`max` 是终端高度减去留给上文的那一行。
pub fn viewport_height(app: &App, width: u16, max: u16, live_lines: usize) -> u16 {
    let max = max.max(1);
    let live = (live_lines as u16).min(LIVE_MAX);
    (live + 1 + palette_height(app) + input_height(app, width)).min(max)
}

/// 画一帧。`live` 是 [`crate::tui::transcript::Emitted::live`] 给的那一段。
pub fn draw(frame: &mut Frame<'_>, app: &App, live: &[Line<'static>]) {
    let area = frame.area();

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
    frame.render_widget(status_line(app, area.width), chunks[1]);
    if let Some(picker) = app.model_picker.as_ref() {
        let current = app.model.as_deref();
        frame.render_widget(picker_block(picker, current, chunks[2]), chunks[2]);
    } else if palette_height > 0 {
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
    if let Some(picker) = app.model_picker.as_ref() {
        return (picker.entries.len() as u16).min(PICKER_MAX);
    }
    (app.palette().len() as u16).min(PALETTE_MAX)
}

/// 输入框行首的提示符（照 Claude Code）；折出来的后续行缩进同样宽。
const PROMPT: &str = "❯ ";
/// 输入框左右各留一列，与上面那段正文、状态行对齐。
const INPUT_PAD: u16 = 1;

/// 输入框连上下两条线、外加底下那行提示一共几行。提示行空着也占位：提示一出一没就改
/// 视口高度，等于让底下那块整个重开一次。
fn input_height(app: &App, width: u16) -> u16 {
    (input_rows(app, width).len() as u16).clamp(1, INPUT_MAX_ROWS) + 3
}

/// 输入框里折好的每一行，附它在 [`crate::tui::paste::Input::display`] 那份文本里的起始
/// 偏移——光标靠它落位。
fn input_rows(app: &App, width: u16) -> Vec<(usize, String)> {
    let inner = width
        .saturating_sub(2 * INPUT_PAD + markdown::display_width(PROMPT) as u16)
        .max(1) as usize;
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
    let visible = area.height.saturating_sub(3).max(1) as usize;
    let offset = (caret_row + 1).saturating_sub(visible);

    let style = if enabled {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    // Shell 模式（草稿以 `!` 开头）：那个 `!`、提示符与横线换个颜色，一眼看得出这一条不经
    // 模型。
    let shell = app.shell_mode();
    let accent = if !enabled {
        Color::Yellow
    } else if shell {
        SHELL_COLOUR
    } else {
        Color::DarkGray
    };
    let indent = " ".repeat(markdown::display_width(PROMPT));
    let body: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(index, (at, line))| {
            let lead = if index == 0 {
                Span::styled(PROMPT, Style::default().fg(accent))
            } else {
                Span::raw(indent.clone())
            };
            match line.strip_prefix('!') {
                Some(rest) if shell && *at == 0 => Line::from(vec![
                    lead,
                    Span::styled(
                        "!",
                        Style::default()
                            .fg(SHELL_COLOUR)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(rest.to_string(), style),
                ]),
                _ => Line::from(vec![lead, Span::styled(line.clone(), style)]),
            }
        })
        .collect();

    let [field, hint_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(accent))
        .padding(Padding::horizontal(INPUT_PAD));
    let inner = block.inner(field);
    frame.render_widget(Paragraph::new(body).block(block), field);

    let hint = app.input_hint();
    frame.render_widget(
        Paragraph::new(Span::styled(
            markdown::truncate_to_width(
                &hint,
                hint_area.width.saturating_sub(2 * INPUT_PAD) as usize,
            ),
            Style::default().fg(Color::DarkGray),
        ))
        .block(Block::default().padding(Padding::horizontal(INPUT_PAD))),
        hint_area,
    );

    let prompt = markdown::display_width(PROMPT) as u16;
    if enabled && inner.width > prompt && inner.height > 0 {
        frame.set_cursor_position(Position {
            x: inner.x + prompt + (caret_column as u16).min(inner.width - prompt - 1),
            y: inner.y + (caret_row.saturating_sub(offset) as u16).min(inner.height - 1),
        });
    }
}

/// 状态行。
///
/// **左边是"此刻在发生什么"，右边是"这一摊是什么"**，中间撑开：
///
/// ```text
///  ⠋ 0.5s      opus · 已连接 · ↑38 ↓7.6k R1.4M W108k CH99.5%
/// ```
///
/// 右边三格固定，位置不动，眼睛不用每次重新找；左边只在**真有事**的时候说话——跑着有
/// 转圈，跑完了屏幕上就是那段回答，"空闲 / 运行中 / 已完成"都是让人读一件他正看着的事。
/// 但停着不动的那几个要说，而且要说清在等谁（§8.4：排队二十分钟不知道为什么，就是少了
/// 这一格）。
fn status_line(app: &App, width: u16) -> Paragraph<'static> {
    // 左边永远先空一格：有没有转圈，后面那句话的起点都在同一列。
    let mut left: Vec<Span<'static>> = vec![Span::raw(" ")];

    if app.phase.is_backfilling() {
        left.push(Span::styled(
            " 补读历史 ",
            Style::default().fg(Color::Black).bg(Color::LightBlue),
        ));
    }

    if let Some(state) = app.run_state() {
        if !state.is_terminal() {
            left.push(Span::styled(
                format!("{} ", crate::tui::spinner::frame(app.now)),
                Style::default().fg(run_colour(state)),
            ));
        }
        // 一切正常的三个状态不写出来（见函数头上那段）。
        if !matches!(state, RunState::Running | RunState::Completed) {
            left.push(Span::styled(
                format!("{} ", status_text(state)),
                Style::default()
                    .fg(run_colour(state))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(reason) = app.run_wait() {
            left.push(Span::styled(
                format!("{} ", crate::tui::app::wait_text(reason, app.now)),
                Style::default().fg(Color::Yellow),
            ));
        }
        if let Some(elapsed) = app.elapsed() {
            left.push(Span::styled(
                pretty_duration(elapsed),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    let mut right: Vec<Span<'static>> = Vec::new();
    right.push(Span::styled(
        app.model_label(),
        Style::default().fg(Color::Gray),
    ));

    let connection = &app.connection;
    right.push(Span::styled(
        format!(" · {}", connection.label()),
        Style::default().fg(if connection.is_connected() {
            Color::Green
        } else {
            Color::Yellow
        }),
    ));

    right.push(Span::styled(
        format!(" · {}", token_cell(&app.tokens)),
        Style::default().fg(Color::DarkGray),
    ));

    if app.pending_count() > 0 {
        // 有人在等回答——这一格不属于"这一摊是什么"，它是此刻最要紧的事，所以顶在左边。
        left.insert(
            0,
            Span::styled(
                format!(" 待处理 {} ", app.pending_count()),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        );
    }

    Paragraph::new(Line::from(justify(left, right, width)))
}

/// 左一摞、右一摞，中间用空格撑开到 `width`。
///
/// 放不下就**截左边**：右边那三格是固定要看的东西，左边那句话截掉的是后半段。
fn justify(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: u16) -> Vec<Span<'static>> {
    let width = width as usize;
    let right_width: usize = right.iter().map(|span| span.width()).sum();
    let room = width.saturating_sub(right_width);

    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in left {
        let left_room = room.saturating_sub(used);
        if left_room == 0 {
            break;
        }
        let text = markdown::truncate_to_width(span.content.as_ref(), left_room);
        if text.is_empty() {
            continue;
        }
        used += markdown::display_width(&text);
        out.push(Span::styled(text, span.style));
    }
    out.push(Span::raw(" ".repeat(room.saturating_sub(used))));
    out.extend(right);
    out
}

/// token 那一格，照 pi 的页脚：`↑38 ↓7.6k R1.4M W108k CH99.5%`。
///
/// `↑` 只算没走缓存的输入，`R` / `W` 是缓存读写，四个加起来才是模型吃进去的；`CH` 是
/// 最近一轮的缓存命中率（[`TokenTally`]）。`↑↓` 总在，缓存那几格这家模型报了才出现。
fn token_cell(tokens: &TokenTally) -> String {
    let mut cell = format!("↑{} ↓{}", compact(tokens.input), compact(tokens.output));
    if tokens.cache_read > 0 {
        cell.push_str(&format!(" R{}", compact(tokens.cache_read)));
    }
    if tokens.cache_write > 0 {
        cell.push_str(&format!(" W{}", compact(tokens.cache_write)));
    }
    if let Some(rate) = tokens.hit_rate() {
        cell.push_str(&format!(" CH{rate:.1}%"));
    }
    cell
}

/// 一个大数写短一点（同 pi）：`980` · `7.6k` · `108k` · `1.4M` · `12M`。
fn compact(count: u64) -> String {
    let n = count as f64;
    if count < 1_000 {
        count.to_string()
    } else if count < 9_950 {
        format!("{:.1}k", n / 1_000.0)
    } else if count < 999_500 {
        format!("{:.0}k", n / 1_000.0)
    } else if count < 9_950_000 {
        format!("{:.1}M", n / 1_000_000.0)
    } else {
        format!("{:.0}M", n / 1_000_000.0)
    }
}

/// 开场横幅：进 TUI 时印一次，之后它就是终端回滚区里普通的两行。
///
/// 会话 Id 在这里"闪过一次"；`komo` 裸命令这时还没有会话，等第一条消息把它铸出来时由
/// 一条提示补上。
pub fn banner(app: &App, cwd: &str) -> Vec<Line<'static>> {
    let mut head = vec![
        Span::styled(
            " komo ",
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
        RunState::Cancelled | RunState::Abandoned | RunState::Absorbed => Color::DarkGray,
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

/// `/model` 的选择菜单：一行一个模型，高亮那一行是实心色块。
///
/// ```text
///  ▸ 1 deepseek   deepseek-flash · deepseek · effort low/high   当前
///    2 main       stealth/union-alpha · openrouter · 无 effort   默认
/// ```
///
/// 行数超过 `area` 时只画高亮附近那一段——高亮永远看得见。
fn picker_block(picker: &ModelPicker, current: Option<&str>, area: Rect) -> Paragraph<'static> {
    let height = area.height as usize;
    let width = area.width as usize;
    let start = (picker.selected + 1).saturating_sub(height.max(1));
    let id_width = picker
        .entries
        .iter()
        .map(|entry| markdown::display_width(&entry.id))
        .max()
        .unwrap_or(0);
    let lines: Vec<Line<'static>> = picker
        .entries
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(index, entry)| {
            let chosen = index == picker.selected;
            let style = if chosen {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let dim = if chosen {
                style
            } else {
                Style::default().fg(Color::DarkGray)
            };
            // 当前 = `/model` 设过的那个；没设过，网关的 default 就是当前。
            let is_current = match current {
                Some(id) => entry.id == id,
                None => entry.default,
            };
            let mark = if is_current {
                "当前"
            } else if entry.default {
                "默认"
            } else {
                ""
            };
            let head = format!(
                "{}{} {}",
                if chosen { "▸ " } else { "  " },
                ModelPicker::shortcut(index).unwrap_or(' '),
                entry.id
            );
            let pad = id_width.saturating_sub(markdown::display_width(&entry.id)) + 2;
            // 行首 + 对齐空格 + 说明 + 标记；说明放不下就截它，id 和标记不让。
            let used = markdown::display_width(&head) + pad + markdown::display_width(mark) + 3;
            let detail =
                markdown::truncate_to_width(&entry_detail(entry), width.saturating_sub(used));
            let fill = width
                .saturating_sub(used)
                .saturating_sub(markdown::display_width(&detail))
                + 1;
            Line::from(vec![
                Span::styled(head, style),
                Span::styled(" ".repeat(pad), style),
                Span::styled(detail, dim),
                Span::styled(" ".repeat(fill), style),
                Span::styled(
                    mark.to_string(),
                    style.fg(if chosen { Color::Black } else { Color::Yellow }),
                ),
                Span::styled("  ", style),
            ])
        })
        .collect();
    Paragraph::new(lines)
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
                cache_read_tokens: None,
                cache_write_tokens: None,
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
        assert_eq!(empty.x, 3, "左边留一列，再让过 `❯ `");
        assert_eq!(empty.y, height - 3, "两条横线中间那一行，底下还有提示行");

        // 中文一个字占两列，所以四个字之后是第 8 列，不是第 4 列。
        app.input.set("清理一下");
        let mut terminal = viewport(&app, 60, height);
        let typed = terminal.get_cursor_position().expect("有光标");
        assert_eq!(typed.x, 3 + 8, "宽字符按显示宽度算");
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
        assert!(caret.y > height - 4, "光标跟着折到后面的行：{caret:?}");
    }

    #[test]
    fn the_model_picker_lists_every_model_and_marks_the_current_one() {
        use komo_kernel::protocol::http::ModelMenuEntry;
        use komo_kernel::types::model::Effort;
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for ch in "model".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.apply(ServerEvent::ModelMenu(vec![
            ModelMenuEntry {
                id: "main".into(),
                model: "stealth/union-alpha".into(),
                provider: "openrouter".into(),
                efforts: vec![Effort::new("low"), Effort::new("medium")],
                default: true,
                ..ModelMenuEntry::default()
            },
            ModelMenuEntry {
                id: "deepseek".into(),
                model: "deepseek-flash".into(),
                provider: "deepseek".into(),
                ..ModelMenuEntry::default()
            },
        ]));
        let rows = screen_rows(&app, 80, 24);
        assert_within(&rows, 80);
        let screen = rows.join("\n");
        assert!(screen.contains("▸ 1 main"), "{screen}");
        assert!(screen.contains("2 deepseek"), "{screen}");
        assert!(screen.contains("stealth/union-alpha"), "{screen}");
        assert!(screen.contains("当前"), "{screen}");
        assert!(screen.contains("Enter 确认"), "{screen}");
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
        // 状态行右边固定这三格，别的都不占地方。
        assert!(screen.contains("chat-a"), "模型：{screen}");
        assert!(screen.contains("已连接"), "连接：{screen}");
        assert!(screen.contains("↑"), "这一段花了多少 token：{screen}");
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
        assert!(screen.contains("bash $ rm -rf build"), "{screen}");
        assert!(screen.contains("✔ bash"), "跑完了定住一个符号：{screen}");
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
        assert!(screen.contains("? bash"), "{screen}");
        assert!(!screen.contains("✖ bash"), "uncertain 不是失败：{screen}");
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

    /// 状态行右边那三格**贴着右边对齐**，位置不动——眼睛不用每次重新找。
    #[test]
    fn the_three_fields_on_the_right_stay_put() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        feed(&mut app, &fixture::conversation());
        app.apply(ServerEvent::Connection(ConnectionState::Connected));

        for width in [60u16, 100, 200] {
            let rows = screen_rows(&app, width, 20);
            let status = rows
                .iter()
                .find(|row| row.contains("已连接"))
                .unwrap_or_else(|| panic!("{width} 列下找不到状态行：{rows:?}"));
            assert_eq!(
                markdown::display_width(status),
                width as usize,
                "{width} 列：右边那三格要顶到最右边——{status:?}"
            );
            assert!(status.contains("chat-a"), "{status}");
            assert!(status.contains("↑"), "{status}");
        }
    }

    /// 这一段对话花了多少 token 是**累加**出来的：模型每答一轮报一次。
    #[test]
    fn the_token_count_adds_up_over_the_session() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        assert_eq!(app.tokens, TokenTally::default());
        feed(&mut app, &fixture::conversation());
        assert!(app.tokens.input > 0, "答过话就该有数");

        // 同一条事件再来一遍（补读与订阅万一重叠）不许加两遍。
        let before = app.tokens;
        feed(&mut app, &fixture::conversation());
        assert_eq!(app.tokens, before);
    }

    /// token 那一格照 pi 的页脚写：`↑` 只算没走缓存的输入，缓存读写与最近一轮的命中率
    /// 各占一格。
    #[test]
    fn the_token_cell_reads_like_the_pi_footer() {
        let tokens = TokenTally {
            input: 38,
            output: 7_600,
            cache_read: 1_400_000,
            cache_write: 108_000,
            last_round: Some((199, 200)),
        };
        assert_eq!(token_cell(&tokens), "↑38 ↓7.6k R1.4M W108k CH99.5%");

        // 没报过缓存的模型只有 ↑↓，不挂一个 0% 冒充"缓存坏了"。
        let plain = TokenTally {
            input: 320,
            output: 50,
            ..TokenTally::default()
        };
        assert_eq!(token_cell(&plain), "↑320 ↓50");
    }

    #[test]
    fn big_numbers_round_like_pi() {
        let cases = [
            (980, "980"),
            (7_640, "7.6k"),
            (9_960, "10k"),
            (108_000, "108k"),
            (999_600, "1.0M"),
            (1_420_000, "1.4M"),
            (12_300_000, "12M"),
        ];
        for (count, want) in cases {
            assert_eq!(compact(count), want, "{count}");
        }
    }

    /// 模型报了缓存，状态行上就看得见：两轮各报一次，`↑` 是两轮没命中的部分之和，`CH`
    /// 是第二轮的。
    #[test]
    fn the_status_line_shows_the_cache_once_the_model_reports_it() {
        let mut events = fixture::conversation();
        let rounds = [(1_510, 7, 0, 1_500), (1_550, 9, 1_500, 30)];
        let mut bodies = events
            .iter_mut()
            .filter_map(|event| match &mut event.payload {
                EventPayload::MessageAssistant(body) => Some(body),
                _ => None,
            });
        for (input, output, read, write) in rounds {
            let body = bodies.next().expect("fixture 里有两轮回答");
            body.input_tokens = Some(input);
            body.output_tokens = Some(output);
            body.cache_read_tokens = Some(read);
            body.cache_write_tokens = Some(write);
        }
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        feed(&mut app, &events);
        app.apply(ServerEvent::Connection(ConnectionState::Connected));

        let rows = screen_rows(&app, 100, 20);
        let status = rows
            .iter()
            .find(|row| row.contains("已连接"))
            .unwrap_or_else(|| panic!("找不到状态行：{rows:?}"));
        assert!(
            status.ends_with("↑30 ↓16 R1.5k W1.5k CH96.8%"),
            "{status:?}"
        );
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
        assert!(text.contains("✔ bash"), "跑完了是一个定住的符号：{text}");
        for frame in crate::tui::spinner::FRAMES {
            assert!(!text.contains(frame), "回滚区上不许有转圈的那一格：{text}");
        }
    }

    /// 开场横幅把「在哪、哪个会话」说一次，然后它就是回滚区里普通的一行。
    #[test]
    fn the_banner_says_which_session_this_is() {
        let app = App::new(Some(fixture::session()), TuiMode::Resume, "seed");
        let text: String = banner(&app, "/home/u/project")
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(text.contains("/home/u/project"), "{text}");
        assert!(text.contains("sess-1"), "{text}");
    }

    /// 转圈只有状态行那一格：草稿下面不再另起一行"⠼ 生成中"。
    #[test]
    fn a_draft_shows_on_screen_with_one_spinner_and_then_goes_away() {
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
        app.apply(ServerEvent::Tick(fixture::T0));
        let drafting = screen(&app, 80, 24);
        assert!(drafting.contains("我来清一"), "{drafting}");
        assert!(!drafting.contains("生成中"), "{drafting}");
        let spinning: usize = crate::tui::spinner::FRAMES
            .iter()
            .map(|frame| drafting.matches(frame).count())
            .sum();
        assert_eq!(spinning, 1, "只有状态行那一格在转：{drafting}");

        // 正式回复到了，草稿消失，屏幕上只剩那一条。
        feed(&mut app, &fixture::conversation()[3..4]);
        let screen = screen(&app, 80, 24);
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

    /// 状态行上的条数是**两类合计**（§7.5）：结果不明与阻塞都在等人。
    #[test]
    fn the_status_line_counts_every_kind_waiting_on_a_person() {
        let mut app = App::new(Some(fixture::session()), TuiMode::New, "seed");
        app.apply(ServerEvent::Pending(vec![
            fixture::intervention_summary(
                "run-1",
                InterventionKind::Verify,
                "上次那个调用发生了没有",
            ),
            fixture::intervention_summary("run-2", InterventionKind::Blocked, "会话已删除"),
        ]));
        let screen = screen(&app, 120, 20);
        assert!(screen.contains("待处理 2"), "{screen}");
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
        app.input.set("/pe");
        let screen = screen(&app, 80, 20);
        assert!(screen.contains("/pending"), "{screen}");
        assert!(screen.contains("结果不明 / 阻塞"), "{screen}");
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
