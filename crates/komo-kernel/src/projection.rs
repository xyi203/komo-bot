//! 一次工具结果在**模型上下文**里的样子（§8.3）。
//!
//! 事实在账本与输出存储里——状态、引用、大小、工具自己写的那段正文——这里是它的**投影**。
//! 投影是纯函数，而且**同一个事实只该有一处渲染**：刚跑完的那一次与重启之后回放，必须得到
//! 逐字节相同的字符串。所以输入里没有"现在几点""这是第几次请求"这类东西，也没有任何 I/O：
//! 那些属于策略与取数，不属于渲染。
//!
//! 同一份事实渲染出**两个视图**（[`project_views`]）：
//!
//! - **full**：放得下（30 KiB / 2000 行以内）就给全文；放不下只给开头 2 KiB 的预览，加一行
//!   写明原文多大、为什么只给这些。还有更多时给出**可直接 `read` 的完整输出引用**
//!   （`artifact://<run>/<call>/<attempt>/…`，§4.7）。
//! - **decayed**：大结果换下来的稳定短视图——抬头 + 首尾整行 + 那条引用。
//!   省略不等于丢失：完整事实始终在 `output.json` / `stdout.txt` 里。
//!
//! 某一次请求用哪个视图**不在这里判**：换不换是改写提示前缀，按缓存账决定、记成
//! `context.compacted` 的 `decayed`（§8.3）；这里只回答"有没有资格换"（[`decay_eligible`]）
//! 与"按那个决定给哪份正文"（[`select`]）。

use serde::{Deserialize, Serialize};

use crate::types::ids::{AttemptId, RunId, ToolCallId};
use crate::types::refs::{ContentRef, ImageRef, OutputRef, ToolResultStatus};
use crate::types::resource::{OutputFile, ResourceUri};

/// 投影用的全部事实。
///
/// **每一格都能从落盘的东西里读回来**（`tool.result` 事件 + `output.json`），所以回放时才
/// 渲染得出同一个字符串。工具名来自那一轮的调用计划，引用与流的大小来自那次尝试的输出。
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
    /// 这一轮**产出**的文件（`output.json` 里那一格）。给模型的是入口与大小，不是正文
    /// ——正文按引用去 `read`（§4.7）。
    pub artifacts: &'a [ContentRef],
    /// 随结果附给模型的图片（`output.json` 里那一格）。正文里只印一行说明，字节另走。
    pub images: &'a [ImageRef],
}

/// 这一次投影的设置：正文预算与衰减策略。
///
/// 它冻结在 `RunSnapshot` 里（§6.2）：一条 Run 从头到尾、刚跑完与回放都按同一份投影，
/// 配置热重载只影响之后受理的 Run。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionContext {
    pub model_result_bytes: usize,
    /// `None` = 不衰减（`decay_threshold_bytes = 0`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decay: Option<DecayPolicy>,
}

/// 大结果什么时候、换成多短的视图（`[execution] decay_*`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecayPolicy {
    /// full 视图超过这么多字节才有 decayed 视图。
    pub threshold_bytes: usize,
    /// 完整给过几次之后才有资格换成 decayed；真换不换由缓存账决定（§8.3）。
    pub full_sends: u32,
    /// decayed 视图开头最多留多少字节的整行。
    pub head_bytes: usize,
    /// decayed 视图结尾最多留多少字节的整行。
    pub tail_bytes: usize,
}

/// 交给模型的正文默认预算，**也是上限**：配置只能往小调。
///
/// 这只约束模型视图，不约束 JSONL 或 tool-output 中保存的完整事实。默认值可由配置覆盖，
/// 并随 [`ProjectionContext`] 冻结进 RunSnapshot（§6「Gateway 设置……输出长度」）。
///
/// 30 KiB 是照模型后端定的：magpie 把工具结果当 MCP 结果交给 Claude Code，超过
/// `MAX_MCP_OUTPUT_TOKENS`（默认 25000 token）它就把**整条**结果换成一句报错、全文另存——
/// 账上记的是一份，模型看到的是另一份，komo 却不知道。30 KiB 的日志 / JSON / 中文都远在
/// 25000 token 以下，所以裁剪总在这里发生、写在正文里，后端不会再动它。
pub const DEFAULT_MODEL_RESULT_BYTES: usize = 30 * 1024;
/// 交给模型的工具结果正文最多多少行；行数与字节数谁先超都按放不下处理。
pub const MODEL_RESULT_LINES: usize = 2000;
/// 放不下时开头预览给多少字节的整行。预览只用来让模型认出这是什么、决定去读哪一段。
pub const PREVIEW_BYTES: usize = 2 * 1024;

