//! 粘贴处理：括号粘贴与快速按键合并（§13.4）。
//!
//! 两个终端行为，一件事：**一大段文字一次进来**。
//!
//! - 支持括号粘贴的终端给一个 `Event::Paste(String)`。
//! - 不支持的终端把它拆成一串按键事件，中间几乎没有间隔——[`coalesce_rapid_keys`]
//!   按时间窗把这样一串合回一次粘贴。人打字的键间隔是几十到几百毫秒，粘贴是零点几
//!   毫秒，两者不在同一个数量级。
//!
//! 折叠成 chip 之后，**`input` 仍然持有全文**：chip 只记它在文本里的字节范围，渲染时
//! 跳过这一段改印标签。折起来的是显示，不是内容——发出去的永远是全文。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 超过这么多行就折成 chip。
pub const PASTE_MIN_LINES: usize = 4;
/// 或者超过这么多字节。
pub const PASTE_MIN_BYTES: usize = 10 * 1024;
/// 快速按键合并的时间窗：相邻两键间隔不超过它才算同一次粘贴。
pub const COALESCE_WINDOW_MS: u64 = 10;
/// 少于这么多键的一串不当成粘贴——两三个快键是真的有人在敲。
pub const COALESCE_MIN_RUN: usize = 8;

/// 输入框里被折起来的一段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteChip {
    /// 在 `Input::text()` 里的字节范围。
    pub start: usize,
    pub end: usize,
    pub lines: usize,
    pub bytes: usize,
}

impl PasteChip {
    /// 渲染时代替这一段出现的标签。
    pub fn label(&self) -> String {
        format!("[粘贴 {} 行 · {}]", self.lines, human_bytes(self.bytes))
    }

    pub fn contains(&self, offset: usize) -> bool {
        offset > self.start && offset < self.end
    }
}

fn human_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// 这一段文字该不该折起来。
pub fn should_fold(text: &str) -> bool {
    text.len() > PASTE_MIN_BYTES || text.lines().count() >= PASTE_MIN_LINES
}

/// 输入框的文本、光标与折叠区。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Input {
    text: String,
    /// 字节偏移，始终落在字符边界上。
    cursor: usize,
    chips: Vec<PasteChip>,
}

impl Input {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn chips(&self) -> &[PasteChip] {
        &self.chips
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.chips.clear();
    }

    pub fn clear(&mut self) {
        self.set("");
    }

