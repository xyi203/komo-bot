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

/// 正在往下收的那一段。
enum Collecting {
    /// `platforms:` 下面那串 `- linux`。
    List(&'static str),
    /// 块标量（`description: >`）：下面那些缩进行/空行是它的正文。
    Block {
        key: BlockKey,
        literal: bool,
        lines: Vec<String>,
    },
}

/// 块标量的两个落点。
#[derive(Clone, Copy)]
enum BlockKey {
    Name,
    Description,
}

fn parse(block: &str) -> FrontMatter {
    let mut front = FrontMatter::default();
    let mut collecting: Option<Collecting> = None;

    for line in block.lines() {
        let trimmed = line.trim_end();
        let mut taken = false;
        match collecting.as_mut() {
            // 块状列表的一项：`  - shell`
            Some(Collecting::List(key)) => {
                if let Some(item) = trimmed.trim_start().strip_prefix("- ") {
                    push(&mut front, key, clean(item));
                    taken = true;
                }
            }
            // 块标量的正文：缩进行，空行是段落分隔。
            Some(Collecting::Block { lines, .. })
                if trimmed.is_empty() || line.starts_with([' ', '\t']) =>
            {
                lines.push(trimmed.trim().to_string());
                taken = true;
            }
            _ => {}
        }
        if taken {
            continue;
        }

        // 换了一行键值：上一段先收尾。
        if let Some(Collecting::Block {
            key,
            literal,
            lines,
        }) = collecting.take()
        {
            assign_block(&mut front, key, literal, &lines);
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "name" | "description" => {
                let slot = if key == "name" {
                    BlockKey::Name
                } else {
                    BlockKey::Description
                };
                match block_scalar(value) {
                    Some(literal) => {
                        collecting = Some(Collecting::Block {
                            key: slot,
                            literal,
                            lines: Vec::new(),
                        })
                    }
                    None => match slot {
                        BlockKey::Name => front.name = non_empty(clean(value)),
                        BlockKey::Description => front.description = non_empty(clean(value)),
                    },
                }
            }
            "platforms" => {
                collecting = collect(&mut front, "platforms", value).map(Collecting::List)
            }
            "requires_tools" | "requires-tools" => {
                collecting = collect(&mut front, "requires_tools", value).map(Collecting::List)
            }
            _ => {}
        }
    }
    // 块标量一直写到文件末尾（fence 里最后一行没有换行）也要收尾。
    if let Some(Collecting::Block {
        key,
        literal,
        lines,
    }) = collecting
    {
        assign_block(&mut front, key, literal, &lines);
    }
    front
}

fn assign_block(front: &mut FrontMatter, key: BlockKey, literal: bool, lines: &[String]) {
    let text = fold_block(lines, literal);
    match key {
        BlockKey::Name => front.name = non_empty(text),
        BlockKey::Description => front.description = non_empty(text),
    }
}

/// `>` / `|`（可带 chomping 或缩进指示）：YAML 的块标量。
///
/// **描述几乎都是这么写的**，而按单行读的话 `description: >` 得到的是一个字符 `>`——目录行
/// 于是变成 `- log-diagnosis：>`，模型既不知道它讲什么，也不知道它存不存在（真实会话里它为
/// 了找这个 skill 去 `ls` 了整个目录）。§5.6 说 frontmatter 是 YAML，这是解析漏了一档。
///
/// 答的是"是不是字面块"（`|` 保留换行，`>` 折成一格空格）。
fn block_scalar(value: &str) -> Option<bool> {
    let marker = value.trim_matches(|c: char| c == '"' || c == '\'');
    let mut chars = marker.chars();
    let literal = match chars.next()? {
        '>' => false,
        '|' => true,
        _ => return None,
    };
    // 只认这三种附加标记，别的（比如把 `>` 当正文用）当普通单行值。
    if chars.all(|c| c == '-' || c == '+' || c.is_ascii_digit()) {
        Some(literal)
    } else {
        None
    }
}

/// 块标量那些行拼成一段：折行（`>`）用空格接、字面（`|`）用换行接，空行留一个换行。
fn fold_block(lines: &[String], literal: bool) -> String {
    let mut out = String::new();
    for line in lines {
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push(if literal { '\n' } else { ' ' });
        }
        out.push_str(line);
    }
    out.trim().to_string()
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

    /// 折行块（`>`）：描述几乎都是这么写的，按单行读只会得到一个字符 `>`。
    ///
    /// 描述里带冒号的行（`Path convention: …`）**不能**被当成一个键——那正是折叠块要收的
    /// 正文。
    #[test]
    fn a_folded_description_is_joined_into_one_sentence() {
        let text = "---\nname: log-diagnosis\ndescription: >\n  下单 / 交易链路 bug 的端到端日志诊断。\n  Path convention: `../archery/scripts/archery`。\n  触发词：「查一下这个订单为什么失败」。\nplatforms: [macos]\n---\n正文\n";
        let (front, body) = split(text);
        assert_eq!(front.name.as_deref(), Some("log-diagnosis"));
        assert_eq!(
            front.description.as_deref(),
            Some(
                "下单 / 交易链路 bug 的端到端日志诊断。 Path convention: `../archery/scripts/archery`。 触发词：「查一下这个订单为什么失败」。"
            ),
        );
        assert_eq!(front.platforms, vec!["macos"], "块在下一个键处收尾");
        assert_eq!(body, "正文\n");
    }

    /// 字面块（`|`）保留换行，chomping 标记（`>-` / `|-`）不影响取值。
    #[test]
    fn a_literal_block_keeps_its_breaks_and_the_chomping_marker_is_not_a_value() {
        let text = "---\nname: a\ndescription: |-\n  第一行\n  第二行\n---\n";
        let (front, _) = split(text);
        assert_eq!(front.description.as_deref(), Some("第一行\n第二行"));

        let (folded, _) = split("---\nname: a\ndescription: >-\n  一段说明\n---\n");
        assert_eq!(folded.description.as_deref(), Some("一段说明"));
    }

    /// 单行里出现 `>`（不是块标量）时按普通值读；块标量写到 fence 前一行也收得回来。
    #[test]
    fn a_lone_marker_character_is_only_a_marker_on_its_own() {
        let (front, _) = split("---\nname: a\ndescription: 大于 > 小于 <\n---\n");
        assert_eq!(front.description.as_deref(), Some("大于 > 小于 <"));

        let (last, _) = split("---\nname: a\ndescription: >\n  最后一行没有换行\n---");
        assert_eq!(last.description.as_deref(), Some("最后一行没有换行"));
    }
}