/// decayed 视图至少要比 full 短这么多才值得换：省几十字节却让模型面对一份变了样的
/// 结果，不划算。
pub const MIN_DECAY_SAVING_BYTES: usize = 1024;

/// 同一份事实的两个视图。`decayed` 只在值得换的时候有（见 [`project_views`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projected {
    pub full: String,
    pub decayed: Option<String>,
}

/// 已经完整给过 `sends_so_far` 次的这条结果，有没有资格换成短视图：有 decayed 视图、
/// 且完整给够了次数。次数 = 这条 Run 在它之后记了几条 `message.assistant`。
pub fn decay_eligible(projected: &Projected, sends_so_far: u32, ctx: &ProjectionContext) -> bool {
    projected.decayed.is_some()
        && ctx
            .decay
            .is_some_and(|policy| sends_so_far >= policy.full_sends)
}

/// 按已经记下的决定给正文：换过的给 decayed（没有 decayed 视图时仍是 full）。刚跑完与
/// 回放都从这里过，所以两边给出同一份字节。
pub fn select(projected: Projected, decayed: bool) -> String {
    match projected.decayed {
        Some(view) if decayed => view,
        _ => projected.full,
    }
}

/// 交给模型的正文（§8.3）：full 视图。放得下给全文，放不下给开头预览（[`push_preview`]）。
///
/// 每个工具都一样：bash 的退出码与流大小在正文第一行，预览里就有。
pub fn project(facts: &ToolResultFacts<'_>, ctx: &ProjectionContext) -> String {
    let mut out = header(facts);
    let limit = ctx.model_result_bytes.min(DEFAULT_MODEL_RESULT_BYTES);
    let room = limit.saturating_sub(out.len());

    let text = facts.text.unwrap_or_default();
    let fits = text.len() <= room && text.lines().count() <= MODEL_RESULT_LINES;
    if fits {
        push_excerpt(&mut out, &whole(text));
    } else {
        push_preview(&mut out, text, limit, room);
    }
    if !fits || has_more(facts, text) {
        out.push_str(&recall(facts));
    }
    out.push_str(&artifacts(facts));
    out.push_str(&images(facts));
    out
}

/// 两个视图一起渲染（§8.3）。
///
/// decayed 只在三件事都成立时才有：结果是 `完成`（失败与结果不明的正文是模型要对着改的
/// 东西，不衰减）、full 超过阈值、decayed 比 full 至少短 [`MIN_DECAY_SAVING_BYTES`]。
pub fn project_views(facts: &ToolResultFacts<'_>, ctx: &ProjectionContext) -> Projected {
    let full = project(facts, ctx);
    let decayed = ctx
        .decay
        .filter(|_| facts.status == ToolResultStatus::Completed)
        .filter(|policy| full.len() > policy.threshold_bytes)
        .map(|policy| decay(facts, &policy))
        .filter(|decayed| decayed.len() + MIN_DECAY_SAVING_BYTES <= full.len());
    Projected { full, decayed }
}

/// decayed 视图：抬头 + 一句交代 + 首尾整行 + **总是**给出完整输出的引用。
///
/// 摘录取自工具的原正文（事实），不是从 full 那份已经裁过的字符串里再裁。
fn decay(facts: &ToolResultFacts<'_>, policy: &DecayPolicy) -> String {
    let mut out = header(facts);
    let text = facts.text.unwrap_or_default();
    out.push_str(&format!(
        "（这份结果前文已完整给过，此后只留首尾整行：原文 {} 字节 / {} 行）\n",
        text.len(),
        text.lines().count()
    ));
    let cut = excerpt(text, policy.head_bytes, policy.tail_bytes);
    push_excerpt(&mut out, &cut);
    out.push_str(&recall(facts));
    out.push_str(&artifacts(facts));
    out.push_str(&images(facts));
    out
}

