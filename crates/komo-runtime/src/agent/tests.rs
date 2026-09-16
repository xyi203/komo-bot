//! Agent Loop 的验收（§14 阶段 2）。

use std::sync::Arc;

use komo_kernel::fold::Surface;
use komo_kernel::test_support::{ScriptedLlm, sample_model};
use komo_kernel::traits::ApprovalRepo;
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::status::{RunStatus, Wait};
use komo_kernel::types::tool::{CancelToken, ToolError, ToolOutput};
use komo_kernel::types::turn::{ProviderToolCall, Role, Round, TurnRequest};

use super::{AgentLoop, Budget, Segment, SegmentOutcome};
use crate::executor::harness::{Harness, RecordingTool};

fn round(number: u32, text: Option<&str>, calls: Vec<ProviderToolCall>) -> Round {
    Round {
        round: number,
        text: text.map(str::to_string),
        tool_calls: calls,
        provider_blocks: None,
        usage: Default::default(),
        truncated: false,
    }
}

fn call(id: &str, name: &str, args: serde_json::Value) -> ProviderToolCall {
    ProviderToolCall {
        provider_call_id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn turn_request(session: &SessionId, run: &komo_kernel::types::ids::RunId) -> TurnRequest {
    TurnRequest {
        session: session.clone(),
        run: run.clone(),
        model: sample_model(),
        system_prompt: "你是 komo".into(),
        messages: vec![],
        tools: vec![],
        memories: vec![],
        covers: None,
    }
}

struct Wired {
    harness: Harness,
    agent: AgentLoop,
}

impl Wired {
    fn new(
        scripts: Vec<Vec<Round>>,
        tools: Vec<Arc<dyn komo_kernel::traits::Tool>>,
        permissive: bool,
    ) -> Self {
        let harness = Harness::new();
        let executor = if permissive {
            harness.permissive(tools)
        } else {
            harness.initial(tools)
        };
        let agent = AgentLoop::new(
            Arc::new(ScriptedLlm::new(scripts)),
            harness.ledger.clone(),
            executor,
            Arc::new(harness.clock.clone()),
        );
        Self { harness, agent }
    }

    async fn segment(&self, cancel: CancelToken) -> Segment {
        let (session, run) = self.harness.open_run().await;
        Segment {
            request: turn_request(&session, &run),
            env: self.harness.env_with_cancel(&session, &run, cancel),
            budget: Budget::default(),
            resume: None,
            session,
            run,
        }
    }
}

fn surface(harness: &Harness) -> Surface {
    harness.ledger.surface()
}

/// ⑨ 先 `record_round` 再 `complete`：最终回复必须留在消息面上。
#[tokio::test]
async fn the_final_reply_is_recorded_before_the_run_completes() {
    let wired = Wired::new(vec![vec![round(1, Some("写好了。"), vec![])]], vec![], true);
    let segment = wired.segment(CancelToken::new()).await;
    let outcome = wired.agent.run(segment).await.unwrap();

    assert!(
        matches!(&outcome, SegmentOutcome::Completed { final_message, .. }
            if final_message.as_deref() == Some("写好了。")),
        "{outcome:?}"
    );

    let surface = surface(&wired.harness);
    let last = surface.messages.last().expect("消息面非空");
    assert_eq!(last.role, Role::Assistant);
    assert_eq!(last.text.as_deref(), Some("写好了。"));
    let run = surface.runs.values().next().unwrap();
    assert_eq!(run.status, RunStatus::Completed);

    // 顺序也要对：assistant 消息在终态事件之前。
    let events = wired.harness.ledger.events();
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event.payload.type_name())
        .collect();
    let assistant = kinds
        .iter()
        .position(|k| *k == "message.assistant")
        .unwrap();
    let completed = kinds.iter().position(|k| *k == "run.completed").unwrap();
    assert!(assistant < completed, "{kinds:?}");
}

/// 一轮有调用 → 结果按 `call_id` 回传 → 下一轮模型 → 结束。
#[tokio::test]
async fn tool_results_go_back_to_the_model_keyed_by_call_id() {
    let tool = Arc::new(RecordingTool::shell());
    let wired = Wired::new(
        vec![vec![
            round(1, None, vec![call("pc-1", "shell", serde_json::json!({}))]),
            round(2, Some("跑完了。"), vec![]),
        ]],
        vec![tool.clone()],
        true,
    );
    let segment = wired.segment(CancelToken::new()).await;
    let outcome = wired.agent.run(segment).await.unwrap();

    assert!(
        matches!(outcome, SegmentOutcome::Completed { rounds: 2, .. }),
        "{outcome:?}"
    );
    assert_eq!(tool.ran(), 1);

    let surface = surface(&wired.harness);
    // 消息面：assistant（带调用）→ tool（结果）→ assistant（最终回复）。
    let roles: Vec<Role> = surface.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    let tool_node = &surface.messages[2];
    assert_eq!(tool_node.tool_results.len(), 1);
    assert_eq!(
        tool_node.tool_results[0].status,
        ToolResultStatus::Completed
    );
}

/// ① `Ask` 之后 Run 让出名额——**不在进程里等人**。
#[tokio::test]
async fn an_ask_suspends_the_run_instead_of_waiting_in_process() {
    let tool = Arc::new(RecordingTool::shell());
    let wired = Wired::new(
        vec![vec![round(
            1,
            None,
            vec![call(
                "pc-1",
                "shell",
                serde_json::json!({ "command": "rm -rf /tmp/x" }),
            )],
        )]],
        vec![tool.clone()],
        false,
    );
    let segment = wired.segment(CancelToken::new()).await;
    let outcome = wired.agent.run(segment).await.unwrap();

    let SegmentOutcome::Suspended { wait, .. } = &outcome else {
        panic!("{outcome:?}")
    };
    assert!(matches!(wait, Wait::Approval { .. }), "{wait:?}");
    assert_eq!(tool.ran(), 0, "批准之前不执行");

    let surface = surface(&wired.harness);
    let run = surface.runs.values().next().unwrap();
    assert_eq!(run.status, RunStatus::WaitingApproval);
    // 待处理的审批在数据库里，**不是**折出来的：JSONL 里的审批事件是审计补写
    // （§8.5 的反向顺序），它不创建授权，也不由 executor 写。
    let pending = wired.harness.approvals.list_pending(None).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].plan.tool, "shell");
}

