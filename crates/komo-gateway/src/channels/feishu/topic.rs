//! 飞书话题上下文（§11.3）：被 @ 的那一刻，把话题里 komo 没收到的消息读进来。
//!
//! 为什么：话题里没 @机器人 的消息按设计不响应、也不读——同事先 @ 另一个机器人评审了
//! 三个 MR，随后有人在同一话题里对 komo 说"看看 owl 的两条 Blocking，修复一下"，komo 只
//! 收到这一句，满世界搜 Blocking 最后答"没找到"（2026-10-23）。
//!
//! 做法照 OpenClaw 群聊的"自你上次回复以来的消息"，**无状态**：每次被 @ 时用 bot 身份
//! 读一遍这个话题（[`FeishuApi::list_thread_messages`]，最新的在前），取
//!
//! - 早于当前这条、晚于**本机器人在话题里的最后一条消息**的消息（没有就从话题根开始，含
//!   根消息）；
//! - 排除机器人自己发的、@ 了机器人的（它们本身就作为输入进过会话）、已撤回的、系统
//!   消息与当前这条。
//!
//! 渲染成纯文本，每条以 `发送者 HH:MM：` 开头；总长有上限，超了留最近的。**任何失败都只
//! 记 WARN 并退回"没有上下文"**——读不到话题不该挡住用户这句话。
//!
//! 发送者名字先从话题里的 `mentions` 找，找不到的再去开放平台查（[`SenderNames`]），
//! 用的是同一个超时里读完话题剩下的时间；查不到或到点就写 id，不影响其余部分。

use std::collections::HashMap;
use std::time::Duration;

use jiff::tz::TimeZone;
use serde_json::{Map, Value};
use tokio::time::Instant;

use super::api::{FeishuApi, FeishuError, ThreadMessage};
use super::names::SenderNames;

/// 最多读多少条（含被排除的）。话题再长，被 @ 时要的是最近那一段。
pub const MAX_MESSAGES: usize = 200;

/// 渲染后总字符数上限。超了保留最近的，更早的整条省略。
pub const MAX_CHARS: usize = 16_000;

/// 整个读取（含分页与查名字）的超时。
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// 机器人自己是谁：话题列表里它发的消息 `sender.id` 是 app_id，@ 它的 `mentions[].id`
/// 是 open_id。
#[derive(Debug, Clone, Copy)]
pub struct Me<'a> {
    pub app_id: &'a str,
    pub open_id: &'a str,
}

/// 当前这条（触发本次读取的）消息。
#[derive(Debug, Clone, Copy)]
pub struct Current<'a> {
    pub message_id: &'a str,
    /// 事件里的 `create_time`（毫秒）。缺了就从话题列表里找它自己的那一条。
    pub created_ms: Option<i64>,
}

/// 读话题、选消息、查名字、渲染。`None` = 没有要给的上下文（含读取失败，失败只记 WARN）。
pub async fn context(
    api: &FeishuApi,
    names: &SenderNames,
    thread_id: &str,
    current: Current<'_>,
    me: Me<'_>,
    tz: &TimeZone,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let collected =
        match tokio::time::timeout_at(deadline, collect(api, thread_id, current, me)).await {
            Ok(Ok(collected)) => collected,
            Ok(Err(error)) => {
                tracing::warn!(%error, thread_id, "飞书：话题消息没读到，只交原文");
                return None;
            }
            Err(_) => {
                tracing::warn!(thread_id, ?timeout, "飞书：读话题超时，只交原文");
                return None;
            }
        };

    let cutoff = current.created_ms.or_else(|| {
        collected
            .messages
            .iter()
            .find(|m| m.message_id == current.message_id)
            .and_then(ThreadMessage::created_ms)
    });
    let chosen = select(&collected.messages, current.message_id, cutoff, me);
    let mut known = mention_names(&collected.messages);
    let unnamed = chosen
        .iter()
        .map(|m| &m.sender)
        .filter(|sender| !known.contains_key(&sender.id));
    let looked_up = names.resolve(api, unnamed, deadline).await;
    known.extend(looked_up);
    render(&chosen, &known, tz, MAX_CHARS, !collected.complete)
}

/// 读到的话题消息，按时间正序。
#[derive(Debug)]
struct Collected {
    messages: Vec<ThreadMessage>,
    /// 读全了：看到了机器人更早的一条，或者话题已经到头。`false` = 撞上
    /// [`MAX_MESSAGES`]，更早的没读。
    complete: bool,
}

