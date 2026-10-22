//! Agent Loop 的验收（§14 阶段 2）。

use std::sync::Arc;

use komo_kernel::fold::Surface;
use komo_kernel::test_support::{ScriptedLlm, sample_model};
use komo_kernel::traits::Clock;
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::status::{RetryCause, RunState, WaitReason};
use komo_kernel::types::tool::{CancelToken, ToolError, ToolOutput};
use komo_kernel::types::turn::{Role, Round};

use super::tests_support::{FailingLlm, PanickingLlm, call, round, turn_request};
use super::{AgentLoop, Budget, RetryBudget, Segment, SegmentOutcome};
use crate::executor::harness::{Harness, RecordingTool};

struct Wired {
    harness: Harness,
    agent: AgentLoop,
}

impl Wired {
    fn new(scripts: Vec<Vec<Round>>, tools: Vec<Arc<dyn komo_kernel::traits::Tool>>) -> Self {
        Self::with_llm(Arc::new(ScriptedLlm::new(scripts)), tools)
    }

    fn with_llm(
        llm: Arc<dyn komo_kernel::traits::LlmClient>,
        tools: Vec<Arc<dyn komo_kernel::traits::Tool>>,
    ) -> Self {
        let harness = Harness::new();
        let executor = harness.executor(tools);
        let agent = AgentLoop::new(
            llm,
            harness.ledger.clone(),
            executor,
            Arc::new(harness.clock.clone()),
        );
        Self { harness, agent }
    }

    async fn segment(&self, cancel: CancelToken) -> Segment {
        self.command_segment(cancel, None).await
    }

