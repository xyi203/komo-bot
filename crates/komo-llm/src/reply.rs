//! 模型回复正文里的那个 JSON 对象。
//!
//! 记忆提取、关系判断与 `task` 交回前的契约复验（`durable::harness::orchestration::report` 的 `check_contract`）共用这一步：
//! 三处都是"模型回复里那个配平的 JSON 对象"，两套括号处理迟早会有一边少一个分支。

/// 剥掉围栏、取出第一个配平的 JSON 对象。提取与关系判断共用它——两边容忍的东西不同，
/// 迟早会有一边的括号处理少一个分支。
pub fn parse_json_object(text: &str) -> Option<&str> {
    first_object(strip_fence(text))
}

fn strip_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.trim_start_matches('\n')
        .strip_suffix("```")
        .unwrap_or(rest)
        .trim()
}

/// 第一个配平的 `{…}`。字符串里的括号不算。
fn first_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=index]);
                }
            }
            _ => {}
        }
    }
    None
}