/// 从最新往旧分页读，读到机器人在当前这条之前的最后一条（再往前的用不上）、话题到头或
/// 满 [`MAX_MESSAGES`] 为止。
async fn collect(
    api: &FeishuApi,
    thread_id: &str,
    current: Current<'_>,
    me: Me<'_>,
) -> Result<Collected, FeishuError> {
    let mut newest_first: Vec<ThreadMessage> = Vec::new();
    let mut cutoff = current.created_ms;
    let mut token: Option<String> = None;
    let complete = loop {
        let page = api
            .list_thread_messages(thread_id, token.as_deref())
            .await?;
        let mut reached = false;
        for message in page.items {
            if cutoff.is_none() && message.message_id == current.message_id {
                cutoff = message.created_ms();
            }
            reached = cutoff.is_some() && is_mine(&message, me) && precedes(&message, cutoff);
            newest_first.push(message);
            if reached {
                break;
            }
        }
        if reached || !page.has_more {
            break true;
        }
        match page.page_token.filter(|token| !token.is_empty()) {
            None => break true,
            Some(next) if newest_first.len() < MAX_MESSAGES => token = Some(next),
            Some(_) => break false,
        }
    };
    newest_first.reverse();
    Ok(Collected {
        messages: newest_first,
        complete,
    })
}

fn is_mine(message: &ThreadMessage, me: Me<'_>) -> bool {
    let id = message.sender.id.as_str();
    !id.is_empty() && (id == me.app_id || id == me.open_id)
}

fn mentions_me(message: &ThreadMessage, me: Me<'_>) -> bool {
    !me.open_id.is_empty() && message.mentions.iter().any(|m| m.id == me.open_id)
}

/// 早于 `cutoff`。`cutoff` 不明时没法比，当作都早于（见 [`select`]）。
fn precedes(message: &ThreadMessage, cutoff: Option<i64>) -> bool {
    match (cutoff, message.created_ms()) {
        (Some(cutoff), Some(created)) => created < cutoff,
        (None, _) => true,
        // 没有时间戳的消息没法排顺序，宁可当作更早也别丢。
        (Some(_), None) => true,
    }
}

/// 选出要给模型看的：`all` 按时间正序。
fn select<'a>(
    all: &'a [ThreadMessage],
    current_id: &str,
    cutoff: Option<i64>,
    me: Me<'_>,
) -> Vec<&'a ThreadMessage> {
    let before: Vec<&ThreadMessage> = all
        .iter()
        .filter(|m| m.message_id != current_id && precedes(m, cutoff))
        .collect();
    let start = before
        .iter()
        .rposition(|m| is_mine(m, me))
        .map_or(0, |last| last + 1);
    before[start..]
        .iter()
        .copied()
        .filter(|m| !m.deleted && m.msg_type != "system" && !is_mine(m, me) && !mentions_me(m, me))
        .collect()
}

/// 话题里被 @ 过的人与机器人：`mentions` 带着名字，话题列表里发送者自己没有。
fn mention_names(all: &[ThreadMessage]) -> HashMap<String, String> {
    all.iter()
        .flat_map(|m| &m.mentions)
        .filter(|m| !m.id.is_empty() && !m.name.is_empty())
        .map(|m| (m.id.clone(), m.name.clone()))
        .collect()
}

/// 渲染。`names` 是 id → 名字，没有的写 id。`incomplete` = 更早的消息没读。
fn render(
    chosen: &[&ThreadMessage],
    names: &HashMap<String, String>,
    tz: &TimeZone,
    max_chars: usize,
    incomplete: bool,
) -> Option<String> {
    if chosen.is_empty() {
        return None;
    }
    let lines: Vec<String> = chosen
        .iter()
        .map(|m| {
            let who = names.get(&m.sender.id).unwrap_or(&m.sender.id);
            format!("{who} {}：{}", clock(m.created_ms(), tz), message_text(m))
        })
        .collect();

    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;
    for line in lines.iter().rev() {
        let size = line.chars().count() + 1;
        if used + size > max_chars {
            if kept.is_empty() {
                // 最近的一条自己就超了：留头部，总比一条都没有强。
                let head: String = line.chars().take(max_chars).collect();
                kept.push(format!("{head}…（后略）"));
            }
            break;
        }
        used += size;
        kept.push(line.clone());
    }
    let omitted = lines.len() - kept.len();
    kept.reverse();

    let mut out = String::new();
    if omitted > 0 {
        out.push_str(&format!("（更早的 {omitted} 条省略）\n"));
    }
    if incomplete {
        out.push_str(&format!(
            "（话题更早的消息没有读取，只读了最近 {MAX_MESSAGES} 条）\n"
        ));
    }
    out.push_str(&kept.join("\n"));
    Some(out)
}