    /// 取走全文，输入框清空。**chip 不影响取走的内容**。
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.chips.clear();
        text
    }

    pub fn insert_char(&mut self, ch: char) {
        let mut buffer = [0u8; 4];
        self.insert_str(ch.encode_utf8(&mut buffer));
    }

    pub fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.dissolve_at(self.cursor);
        self.text.insert_str(self.cursor, text);
        self.shift(self.cursor, text.len() as isize);
        self.cursor += text.len();
    }

    /// 粘贴一段。够大就折成 chip，**全文照样进 `text`**。
    pub fn paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if !should_fold(text) {
            self.insert_str(text);
            return;
        }
        self.dissolve_at(self.cursor);
        let start = self.cursor;
        self.text.insert_str(start, text);
        self.shift(start, text.len() as isize);
        let chip = PasteChip {
            start,
            end: start + text.len(),
            lines: text.lines().count().max(1),
            bytes: text.len(),
        };
        self.cursor = chip.end;
        self.chips.push(chip);
        self.chips.sort_by_key(|c| c.start);
    }

    /// 退格。光标正好在一个 chip 的末尾时，**整块删掉**——一个字符一个字符地啃掉一万
    /// 字节，是折叠没做完的那一半。
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some(index) = self.chips.iter().position(|c| c.end == self.cursor) {
            let chip = self.chips.remove(index);
            self.text.replace_range(chip.start..chip.end, "");
            let removed = (chip.end - chip.start) as isize;
            self.shift(chip.start, -removed);
            self.cursor = chip.start;
            return;
        }
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.dissolve_at(self.cursor);
        let removed = (self.cursor - previous) as isize;
        self.text.replace_range(previous..self.cursor, "");
        self.shift(previous, -removed);
        self.cursor = previous;
    }

    pub fn move_left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(ch) = self.text[self.cursor..].chars().next() {
            self.cursor += ch.len_utf8();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map(|index| self.cursor + index)
            .unwrap_or(self.text.len());
    }

    /// 整段文本的开头 / 结尾（`Ctrl-Home` / `Ctrl-End`）。
    pub fn move_start_of_text(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end_of_text(&mut self) {
        self.cursor = self.text.len();
    }

    /// 往左跳一个词：先吃掉左边的空白，再吃掉一整段同类字符。
    ///
    /// **chip 整块跳过**——一段折起来的粘贴在屏幕上是一个标签，按词走进它的内部只会让
    /// 光标停在一个看不见的地方。
    pub fn move_word_left(&mut self) {
        // 光标正贴着一段折起来的粘贴（末尾或内部）：整块跳过去。
        if let Some(chip) = self
            .chips
            .iter()
            .find(|chip| chip.start < self.cursor && self.cursor <= chip.end)
        {
            self.cursor = chip.start;
            return;
        }
        // 左边最近的那个 chip 的右沿是走不过去的地脚：越过它就走进了屏幕上根本没画出来
        // 的字里。
        let floor = self
            .chips
            .iter()
            .filter(|chip| chip.end <= self.cursor)
            .map(|chip| chip.end)
            .max()
            .unwrap_or(0);

        let mut index = self.cursor;
        while index > floor {
            let Some((at, ch)) = self.text[..index].char_indices().next_back() else {
                break;
            };
            if !ch.is_whitespace() {
                break;
            }
            index = at;
        }
        let Some(class) = self.text[..index].chars().next_back().map(char_class) else {
            self.cursor = index;
            return;
        };
        while index > floor {
            let Some((at, ch)) = self.text[..index].char_indices().next_back() else {
                break;
            };
            if char_class(ch) != class {
                break;
            }
            index = at;
        }
        self.cursor = index.max(floor);
    }

    /// 往右跳一个词：先吃掉一整段同类字符，再吃掉右边的空白。
    pub fn move_word_right(&mut self) {
        if let Some(chip) = self
            .chips
            .iter()
            .find(|chip| chip.start <= self.cursor && self.cursor < chip.end)
        {
            self.cursor = chip.end;
            return;
        }
        let ceiling = self
            .chips
            .iter()
            .filter(|chip| chip.start >= self.cursor)
            .map(|chip| chip.start)
            .min()
            .unwrap_or(self.text.len());

        let mut index = self.cursor;
        while index < ceiling {
            let Some(ch) = self.text[index..].chars().next() else {
                break;
            };
            if !ch.is_whitespace() {
                break;
            }
            index += ch.len_utf8();
        }
        let Some(class) = self.text[index..].chars().next().map(char_class) else {
            self.cursor = index;
            return;
        };
        while index < ceiling {
            let Some(ch) = self.text[index..].chars().next() else {
                break;
            };
            if char_class(ch) != class {
                break;
            }
            index += ch.len_utf8();
        }
        self.cursor = index.min(ceiling);
    }

    /// 往前删一个词（`Ctrl-W` / `Alt-Backspace`）。
    pub fn delete_word_before(&mut self) {
        let end = self.cursor;
        self.move_word_left();
        let start = self.cursor;
        if start < end {
            self.cut(start, end);
        }
    }

    /// 删到行尾（`Ctrl-K`）。已经在行尾时删掉那个换行。
    pub fn delete_to_line_end(&mut self) {
        let start = self.cursor;
        let end = match self.text[start..].find('\n') {
            Some(0) => start + 1,
            Some(index) => start + index,
            None => self.text.len(),
        };
        if start < end {
            self.cut(start, end);
        }
    }

    /// 删到行首（`Ctrl-U`）。
    pub fn delete_to_line_start(&mut self) {
        let end = self.cursor;
        self.move_home();
        let start = self.cursor;
        if start < end {
            self.cut(start, end);
        }
    }

    /// 往后删一个字符（`Delete`）。
    pub fn delete_forward(&mut self) {
        if let Some(index) = self.chips.iter().position(|c| c.start == self.cursor) {
            let chip = self.chips.remove(index);
            self.cut_raw(chip.start, chip.end);
            return;
        }
        if let Some(ch) = self.text[self.cursor..].chars().next() {
            let end = self.cursor + ch.len_utf8();
            self.cut(self.cursor, end);
        }
    }

    /// 输入里有几行。
    pub fn line_count(&self) -> usize {
        self.text.bytes().filter(|b| *b == b'\n').count() + 1
    }

    /// 光标在第几行（从 0 数）。
    pub fn line_index(&self) -> usize {
        self.text[..self.cursor]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
    }

    /// 上下移动一个**逻辑行**，列按显示宽度尽量对齐。移动不了（已经在首行 / 末行）返回
    /// `false`——调用方据此决定要不要改去翻历史。
    pub fn move_line_up(&mut self) -> bool {
        if self.line_index() == 0 {
            return false;
        }
        let column = self.column();
        let line_start = self.line_start(self.cursor);
        let previous_start = self.line_start(line_start.saturating_sub(1));
        self.cursor = self.offset_at_column(previous_start, column);
        true
    }

    pub fn move_line_down(&mut self) -> bool {
        let line_end = self.line_end(self.cursor);
        if line_end >= self.text.len() {
            return false;
        }
        let column = self.column();
        self.cursor = self.offset_at_column(line_end + 1, column);
        true
    }

    /// 光标离行首有几列（显示宽度）。
    fn column(&self) -> usize {
        let start = self.line_start(self.cursor);
        crate::tui::markdown::display_width(&self.text[start..self.cursor])
    }

    fn line_start(&self, at: usize) -> usize {
        self.text[..at]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0)
    }

    fn line_end(&self, at: usize) -> usize {
        self.text[at..]
            .find('\n')
            .map(|index| at + index)
            .unwrap_or(self.text.len())
    }

    /// 从 `line_start` 起走到第 `column` 列（或行尾）所在的字节偏移。
    fn offset_at_column(&self, line_start: usize, column: usize) -> usize {
        let end = self.line_end(line_start);
        let mut used = 0usize;
        for (at, ch) in self.text[line_start..end].char_indices() {
            if used >= column {
                return line_start + at;
            }
            used += crate::tui::markdown::display_width(ch.encode_utf8(&mut [0u8; 4])).max(1);
        }
        end
    }

    /// 删掉 `[start, end)`，chip 与光标跟着挪。删之前先打散被切开的那个 chip。
    fn cut(&mut self, start: usize, end: usize) {
        self.dissolve_at(start);
        self.dissolve_at(end);
        self.cut_raw(start, end);
    }

    fn cut_raw(&mut self, start: usize, end: usize) {
        self.text.replace_range(start..end, "");
        self.shift(start, -((end - start) as isize));
        self.cursor = start;
    }

    /// 渲染用的文本：每个 chip 的范围换成它的标签。**这是唯一跳过折叠内容的地方。**
    pub fn display(&self) -> String {
        let mut out = String::new();
        let mut at = 0usize;
        for chip in &self.chips {
            if chip.start > at {
                out.push_str(&self.text[at..chip.start]);
            }
            out.push_str(&chip.label());
            at = chip.end;
        }
        out.push_str(&self.text[at..]);
        out
    }

    /// 光标在 [`Input::display`] 的那份文本里的字节偏移。
    pub fn display_cursor(&self) -> usize {
        let mut offset = 0isize;
        for chip in &self.chips {
            if chip.end <= self.cursor {
                offset += chip.label().len() as isize - (chip.end - chip.start) as isize;
            } else if chip.start < self.cursor {
                // 光标落在 chip 内部（只会出现在 chip 刚被打散前的一瞬）。
                return (chip.start as isize + offset) as usize;
            }
        }
        (self.cursor as isize + offset).max(0) as usize
    }

    /// 在 `at` 处编辑会打散包住它的那个 chip——内容还在，只是不再折起来。
    fn dissolve_at(&mut self, at: usize) {
        self.chips.retain(|chip| !chip.contains(at));
    }

    fn shift(&mut self, from: usize, delta: isize) {
        for chip in &mut self.chips {
            if chip.start >= from {
                chip.start = (chip.start as isize + delta).max(0) as usize;
                chip.end = (chip.end as isize + delta).max(0) as usize;
            }
        }
    }
}