    async fn command_segment(
        &self,
        cancel: CancelToken,
        command: Option<super::command_driver::CommandSpec>,
    ) -> Segment {
        let (session, run) = self.harness.open_run().await;
        Segment {
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: self.harness.env_with_cancel(&session, &run, cancel),
            budget: Budget::default(),
            resume: None,
            command,
            compaction: None,
            compactor: None,
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
    let wired = Wired::new(vec![vec![round(1, Some("写好了。"), vec![])]], vec![]);
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
    assert_eq!(run.status, RunState::Completed);

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
    let tool = Arc::new(RecordingTool::bash());
    let wired = Wired::new(
        vec![vec![
            round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]),
            round(2, Some("跑完了。"), vec![]),
        ]],
        vec![tool.clone()],
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

/// 同步委派 2026-10-16 删掉：模型新发的 `delegate` 调用不再派子 Run，按"工具集里没有
/// 它"落一条失败结果交给模型，Run 接着往下走，不停在 `dependency` 上。
#[tokio::test]
async fn a_new_delegate_call_is_refused_and_the_run_carries_on() {
    let wired = Wired::new(
        vec![vec![
            round(
                1,
                None,
                vec![call(
                    "pc-1",
                    "delegate",
                    serde_json::json!({ "task": "去查一下这个接口的超时" }),
                )],
            ),
            round(2, Some("我自己查。"), vec![]),
        ]],
        vec![Arc::new(RecordingTool::bash())],
    );
    let segment = wired.segment(CancelToken::new()).await;
    let outcome = wired.agent.run(segment).await.unwrap();

    assert!(
        matches!(outcome, SegmentOutcome::Completed { rounds: 2, .. }),
        "{outcome:?}"
    );
    let surface = surface(&wired.harness);
    assert!(
        surface.runs.values().all(|view| view.delegate.is_none()),
        "没有派出子 Run"
    );
    let tool_node = surface
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("有工具结果节点");
    assert_eq!(tool_node.tool_results[0].status, ToolResultStatus::Failed);
}

/// ⑧ 工具失败作为结果回给模型，模型继续；Run 正常结束。
#[tokio::test]
async fn a_tool_failure_is_handed_to_the_model_and_the_run_carries_on() {
    let tool = Arc::new(RecordingTool::bash().with_outcome(Err(ToolError::Failed {
        message: "命令不存在".into(),
    })));
    let llm = Arc::new(ScriptedLlm::once(vec![
        round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]),
        round(2, Some("换个办法。"), vec![]),
    ]));
    let harness = Harness::new();
    let executor = harness.executor(vec![tool.clone()]);
    let agent = AgentLoop::new(
        llm.clone(),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let outcome = agent
        .run(Segment {
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget::default(),
            resume: None,
            command: None,
            compaction: None,
            compactor: None,
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
    let tool = Arc::new(RecordingTool::bash());
    let wired = Wired::new(
        vec![vec![round(
            1,
            None,
            vec![call("pc-1", "bash", serde_json::json!({}))],
        )]],
        vec![tool],
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
        RunState::Failed
    );
}

/// 模型回复被截断时不能开始执行（§6）。
#[tokio::test]
async fn a_truncated_reply_never_starts_a_call() {
    let tool = Arc::new(RecordingTool::bash());
    let mut truncated = round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]);
    truncated.truncated = true;
    let wired = Wired::new(vec![vec![truncated]], vec![tool.clone()]);
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
    let tool = Arc::new(RecordingTool::bash());
    let script: Vec<Round> = (1..=5)
        .map(|n| round(n, None, vec![call("pc-1", "bash", serde_json::json!({}))]))
        .collect();
    let harness = Harness::new();
    let executor = harness.executor(vec![tool.clone()]);
    let agent = AgentLoop::new(
        Arc::new(ScriptedLlm::once(script)),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let outcome = agent
        .run(Segment {
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget {
                max_rounds: Some(2),
                ..Budget::default()
            },
            resume: None,
            command: None,
            compaction: None,
            compactor: None,
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

/// 默认预算不设轮数上限：跑过以前的 100 轮也照样跑到模型自己收尾。
#[tokio::test]
async fn the_default_budget_has_no_round_limit() {
    let tool = Arc::new(RecordingTool::bash());
    let mut script: Vec<Round> = (1..=120)
        .map(|n| round(n, None, vec![call("pc-1", "bash", serde_json::json!({}))]))
        .collect();
    script.push(round(121, Some("跑完了。"), vec![]));
    let wired = Wired::new(vec![script], vec![tool.clone()]);
    let segment = wired.segment(CancelToken::new()).await;
    assert_eq!(segment.budget.max_rounds, None);
    let outcome = wired.agent.run(segment).await.unwrap();

    assert!(
        matches!(outcome, SegmentOutcome::Completed { rounds: 121, .. }),
        "{outcome:?}"
    );
    assert_eq!(tool.ran(), 120);
}

/// 取消：Run 明确终止在 cancelled，不留一个跑着的调用。
#[tokio::test]
async fn cancelling_ends_the_run_as_cancelled() {
    let tool = Arc::new(RecordingTool::bash());
    let wired = Wired::new(
        vec![vec![round(
            1,
            None,
            vec![call("pc-1", "bash", serde_json::json!({}))],
        )]],
        vec![tool.clone()],
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
        RunState::Cancelled
    );
}

/// 续跑：把上一回合没收尾的调用跑完，再请求下一轮模型。
#[tokio::test]
async fn a_resumed_segment_finishes_the_round_it_came_back_to() {
    let tool = Arc::new(RecordingTool::bash());
    let harness = Harness::new();
    let executor = harness.executor(vec![tool.clone()]);
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
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
        .await;

    let outcome = agent
        .run(Segment {
            telemetry: Default::default(),
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
            command: None,
            compaction: None,
            compactor: None,
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

/// 续跑时这次调用结果不明：不停下来，结果交给模型，这一段接着跑完（§8.6）。
#[tokio::test]
async fn a_resumed_uncertain_call_goes_on_to_the_model() {
    let tool = Arc::new(
        RecordingTool::bash().with_outcome(Err(ToolError::Uncertain {
            message: "响应丢了".into(),
        })),
    );
    let harness = Harness::new();
    let executor = harness.executor(vec![tool.clone()]);
    let agent = AgentLoop::new(
        Arc::new(ScriptedLlm::once(vec![round(
            2,
            Some("核对过了。"),
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
            &[("bash", serde_json::json!({ "command": "rm -rf /tmp/x" }))],
        )
        .await;

    let outcome = agent
        .run(Segment {
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget::default(),
            resume: Some(super::ResumedRound {
                settled: vec![],
                pending,
            }),
            command: None,
            compaction: None,
            compactor: None,
            session,
            run,
        })
        .await
        .unwrap();

    assert!(
        matches!(
            &outcome,
            SegmentOutcome::Completed { final_message: Some(text), .. } if text == "核对过了。"
        ),
        "{outcome:?}"
    );
    assert_eq!(tool.ran(), 1, "跑了一次，结果不明也不自动重试");
}

/// 结果不明 → 不停下来等人：结果连同判断方法交给模型，这一段接着跑（§8.6）。
#[tokio::test]
async fn an_uncertain_call_goes_back_to_the_model() {
    let tool = Arc::new(
        RecordingTool::bash().with_outcome(Err(ToolError::Uncertain {
            message: "响应丢了".into(),
        })),
    );
    let wired = Wired::new(
        vec![vec![
            round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]),
            round(2, Some("核对过了。"), vec![]),
        ]],
        vec![tool.clone()],
    );
    let segment = wired.segment(CancelToken::new()).await;
    let outcome = wired.agent.run(segment).await.unwrap();

    assert!(
        matches!(&outcome, SegmentOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(tool.ran(), 1, "执行器不自动重跑");
    let run = surface(&wired.harness)
        .runs
        .values()
        .next()
        .cloned()
        .unwrap();
    assert_eq!(run.status, RunState::Completed);
}

/// 每个 Run 固定自己的模型配置快照，一段只 `begin_turn` 一次（§6）。
#[tokio::test]
async fn one_segment_begins_exactly_one_turn() {
    let llm = Arc::new(ScriptedLlm::once(vec![round(1, Some("好。"), vec![])]));
    let harness = Harness::new();
    let executor = harness.executor(vec![]);
    let agent = AgentLoop::new(
        llm.clone(),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    agent
        .run(Segment {
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget::default(),
            resume: None,
            command: None,
            compaction: None,
            compactor: None,
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
    let tool = Arc::new(RecordingTool::bash());
    let harness = Harness::new();
    let executor = harness.executor(vec![tool]);
    let llm = Arc::new(ScriptedLlm::once(vec![
        round(
            1,
            None,
            vec![
                call("pc-a", "bash", serde_json::json!({ "command": "a" })),
                call("pc-b", "bash", serde_json::json!({ "command": "b" })),
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
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget::default(),
            resume: None,
            command: None,
            compaction: None,
            compactor: None,
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
    let tool = Arc::new(RecordingTool::bash().with_outcome(Ok(ToolOutput {
        status: ToolResultStatus::Completed,
        result: serde_json::json!({ "value": 42 }),
        exit_code: None,
        artifacts: vec![],
        images: vec![],
        preview: None,
    })));
    let wired = Wired::new(
        vec![vec![
            round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]),
            round(2, Some("好。"), vec![]),
        ]],
        vec![tool],
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

/// 能重试的模型错误**让出名额去等退避**，不当场失败（§8.5）。
#[tokio::test]
async fn a_retryable_model_error_suspends_on_a_backoff_instead_of_failing() {
    let harness = Harness::new();
    let executor = harness.executor(vec![]);
    let agent = AgentLoop::new(
        Arc::new(FailingLlm::at_round(
            komo_kernel::types::turn::LlmError::Timeout,
        )),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let outcome = agent
        .run(Segment {
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget::default(),
            resume: None,
            command: None,
            compaction: None,
            compactor: None,
            session,
            run: run.clone(),
        })
        .await
        .unwrap();

    let SegmentOutcome::Suspended {
        wait:
            WaitReason::Retry {
                attempts,
                not_before,
                cause,
            },
        ..
    } = &outcome
    else {
        panic!("{outcome:?}")
    };
    assert_eq!(*attempts, 1);
    assert!(*not_before > harness.clock.now(), "退避要落在将来");
    assert_eq!(
        *cause,
        RetryCause::Transport,
        "超时是「这一次没送达」，不是限流"
    );
    let view = surface(&harness).runs.get(&run).cloned().unwrap();
    assert_eq!(view.status, RunState::Waiting);
    assert!(
        matches!(
            view.wait,
            Some(WaitReason::Retry {
                attempts: 1,
                cause: RetryCause::Transport,
                ..
            })
        ),
        "{:?}",
        view.wait
    );
}

/// 结果与用量都未知不能自动再来一次（§8.5）——它是终止，不是退避。
#[tokio::test]
async fn an_unknown_model_outcome_is_not_retried() {
    let harness = Harness::new();
    let executor = harness.executor(vec![]);
    let agent = AgentLoop::new(
        Arc::new(FailingLlm::at_begin(
            komo_kernel::types::turn::LlmError::Unknown("结果未知".into()),
        )),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let outcome = agent
        .run(Segment {
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget::default(),
            resume: None,
            command: None,
            compaction: None,
            compactor: None,
            session,
            run,
        })
        .await
        .unwrap();
    assert!(
        matches!(outcome, SegmentOutcome::Failed { .. }),
        "{outcome:?}"
    );
}

/// **重启不重置预算**：已经用掉的次数由调用方给，用完就是 failed（§8.4、§8.5）。
#[tokio::test]
async fn an_exhausted_retry_budget_ends_in_a_failure() {
    let harness = Harness::new();
    let executor = harness.executor(vec![]);
    let agent = AgentLoop::new(
        Arc::new(FailingLlm::at_round(
            komo_kernel::types::turn::LlmError::Timeout,
        )),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let outcome = agent
        .run(Segment {
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget {
                retry: RetryBudget {
                    attempts: 4,
                    max_attempts: 5,
                    ..RetryBudget::default()
                },
                ..Budget::default()
            },
            resume: None,
            command: None,
            compaction: None,
            compactor: None,
            session,
            run: run.clone(),
        })
        .await
        .unwrap();

    let SegmentOutcome::Failed { reason, .. } = &outcome else {
        panic!("{outcome:?}")
    };
    assert!(reason.contains("重试预算已用完"), "{reason}");
    assert_eq!(
        surface(&harness).runs.get(&run).unwrap().status,
        RunState::Failed
    );
}

/// **重启不重置预算**（§8.5）：已经用掉的次数由调用方从账本里给，第三次失败记的就是
/// 第 3 次，不是第 1 次；退避也按它指数增长。
#[tokio::test]
async fn the_retry_count_continues_from_what_the_ledger_already_saved() {
    let harness = Harness::new();
    let executor = harness.executor(vec![]);
    let agent = AgentLoop::new(
        Arc::new(FailingLlm::at_round(
            komo_kernel::types::turn::LlmError::Timeout,
        )),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let started_at = harness.clock.now();

    let outcome = agent
        .run(Segment {
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget {
                retry: RetryBudget {
                    attempts: 2,
                    max_attempts: 5,
                    base: std::time::Duration::from_secs(2),
                    ..RetryBudget::default()
                },
                ..Budget::default()
            },
            resume: None,
            command: None,
            compaction: None,
            compactor: None,
            session,
            run: run.clone(),
        })
        .await
        .unwrap();

    let SegmentOutcome::Suspended {
        wait:
            WaitReason::Retry {
                attempts,
                not_before,
                ..
            },
        ..
    } = &outcome
    else {
        panic!("{outcome:?}")
    };
    assert_eq!(*attempts, 3, "接着上一次的次数数，不是从 1 起");
    // base * 2^2 = 8s：退避跟着已经用掉的次数涨，不是每次都退回起步值。
    assert_eq!(*not_before, started_at + time::Duration::seconds(8));

    // 账本上落的也是这个数——**次数只在 `WaitReason::Retry` 里**，没有第二处可读。
    let waiting = harness
        .ledger
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            komo_kernel::events::EventPayload::RunWaiting(body) => match &body.reason {
                WaitReason::Retry { attempts, .. } => Some(*attempts),
                _ => None,
            },
            _ => None,
        })
        .expect("有一条 run.waiting");
    assert_eq!(waiting, 3);
}

/// 预算只管**一串连续失败**：之前攒下的次数在模型成功回了一轮之后清零。长任务里
/// 零星的几次抖动不该攒到后面，把一次短暂的 503 直接打成 failed（SR-007）。
#[tokio::test]
async fn a_successful_round_resets_the_retry_count() {
    let tool = Arc::new(RecordingTool::bash());
    let harness = Harness::new();
    let executor = harness.executor(vec![tool.clone()]);
    let agent = AgentLoop::new(
        Arc::new(FailingLlm::after(
            vec![round(
                1,
                None,
                vec![call("pc-1", "bash", serde_json::json!({}))],
            )],
            komo_kernel::types::turn::LlmError::Rejected {
                status: 503,
                message: "No available accounts for this model.".into(),
            },
        )),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let started_at = harness.clock.now();

    let outcome = agent
        .run(Segment {
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget {
                retry: RetryBudget {
                    attempts: 9,
                    max_attempts: 10,
                    ..RetryBudget::default()
                },
                ..Budget::default()
            },
            resume: None,
            command: None,
            compaction: None,
            compactor: None,
            session,
            run,
        })
        .await
        .unwrap();

    let SegmentOutcome::Suspended {
        wait:
            WaitReason::Retry {
                attempts,
                not_before,
                cause,
            },
        ..
    } = &outcome
    else {
        panic!("{outcome:?}")
    };
    assert_eq!(*attempts, 1, "成功过一轮，从头数");
    assert_eq!(*not_before, started_at + time::Duration::seconds(2));
    assert_eq!(*cause, RetryCause::Server);
}

/// 指数退避**单次封顶**：连续失败到后面每次等 `max_wait`，不会一等几十分钟。
#[tokio::test]
async fn a_long_failure_streak_waits_at_most_max_wait_per_retry() {
    let harness = Harness::new();
    let executor = harness.executor(vec![]);
    let agent = AgentLoop::new(
        Arc::new(FailingLlm::at_round(
            komo_kernel::types::turn::LlmError::Timeout,
        )),
        harness.ledger.clone(),
        executor,
        Arc::new(harness.clock.clone()),
    );
    let (session, run) = harness.open_run().await;
    let started_at = harness.clock.now();

    let outcome = agent
        .run(Segment {
            telemetry: Default::default(),
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget {
                retry: RetryBudget {
                    attempts: 8,
                    ..RetryBudget::default()
                },
                ..Budget::default()
            },
            resume: None,
            command: None,
            compaction: None,
            compactor: None,
            session,
            run,
        })
        .await
        .unwrap();

    let SegmentOutcome::Suspended {
        wait: WaitReason::Retry { not_before, .. },
        ..
    } = &outcome
    else {
        panic!("{outcome:?}")
    };
    // 2s * 2^8 = 512s，封顶 60s。
    assert_eq!(*not_before, started_at + time::Duration::seconds(60));
}

// ---- 命令直跑模式（§10）：选驱动按 Run 的来源，命令 Run 全程零模型请求 ----

/// 选驱动的接缝在 `AgentLoop::run` 里：`segment.command` 一给，`self.llm` 一次都不会
/// 被摸到——不是"这次没观察到调用"，而是接了一个**碰就 panic** 的假 `LlmClient` 还是
/// 没炸。第一轮固定发那个 `bash` 调用，最终答复就是那条工具结果的正文。
#[tokio::test]
async fn a_command_run_never_touches_the_llm_client_and_finishes_with_the_shell_result() {
    let tool = Arc::new(RecordingTool::bash().with_outcome(Ok(ToolOutput {
        status: ToolResultStatus::Completed,
        result: serde_json::json!({}),
        exit_code: Some(0),
        artifacts: vec![],
        images: vec![],
        preview: Some("hi".into()),
    })));
    let wired = Wired::with_llm(Arc::new(PanickingLlm), vec![tool.clone()]);
    let segment = wired
        .command_segment(
            CancelToken::new(),
            Some(super::command_driver::CommandSpec::job("echo hi".into())),
        )
        .await;

    let outcome = wired.agent.run(segment).await.unwrap();
    let SegmentOutcome::Completed { final_message, .. } = &outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(tool.ran(), 1, "那条 bash 调用真的执行了一次");
    assert!(
        final_message
            .as_deref()
            .is_some_and(|text| text.contains("hi")),
        "{final_message:?}"
    );
}

/// 命令为空输出、非 0 退出，都是**结果**，不是驱动错误——"工具失败是结果，驱动失败
/// 是终止"（§6）对命令 Run 同样成立：`CommandDriver` 永远正常收尾，ok / error 的区分
/// 是投递那一层的事（cron_watch），不是这里的事。
#[tokio::test]
async fn a_non_zero_exit_still_completes_the_run_not_fails_it() {
    let tool = Arc::new(RecordingTool::bash().with_outcome(Ok(ToolOutput {
        status: ToolResultStatus::Failed,
        result: serde_json::json!({}),
        exit_code: Some(1),
        artifacts: vec![],
        images: vec![],
        preview: Some("boom".into()),
    })));
    let wired = Wired::with_llm(Arc::new(PanickingLlm), vec![tool]);
    let segment = wired
        .command_segment(
            CancelToken::new(),
            Some(super::command_driver::CommandSpec::job("exit 1".into())),
        )
        .await;

    let outcome = wired.agent.run(segment).await.unwrap();
    assert!(
        matches!(outcome, SegmentOutcome::Completed { .. }),
        "{outcome:?}"
    );
}

/// 一份 20 KB 的正文：投影成 full 之后远超衰减阈值（默认 4 KiB）。
fn big_text() -> String {
    (0..400)
        .map(|n| format!("line {n:04}: {}\n", "x".repeat(40)))
        .collect()
}

fn reading(name: &'static str, status: ToolResultStatus, preview: String) -> Arc<RecordingTool> {
    Arc::new(
        RecordingTool::new(name, komo_kernel::types::plan::Operation::ReadFile).with_outcome(Ok(
            ToolOutput {
                status,
                result: serde_json::json!({}),
                exit_code: None,
                artifacts: vec![],
                images: vec![],
                preview: Some(preview),
            },
        )),
    )
}

/// 第一轮调 `first`（名字是 `read`），之后三轮各调一次小工具，最后收尾。返回每次 `next` 拿到的轮输入。
async fn four_rounds_after(first: Arc<RecordingTool>) -> Vec<komo_kernel::types::turn::RoundInput> {
    let small = reading("small", ToolResultStatus::Completed, "ok".into());
    let llm = Arc::new(ScriptedLlm::once(vec![
        round(
            1,
            None,
            vec![call("pc-first", "read", serde_json::json!({}))],
        ),
        round(2, None, vec![call("pc-s1", "small", serde_json::json!({}))]),
        round(3, None, vec![call("pc-s2", "small", serde_json::json!({}))]),
        round(4, None, vec![call("pc-s3", "small", serde_json::json!({}))]),
        round(5, Some("看完了。"), vec![]),
    ]));
    let wired = Wired::with_llm(llm.clone(), vec![first, small]);
    let segment = wired.segment(CancelToken::new()).await;
    let outcome = wired.agent.run(segment).await.unwrap();
    assert!(
        matches!(outcome, SegmentOutcome::Completed { rounds: 5, .. }),
        "{outcome:?}"
    );
    llm.inputs.lock().unwrap().clone()
}

fn revised_of(
    input: &komo_kernel::types::turn::RoundInput,
) -> &[komo_kernel::types::turn::ToolResultForModel] {
    match input {
        komo_kernel::types::turn::RoundInput::ToolResults { revised, .. } => revised,
        komo_kernel::types::turn::RoundInput::First => &[],
    }
}

/// planner 没开口（没有 compactor）时从不换：大结果给多少次都不自动换——换不换是对前缀的
/// 改写决策，不是按次数倒数（§8.3）。
#[tokio::test]
async fn a_large_result_is_never_revised_without_a_planner_decision() {
    let inputs = four_rounds_after(reading("read", ToolResultStatus::Completed, big_text())).await;
    assert_eq!(inputs.len(), 5, "{inputs:?}");
    let komo_kernel::types::turn::RoundInput::ToolResults { results, .. } = &inputs[1] else {
        panic!("第二次请求带着工具结果：{:?}", inputs[1]);
    };
    assert!(
        results[0].content.len() > 4096,
        "{}",
        results[0].content.len()
    );
    assert!(
        inputs.iter().all(|input| revised_of(input).is_empty()),
        "{inputs:?}"
    );
}

// ---------------------------------------------------------------- 在线压缩（§6）

mod compaction {
    use super::*;
    use crate::agent::tests_support::{
        FakePlanner, HangingLlm, compaction_job, compactor, decay_job,
    };
    use komo_kernel::compaction::Reshape;
    use komo_kernel::events::{ContextCompacted, EventPayload};
    use komo_kernel::types::ids::Seq;
    use komo_kernel::types::turn::RoundInput;

    fn compactions(harness: &Harness) -> Vec<ContextCompacted> {
        harness
            .ledger
            .events()
            .into_iter()
            .filter_map(|event| match event.payload {
                EventPayload::ContextCompacted(body) => Some(body),
                _ => None,
            })
            .collect()
    }

    fn assistants(harness: &Harness) -> usize {
        harness
            .ledger
            .events()
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::MessageAssistant(_)))
            .count()
    }

    /// 一条小调用 → 一条参数很大的调用（把估计推过 [`pressured`] 的线）→ 本该收尾的第三轮。
    fn pressure_rounds() -> Vec<Round> {
        vec![
            round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]),
            round(
                2,
                None,
                vec![call(
                    "pc-2",
                    "bash",
                    serde_json::json!({ "x": "y".repeat(400) }),
                )],
            ),
            round(3, Some("都做完了。"), vec![]),
        ]
    }

    /// 线画在 100 token：[`pressure_rounds`] 只有第二轮过线。
    fn pressured(planner: Arc<FakePlanner>) -> crate::agent::Compactor {
        crate::agent::Compactor {
            pressure_tokens: Some(100),
            estimate: 10,
            ..compactor(planner)
        }
    }

    fn wired(llm: Arc<ScriptedLlm>) -> Wired {
        Wired::with_llm(llm, vec![Arc::new(RecordingTool::bash())])
    }

    #[tokio::test]
    async fn a_compaction_job_summarizes_records_and_recomposes() {
        let llm = Arc::new(ScriptedLlm::new(vec![vec![round(
            1,
            Some("  摘要：a 做完了，cargo test 通过。  "),
            vec![],
        )]]));
        let wired = wired(Arc::clone(&llm));
        let mut segment = wired.segment(CancelToken::new()).await;
        segment.compaction = Some(compaction_job(&segment.session, &segment.run));

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Recompose { rounds: 0, .. }),
            "{outcome:?}"
        );

        let requests = llm.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "只发了摘要请求，这一段的主请求作废了");
        assert!(requests[0].tools.is_empty());
        assert_eq!(requests[0].system_prompt, "压缩上下文");
        drop(requests);

        let recorded = compactions(&wired.harness);
        let [
            ContextCompacted::Compacted {
                first_kept,
                summary,
                decision,
                debt,
                ..
            },
        ] = recorded.as_slice()
        else {
            panic!("{:?}", compactions(&wired.harness));
        };
        assert_eq!(*first_kept, Seq(2));
        assert_eq!(
            summary.as_deref(),
            Some("摘要：a 做完了，cargo test 通过。")
        );
        assert_eq!(*debt, decision.debt());
        assert_eq!(assistants(&wired.harness), 0, "摘要不是这条 Run 的一轮");
    }

    #[tokio::test]
    async fn a_failed_summary_is_recorded_as_skipped_and_the_run_goes_on() {
        let truncated = Round {
            truncated: true,
            ..round(1, Some("摘要写到一半"), vec![])
        };
        let llm = Arc::new(ScriptedLlm::new(vec![
            vec![truncated],
            vec![round(1, Some("接着做完了。"), vec![])],
        ]));
        let wired = wired(Arc::clone(&llm));
        let mut segment = wired.segment(CancelToken::new()).await;
        segment.compaction = Some(compaction_job(&segment.session, &segment.run));

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(&outcome, SegmentOutcome::Completed { final_message, .. }
                if final_message.as_deref() == Some("接着做完了。")),
            "{outcome:?}"
        );
        let recorded = compactions(&wired.harness);
        let [ContextCompacted::Skipped { reason, decision }] = recorded.as_slice() else {
            panic!("{:?}", compactions(&wired.harness));
        };
        assert_eq!(reason, "摘要被截断");
        assert!(
            decision.as_ref().is_some_and(|decision| decision.compact),
            "带着那份决定了压的决策：在线状态据此不在窗口压力下马上重试"
        );
    }

    #[tokio::test]
    async fn cancel_during_summary_cancels() {
        let wired = Wired::with_llm(Arc::new(HangingLlm), vec![]);
        let cancel = CancelToken::new();
        let mut segment = wired.segment(cancel.clone()).await;
        segment.compaction = Some(compaction_job(&segment.session, &segment.run));

        let pressing = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel.cancel();
        });
        let outcome = wired.agent.run(segment).await.unwrap();
        pressing.await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Cancelled { .. }),
            "{outcome:?}"
        );
        assert!(compactions(&wired.harness).is_empty(), "取消什么都不记");
        let surface = surface(&wired.harness);
        assert_eq!(
            surface.runs.values().next().unwrap().status,
            RunState::Cancelled
        );
    }

    #[tokio::test]
    async fn a_pressured_round_hands_back_for_recomposition() {
        let llm = Arc::new(ScriptedLlm::new(vec![
            pressure_rounds(),
            vec![round(1, Some("摘要：a 做完了。"), vec![])],
        ]));
        let wired = wired(Arc::clone(&llm));
        let mut segment = wired.segment(CancelToken::new()).await;
        let job = compaction_job(&segment.session, &segment.run);
        let planner = FakePlanner::new(vec![Some(Reshape::Compact(job))]);
        segment.compactor = Some(pressured(Arc::clone(&planner)));

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Recompose { rounds: 2, .. }),
            "{outcome:?}"
        );
        assert_eq!(planner.asked(), 1, "第一轮没过线，不问");
        assert_eq!(assistants(&wired.harness), 2, "第三轮没发出去");
        assert!(matches!(
            compactions(&wired.harness).as_slice(),
            [ContextCompacted::Compacted { .. }]
        ));
        // 过线那一轮的结果已经落账，下一段的回放带着它；这一段的 driver 没再拿到它。
        let inputs = llm.inputs.lock().unwrap();
        assert_eq!(inputs.len(), 3, "主 driver 两轮 + 摘要一轮：{inputs:?}");
        assert!(matches!(inputs[2], RoundInput::First), "最后一次是摘要请求");
    }

    /// 决定不压：同一个 driver 接着跑，下一次请求就是没有这个功能时那一次。
    #[tokio::test]
    async fn a_planner_that_declines_leaves_the_driver_running() {
        let llm = Arc::new(ScriptedLlm::new(vec![pressure_rounds()]));
        let wired = wired(Arc::clone(&llm));
        let mut segment = wired.segment(CancelToken::new()).await;
        let planner = FakePlanner::new(vec![None]);
        segment.compactor = Some(pressured(Arc::clone(&planner)));

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Completed { rounds: 3, .. }),
            "{outcome:?}"
        );
        assert_eq!(planner.asked(), 1);
        assert_eq!(
            llm.requests.lock().unwrap().len(),
            1,
            "没有第二次 begin_turn"
        );
    }

    /// 换短视图：先落 `decayed`，整批在下一次请求里交给驱动，这一段不 Recompose、driver 不重开。
    #[tokio::test]
    async fn a_decay_job_is_recorded_then_revised_without_recomposing() {
        let llm = Arc::new(ScriptedLlm::new(vec![pressure_rounds()]));
        let wired = wired(Arc::clone(&llm));
        let mut segment = wired.segment(CancelToken::new()).await;
        let job = decay_job("pc-old");
        let expected = job.revised.clone();
        let planner = FakePlanner::new(vec![Some(Reshape::Decay(job))]);
        segment.compactor = Some(pressured(Arc::clone(&planner)));

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Completed { rounds: 3, .. }),
            "{outcome:?}"
        );
        assert_eq!(planner.asked(), 1);
        assert_eq!(llm.requests.lock().unwrap().len(), 1, "没有重新装配");
        match compactions(&wired.harness).as_slice() {
            [ContextCompacted::Decayed { calls, .. }] => assert_eq!(calls.len(), 1),
            other => panic!("{other:?}"),
        }
        let inputs = llm.inputs.lock().unwrap();
        assert_eq!(inputs.len(), 3, "{inputs:?}");
        // 过线那一轮之后的请求带着整批；之前那次没有。
        assert_eq!(revised_of(&inputs[2]), expected.as_slice(), "{inputs:?}");
        assert!(revised_of(&inputs[1]).is_empty(), "{inputs:?}");
    }

    /// 没有压力：一条跑得比缓存存活时间还久的工具调用之后（缓存已冷）也要问；
    /// 跑得快的那一轮不问。
    #[tokio::test(start_paused = true)]
    async fn a_tool_slower_than_the_cache_ttl_asks_the_planner() {
        let slow = Arc::new(RecordingTool::bash().with_delay(
            komo_kernel::compaction::PROMPT_CACHE_TTL + std::time::Duration::from_secs(1),
        ));
        let llm = Arc::new(ScriptedLlm::new(vec![vec![
            round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]),
            round(2, None, vec![call("pc-2", "bash", serde_json::json!({}))]),
            round(3, Some("好了。"), vec![]),
        ]]));
        let wired = Wired::with_llm(Arc::clone(&llm) as _, vec![slow]);
        let mut segment = wired.segment(CancelToken::new()).await;
        let planner = FakePlanner::new(vec![None, None]);
        segment.compactor = Some(compactor(Arc::clone(&planner)));

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Completed { .. }),
            "{outcome:?}"
        );
        assert_eq!(planner.asked(), 2, "两轮各跑了一条慢调用");
    }

    #[tokio::test(start_paused = true)]
    async fn fast_tools_never_ask_the_planner_without_pressure() {
        let tool = Arc::new(RecordingTool::bash());
        let llm = Arc::new(ScriptedLlm::new(vec![vec![
            round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]),
            round(2, Some("好了。"), vec![]),
        ]]));
        let wired = Wired::with_llm(Arc::clone(&llm) as _, vec![tool]);
        let mut segment = wired.segment(CancelToken::new()).await;
        let planner = FakePlanner::new(vec![None]);
        segment.compactor = Some(compactor(Arc::clone(&planner)));

