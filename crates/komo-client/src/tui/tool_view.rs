//! 一次工具调用画成哪几行。
//!
//! 照 pi 的做法**按工具分开画**。原来一次调用只有一行摘要：长命令截在一屏宽，多行脚本
//! 压成一行 JSON，结果只剩一个耗时，看不出这次调用到底动了什么、得到了什么。现在分三块：
//!
//! - **头一行**说这次调用动了什么：`bash` 是 `$ 命令`，`read` 是 `路径:起-止`，`rg` 是
//!   模式与目录。按宽度折行，不截在一屏宽；
//! - **正文**是它带的那段内容：多行命令与脚本的续行、`write` 写进去的内容、`edit` 的
//!   diff；
//! - **结果**以 `⎿` 起头：`bash`（旧记录叫 `shell`）留尾，其他留头。成功的 `read` / `edit` / `write` 不印——
//!   前者就是文件本身，后两者的回执头一行已经说了；失败了照印，那是原因。
//!
//! 折叠时结果最多 [`RESULT_LINES`] 行、头一行与正文最多 [`BODY_LINES`] 行（照 Claude
//! Code：一轮几次调用落进回滚区，每次只占几行），超出的说一句还有几行、按什么展开，每行
//! 截在一屏宽；`Ctrl-T` 打开就全给、长行折行，并补上头一行与正文没用到的参数——已经印出去的那些
//! 也一起重画（见 [`crate::tui::transcript`]）。**`edit` 的 diff 不折**：改了什么就是这次
//! 调用的全部内容，折掉一半等于没给。
//!
//! 结果正文是工具写的那段完整正文（[`ToolLine::preview`]），不是事件顶层那份 1 KiB 预览。

use komo_kernel::types::status::ToolCallState;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use time::OffsetDateTime;

use crate::tui::app::ToolLine;
use crate::tui::transcript::state_colour;
use crate::tui::{markdown, spinner};

/// 折叠时结果最多几行。
pub const RESULT_LINES: usize = 3;

/// 折叠时头一行（折出来的续行）与正文各最多几行。
pub const BODY_LINES: usize = 4;

/// 头一行以下那几块缩进多少。
const INDENT: &str = "    ";

/// 结果那一块的第一行，与 [`INDENT`] 同宽。
const RESULT_MARK: &str = "  ⎿ ";

/// diff 里一段没改的行留几行上下文（pi 也是 4）。
const DIFF_CONTEXT: usize = 4;

/// 新旧两段行数的乘积超过它就不逐行比对，整段先删后加——一次 edit 很少这么大。
const DIFF_CELLS: usize = 250_000;

/// 一次调用画成的全部行。`now` 是 `None` 就不转——那是要冻进回滚区的行。
pub fn lines(
    tool: &ToolLine,
    detail: bool,
    width: u16,
    now: Option<OffsetDateTime>,
) -> Vec<Line<'static>> {
    let width = width as usize;
    let view = View::of(tool, detail);
    let mut out = head_lines(tool, &view.head, detail, width, now);
    match &view.body {
        Body::None => {}
        Body::Text(text) => out.extend(block(
            text,
            Keep::Head,
            BODY_LINES,
            detail,
            width,
            INDENT,
            Style::default().fg(Color::DarkGray),
        )),
        Body::Diff { old, new } => out.extend(diff_lines(old, new, detail, width)),
    }
    if detail {
        out.extend(extra_args(&tool.args, &view.used, width));
    }
    out.extend(result_lines(tool, view.result, detail, width));
    out
}

/// 这次调用是不是"看一眼"：读文件、搜一下。连着的几次在回滚区上收成一行
/// （[`group_lines`]）。
pub fn explores(tool: &ToolLine) -> bool {
    matches!(tool.tool.as_str(), "read" | "rg")
}