/// 按词移动时的字符分类：空白 / 词内 / 标点。**只分三类**——中文没有空格分词，再细分
/// 也只是猜。
fn char_class(ch: char) -> u8 {
    if ch.is_whitespace() {
        0
    } else if ch.is_alphanumeric() || ch == '_' {
        1
    } else {
        2
    }
}

/// 一个带到达时刻的按键。`at_ms` 是进程内的相对毫秒，谁产生的都行——测试直接给数字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedKey {
    pub key: KeyEvent,
    pub at_ms: u64,
}

impl TimedKey {
    pub fn new(key: KeyEvent, at_ms: u64) -> Self {
        TimedKey { key, at_ms }
    }
}

/// 合并后的输入事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Key(KeyEvent),
    Paste(String),
}

/// 把一串按键按时间窗合并成粘贴（没有括号粘贴的终端）。
///
/// 只有**能产生文本**的键参与合并：字符、回车（换行）、Tab。功能键打断一串——粘贴里
/// 不会出现 F5。
pub fn coalesce_rapid_keys(keys: &[TimedKey]) -> Vec<InputEvent> {
    coalesce_with(keys, COALESCE_WINDOW_MS, COALESCE_MIN_RUN)
}

pub fn coalesce_with(keys: &[TimedKey], window_ms: u64, min_run: usize) -> Vec<InputEvent> {
    let mut out = Vec::new();
    let mut run: Vec<&TimedKey> = Vec::new();

    let flush = |run: &mut Vec<&TimedKey>, out: &mut Vec<InputEvent>| {
        if run.len() >= min_run {
            let text: String = run.iter().filter_map(|k| text_of(k.key)).collect();
            out.push(InputEvent::Paste(text));
        } else {
            out.extend(run.iter().map(|k| InputEvent::Key(k.key)));
        }
        run.clear();
    };

    for key in keys {
        let is_text = text_of(key.key).is_some();
        let continues = run
            .last()
            .is_some_and(|last| key.at_ms.saturating_sub(last.at_ms) <= window_ms);
        if !is_text {
            flush(&mut run, &mut out);
            out.push(InputEvent::Key(key.key));
            continue;
        }
        if !continues {
            flush(&mut run, &mut out);
        }
        run.push(key);
    }
    flush(&mut run, &mut out);
    out
}

