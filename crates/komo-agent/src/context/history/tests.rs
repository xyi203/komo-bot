//! `entries` 选窗口、`to_replay_messages` 投影：两步都是纯逻辑，不用读任何 payload 或
//! `output.json`。真的要读外置正文、或者读不出来该怎么办的那几条测试，留在 Gateway 的
//! `context_sources`（它们需要 `PayloadStore` / `ToolOutputStore`，`docs/agent.md` §20
//! Phase 2）。

use super::*;
use komo_kernel::events::{
    Event, EventPayload, MessageAssistant, RunAccepted, RunCompleted, RunStarted,
};
use komo_kernel::fold::fold;
use komo_kernel::projection::ProjectionContext;
use komo_kernel::traits::Clock;
use komo_kernel::types::delegate::DelegateSpec;
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{AttemptId, EventId, ExecutorId, RequestKey, SessionId};
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::refs::{ContentRef, OutputRef, ToolResultStatus};

const BUDGET: ProjectionContext = ProjectionContext {
    model_result_bytes: 8 * 1024,
    decay: None,
};

/// komo-agent 不依赖 `time`（§13.4 的依赖表）：借道 `TestClock`（实现了
/// `komo_kernel::traits::Clock`）拿一个固定时间戳，不必自己拼这个类型的路径。
fn event(seq: u64, run: &RunId, payload: EventPayload) -> Event {
    Event {
        v: 1,
        seq: Seq(seq),
        event_id: EventId::from_raw(format!("evt-{seq}")),
        session: SessionId::from_raw("sess-1"),
        run: Some(run.clone()),
        ts: komo_kernel::test_support::TestClock::fixed().now(),
        payload,
    }
}

fn accepted(run: &RunId, seq: u64, text: &str) -> Event {
    accepted_as(run, seq, Some(text), None)
}

fn accepted_as(run: &RunId, seq: u64, text: Option<&str>, delegate: Option<DelegateSpec>) -> Event {
    event(
        seq,
        run,
        EventPayload::RunAccepted(RunAccepted {
            request_key: RequestKey::new(format!("key-{seq}")),
            input_hash: ContentHash::of_str(text.unwrap_or_default()),
            text: text.map(str::to_string),
            text_ref: None,
            source: PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            peer: None,
            model: None,
            effort: None,
            delegate,
            snapshot: None,
        }),
    )
}

fn started(run: &RunId, seq: u64) -> Event {
    event(
        seq,
        run,
        EventPayload::RunStarted(RunStarted {
            executor: ExecutorId::from_raw("exec-1"),
            generation: 1,
        }),
    )
}

fn completed(run: &RunId, seq: u64) -> Event {
    event(
        seq,
        run,
        EventPayload::RunCompleted(RunCompleted {
            final_message: None,
            final_message_ref: None,
            rounds: 2,
        }),
    )
}

fn assistant(
    run: &RunId,
    seq: u64,
    text: Option<&str>,
    calls: Vec<ToolCallRequest>,
    blocks: Option<serde_json::Value>,
) -> Event {
    event(
        seq,
        run,
        EventPayload::MessageAssistant(MessageAssistant {
            round: seq as u32,
            text: text.map(str::to_string),
            text_ref: None,
            tool_calls: calls,
            provider_blocks: blocks,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
        }),
    )
}

fn request(call: &ToolCallId) -> ToolCallRequest {
    ToolCallRequest {
        call_id: call.clone(),
        provider_call_id: "pc-1".into(),
        name: "read".into(),
        arguments: serde_json::json!({"path": "a.txt"}),
        arguments_ref: None,
    }
}