/// 连着的读与搜收成的一行（照 Claude Code）：读了几个文件、搜了几次。一轮里并发的五六次
/// 读文件各占一行，看的人只关心"它去翻过了"；要看翻了哪些，`Ctrl-T` 逐条展开。
pub fn group_lines(
    tools: &[&ToolLine],
    width: u16,
    now: Option<OffsetDateTime>,
) -> Vec<Line<'static>> {
    let running = tools.iter().any(|tool| !tool.state.is_terminal());
    let state = if running {
        ToolCallState::Started
    } else {
        ToolCallState::Completed
    };
    let style = Style::default().fg(state_colour(state));
    let (mark, mark_style) = match (running, now) {
        (false, _) => ("✔", style),
        (true, None) => (spinner::STILL, style),
        (true, Some(_)) if spinner::lit(now) => (spinner::PULSE, style),
        (true, Some(_)) => (spinner::PULSE, style.add_modifier(Modifier::DIM)),
    };
    let files: std::collections::BTreeSet<&str> = tools
        .iter()
        .filter(|tool| tool.tool == "read")
        .map(|tool| {
            tool.args
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
        })
        .collect();
    let searches = tools.iter().filter(|tool| tool.tool == "rg").count();
    let (read, search) = if running {
        ("读取", "搜索")
    } else {
        ("读了", "搜了")
    };
    let mut parts = Vec::new();
    if !files.is_empty() {
        parts.push(format!("{read} {} 个文件", files.len()));
    }
    if searches > 0 {
        parts.push(format!("{search} {searches} 次"));
    }
    let mut text = format!(" {}", parts.join("、"));
    if running {
        text.push('…');
    }
    vec![fit_spans(
        vec![
            Span::styled(format!("  {mark}"), mark_style),
            Span::raw(text),
            Span::styled("（Ctrl-T 展开）", Style::default().fg(Color::DarkGray)),
        ],
        width as usize,
    )]
}

/// 一次调用按它是哪个工具拆开的样子。
struct View {
    /// 头一行：这次调用动了什么。
    head: String,
    body: Body,
    /// 头一行与正文已经用掉的参数；`Ctrl-T` 打开时其余的逐个列出来。
    used: Vec<&'static str>,
    /// 成功时结果印哪一头。
    result: Keep,
}

enum Body {
    None,
    /// 一段原样印出来的正文：命令与脚本的续行、要写进文件的内容。
    Text(String),
    /// `edit` 的 `match_text` → `replace_text`。
    Diff {
        old: String,
        new: String,
    },
}

/// 一段正文超出折叠的行数时留哪一头。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keep {
    Head,
    /// 命令的输出看尾巴：最后那几行是结果，前面是过程。
    Tail,
    /// 成功时不印。
    Hidden,
}

impl View {
    fn of(tool: &ToolLine, detail: bool) -> Self {
        let args = &tool.args;
        let text = |key: &str| args.get(key).and_then(Value::as_str);
        match tool.tool.as_str() {
            "shell" | "bash" => {
                let command = text("command").unwrap_or_default();
                let (first, rest) = command.split_once('\n').unwrap_or((command, ""));
                View {
                    head: format!("$ {first}"),
                    body: text_body(rest),
                    used: vec!["command"],
                    result: Keep::Tail,
                }
            }
            "read" => {
                let number = |key: &str| args.get(key).and_then(Value::as_u64);
                let range = match (number("start_line"), number("line_count")) {
                    (Some(start), Some(count)) => {
                        format!(":{start}-{}", start + count.saturating_sub(1))
                    }
                    (Some(start), None) => format!(":{start}-"),
                    (None, Some(count)) => format!(":1-{count}"),
                    (None, None) => String::new(),
                };
                View {
                    head: format!("{}{range}", text("path").unwrap_or_default()),
                    body: Body::None,
                    used: vec!["path", "start_line", "line_count"],
                    result: Keep::Hidden,
                }
            }
            "rg" => {
                let mut head = text("pattern").unwrap_or_default().to_string();
                for key in ["path", "glob"] {
                    if let Some(value) = text(key) {
                        head.push_str(" · ");
                        head.push_str(value);
                    }
                }
                View {
                    head,
                    body: Body::None,
                    used: vec!["pattern", "path", "glob"],
                    result: Keep::Head,
                }
            }
            "edit" => {
                let mut head = text("path").unwrap_or_default().to_string();
                if args.get("replace_all").and_then(Value::as_bool) == Some(true) {
                    head.push_str("（全部替换）");
                }
                View {
                    head,
                    body: Body::Diff {
                        old: text("match_text").unwrap_or_default().to_string(),
                        new: text("replace_text").unwrap_or_default().to_string(),
                    },
                    used: vec!["path", "match_text", "replace_text", "replace_all"],
                    result: Keep::Hidden,
                }
            }
            "write" => View {
                head: text("path").unwrap_or_default().to_string(),
                body: text_body(text("content").unwrap_or_default()),
                used: vec!["path", "content"],
                result: Keep::Hidden,
            },
            // 历史记录里的 python 调用（2026-10-15 工具删除）照旧渲染。
            "python" if text("code").is_some() => View {
                head: String::new(),
                body: text_body(text("code").unwrap_or_default()),
                used: vec!["mode", "code"],
                result: Keep::Head,
            },
            "codemode" => View {
                head: String::new(),
                body: text_body(text("code").unwrap_or_default()),
                used: vec!["code"],
                result: Keep::Head,
            },
            _ => match tool.summary_key() {
                Some(key) => View {
                    head: tool.summary(),
                    body: Body::None,
                    used: vec![key],
                    result: Keep::Head,
                },
                // 认不出哪个参数是主角：折叠时整个压成一行，展开时逐个列出来。
                None => View {
                    head: if detail {
                        String::new()
                    } else {
                        tool.summary()
                    },
                    body: Body::None,
                    used: vec![],
                    result: Keep::Head,
                },
            },
        }
    }
}

