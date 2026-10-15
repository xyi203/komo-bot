//! Markdown → ratatui 的行（pulldown-cmark，§13.4）。
//!
//! 两种宽度策略，分开对待：
//!
//! - **散文换行**：标题、段落、列表、引用按可用宽度折行，按显示宽度算——中文一个字两
//!   列，按 `char` 数折会在 80 列的终端上溢出一半。
//! - **代码块与表格横向截断**：它们的对齐本身就是内容，折行等于毁掉它。超出宽度的部分
//!   截掉并补 `…`，**绝不撑破布局**。
//!
//! 块与块之间空一行（照 pi）：标题、段落、代码块、表格、引用、分隔线各自成块，挤在一起
//! 就分不出哪儿是重点。例外是紧跟在一段话下面、原文里也没隔空行的列表——「如下：」和
//! 它引出的几项是一体的。
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
    let mut renderer = Renderer::new(markdown, width);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        renderer.event(event, range.start);
    }
    renderer.finish()
}

/// 接在 `before` 后面的那一段：两边都有块，就先空一行——整段渲染时这两块之间也是这一行。
///
/// 草稿分段交出去时用它：每一段单独 [`render`] 都不带头尾空行，块间那一行只能由接缝补上。
pub fn render_after(before: &str, markdown: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = render(markdown, width);
    if !lines.is_empty() && !before.trim().is_empty() {
        lines.insert(0, Line::default());
    }
    lines
}

/// 一段还在生成的 Markdown 里**已经定型**的那一段前缀有多长（字节）。
///
/// 定型 = 停在一个代码块之外的空行之后。渲染器一块一块地出行、块间的空行只插在**后一块
/// 的前面**，所以这一段单独渲染出来的行，与整段渲染出来的开头那几行一样——可以先交给回
/// 滚区，后面再来的字改不动它；接缝上那一行空行由 [`render_after`] 补。段落里不能切：软
/// 换行会把上下两行接起来重新折。
pub fn settled_prefix(markdown: &str) -> usize {
    let mut settled = 0usize;
    let mut offset = 0usize;
    // 开着的代码围栏：围栏字符与长度，收尾要同一种字符、至少一样长。
    let mut fence: Option<(char, usize)> = None;
    for line in markdown.split_inclusive('\n') {
        offset += line.len();
        // 最后一行还没打完：它是什么还说不准。
        if !line.ends_with('\n') {
            break;
        }
        let body = line.trim_end_matches(['\n', '\r']);
        let indent = body.len() - body.trim_start_matches(' ').len();
        let marker = body.trim_start_matches(' ');
        let run = |ch: char| marker.chars().take_while(|c| *c == ch).count();
        if indent <= 3 {
            match fence {
                Some((ch, len))
                    if run(ch) >= len && marker.trim_end_matches(ch).trim().is_empty() =>
                {
                    fence = None;
                    continue;
                }
                None if run('`') >= 3 => {
                    fence = Some(('`', run('`')));
                    continue;
                }
                None if run('~') >= 3 => {
                    fence = Some(('~', run('~')));
                    continue;
                }
                _ => {}
            }
        }
        if fence.is_none() && body.trim().is_empty() {
            settled = offset;
        }
    }
    settled
}

#[derive(Default)]
struct Renderer<'a> {
    /// 原文：判一个列表与它上面那段话之间隔没隔空行。
    source: &'a str,
    width: usize,
    lines: Vec<Line<'static>>,
    /// 正在攒的一段散文。
    text: String,
    style: Style,
    /// 列表嵌套，外层在前。
    lists: Vec<List>,
    /// 刚开了一个列表项、还没有内容：松散列表的项里先来的是一个段落，记号归它。
    item_fresh: bool,
    quote_depth: usize,
    /// 能并排放块的容器：整篇一个，每层引用一个。块间空行按容器算。
    containers: Vec<Container>,
    /// 在代码块里时，逐行原样收集。
    code: Option<Vec<String>>,
    /// 表格：表头 + 各行。
    table: Option<Table>,
    heading: Option<HeadingLevel>,
}