/// 这个键在粘贴里代表哪个字符。
fn text_of(key: KeyEvent) -> Option<char> {
    // 带 Ctrl / Alt 的组合是命令，不是文本。
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(ch) => Some(ch),
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab => Some('\t'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    #[test]
    fn a_short_paste_is_not_folded() {
        let mut input = Input::new();
        input.paste("一行\n两行");
        assert!(input.chips().is_empty());
        assert_eq!(input.display(), "一行\n两行");
    }

    #[test]
    fn four_lines_fold_into_a_chip_that_still_holds_the_whole_text() {
        let mut input = Input::new();
        let pasted = "a\nb\nc\nd";
        input.paste(pasted);
        assert_eq!(input.chips().len(), 1);
        assert_eq!(input.chips()[0].lines, 4);
        // 折起来的是显示。
        assert_eq!(input.display(), "[粘贴 4 行 · 7 B]");
        // 内容一个字节都没少。
        assert_eq!(input.text(), pasted);
        assert_eq!(input.take(), pasted);
    }

    #[test]
    fn a_ten_kilobyte_single_line_folds_too() {
        let mut input = Input::new();
        let pasted = "x".repeat(PASTE_MIN_BYTES + 1);
        input.paste(&pasted);
        assert_eq!(input.chips().len(), 1, "只有一行，但超过 10 KB");
        assert!(input.display().starts_with("[粘贴 1 行 · 10.0 KB"));
        assert_eq!(input.text().len(), PASTE_MIN_BYTES + 1);
    }

    #[test]
    fn exactly_ten_kilobytes_on_one_line_is_not_folded() {
        let mut input = Input::new();
        input.paste(&"x".repeat(PASTE_MIN_BYTES));
        assert!(input.chips().is_empty(), "阈值是「超过」，不是「达到」");
    }

    #[test]
    fn text_typed_around_a_chip_keeps_it_folded() {
        let mut input = Input::new();
        input.insert_str("看这个：");
        input.paste("a\nb\nc\nd");
        input.insert_str("，对吧");
        assert_eq!(input.chips().len(), 1);
        assert_eq!(input.display(), "看这个：[粘贴 4 行 · 7 B]，对吧");
        assert_eq!(input.text(), "看这个：a\nb\nc\nd，对吧");
    }

    #[test]
    fn backspace_at_the_end_of_a_chip_removes_the_whole_block() {
        let mut input = Input::new();
        input.insert_str("前");
        input.paste("a\nb\nc\nd");
        input.backspace();
        assert!(input.chips().is_empty());
        assert_eq!(input.text(), "前");
    }

    #[test]
    fn editing_inside_a_chip_dissolves_it_without_losing_text() {
        let mut input = Input::new();
        input.paste("a\nb\nc\nd");
        input.move_left();
        input.insert_char('!');
        assert!(input.chips().is_empty());
        assert_eq!(input.text(), "a\nb\nc\n!d");
    }

    #[test]
    fn the_display_cursor_sits_after_the_label_not_after_the_text() {
        let mut input = Input::new();
        input.paste("a\nb\nc\nd");
        assert_eq!(input.cursor(), 7);
        assert_eq!(input.display_cursor(), input.display().len());
    }

    #[test]
    fn a_burst_of_keys_becomes_one_paste() {
        let keys: Vec<_> = "hello world"
            .chars()
            .enumerate()
            .map(|(index, ch)| TimedKey::new(key(ch), index as u64))
            .collect();
        let out = coalesce_rapid_keys(&keys);
        assert_eq!(out, vec![InputEvent::Paste("hello world".into())]);
    }

    #[test]
    fn keys_a_human_could_have_typed_stay_keys() {
        let keys: Vec<_> = "hello world"
            .chars()
            .enumerate()
            .map(|(index, ch)| TimedKey::new(key(ch), index as u64 * 80))
            .collect();
        let out = coalesce_rapid_keys(&keys);
        assert_eq!(out.len(), keys.len());
        assert!(out.iter().all(|e| matches!(e, InputEvent::Key(_))));
    }

    #[test]
    fn a_burst_shorter_than_the_minimum_run_is_not_a_paste() {
        let keys: Vec<_> = "abc"
            .chars()
            .enumerate()
            .map(|(index, ch)| TimedKey::new(key(ch), index as u64))
            .collect();
        assert_eq!(coalesce_rapid_keys(&keys).len(), 3);
    }

    #[test]
    fn enter_inside_a_burst_is_a_newline_not_a_send() {
        let mut keys = Vec::new();
        for (index, ch) in "aaaa".chars().enumerate() {
            keys.push(TimedKey::new(key(ch), index as u64));
        }
        keys.push(TimedKey::new(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            4,
        ));
        for (index, ch) in "bbbb".chars().enumerate() {
            keys.push(TimedKey::new(key(ch), 5 + index as u64));
        }
        assert_eq!(
            coalesce_rapid_keys(&keys),
            vec![InputEvent::Paste("aaaa\nbbbb".into())]
        );
    }

    #[test]
    fn a_function_key_breaks_a_burst() {
        let mut keys: Vec<_> = "aaaaaaaa"
            .chars()
            .enumerate()
            .map(|(index, ch)| TimedKey::new(key(ch), index as u64))
            .collect();
        keys.push(TimedKey::new(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            8,
        ));
        let out = coalesce_rapid_keys(&keys);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], InputEvent::Paste("aaaaaaaa".into()));
        assert!(matches!(out[1], InputEvent::Key(_)));
    }
}

