//! `system_sections`：系统提示的按段装配。这里测的是 [`super::system_sections`] 自己拼的那几段
//! （instructions 最前），不是 `prompt` 内部的规则——那块有自己的测试。

use std::path::PathBuf;

use komo_kernel::types::task::{TASK_STATUS_BLOCKED, TASK_STATUS_DONE};

use super::*;

fn input(instructions: Option<&str>) -> ContextInput {
    ContextInput {
        instructions: instructions.map(str::to_string),
        project_files: Vec::new(),
        workspace: PathBuf::from("/tmp/w"),
        tools: vec!["read".into()],
        mcp_servers: Vec::new(),
        skills: None,
        invocation: InvocationContext::Main,
        background_task: false,
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
    let prompt = prompt_of(&input(Some("  你是 coder。\n只改 Rust。\n")));
    assert!(
        prompt.starts_with("你是 coder。\n只改 Rust。\n\n你是 komo"),
        "{prompt}"
    );
}

/// 没有指令时基座提示前面不多一个字。
#[test]
fn no_instructions_means_no_leading_blank_section() {
    let prompt = prompt_of(&input(None));
    assert!(prompt.starts_with("你是 komo"), "{prompt}");
}

/// 任务会话的 `base` 段末尾是任务模式；对话会话没有，段的 key 与次序不变。
#[test]
fn a_task_session_ends_its_base_with_the_task_mode() {
    let task = system_sections(&ContextInput {
        background_task: true,
        ..input(Some("你是 coder。"))
    });
    let keys: Vec<&str> = task.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["instructions", "base"]);
    assert!(task[1].1.ends_with(TASK_MODE_BLOCK), "{}", task[1].1);
    // 交回按这两种末行定标题（runtime 的 report），写法以 kernel 的常量为准。
    for status in [TASK_STATUS_DONE, TASK_STATUS_BLOCKED] {
        assert!(TASK_MODE_BLOCK.contains(status), "{status}");
    }

    let prompt = prompt_of(&input(None));
    assert!(!prompt.contains(TASK_MODE_BLOCK), "{prompt}");
}

/// 召回的记忆不进系统提示：它按 Run 追加在输入之后（`docs/memory.md` §9.4）。
#[test]
fn the_system_prompt_never_carries_recalled_memories() {
    let prompt = prompt_of(&input(None));
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
        ..input(Some("你是 coder。"))
    });
    let keys: Vec<&str> = sections.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["instructions", "base", "project", "skills"]);
    assert_eq!(sections[0].1, "你是 coder。");
    assert!(sections[1].1.starts_with("你是 komo"));

    let bare = system_sections(&input(None));
    let keys: Vec<&str> = bare.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["base"]);
}

/// 能跑 bash 时系统提示教 Agent 自己查记忆（`docs/memory.md` §9.4「Agent 主动查」）；这句是静态文字，
/// 跨轮、跨输入逐字不变，不影响前缀缓存。没有 bash 就没有这一段。
#[test]
fn the_self_block_teaches_searching_memory_and_stays_byte_stable() {
    let with_bash = ContextInput {
        tools: vec!["bash".into()],
        ..input(None)
    };
    let first = prompt_of(&with_bash);
    assert!(first.contains("komo memory search"), "{first}");
    assert!(first.contains("约定照做，线索先核实"), "{first}");
    // 同一份输入再装配一次逐字相同；换了工作目录之外的输入（比如召回）不会进系统段。
    assert_eq!(first, prompt_of(&with_bash));
    assert!(!prompt_of(&input(None)).contains("komo memory search"));
}

/// 能跑 bash 时系统提示教 Agent 当场记目标与领域（`docs/goals.md` §4）；静态文字，逐字不变。
#[test]
fn the_self_block_teaches_recording_goals_and_areas_and_stays_byte_stable() {
    let with_bash = ContextInput {
        tools: vec!["bash".into()],
        ..input(None)
    };
    let first = prompt_of(&with_bash);
    assert!(first.contains("`komo goal` / `komo area` 记下"), "{first}");
    assert!(first.contains("先放一放、办完了、不管了"), "{first}");
    assert!(first.contains("`komo goal next` 更新下一步"), "{first}");
    assert!(first.contains("`komo goal link` 挂上"), "{first}");
    assert!(first.contains("`komo goal resume`"), "{first}");
    assert_eq!(first, prompt_of(&with_bash));
    assert!(!prompt_of(&input(None)).contains("komo goal"));
}
