//! 飞书消息与卡片的渲染（§11.3）。
//!
//! 渠道之间的差别只在渲染，不在决策：这里把一个 [`Outbound`] 变成若干条待发送的飞书
//! 消息（`msg_type` + `content` 字符串），**不发送**、不认识 HTTP，也不知道 Session
//! 是什么。
//!
//! 卡片只剩 Run 的那一张回复卡片：收到消息时回一张"处理中"，终态时 PATCH 成结果（§11.3）。
//! 审批卡片与它的按钮随审批一起删掉了（§7，2026-10-11）；介入是一条文本。
//!
//! 卡片用 **JSON 2.0**（`"schema": "2.0"`）。`config.update_multi` 两个版本都有，且
//! PATCH 要求更新前后都为 `true`（spike callbacks.md 3d）。

use serde_json::{Value, json};

use komo_kernel::types::chat::Outbound;

/// 一条文本消息的分段上限，按 `char` 计。
///
/// 飞书给的硬上限是 `content` 序列化后 150 KB，没有给字符数；这里取一个远低于它、又
/// 不至于把一段正常回答切碎的值。
// TODO(decide: 官方只给了 150KB 的 content 上限，没有给"一条消息多少字合适"。3000 是
// 保守取值，真机看过排版后再定。)
pub const MESSAGE_LIMIT: usize = 3000;

/// 结果卡片里最多放多少字符的正文，其余发成后续文本消息。
///
/// 飞书对卡片消息的请求体上限是 30 KB，比文本的 150 KB 紧得多；而 PATCH 时卡片 JSON
/// 还要作为字符串再转义一层。3000 个汉字约 9 KB，转义膨胀一倍也只占上限的一半多，
/// 留给标题和那行备注。与 [`MESSAGE_LIMIT`] 同值只是巧合，两者的约束不同。
pub const RESULT_CARD_LIMIT: usize = 3000;

/// 一条渲染好的飞书消息。
///
/// `content` 已经是**字符串**：飞书的 `im/v1/messages` 收的 `content` 是一个 JSON
/// 字符串而不是 JSON 对象，序列化的时机定在渲染这一侧，发送方只管把它放进 body。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMessage {
    /// `text` / `interactive`。
    pub msg_type: &'static str,
    pub content: String,
}

impl RenderedMessage {
    /// 一条纯文本消息。
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            msg_type: "text",
            content: json!({ "text": text.into() }).to_string(),
        }
    }

    /// 一条交互卡片消息。
    pub fn card(card: &Value) -> Self {
        Self {
            msg_type: "interactive",
            content: card.to_string(),
        }
    }

    pub fn is_card(&self) -> bool {
        self.msg_type == "interactive"
    }
}

/// 一个 [`Outbound`] 渲染成的消息序列。超过 [`MESSAGE_LIMIT`] 的正文在这里就已经切好段。
pub fn render(outbound: &Outbound) -> Vec<RenderedMessage> {
    match outbound {
        Outbound::Text { text } => text_segments(text),
        Outbound::RunFinished { summary, .. } => text_segments(summary),
        Outbound::NeedsAttention { reason, .. } => {
            text_segments(&format!("⚠️ 需要你判断\n\n{reason}"))
        }
    }
}

/// 一段正文切成若干条文本消息。空正文也要有一条——否则"回执"会变成什么都没发生。
pub fn text_segments(text: &str) -> Vec<RenderedMessage> {
    let segments = split_message(text, MESSAGE_LIMIT);
    if segments.is_empty() {
        return vec![RenderedMessage::text("（空）")];
    }
    segments.into_iter().map(RenderedMessage::text).collect()
}

// ---------------------------------------------------------------- Run 的回复卡片

/// 收到一条会起 Run 的消息时，回复原消息的那张"处理中"卡片。
pub fn run_pending_card() -> Value {
    card(
        "处理中…".to_string(),
        "blue",
        json!([note("完成后结果会更新在这张卡片上")]),
    )
}