/// `HH:MM`，按本机时区。
fn clock(created_ms: Option<i64>, tz: &TimeZone) -> String {
    created_ms
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map(|ts| ts.to_zoned(tz.clone()).strftime("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".into())
}

// ---------------------------------------------------------------- 正文

/// 一条消息的正文，纯文本。认不出或抽不出字的类型写成 `[类型]`。
fn message_text(message: &ThreadMessage) -> String {
    let raw = message.body.content.as_str();
    let content: Value =
        serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
    let placeholder = |label: &str| format!("[{label}]");
    let text = match message.msg_type.as_str() {
        "text" => fill_mentions(
            content
                .get("text")
                .and_then(Value::as_str)
                .or(content.as_str())
                .unwrap_or_default(),
            message,
        ),
        "post" => post_text(&content, message),
        "interactive" => card_text(&content),
        "image" => placeholder("图片"),
        "file" => match str_at(&content, "file_name") {
            "" => placeholder("文件"),
            name => format!("[文件 {name}]"),
        },
        "audio" => placeholder("语音"),
        "media" => placeholder("视频"),
        "sticker" => placeholder("表情包"),
        "share_chat" => placeholder("群名片"),
        "share_user" => placeholder("个人名片"),
        "merge_forward" => placeholder("合并转发"),
        other => placeholder(other),
    };
    let text = text.trim();
    if text.is_empty() {
        placeholder(match message.msg_type.as_str() {
            "interactive" => "卡片",
            other => other,
        })
    } else {
        text.to_string()
    }
}

/// `@_user_1` → `@名字`。键长的先换，免得 `@_user_1` 吃掉 `@_user_10` 的前缀。
fn fill_mentions(text: &str, message: &ThreadMessage) -> String {
    let mut mentions: Vec<_> = message
        .mentions
        .iter()
        .filter(|m| !m.key.is_empty() && !m.name.is_empty())
        .collect();
    mentions.sort_by_key(|m| std::cmp::Reverse(m.key.len()));
    let mut out = text.to_string();
    for mention in mentions {
        out = out.replace(&mention.key, &format!("@{}", mention.name));
    }
    out
}

/// 富文本：标题与每段文字；@ 换成名字，图片写占位。
fn post_text(content: &Value, message: &ThreadMessage) -> String {
    let body = if content.get("content").is_some() {
        content
    } else {
        content
            .as_object()
            .and_then(|map| map.values().find(|v| v.get("content").is_some()))
            .unwrap_or(content)
    };
    let mut lines: Vec<String> = Vec::new();
    let title = str_at(body, "title").trim();
    if !title.is_empty() {
        lines.push(title.to_string());
    }
    for paragraph in body
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut line = String::new();
        for element in paragraph.as_array().into_iter().flatten() {
            match str_at(element, "tag") {
                "text" | "a" | "md" | "code_block" => line.push_str(str_at(element, "text")),
                "at" => {
                    let key = str_at(element, "user_id");
                    let name = match str_at(element, "user_name") {
                        "" => message
                            .mentions
                            .iter()
                            .find(|m| m.key == key)
                            .map_or("", |m| m.name.as_str()),
                        name => name,
                    };
                    line.push('@');
                    line.push_str(if name.is_empty() { key } else { name });
                }
                "img" => line.push_str("[图片]"),
                "media" => line.push_str("[视频]"),
                _ => {}
            }
        }
        if !line.trim().is_empty() {
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn str_at<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

// ---------------------------------------------------------------- 卡片

/// 别的机器人发的卡片，正文在原始卡片 JSON 里（`raw_card_content`）。
///
/// 卡片 2.0 的形状是 `{"json_card": "<又一层 JSON 字符串>", "json_attachment": …}`，里面
/// 是 `{tag, property: {content | elements | …}}` 的树；1.0 是 `{header, elements}`。两种
/// 都走同一个**宽容的递归**：凡是 `content` / `text` / `title` 之类的字符串字段都收，
/// `br`、标题、列表项、表格行、代码块行换行，图片写占位。宁可多收不要漏。
pub fn card_text(content: &Value) -> String {
    let mut out = Text::default();
    walk(content, &mut out);
    tidy(&out.buf)
}

#[derive(Default)]
struct Text {
    buf: String,
}

impl Text {
    fn push(&mut self, s: &str) {
        self.buf.push_str(s);
    }

    /// 另起一行；已经在行首就不动。
    fn line_break(&mut self) {
        if !self.buf.is_empty() && !self.buf.ends_with('\n') {
            self.buf.push('\n');
        }
    }
}

/// 行内的标签：不换行。其余带 `tag` 的元素按块处理，前后换行。
const INLINE_TAGS: &[&str] = &[
    "plain_text",
    "lark_md",
    "code_span",
    "text",
    "a",
    "at",
    "md",
    "emoji",
    "text_tag",
    "link",
    "number_tag",
];

/// 收字符串的字段。
const TEXT_KEYS: &[&str] = &["content", "text", "title", "subtitle"];

/// 遍历顺序：这几个键先走（对象的键是字母序，不排的话 `header` 会跑到正文后面）。
const KEY_ORDER: &[&str] = &[
    "header", "title", "subtitle", "text", "content", "elements", "body", "property",
];

fn walk(value: &Value, out: &mut Text) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
        Value::Object(map) => walk_object(map, out),
        _ => {}
    }
}

fn walk_object(map: &Map<String, Value>, out: &mut Text) {
    match map.get("json_card") {
        Some(Value::String(inner)) => {
            if let Ok(card) = serde_json::from_str::<Value>(inner) {
                walk(&card, out);
            }
            return;
        }
        Some(card @ Value::Object(_)) => {
            walk(card, out);
            return;
        }
        _ => {}
    }

    let tag = map.get("tag").and_then(Value::as_str).unwrap_or_default();
    match tag {
        "br" | "hr" => return out.line_break(),
        "img" | "image" => return out.push("[图片]"),
        "code_block" => return code_block(map, out),
        "table" => return table(map, out),
        "list" => return list(map, out),
        _ => {}
    }

    let block = !tag.is_empty() && !INLINE_TAGS.contains(&tag);
    if block {
        out.line_break();
    }
    if tag == "code_span" {
        out.push("`");
    }

    // 有 `elements` 就以它为准：同一个对象里再收一遍自己的字符串，会把同一段话说两遍。
    let has_elements = map
        .get("elements")
        .and_then(Value::as_array)
        .is_some_and(|elements| !elements.is_empty());
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort_by_key(|key| {
        (
            KEY_ORDER
                .iter()
                .position(|k| k == key)
                .unwrap_or(KEY_ORDER.len()),
            key.to_string(),
        )
    });
    for key in keys {
        match &map[key] {
            Value::String(s) if TEXT_KEYS.contains(&key.as_str()) && !has_elements => {
                out.push(s);
            }
            Value::Object(_) | Value::Array(_) => {
                if key == "json_attachment" || (key == "markdownElements" && has_elements) {
                    continue;
                }
                walk(&map[key], out);
            }
            _ => {}
        }
    }

    if tag == "code_span" {
        out.push("`");
    }
    if block {
        out.line_break();
    }
}

/// 卡片 2.0 的元素把参数放在 `property` 里，1.0 直接放在元素上。
fn props(map: &Map<String, Value>) -> &Map<String, Value> {
    map.get("property")
        .and_then(Value::as_object)
        .unwrap_or(map)
}

/// 代码块：`contents` 是行的数组，每行又是 `{contents: [{content}]}`。
fn code_block(map: &Map<String, Value>, out: &mut Text) {
    out.line_break();
    for line in props(map)
        .get("contents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut text = Text::default();
        collect_contents(line, &mut text);
        out.push(&text.buf);
        out.push("\n");
    }
}

fn collect_contents(value: &Value, out: &mut Text) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| collect_contents(item, out)),
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get("content") {
                out.push(s);
            }
            if let Some(inner) = map.get("contents") {
                collect_contents(inner, out);
            }
        }
        _ => {}
    }
}