/// 正文全都内联：`entries` 选中的那几条直接从 `SurfaceMessage` 上抄正文，不必真的读
/// 任何东西——用来测 `entries` 与 `to_replay_messages` 的这几个测试都不外置正文。
fn resolve_inline(entries: Vec<Entry<'_>>) -> Vec<ResolvedMessage<'_>> {
    entries
        .into_iter()
        .map(|entry| ResolvedMessage {
            text: entry.message.text.clone(),
            outputs: Vec::new(),
            entry,
        })
        .collect()
}

/// 上一轮说了什么、最后答了什么，下一轮必须还在；历史 Run 里那轮工具往返不该跟着进来。
#[test]
fn a_later_run_still_reads_what_the_earlier_one_said() {
    let first = RunId::from_raw("run-1");
    let second = RunId::from_raw("run-2");
    let call = ToolCallId::from_raw("call-1");
    let events = vec![
        accepted(&first, 1, "捞一下线上订单在 loong 请求了哪些接口，先查 db"),
        started(&first, 2),
        // 一轮工具往返：**历史 Run 里它不该再进新请求**——协议只属于正跑的那条 Run。
        assistant(
            &first,
            3,
            None,
            vec![request(&call)],
            Some(serde_json::json!([{"type": "reasoning"}])),
        ),
        assistant(
            &first,
            4,
            Some("SQL 如下：SELECT 1。确认执行吗？"),
            vec![],
            Some(serde_json::json!([{"type": "message"}])),
        ),
        completed(&first, 5),
        accepted(&second, 6, "去查一下"),
        started(&second, 7),
    ];
    let surface = fold(&events);
    let resolved = resolve_inline(entries(&surface, ReplayScope::Conversation(&second)));
    let messages = to_replay_messages(resolved, &BUDGET);

    let seen: Vec<(Role, Option<&str>)> = messages
        .iter()
        .map(|message| (message.role, message.text.as_deref()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (
                Role::User,
                Some("捞一下线上订单在 loong 请求了哪些接口，先查 db")
            ),
            (Role::Assistant, Some("SQL 如下：SELECT 1。确认执行吗？")),
            (Role::User, Some("去查一下")),
        ],
        "上一轮的问答要跟着进新 Run，而它那轮工具往返不进"
    );
    for message in &messages {
        assert!(message.tool_calls.is_empty(), "{message:?}");
        assert!(message.tool_results.is_empty(), "{message:?}");
        assert!(message.provider_blocks.is_none(), "{message:?}");
    }
}

/// 正跑着的那条 Run 仍然是**完整协议**：它自己那轮的调用、结果与原生块一个都不能少。
#[test]
fn the_running_run_keeps_its_whole_protocol() {
    let run = RunId::from_raw("run-1");
    let call = ToolCallId::from_raw("call-1");
    let events = vec![
        accepted(&run, 1, "看一下 a.txt"),
        started(&run, 2),
        assistant(
            &run,
            3,
            None,
            vec![request(&call)],
            Some(serde_json::json!([{"type": "reasoning"}])),
        ),
    ];
    let surface = fold(&events);
    let resolved = resolve_inline(entries(&surface, ReplayScope::Conversation(&run)));
    let messages = to_replay_messages(resolved, &BUDGET);

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].tool_calls.len(), 1);
    assert_eq!(
        messages[1].tool_calls[0].call_id, call,
        "调用要原样交给 provider"
    );
    assert_eq!(
        messages[1].provider_blocks,
        Some(serde_json::json!([{"type": "reasoning"}])),
        "原生块逐字回放（§13.2）"
    );
}

/// 子代理只看得见自己那条 Run，父的窗口里也没有它的过程（§4）。
#[test]
fn a_subagent_and_its_parent_are_two_different_conversations() {
    let parent = RunId::from_raw("run-parent");
    let child = RunId::from_raw("run-child");
    let call = ToolCallId::from_raw("call-1");
    let spec = DelegateSpec::new(parent.clone(), call.clone(), "看一下这个 PR");
    let events = vec![
        accepted(&parent, 1, "父的输入"),
        started(&parent, 2),
        accepted_as(&child, 3, Some("看一下这个 PR"), Some(spec)),
        started(&child, 4),
        assistant(&child, 5, Some("子代理的过程"), vec![], None),
        assistant(&parent, 6, Some("父的回复"), vec![], None),
    ];
    let surface = fold(&events);

    let of = |scope| {
        to_replay_messages(resolve_inline(entries(&surface, scope)), &BUDGET)
            .into_iter()
            .map(|message| message.text)
            .collect::<Vec<_>>()
    };

    assert_eq!(
        of(ReplayScope::Conversation(&parent)),
        vec![Some("父的输入".into()), Some("父的回复".into())],
        "父的窗口里不该有子代理的过程——它只该拿到那条结果（§4）"
    );
    assert_eq!(
        of(ReplayScope::Thread(std::slice::from_ref(&child))),
        vec![Some("看一下这个 PR".into()), Some("子代理的过程".into())],
        "子代理拿不到父的对话历史"
    );
}