/// Run 终态时 PATCH 上去的结果卡片，以及卡片放不下、要接着发的文本消息。
///
/// `Outbound::RunFinished` 只带正文不带状态，所以标题只说"已结束"——失败、取消的话
/// 已经写在正文里了。
pub fn run_result_card(summary: &str) -> (Value, Vec<RenderedMessage>) {
    // 先去掉开头的空白：否则第一段可能整段是空白、被 `split_message` 跳过，下面按前缀
    // 切就切错了地方。
    let summary = summary.trim_start();
    let head = split_message(summary, RESULT_CARD_LIMIT)
        .into_iter()
        .next()
        .unwrap_or_default();
    // `split_message` 只修剪段尾，所以第一段是原文的前缀。
    let rest = summary[head.len()..].trim_start();
    let mut elements = vec![markdown(if head.is_empty() {
        "（空）".to_string()
    } else {
        head
    })];
    let follow = if rest.is_empty() {
        Vec::new()
    } else {
        elements.push(note("正文较长，其余见下方消息"));
        text_segments(rest)
    };
    (
        card("已结束".to_string(), "grey", Value::Array(elements)),
        follow,
    )
}

/// 卡片晚于终态才发出去、结果已经另发成文本时，把那张卡收尾——不能让它一直停在
/// "处理中"。
pub fn run_moved_card() -> Value {
    card(
        "已结束".to_string(),
        "grey",
        json!([note("结果见下方消息")]),
    )
}

/// 卡片骨架。
///
/// `config.update_multi` 必须为 `true`：PATCH 要求**更新前后**两张卡都带着它
/// （spike callbacks.md 3d）。它写在这一个函数里，所以两张卡不可能只有一张带。
fn card(title: String, template: &str, elements: Value) -> Value {
    json!({
        "schema": "2.0",
        "config": { "update_multi": true, "width_mode": "fill" },
        "header": {
            "title": { "tag": "plain_text", "content": title },
            "template": template,
        },
        "body": { "elements": elements },
    })
}

fn markdown(content: impl Into<String>) -> Value {
    json!({ "tag": "markdown", "content": content.into() })
}

/// 一行小字（"完成后结果会更新在这张卡片上"之类）。
///
/// **卡片 2.0 里没有 `note` 组件。** 飞书的 2.0 不兼容变更写得很直接：「2.0 结构不再
/// 支持 note 组件与 action 模块（`tag` 为 `action`）」，并给出替代写法——普通文本组件
/// 加 `notation` 字号与灰色。带 `note` 的卡片被平台整张打回（`230099 / 200861
/// unsupported tag note`），而 2.0 对不认识的属性是**报错**而不是忽略。
fn note(content: impl Into<String>) -> Value {
    json!({
        "tag": "div",
        "text": {
            "tag": "plain_text",
            "content": content.into(),
            "text_size": "notation",
            "text_color": "grey",
        },
    })
}