/// 列表：每项一行，`- ` 或 `1. ` 打头，按 `level` 缩进。
fn list(map: &Map<String, Value>, out: &mut Text) {
    let items = props(map)
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for (index, item) in items.enumerate() {
        out.line_break();
        let level = item.get("level").and_then(Value::as_u64).unwrap_or(0) as usize;
        out.push(&"  ".repeat(level));
        if str_at(item, "type") == "ol" {
            out.push(&format!("{}. ", index + 1));
        } else {
            out.push("- ");
        }
        if let Some(elements) = item.get("elements") {
            walk(elements, out);
        }
        out.line_break();
    }
}

/// 表格：表头一行，之后每行一行，单元格用 ` | ` 隔开。
fn table(map: &Map<String, Value>, out: &mut Text) {
    let table = props(map);
    let columns: Vec<&Value> = table
        .get("columns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let names: Vec<&str> = columns.iter().map(|c| str_at(c, "name")).collect();
    let headers: Vec<&str> = columns
        .iter()
        .map(|c| {
            ["displayName", "display_name", "name"]
                .into_iter()
                .map(|key| str_at(c, key))
                .find(|s| !s.is_empty())
                .unwrap_or_default()
        })
        .collect();

    out.line_break();
    if headers.iter().any(|h| !h.is_empty()) {
        out.push(&headers.join(" | "));
        out.push("\n");
    }
    for row in table
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let cells: Vec<String> = match row {
            Value::Object(cells) if !names.is_empty() => names
                .iter()
                .map(|name| cells.get(*name).map(cell_text).unwrap_or_default())
                .collect(),
            Value::Object(cells) => cells.values().map(cell_text).collect(),
            other => vec![cell_text(other)],
        };
        out.push(&cells.join(" | "));
        out.push("\n");
    }
}

fn cell_text(cell: &Value) -> String {
    match cell {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Object(map) if map.contains_key("data") => cell_text(&map["data"]),
        other => {
            let mut text = Text::default();
            walk(other, &mut text);
            text.buf.split_whitespace().collect::<Vec<_>>().join(" ")
        }
    }
}