fn text_body(text: &str) -> Body {
    if text.trim().is_empty() {
        Body::None
    } else {
        Body::Text(text.to_string())
    }
}

/// 头一行：记号、工具名、动了什么，跑完了再在末尾补一个耗时。按宽度折行，续行缩进；
/// 折叠时最多 [`BODY_LINES`] 行。
fn head_lines(
    tool: &ToolLine,
    summary: &str,
    detail: bool,
    width: usize,
    now: Option<OffsetDateTime>,
) -> Vec<Line<'static>> {
    // 只有记号带状态色，工具名加粗、其余用终端本色（照 Claude Code）：整行上色会和下面
    // diff 的红绿撞在一起。
    let colour = if tool.detached {
        Color::Cyan
    } else {
        state_colour(tool.state)
    };
    let mut mark = Style::default().fg(colour);
    if tool.state == ToolCallState::Started && !spinner::lit(now) {
        mark = mark.add_modifier(Modifier::DIM);
    }
    let summary = clean(summary);
    let mut spans = vec![
        Span::styled(format!("  {}", tool.marker(now)), mark),
        Span::styled(
            format!(" {}", tool.tool),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if !summary.trim().is_empty() {
        spans.push(Span::raw(format!(" {}", summary.trim_end())));
    }
    let mut rows = wrap_spans(spans, width, INDENT);
    if !detail && rows.len() > BODY_LINES {
        rows.truncate(BODY_LINES);
        if let Some(last) = rows.pop() {
            let mut spans = last.spans;
            spans.push(Span::raw("…"));
            rows.push(fit_spans(spans, width));
        }
    }
    if tool.elapsed_ms > 0 && tool.state.is_terminal() {
        let took = Span::styled(
            format!("  {}", elapsed(tool.elapsed_ms)),
            Style::default().fg(Color::DarkGray),
        );
        match rows.last_mut() {
            Some(last) if last.width() + took.width() <= width => last.spans.push(took),
            _ => rows.push(Line::from(vec![Span::raw(INDENT), took])),
        }
    }
    rows
}

/// 结果那一块。
fn result_lines(tool: &ToolLine, keep: Keep, detail: bool, width: usize) -> Vec<Line<'static>> {
    let Some(text) = tool
        .preview
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    else {
        return Vec::new();
    };
    let failed = matches!(tool.state, ToolCallState::Failed | ToolCallState::Uncertain);
    let keep = match keep {
        Keep::Hidden if failed || detail => Keep::Head,
        Keep::Hidden => return Vec::new(),
        keep => keep,
    };
    let style = if failed {
        Style::default().fg(state_colour(tool.state))
    } else {
        Style::default().fg(Color::Gray)
    };
    block(text, keep, RESULT_LINES, detail, width, RESULT_MARK, style)
}

/// 一段多行正文：折叠时留一头、说一句还有几行；第一行顶 `first`，其余顶 [`INDENT`]。
fn block(
    text: &str,
    keep: Keep,
    limit: usize,
    detail: bool,
    width: usize,
    first: &str,
    style: Style,
) -> Vec<Line<'static>> {
    let mut lines: Vec<String> = text.lines().map(clean).collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let more = Style::default().fg(Color::DarkGray);
    let mut rows: Vec<(String, Style)> = Vec::new();
    if detail || lines.len() <= limit {
        rows.extend(lines.into_iter().map(|line| (line, style)));
    } else if keep == Keep::Tail {
        let hidden = lines.len() - limit;
        rows.push((format!("… 前面还有 {hidden} 行（Ctrl-T 展开）"), more));
        rows.extend(lines.drain(hidden..).map(|line| (line, style)));
    } else {
        let hidden = lines.len() - limit;
        rows.extend(lines.drain(..limit).map(|line| (line, style)));
        rows.push((format!("… 还有 {hidden} 行（Ctrl-T 展开）"), more));
    }
    rows.into_iter()
        .enumerate()
        .flat_map(|(index, (line, style))| {
            let lead = if index == 0 { first } else { INDENT };
            row(
                vec![Span::raw(lead.to_string()), Span::styled(line, style)],
                detail,
                width,
                INDENT,
            )
        })
        .collect()
}

