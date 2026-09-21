//! 一次工具结果在**模型上下文**里的样子（§8.3）。
//!
//! 事实在账本与输出存储里——状态、引用、大小、工具自己写的那段正文——这里是它的**投影**。
//! 投影是纯函数，而且**同一个事实只该有一处渲染**：刚跑完的那一次与重启之后回放，必须得到
//! 逐字节相同的字符串。所以输入里没有"现在几点""这是第几次请求"这类东西，也没有任何 I/O：
//! 那些属于策略与取数，不属于渲染。
//!
//! 两个视图，按事实直接判得出来：
//!
//! - **Full**：这段正文就是全部（流里没有更多字节）。
//! - **Excerpt**：正文是一部分——头尾各留一段，中间写明省了多少，并给出**可直接 `read` 的
//!   完整输出路径**。省略不等于丢失：完整事实始终在 `output.json` / `stdout.txt` 里。
//!
//! 第三种（Handle：连正文都不给，只留引用）是**按请求**决定的——同一份事实在不同轮次可以
//! 有不同的视图，而当前阶段只投影一次（`docs/komo_observation.md` 的第二阶段）。

use std::path::Path;

use crate::types::refs::{ContentRef, OutputRef, ToolResultStatus};

/// 投影用的全部事实。
///
/// **每一格都能从落盘的东西里读回来**（`tool.result` 事件 + `output.json`），所以回放时才
/// 渲染得出同一个字符串。工具名来自那一轮的调用计划，`session_root` 来自这次执行的环境。
#[derive(Debug, Clone, Copy)]
pub struct ToolResultFacts<'a> {
    pub tool: &'a str,
    pub status: ToolResultStatus,
    pub elapsed_ms: u64,
    /// 工具自己写给模型看的那段正文（`ToolResultBody::preview`）。没有就只剩引用可给。
    pub text: Option<&'a str>,
    /// 这次尝试的输出引用（`output.json`）。
    pub output: &'a OutputRef,
    pub stdout: Option<&'a ContentRef>,
    pub stderr: Option<&'a ContentRef>,
    /// 这个 Session 的内容目录。给了才印得出**可直接 `read` 的绝对路径**——模型的工作目录
    /// 是 workspace，相对 Session 目录的路径它解析不了（§8.3）。
    pub session_root: Option<&'a Path>,
}

/// 这一次投影的预算：正文最多占多少字节。
#[derive(Debug, Clone, Copy)]
pub struct ProjectionContext {
    pub model_result_bytes: usize,
}

/// 交给模型的正文默认预算。
///
/// **它和 JSONL 里那 1 KiB 是两件事**：那一份是账本的行预算（每行都要小），这一份是模型
/// 上下文预算。默认值由配置覆盖（§6「Gateway 设置……输出长度」）。
pub const DEFAULT_MODEL_RESULT_BYTES: usize = 8 * 1024;

/// 交给模型的正文（§8.3）。
pub fn project(facts: &ToolResultFacts<'_>, ctx: &ProjectionContext) -> String {
    let mut out = header(facts);
    let room = ctx.model_result_bytes.saturating_sub(out.len());

    let text = facts.text.unwrap_or_default();
    let cut = truncate_head_tail(text, room);
    out.push_str(cut.head);
    if cut.omitted > 0 {
        // 省略那条**夹在头尾之间**：读者顺着往下读，走到这里就知道中间跳了多远。
        out.push_str(&format!("\n…（中间省略 {} 字节）…\n", cut.omitted));
        out.push_str(cut.tail);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if cut.omitted > 0 || has_more(facts, text) {
        out.push_str(&recall(facts));
    }
    out
}

/// 抬头那一行：谁、什么状态、花了多久、流里有多少字节。
///
/// 大小来自引用本身，不从工具的正文里猜——那样每加一个工具就要在这里加一条解析规则。
fn header(facts: &ToolResultFacts<'_>) -> String {
    let mut out = format!(
        "[{} · {} · {}",
        facts.tool,
        status_text(facts.status),
        millis(facts.elapsed_ms)
    );
    for (label, reference) in [("stdout", facts.stdout), ("stderr", facts.stderr)] {
        if let Some(reference) = reference {
            out.push_str(&format!(" · {label} {}", bytes(reference.size)));
        }
    }
    out.push_str("]\n");
    out
}

/// 「完整输出在哪」——只有在**确实还有更多**的时候才印，否则那行就是对正文的噪音。
fn recall(facts: &ToolResultFacts<'_>) -> String {
    let mut out = String::new();
    let where_to_read = match facts.session_root {
        // 会话目录 + 引用里的相对路径，拼出来就是模型能直接 `read` 的绝对路径。
        Some(root) => root.join(facts.output.path()).display().to_string(),
        None => format!("{}（相对 Session 目录）", facts.output.path()),
    };
    out.push_str(&format!("完整输出：{where_to_read}"));
    if facts.stdout.is_some() || facts.stderr.is_some() {
        out.push_str("（同目录下有 stdout.txt / stderr.txt）");
    }
    out.push('\n');
    out
}

/// 这段正文是不是只是整个输出的一部分。
///
/// 判据是**引用里的大小**：流里有 812 KB 而我们只拿着 4 KB，那就是有更多。没有流引用时
/// 只能靠工具自己写的正文说清楚（`read` 的抬头就写着未读范围）。
fn has_more(facts: &ToolResultFacts<'_>, text: &str) -> bool {
    if facts.text.is_none() {
        return true;
    }
    let stream = facts.stdout.map_or(0, |reference| reference.size)
        + facts.stderr.map_or(0, |reference| reference.size);
    stream > text.len() as u64
}

struct Truncated<'a> {
    head: &'a str,
    tail: &'a str,
    omitted: u64,
}