/// ⑧ 工具失败作为结果回给模型，模型继续；Run 正常结束。
#[tokio::test]
async fn a_tool_failure_is_handed_to_the_model_and_the_run_carries_on() {
    let tool = Arc::new(RecordingTool::shell().with_outcome(Err(ToolError::Failed {
        message: "命令不存在".into(),
    })));
    let llm = Arc::new(ScriptedLlm::once(vec![
        round(1, None, vec![call("pc-1", "shell", serde_json::json!({}))]),
        round(2, Some("换个办法。"), vec![]),
    ]));
    let harness = Harness::new();
    let executor = harness.permissive(vec![tool.clone()]);
    let agent = AgentLoop::new(
        llm.clone(),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let outcome = agent
        .run(Segment {
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget::default(),
            resume: None,
            session,
            run,
        })
        .await
        .unwrap();

    assert!(
        matches!(outcome, SegmentOutcome::Completed { .. }),
        "{outcome:?}"
    );
    // 第二轮的输入里带着那条失败结果。
    let requests = llm.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "一段只 begin_turn 一次");
    drop(requests);
    let surface = harness.ledger.surface();
    let tool_node = surface
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("有工具结果节点");
    assert_eq!(tool_node.tool_results[0].status, ToolResultStatus::Failed);
}

/// ⑧（另一半）驱动 / LLM 错误中止 Run。
#[tokio::test]
async fn a_driver_error_fails_the_run() {
    // 脚本只有一轮，第二轮一开口就是 `Unknown("脚本已经演完了")`。
    let tool = Arc::new(RecordingTool::shell());
    let wired = Wired::new(
        vec![vec![round(
            1,
            None,
            vec![call("pc-1", "shell", serde_json::json!({}))],
        )]],
        vec![tool],
        true,
    );
    let segment = wired.segment(CancelToken::new()).await;
    let outcome = wired.agent.run(segment).await.unwrap();

    assert!(
        matches!(outcome, SegmentOutcome::Failed { .. }),
        "{outcome:?}"
    );
    let surface = surface(&wired.harness);
    assert_eq!(
        surface.runs.values().next().unwrap().status,
        RunStatus::Failed
    );
}