struct List {
    /// 下一个序号，None = 无序。
    next: Option<u64>,
    /// 开过几项了。
    items: usize,
}

#[derive(Default)]
struct Container {
    /// 开在几层列表里——只有同一层的块才算并排。
    lists: usize,
    /// 已经放过块了：再来一块就先空一行。
    seen: bool,
    /// 上一块是一段话。
    after_paragraph: bool,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    in_head: bool,
    head_rows: usize,
}

impl<'a> Renderer<'a> {
    fn new(source: &'a str, width: usize) -> Self {
        Renderer {
            source,
            width,
            containers: vec![Container::default()],
            ..Default::default()
        }
    }

    /// `at`：这个事件在原文里的起始字节。
    fn event(&mut self, event: Event<'_>, at: usize) {
        match event {
            Event::Start(tag) => self.start(tag, at),
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
                self.open_block(at, false);
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(self.width.min(40)),
                    Style::default().fg(Color::DarkGray),
                )));
                self.close_block(false);
            }
            Event::TaskListMarker(done) => {
                // 记号的一部分：松散列表里它可能先于项里的段落到，不能让那一段另起一行。
                self.text.push_str(if done { "[x] " } else { "[ ] " });
            }
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(&html),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>, at: usize) {
        match tag {
            Tag::Heading { level, .. } => {
                self.open_block(at, false);
                self.heading = Some(level);
                // 一级标题再加一道下划线，与二级以下分出层次（照 pi）。
                let mut style = Style::default()
                    .fg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD);
                if level == HeadingLevel::H1 {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                self.style = style;
                // 一、二级靠样式区分，不写 `#`；三级起才留 `###` 前缀（照 pi）。
                let depth = heading_depth(level);
                if depth >= 3 {
                    self.text.push_str(&format!("{} ", "#".repeat(depth)));
                }
            }
            Tag::Paragraph if !self.lists.is_empty() => {
                // 松散列表：项里的段落。项的第一段接着记号写，不另起一行；项与项、同一项
                // 里的段与段之间空一行——原文里它们之间本来就隔着空行。
                if std::mem::take(&mut self.item_fresh) {
                    if self.lists.last().is_some_and(|list| list.items > 1) {
                        self.push_gap();
                    }
                } else {
                    self.flush_paragraph();
                    self.push_gap();
                }
            }
            Tag::Paragraph => self.open_block(at, false),
            Tag::BlockQuote(_) => {
                self.open_block(at, false);
                self.quote_depth += 1;
                self.containers.push(Container {
                    lists: self.lists.len(),
                    ..Default::default()
                });
            }
            Tag::HtmlBlock => self.open_block(at, false),
            Tag::CodeBlock(kind) => {
                self.open_block(at, false);
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
                self.open_block(at, true);
                self.item_fresh = false;
                self.lists.push(List {
                    next: start,
                    items: 0,
                });
            }
            Tag::Item => {
                self.flush_paragraph();
                self.item_fresh = true;
                let marker = match self.lists.last_mut() {
                    Some(list) => {
                        list.items += 1;
                        match &mut list.next {
                            Some(number) => {
                                let marker = format!("{number}. ");
                                *number += 1;
                                marker
                            }
                            None => "· ".to_string(),
                        }
                    }
                    None => "· ".to_string(),
                };
                let indent = "  ".repeat(self.lists.len().saturating_sub(1));
                self.text.push_str(&indent);
                self.text.push_str(&marker);
            }
            Tag::Table(_) => {
                self.open_block(at, false);
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
                self.close_block(false);
            }
            TagEnd::Paragraph => {
                self.flush_paragraph();
                self.close_block(true);
            }
            TagEnd::Item => {
                self.flush_paragraph();
                self.item_fresh = false;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_paragraph();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                if self.containers.len() > 1 {
                    self.containers.pop();
                }
                self.close_block(false);
            }
            TagEnd::HtmlBlock => {
                self.flush_paragraph();
                self.close_block(false);
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
                self.close_block(false);
            }
            TagEnd::List(_) => {
                self.flush_paragraph();
                self.lists.pop();
                self.close_block(false);
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.render_table(table);
                }
                self.close_block(false);
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

    /// 一块开头：同一个容器里前面已经有块了，就先空一行。列表项里的块不算——紧凑列表
    /// 一项一行，松散列表的空行另算。
    fn open_block(&mut self, at: usize, list: bool) {
        self.flush_paragraph();
        let depth = self.lists.len();
        let source = self.source;
        let Some(container) = self.containers.last_mut() else {
            return;
        };
        if container.lists != depth {
            return;
        }
        // 「如下：」紧跟着的列表与那段话是一体的；原文隔了空行才各算一块。
        let attached = list && container.after_paragraph && !follows_blank_line(source, at);
        let gap = container.seen && !attached;
        container.seen = true;
        container.after_paragraph = false;
        if gap {
            self.push_gap();
        }
    }

    fn close_block(&mut self, paragraph: bool) {
        let depth = self.lists.len();
        if let Some(container) = self.containers.last_mut()
            && container.lists == depth
        {
            container.after_paragraph = paragraph;
        }
    }

    /// 块间那一行空行；在引用里它也带着引用的竖线，否则一段引用看上去就断成了两段。
    fn push_gap(&mut self) {
        if self.quote_depth == 0 {
            self.lines.push(Line::default());
            return;
        }
        let bars = "│ ".repeat(self.quote_depth);
        self.lines.push(Line::from(Span::styled(
            bars.trim_end().to_string(),
            Style::default().fg(Color::Gray),
        )));
    }

    fn push_text(&mut self, text: &str) {
        self.item_fresh = false;
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

/// `at` 所在那一行的上一行是不是空行。引用里的 `>` 不算字：`> ` 那一行在引用里就是空行。
fn follows_blank_line(source: &str, at: usize) -> bool {
    let head = &source[..at.min(source.len())];
    let Some(line_start) = head.rfind('\n') else {
        return false;
    };
    let previous = head[..line_start].rsplit('\n').next().unwrap_or("");
    previous
        .trim_matches(|c: char| c == '>' || c.is_whitespace())
        .is_empty()
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

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn only_a_blank_line_outside_a_fence_settles_what_came_before() {
        assert_eq!(settled_prefix("第一段还没完"), 0);
        assert_eq!(settled_prefix("第一段\n第一段续行\n"), 0, "段落里不切");
        assert_eq!(settled_prefix("第一段\n\n第二段"), "第一段\n\n".len());
        // 围栏里的空行是代码的一部分。
        let open = "前言\n\n```rust\nfn a() {}\n\nfn b() {}\n";
        assert_eq!(settled_prefix(open), "前言\n\n".len());
        let closed = format!("{open}```\n\n尾巴");
        assert_eq!(settled_prefix(&closed), closed.len() - "尾巴".len());
        // 四个反引号开的围栏，三个关不上。
        let long = "````\na\n```\n\nb\n````\n\n";
        assert_eq!(settled_prefix(long), long.len());
        assert_eq!(settled_prefix("````\na\n```\n\nb\n"), 0);
    }

    /// 切下来的那一段单独渲染，与整段渲染的开头逐行相同——先交出去的行后来改不了。
    #[test]
    fn a_settled_prefix_renders_as_the_head_of_the_whole() {
        let whole = "# 标题\n\n一段很长的中文段落，需要在窄终端里折成好几行才放得下，这样才能检验折行。\n\n\
                     - 第一项\n- 第二项\n\n```sh\nls -la\n\necho hi\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n尾巴还在写";
        let cut = settled_prefix(whole);
        assert!(cut > 0 && cut < whole.len());
        let head = plain(&render(&whole[..cut], 30));
        let full = plain(&render(whole, 30));
        assert_eq!(full[..head.len()], head[..]);
        let mut joined = head.clone();
        joined.extend(plain(&render_after(&whole[..cut], &whole[cut..], 30)));
        assert_eq!(joined, full, "前缀 + 余下各自渲染，拼起来就是整段");
    }

    /// 像生成时那样一个字一个字地喂：每到一个定型点就交出去一段，最后交余下的。块间的
    /// 空行一行不多一行不少——接缝上的那一行由 `render_after` 补。
    #[test]
    fn streaming_cut_by_cut_renders_the_same_gaps_as_the_whole() {
        let whole = "# 标题\n\n第一段。\n\n如下：\n- 甲\n- 乙\n\n* 松散一\n\n* 松散二\n\n\
                     > 引用一\n>\n> 引用二\n\n```sh\nls\n\nls -la\n```\n\n---\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n## 结尾\n\n收尾";
        let mut done = 0usize;
        let mut streamed: Vec<String> = Vec::new();
        for (at, ch) in whole.char_indices() {
            let typed = &whole[..at + ch.len_utf8()];
            let settled = settled_prefix(typed);
            if settled > done {
                streamed.extend(plain(&render_after(
                    &whole[..done],
                    &whole[done..settled],
                    40,
                )));
                done = settled;
            }
        }
        streamed.extend(plain(&render_after(&whole[..done], &whole[done..], 40)));
        assert_eq!(streamed, plain(&render(whole, 40)));
    }

    /// 块与块之间空一行，开头结尾不留空行（照 pi）。
    #[test]
    fn blocks_are_separated_by_one_blank_line() {
        let lines = plain(&render(
            "# 标题\n正文紧贴标题\n\n## 二级\n\n```\nx\n```\n\n---\n\n收尾",
            40,
        ));
        assert_eq!(
            lines,
            vec![
                "标题",
                "",
                "正文紧贴标题",
                "",
                "二级",
                "",
                "```",
                "x",
                "```",
                "",
                "─".repeat(40).as_str(),
                "",
                "收尾",
            ]
        );
    }

    /// 「如下：」紧跟着的列表与那段话是一体的；原文隔了空行才分开。
    #[test]
    fn a_list_right_under_its_paragraph_stays_attached() {
        assert_eq!(
            plain(&render("如下：\n- 甲\n- 乙\n\n之后", 40)),
            vec!["如下：", "· 甲", "· 乙", "", "之后"]
        );
        assert_eq!(
            plain(&render("如下：\n\n- 甲", 40)),
            vec!["如下：", "", "· 甲"]
        );
    }

    /// 松散列表：记号与它那一段在同一行，项与项之间空一行。
    #[test]
    fn a_loose_list_keeps_its_markers_and_spaces_its_items() {
        assert_eq!(
            plain(&render("- 甲\n\n- 乙\n\n1. 一\n\n2. 二", 40)),
            vec!["· 甲", "", "· 乙", "", "1. 一", "", "2. 二"]
        );
        assert_eq!(
            plain(&render("- [x] 做完\n\n- [ ] 没做", 40)),
            vec!["· [x] 做完", "", "· [ ] 没做"]
        );
    }

    /// 引用里的块间空行带着竖线，看上去仍是一段引用。
    #[test]
    fn a_gap_inside_a_quote_keeps_the_bar() {
        assert_eq!(
            plain(&render("> 甲\n>\n> 乙", 40)),
            vec!["│ 甲", "│", "│ 乙"]
        );
    }

    #[test]
    fn only_a_first_level_heading_is_underlined() {
        let underlined = |markdown: &str| {
            render(markdown, 40)[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        };
        assert!(underlined("# 一级"));
        assert!(!underlined("## 二级"));
    }

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

    /// 一、二级标题不写 `#`，三级起才留前缀；都加粗（照 pi）。
    #[test]
    fn only_third_level_headings_and_below_keep_their_hashes() {
        assert_eq!(plain(&render("# 一级", 40)), vec!["一级"]);
        assert_eq!(plain(&render("## 二级", 40)), vec!["二级"]);
        assert_eq!(plain(&render("### 三级", 40)), vec!["### 三级"]);
        assert_eq!(plain(&render("###### 六级", 40)), vec!["###### 六级"]);
        let lines = render("## 二级标题", 40);
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