/// 头尾各留一段，中间省略。
///
/// **不是只留头**：`cargo test` 的错误在尾部，只留头等于把要找的那一行丢掉（§8.3 的
/// "超限时提供截断提示"不该变成"信息销毁"）。切点落在字符边界上，UTF-8 不会从中间切开。
fn truncate_head_tail(text: &str, room: usize) -> Truncated<'_> {
    if text.len() <= room {
        return Truncated {
            head: text,
            tail: "",
            omitted: 0,
        };
    }
    let head = boundary(text, room / 2);
    let tail_start = boundary_rev(text, text.len() - (room - head));
    Truncated {
        head: &text[..head],
        tail: &text[tail_start..],
        omitted: (tail_start - head) as u64,
    }
}

/// 不大于 `limit` 的最大字符边界。
fn boundary(text: &str, limit: usize) -> usize {
    let mut cut = limit.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// 不小于 `floor` 的最小字符边界。
fn boundary_rev(text: &str, floor: usize) -> usize {
    let mut cut = floor.min(text.len());
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    cut
}

fn status_text(status: ToolResultStatus) -> &'static str {
    match status {
        ToolResultStatus::Completed => "完成",
        ToolResultStatus::Failed => "失败",
        // 副作用发生没发生不知道——不能在这里说成失败（§8.6）。
        ToolResultStatus::Uncertain => "结果不明",
    }
}

fn millis(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        _ => format!("{:.1}s", ms as f64 / 1000.0),
    }
}