/// `Ctrl-T` 打开时，头一行与正文没用到的那些参数：`键: 值`，字符串原样多行。
fn extra_args(args: &Value, used: &[&'static str], width: usize) -> Vec<Line<'static>> {
    let Some(map) = args.as_object() else {
        return Vec::new();
    };
    let style = Style::default().fg(Color::DarkGray);
    let mut out = Vec::new();
    for (key, value) in map {
        if used.contains(&key.as_str()) {
            continue;
        }
        let text = match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let mut lines = text.lines();
        let first = format!("{key}: {}", clean(lines.next().unwrap_or_default()));
        out.extend(row(
            vec![Span::raw(INDENT), Span::styled(first, style)],
            true,
            width,
            INDENT,
        ));
        for line in lines {
            out.extend(row(
                vec![
                    Span::raw(INDENT),
                    Span::styled(format!("  {}", clean(line)), style),
                ],
                true,
                width,
                INDENT,
            ));
        }
    }
    out
}

/// diff 里的一行做了什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Same,
    Gone,
    Added,
}

/// `edit` 的 diff：`match_text` 与 `replace_text` 逐行比对，没改的行只留首尾几行上下文。
///
/// 没有行号——客户端看不到文件，只看得到模型给的那两段。
fn diff_lines(old: &str, new: &str, detail: bool, width: usize) -> Vec<Line<'static>> {
    let old: Vec<String> = old.lines().map(clean).collect();
    let new: Vec<String> = new.lines().map(clean).collect();
    let ops = trim_context(line_diff(&old, &new));
    let mut out = Vec::new();
    let mut at = 0;
    while at < ops.len() {
        let Some((op, line)) = ops[at] else {
            out.push(Line::from(vec![
                Span::raw(INDENT),
                Span::styled("  …", Style::default().fg(Color::DarkGray)),
            ]));
            at += 1;
            continue;
        };
        if op == Op::Same {
            out.extend(diff_row(
                Op::Same,
                vec![plain(line, Op::Same)],
                detail,
                width,
            ));
            at += 1;
            continue;
        }
        // 一段连着的删与加：恰好各一行时标出行内改了哪一截。
        let gone = run(&ops[at..], Op::Gone);
        let added = run(&ops[at + gone.len()..], Op::Added);
        at += gone.len() + added.len();
        if let ([before], [after]) = (gone.as_slice(), added.as_slice()) {
            let (old_spans, new_spans) = intra_line(before, after);
            out.extend(diff_row(Op::Gone, old_spans, detail, width));
            out.extend(diff_row(Op::Added, new_spans, detail, width));
        } else {
            for line in gone {
                out.extend(diff_row(
                    Op::Gone,
                    vec![plain(line, Op::Gone)],
                    detail,
                    width,
                ));
            }
            for line in added {
                out.extend(diff_row(
                    Op::Added,
                    vec![plain(line, Op::Added)],
                    detail,
                    width,
                ));
            }
        }
    }
    out
}

/// 从头开始连着几行是 `op`。
fn run<'a>(ops: &[Option<(Op, &'a str)>], op: Op) -> Vec<&'a str> {
    ops.iter()
        .map_while(|item| item.filter(|(each, _)| *each == op).map(|(_, line)| line))
        .collect()
}

fn diff_row(op: Op, content: Vec<Span<'static>>, detail: bool, width: usize) -> Vec<Line<'static>> {
    let sign = match op {
        Op::Same => "  ",
        Op::Gone => "- ",
        Op::Added => "+ ",
    };
    let mut spans = vec![Span::raw(INDENT), Span::styled(sign, diff_style(op))];
    spans.extend(content);
    row(spans, detail, width, &format!("{INDENT}  "))
}

fn diff_style(op: Op) -> Style {
    match op {
        Op::Same => Style::default().fg(Color::DarkGray),
        Op::Gone => Style::default().fg(Color::Red),
        Op::Added => Style::default().fg(Color::Green),
    }
}

fn plain(line: &str, op: Op) -> Span<'static> {
    Span::styled(line.to_string(), diff_style(op))
}