/// 产物在**回放那一侧**同样印出来：入口与大小从"读回来的" `output.json` 里来（事件里
/// 没有那一格），所以"刚跑完"与"重启之后回放"给模型的是同一份入口清单（§4.7、§8.3）。
/// 这份"读回来的" `StoredOutput` 是手喂的——真的去 `ToolOutputStore` 读它是 Gateway 的事。
#[test]
fn a_replayed_round_names_the_artifacts_it_produced() {
    let run = RunId::from_raw("run-1");
    let call = ToolCallId::from_raw("call-1");
    let events = vec![
        accepted(&run, 1, "写一份报告"),
        started(&run, 2),
        assistant(&run, 3, None, vec![request(&call)], None),
        event(
            4,
            &run,
            EventPayload::ToolResult(komo_kernel::events::ToolResult {
                call_id: call.clone(),
                attempt_id: AttemptId::from_raw("attempt-1"),
                status: ToolResultStatus::Completed,
                output_ref: OutputRef(ContentRef {
                    path: "tool-output/run-1/call-1/attempt-1/output.json".into(),
                    size: 0,
                    hash: ContentHash::of_str(""),
                    pointer: None,
                }),
                elapsed_ms: 1200,
                preview: Some("账本里的那句预览（读不回 output.json 时才用它）".into()),
                stdout: None,
                stderr: None,
                attempt_state: None,
            }),
        ),
    ];
    let surface = fold(&events);
    let artifacts = vec![ContentRef {
        path: "artifacts/run-1/报告.md".into(),
        size: "产物正文\n".len() as u64,
        hash: ContentHash::of_str("产物正文\n"),
        pointer: None,
    }];
    let resolved: Vec<ResolvedMessage<'_>> = entries(&surface, ReplayScope::Conversation(&run))
        .into_iter()
        .map(|entry| {
            let outputs =
                if entry.kind == EntryKind::Protocol && !entry.message.tool_results.is_empty() {
                    vec![Some(StoredOutput {
                        preview: Some("写完了一份报告\n".into()),
                        artifacts: artifacts.clone(),
                    })]
                } else {
                    Vec::new()
                };
            ResolvedMessage {
                text: entry.message.text.clone(),
                outputs,
                entry,
            }
        })
        .collect();
    let messages = to_replay_messages(resolved, &BUDGET);

    let content = &messages
        .iter()
        .flat_map(|message| message.tool_results.iter())
        .next()
        .expect("回放里有工具结果")
        .content;
    assert!(content.contains("写完了一份报告"), "{content}");
    assert!(
        content.contains("产物：artifact://files/run-1/报告.md（13 B）"),
        "{content}"
    );
    assert!(
        !content.contains("产物正文"),
        "产物的正文按引用去读，回放也不抄：{content}"
    );
}

fn thread_texts(surface: &Surface, chain: &[RunId]) -> Vec<ReplayMessage> {
    to_replay_messages(
        resolve_inline(entries(surface, ReplayScope::Thread(chain))),
        &BUDGET,
    )
}

/// 续跑（`docs/komo_bot.md` §4）：正在跑的这一条子 Run 的窗口里有上一环的任务正文与最后
/// 的回答，**没有**上一环的 `tool_calls` / `provider_blocks`，也没有父对话的任何一句。
#[test]
fn a_resumed_link_sees_the_earlier_links_task_and_answer_but_not_its_process() {
    let parent = RunId::from_raw("run-parent");
    let first = RunId::from_raw("run-child-1");
    let second = RunId::from_raw("run-child-2");
    let first_spec = DelegateSpec::new(parent.clone(), ToolCallId::from_raw("call-1"), "查 A");
    let second_spec = DelegateSpec::new(parent.clone(), ToolCallId::from_raw("call-2"), "接着查 B")
        .with_resumes(first.clone());

    let events = vec![
        accepted(&parent, 1, "父的输入"),
        started(&parent, 2),
        accepted_as(&first, 3, Some("查 A"), Some(first_spec)),
        started(&first, 4),
        // 第一环的过程：一次工具往返，**不该进第二环的窗口**。
        assistant(
            &first,
            5,
            None,
            vec![request(&ToolCallId::from_raw("call-inner"))],
            Some(serde_json::json!([{"type": "reasoning"}])),
        ),
        assistant(&first, 6, Some("A 查完了"), vec![], None),
        completed(&first, 7),
        accepted_as(&second, 8, Some("接着查 B"), Some(second_spec)),
        started(&second, 9),
    ];
    let surface = fold(&events);
    let chain = delegate_thread(&surface, &second);
    assert_eq!(
        chain,
        vec![first.clone(), second.clone()],
        "旧→新，含正在跑的这一条"
    );

    let messages = thread_texts(&surface, &chain);
    let seen: Vec<(Role, Option<&str>)> = messages
        .iter()
        .map(|message| (message.role, message.text.as_deref()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (Role::User, Some("查 A")),
            (Role::Assistant, Some("A 查完了")),
            (Role::User, Some("接着查 B")),
        ],
        "上一环只发布任务正文与最后的回答，父对话一句都不该在里面"
    );
    for message in &messages[..2] {
        assert!(message.tool_calls.is_empty(), "{message:?}");
        assert!(message.provider_blocks.is_none(), "{message:?}");
    }
}

