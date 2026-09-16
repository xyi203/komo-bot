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