/// 一行改成另一行：两头相同的部分照常，中间改掉的那一截反白。两行毫无共同的首尾就整行
/// 照常——整行反白比不标还难看。
fn intra_line(before: &str, after: &str) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let prefix: usize = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let suffix: usize = before[prefix..]
        .chars()
        .rev()
        .zip(after[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    if prefix + suffix == 0 {
        return (vec![plain(before, Op::Gone)], vec![plain(after, Op::Added)]);
    }
    let split = |line: &str, op: Op| {
        let style = diff_style(op);
        let middle = &line[prefix..line.len() - suffix];
        [
            Span::styled(line[..prefix].to_string(), style),
            Span::styled(middle.to_string(), style.add_modifier(Modifier::REVERSED)),
            Span::styled(line[line.len() - suffix..].to_string(), style),
        ]
        .into_iter()
        .filter(|span| !span.content.is_empty())
        .collect()
    };
    (split(before, Op::Gone), split(after, Op::Added))
}

/// 逐行比对：首尾相同的行先剥掉，中间按最长公共子序列；太大就整段先删后加。
fn line_diff<'a>(old: &'a [String], new: &'a [String]) -> Vec<(Op, &'a str)> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &old[prefix..old.len() - suffix];
    let b = &new[prefix..new.len() - suffix];
    let mut ops: Vec<(Op, &str)> = old[..prefix]
        .iter()
        .map(|l| (Op::Same, l.as_str()))
        .collect();
    if a.len().saturating_mul(b.len()) > DIFF_CELLS {
        ops.extend(a.iter().map(|l| (Op::Gone, l.as_str())));
        ops.extend(b.iter().map(|l| (Op::Added, l.as_str())));
    } else {
        // table[i][j]：a[i..] 与 b[j..] 的最长公共子序列长度。
        let mut table = vec![vec![0u32; b.len() + 1]; a.len() + 1];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                table[i][j] = if a[i] == b[j] {
                    table[i + 1][j + 1] + 1
                } else {
                    table[i + 1][j].max(table[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            if a[i] == b[j] {
                ops.push((Op::Same, a[i].as_str()));
                i += 1;
                j += 1;
            } else if table[i + 1][j] >= table[i][j + 1] {
                ops.push((Op::Gone, a[i].as_str()));
                i += 1;
            } else {
                ops.push((Op::Added, b[j].as_str()));
                j += 1;
            }
        }
        ops.extend(a[i..].iter().map(|l| (Op::Gone, l.as_str())));
        ops.extend(b[j..].iter().map(|l| (Op::Added, l.as_str())));
    }
    ops.extend(
        old[old.len() - suffix..]
            .iter()
            .map(|l| (Op::Same, l.as_str())),
    );
    ops
}

/// 没改的行只留挨着改动的 [`DIFF_CONTEXT`] 行，其余收成一个 `None`（画成 `…`）。
fn trim_context(ops: Vec<(Op, &str)>) -> Vec<Option<(Op, &str)>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < ops.len() {
        if ops[at].0 != Op::Same {
            out.push(Some(ops[at]));
            at += 1;
            continue;
        }
        let end = ops[at..]
            .iter()
            .position(|(op, _)| *op != Op::Same)
            .map_or(ops.len(), |len| at + len);
        let same = &ops[at..end];
        let keep_head = if at == 0 { 0 } else { DIFF_CONTEXT };
        let keep_tail = if end == ops.len() { 0 } else { DIFF_CONTEXT };
        if same.len() <= keep_head + keep_tail || (at == 0 && end == ops.len()) {
            // 太短不值得收；整段都没改（两段原文相同）就照实全给。
            out.extend(same.iter().copied().map(Some));
        } else {
            out.extend(same[..keep_head].iter().copied().map(Some));
            out.push(None);
            out.extend(same[same.len() - keep_tail..].iter().copied().map(Some));
        }
        at = end;
    }
    out
}