        wired.agent.run(segment).await.unwrap();
        assert_eq!(planner.asked(), 0);
    }

    #[tokio::test]
    async fn disabled_compaction_never_hands_back() {
        let llm = Arc::new(ScriptedLlm::new(vec![pressure_rounds()]));
        let wired = wired(Arc::clone(&llm));
        let segment = wired.segment(CancelToken::new()).await;
        assert!(segment.compactor.is_none() && segment.compaction.is_none());

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Completed { rounds: 3, .. }),
            "{outcome:?}"
        );
        assert_eq!(llm.requests.lock().unwrap().len(), 1);
        assert!(compactions(&wired.harness).is_empty());
    }

    /// 估计的上下文过了线就问，没过线的轮次一次都不问。
    #[tokio::test]
    async fn window_pressure_asks_the_planner() {
        let tool = Arc::new(RecordingTool::bash());
        let llm = Arc::new(ScriptedLlm::new(vec![vec![
            round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]),
            round(
                2,
                None,
                vec![call(
                    "pc-2",
                    "bash",
                    serde_json::json!({ "x": "y".repeat(400) }),
                )],
            ),
            round(3, Some("好了。"), vec![]),
        ]]));
        let wired = Wired::with_llm(Arc::clone(&llm) as _, vec![tool]);
        let mut segment = wired.segment(CancelToken::new()).await;
        let planner = FakePlanner::new(vec![None]);
        segment.compactor = Some(crate::agent::Compactor {
            pressure_tokens: Some(100),
            estimate: 10,
            ..compactor(Arc::clone(&planner))
        });

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Completed { .. }),
            "{outcome:?}"
        );
        assert_eq!(planner.asked(), 1, "只有第二轮那一大段参数把估计推过了线");
    }
}

