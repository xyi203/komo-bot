//! Agent Loop 的验收（§14 阶段 2）。

use std::sync::Arc;

use komo_kernel::fold::Surface;
use komo_kernel::test_support::{ScriptedLlm, sample_model};
use komo_kernel::traits::{ApprovalRepo, Clock};
use komo_kernel::types::ids::InterventionId;
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::status::{RetryCause, RunState, WaitReason};
use komo_kernel::types::tool::{CancelToken, ToolError, ToolOutput};
use komo_kernel::types::turn::{Role, Round};

use super::tests_support::{FailingLlm, call, round, turn_request};
use super::{AgentLoop, Budget, RetryBudget, Segment, SegmentOutcome};
use crate::executor::harness::{Harness, RecordingTool};

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
    assert!(matches!(wait, WaitReason::Approval { .. }), "{wait:?}");
    assert_eq!(tool.ran(), 0, "批准之前不执行");

    let surface = surface(&wired.harness);
    let run = surface.runs.values().next().unwrap();
    assert_eq!(run.status, RunState::Waiting, "停着");
    assert!(
        matches!(run.wait, Some(WaitReason::Approval { .. })),
        "而且说得出在等一条审批：{:?}",
        run.wait
    );
    // 待处理的审批在数据库里，**不是**折出来的：JSONL 里的审批事件是审计补写
    // （§8.5 的反向顺序），它不创建授权，也不由 executor 写。
    let pending = wired.harness.approvals.list_pending(None).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].plan.tool, "shell");
}

/// 委派：父 Run 停在 `dependency` 上**让出执行名额**，不在进程里等子 Run。
///
/// 它和"等审批"是同一件事的两种外因：停下来的形状都是 `waiting` + 一条 `WaitReason`，
/// 而"在等谁"由理由说清楚——这里等的是那条子 Run 的终态，没有任何人要回答什么。
#[tokio::test]
async fn a_delegated_call_suspends_the_run_on_its_child() {
    let wired = Wired::new(
        vec![vec![round(
            1,
            None,
            vec![call(
                "pc-1",
                "delegate",
                serde_json::json!({ "task": "去查一下这个接口的超时" }),
            )],
        )]],
        vec![Arc::new(crate::tools::DelegateTool::new())],
        true,
    );
    let segment = wired.segment(CancelToken::new()).await;
    let parent = segment.run.clone();
    let outcome = wired.agent.run(segment).await.unwrap();

    let SegmentOutcome::Suspended {
        wait: WaitReason::Dependency { run: child },
        ..
    } = &outcome
    else {
        panic!("{outcome:?}")
    };

    let surface = surface(&wired.harness);
    let view = &surface.runs[&parent];
    assert_eq!(view.status, RunState::Waiting, "停着，不占执行名额");
    assert_eq!(
        view.wait,
        Some(WaitReason::Dependency { run: child.clone() })
    );
    // 子 Run 是一条**普通 Run**：它已经在队列里等 worker，可领取、可审批、可取消，
    // 而且带着那份 spec。
    let child_view = &surface.runs[child];
    assert_eq!(child_view.status, RunState::Queued);
    assert_eq!(
        child_view.delegate.as_ref().map(|spec| &spec.parent),
        Some(&parent)
    );
    // 父这一次调用还没有结果：结果就是子 Run 的终态，它还没到。
    assert!(surface.calls.values().all(|call| call.output.is_none()));
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
        RunState::Failed
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
        RunState::Cancelled
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
                wait: WaitReason::Approval { .. },
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(tool.ran(), 0);
}

/// 结果不明 → 这一段停在 `waiting + intervention`，等人（§8.6）。
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
                wait: WaitReason::Intervention { .. },
                ..
            }
        ),
        "{outcome:?}"
    );
    let run = surface(&wired.harness)
        .runs
        .values()
        .next()
        .cloned()
        .unwrap();
    assert_eq!(run.status, RunState::Waiting);
    assert_eq!(
        run.wait,
        Some(WaitReason::Intervention {
            intervention: InterventionId::for_run(&run.run)
        }),
        "干预的句柄就是这个 Run：一个 Run 上最多停一条要人判断的干预"
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

/// 能重试的模型错误**让出名额去等退避**，不当场失败（§8.5）。
#[tokio::test]
async fn a_retryable_model_error_suspends_on_a_backoff_instead_of_failing() {
    let harness = Harness::new();
    let executor = harness.permissive(vec![]);
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
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget::default(),
            resume: None,
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
    let executor = harness.permissive(vec![]);
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
        matches!(outcome, SegmentOutcome::Failed { .. }),
        "{outcome:?}"
    );
}

/// **重启不重置预算**：已经用掉的次数由调用方给，用完就是 failed（§8.4、§8.5）。
#[tokio::test]
async fn an_exhausted_retry_budget_ends_in_a_failure() {
    let harness = Harness::new();
    let executor = harness.permissive(vec![]);
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
    let executor = harness.permissive(vec![]);
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
            request: turn_request(&session, &run),
            env: harness.env(&session, &run),
            budget: Budget {
                retry: RetryBudget {
                    attempts: 2,
                    max_attempts: 5,
                    base: std::time::Duration::from_secs(2),
                },
                ..Budget::default()
            },
            resume: None,
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