/// 续跑一条 `failed` 的上一环：它没有最终回答，窗口里只剩任务正文，外加一句它怎么
/// 结束的（§4、§8.3）。
#[test]
fn a_failed_link_gets_one_sentence_about_how_it_ended_instead_of_a_final_answer() {
    let parent = RunId::from_raw("run-parent");
    let first = RunId::from_raw("run-child-1");
    let second = RunId::from_raw("run-child-2");
    let first_spec = DelegateSpec::new(parent.clone(), ToolCallId::from_raw("call-1"), "查 A");
    let second_spec = DelegateSpec::new(parent.clone(), ToolCallId::from_raw("call-2"), "接着查 B")
        .with_resumes(first.clone());

    let events = vec![
        accepted_as(&first, 1, Some("查 A"), Some(first_spec)),
        started(&first, 2),
        event(
            3,
            &first,
            EventPayload::RunFailed(komo_kernel::events::RunFailed {
                reason: "工具连续失败".into(),
            }),
        ),
        accepted_as(&second, 4, Some("接着查 B"), Some(second_spec)),
        started(&second, 5),
    ];
    let surface = fold(&events);
    let chain = delegate_thread(&surface, &second);

    let messages = thread_texts(&surface, &chain);
    let seen: Vec<(Role, Option<&str>)> = messages
        .iter()
        .map(|message| (message.role, message.text.as_deref()))
        .collect();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert_eq!(seen[0], (Role::User, Some("查 A")));
    let (role, text) = seen[1];
    assert_eq!(role, Role::Assistant);
    let text = text.expect("有一句它怎么结束的");
    assert!(text.contains("失败了"), "{text}");
    assert!(text.contains("工具连续失败"), "{text}");
    assert_eq!(seen[2], (Role::User, Some("接着查 B")));
}

/// 重启在续跑子 Run 跑到一半时，照 §8.4 接着跑：窗口与重启前逐字节相同——`fold` 与
/// `entries` / `to_replay_messages` 都是纯函数，同一份事件折两遍必须给出同一份正文。
#[test]
fn the_thread_window_is_byte_identical_before_and_after_a_restart() {
    let parent = RunId::from_raw("run-parent");
    let first = RunId::from_raw("run-child-1");
    let second = RunId::from_raw("run-child-2");
    let first_spec = DelegateSpec::new(parent.clone(), ToolCallId::from_raw("call-1"), "查 A");
    let second_spec = DelegateSpec::new(parent.clone(), ToolCallId::from_raw("call-2"), "接着查 B")
        .with_resumes(first.clone());

    let events = vec![
        accepted(&parent, 1, "父的输入"),
        started(&parent, 2),
        accepted_as(&first, 3, Some("查 A"), Some(first_spec)),
        started(&first, 4),
        assistant(&first, 5, Some("A 查完了"), vec![], None),
        completed(&first, 6),
        accepted_as(&second, 7, Some("接着查 B"), Some(second_spec)),
        started(&second, 8),
        // 正在跑的这一条**跑到一半**：一次工具调用发出去了，还没有结果。
        assistant(
            &second,
            9,
            None,
            vec![request(&ToolCallId::from_raw("call-inner"))],
            Some(serde_json::json!([{"type": "reasoning"}])),
        ),
    ];

    let replay_once = || {
        let surface = fold(&events);
        let chain = delegate_thread(&surface, &second);
        thread_texts(&surface, &chain)
    };
    let before = replay_once();
    let after = replay_once();

    assert_eq!(before, after);
    assert_eq!(
        serde_json::to_string(&before).unwrap(),
        serde_json::to_string(&after).unwrap(),
        "窗口逐字节相同"
    );
    let running = after.last().expect("至少有正在跑的这一条");
    assert_eq!(running.tool_calls.len(), 1, "正在跑的这一条带着完整协议");
    assert!(running.provider_blocks.is_some());
}