/// 人看得懂的大小。**不改精度在 KB 以下**：一个 512 字节的输出写成 0.5 KB 只是更难对。
fn bytes(size: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let size = size as f64;
    if size < KB {
        format!("{size} B")
    } else if size < MB {
        format!("{:.1} KB", size / KB)
    } else {
        format!("{:.1} MB", size / MB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::digest::ContentHash;
    use std::path::PathBuf;

    fn reference(path: &str, size: u64) -> ContentRef {
        ContentRef {
            path: path.into(),
            size,
            hash: ContentHash::of_str(path),
            pointer: None,
        }
    }

    fn facts<'a>(text: Option<&'a str>, stdout: Option<&'a ContentRef>) -> ToolResultFacts<'a> {
        ToolResultFacts {
            tool: "shell",
            status: ToolResultStatus::Completed,
            elapsed_ms: 1_240,
            text,
            output: &OUTPUT,
            stdout,
            stderr: None,
            session_root: None,
        }
    }

    static OUTPUT_PATH: &str = "tool-output/run-1/call-7/attempt-1/output.json";
    static OUTPUT: std::sync::LazyLock<OutputRef> =
        std::sync::LazyLock::new(|| OutputRef(reference(OUTPUT_PATH, 64)));
    static STDOUT: std::sync::LazyLock<ContentRef> = std::sync::LazyLock::new(|| {
        reference("tool-output/run-1/call-7/attempt-1/stdout.txt", 400)
    });

    fn budget(model_result_bytes: usize) -> ProjectionContext {
        ProjectionContext { model_result_bytes }
    }

    #[test]
    fn a_small_result_is_shown_whole_with_its_header() {
        let printed = project(&facts(Some("exit=0\n全部输出\n"), None), &budget(1024));
        assert!(printed.starts_with("[shell · 完成 · 1.2s]\n"), "{printed}");
        assert!(printed.contains("全部输出"), "{printed}");
        assert!(
            !printed.contains("完整输出："),
            "没有更多可读的东西时不留那一行：{printed}"
        );
    }

    #[test]
    fn a_stream_bigger_than_the_text_says_where_the_rest_is() {
        // 流里有 400 字节，工具只交出 3 个字符——那 397 字节还在 stdout.txt 里。
        let printed = project(&facts(Some("ab\n"), Some(&STDOUT)), &budget(1024));
        assert!(printed.contains("stdout 400 B"), "{printed}");
        assert!(
            printed.contains("完整输出：tool-output/run-1/call-7/attempt-1/output.json"),
            "{printed}"
        );
        assert!(printed.contains("stdout.txt / stderr.txt"), "{printed}");
    }

    #[test]
    fn a_session_root_turns_the_reference_into_a_path_the_model_can_read() {
        let root = PathBuf::from("/home/yi/.komo/sessions/sess-1");
        let mut facts = facts(Some("head\ntail\n"), Some(&STDOUT));
        facts.session_root = Some(&root);
        let printed = project(&facts, &budget(1024));
        assert!(
            printed.contains("完整输出：/home/yi/.komo/sessions/sess-1/tool-output/run-1/call-7/attempt-1/output.json"),
            "{printed}"
        );
    }

    #[test]
    fn what_is_too_big_keeps_both_ends_and_says_how_much_went_missing() {
        let text = format!("{}中间{}", "h".repeat(600), "t".repeat(600));
        let printed = project(&facts(Some(&text), None), &budget(400));
        let head = printed.find(&"h".repeat(64)).expect("头部要留着");
        let marker = printed.find("…（中间省略").expect("要说省了多少");
        let tail = printed.find(&"t".repeat(64)).expect("尾部也要留着");
        assert!(
            head < marker && marker < tail,
            "顺序是头 → 省略 → 尾：\n{printed}"
        );

        // 省了多少不是随口说的：印出来的头尾加省略数，正好是原文。
        let omitted: usize = printed
            .split("省略 ")
            .nth(1)
            .and_then(|rest| rest.split(" 字节").next())
            .and_then(|number| number.parse().ok())
            .expect("省略数是个整数");
        let shown = text.len() - omitted;
        assert!(
            text.starts_with(&printed[head..head + 64])
                && text.ends_with(&printed[tail..tail + 64]),
            "头尾都是原文里的那一段：\n{printed}"
        );
        assert!(shown <= text.len(), "{shown} > {}", text.len());
        assert!(shown >= 300, "预算 400 摆在那儿，别只印几个字节：{shown}");
    }

    #[test]
    fn cutting_never_splits_a_character() {
        // 每个汉字 3 字节：预算落在字符中间时，两边都退到边界上。
        let text = "汉字".repeat(50);
        let printed = project(&facts(Some(&text), None), &budget(101));
        assert!(printed.contains('汉'), "{printed}");
        assert!(printed.contains('字'), "{printed}");
    }

    #[test]
    fn a_result_without_text_still_gives_the_reference() {
        let printed = project(&facts(None, Some(&STDOUT)), &budget(1024));
        assert!(
            printed.starts_with("[shell · 完成 · 1.2s · stdout 400 B]\n"),
            "{printed}"
        );
        assert!(printed.contains("完整输出："), "{printed}");
    }

    #[test]
    fn the_status_is_said_in_words_and_uncertain_is_not_a_failure() {
        let mut uncertain = facts(Some("x\n"), None);
        uncertain.status = ToolResultStatus::Uncertain;
        let printed = project(&uncertain, &budget(1024));
        assert!(printed.contains("结果不明"), "{printed}");
        assert!(!printed.contains("失败"), "{printed}");
    }

    #[test]
    fn the_same_facts_render_the_same_bytes() {
        let one = project(&facts(Some("a\nb\n"), Some(&STDOUT)), &budget(64));
        let two = project(&facts(Some("a\nb\n"), Some(&STDOUT)), &budget(64));
        assert_eq!(one, two, "投影是纯函数：同一份事实两次投影必须一样");
    }
}
