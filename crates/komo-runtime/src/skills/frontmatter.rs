//! `SKILL.md` 的 frontmatter（§5.6）。
//!
//! 只认四个键：`name`、`description`、可选的 `platforms` 与 `requires_tools`。**认不出
//! 的键留着不管**——skills 是人写的文件，为一个拼错的键把整份说明拒之门外，只会让人
//! 以为 komo 坏了。
//!
//! 自己解析而不是拉一个 YAML 库：这四个键的取值只有"一行字"和"一串字符串"两种形状，
//! 而 §13.4 的依赖清单里没有 YAML，为它加一个依赖要付编译时间。

/// frontmatter 里读出来的东西。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrontMatter {
    pub name: Option<String>,
    pub description: Option<String>,
    /// 只在这些平台上进提示目录。空 = 不限。
    pub platforms: Vec<String>,
    /// 需要这些工具（5 个基础工具或 toolbox 模块名）才进提示目录。
    pub requires_tools: Vec<String>,
}

/// 拆出 frontmatter 和正文。没有 frontmatter 时前者是默认值，正文是整份文件。
pub fn split(text: &str) -> (FrontMatter, &str) {
    let Some(rest) = strip_opening_fence(text) else {
        return (FrontMatter::default(), text);
    };
    let Some(end) = find_closing_fence(rest) else {
        // 开了头没收尾：当作没有 frontmatter，正文照给。
        return (FrontMatter::default(), text);
    };
    let (block, body) = rest.split_at(end.0);
    (parse(block), &body[end.1..])
}

fn strip_opening_fence(text: &str) -> Option<&str> {
    let trimmed = text.trim_start_matches('\u{feff}');
    for fence in ["---\r\n", "---\n"] {
        if let Some(rest) = trimmed.strip_prefix(fence) {
            return Some(rest);
        }
    }
    None
}

/// 返回 (正文块长度, 结束标记长度)。
fn find_closing_fence(rest: &str) -> Option<(usize, usize)> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((offset, line.len()));
        }
        offset += line.len();
    }
    None
}

fn parse(block: &str) -> FrontMatter {
    let mut front = FrontMatter::default();
    let mut collecting: Option<&'static str> = None;

    for line in block.lines() {
        let trimmed = line.trim_end();
        // 块状列表的一项：`  - shell`
        if let Some(item) = trimmed.trim_start().strip_prefix("- ")
            && let Some(key) = collecting
        {
            push(&mut front, key, clean(item));
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        collecting = None;
        match key.as_str() {
            "name" => front.name = non_empty(clean(value)),
            "description" => front.description = non_empty(clean(value)),
            "platforms" => collecting = collect(&mut front, "platforms", value),
            "requires_tools" | "requires-tools" => {
                collecting = collect(&mut front, "requires_tools", value)
            }
            _ => {}
        }
    }
    front
}

/// 行内列表就地收下，空值表示"下面几行是块状列表"。
fn collect(front: &mut FrontMatter, key: &'static str, value: &str) -> Option<&'static str> {
    let value = value.trim();
    if value.is_empty() {
        return Some(key);
    }
    let inline = value.trim_start_matches('[').trim_end_matches(']');
    for item in inline.split(',') {
        let item = clean(item);
        if !item.is_empty() {
            push(front, key, item);
        }
    }
    None
}

fn push(front: &mut FrontMatter, key: &str, value: String) {
    if value.is_empty() {
        return;
    }
    match key {
        "platforms" => front.platforms.push(value),
        "requires_tools" => front.requires_tools.push(value),
        _ => {}
    }
}

fn clean(value: &str) -> String {
    value
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim()
        .to_string()
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_keys_are_read_and_the_body_starts_after_the_fence() {
        let text = "---\nname: pr-review\ndescription: 怎么审一个 PR\nplatforms: [linux, macos]\nrequires_tools:\n  - shell\n  - read\n---\n第一步……\n";
        let (front, body) = split(text);
        assert_eq!(front.name.as_deref(), Some("pr-review"));
        assert_eq!(front.description.as_deref(), Some("怎么审一个 PR"));
        assert_eq!(front.platforms, vec!["linux", "macos"]);
        assert_eq!(front.requires_tools, vec!["shell", "read"]);
        assert_eq!(body, "第一步……\n");
    }

    #[test]
    fn an_unknown_key_is_ignored_rather_than_refusing_the_file() {
        let text = "---\nname: a\nauthor: 我\n---\n正文";
        let (front, body) = split(text);
        assert_eq!(front.name.as_deref(), Some("a"));
        assert_eq!(body, "正文");
    }

    #[test]
    fn a_file_without_frontmatter_is_all_body() {
        let (front, body) = split("# 标题\n正文");
        assert_eq!(front, FrontMatter::default());
        assert_eq!(body, "# 标题\n正文");
    }

    #[test]
    fn an_unclosed_fence_is_not_read_as_frontmatter() {
        let text = "---\nname: a\n正文没有收尾";
        let (front, body) = split(text);
        assert_eq!(front.name, None);
        assert_eq!(body, text);
    }

    #[test]
    fn quotes_and_crlf_do_not_end_up_in_the_values() {
        let text = "---\r\nname: \"a b\"\r\nrequires_tools: ['shell']\r\n---\r\n正文";
        let (front, _) = split(text);
        assert_eq!(front.name.as_deref(), Some("a b"));
        assert_eq!(front.requires_tools, vec!["shell"]);
    }

    #[test]
    fn an_empty_list_stays_empty() {
        let (front, _) = split("---\nname: a\nplatforms: []\n---\n");
        assert!(front.platforms.is_empty());
    }
}