/// 放不下的正文：开头一段整行预览，再用一行写明原文多大、为什么只给这些、剩下的怎么看。
///
/// 裁剪**必须写在正文里**：模型据此知道自己看到的不是全部，不会把预览当成结论的依据。
fn push_preview(out: &mut String, text: &str, limit: usize, room: usize) {
    let head = &text[..head_end(text, PREVIEW_BYTES.min(room))];
    out.push_str(head);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "…（正文 {} 字节 / {} 行，超过单条结果上限（{} / {} 行），只给了开头 {} 字节；\
         其余在下面的完整输出里，用 read 的 start_line/line_count 分段读，或用 rg 搜要的那几行）…\n",
        text.len(),
        text.lines().count(),
        bytes(limit as u64),
        MODEL_RESULT_LINES,
        head.len()
    ));
}

/// 头 → 省略那条 → 尾，结尾补齐换行。
///
/// 省略那条**夹在头尾之间**：读者顺着往下读，走到这里就知道中间跳了多远。
fn push_excerpt(out: &mut String, cut: &Truncated<'_>) {
    out.push_str(cut.head);
    if cut.omitted > 0 {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("…（中间省略 {} 字节）…\n", cut.omitted));
        out.push_str(cut.tail);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
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
///
/// 印的是 `artifact://` **入口**（§4.7）：模型把它原样交给 `read` 就拿到了完整正文，而
/// 绝对路径既是这台机器的事、又把"这是**这个会话自己的**哪一次落盘"说漏了——一句话在两处
/// 说同一件事，改一处就会漏一处。run / call / attempt 从**落盘事实**（引用里那个 Session
/// 相对路径）里取，所以刚跑完与重启之后回放拿到的是同一串字节。
fn recall(facts: &ToolResultFacts<'_>) -> String {
    let mut out = format!(
        "完整输出：{}",
        where_to_read(facts.output.path(), OutputFile::Result)
    );
    let mut streams: Vec<String> = Vec::new();
    for (file, stream) in [
        (OutputFile::Stdout, facts.stdout),
        (OutputFile::Stderr, facts.stderr),
    ] {
        if let Some(stream) = stream {
            streams.push(where_to_read(&stream.path, file));
        }
    }
    if !streams.is_empty() {
        out.push_str(&format!("（流：{}）", streams.join("、")));
    }
    out.push('\n');
    out
}

/// 引用里那个 Session 相对路径 → 模型能直接 `read` 的入口；认不出来时如实给相对路径。
///
/// **不编**：`output_uri` 认不出来（形状不是 `tool-output/<run>/<call>/<attempt>/<文件>`、
/// 或者最后那一段不是这个文件）就说明这份引用不是我们落盘的那一次，这时给一个"看起来像
/// 资源"的 URI 才是撒谎——那会让模型拿着一个读不回来的东西去试。
fn where_to_read(path: &str, file: OutputFile) -> String {
    match output_uri(path, file) {
        Some(uri) => uri.to_string(),
        None => format!("{path}（相对 Session 目录）"),
    }
}

/// 落盘路径 → 它对应的资源入口。**路径的形状就是事实**（`komo-store` 只写这一种：
/// `tool-output/<run>/<call>/<attempt>/…`），所以这里做的是解读，不是猜测。
fn output_uri(path: &str, file: OutputFile) -> Option<ResourceUri> {
    let mut segments = path.strip_prefix("tool-output/")?.split('/');
    let (run, call, attempt) = (segments.next()?, segments.next()?, segments.next()?);
    let named = segments.next() == Some(file.file_name());
    if !named || run.is_empty() || call.is_empty() || attempt.is_empty() {
        return None;
    }
    Some(ResourceUri::Output {
        run: RunId::from_raw(run),
        call: ToolCallId::from_raw(call),
        attempt: AttemptId::from_raw(attempt),
        file,
    })
}

/// 这一轮**产出**的文件：一行一个，印 `artifact://files/…` 入口与大小。
///
/// **不印正文**：产物多半是二进制或很大的东西，抄进上下文就是把预算花在"再说一遍"上
/// （§8.3 的老规矩：省略不等于丢失，正文按引用去 `read`）。没有这一句，模型根本不知道
/// 自己产出了什么——§4.7 要的正是"正文里给出那一条入口"。
///
/// 映射不出入口的（`artifacts/` 之外的路径）**跳过**：那不是产物引用，编一个入口出来
/// 只会让模型去读一个不存在的文件（`artifact_uri` 是这件事唯一的那处映射）。
fn artifacts(facts: &ToolResultFacts<'_>) -> String {
    let mut out = String::new();
    for artifact in facts.artifacts {
        let Some(uri) = crate::types::artifact_uri(&artifact.path) else {
            continue;
        };
        out.push_str(&format!("产物：{uri}（{}）\n", bytes(artifact.size)));
    }
    out
}

/// 随结果附上的图片：一行一张，印格式与大小。图片本身由各家接口接在这条结果后面。
fn images(facts: &ToolResultFacts<'_>) -> String {
    facts
        .images
        .iter()
        .map(|image| {
            format!(
                "图片：{}（{}），已附在这条结果里\n",
                image.mime,
                bytes(image.content.size)
            )
        })
        .collect()
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

/// 头尾各留一段，预算各自固定（decayed 视图）。
fn excerpt(text: &str, head_bytes: usize, tail_bytes: usize) -> Truncated<'_> {
    if text.len() <= head_bytes + tail_bytes {
        return whole(text);
    }
    let head = head_end(text, head_bytes);
    let tail = tail_start(text, tail_bytes);
    Truncated {
        head: &text[..head],
        tail: &text[tail..],
        omitted: (tail - head) as u64,
    }
}

fn whole(text: &str) -> Truncated<'_> {
    Truncated {
        head: text,
        tail: "",
        omitted: 0,
    }
}