#[cfg(test)]
mod editor_tests {
    use super::*;

    fn at(text: &str, cursor: usize) -> Input {
        let mut input = Input::new();
        input.set(text);
        input.cursor = cursor;
        input
    }

    #[test]
    fn word_motions_step_over_one_word_at_a_time() {
        let text = "cargo test --workspace";
        let mut input = at(text, text.len());
        input.move_word_left();
        assert_eq!(&text[input.cursor()..], "workspace");
        input.move_word_left();
        assert_eq!(&text[input.cursor()..], "--workspace", "标点自成一段");
        input.move_word_left();
        assert_eq!(&text[input.cursor()..], "test --workspace");

        input.move_word_right();
        assert_eq!(&text[input.cursor()..], " --workspace");
        input.move_word_right();
        assert_eq!(&text[input.cursor()..], "workspace");
    }

    /// 中文没有空格，按词走靠的是"同一类字符连成一段"——标点是另一类。
    #[test]
    fn a_chinese_clause_is_one_word() {
        let text = "把构建目录清掉，然后跑测试";
        let mut input = at(text, text.len());
        input.move_word_left();
        assert_eq!(&text[input.cursor()..], "然后跑测试");
        input.move_word_left();
        assert_eq!(&text[input.cursor()..], "，然后跑测试");
    }

