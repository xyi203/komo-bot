//! 工具参数的 JSON 容错解析：模型偶尔在字符串里直接写换行、写出非法转义，或者流在对象
//! 中间收了尾。三层依次试，前一层成功就不往下走：
//!
//! 1. 严格解析；
//! 2. 修复后解析：转义字符串里的裸控制字符、把非法转义前的反斜杠加倍；
//! 3. 补上没闭合的字符串 / 对象 / 数组后解析（`complete` 为真才用）。
//!
//! 第 3 层会让半截参数看起来像完整的一份，所以只给**流正常结束**的回复用；被截断的回复不执行
//! 任何调用，用不着它（调用方传 `complete = false`）。

use serde_json::Value;

/// 解析工具参数文本。`complete` 表示流正常结束，允许第 3 层补闭合。修复层与补闭合层只接受
/// 对象（工具参数恒为对象），全失败给 `None`。
pub(crate) fn parse_arguments(raw: &str, complete: bool) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(raw) {
        return Some(value);
    }
    let repaired = escape_strings(raw);
    if let Some(value) = parse_object(&repaired) {
        return Some(value);
    }
    if complete {
        return close_and_parse(&repaired);
    }
    None
}

fn parse_object(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(Value::is_object)
}

/// 转义字符串里的裸控制字符；反斜杠后不是合法转义（含不足 4 位十六进制的 `\u`）就把反斜杠
/// 加倍。字符串之外的字符原样保留。
fn escape_strings(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if !in_string {
            out.push(c);
            in_string = c == '"';
            continue;
        }
        match c {
            '"' => {
                out.push(c);
                in_string = false;
            }
            '\\' => match chars.get(i).copied() {
                Some(next @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't')) => {
                    out.push('\\');
                    out.push(next);
                    i += 1;
                }
                Some('u')
                    if chars
                        .get(i + 1..i + 5)
                        .is_some_and(|hex| hex.iter().all(char::is_ascii_hexdigit)) =>
                {
                    out.push_str("\\u");
                    out.extend(&chars[i + 1..i + 5]);
                    i += 5;
                }
                _ => out.push_str("\\\\"),
            },
            c if (c as u32) < 0x20 => match c {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                _ => out.push_str(&format!("\\u{:04x}", c as u32)),
            },
            c => out.push(c),
        }
    }
    out
}

/// 补闭合：只补收尾符号，结尾是半个键、半个字面量、悬着的冒号这类补不出确定值的残片就不
/// 猜，交回 `None`（调用方当没收齐重试）。
fn close_and_parse(text: &str) -> Option<Value> {
    parse_object(&close(text.trim_end()))
}

/// 在 `prefix` 后面补上没闭合的字符串与括号；悬着的逗号去掉。
fn close(prefix: &str) -> String {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in prefix.chars() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut out = prefix.to_owned();
    if in_string {
        // 停在转义符上：补一个反斜杠凑成 `\\`。
        if escaped {
            out.push('\\');
        }
        out.push('"');
    }
    let trimmed = out.trim_end();
    if trimmed.ends_with(',') {
        out = trimmed.trim_end_matches(',').to_owned();
    }
    while let Some(closer) = stack.pop() {
        out.push(closer);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_json_is_returned_as_is() {
        assert_eq!(parse_arguments(r#"{"a":1}"#, false), Some(json!({"a": 1})));
    }

    #[test]
    fn raw_control_characters_inside_strings_are_escaped() {
        let value = parse_arguments("{\"text\":\"line1\nline2\ttab\"}", false).unwrap();
        assert_eq!(value, json!({"text": "line1\nline2\ttab"}));
    }

    #[test]
    fn invalid_escapes_get_their_backslash_doubled() {
        // 正则、Windows 路径里常见的 `\d` `\U`；`\u12` 不足 4 位也算非法。
        let value = parse_arguments(r#"{"re":"\d+\.\u12"}"#, false).unwrap();
        assert_eq!(value, json!({"re": "\\d+\\.\\u12"}));
        // 合法转义不动。
        let value = parse_arguments(r#"{"a":"x\nyé"}"#, false).unwrap();
        assert_eq!(value, json!({"a": "x\ny\u{e9}"}));
    }

    #[test]
    fn unclosed_json_is_completed_only_when_the_stream_ended_normally() {
        let raw = r#"{"path":"a.txt","items":[1,2"#;
        assert_eq!(
            parse_arguments(raw, true),
            Some(json!({"path": "a.txt", "items": [1, 2]}))
        );
        assert_eq!(parse_arguments(raw, false), None);
    }

    #[test]
    fn unclosed_string_and_nested_objects_are_closed() {
        assert_eq!(
            parse_arguments(r#"{"a":{"b":"hel"#, true),
            Some(json!({"a": {"b": "hel"}}))
        );
    }

    #[test]
    fn half_written_key_or_value_is_not_guessed() {
        assert_eq!(parse_arguments(r#"{"a":1,"b"#, true), None);
        assert_eq!(parse_arguments(r#"{"a":1,"b":tru"#, true), None);
        assert_eq!(parse_arguments(r#"{"a":1,"b":"#, true), None);
        // 悬着的逗号可以去掉。
        assert_eq!(parse_arguments(r#"{"a":1,"#, true), Some(json!({"a": 1})));
    }

    #[test]
    fn repair_and_closing_combine() {
        let value = parse_arguments("{\"cmd\":\"echo 1\necho 2", true).unwrap();
        assert_eq!(value, json!({"cmd": "echo 1\necho 2"}));
    }

    #[test]
    fn hopeless_text_gives_none() {
        assert_eq!(parse_arguments("not json", true), None);
        assert_eq!(parse_arguments("[1,2", true), None);
    }
}
