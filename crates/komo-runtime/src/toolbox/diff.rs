//! 上一版与候选之间那份"改动"（§7.2：界面要显示**改动**）。
//!
//! 这里的 diff 是给人在**聊天消息里**看的，不是给 `patch` 吃的：所以只做"掐掉两头相同
//! 的行，把中间整段按 `-` / `+` 列出来"。不引第三方 diff 库（§13.4 不加新依赖），也不
//! 追求最小编辑脚本——一个把整段替换写成整段替换的结果，比一个精巧到看不懂的结果更
//! 适合"批不批"这个判断。

/// 上下文行数：前后各留这么多行相同的行。
const CONTEXT: usize = 3;

/// 一份可读的行 diff。两边相同时答空串。
pub fn unified(before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();

    let mut head = 0usize;
    while head < old.len() && head < new.len() && old[head] == new[head] {
        head += 1;
    }
    let mut tail = 0usize;
    while tail < old.len() - head
        && tail < new.len() - head
        && old[old.len() - 1 - tail] == new[new.len() - 1 - tail]
    {
        tail += 1;
    }

    let context_start = head.saturating_sub(CONTEXT);
    let mut out = String::new();
    if context_start > 0 {
        out.push_str(&format!("@@ 前面 {context_start} 行相同 @@\n"));
    }
    for line in &old[context_start..head] {
        out.push_str(&format!("  {line}\n"));
    }
    for line in &old[head..old.len() - tail] {
        out.push_str(&format!("- {line}\n"));
    }
    for line in &new[head..new.len() - tail] {
        out.push_str(&format!("+ {line}\n"));
    }
    let context_end = (old.len() - tail + CONTEXT).min(old.len());
    for line in &old[old.len() - tail..context_end] {
        out.push_str(&format!("  {line}\n"));
    }
    if context_end < old.len() {
        out.push_str(&format!("@@ 后面 {} 行相同 @@\n", old.len() - context_end));
    }
    out
}

/// 一个**新**模块：没有上一版，改动就是"整份新增"。
pub fn added(after: &str) -> String {
    let mut out = String::from("（这个模块还没有已启用版本，以下是全部新增内容）\n");
    for line in after.lines() {
        out.push_str(&format!("+ {line}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unchanged_body_has_no_diff() {
        assert_eq!(unified("a\nb\n", "a\nb\n"), "");
    }

    #[test]
    fn a_changed_line_shows_up_on_both_sides() {
        let diff = unified("a\nb\nc\n", "a\nB\nc\n");
        assert!(diff.contains("- b"), "{diff}");
        assert!(diff.contains("+ B"), "{diff}");
        assert!(diff.contains("  a"), "上下文也在：{diff}");
    }

    #[test]
    fn a_long_common_prefix_is_summarised_rather_than_printed() {
        let before: String = (0..50).map(|i| format!("line {i}\n")).collect();
        let after = before.replace("line 40", "LINE 40");
        let diff = unified(&before, &after);
        assert!(diff.contains("@@ 前面 37 行相同 @@"), "{diff}");
        assert!(diff.contains("- line 40"), "{diff}");
        assert!(diff.contains("+ LINE 40"), "{diff}");
        assert!(!diff.contains("line 3\n"), "前面那 37 行不逐行印：{diff}");
    }

    #[test]
    fn a_brand_new_module_is_all_additions() {
        let diff = added("def f():\n    return 1\n");
        assert!(diff.contains("+ def f():"), "{diff}");
    }
}
