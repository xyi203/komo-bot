//! `assemble`：唯一的装配入口。这里测的是 [`super::assemble`] 自己拼的那几段
//! （instructions 最前、memory 最后），不是 `prompt::build` 或 `history` 内部的规则——
//! 那两块各有自己的测试。

use std::path::PathBuf;

use super::*;

fn input(instructions: Option<&str>, memory: Option<&str>) -> ContextInput<'static> {
    ContextInput {
        instructions: instructions.map(str::to_string),
        project_files: Vec::new(),
        project_update: None,
        contract: None,
        workspace: PathBuf::from("/tmp/w"),
        tools: vec!["read".into()],
        mcp_servers: Vec::new(),
        history: Vec::new(),
        memory: memory.map(str::to_string),
        skills: None,
        invocation: InvocationContext::Main,
        projection: ProjectionContext {
            model_result_bytes: 8 * 1024,
            decay: None,
        },
        decayed: Default::default(),
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

/// `task` 的结果契约补在这条 Run 的任务后面，系统提示一个字不变（前缀缓存不作废）。
#[test]
fn a_task_contract_trails_the_task_not_the_system_prompt() {
    use komo_kernel::events::{Event, EventPayload, RunAccepted, RunStarted};
    use komo_kernel::traits::Clock;
    use komo_kernel::types::digest::ContentHash;
    use komo_kernel::types::ids::{EventId, ExecutorId, RequestKey, RunId, Seq, SessionId};
    use komo_kernel::types::plan::PlanSource;

    let run = RunId::from_raw("run-1");
    let event = |seq: u64, payload| Event {
        v: 1,
        seq: Seq(seq),
        event_id: EventId::from_raw(format!("evt-{seq}")),
        session: SessionId::from_raw("sess-1"),
        run: Some(run.clone()),
        ts: komo_kernel::test_support::TestClock::fixed().now(),
        payload,
    };
    let events = vec![
        event(
            1,
            EventPayload::RunAccepted(RunAccepted {
                request_key: RequestKey::new("key-1"),
                input_hash: ContentHash::of_str("查一下空调"),
                text: Some("查一下空调".into()),
                text_ref: None,
                source: PlanSource::Interactive {
                    session: SessionId::from_raw("sess-1"),
                },
                peer: None,
                model: None,
                effort: None,
                delegate: None,
                snapshot: None,
                interject: false,
            }),
        ),
        event(
            2,
            EventPayload::RunStarted(RunStarted {
                executor: ExecutorId::from_raw("exec-1"),
                generation: 1,
            }),
        ),
    ];
    let surface = komo_kernel::fold::fold(&events);
    let history = || {
        entries(&surface, ReplayScope::Conversation(&run))
            .into_iter()
            .map(|entry| ResolvedMessage {
                text: entry.message.text.clone(),
                outputs: Vec::new(),
                entry,
            })
            .collect::<Vec<_>>()
    };
    let plain = assemble(ContextInput {
        history: history(),
        ..input(None, None)
    });
    let with_contract = assemble(ContextInput {
        history: history(),
        contract: Some(DelegateContract {
            schema: serde_json::json!({ "type": "object", "required": ["on"] }),
            mode: Default::default(),
        }),
        ..input(None, None)
    });

    assert_eq!(with_contract.system_prompt, plain.system_prompt);
    let task = with_contract.messages[with_contract.run_from]
        .text
        .clone()
        .unwrap_or_default();
    assert!(task.starts_with("查一下空调\n\n（结果契约）"), "{task}");
    assert!(task.contains("\"required\""), "{task}");
}