const DECAYING: ProjectionContext = ProjectionContext {
    model_result_bytes: 8 * 1024,
    decay: Some(komo_kernel::projection::DecayPolicy {
        threshold_bytes: 4096,
        full_sends: 2,
        head_bytes: 2048,
        tail_bytes: 1536,
    }),
};

/// 正在跑的 Run：一次 `read` 拿回 20 KB，之后又记了 `later` 条 `message.assistant`。
/// 返回回放出来的那条结果。
fn replayed_after(later: u64) -> ToolResultForModel {
    let run = RunId::from_raw("run-1");
    let call = ToolCallId::from_raw("call-1");
    let mut events = vec![
        accepted(&run, 1, "读一遍"),
        started(&run, 2),
        assistant(&run, 3, None, vec![request(&call)], None),
        event(
            4,
            &run,
            EventPayload::ToolResult(komo_kernel::events::ToolResult {
                call_id: call.clone(),
                attempt_id: AttemptId::from_raw("attempt-1"),
                status: ToolResultStatus::Completed,
                output_ref: OutputRef(ContentRef {
                    path: "tool-output/run-1/call-1/attempt-1/output.json".into(),
                    size: 0,
                    hash: ContentHash::of_str(""),
                    pointer: None,
                }),
                elapsed_ms: 30,
                preview: None,
                stdout: None,
                stderr: None,
                attempt_state: None,
            }),
        ),
    ];
    for seq in 5..5 + later {
        events.push(assistant(&run, seq, Some("再看看"), vec![], None));
    }
    let surface = fold(&events);
    let big: String = (0..400)
        .map(|n| format!("line {n:04}: {}\n", "x".repeat(40)))
        .collect();
    let resolved: Vec<ResolvedMessage<'_>> = entries(&surface, ReplayScope::Conversation(&run))
        .into_iter()
        .map(|entry| {
            let outputs = if entry.message.tool_results.is_empty() {
                Vec::new()
            } else {
                vec![Some(StoredOutput {
                    preview: Some(big.clone()),
                    artifacts: Vec::new(),
                })]
            };
            ResolvedMessage {
                text: entry.message.text.clone(),
                outputs,
                entry,
            }
        })
        .collect();
    to_replay_messages(resolved, &DECAYING)
        .into_iter()
        .flat_map(|message| message.tool_results)
        .next()
        .expect("回放里有那条结果")
}

/// 它之后已经有两次请求带着它完整发出去了（两条 `message.assistant`）：回放直接给短视图，
/// 不再交给 loop 去换（§8.3）。
#[test]
fn a_result_two_rounds_old_replays_its_decayed_view() {
    let full = replayed_after(0).content;
    let result = replayed_after(2);
    assert!(result.decay.is_none(), "{:?}", result.decay);
    assert!(
        result.content.contains("这份结果已完整给过 2 次"),
        "{}",
        result.content
    );
    assert!(result.content.len() + 1024 <= full.len());
    assert_eq!(
        replayed_after(5).content,
        result.content,
        "之后一直是同一份"
    );
}

/// 只完整给过一次：回放仍是完整视图，并告诉 loop 还剩一次、之后换成哪份。
#[test]
fn a_result_one_round_old_replays_whole_with_one_full_send_left() {
    let fresh = replayed_after(0);
    assert_eq!(
        fresh.decay.as_ref().map(|decay| decay.remaining_full_sends),
        Some(2)
    );

    let result = replayed_after(1);
    assert_eq!(result.content, fresh.content, "完整视图不随次数变");
    let decay = result.decay.expect("还在完整期");
    assert_eq!(decay.remaining_full_sends, 1);
    assert_eq!(
        decay.view,
        replayed_after(2).content,
        "到点换成的就是回放会给的那份"
    );
}

