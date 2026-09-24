//! `assemble`：唯一的装配入口。这里测的是 [`super::assemble`] 自己拼的那几段
//! （instructions 最前、memory 最后），不是 `prompt::build` 或 `history` 内部的规则——
//! 那两块各有自己的测试。

use std::path::PathBuf;

use super::*;

fn input(instructions: Option<&str>, memory: Option<&str>) -> ContextInput<'static> {
    ContextInput {
        instructions: instructions.map(str::to_string),
        workspace: PathBuf::from("/tmp/w"),
        tools: vec!["read".into()],
        history: Vec::new(),
        memory: memory.map(str::to_string),
        skills: None,
        tasks: None,
        invocation: InvocationContext::Main,
        model_result_bytes: 8 * 1024,
    }
}

/// 身份指令在系统提示**最前面**，首尾空白先修掉，与基座提示之间空一行。
#[test]
fn instructions_lead_the_system_prompt() {
    let context = assemble(input(Some("  你是 coder。\n只改 Rust。\n"), None));
    assert!(
        context
            .system_prompt
            .starts_with("你是 coder。\n只改 Rust。\n\n你是 komo"),
        "{}",
        context.system_prompt
    );
}

/// 没有指令时基座提示前面不多一个字。
#[test]
fn no_instructions_means_no_leading_blank_section() {
    let context = assemble(input(None, None));
    assert!(
        context.system_prompt.starts_with("你是 komo"),
        "{}",
        context.system_prompt
    );
}

/// Memory 永远排在**最后**（§10：同一段对话里逐字复用，放在尾部是为了不打掉前面那段的
/// 前缀缓存），用 `"\n\n"` 隔开，且不受有没有指令影响。
#[test]
fn memory_trails_the_prompt() {
    let context = assemble(input(
        Some("你是 coder。"),
        Some("[已知记忆]\n- 用户喜欢中文"),
    ));
    assert!(
        context
            .system_prompt
            .ends_with("\n\n[已知记忆]\n- 用户喜欢中文"),
        "{}",
        context.system_prompt
    );
    assert!(
        context.system_prompt.starts_with("你是 coder。"),
        "{}",
        context.system_prompt
    );
}

/// 没有记忆时结尾就是提示本身，一个字不多。
#[test]
fn no_memory_means_no_trailing_section() {
    let context = assemble(input(None, None));
    assert!(
        !context.system_prompt.contains("已知记忆"),
        "{}",
        context.system_prompt
    );
}

/// `tasks: None` 时不多一段——非分发器场景的提示逐字不变（golden 不受影响，
/// `docs/home-dispatcher.md` §9 Phase 3）。
#[test]
fn no_tasks_means_no_board_section() {
    let context = assemble(input(None, None));
    assert!(
        !context.system_prompt.contains("任务看板"),
        "{}",
        context.system_prompt
    );
}

/// 任务看板排在 **skills 之后、memory 之前**（`docs/agent.md` §10、
/// `docs/home-dispatcher.md` §5）。
#[test]
fn the_task_board_sits_between_skills_and_memory() {
    use crate::skills::{OfferContext, SkillRegistry};
    use tasks::{TaskBoard, TaskEntry, TaskState};

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("pr-review")).unwrap();
    std::fs::write(
        dir.path().join("pr-review").join("SKILL.md"),
        "---\nname: pr-review\ndescription: 怎么审一个 PR\n---\n正文",
    )
    .unwrap();
    let registry = SkillRegistry::new(vec![dir.path().to_path_buf()]);
    let catalog = registry.offer(&OfferContext::here(["read".to_string()]));

    let board = TaskBoard {
        entries: vec![TaskEntry {
            session: komo_kernel::types::ids::SessionId::from_raw(
                "0190f000-aaaa-7000-8000-0000003f2a9c",
            ),
            title: "查空调状态".into(),
            state: TaskState::Running,
            last_reply: None,
        }],
    };

    let context = assemble(ContextInput {
        instructions: Some("你是 komo 的分发器。".into()),
        workspace: PathBuf::from("/tmp/w"),
        tools: vec!["dispatch".into(), "follow".into()],
        history: Vec::new(),
        memory: Some("[已知记忆]\n- 用户喜欢中文".into()),
        skills: Some(catalog),
        tasks: Some(board),
        invocation: InvocationContext::Main,
        model_result_bytes: 8 * 1024,
    });

    let prompt = &context.system_prompt;
    let skills_at = prompt.find("Skills").expect("有 skills 目录");
    let tasks_at = prompt.find("任务看板").expect("有任务看板");
    let memory_at = prompt.find("已知记忆").expect("有记忆段");
    assert!(skills_at < tasks_at, "{prompt}");
    assert!(tasks_at < memory_at, "{prompt}");
}