/// 溢出恢复（§6）：provider 以上下文超长拒了请求，强制压一次、重新装配接着跑；只救一次。
mod overflow {
    use super::*;
    use crate::agent::tests_support::{FakePlanner, SequenceLlm, compaction_job, compactor};
    use komo_kernel::events::{ContextCompacted, EventPayload};
    use komo_kernel::types::turn::LlmError;

    fn too_long(status: u16) -> LlmError {
        LlmError::Rejected {
            status,
            message: "prompt is too long: 213462 tokens > 200000 maximum".into(),
        }
    }

    fn compactions(harness: &Harness) -> Vec<ContextCompacted> {
        harness
            .ledger
            .events()
            .into_iter()
            .filter_map(|event| match event.payload {
                EventPayload::ContextCompacted(body) => Some(body),
                _ => None,
            })
            .collect()
    }

    fn forced_job(segment: &Segment) -> komo_kernel::compaction::CompactionJob {
        let job = compaction_job(&segment.session, &segment.run);
        komo_kernel::compaction::CompactionJob {
            decision: job.decision.clone().forced_by_overflow(),
            ..job
        }
    }

    #[tokio::test]
    async fn an_overflow_on_the_first_request_compacts_and_recomposes() {
        let llm = SequenceLlm::new(vec![
            Err(too_long(400)),
            Ok(vec![Ok(round(1, Some("摘要：a 做完了。"), vec![]))]),
        ]);
        let wired = Wired::with_llm(Arc::clone(&llm) as _, vec![]);
        let mut segment = wired.segment(CancelToken::new()).await;
        let planner = FakePlanner::with_overflow(vec![], vec![Some(forced_job(&segment))]);
        segment.compactor = Some(compactor(Arc::clone(&planner)));

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Recompose { rounds: 0, .. }),
            "{outcome:?}"
        );
        assert_eq!(planner.overflow_asked(), 1);
        let requests = llm.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "主请求 + 摘要请求");
        assert!(requests[1].tools.is_empty(), "摘要请求没有工具");
        drop(requests);
        let recorded = compactions(&wired.harness);
        let [ContextCompacted::Compacted { decision, .. }] = recorded.as_slice() else {
            panic!("{recorded:?}");
        };
        assert!(decision.context_overflow && decision.compact);
    }

    #[tokio::test]
    async fn an_overflow_mid_segment_compacts_after_the_finished_rounds() {
        let llm = SequenceLlm::new(vec![
            Ok(vec![
                Ok(round(
                    1,
                    None,
                    vec![call("pc-1", "bash", serde_json::json!({}))],
                )),
                Err(too_long(400)),
            ]),
            Ok(vec![Ok(round(1, Some("摘要"), vec![]))]),
        ]);
        let wired = Wired::with_llm(Arc::clone(&llm) as _, vec![Arc::new(RecordingTool::bash())]);
        let mut segment = wired.segment(CancelToken::new()).await;
        let planner = FakePlanner::with_overflow(vec![], vec![Some(forced_job(&segment))]);
        segment.compactor = Some(compactor(Arc::clone(&planner)));

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Recompose { rounds: 1, .. }),
            "{outcome:?}"
        );
        assert_eq!(compactions(&wired.harness).len(), 1);
    }

    /// planner 说救不回来（刚压过还是超长、切不出）：照原样失败，不退避重试。
    #[tokio::test]
    async fn an_overflow_the_planner_cannot_cut_fails_the_run() {
        let llm = SequenceLlm::new(vec![Err(too_long(400))]);
        let wired = Wired::with_llm(Arc::clone(&llm) as _, vec![]);
        let mut segment = wired.segment(CancelToken::new()).await;
        let planner = FakePlanner::with_overflow(vec![], vec![None]);
        segment.compactor = Some(compactor(Arc::clone(&planner)));

        let outcome = wired.agent.run(segment).await.unwrap();
        let SegmentOutcome::Failed { reason, .. } = &outcome else {
            panic!("{outcome:?}");
        };
        assert!(reason.contains("prompt is too long"), "{reason}");
        assert_eq!(planner.overflow_asked(), 1);
        assert!(compactions(&wired.harness).is_empty());
        assert_eq!(llm.requests.lock().unwrap().len(), 1);
    }

    /// 摘要没成：记 `skipped`，Run 按原来的超长失败——不再在这一段里重试。
    #[tokio::test]
    async fn a_failed_overflow_summary_is_skipped_and_the_run_fails() {
        let truncated = Round {
            truncated: true,
            ..round(1, Some("摘要写到一半"), vec![])
        };
        let llm = SequenceLlm::new(vec![Err(too_long(400)), Ok(vec![Ok(truncated)])]);
        let wired = Wired::with_llm(Arc::clone(&llm) as _, vec![]);
        let mut segment = wired.segment(CancelToken::new()).await;
        let planner = FakePlanner::with_overflow(vec![], vec![Some(forced_job(&segment))]);
        segment.compactor = Some(compactor(Arc::clone(&planner)));

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Failed { .. }),
            "{outcome:?}"
        );
        let recorded = compactions(&wired.harness);
        assert!(
            matches!(recorded.as_slice(), [ContextCompacted::Skipped { .. }]),
            "{recorded:?}"
        );
    }

    /// 没有 planner（命令 Run、没接配置）：包成 500 的超长也不退避，直接失败。
    #[tokio::test]
    async fn an_overflow_without_a_planner_fails_instead_of_backing_off() {
        let llm = SequenceLlm::new(vec![Err(too_long(500))]);
        let wired = Wired::with_llm(Arc::clone(&llm) as _, vec![]);
        let segment = wired.segment(CancelToken::new()).await;

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Failed { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn other_errors_never_ask_for_an_overflow_compaction() {
        let llm = SequenceLlm::new(vec![Err(LlmError::Rejected {
            status: 503,
            message: "overloaded".into(),
        })]);
        let wired = Wired::with_llm(Arc::clone(&llm) as _, vec![]);
        let mut segment = wired.segment(CancelToken::new()).await;
        let planner = FakePlanner::with_overflow(vec![], vec![Some(forced_job(&segment))]);
        segment.compactor = Some(compactor(Arc::clone(&planner)));

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Suspended { .. }),
            "照常退避：{outcome:?}"
        );
        assert_eq!(planner.overflow_asked(), 0);
        assert!(compactions(&wired.harness).is_empty());
    }
}

