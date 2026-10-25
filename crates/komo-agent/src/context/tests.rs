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
        fork: None,
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
        entry: None,
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

    // fork（`docs/background-tasks.md` §2b）：系统提示换成冻结的那份，回放以前缀开头，
    // 这条 Run 的任务（连同它的契约）排在前缀之后。
    let prefix = komo_kernel::types::turn::ForkPrefix {
        system_prompt: "父会话的系统提示".into(),
        messages: vec![
            ReplayMessage {
                role: Role::User,
                seq: Seq(7),
                text: Some("父会话里用户说的".into()),
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
                provider_blocks: None,
            },
            ReplayMessage {
                role: Role::Assistant,
                seq: Seq(8),
                text: Some("父会话里答的".into()),
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
                provider_blocks: None,
            },
        ],
    };
    let forked = assemble(ContextInput {
        history: history(),
        contract: Some(DelegateContract {
            schema: serde_json::json!({ "type": "object", "required": ["on"] }),
            mode: Default::default(),
        }),
        fork: Some(std::sync::Arc::new(prefix.clone())),
        ..input(Some("不该出现的指令"), Some("不该出现的记忆"))
    });
    assert_eq!(forked.system_prompt, "父会话的系统提示");
    assert_eq!(forked.messages[..2], prefix.messages[..]);
    assert_eq!(forked.run_from, 2, "压缩只在这条 Run 里切，前缀在它之前");
    assert_eq!(
        forked.messages[2].text.as_deref(),
        with_contract.messages[with_contract.run_from]
            .text
            .as_deref()
    );
}

/// 新存储按段补丁系统提示（`system_sections`）：把段用 `"\n\n"` 连起来必须与旧 `assemble` 的
/// 系统提示逐字相同——各种事实组合（有无指令 / bash / 项目规范 / skills / MCP 服务器 / 记忆、
/// 主 Agent 与子代理）都过一遍。
#[test]
fn golden_system_sections_match_legacy_prompt() {
    use komo_kernel::types::delegate::DelegateSpec;
    use komo_kernel::types::ids::{RunId, ToolCallId};

    let project = vec![
        ProjectFile {
            path: "/w/AGENTS.md".into(),
            content: "外层规范\n".into(),
        },
        ProjectFile {
            path: "/w/repo/CLAUDE.md".into(),
            content: "仓库规范".into(),
        },
        ProjectFile {
            path: "/w/repo/EMPTY.md".into(),
            content: "  \n".into(),
        },
    ];
    let servers = vec![ToolNamespace {
        name: "mcp__fs".into(),
        server: "fs".into(),
        description: Some("文件服务".into()),
        instructions: None,
    }];
    let delegate = InvocationContext::Delegated(DelegateSpec::new(
        RunId::from_raw("parent"),
        ToolCallId::from_raw("call"),
        "查一下 X",
    ));
    let tool_sets: [Vec<String>; 3] = [
        vec![],
        vec!["read".into()],
        vec![
            "read".into(),
            "rg".into(),
            "bash".into(),
            "edit".into(),
            "codemode".into(),
        ],
    ];
    let mut checked = 0;
    for tools in &tool_sets {
        for instructions in [None, Some("  你是 coder。\n"), Some("   ")] {
            for memory in [None, Some("[已知记忆]\n- 用户喜欢中文")] {
                for skills in [None, Some("可用技能：\n- a")] {
                    for (files, servers) in [(&[][..], &[][..]), (&project[..], &servers[..])] {
                        for invocation in [InvocationContext::Main, delegate.clone()] {
                            let input = ContextInput {
                                project_files: files.to_vec(),
                                tools: tools.clone(),
                                mcp_servers: servers.to_vec(),
                                skills: skills.map(str::to_string),
                                invocation: invocation.clone(),
                                ..input(instructions, memory)
                            };
                            let sections = system_sections(&input);
                            let joined = sections
                                .iter()
                                .map(|(_, text)| text.as_str())
                                .collect::<Vec<_>>()
                                .join("\n\n");
                            assert_eq!(
                                joined,
                                assemble(input).system_prompt,
                                "tools={tools:?} instructions={instructions:?} memory={memory:?}"
                            );
                            checked += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(checked, 3 * 3 * 2 * 2 * 2 * 2);
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
