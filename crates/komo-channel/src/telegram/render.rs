//! Telegram 消息渲染（§11.3）。
//!
//! 渠道之间的差别只在渲染，不在决策：这里把一个 [`Outbound`] 变成若干条待发送的
//! Telegram 消息，**不发送**、不认识 HTTP，也不知道 Session 是什么。
//!
//! 定在这里的是 4096 这个平台上限：按 **UTF-16 码元**计（Telegram 数的是码元，不是
//! 字节也不是 `char`），一个 emoji 占 2、一个汉字占 1。审批请求的内联按钮与它的回调
//! 随审批一起删掉了（§7，2026-10-11）。

use komo_kernel::types::chat::Outbound;

/// 一条消息的上限，按 UTF-16 码元计。
pub const MESSAGE_LIMIT: usize = 4096;

/// Telegram 的两种解析模式里我们只用一种；`None` = 纯文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMode {
    MarkdownV2,
}

impl ParseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ParseMode::MarkdownV2 => "MarkdownV2",
        }
    }
}

/// 一条渲染好的消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMessage {
    pub text: String,
    pub parse_mode: Option<ParseMode>,
}

impl RenderedMessage {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            parse_mode: None,
        }
    }

    pub fn markdown(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            parse_mode: Some(ParseMode::MarkdownV2),
        }
    }

    /// 同一条消息的纯文本版本。MarkdownV2 被服务端拒绝时原样重发这一条（§13.2）。
    ///
    /// 只去掉 `parse_mode`，**不**去掉转义反斜杠：转义只在 MarkdownV2 里有意义，但
    /// 把它们从已经拼好的正文里摘出来需要重新解析一遍我们自己的输出，得不偿失，而
    /// 多出来的反斜杠不会让人读不懂一条已经失败过一次的消息。
    pub fn without_parse_mode(&self) -> Self {
        Self {
            text: self.text.clone(),
            parse_mode: None,
        }
    }
}

/// 一个 [`Outbound`] 渲染成的消息序列。超过 4096 的正文在这里就已经切好段。
pub fn render(outbound: &Outbound) -> Vec<RenderedMessage> {
    match outbound {
        Outbound::Text { text } => markdown_segments(text),
        Outbound::RunFinished { summary, .. } => markdown_segments(summary),
        Outbound::NeedsAttention { reason, .. } => {
            markdown_segments(&format!("⚠️ 需要你判断\n\n{reason}"))
        }
        // 没有回复卡片可收尾：什么都不发。
        Outbound::RunAbsorbed { .. } => Vec::new(),
    }
}

// ---------------------------------------------------------------- MarkdownV2

/// MarkdownV2 在普通文本里要求转义的全部字符（Bot API 文档逐字列出的那一串）。
const MARKDOWN_V2_RESERVED: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!', '\\',
];