fn result(run: &RunId, seq: u64, call: &ToolCallId, preview: &str) -> Event {
    event(
        seq,
        run,
        EventPayload::ToolResult(komo_kernel::events::ToolResult {
            call_id: call.clone(),
            attempt_id: AttemptId::from_raw(format!("attempt-{seq}")),
            status: ToolResultStatus::Completed,
            output_ref: OutputRef(ContentRef {
                path: format!("tool-output/{run}/{call}/attempt-{seq}/output.json"),
                size: 0,
                hash: ContentHash::of_str(""),
                pointer: None,
            }),
            elapsed_ms: 5,
            preview: Some(preview.to_string()),
            stdout: None,
            stderr: None,
            attempt_state: None,
        }),
    )
}

fn compaction(run: &RunId, seq: u64, first_kept: u64, summary: &str) -> Event {
    event(
        seq,
        run,
        EventPayload::ContextCompacted(komo_kernel::test_support::compacted(
            Seq(first_kept),
            summary,
        )),
    )
}

fn calling(id: &str) -> ToolCallRequest {
    ToolCallRequest {
        provider_call_id: format!("pc-{id}"),
        ..request(&ToolCallId::from_raw(id))
    }
}

/// 一条历史 Run，然后正在跑的这条做了三轮工具往返。
fn three_rounds(run: &RunId) -> Vec<Event> {
    let earlier = RunId::from_raw("run-0");
    let (c1, c2, c3) = (
        ToolCallId::from_raw("c1"),
        ToolCallId::from_raw("c2"),
        ToolCallId::from_raw("c3"),
    );
    vec![
        accepted(&earlier, 1, "上一件事"),
        started(&earlier, 2),
        assistant(&earlier, 3, Some("上一件事的回答"), vec![], None),
        completed(&earlier, 4),
        accepted(run, 5, "这次的任务"),
        started(run, 6),
        assistant(run, 7, Some("先看 a"), vec![calling("c1")], None),
        result(run, 8, &c1, "a 的内容"),
        assistant(run, 9, Some("再看 b"), vec![calling("c2")], None),
        result(run, 10, &c2, "b 的内容"),
        assistant(run, 11, Some("最后看 c"), vec![calling("c3")], None),
        result(run, 12, &c3, "c 的内容"),
    ]
}

/// 同 [`resolve_inline`]，但每条结果都"读不回 `output.json`"，退回账本里的预览。
fn replay(events: &[Event], run: &RunId, projection: &ProjectionContext) -> Vec<ReplayMessage> {
    let surface = fold(events);
    let resolved = entries(&surface, ReplayScope::Conversation(run))
        .into_iter()
        .map(|entry| ResolvedMessage {
            text: entry.message.text.clone(),
            outputs: vec![None; entry.message.tool_results.len()],
            entry,
        })
        .collect();
    to_replay_messages(resolved, projection)
}

fn texts(messages: &[ReplayMessage]) -> Vec<(Role, Option<String>)> {
    messages
        .iter()
        .map(|message| (message.role, message.text.clone()))
        .collect()
}

/// 压过之后：历史 Run 照旧 → 这条 Run 开头那句任务 → 摘要 → 从 `first_kept` 起原样。
#[test]
fn a_compacted_run_replays_task_then_summary_then_tail() {
    let run = RunId::from_raw("run-1");
    let mut events = three_rounds(&run);
    events.push(compaction(&run, 13, 11, "看过了 a 和 b，结论是……"));
    let messages = replay(&events, &run, &BUDGET);

    assert_eq!(
        texts(&messages),
        vec![
            (Role::User, Some("上一件事".into())),
            (Role::Assistant, Some("上一件事的回答".into())),
            (Role::User, Some("这次的任务".into())),
            (
                Role::User,
                Some(format!("{COMPACTION_PREFIX}\n\n看过了 a 和 b，结论是……"))
            ),
            (Role::Assistant, Some("最后看 c".into())),
            (Role::Tool, None),
        ]
    );
    assert_eq!(messages[4].tool_calls[0].provider_call_id, "pc-c3");
    let results: Vec<_> = messages
        .iter()
        .flat_map(|message| message.tool_results.iter())
        .map(|result| result.provider_call_id.as_str())
        .collect();
    assert_eq!(results, vec!["pc-c3"], "被摘要覆盖的调用与结果一起走");
    assert!(COMPACTION_PREFIX.contains("artifact://"));

    // 同一份日志折两遍：逐字节相同（重启前后）。
    assert_eq!(
        serde_json::to_string(&messages).unwrap(),
        serde_json::to_string(&replay(&events, &run, &BUDGET)).unwrap()
    );
}