    #[test]
    fn ctrl_w_deletes_the_word_before_the_caret() {
        let mut input = at("cargo test --workspace", 22);
        input.delete_word_before();
        assert_eq!(input.text(), "cargo test --");
        input.delete_word_before();
        assert_eq!(input.text(), "cargo test ");
    }

    #[test]
    fn ctrl_k_and_ctrl_u_cut_to_the_ends_of_the_line() {
        let mut input = at("第一行\n第二行", "第一行\n".len() + "第二".len());
        input.delete_to_line_end();
        assert_eq!(input.text(), "第一行\n第二");
        input.delete_to_line_start();
        assert_eq!(input.text(), "第一行\n");
        // 已经在行尾了，再按一下把那个换行也吃掉。
        input.delete_to_line_start();
        assert_eq!(input.text(), "第一行\n");
    }

    /// **多行草稿里 ↑ 先是走行**：一条三行的草稿按一下就跳走翻历史，等于这三行白写了。
    #[test]
    fn the_caret_walks_lines_before_it_gives_up_to_history() {
        let mut input = at("第一行\n第二行\n第三行", 0);
        assert!(!input.move_line_up(), "已经在首行，交给历史");
        assert!(input.move_line_down());
        assert_eq!(input.line_index(), 1);
        assert!(input.move_line_down());
        assert_eq!(input.line_index(), 2);
        assert!(!input.move_line_down(), "已经在末行，交给历史");
        assert!(input.move_line_up());
        assert_eq!(input.line_index(), 1);
    }

    /// 上下移动时列按**显示宽度**对齐：中文一个字两列，按字符数对齐会歪。
    #[test]
    fn moving_between_lines_keeps_the_column_by_display_width() {
        let mut input = at("中文中文\nabcdefgh", 0);
        input.move_end();
        assert_eq!(input.cursor(), "中文中文".len());
        assert!(input.move_line_down());
        assert_eq!(&input.text()[input.cursor()..], "", "八列 = 八个半角字符");
    }

    /// 折起来的粘贴在屏幕上是一个标签：按词走、往后删，都该整块处理，不许把光标丢进
    /// 一段看不见的文本里。
    #[test]
    fn a_folded_paste_is_crossed_and_deleted_as_one_block() {
        let mut input = Input::new();
        input.insert_str("前");
        input.paste("一\n二\n三\n四");
        input.insert_str("后");
        assert_eq!(input.chips().len(), 1);

        input.move_word_left();
        assert_eq!(input.cursor(), input.chips()[0].end, "先停在 chip 的右沿");
        input.move_word_left();
        assert_eq!(input.cursor(), input.chips()[0].start, "整块跳过去");

        input.delete_forward();
        assert_eq!(input.text(), "前后", "往后删一下删掉整块");
        assert!(input.chips().is_empty());
    }

    #[test]
    fn delete_removes_the_character_after_the_caret() {
        let mut input = at("清理一下", 0);
        input.delete_forward();
        assert_eq!(input.text(), "理一下");
        input.move_end_of_text();
        input.delete_forward();
        assert_eq!(input.text(), "理一下", "末尾再按没有东西可删");
    }
}