/// 一行：展开时按宽度折行（续行顶 `indent`），折叠时截在一屏宽。
fn row(spans: Vec<Span<'static>>, detail: bool, width: usize, indent: &str) -> Vec<Line<'static>> {
    if detail {
        wrap_spans(spans, width, indent)
    } else {
        vec![fit_spans(spans, width)]
    }
}

/// 一串带样式的片段按显示宽度折成几行，续行顶 `indent`。宽度放不下一个字时也会前进，
/// 不会死循环。
fn wrap_spans(spans: Vec<Span<'static>>, width: usize, indent: &str) -> Vec<Line<'static>> {
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0usize;
    let mut filled = false;
    for span in spans {
        let mut piece = String::new();
        for ch in span.content.chars() {
            let w = char_width(ch);
            if used + w > width && filled {
                if !piece.is_empty() {
                    rows.last_mut()
                        .expect("至少一行")
                        .push(Span::styled(std::mem::take(&mut piece), span.style));
                }
                rows.push(vec![Span::raw(indent.to_string())]);
                used = markdown::display_width(indent);
            }
            piece.push(ch);
            used += w;
            filled = true;
        }
        if !piece.is_empty() {
            rows.last_mut()
                .expect("至少一行")
                .push(Span::styled(piece, span.style));
        }
    }
    rows.into_iter().map(Line::from).collect()
}

/// 截到 `width` 列；截掉了就补一个 `…`（它自己占一列，样式随被截的那一段）。
fn fit_spans(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= width {
        return Line::from(spans);
    }
    let room = width.saturating_sub(1);
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let mut piece = String::new();
        for ch in span.content.chars() {
            let w = char_width(ch);
            if used + w > room {
                if !piece.is_empty() {
                    out.push(Span::styled(piece, span.style));
                }
                if width > 0 {
                    out.push(Span::styled("…", span.style));
                }
                return Line::from(out);
            }
            piece.push(ch);
            used += w;
        }
        out.push(Span::styled(piece, span.style));
    }
    Line::from(out)
}

fn char_width(ch: char) -> usize {
    markdown::display_width(ch.encode_utf8(&mut [0u8; 4])).max(1)
}

/// 进终端之前把控制字符拿掉。工具输出里的颜色转义原样写出去会把后面的行都染上色，制表
/// 符占几列由终端说了算、这里算不准，行内的回车（进度条）只留终端最后会显示的那一截。
fn clean(line: &str) -> String {
    let line = line
        .rsplit('\r')
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                // CSI：参数与中间字节之后，一个 0x40–0x7E 的终止字节。
                Some('[') => {
                    for ch in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&ch) {
                            break;
                        }
                    }
                }
                // OSC（终端标题、超链接）：到 BEL 或 ST（ESC \）为止。
                Some(']') => {
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' {
                            break;
                        }
                        if ch == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                // 两个字节的转义：跟着的那一个已经吃掉了。
                _ => {}
            },
            '\t' => out.push_str("    "),
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