/// 切点不在这条 Run 某一轮的开头（落在结果上、落在别的 Run 上、落在压缩之后）：不认，
/// 回放完整历史。
#[test]
fn a_cut_not_at_a_round_start_is_ignored_and_the_full_history_replays() {
    let run = RunId::from_raw("run-1");
    let full = replay(&three_rounds(&run), &run, &BUDGET);
    for first_kept in [10, 3, 5, 14] {
        let mut events = three_rounds(&run);
        events.push(compaction(&run, 13, first_kept, "不该出现"));
        assert_eq!(
            replay(&events, &run, &BUDGET),
            full,
            "first_kept = {first_kept}"
        );
    }
}

/// 子代理压缩自己那条线上的窗口，规则一样。
#[test]
fn a_compacted_child_replays_its_own_task_then_summary_then_tail() {
    let parent = RunId::from_raw("run-parent");
    let child = RunId::from_raw("run-child");
    let spec = DelegateSpec::new(parent.clone(), ToolCallId::from_raw("call-1"), "查 A");
    let c1 = ToolCallId::from_raw("k1");
    let events = vec![
        accepted(&parent, 1, "父的输入"),
        started(&parent, 2),
        accepted_as(&child, 3, Some("查 A"), Some(spec)),
        started(&child, 4),
        assistant(&child, 5, Some("先查"), vec![calling("k1")], None),
        result(&child, 6, &c1, "查到了"),
        assistant(&child, 7, Some("再确认"), vec![], None),
        compaction(&child, 8, 7, "查过一轮"),
    ];
    let surface = fold(&events);
    let chain = delegate_thread(&surface, &child);
    assert_eq!(
        texts(&thread_texts(&surface, &chain)),
        vec![
            (Role::User, Some("查 A".into())),
            (Role::User, Some(format!("{COMPACTION_PREFIX}\n\n查过一轮"))),
            (Role::Assistant, Some("再确认".into())),
        ]
    );
}

/// 留下来的大结果"给过几次"不随压缩变：它之后的 `message.assistant` 全在尾巴里。
#[test]
fn the_decay_count_on_the_kept_tail_is_unchanged() {
    let run = RunId::from_raw("run-1");
    let big: String = (0..400)
        .map(|n| format!("line {n:04}: {}\n", "x".repeat(40)))
        .collect();
    let replay_big = |events: &[Event]| {
        let surface = fold(events);
        let resolved: Vec<ResolvedMessage<'_>> = entries(&surface, ReplayScope::Conversation(&run))
            .into_iter()
            .map(|entry| {
                let outputs = entry
                    .message
                    .tool_results
                    .iter()
                    .map(|result| {
                        Some(StoredOutput {
                            preview: Some(match result.call.as_str() {
                                "c3" => big.clone(),
                                _ => "小".into(),
                            }),
                            artifacts: Vec::new(),
                        })
                    })
                    .collect();
                ResolvedMessage {
                    text: entry.message.text.clone(),
                    outputs,
                    entry,
                }
            })
            .collect();
        to_replay_messages(resolved, &DECAYING)
            .into_iter()
            .flat_map(|message| message.tool_results)
            .find(|result| result.provider_call_id == "pc-c3")
            .expect("c3 的结果在尾巴里")
    };

    for later in 0..4u64 {
        let mut plain = three_rounds(&run);
        for seq in 13..13 + later {
            plain.push(assistant(&run, seq, Some("再看看"), vec![], None));
        }
        let mut compacted = plain.clone();
        compacted.push(compaction(&run, 13 + later, 9, "看过了 a"));
        assert!(
            replay(&compacted, &run, &DECAYING)
                .iter()
                .all(|message| message
                    .tool_calls
                    .iter()
                    .all(|call| call.provider_call_id != "pc-c1")),
            "压缩真的生效了"
        );
        assert_eq!(
            replay_big(&compacted),
            replay_big(&plain),
            "later = {later}"
        );
    }
}