/// 插话（§6）：一轮的调用全部收尾、下一次请求之前，排着队要求插话的输入被并进来，这一段
/// 交回去按日志重新装配；最终回答那一轮没有"下一次请求"，轮不上。
mod interjection {
    use super::*;
    use komo_kernel::events::EventPayload;
    use komo_kernel::traits::Ledger;
    use komo_kernel::types::ids::{ExecutorId, RequestKey, RunId, SessionId};
    use komo_kernel::types::plan::PlanSource;
    use komo_kernel::types::turn::AcceptInput;

    async fn started(wired: &Wired) -> Segment {
        let segment = wired.segment(CancelToken::new()).await;
        wired
            .harness
            .ledger
            .start_run(&segment.run, &ExecutorId::from_raw("ex-1"), 1)
            .await
            .unwrap();
        segment
    }

    async fn typed(wired: &Wired, session: &SessionId, key: &str, interject: bool) -> RunId {
        wired
            .harness
            .ledger
            .accept_input(AcceptInput {
                session: session.clone(),
                request_key: RequestKey::new(key),
                text: "顺便看下 b".into(),
                source: PlanSource::Interactive {
                    session: session.clone(),
                },
                peer: None,
                model: sample_model(),
                workdir: None,
                delegate: None,
                snapshot: None,
                skip_memory: false,
                interject,
                at: wired.harness.clock.now(),
            })
            .await
            .unwrap()
            .run
    }

