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
        direct_reply: false,
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
        direct_reply: false,
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

/// 分发派出的任务会话用「直接对话」那一段，不再说"交回给派你的会话"；两段不会同时出现。
#[test]
fn a_dispatched_task_session_talks_to_the_user_directly() {
    let direct = prompt_of(&ContextInput {
        background_task: true,
        direct_reply: true,
        ..input(None)
    });
    assert!(direct.ends_with(TASK_DIRECT_BLOCK), "{direct}");
    assert!(!direct.contains(TASK_MODE_BLOCK), "{direct}");
    // 末行状态行照旧要写（程序投递前去掉），写法以 kernel 的常量为准。
    for status in [TASK_STATUS_DONE, TASK_STATUS_BLOCKED] {
        assert!(TASK_DIRECT_BLOCK.contains(status), "{status}");
    }
    // 提示里不能把派发暴露给用户：要求模型不说的那几个词只出现在"不要出现"的那一句里。
    assert!(TASK_DIRECT_BLOCK.contains("不要出现「任务」"));
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

/// 挂了 `komo` 工具时系统提示教 Agent 用它自己查记忆（`docs/memory.md` §9.4「Agent 主动查」）；这句是
/// 静态文字，跨轮、跨输入逐字不变，不影响前缀缓存。没挂 `komo` 就没有这一段。
#[test]
fn the_self_block_points_at_the_komo_tool_for_memory_and_stays_byte_stable() {
    let with_komo = ContextInput {
        tools: vec!["komo".into()],
        ..input(None)
    };
    let first = prompt_of(&with_komo);
    assert!(first.contains("memory.search"), "{first}");
    assert!(first.contains("tool://komo/doc"), "{first}");
    assert!(first.contains("约定照做，线索先核实"), "{first}");
    // 同一份输入再装配一次逐字相同；换了工作目录之外的输入（比如召回）不会进系统段。
    assert_eq!(first, prompt_of(&with_komo));
    assert!(!prompt_of(&input(None)).contains("memory.search"));
}

/// 系统提示教 Agent 当场记目标（`docs/goals.md` §4）：动作写成 `komo` 工具的动作；静态文字，逐字不变。
#[test]
fn the_self_block_teaches_recording_goals_with_the_komo_actions() {
    let with_komo = ContextInput {
        tools: vec!["komo".into()],
        ..input(None)
    };
    let first = prompt_of(&with_komo);
    for action in [
        "goal.add",
        "goal.wait",
        "goal.done",
        "goal.next",
        "goal.link",
        "goal.resume",
        "cron.list",
        "session.search",
        "task.list",
        "memory.add",
    ] {
        assert!(first.contains(action), "{action}：{first}");
    }
    assert_eq!(first, prompt_of(&with_komo));
    assert!(!prompt_of(&input(None)).contains("goal.add"));
}

/// 不再教模型去 shell 里跑 `komo memory/goal/area/session/cron …`：挂着 `bash` 也一样；这一段只跟着
/// `komo` 工具走，`bash` 只带来重启 Gateway 的那句提醒（`komo gateway` 是给人用的命令，不在工具里）。
#[test]
fn the_self_block_follows_the_komo_tool_not_the_shell() {
    let only_bash = prompt_of(&ContextInput {
        tools: vec!["bash".into()],
        ..input(None)
    });
    assert!(!only_bash.contains("tool://komo/doc"), "{only_bash}");
    assert!(!only_bash.contains("memory.search"), "{only_bash}");
    assert!(
        only_bash.contains("komo gateway restart|stop"),
        "{only_bash}"
    );

    let both = prompt_of(&ContextInput {
        tools: vec!["bash".into(), "komo".into()],
        ..input(None)
    });
    assert!(both.contains("tool://komo/doc"), "{both}");
    assert!(both.contains("komo gateway restart|stop"), "{both}");
    for old in [
        "komo memory",
        "komo goal",
        "komo area",
        "komo session",
        "komo cron",
        "用 bash 跑 `komo",
        "--help",
    ] {
        assert!(!both.contains(old), "{old}：{both}");
    }

    let only_komo = prompt_of(&ContextInput {
        tools: vec!["komo".into()],
        ..input(None)
    });
    assert!(only_komo.contains("tool://komo/doc"));
    assert!(
        !only_komo.contains("komo gateway"),
        "没有 bash 就没有重启的提醒：{only_komo}"
    );
}