/// 耗时：一秒以内按毫秒，一分钟以内按十分之一秒，再长按分秒。
fn elapsed(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1_000..=59_999 => format!("{:.1}s", ms as f64 / 1000.0),
        _ => format!("{}m{:02}s", ms / 60_000, ms / 1000 % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::{Seq, ToolCallId};
    use serde_json::json;

    fn call(tool: &str, args: Value, state: ToolCallState, preview: Option<&str>) -> ToolLine {
        ToolLine {
            call: ToolCallId::from_raw("call-1"),
            run: None,
            tool: tool.into(),
            state,
            attempts: 1,
            args,
            cwd: None,
            preview: preview.map(Into::into),
            elapsed_ms: 1200,
            seq: Seq(1),
            detached: false,
        }
    }

    /// 转到后台的那次 bash 不画 ✔：命令还在跑，成败要等交回来的消息。
    #[test]
    fn a_detached_command_is_not_drawn_as_a_success() {
        let mut tool = call(
            "bash",
            json!({"command": "cargo test --workspace"}),
            ToolCallState::Completed,
            Some("已转后台 #d63a：命令跑了 120 秒还没完"),
        );
        tool.detached = true;
        let lines = lines(&tool, false, 80, None);
        let text = text_of(&lines);
        assert!(
            text.starts_with("  ↗ bash $ cargo test --workspace"),
            "{text}"
        );
        assert!(!text.contains('✔'), "{text}");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Cyan));
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

    fn numbered(count: usize) -> String {
        (1..=count)
            .map(|n| format!("第 {n} 行"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 长命令折行给全，不截在一屏宽——截掉的那半截往往正是要看的（管道、重定向）。
    #[test]
    fn a_long_command_wraps_instead_of_being_cut() {
        let command = format!("cargo test --workspace {} | tail -20", "x".repeat(60));
        let tool = call(
            "bash",
            json!({ "command": command }),
            ToolCallState::Completed,
            None,
        );
        let lines = lines(&tool, false, 40, None);
        assert!(lines.len() > 1, "{}", text_of(&lines));
        assert!(lines.iter().all(|line| line.width() <= 40));
        let joined: String = text_of(&lines).replace(&format!("\n{INDENT}"), "");
        assert!(joined.contains(&format!("$ {command}")), "{joined}");
    }

    /// bash 的结果留尾巴：最后几行是结论，前面是过程。改名前的旧记录叫 `shell`，同样处理。
    #[test]
    fn bash_output_keeps_its_last_lines() {
        let output = numbered(15);
        for name in ["bash", "shell"] {
            let tool = call(
                name,
                json!({ "command": "make" }),
                ToolCallState::Completed,
                Some(&output),
            );
            let text = text_of(&lines(&tool, false, 80, None));
            assert!(text.contains("… 前面还有 12 行（Ctrl-T 展开）"), "{text}");
            assert!(
                text.contains("第 13 行") && text.contains("第 15 行"),
                "{text}"
            );
            assert!(!text.contains("第 12 行"), "{text}");
        }
    }

    /// 其他工具留头，超出的说一句还有几行、按什么展开。
    #[test]
    fn other_results_keep_their_first_lines() {
        let output = numbered(14);
        let tool = call(
            "rg",
            json!({ "pattern": "fn main", "path": "crates", "glob": "*.rs" }),
            ToolCallState::Completed,
            Some(&output),
        );
        let text = text_of(&lines(&tool, false, 80, None));
        assert!(text.contains("rg fn main · crates · *.rs"), "{text}");
        assert!(text.contains("  ⎿ 第 1 行"), "{text}");
        assert!(
            text.contains("第 3 行") && !text.contains("第 4 行"),
            "{text}"
        );
        assert!(text.contains("… 还有 11 行（Ctrl-T 展开）"), "{text}");
    }

    /// 读到的正文就是文件本身，成功时不印；失败了要印，那是原因。
    #[test]
    fn a_read_shows_its_range_and_only_a_failure() {
        let args = json!({ "path": "src/main.rs", "start_line": 10, "line_count": 50 });
        let ok = call(
            "read",
            args.clone(),
            ToolCallState::Completed,
            Some("fn main() {}"),
        );
        let text = text_of(&lines(&ok, false, 80, None));
        assert!(text.contains("read src/main.rs:10-59"), "{text}");
        assert!(!text.contains("fn main"), "{text}");

        let failed = call(
            "read",
            args,
            ToolCallState::Failed,
            Some("src/main.rs 不存在"),
        );
        assert!(text_of(&lines(&failed, false, 80, None)).contains("不存在"));
    }

    /// edit 的 diff 不折：十几行的改动折叠时也一行不少。
    #[test]
    fn an_edit_shows_its_whole_diff_even_when_folded() {
        let old = numbered(12);
        let new = (1..=12)
            .map(|n| format!("新的第 {n} 行"))
            .collect::<Vec<_>>()
            .join("\n");
        let tool = call(
            "edit",
            json!({ "path": "a.txt", "match_text": old, "replace_text": new }),
            ToolCallState::Completed,
            Some("a.txt 替换了 1 处"),
        );
        let text = text_of(&lines(&tool, false, 80, None));
        assert_eq!(text.matches("    - ").count(), 12, "{text}");
        assert_eq!(text.matches("    + ").count(), 12, "{text}");
        assert!(!text.contains("还有"), "{text}");
        assert!(!text.contains("替换了"), "成功的回执头一行已经说了：{text}");
    }

    /// 头一行只有记号带状态色，路径用终端本色——不和下面 diff 的绿色混成一片。
    #[test]
    fn only_the_marker_carries_the_state_colour() {
        let tool = call(
            "edit",
            json!({ "path": "a.go", "match_text": "a", "replace_text": "b" }),
            ToolCallState::Completed,
            None,
        );
        let head = &lines(&tool, false, 80, None)[0];
        assert_eq!(head.spans[0].style.fg, Some(Color::Green));
        for span in &head.spans[1..3] {
            assert_eq!(span.style.fg, None, "{:?} 不该着色", span.content);
        }
        assert!(head.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    /// 只改了一行里的一截：那一截反白，两头照常。
    #[test]
    fn a_one_line_change_highlights_the_changed_part() {
        let tool = call(
            "edit",
            json!({
                "path": "a.rs",
                "match_text": "let x = 1;\nkeep();",
                "replace_text": "let x = 42;\nkeep();"
            }),
            ToolCallState::Completed,
            None,
        );
        let lines = lines(&tool, false, 80, None);
        let reversed: Vec<String> = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(reversed, vec!["1", "42"]);
        assert!(text_of(&lines).contains("      keep();"));
    }

    /// 改动两边没改的长段只留挨着的几行上下文，中间收成 `…`。
    #[test]
    fn long_unchanged_context_is_trimmed() {
        let old = format!("{}\nold\n{}", numbered(10), numbered(10));
        let new = format!("{}\nnew\n{}", numbered(10), numbered(10));
        let tool = call(
            "edit",
            json!({ "path": "a.txt", "match_text": old, "replace_text": new }),
            ToolCallState::Completed,
            None,
        );
        let text = text_of(&lines(&tool, false, 80, None));
        assert!(text.contains("- old") && text.contains("+ new"), "{text}");
        assert_eq!(text.matches("…").count(), 2, "{text}");
        assert_eq!(text.matches("第 ").count(), DIFF_CONTEXT * 2, "{text}");
    }

    /// 插入几行：没改的行不出现在 `-` 那边。
    #[test]
    fn an_insertion_only_adds_lines() {
        let tool = call(
            "edit",
            json!({ "path": "a.txt", "match_text": "a\nc", "replace_text": "a\nb1\nb2\nc" }),
            ToolCallState::Completed,
            None,
        );
        let text = text_of(&lines(&tool, false, 80, None));
        assert!(!text.contains("- "), "{text}");
        assert!(text.contains("+ b1") && text.contains("+ b2"), "{text}");
    }

    /// 脚本折叠时只给前几行；`Ctrl-T` 打开全给，连同头一行没用到的参数。
    #[test]
    fn detail_gives_everything_folding_hides() {
        let code = numbered(13);
        let output = numbered(7);
        let tool = call(
            "bash",
            json!({ "command": format!("bash <<'EOF'\n{code}\nEOF"), "cwd": "/tmp/work" }),
            ToolCallState::Completed,
            Some(&output),
        );
        let folded = text_of(&lines(&tool, false, 80, None));
        assert!(
            folded.contains("… 还有 10 行"),
            "十三行脚本加一行 EOF：{folded}"
        );
        assert!(folded.contains("… 前面还有 4 行"), "{folded}");
        assert!(!folded.contains("cwd"), "{folded}");

        let detail = text_of(&lines(&tool, true, 80, None));
        assert!(!detail.contains("还有"), "{detail}");
        assert_eq!(detail.matches("第 13 行").count(), 1, "{detail}");
        assert_eq!(detail.matches("第 7 行").count(), 2, "{detail}");
        assert!(detail.contains("cwd: /tmp/work"), "{detail}");
    }

    /// 输出里的颜色转义、制表符、进度条回车不原样进终端。
    #[test]
    fn control_sequences_never_reach_the_terminal() {
        assert_eq!(clean("\x1b[1;31mred\x1b[0m done"), "red done");
        assert_eq!(clean("\x1b]0;title\x07after"), "after");
        assert_eq!(clean("a\tb"), "a    b");
        assert_eq!(clean(" 10%\r 50%\r100%"), "100%");
    }

    /// 耗时贴在头一行末尾，不再单占一行。
    #[test]
    fn elapsed_time_rides_on_the_head_line() {
        assert_eq!(elapsed(340), "340ms");
        assert_eq!(elapsed(4_200), "4.2s");
        assert_eq!(elapsed(125_000), "2m05s");
        let tool = call(
            "rg",
            json!({ "pattern": "x" }),
            ToolCallState::Completed,
            None,
        );
        let lines = lines(&tool, false, 80, None);
        assert_eq!(text_of(&lines), "  ✔ rg x  1.2s");
    }

    /// 在跑的那一格是圆点，按钟一明一暗；冻进回滚区的没有钟，不用圆点。
    #[test]
    fn a_running_call_pulses_a_dot_not_the_status_spinner() {
        let tool = call(
            "bash",
            json!({ "command": "sleep 5" }),
            ToolCallState::Started,
            None,
        );
        let t0 = time::macros::datetime!(2026-09-22 08:00:00 UTC);
        let half = t0 + time::Duration::milliseconds(spinner::PULSE_MS as i64);
        let mark = |now| lines(&tool, false, 80, Some(now))[0].spans[0].clone();
        assert_eq!(mark(t0).content, "  ●");
        assert_eq!(mark(half).content, "  ●");
        assert_ne!(
            mark(t0).style.add_modifier.contains(Modifier::DIM),
            mark(half).style.add_modifier.contains(Modifier::DIM),
            "一明一暗"
        );
        let frozen = text_of(&lines(&tool, false, 80, None));
        assert!(!frozen.contains('●'), "{frozen}");
    }

    /// 认不出主参数的工具：折叠时压成一行，展开时逐个列出来。
    #[test]
    fn an_unknown_tool_lists_its_arguments_in_detail() {
        let tool = call(
            "mcp__x__y",
            json!({ "alpha": 1, "beta": "two\nlines" }),
            ToolCallState::Completed,
            None,
        );
        let folded = text_of(&lines(&tool, false, 80, None));
        assert!(
            folded.contains(r#"{"alpha":1,"beta":"two\nlines"}"#),
            "{folded}"
        );
        let detail = text_of(&lines(&tool, true, 80, None));
        assert!(detail.contains("alpha: 1"), "{detail}");
        assert!(detail.contains("beta: two\n      lines"), "{detail}");
    }
}