/// 模型回复被截断时不能开始执行（§6）。
#[tokio::test]
async fn a_truncated_reply_never_starts_a_call() {
    let tool = Arc::new(RecordingTool::shell());
    let mut truncated = round(1, None, vec![call("pc-1", "shell", serde_json::json!({}))]);
    truncated.truncated = true;
    let wired = Wired::new(vec![vec![truncated]], vec![tool.clone()], true);
    let segment = wired.segment(CancelToken::new()).await;
    let outcome = wired.agent.run(segment).await.unwrap();

    assert!(
        matches!(outcome, SegmentOutcome::Failed { .. }),
        "{outcome:?}"
    );
    assert_eq!(tool.ran(), 0);
}

/// 总轮数预算用完 → failed，不是无限循环（§8.4）。
#[tokio::test]
async fn the_round_budget_ends_in_a_failure_not_a_loop() {
    let tool = Arc::new(RecordingTool::shell());
    let script: Vec<Round> = (1..=5)
        .map(|n| round(n, None, vec![call("pc-1", "shell", serde_json::json!({}))]))
        .collect();
    let harness = Harness::new();
    let executor = harness.permissive(vec![tool.clone()]);
    let agent = AgentLoop::new(
        Arc::new(ScriptedLlm::once(script)),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let outcome = agent
        .run(Segment {
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget {
                max_rounds: 2,
                ..Budget::default()
            },
            resume: None,
            session,
            run,
        })
        .await
        .unwrap();

    let SegmentOutcome::Failed { reason, rounds } = &outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(*rounds, 2);
    assert!(reason.contains("总轮数预算"), "{reason}");
    assert_eq!(tool.ran(), 2);
}

/// 取消：Run 明确终止在 cancelled，不留一个跑着的调用。
#[tokio::test]
async fn cancelling_ends_the_run_as_cancelled() {
    let tool = Arc::new(RecordingTool::shell());
    let wired = Wired::new(
        vec![vec![round(
            1,
            None,
            vec![call("pc-1", "shell", serde_json::json!({}))],
        )]],
        vec![tool.clone()],
        true,
    );
    let cancel = CancelToken::new();
    cancel.cancel();
    let segment = wired.segment(cancel).await;
    let outcome = wired.agent.run(segment).await.unwrap();

    assert!(
        matches!(outcome, SegmentOutcome::Cancelled { .. }),
        "{outcome:?}"
    );
    assert_eq!(tool.ran(), 0);
    assert_eq!(
        surface(&wired.harness).runs.values().next().unwrap().status,
        RunStatus::Cancelled
    );
}

/// 续跑：把上一回合没收尾的调用跑完，再请求下一轮模型。
#[tokio::test]
async fn a_resumed_segment_finishes_the_round_it_came_back_to() {
    let tool = Arc::new(RecordingTool::shell());
    let harness = Harness::new();
    let executor = harness.permissive(vec![tool.clone()]);
    let agent = AgentLoop::new(
        Arc::new(ScriptedLlm::once(vec![round(
            2,
            Some("继续完成。"),
            vec![],
        )])),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let pending = harness
        .record_round(&run, &[("shell", serde_json::json!({ "command": "x" }))])
        .await;

    let outcome = agent
        .run(Segment {
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget {
                first_round: 2,
                ..Budget::default()
            },
            resume: Some(super::ResumedRound {
                settled: vec![],
                pending,
            }),
            session,
            run,
        })
        .await
        .unwrap();

    assert!(
        matches!(outcome, SegmentOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(tool.ran(), 1);
}

/// 续跑时停在审批上：这一段仍然只是让出名额。
#[tokio::test]
async fn a_resumed_segment_can_suspend_again() {
    let tool = Arc::new(RecordingTool::shell());
    let harness = Harness::new();
    let executor = harness.initial(vec![tool.clone()]);
    let agent = AgentLoop::new(
        Arc::new(ScriptedLlm::once(vec![round(
            2,
            Some("不会走到这里"),
            vec![],
        )])),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let pending = harness
        .record_round(
            &run,
            &[("shell", serde_json::json!({ "command": "rm -rf /tmp/x" }))],
        )
        .await;

    let outcome = agent
        .run(Segment {
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget::default(),
            resume: Some(super::ResumedRound {
                settled: vec![],
                pending,
            }),
            session,
            run,
        })
        .await
        .unwrap();

    assert!(
        matches!(
            &outcome,
            SegmentOutcome::Suspended {
                wait: Wait::Approval { .. },
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(tool.ran(), 0);
}

/// 结果不明 → 这一段停在 `needs_attention`，等人（§8.6）。
#[tokio::test]
async fn an_uncertain_call_suspends_the_run_for_a_human() {
    let tool = Arc::new(
        RecordingTool::shell().with_outcome(Err(ToolError::Uncertain {
            message: "响应丢了".into(),
        })),
    );
    let wired = Wired::new(
        vec![vec![round(
            1,
            None,
            vec![call("pc-1", "shell", serde_json::json!({}))],
        )]],
        vec![tool],
        true,
    );
    let segment = wired.segment(CancelToken::new()).await;
    let outcome = wired.agent.run(segment).await.unwrap();

    assert!(
        matches!(
            &outcome,
            SegmentOutcome::Suspended {
                wait: Wait::Attention { .. },
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        surface(&wired.harness).runs.values().next().unwrap().status,
        RunStatus::NeedsAttention
    );
}

/// 每个 Run 固定自己的模型配置快照，一段只 `begin_turn` 一次（§6）。
#[tokio::test]
async fn one_segment_begins_exactly_one_turn() {
    let llm = Arc::new(ScriptedLlm::once(vec![round(1, Some("好。"), vec![])]));
    let harness = Harness::new();
    let executor = harness.permissive(vec![]);
    let agent = AgentLoop::new(
        llm.clone(),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    agent
        .run(Segment {
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget::default(),
            resume: None,
            session: session.clone(),
            run: run.clone(),
        })
        .await
        .unwrap();

    let requests = llm.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].run, run);
    assert_eq!(requests[0].model, sample_model());
}

/// 工具结果按 `call_id` 配对回传给驱动——不是按顺序猜。
#[tokio::test]
async fn the_driver_is_handed_results_paired_by_provider_call_id() {
    let tool = Arc::new(RecordingTool::shell());
    let harness = Harness::new();
    let executor = harness.permissive(vec![tool]);
    let llm = Arc::new(ScriptedLlm::once(vec![
        round(
            1,
            None,
            vec![
                call("pc-a", "shell", serde_json::json!({ "command": "a" })),
                call("pc-b", "shell", serde_json::json!({ "command": "b" })),
            ],
        ),
        round(2, Some("完成。"), vec![]),
    ]));
    let agent = AgentLoop::new(
        llm,
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    agent
        .run(Segment {
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget::default(),
            resume: None,
            session,
            run,
        })
        .await
        .unwrap();

    let surface = harness.ledger.surface();
    let assistant = surface
        .messages
        .iter()
        .find(|message| !message.tool_calls.is_empty())
        .expect("有带调用的 assistant 消息");
    let provider_ids: Vec<&str> = assistant
        .tool_calls
        .iter()
        .map(|call| call.provider_call_id.as_str())
        .collect();
    assert_eq!(provider_ids, vec!["pc-a", "pc-b"]);
    // Runtime 自己发的 ID 与 provider 的另存，两两不同。
    let runtime_ids: Vec<&str> = assistant
        .tool_calls
        .iter()
        .map(|call| call.call_id.as_str())
        .collect();
    assert_ne!(runtime_ids[0], runtime_ids[1]);
}

#[tokio::test]
async fn a_tool_output_with_no_preview_still_produces_one() {
    let tool = Arc::new(RecordingTool::shell().with_outcome(Ok(ToolOutput {
        status: ToolResultStatus::Completed,
        result: serde_json::json!({ "value": 42 }),
        exit_code: None,
        artifacts: vec![],
        preview: None,
    })));
    let wired = Wired::new(
        vec![vec![
            round(1, None, vec![call("pc-1", "shell", serde_json::json!({}))]),
            round(2, Some("好。"), vec![]),
        ]],
        vec![tool],
        true,
    );
    let segment = wired.segment(CancelToken::new()).await;
    wired.agent.run(segment).await.unwrap();

    let surface = surface(&wired.harness);
    let preview = surface
        .messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .flat_map(|message| message.tool_results.clone())
        .find_map(|result| result.preview)
        .expect("有预览");
    assert!(preview.contains("42"), "{preview}");
}