/// 普通文本 → MarkdownV2 字面量。
pub fn escape_markdown_v2(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if MARKDOWN_V2_RESERVED.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------- 分段

/// 把正文切成 ≤ [`MESSAGE_LIMIT`] 的若干段。
///
/// 正文按 **MarkdownV2 原样**送出（`Outbound::Text` / `RunFinished` 的内容本来就是
/// 模型写的 markdown）。服务端拒绝时由发送方去掉 `parse_mode` 重发一次（§13.2），
/// 这里不预先判断哪段能过——判断得准就等于自己实现了一遍 Telegram 的解析器。
// TODO(decide: 要不要在这里做一遍 markdown → MarkdownV2 的方言转换。不转 = 多数模型
// 输出会走一次 400 再纯文本重发（多一个来回，但永远不丢消息）；转 = 少一个来回，但
// 转换器本身会成为一个要维护的解析器。文档只写了"失败回退纯文本重发"。)
fn markdown_segments(text: &str) -> Vec<RenderedMessage> {
    let mut messages: Vec<RenderedMessage> = split_message(text, MESSAGE_LIMIT)
        .into_iter()
        .map(RenderedMessage::markdown)
        .collect();
    if messages.is_empty() {
        messages.push(RenderedMessage::markdown(escape_markdown_v2("（空）")));
    }
    messages
}

/// UTF-16 码元数——Telegram 数的就是它。
pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// 按 [`MESSAGE_LIMIT`] 切段，尽量切在换行处。
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    assert!(limit > 0, "分段上限必须为正");
    if utf16_len(text) <= limit {
        let trimmed = text.trim_end();
        return if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![trimmed.to_string()]
        };
    }

    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if utf16_len(rest) <= limit {
            let trimmed = rest.trim_end();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            break;
        }
        // 限额内能放下的最大字节边界。
        let mut end = 0;
        let mut width = 0;
        for (index, ch) in rest.char_indices() {
            let next = width + ch.len_utf16();
            if next > limit {
                break;
            }
            width = next;
            end = index + ch.len_utf8();
        }
        // 宁可切在换行上——但不为此丢掉半段的容量。
        let cut = rest[..end]
            .rfind('\n')
            .map(|index| index + 1)
            .filter(|index| *index * 2 > end)
            .unwrap_or(end);
        let segment = rest[..cut].trim_end();
        if !segment.is_empty() {
            out.push(segment.to_string());
        }
        rest = &rest[cut..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use komo_kernel::types::chat::ChannelPlatform;
    use komo_kernel::types::ids::{RunId, SessionId};

    // ⑧ 4096 分段。
    #[test]
    fn a_long_body_is_split_at_the_platform_limit() {
        let body = "x".repeat(MESSAGE_LIMIT * 2 + 7);
        let messages = render(&Outbound::Text { text: body.clone() });
        assert_eq!(messages.len(), 3);
        for message in &messages {
            assert!(utf16_len(&message.text) <= MESSAGE_LIMIT);
        }
        let rejoined: String = messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(rejoined, body, "分段不能丢字符");
    }

    #[test]
    fn splitting_counts_utf16_units_and_prefers_newlines() {
        // 汉字是一个码元，emoji 是两个——按 `char` 数会算错。
        assert_eq!(utf16_len("你好"), 2);
        assert_eq!(utf16_len("🙂"), 2);
        let segments = split_message("🙂🙂🙂", 2);
        assert_eq!(segments, vec!["🙂", "🙂", "🙂"]);

        let text = format!("{}\n{}", "a".repeat(60), "b".repeat(60));
        let segments = split_message(&text, 100);
        assert_eq!(segments, vec!["a".repeat(60), "b".repeat(60)]);
    }

    #[test]
    fn markdown_v2_escapes_every_reserved_character() {
        assert_eq!(escape_markdown_v2("a.b-c!"), "a\\.b\\-c\\!");
        assert_eq!(escape_markdown_v2("a\\b"), "a\\\\b");
    }

    #[test]
    fn a_run_finished_renders_its_summary() {
        let messages = render(&Outbound::RunFinished {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            summary: "跑完了".into(),
        });
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "跑完了");
        assert_eq!(messages[0].parse_mode, Some(ParseMode::MarkdownV2));
    }

    #[test]
    fn dropping_the_parse_mode_keeps_the_text() {
        let messages = render(&Outbound::Text {
            text: "记得 *不要* 忘了".into(),
        });
        let plain = messages[0].without_parse_mode();
        assert_eq!(plain.parse_mode, None);
        assert_eq!(plain.text, messages[0].text);
    }

    #[test]
    fn needs_attention_says_so_up_front() {
        let messages = render(&Outbound::NeedsAttention {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            reason: "工具结果不确定".into(),
        });
        assert!(messages[0].text.contains("需要你判断"));
    }

    #[test]
    fn a_peer_is_only_ever_rendered_as_platform_colon_chat() {
        // 渲染层不认识 Session，只认识 peer 的字面写法。
        assert_eq!(
            komo_kernel::types::chat::ChannelPeer::new(ChannelPlatform::Telegram, "42").to_string(),
            "telegram:42"
        );
    }
}