/// 每行去尾空白，连续空行压成一个，去掉首尾空白。
fn tidy(raw: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in raw.lines().map(str::trim_end) {
        if line.is_empty() && lines.last().is_none_or(|last| last.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    use super::super::fake::{Behavior, FakeOpenApi, thread_item};

    const ME: Me<'static> = Me {
        app_id: "cli_test",
        open_id: "ou_komo_bot",
    };

    /// 真实原始响应里截下来的一张卡片 2.0（Jeeson Bot 的评审结论，已换成无关内容）：
    /// `json_card` 是一层字符串，里面有标题、含 `code_span` 的段落、表格、列表与代码块。
    const REVIEW_CARD: &str = include_str!("testdata/review_card.json");

    fn at_utc(ms: i64) -> String {
        clock(Some(ms), &TimeZone::UTC)
    }

    fn text(id: &str, at_ms: i64, sender: &str, body: &str) -> ThreadMessage {
        thread_item(id, at_ms, sender, "text", &json!({ "text": body }), &[])
    }

    fn ids<'a>(chosen: &[&'a ThreadMessage]) -> Vec<&'a str> {
        chosen.iter().map(|m| m.message_id.as_str()).collect()
    }

    // ---- 选取规则

    #[test]
    fn a_first_mention_reads_from_the_thread_root() {
        let all = vec![
            text("om_root", 1000, "ou_a", "根消息"),
            text("om_b", 2000, "ou_b", "第二句"),
            text("om_now", 3000, "ou_a", "当前"),
        ];
        let chosen = select(&all, "om_now", Some(3000), ME);
        assert_eq!(
            ids(&chosen),
            ["om_root", "om_b"],
            "没有自己的消息就从根开始，含根"
        );
    }

    #[test]
    fn only_messages_after_the_bots_last_one_are_chosen() {
        let all = vec![
            text("om_old", 1000, "ou_a", "上一轮之前"),
            text("om_bot1", 2000, "cli_test", "第一次回答"),
            text("om_mid", 3000, "ou_b", "中间"),
            text("om_bot2", 4000, "cli_test", "第二次回答"),
            text("om_after1", 5000, "ou_a", "之后一"),
            text("om_after2", 6000, "ou_c", "之后二"),
            text("om_now", 7000, "ou_a", "当前"),
        ];
        let chosen = select(&all, "om_now", Some(7000), ME);
        assert_eq!(ids(&chosen), ["om_after1", "om_after2"]);
    }

    #[test]
    fn messages_that_mention_the_bot_or_were_sent_by_it_are_left_out() {
        let mention_bot = thread_item(
            "om_at",
            2000,
            "ou_a",
            "text",
            &json!({ "text": "@_user_1 看一下" }),
            &[("@_user_1", "ou_komo_bot", "komo")],
        );
        let mention_other = thread_item(
            "om_other",
            3000,
            "ou_a",
            "text",
            &json!({ "text": "@_user_1 帮忙" }),
            &[("@_user_1", "ou_other", "Jeeson Bot")],
        );
        let mut recalled = text("om_gone", 4000, "ou_b", "撤回的");
        recalled.deleted = true;
        let mut system = text("om_sys", 4500, "ou_b", "x");
        system.msg_type = "system".into();
        // 机器人的消息夹在后面：它之后才算"没读过"，所以它自己必须在 cutoff 之后才不影响。
        let all = vec![
            text("om_root", 1000, "ou_a", "根"),
            mention_bot,
            mention_other,
            recalled,
            system,
            text("om_now", 9000, "ou_a", "当前"),
            text("om_bot_later", 9500, "cli_test", "当前之后的回答"),
        ];
        let chosen = select(&all, "om_now", Some(9000), ME);
        assert_eq!(ids(&chosen), ["om_root", "om_other"]);
    }

    #[test]
    fn messages_after_the_current_one_never_count() {
        let all = vec![
            text("om_a", 1000, "ou_a", "之前"),
            text("om_now", 2000, "ou_a", "当前"),
            text("om_late", 2500, "ou_b", "当前之后才到的"),
            // 当前之后机器人的回复不能被当成"上次回复"，把前面的都切掉。
            text("om_bot_later", 2600, "cli_test", "处理中"),
        ];
        let chosen = select(&all, "om_now", Some(2000), ME);
        assert_eq!(ids(&chosen), ["om_a"]);
    }

    // ---- 渲染

    #[test]
    fn a_line_starts_with_the_sender_and_the_clock() {
        // 1792722782506 ms = 2026-10-23 02:33:02 UTC
        let all = vec![thread_item(
            "om_1",
            1_792_722_782_506,
            "ou_a",
            "text",
            &json!({ "text": "@_user_1 帮忙 CR 一下" }),
            &[("@_user_1", "ou_jeeson", "Jeeson Bot")],
        )];
        let chosen: Vec<&ThreadMessage> = all.iter().collect();
        let out = render(
            &chosen,
            &mention_names(&all),
            &TimeZone::UTC,
            MAX_CHARS,
            false,
        )
        .unwrap();
        assert_eq!(
            out, "ou_a 02:33：@Jeeson Bot 帮忙 CR 一下",
            "没有名字就用 id，@ 换成名字"
        );
        assert_eq!(at_utc(1_792_722_782_506), "02:33");
    }

    #[test]
    fn a_sender_known_from_a_mention_is_shown_by_name() {
        let all = vec![
            thread_item(
                "om_1",
                1000,
                "ou_a",
                "text",
                &json!({ "text": "@_user_1 看看" }),
                &[("@_user_1", "ou_b", "张三")],
            ),
            text("om_2", 2000, "ou_b", "好"),
        ];
        let chosen: Vec<&ThreadMessage> = all.iter().collect();
        let out = render(
            &chosen,
            &mention_names(&all),
            &TimeZone::UTC,
            MAX_CHARS,
            false,
        )
        .unwrap();
        assert!(out.lines().nth(1).unwrap().starts_with("张三 "), "{out}");
    }

    #[test]
    fn media_and_unknown_types_become_placeholders() {
        let cases = [
            ("image", json!({ "image_key": "img_x" }), "[图片]"),
            (
                "file",
                json!({ "file_key": "f", "file_name": "a.pdf" }),
                "[文件 a.pdf]",
            ),
            ("audio", json!({}), "[语音]"),
            ("media", json!({}), "[视频]"),
            ("sticker", json!({}), "[表情包]"),
            ("todo", json!({}), "[todo]"),
        ];
        for (kind, content, want) in cases {
            let message = thread_item("om_1", 1000, "ou_a", kind, &content, &[]);
            assert_eq!(message_text(&message), want, "{kind}");
        }
    }

    #[test]
    fn a_post_keeps_its_text_names_and_image_placeholders() {
        let content = json!({
            "title": "周报",
            "content": [
                [{ "tag": "text", "text": "请 " }, { "tag": "at", "user_id": "@_user_1" },
                 { "tag": "text", "text": " 看看 " }, { "tag": "a", "text": "链接", "href": "https://x" }],
                [{ "tag": "img", "image_key": "img_1" }],
            ],
        });
        let message = thread_item(
            "om_1",
            1000,
            "ou_a",
            "post",
            &content,
            &[("@_user_1", "ou_b", "张三")],
        );
        assert_eq!(message_text(&message), "周报\n请 @张三 看看 链接\n[图片]");
    }

    #[test]
    fn over_the_limit_keeps_the_newest_and_says_how_many_were_dropped() {
        let all: Vec<ThreadMessage> = (0..5)
            .map(|i| {
                text(
                    &format!("om_{i}"),
                    1000 + i,
                    "ou_a",
                    &format!("第{i}条{}", "字".repeat(20)),
                )
            })
            .collect();
        let chosen: Vec<&ThreadMessage> = all.iter().collect();
        // 每行约 "ou_a --:--：第N条" + 20 字，给三条多一点的预算。
        let line_len = render(
            &chosen[4..],
            &mention_names(&all),
            &TimeZone::UTC,
            MAX_CHARS,
            false,
        )
        .unwrap()
        .chars()
        .count();
        let out = render(
            &chosen,
            &mention_names(&all),
            &TimeZone::UTC,
            (line_len + 1) * 3 + 2,
            false,
        )
        .unwrap();
        assert!(out.starts_with("（更早的 2 条省略）\n"), "{out}");
        assert!(out.contains("第2条") && out.contains("第4条"), "{out}");
        assert!(!out.contains("第1条") && !out.contains("第0条"), "{out}");
    }

    #[test]
    fn one_huge_newest_message_is_cut_not_dropped() {
        let all = vec![text("om_1", 1000, "ou_a", &"长".repeat(500))];
        let chosen: Vec<&ThreadMessage> = all.iter().collect();
        let out = render(&chosen, &mention_names(&all), &TimeZone::UTC, 100, false).unwrap();
        assert!(out.ends_with("…（后略）"), "{out}");
        assert!(out.chars().count() < 120, "{}", out.chars().count());
    }

    #[test]
    fn an_incomplete_read_says_so() {
        let all = vec![text("om_1", 1000, "ou_a", "x")];
        let chosen: Vec<&ThreadMessage> = all.iter().collect();
        let out = render(
            &chosen,
            &mention_names(&all),
            &TimeZone::UTC,
            MAX_CHARS,
            true,
        )
        .unwrap();
        assert!(out.starts_with("（话题更早的消息没有读取"), "{out}");
    }

    // ---- 卡片文字

    #[test]
    fn a_real_review_card_yields_its_text_in_order() {
        let card: Value = serde_json::from_str(REVIEW_CARD).unwrap();
        let out = card_text(&card);
        // 段落里 plain_text 与 code_span 接成一行。
        assert!(
            out.starts_with(
                "三个 MR 审完了。结论：owl !3105 有两处条件性阻塞风险，需要负责人确认。"
            ),
            "{out}"
        );
        // 标题单独一行，表格有表头与行，列表有项目符号，代码块逐行。
        assert!(out.contains("\n一、两条阻塞项\n"), "{out}");
        assert!(out.contains("编号 | 位置 | 说明\n"), "{out}");
        assert!(
            out.contains("B1 | Drop.php:invokeDrop | `$this->$method()` 在 try 之外，异常会穿透"),
            "{out}"
        );
        assert!(
            out.contains("B2 | Template.php:escape | ESCAPE_BY_DEFAULT 为 false"),
            "{out}"
        );
        assert!(out.contains("\n- 先修 B1：把调用挪进 try\n"), "{out}");
        assert!(
            out.contains("\n- 再确认 B2：主题模板是否手动 escape\n"),
            "{out}"
        );
        assert!(
            out.contains("try {\n    $this->$method();\n} catch"),
            "{out}"
        );
        // 头像等附件不是正文。
        assert!(!out.contains("img_v3"), "{out}");
    }

    #[test]
    fn a_schema_one_card_is_read_header_first() {
        let card = json!({
            "config": { "wide_screen_mode": true },
            "elements": [
                { "tag": "div", "text": { "tag": "lark_md", "content": "**部署完成**\n版本 v1.2" } },
                { "tag": "hr" },
                { "tag": "action", "actions": [{ "tag": "button", "text": { "tag": "plain_text", "content": "查看" } }] },
                { "tag": "note", "elements": [{ "tag": "plain_text", "content": "来自 CI" }] },
            ],
            "header": { "title": { "tag": "plain_text", "content": "发布通知" } },
        });
        assert_eq!(
            card_text(&card),
            "发布通知\n**部署完成**\n版本 v1.2\n查看\n来自 CI"
        );
    }

    #[test]
    fn a_schema_one_table_reads_its_rows() {
        let card = json!({
            "elements": [{
                "tag": "table",
                "columns": [{ "name": "a", "display_name": "名称" }, { "name": "b", "display_name": "数量" }],
                "rows": [{ "a": "苹果", "b": 3 }, { "a": "梨", "b": 5 }],
            }],
        });
        assert_eq!(card_text(&card), "名称 | 数量\n苹果 | 3\n梨 | 5");
    }

    #[test]
    fn a_card_without_any_text_is_a_placeholder() {
        let message = thread_item(
            "om_1",
            1000,
            "cli_x",
            "interactive",
            &json!({ "json_card": "{\"body\":{\"tag\":\"body\",\"property\":{\"elements\":[]}}}" }),
            &[],
        );
        assert_eq!(message_text(&message), "[卡片]");
    }

    // ---- 读取（假开放平台）

    fn current(id: &'static str, at_ms: i64) -> Current<'static> {
        Current {
            message_id: id,
            created_ms: Some(at_ms),
        }
    }

    async fn read(fake: &FakeOpenApi, now: Current<'_>, timeout: Duration) -> Option<String> {
        let names = SenderNames::default();
        context(
            &fake.api(),
            &names,
            "omt_1",
            now,
            ME,
            &TimeZone::UTC,
            timeout,
        )
        .await
    }

    #[tokio::test]
    async fn the_thread_is_read_with_the_documented_query_and_rendered() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        fake.host_thread(
            "omt_1",
            vec![
                text("om_root", 1_792_722_782_000, "ou_a", "根"),
                text("om_now", 1_792_722_900_000, "ou_a", "当前"),
            ],
        );
        let out = read(&fake, current("om_now", 1_792_722_900_000), FETCH_TIMEOUT).await;
        assert_eq!(out.as_deref(), Some("ou_a 02:33：根"));

        let calls = fake.calls_to("thread_list");
        assert_eq!(calls.len(), 1, "一页就读完，不多问");
        assert_eq!(calls[0]["container_id_type"], "thread");
        assert_eq!(calls[0]["container_id"], "omt_1");
        assert_eq!(calls[0]["card_msg_content_type"], "raw_card_content");
        assert_eq!(calls[0]["sort_type"], "ByCreateTimeDesc");
    }

    #[tokio::test]
    async fn paging_stops_at_the_bots_last_message() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        // 130 条里机器人在第 60 条说过话：从最新往旧读，每页 50，读到它所在的第二页就够。
        let mut items: Vec<ThreadMessage> = (1..=130)
            .map(|i| text(&format!("om_{i}"), 1000 + i, "ou_a", &format!("msg-{i:03}")))
            .collect();
        items[59] = text("om_60", 1060, "cli_test", "之前的回答");
        items.push(text("om_now", 5000, "ou_a", "当前"));
        fake.host_thread("omt_1", items);
        let out = read(&fake, current("om_now", 5000), FETCH_TIMEOUT)
            .await
            .expect("有上下文");
        assert!(out.contains("msg-130") && out.contains("msg-061"), "{out}");
        assert!(
            !out.contains("msg-060") && !out.contains("msg-059"),
            "机器人那条及更早的不要：{out}"
        );
        assert_eq!(
            fake.calls_to("thread_list").len(),
            2,
            "读到第 60 条所在的第二页就停"
        );
    }

    #[tokio::test]
    async fn a_long_thread_is_capped_and_says_the_older_part_was_not_read() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        let mut items: Vec<ThreadMessage> = (1..=300)
            .map(|i| text(&format!("om_{i}"), 1000 + i, "ou_a", "x"))
            .collect();
        items.push(text("om_now", 9000, "ou_a", "当前"));
        fake.host_thread("omt_1", items);
        let out = read(&fake, current("om_now", 9000), FETCH_TIMEOUT)
            .await
            .expect("有上下文");
        assert_eq!(fake.calls_to("thread_list").len(), 4, "最多 200 条，四页");
        assert!(out.contains("话题更早的消息没有读取"), "{out}");
        // 200 条里含当前这一条，所以渲染出来 199 条正文。
        assert_eq!(out.lines().filter(|l| l.starts_with("ou_a ")).count(), 199);
    }

    #[tokio::test]
    async fn senders_without_a_mention_are_named_from_the_open_platform() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        fake.host_user("ou_a", "张三");
        fake.host_app("cli_jeeson", "Jeeson Bot");
        fake.host_thread(
            "omt_1",
            vec![
                text("om_root", 1000, "ou_a", "帮忙 CR"),
                text("om_review", 2000, "cli_jeeson", "审完了"),
                text("om_b", 3000, "ou_a", "收到"),
                text("om_now", 4000, "ou_a", "当前"),
            ],
        );
        let out = read(&fake, current("om_now", 4000), FETCH_TIMEOUT)
            .await
            .expect("有上下文");
        assert_eq!(
            out,
            "张三 00:00：帮忙 CR\nJeeson Bot 00:00：审完了\n张三 00:00：收到"
        );
        assert_eq!(fake.calls_to("user_info").len(), 1, "同一个人只查一次");
        assert_eq!(fake.calls_to("app_info").len(), 1);
    }

    #[tokio::test]
    async fn a_sender_named_by_a_mention_is_not_looked_up() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        fake.host_thread(
            "omt_1",
            vec![
                thread_item(
                    "om_root",
                    1000,
                    "ou_a",
                    "text",
                    &json!({ "text": "@_user_1 看看" }),
                    &[("@_user_1", "cli_jeeson", "Jeeson Bot")],
                ),
                text("om_review", 2000, "cli_jeeson", "好"),
                text("om_now", 4000, "ou_b", "当前"),
            ],
        );
        fake.host_user("ou_a", "张三");
        let out = read(&fake, current("om_now", 4000), FETCH_TIMEOUT)
            .await
            .expect("有上下文");
        assert!(out.contains("\nJeeson Bot 00:00：好"), "{out}");
        assert!(fake.calls_to("app_info").is_empty(), "mentions 里有就不查");
    }

    #[tokio::test]
    async fn refused_name_lookups_fall_back_to_ids() {
        let fake = FakeOpenApi::start(Behavior::refuse_names()).await;
        fake.host_thread(
            "omt_1",
            vec![
                text("om_root", 1000, "ou_a", "帮忙 CR"),
                text("om_review", 2000, "cli_jeeson", "审完了"),
                text("om_now", 4000, "ou_a", "当前"),
            ],
        );
        let out = read(&fake, current("om_now", 4000), FETCH_TIMEOUT).await;
        assert_eq!(
            out.as_deref(),
            Some("ou_a 00:00：帮忙 CR\ncli_jeeson 00:00：审完了")
        );
    }

    #[tokio::test]
    async fn slow_name_lookups_keep_the_context_within_the_timeout() {
        let fake = FakeOpenApi::start(Behavior::slow_names(Duration::from_secs(2))).await;
        fake.host_user("ou_a", "张三");
        fake.host_thread(
            "omt_1",
            vec![
                text("om_root", 1000, "ou_a", "帮忙 CR"),
                text("om_now", 4000, "ou_b", "当前"),
            ],
        );
        let started = std::time::Instant::now();
        let out = read(&fake, current("om_now", 4000), Duration::from_millis(300)).await;
        assert_eq!(
            out.as_deref(),
            Some("ou_a 00:00：帮忙 CR"),
            "名字没等到，正文还在"
        );
        assert!(started.elapsed() < Duration::from_secs(1), "按超时返回");
    }

    #[tokio::test]
    async fn a_refused_read_is_no_context() {
        let fake = FakeOpenApi::start(Behavior::refuse_thread_list()).await;
        assert_eq!(
            read(&fake, current("om_now", 5000), FETCH_TIMEOUT).await,
            None
        );
    }

    #[tokio::test]
    async fn a_slow_read_times_out_into_no_context() {
        let fake = FakeOpenApi::start(Behavior::slow_thread_list(Duration::from_secs(2))).await;
        fake.host_thread("omt_1", vec![text("om_root", 1000, "ou_a", "根")]);
        let started = std::time::Instant::now();
        let out = read(&fake, current("om_now", 5000), Duration::from_millis(200)).await;
        assert_eq!(out, None);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "按超时返回，不等对面"
        );
    }

    #[tokio::test]
    async fn a_missing_event_time_falls_back_to_the_listed_current_message() {
        let fake = FakeOpenApi::start(Behavior::default()).await;
        fake.host_thread(
            "omt_1",
            vec![
                text("om_a", 1000, "ou_a", "之前"),
                text("om_now", 2000, "ou_a", "当前"),
                text("om_late", 3000, "ou_b", "当前之后"),
            ],
        );
        let now = Current {
            message_id: "om_now",
            created_ms: None,
        };
        let out = read(&fake, now, FETCH_TIMEOUT).await.expect("有上下文");
        assert!(out.contains("之前") && !out.contains("当前之后"), "{out}");
    }
}