/// 按 `limit` 个 `char` 切段，尽量切在换行处。
///
/// 飞书数的是 `content` 的**字节**（150 KB），不是码元也不是字符；这里按 `char` 切是
/// 因为 `MESSAGE_LIMIT` 本来就是一个远低于硬上限的排版取值，用哪种单位都不会撞线。
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    assert!(limit > 0, "分段上限必须为正");
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.chars().count() <= limit {
            let trimmed = rest.trim_end();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            break;
        }
        // 限额内能放下的最大字节边界。
        let end = rest
            .char_indices()
            .nth(limit)
            .map(|(index, _)| index)
            .unwrap_or(rest.len());
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

    use komo_kernel::types::ids::{RunId, SessionId};

    fn body(card: &Value) -> &Vec<Value> {
        card.pointer("/body/elements")
            .and_then(Value::as_array)
            .expect("卡片有 body.elements")
    }

    /// 整张卡片序列化之后的文本，断言"里面有没有这几个字"用。
    fn flat(card: &Value) -> String {
        card.to_string()
    }

    /// **2.0 里没有 `note` 与 `action`**：带着它们的卡片被平台整张打回。按 `tag` 递归找，
    /// 不只看第一层：以后它们长到哪一层都得拦住。
    #[test]
    fn a_card_carries_no_component_the_2_0_schema_dropped() {
        for card in [run_pending_card(), run_moved_card(), run_result_card("x").0] {
            let mut pending = vec![card.clone()];
            while let Some(node) = pending.pop() {
                match node {
                    Value::Object(map) => {
                        let tag = map.get("tag").and_then(Value::as_str);
                        assert_ne!(tag, Some("note"), "2.0 没有 note：{}", flat(&card));
                        assert_ne!(tag, Some("action"), "2.0 没有 action：{}", flat(&card));
                        pending.extend(map.values().cloned());
                    }
                    Value::Array(items) => pending.extend(items),
                    _ => {}
                }
            }
        }
    }

    // ⑧ 超长分段。
    #[test]
    fn a_long_body_is_split_at_the_platform_limit() {
        let text = "x".repeat(MESSAGE_LIMIT * 2 + 7);
        let messages = render(&Outbound::Text { text: text.clone() });
        assert_eq!(messages.len(), 3);
        let mut rejoined = String::new();
        for message in &messages {
            assert_eq!(message.msg_type, "text");
            let content: Value = serde_json::from_str(&message.content).unwrap();
            let part = content["text"].as_str().unwrap();
            assert!(part.chars().count() <= MESSAGE_LIMIT);
            rejoined.push_str(part);
        }
        assert_eq!(rejoined, text, "分段不能丢字符");
    }

    #[test]
    fn splitting_prefers_newlines_and_never_loses_characters() {
        let text = format!("{}\n{}", "a".repeat(60), "b".repeat(60));
        assert_eq!(
            split_message(&text, 100),
            vec!["a".repeat(60), "b".repeat(60)]
        );
        // 汉字按 char 计。
        assert_eq!(split_message("你好世界", 2), vec!["你好", "世界"]);
    }

    #[test]
    fn an_empty_body_still_produces_one_message() {
        let messages = render(&Outbound::Text { text: "   ".into() });
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("（空）"));
    }

    #[test]
    fn a_run_finished_renders_its_summary() {
        let messages = render(&Outbound::RunFinished {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            summary: "跑完了".into(),
        });
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, json!({ "text": "跑完了" }).to_string());
    }

    #[test]
    fn a_short_result_fits_in_the_card_alone() {
        let (card, follow) = run_result_card("**跑完了**");
        assert_eq!(card["config"]["update_multi"], json!(true));
        assert_eq!(body(&card).len(), 1);
        assert_eq!(body(&card)[0]["tag"], json!("markdown"));
        assert_eq!(body(&card)[0]["content"], json!("**跑完了**"));
        assert!(follow.is_empty());
    }

    #[test]
    fn a_long_result_puts_the_head_in_the_card_and_the_rest_in_messages() {
        let summary = format!("{}{}", "a".repeat(RESULT_CARD_LIMIT), "b".repeat(10));
        let (card, follow) = run_result_card(&summary);
        let head = body(&card)[0]["content"].as_str().unwrap();
        assert_eq!(head, "a".repeat(RESULT_CARD_LIMIT));
        assert!(flat(&card).contains("其余见下方消息"), "{}", flat(&card));
        assert_eq!(follow.len(), 1);
        assert_eq!(
            follow[0].content,
            json!({ "text": "b".repeat(10) }).to_string()
        );
    }

    #[test]
    fn run_cards_keep_update_multi_and_avoid_dropped_tags() {
        for card in [run_pending_card(), run_moved_card(), run_result_card("x").0] {
            assert_eq!(card["config"]["update_multi"], json!(true));
            assert_eq!(card["schema"], json!("2.0"));
            assert!(!flat(&card).contains(r#""tag":"note""#), "{}", flat(&card));
        }
    }

    #[test]
    fn needs_attention_says_so_up_front() {
        let messages = render(&Outbound::NeedsAttention {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            reason: "工具结果不确定".into(),
        });
        assert!(messages[0].content.contains("需要你判断"));
    }
}