/// 头的终点：不超过 `limit` 字节的**整行**。整行留不到预算的一半（第一行自己就超了，
/// 或者紧跟着一行很长的，比如一整行 JSON）时退到字符边界——有正文就不给一段空的或
/// 白白浪费预算的摘录。
fn head_end(text: &str, limit: usize) -> usize {
    let cut = boundary(text, limit);
    if cut == text.len() {
        return cut;
    }
    match text[..cut].rfind('\n') {
        Some(newline) if (newline + 1) * 2 >= cut => newline + 1,
        _ => cut,
    }
}

/// 尾的起点：不超过 `limit` 字节的**整行**。同样，整行留不到预算的一半时退到字符边界。
fn tail_start(text: &str, limit: usize) -> usize {
    let floor = boundary_rev(text, text.len().saturating_sub(limit));
    if floor == 0 || text.as_bytes()[floor - 1] == b'\n' {
        return floor;
    }
    match text[floor..].find('\n') {
        Some(newline) if (text.len() - (floor + newline + 1)) * 2 >= text.len() - floor => {
            floor + newline + 1
        }
        _ => floor,
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
            tool: "bash",
            status: ToolResultStatus::Completed,
            elapsed_ms: 1_240,
            text,
            output: &OUTPUT,
            stdout,
            stderr: None,
            artifacts: &[],
            images: &[],
        }
    }

    static OUTPUT_PATH: &str = "tool-output/run-1/call-7/attempt-1/output.json";
    static STDOUT_PATH: &str = "tool-output/run-1/call-7/attempt-1/stdout.txt";
    static OUTPUT: std::sync::LazyLock<OutputRef> =
        std::sync::LazyLock::new(|| OutputRef(reference(OUTPUT_PATH, 64)));
    static STDOUT: std::sync::LazyLock<ContentRef> =
        std::sync::LazyLock::new(|| reference(STDOUT_PATH, 400));

    fn budget(model_result_bytes: usize) -> ProjectionContext {
        ProjectionContext {
            model_result_bytes,
            decay: None,
        }
    }

    const POLICY: DecayPolicy = DecayPolicy {
        threshold_bytes: 4096,
        full_sends: 2,
        head_bytes: 2048,
        tail_bytes: 1536,
    };

    fn decaying(model_result_bytes: usize) -> ProjectionContext {
        ProjectionContext {
            model_result_bytes,
            decay: Some(POLICY),
        }
    }

    /// `n` 行，每行 51 字节，行号在前——摘出来的是哪几行一眼看得出。
    fn lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("line {i:04}: {}\n", "x".repeat(40)))
            .collect()
    }

    #[test]
    fn a_small_result_is_shown_whole_with_its_header() {
        let printed = project(&facts(Some("exit=0\n全部输出\n"), None), &budget(1024));
        assert!(printed.starts_with("[bash · 完成 · 1.2s]\n"), "{printed}");
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
            printed.contains("完整输出：artifact://run-1/call-7/attempt-1/result"),
            "{printed}"
        );
        assert!(
            printed.contains("流：artifact://run-1/call-7/attempt-1/stdout"),
            "{printed}"
        );
    }

    /// 引用是**入口**，不是路径：它指的正是落盘的那一份 `output.json` / `stdout.txt`。
    ///
    /// 从前这里印绝对路径，于是"给模型的这一句"与"允许读哪个逻辑资源"是两句话。现在它们
    /// 是同一句——`artifact://` 入口由 Session 目录拼出来的那**一份**落盘事实说了算
    /// （`output_file_path` 是两处共用的那一份拼法）。
    #[test]
    fn the_reference_names_the_output_that_is_actually_on_disk() {
        let session_root = PathBuf::from("/home/yi/.komo/sessions/sess-1");
        let printed = project(&facts(Some("ab\n"), Some(&STDOUT)), &budget(1024));

        for (file, on_disk) in [
            (OutputFile::Result, OUTPUT_PATH),
            (OutputFile::Stdout, STDOUT_PATH),
        ] {
            let uri = ResourceUri::Output {
                run: RunId::from_raw("run-1"),
                call: ToolCallId::from_raw("call-7"),
                attempt: AttemptId::from_raw("attempt-1"),
                file,
            };
            assert!(printed.contains(&uri.to_string()), "{printed}");
            assert_eq!(
                crate::types::resource::output_file_path(&session_root, &uri),
                Some(session_root.join(on_disk)),
                "印出来的入口要正好指向落盘的那一份"
            );
        }
    }

    /// 产物：**入口 + 大小**，一行一个；正文不抄进上下文（正文按引用去读）。
    #[test]
    fn the_artifacts_of_a_round_are_named_so_the_model_can_read_them() {
        let report = reference("artifacts/run-1/报告.md", 13);
        let big = reference("artifacts/run-1/blob.bin", 3 * 1024 * 1024);
        let odd = reference("payloads/not-an-artifact.json", 4);
        let artifacts = [report, big, odd];

        let facts = ToolResultFacts {
            artifacts: &artifacts,
            ..facts(Some("写完了\n"), None)
        };
        let printed = project(&facts, &budget(1024));
        assert!(printed.contains("写完了"), "{printed}");
        assert!(
            printed.contains("产物：artifact://files/run-1/报告.md（13 B）"),
            "{printed}"
        );
        assert!(
            printed.contains("产物：artifact://files/run-1/blob.bin（3.0 MB）"),
            "{printed}"
        );
        assert!(
            !printed.contains("not-an-artifact"),
            "映射不出入口的路径不是产物，别编一条：{printed}"
        );
        // 产物那条**不受预算影响**：它说的是"这一轮产出了什么"，与正文省了多少无关。
        assert!(!printed.contains("完整输出："), "{printed}");
    }

    /// 图片只印一行说明；字节不进正文。
    #[test]
    fn an_attached_image_is_named_in_the_text() {
        let images = [ImageRef {
            mime: "image/png".into(),
            content: reference("tool-output/run-1/call-7/attempt-1/image-1.png", 2048),
        }];
        let facts = ToolResultFacts {
            tool: "read",
            images: &images,
            ..facts(Some("shot.png 是一张图片\n"), None)
        };
        let printed = project(&facts, &budget(1024));
        assert!(
            printed.ends_with("图片：image/png（2.0 KB），已附在这条结果里\n"),
            "{printed}"
        );
        assert!(!printed.contains("完整输出："), "{printed}");
    }

    /// 没有产物时一个字都不多——那段话是给"真的产出了文件"准备的。
    #[test]
    fn a_round_without_artifacts_says_nothing_about_them() {
        let printed = project(&facts(Some("exit=0\n"), None), &budget(1024));
        assert!(!printed.contains("产物："), "{printed}");
    }

    /// 认不出来源的引用**不编** URI：那会让模型拿着一个读不回来的东西去试。
    #[test]
    fn a_reference_we_cannot_name_as_a_resource_still_names_its_file() {
        let foreign = OutputRef(reference("payloads/elsewhere.json", 8));
        let foreign_stdout = reference("payloads/stdout.txt", 400);
        let facts = ToolResultFacts {
            output: &foreign,
            stdout: Some(&foreign_stdout),
            ..facts(Some("ab\n"), None)
        };
        let printed = project(&facts, &budget(1024));
        assert!(
            printed.contains("完整输出：payloads/elsewhere.json（相对 Session 目录）"),
            "{printed}"
        );
        assert!(
            printed.contains("流：payloads/stdout.txt（相对 Session 目录）"),
            "{printed}"
        );
        assert!(!printed.contains("artifact://"), "{printed}");
    }

    /// 放不下的结果每个工具都一样：开头预览 + 一行写明裁了多少、为什么 + 完整输出的入口。
    #[test]
    fn an_oversized_result_gets_a_visible_head_preview_for_every_tool() {
        let text = format!("HEAD\n{}TAIL\n", "middle\n".repeat(200));
        for tool in ["bash", "shell", "read", "rg"] {
            let oversized = ToolResultFacts {
                tool,
                ..facts(Some(&text), None)
            };
            let printed = project(&oversized, &budget(400));
            assert!(printed.contains("HEAD\nmiddle\n"), "要留开头：{printed}");
            assert!(!printed.contains("TAIL"), "预览只有开头：{printed}");
            assert!(
                printed.contains(&format!(
                    "…（正文 {} 字节 / 202 行，超过单条结果上限",
                    text.len()
                )),
                "裁剪要写在正文里：{printed}"
            );
            assert!(printed.contains("完整输出：artifact://"), "{printed}");
            assert!(printed.len() <= 400 + 400, "{printed}");
        }
    }

    /// 配置写得比上限大也不放大：超过后端那条线的结果会被整条换掉，那时谁都看不见裁剪。
    #[test]
    fn a_configured_budget_above_the_cap_is_held_to_the_cap() {
        let text = lines(1000);
        assert!(text.len() > DEFAULT_MODEL_RESULT_BYTES && text.len() < 64 * 1024);
        let printed = project(&facts(Some(&text), None), &budget(64 * 1024));
        assert!(
            printed.contains("超过单条结果上限（30.0 KB / 2000 行）"),
            "{printed}"
        );
        assert!(!printed.contains("line 0999: "), "{printed}");
    }

    /// 不管正文多大、长什么样，交给模型的那一份都在上限之内——裁剪只发生在 komo 这里。
    #[test]
    fn every_result_stays_within_the_cap() {
        let cap = DEFAULT_MODEL_RESULT_BYTES;
        let texts = [
            "x\n".repeat(cap / 2 - 64),
            "x\n".repeat(cap / 2 + 64),
            lines(4000),
            "汉".repeat(200_000),
            (0..2_001).map(|n| format!("{n}\n")).collect(),
        ];
        for text in &texts {
            let printed = project(&facts(Some(text), Some(&STDOUT)), &budget(cap));
            assert!(
                printed.len() <= cap + 512,
                "{} 字节的正文投影成了 {} 字节",
                text.len(),
                printed.len()
            );
        }
    }

    /// python 工具（2026-10-15 删除）留下的结果：投影只认落盘的事实，不认工具名——抬头带
    /// stdout 大小，短正文原样给，长正文给开头预览，截断时给完整引用。
    #[test]
    fn a_historical_python_result_replays_byte_for_byte() {
        let stdout = STDOUT.clone();
        let short = ToolResultFacts {
            tool: "python",
            ..facts(Some("{\"count\": 3}\n"), Some(&stdout))
        };
        assert_eq!(
            project(&short, &budget(1024)),
            "[python · 完成 · 1.2s · stdout 400 B]\n{\"count\": 3}\n完整输出：artifact://run-1/call-7/attempt-1/result（流：artifact://run-1/call-7/attempt-1/stdout）\n"
        );

        let text = format!("HEAD\n{}TAIL\n", "middle\n".repeat(200));
        let long = ToolResultFacts {
            tool: "python",
            ..facts(Some(&text), Some(&stdout))
        };
        let printed = project(&long, &budget(400));
        assert!(
            printed.starts_with("[python · 完成 · 1.2s · stdout 400 B]\nHEAD\n"),
            "{printed}"
        );
        assert!(!printed.contains("TAIL"), "{printed}");
        assert!(
            printed.ends_with(
                "完整输出：artifact://run-1/call-7/attempt-1/result（流：artifact://run-1/call-7/attempt-1/stdout）\n"
            ),
            "{printed}"
        );
    }

    #[test]
    fn default_model_limits_stay_under_the_backend_cap() {
        assert_eq!(DEFAULT_MODEL_RESULT_BYTES, 30 * 1024);
        assert_eq!(MODEL_RESULT_LINES, 2000);
        assert_eq!(PREVIEW_BYTES, 2 * 1024);
    }

    #[test]
    fn a_single_long_line_previews_on_a_char_boundary() {
        let text = "汉".repeat(30_000);
        let printed = project(
            &facts(Some(&text), None),
            &budget(DEFAULT_MODEL_RESULT_BYTES),
        );
        let head = printed.lines().nth(1).expect("抬头之后是预览");
        assert!(!head.is_empty(), "有正文就不给空预览");
        assert!(head.len() <= PREVIEW_BYTES && head.len() > PREVIEW_BYTES - 3);
        assert!(head.chars().all(|ch| ch == '汉'), "{head}");
    }

    #[test]
    fn line_limit_is_independent_from_the_byte_limit() {
        let text = (0..2_001).map(|n| format!("{n}\n")).collect::<String>();
        assert!(text.len() < DEFAULT_MODEL_RESULT_BYTES);
        let printed = project(
            &facts(Some(&text), None),
            &budget(DEFAULT_MODEL_RESULT_BYTES),
        );
        assert!(printed.contains("\n0\n1\n"), "{printed}");
        assert!(!printed.contains("\n2000\n"), "{printed}");
        assert!(
            printed.contains(&format!("…（正文 {} 字节 / 2001 行，", text.len())),
            "{printed}"
        );
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
            printed.starts_with("[bash · 完成 · 1.2s · stdout 400 B]\n"),
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
        let artifacts = [reference("artifacts/run-1/报告.md", 13)];
        let text = lines(400);
        let one = ToolResultFacts {
            artifacts: &artifacts,
            ..facts(Some(&text), Some(&STDOUT))
        };
        let two = ToolResultFacts {
            artifacts: &artifacts,
            ..facts(Some(&text), Some(&STDOUT))
        };
        let ctx = decaying(DEFAULT_MODEL_RESULT_BYTES);
        let printed = project_views(&one, &ctx);
        assert_eq!(
            printed,
            project_views(&two, &ctx),
            "投影是纯函数：同一份事实两次投影，两个视图都必须一样"
        );
        assert_eq!(printed.full, project(&one, &ctx));
        let decayed = printed.decayed.expect("这么大的结果要有 decayed 视图");
        for view in [&printed.full, &decayed] {
            assert!(
                view.contains("产物：artifact://files/run-1/报告.md"),
                "{view}"
            );
        }
    }

    #[test]
    fn excerpts_cut_on_whole_lines() {
        let text = lines(200);
        let head = &text[..head_end(&text, 1000)];
        assert!(head.ends_with('\n'), "头停在行尾：{head:?}");
        assert!(head.starts_with("line 0000: "), "{head:?}");
        let tail = &text[tail_start(&text, 1000)..];
        assert!(tail.starts_with("line "), "尾从行首开始：{tail:?}");
        assert!(tail.ends_with('\n'), "{tail:?}");

        // decay 仍保留首尾。
        let cut = excerpt(&text, 700, 400);
        assert_eq!(cut.head.lines().count(), 13);
        assert_eq!(cut.tail.lines().count(), 7);

        // 预览停在整行上，裁剪说明单独占一行，不和预览挤在一起。
        let printed = project(&facts(Some(&text), None), &budget(1000));
        assert!(printed.contains("x\n…（正文 "), "{printed}");
    }

    #[test]
    fn a_single_giant_line_falls_back_to_char_boundaries() {
        // 第一行比头的预算还长：退到字符边界，头不能是空的。
        let first = format!("{}\nshort\n", "汉".repeat(2000));
        let cut = excerpt(&first, 100, 100);
        assert!(!cut.head.is_empty(), "有正文就不给空摘录");
        assert!(cut.head.len() > 90 && cut.head.len() <= 100 && cut.head.starts_with('汉'));
        // 尾那一行只占预算的零头：整行留不到一半，同样退到字符边界，把预算用上。
        assert!(
            cut.tail.len() > 90 && cut.tail.len() <= 100,
            "{:?}",
            cut.tail
        );
        assert!(cut.tail.ends_with("汉\nshort\n"), "{:?}", cut.tail);

        // 最后一行比尾的预算还长：同样退到字符边界。
        let last = format!("short\n{}\n", "汉".repeat(2000));
        let cut = excerpt(&last, 100, 100);
        assert!(cut.head.starts_with("short\n汉") && cut.head.len() > 90);
        assert!(
            cut.tail.len() > 90 && cut.tail.len() <= 100,
            "{:?}",
            cut.tail
        );
        assert!(cut.tail.ends_with("汉\n"));

        // 一行短抬头后面跟着一整行巨大的 JSON（`read` 一个压缩过的文件就是这样）：整行
        // 只留得下那行抬头，连预算的一半都不到——退到字符边界，JSON 的开头要看得见。
        let json = format!("共 1 行\n{{\"v\":1,\"body\":\"{}\"}}\n", "x".repeat(4000));
        let cut = excerpt(&json, 200, 200);
        assert!(cut.head.starts_with("共 1 行\n{\"v\":1,"), "{:?}", cut.head);
        assert!(cut.head.len() > 190 && cut.head.len() <= 200);
        assert!(cut.tail.len() > 190 && cut.tail.ends_with("\"}\n"));
    }

    #[test]
    fn the_decayed_view_always_names_the_recall_uri() {
        // full 视图放得下全部正文，所以它不印引用；decayed 视图省了中间，引用必须在。
        let text = lines(400);
        let views = project_views(&facts(Some(&text), None), &decaying(64 * 1024));
        assert!(!views.full.contains("完整输出："), "{}", views.full);
        let decayed = views.decayed.expect("20 KB 的结果要有 decayed 视图");
        assert!(
            decayed.starts_with("[bash · 完成 · 1.2s]\n（这份结果前文已完整给过，"),
            "{decayed}"
        );
        assert!(
            decayed.contains(&format!("原文 {} 字节 / 400 行", text.len())),
            "{decayed}"
        );
        assert!(decayed.contains("line 0000: "), "{decayed}");
        assert!(decayed.contains("line 0399: "), "{decayed}");
        assert!(decayed.contains("…（中间省略 "), "{decayed}");
        assert!(
            decayed.ends_with("完整输出：artifact://run-1/call-7/attempt-1/result\n"),
            "{decayed}"
        );
    }

    #[test]
    fn an_error_result_never_decays() {
        let text = lines(400);
        for status in [ToolResultStatus::Failed, ToolResultStatus::Uncertain] {
            let failed = ToolResultFacts {
                status,
                ..facts(Some(&text), None)
            };
            assert_eq!(project_views(&failed, &decaying(64 * 1024)).decayed, None);
        }
    }

    #[test]
    fn a_small_result_never_decays() {
        let small = lines(60);
        assert!(small.len() < POLICY.threshold_bytes);
        let views = project_views(&facts(Some(&small), None), &decaying(64 * 1024));
        assert_eq!(views.decayed, None);

        // 衰减关着（阈值 0）时再大也没有。
        let big = lines(400);
        assert_eq!(
            project_views(&facts(Some(&big), None), &budget(64 * 1024)).decayed,
            None
        );
    }

    #[test]
    fn the_decayed_view_is_strictly_shorter() {
        let views = project_views(&facts(Some(&lines(400)), None), &decaying(64 * 1024));
        let decayed = views.decayed.expect("要有 decayed 视图");
        assert!(decayed.len() + MIN_DECAY_SAVING_BYTES <= views.full.len());

        // 刚过阈值、头尾几乎就是全部：省不到 1 KiB，不换。
        let barely = lines(90);
        let views = project_views(&facts(Some(&barely), None), &decaying(64 * 1024));
        assert!(views.full.len() > POLICY.threshold_bytes);
        assert_eq!(views.decayed, None, "{}", views.full.len());
    }

    #[test]
    fn a_result_is_eligible_only_after_its_full_sends() {
        let ctx = decaying(64 * 1024);
        let views = project_views(&facts(Some(&lines(400)), None), &ctx);
        assert!(!decay_eligible(&views, 0, &ctx));
        assert!(!decay_eligible(&views, 1, &ctx));
        assert!(decay_eligible(&views, 2, &ctx));

        // 没有 decayed 视图就永远没资格。
        let small = project_views(&facts(Some(&lines(60)), None), &ctx);
        assert!(!decay_eligible(&small, 5, &ctx));
    }

    #[test]
    fn select_follows_the_recorded_decision() {
        let ctx = decaying(64 * 1024);
        let views = project_views(&facts(Some(&lines(400)), None), &ctx);
        let decayed = views.decayed.clone().expect("要有 decayed 视图");
        assert_eq!(select(views.clone(), false), views.full);
        assert_eq!(select(views.clone(), true), decayed);

        let small = project_views(&facts(Some(&lines(60)), None), &ctx);
        assert_eq!(select(small.clone(), true), small.full);
    }
}
