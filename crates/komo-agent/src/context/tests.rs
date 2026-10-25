//! `system_sections`：系统提示的按段装配。这里测的是 [`super::system_sections`] 自己拼的那几段
//! （instructions 最前、memory 最后），不是 `prompt` 内部的规则——那块有自己的测试。

use std::path::PathBuf;

use super::*;

fn input(instructions: Option<&str>, memory: Option<&str>) -> ContextInput {
    ContextInput {
        instructions: instructions.map(str::to_string),
        project_files: Vec::new(),
        workspace: PathBuf::from("/tmp/w"),
        tools: vec!["read".into()],
        mcp_servers: Vec::new(),
        memory: memory.map(str::to_string),
        skills: None,
        invocation: InvocationContext::Main,
    }
}

/// 段用 `"\n\n"` 连起来就是完整的系统提示。
fn prompt_of(input: &ContextInput) -> String {
    system_sections(input)
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 身份指令在系统提示**最前面**，首尾空白先修掉，与基座提示之间空一行。
#[test]
fn instructions_lead_the_system_prompt() {
    let prompt = prompt_of(&input(Some("  你是 coder。\n只改 Rust。\n"), None));
    assert!(
        prompt.starts_with("你是 coder。\n只改 Rust。\n\n你是 komo"),
        "{prompt}"
    );
}

/// 没有指令时基座提示前面不多一个字。
#[test]
fn no_instructions_means_no_leading_blank_section() {
    let prompt = prompt_of(&input(None, None));
    assert!(prompt.starts_with("你是 komo"), "{prompt}");
}

/// Memory 永远排在**最后**（§10：同一段对话里逐字复用，放在尾部是为了不打掉前面那段的
/// 前缀缓存），用 `"\n\n"` 隔开，且不受有没有指令影响。
#[test]
fn memory_trails_the_prompt() {
    let prompt = prompt_of(&input(
        Some("你是 coder。"),
        Some("[已知记忆]\n- 用户喜欢中文"),
    ));
    assert!(
        prompt.ends_with("\n\n[已知记忆]\n- 用户喜欢中文"),
        "{prompt}"
    );
    assert!(prompt.starts_with("你是 coder。"), "{prompt}");
}

/// 没有记忆时结尾就是提示本身，一个字不多。
#[test]
fn no_memory_means_no_trailing_section() {
    let prompt = prompt_of(&input(None, None));
    assert!(!prompt.contains("已知记忆"), "{prompt}");
}

#[test]
fn system_sections_are_keyed_in_prompt_order_and_skip_the_empty_ones() {
    let sections = system_sections(&ContextInput {
        project_files: vec![ProjectFile {
            path: "/w/AGENTS.md".into(),
            content: "规范".into(),
        }],
        skills: Some("技能".into()),
        tools: vec!["bash".into()],
        ..input(Some("你是 coder。"), Some("记忆"))
    });
    let keys: Vec<&str> = sections.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        ["instructions", "base", "project", "skills", "memory"]
    );
    assert_eq!(sections[0].1, "你是 coder。");
    assert!(sections[1].1.starts_with("你是 komo"));

    let bare = system_sections(&input(None, None));
    let keys: Vec<&str> = bare.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["base"]);
}
