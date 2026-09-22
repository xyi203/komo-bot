//! Markdown → ratatui 的行（pulldown-cmark，§13.4）。
//!
//! 两种宽度策略，分开对待：
//!
//! - **散文换行**：标题、段落、列表、引用按可用宽度折行，按显示宽度算——中文一个字两
//!   列，按 `char` 数折会在 80 列的终端上溢出一半。
//! - **代码块与表格横向截断**：它们的对齐本身就是内容，折行等于毁掉它。超出宽度的部分
//!   截掉并补 `…`，**绝不撑破布局**。
//!
//! 不做语法高亮：§13.4 把 `syntect` 列为「可选，按编译预算实测再定」，还没实测。

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// 一个字符串在终端上占几列。
pub fn display_width(text: &str) -> usize {
    Span::raw(text).width()
}

/// 截到 `width` 列；截掉了就补一个 `…`（它自己占一列）。
pub fn truncate_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = display_width(ch.encode_utf8(&mut [0u8; 4]));
        if used + w > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// 按显示宽度折行。宽度不足以放下一个字符时不会死循环。
pub fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    wrap_with_offsets(text, width)
        .into_iter()
        .map(|(_, line)| line)
        .collect()
}

/// 折行，并给出每一行**在原文里的起始字节偏移**。
///
/// 输入框把光标摆在哪一格靠它：画字的和摆光标的必须用同一套折行规则，否则中文一多，
/// 插入符就慢慢往左漂——这正是"输入位置对不上"的另一半。
pub fn wrap_with_offsets(text: &str, width: usize) -> Vec<(usize, String)> {
    if width == 0 {
        return vec![(0, String::new())];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;
    let mut used = 0usize;
    for (at, ch) in text.char_indices() {
        let w = display_width(ch.encode_utf8(&mut [0u8; 4])).max(1);
        if used + w > width && !current.is_empty() {
            lines.push((start, std::mem::take(&mut current)));
            start = at;
            used = 0;
        }
        current.push(ch);
        used += w;
    }
    lines.push((start, current));
    lines
}

/// 折行之后，`offset` 这个字节偏移落在第几行第几列。
pub fn caret_at(lines: &[(usize, String)], offset: usize) -> (usize, usize) {
    let row = lines
        .iter()
        .rposition(|(start, _)| *start <= offset)
        .unwrap_or(0);
    let (start, line) = &lines[row];
    let column = display_width(&line[..offset.saturating_sub(*start).min(line.len())]);
    (row, column)
}

/// 把一段 Markdown 渲染成不超过 `width` 列的行。
pub fn render(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let width = width.max(8) as usize;
    let mut renderer = Renderer::new(width);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    for event in Parser::new_ext(markdown, options) {
        renderer.event(event);
    }
    renderer.finish()
}

#[derive(Default)]
struct Renderer {
    width: usize,
    lines: Vec<Line<'static>>,
    /// 正在攒的一段散文。
    text: String,
    style: Style,
    /// 列表嵌套：每层一个「下一个序号」，None = 无序。
    lists: Vec<Option<u64>>,
    quote_depth: usize,
    /// 在代码块里时，逐行原样收集。
    code: Option<Vec<String>>,
    /// 表格：表头 + 各行。
    table: Option<Table>,
    heading: Option<HeadingLevel>,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    in_head: bool,
    head_rows: usize,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Renderer {
            width,
            ..Default::default()
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => {
                // 行内代码：反引号留着，读的人知道那是字面量。
                let previous = self.style;
                self.style = Style::default().fg(Color::Cyan);
                self.push_text(&format!("`{code}`"));
                self.style = previous;
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.flush_paragraph(),
            Event::Rule => {
                self.flush_paragraph();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(self.width.min(40)),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Event::TaskListMarker(done) => {
                self.push_text(if done { "[x] " } else { "[ ] " });
            }
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(&html),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush_paragraph();
                self.heading = Some(level);
                self.style = Style::default()
                    .fg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD);
                let hashes = "#".repeat(heading_depth(level));
                self.text.push_str(&format!("{hashes} "));
            }
            Tag::Paragraph => self.flush_paragraph(),
            Tag::BlockQuote(_) => {
                self.flush_paragraph();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_paragraph();
                let label = match &kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => format!("```{lang}"),
                    _ => "```".to_string(),
                };
                self.lines.push(Line::from(Span::styled(
                    truncate_to_width(&label, self.width),
                    Style::default().fg(Color::DarkGray),
                )));
                self.code = Some(Vec::new());
            }
            Tag::List(start) => {
                self.flush_paragraph();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush_paragraph();
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "· ".to_string(),
                };
                let indent = "  ".repeat(self.lists.len().saturating_sub(1));
                self.text.push_str(&indent);
                self.text.push_str(&marker);
            }
            Tag::Table(_) => {
                self.flush_paragraph();
                self.table = Some(Table::default());
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = true;
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table
                    && let Some(row) = table.rows.last_mut()
                {
                    row.push(String::new());
                }
            }
            Tag::Emphasis => self.style = self.style.add_modifier(Modifier::ITALIC),
            Tag::Strong => self.style = self.style.add_modifier(Modifier::BOLD),
            Tag::Strikethrough => self.style = self.style.add_modifier(Modifier::CROSSED_OUT),
            Tag::Link { .. } => self.style = self.style.fg(Color::Blue),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.flush_paragraph();
                self.heading = None;
                self.style = Style::default();
            }
            TagEnd::Paragraph | TagEnd::Item => self.flush_paragraph(),
            TagEnd::BlockQuote(_) => {
                self.flush_paragraph();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some(mut code) = self.code.take() {
                    // 围栏内容以换行结束，`split` 会在末尾留下一个空串——那不是一行代码。
                    if code.last().is_some_and(|line| line.is_empty()) {
                        code.pop();
                    }
                    for line in code {
                        // **横向截断**：代码块的对齐是内容，折行会毁掉它。
                        self.lines.push(Line::from(Span::styled(
                            truncate_to_width(line.trim_end_matches('\n'), self.width),
                            Style::default().fg(Color::White).bg(Color::Rgb(30, 30, 38)),
                        )));
                    }
                }
                self.lines.push(Line::from(Span::styled(
                    "```",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            TagEnd::List(_) => {
                self.flush_paragraph();
                self.lists.pop();
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.render_table(table);
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = false;
                    table.head_rows = table.rows.len();
                }
            }
            TagEnd::Emphasis => self.style = self.style.remove_modifier(Modifier::ITALIC),
            TagEnd::Strong => self.style = self.style.remove_modifier(Modifier::BOLD),
            TagEnd::Strikethrough => self.style = self.style.remove_modifier(Modifier::CROSSED_OUT),
            TagEnd::Link => self.style = Style::default(),
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        if let Some(code) = &mut self.code {
            for (index, part) in text.split('\n').enumerate() {
                if index == 0
                    && !code.is_empty()
                    && let Some(last) = code.last_mut()
                {
                    last.push_str(part);
                    continue;
                }
                code.push(part.to_string());
            }
            // `split` 在末尾换行时留下一个空串，那正是下一行的开头。
            return;
        }
        if let Some(table) = &mut self.table
            && let Some(row) = table.rows.last_mut()
            && let Some(cell) = row.last_mut()
        {
            cell.push_str(text);
            return;
        }
        self.text.push_str(text);
    }

    fn flush_paragraph(&mut self) {
        if self.text.trim().is_empty() {
            self.text.clear();
            return;
        }
        let prefix = "│ ".repeat(self.quote_depth);
        let available = self.width.saturating_sub(display_width(&prefix)).max(4);
        let text = std::mem::take(&mut self.text);
        let style = if self.quote_depth > 0 {
            self.style.fg(Color::Gray)
        } else {
            self.style
        };
        for piece in wrap_to_width(text.trim_end(), available) {
            self.lines
                .push(Line::from(Span::styled(format!("{prefix}{piece}"), style)));
        }
    }

    /// 表格：按列取最宽的一格，整体超宽时**按比例压缩并横向截断**。
    fn render_table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let mut widths = vec![0usize; columns];
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(display_width(cell.trim()));
            }
        }
        // 分隔符 " │ " 各占 3 列。
        let separators = 3 * columns.saturating_sub(1);
        let mut budget = self.width.saturating_sub(separators);
        let total: usize = widths.iter().sum();
        if total > budget && total > 0 {
            for width in &mut widths {
                *width = (*width * budget / total).max(3);
            }
            // 压缩后可能仍超一点（每列的下限是 3），再从最宽的列扣。
            while widths.iter().sum::<usize>() > budget && budget > 0 {
                if let Some(widest) = widths.iter_mut().max_by_key(|w| **w)
                    && *widest > 3
                {
                    *widest -= 1;
                } else {
                    break;
                }
            }
        }
        budget = widths.iter().sum::<usize>() + separators;
        let _ = budget;

        for (index, row) in table.rows.iter().enumerate() {
            let is_head = index < table.head_rows.max(1);
            let cells: Vec<String> = widths
                .iter()
                .enumerate()
                .map(|(column, width)| {
                    let cell = row.get(column).map(|c| c.trim()).unwrap_or("");
                    let cut = truncate_to_width(cell, *width);
                    let pad = width.saturating_sub(display_width(&cut));
                    format!("{cut}{}", " ".repeat(pad))
                })
                .collect();
            let style = if is_head {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            self.lines
                .push(Line::from(Span::styled(cells.join(" │ "), style)));
            if is_head && index + 1 == table.head_rows.max(1) {
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                self.lines.push(Line::from(Span::styled(
                    rule.join("─┼─"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_paragraph();
        self.lines
    }
}

fn heading_depth(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(lines: &[Line<'_>]) -> Vec<usize> {
        lines.iter().map(|line| line.width()).collect()
    }

    #[test]
    fn a_cjk_character_counts_as_two_columns() {
        assert_eq!(display_width("中"), 2);
        assert_eq!(display_width("a"), 1);
        assert_eq!(display_width("中文 ab"), 7);
    }

    #[test]
    fn nothing_rendered_at_eighty_columns_is_wider_than_eighty() {
        let markdown = "\
# 标题很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长

一段中文和 English 混排的长段落，它要在八十列的终端里折行而不是撑破布局，因为窄终端也要能用。

```rust
fn main() { println!(\"这一行故意写得非常非常非常非常非常非常非常非常非常非常长，好让它被横向截断\"); }
```

| 列一 | 列二 | 列三 |
|---|---|---|
| 一个很长的单元格内容 | 另一个也很长的单元格内容 | 第三个同样很长的单元格 |

> 引用里的一行中文，也要折行。

1. 第一项
2. 第二项
   - 嵌套一项
";
        for line in widths(&render(markdown, 80)) {
            assert!(line <= 80, "有一行宽 {line} 列");
        }
    }

    #[test]
    fn a_code_block_is_truncated_not_wrapped() {
        let markdown = "```\nabcdefghijklmnopqrstuvwxyz\n```";
        let lines = render(markdown, 12);
        // ```、代码、``` 三行——代码没有被折成两行。
        assert_eq!(lines.len(), 3, "{lines:?}");
        let code = lines[1].spans[0].content.as_ref();
        assert!(code.ends_with('…'), "{code}");
        assert!(display_width(code) <= 12);
    }

    #[test]
    fn a_paragraph_is_wrapped_not_truncated() {
        let lines = render("abcdefghijklmnopqrstuvwxyz", 10);
        assert!(lines.len() > 1, "{lines:?}");
        let joined: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert_eq!(joined, "abcdefghijklmnopqrstuvwxyz", "折行不丢字符");
    }

    #[test]
    fn a_heading_keeps_its_hashes_and_is_bold() {
        let lines = render("## 二级标题", 40);
        let text = lines[0].spans[0].content.as_ref();
        assert!(text.starts_with("## "), "{text}");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn a_blockquote_is_prefixed() {
        let lines = render("> 一句引用", 40);
        assert!(lines[0].spans[0].content.starts_with("│ "), "{lines:?}");
    }

    #[test]
    fn an_ordered_list_numbers_itself() {
        let lines = render("1. 甲\n2. 乙", 40);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans[0].content.to_string())
            .collect();
        assert_eq!(text, vec!["1. 甲", "2. 乙"]);
    }

    #[test]
    fn a_table_gets_a_rule_under_its_head() {
        let lines = render("| a | b |\n|---|---|\n| 1 | 2 |", 40);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans[0].content.to_string())
            .collect();
        assert!(text[0].contains("a") && text[0].contains("b"), "{text:?}");
        assert!(text[1].contains('┼'), "{text:?}");
        assert!(text[2].contains('1') && text[2].contains('2'), "{text:?}");
    }

    #[test]
    fn truncation_never_splits_a_character_in_half() {
        let cut = truncate_to_width("中文中文", 5);
        // 5 列放得下两个汉字加省略号。
        assert_eq!(cut, "中文…");
        assert!(display_width(&cut) <= 5);
    }

    #[test]
    fn a_width_of_zero_does_not_loop_forever() {
        assert_eq!(truncate_to_width("abc", 0), "");
        assert_eq!(wrap_to_width("abc", 0), vec![String::new()]);
    }
}