    #[tokio::test]
    async fn an_interjection_is_absorbed_after_the_round_and_hands_back() {
        let llm = Arc::new(ScriptedLlm::new(vec![vec![
            round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]),
            round(2, Some("不该发出去"), vec![]),
        ]]));
        let tool = Arc::new(RecordingTool::bash());
        let wired = Wired::with_llm(llm.clone(), vec![tool.clone()]);
        let segment = started(&wired).await;
        let (session, host) = (segment.session.clone(), segment.run.clone());
        let late = typed(&wired, &session, "tui:2", true).await;

        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Interjected { rounds: 1, .. }),
            "{outcome:?}"
        );
        assert_eq!(tool.ran(), 1);
        assert_eq!(
            llm.inputs.lock().unwrap().len(),
            1,
            "第二次请求留给重新装配之后"
        );

        let events = wired.harness.ledger.events();
        let kinds: Vec<&str> = events.iter().map(|e| e.payload.type_name()).collect();
        let result = kinds.iter().position(|k| *k == "tool.result").unwrap();
        let absorbed = kinds.iter().position(|k| *k == "run.absorbed").unwrap();
        assert!(result < absorbed, "并在这一轮的结果之后：{kinds:?}");
        assert!(matches!(
            &events[absorbed].payload,
            EventPayload::RunAbsorbed(body) if body.into == host
        ));
        assert_eq!(events[absorbed].run.as_ref(), Some(&late));

        let surface = surface(&wired.harness);
        assert_eq!(surface.runs[&late].status, RunState::Absorbed);
        assert_eq!(surface.runs[&host].status, RunState::Running);
    }

    #[tokio::test]
    async fn a_final_answer_or_a_plain_input_leaves_it_queued() {
        // 没要求插话的（监听事件、渠道消息）照旧排队，宿主照常跑完。
        let wired = Wired::new(
            vec![vec![
                round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]),
                round(2, Some("好了。"), vec![]),
            ]],
            vec![Arc::new(RecordingTool::bash())],
        );
        let segment = started(&wired).await;
        let plain = typed(&wired, &segment.session.clone(), "api:2", false).await;
        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Completed { rounds: 2, .. }),
            "{outcome:?}"
        );
        assert_eq!(
            surface(&wired.harness).runs[&plain].status,
            RunState::Queued
        );

        // 最终回答那一轮没有调用：没有"下一次请求之前"，那句话排在后面。
        let wired = Wired::new(vec![vec![round(1, Some("好了。"), vec![])]], vec![]);
        let segment = started(&wired).await;
        let late = typed(&wired, &segment.session.clone(), "tui:2", true).await;
        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            matches!(outcome, SegmentOutcome::Completed { .. }),
            "{outcome:?}"
        );
        assert_eq!(surface(&wired.harness).runs[&late].status, RunState::Queued);
    }

    /// 已经被取消的 Run 不并：并进来就没人答它了。
    #[tokio::test]
    async fn a_cancelled_host_takes_no_interjection() {
        let cancel = CancelToken::new();
        let tool = Arc::new(RecordingTool::bash());
        let wired = Wired::new(
            vec![vec![round(
                1,
                None,
                vec![call("pc-1", "bash", serde_json::json!({}))],
            )]],
            vec![tool],
        );
        let segment = wired.segment(cancel.clone()).await;
        wired
            .harness
            .ledger
            .start_run(&segment.run, &ExecutorId::from_raw("ex-1"), 1)
            .await
            .unwrap();
        let late = typed(&wired, &segment.session.clone(), "tui:2", true).await;
        cancel.cancel();
        let outcome = wired.agent.run(segment).await.unwrap();
        assert!(
            !matches!(outcome, SegmentOutcome::Interjected { .. }),
            "{outcome:?}"
        );
        assert_eq!(surface(&wired.harness).runs[&late].status, RunState::Queued);
    }
}

/// OpenAI 的 `output_tokens` 已经含推理：预算只算一次。
#[test]
fn reasoning_is_not_counted_twice_against_the_budget() {
    let usage = komo_kernel::types::model::TokenUsage {
        input: Some(100),
        output: Some(40),
        reasoning: Some(30),
        ..Default::default()
    };
    assert_eq!(super::spent(&usage), 140);
}
