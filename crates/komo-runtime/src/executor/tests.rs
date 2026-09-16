//! executor 的验收（§14 阶段 2、3）。

use std::sync::Arc;

use komo_kernel::policy::{Grant, GrantScope, Matcher, OperationMatch, RuleTable};
use komo_kernel::protocol::http::ApprovalDecisionRecord;
use komo_kernel::traits::{ApprovalRepo, Clock, Tool};
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::ids::{ApprovalId, AttemptId, GrantId};
use komo_kernel::types::plan::{Operation, RecoveryMode, Verification};
use komo_kernel::types::refs::{PREVIEW_LIMIT_BYTES, ToolResultStatus};
use komo_kernel::types::status::ToolCallState;
use komo_kernel::types::tool::{CancelToken, ToolError, ToolOutput};

use super::harness::{Harness, RecordingTool};
use super::{RoundStop, resumed_from};
use crate::policy::PolicyEngine;
use crate::tools::{ReadTool, WriteTool};

/// ① 危险操作在批准前不执行。
#[tokio::test]
async fn a_dangerous_action_does_not_run_before_it_is_approved() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::shell());
    let executor = harness.initial(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[("shell", serde_json::json!({ "command": "rm -rf /tmp/x" }))],
        )
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert!(
        matches!(outcome.stop, Some(RoundStop::Approval { .. })),
        "{:?}",
        outcome.stop
    );
    assert_eq!(tool.ran(), 0, "批准之前一次都不能跑");
    assert!(outcome.results.is_empty());

    // 审批请求真的写下来了，而且绑定的是那份计划。
    let pending = harness
        .approvals
        .list_pending(Some(&session))
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].plan.tool, "shell");
    assert_eq!(pending[0].plan_hash, pending[0].plan.plan_hash());
}

/// ② 批准之后执行一次；重复批准不重复执行。
#[tokio::test]
async fn approving_twice_still_executes_exactly_once() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::shell());
    let executor = harness.initial(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[("shell", serde_json::json!({ "command": "rm -rf /tmp/x" }))],
        )
        .await;
    let env = harness.env(&session, &run);

    let stopped = executor.execute_round(calls, &env).await.unwrap();
    let Some(RoundStop::Approval { approval, call }) = stopped.stop.clone() else {
        panic!("{:?}", stopped.stop)
    };

    // 操作者连点两次。第二次是"已决定"，不产生第二条决定。
    harness
        .gate
        .decide(&approval, true, ApprovalScope::Once, None)
        .await
        .unwrap();
    let again = harness
        .gate
        .decide(&approval, true, ApprovalScope::Once, None)
        .await
        .unwrap();
    assert!(again.already_decided);

    // 续跑：同一个调用，带上它停在的那条审批。
    let mut resumed = stopped.remaining;
    resumed[0].approval = Some(approval.clone());
    resumed[0].resumed = Some(resumed_from(ToolCallState::Planned, None, 1));
    let done = executor.execute_round(resumed.clone(), &env).await.unwrap();
    assert!(done.stop.is_none(), "{:?}", done.stop);
    assert_eq!(tool.ran(), 1);
    assert_eq!(done.results[0].call_id, call);

    // 同一条审批被再派发一次（唤醒重投、两个界面同时答）——它不是一次"确定没跑过"的
    // 续跑，所以用过的一次性授权换不来第二次执行，只会重新问人。
    let mut repeated = resumed;
    repeated[0].resumed = None;
    let third = executor.execute_round(repeated, &env).await.unwrap();
    assert_eq!(tool.ran(), 1, "同一条审批不能换来第二次执行");
    assert!(
        matches!(third.stop, Some(RoundStop::Approval { .. })),
        "用过的审批要重新问人：{:?}",
        third.stop
    );
}

/// ③ Deny 不被授权覆盖，而且**不消费任何授权**。
#[tokio::test]
async fn a_deny_is_not_overridden_by_a_matching_grant_and_consumes_nothing() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::shell());

    // 一张对任意 shell 明确 Deny 的表。
    let mut table = RuleTable::empty();
    table.rules.push(komo_kernel::policy::PolicyRule {
        id: "no-shell".into(),
        effect: komo_kernel::policy::Effect::Deny,
        reason: "这台机器上不允许跑 shell".into(),
        matcher: Matcher::operations([OperationMatch::ShellCommand]),
        scopes: vec![],
        requires_isolation: false,
    });
    let executor = harness.executor(vec![tool.clone()], PolicyEngine::from_rules(table));

    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[("shell", serde_json::json!({ "command": "rm -rf /tmp/x" }))],
        )
        .await;
    let env = harness.env(&session, &run);

    // 先造一条**正好覆盖这份计划**的 Run 授权，外加一条已批准的审批。
    let plan = tool
        .prepare(
            serde_json::json!({ "command": "rm -rf /tmp/x" }),
            &komo_kernel::types::tool::ToolContext {
                session: session.clone(),
                run: run.clone(),
                call: calls[0].call.clone(),
                attempt: AttemptId::from_raw("x"),
                source: env.source.clone(),
                cwd: env.cwd.clone(),
                roots: env.roots.clone(),
                env_version: None,
                resumed: None,
                cancel: CancelToken::new(),
            },
        )
        .await
        .unwrap();
    let approval = ApprovalId::from_raw("ap-1");
    harness
        .approvals
        .create(komo_kernel::protocol::http::ApprovalRecord {
            approval: approval.clone(),
            short_id: komo_kernel::types::ids::ShortId::from_index(1),
            session: session.clone(),
            run: Some(run.clone()),
            call: Some(calls[0].call.clone()),
            plan_hash: plan.plan_hash(),
            plan: plan.clone(),
            reason: "人已经批过了".into(),
            changes: None,
            evidence: None,
            scopes: vec![ApprovalScope::Run],
            requested_at: harness.clock.now(),
            valid_until: None,
            decision: Some(ApprovalDecisionRecord {
                approved: true,
                scope: ApprovalScope::Run,
                by: None,
                decided_at: harness.clock.now(),
                grant: Some(GrantId::from_raw("g-1")),
                consumed: false,
            }),
        })
        .await
        .unwrap();
    harness.approvals.add_grant(Grant {
        id: GrantId::from_raw("g-1"),
        approval: approval.clone(),
        scope: GrantScope::Run {
            run: run.clone(),
            matcher: Matcher::operations([OperationMatch::ShellCommand]),
            versions: Default::default(),
        },
        granted_at: harness.clock.now(),
        valid_until: None,
        consumed: false,
        reason: "本次 Run 内可以跑 shell".into(),
    });

    let outcome = executor.execute_round(calls, &env).await.unwrap();

    assert!(outcome.stop.is_none(), "拒绝是结果，不是暂停");
    assert_eq!(tool.ran(), 0);
    let result = &outcome.results[0];
    assert!(result.is_error);
    assert!(result.content.contains("被拒绝"), "{}", result.content);

    // 一条授权都没被消费：授权仍然未用，审批的决定也没被标记消费过。
    let grants = harness
        .approvals
        .grants_for_run(&run, harness.clock.now())
        .await
        .unwrap();
    assert!(grants.iter().all(|grant| !grant.consumed), "{grants:?}");
    let record = harness.approvals.get(&approval).await.unwrap().unwrap();
    assert!(
        !record.decision.unwrap().consumed,
        "Deny 之后不该有任何一次 consume"
    );
}

/// ④ 文件版本冲突作为结果看得见。
#[tokio::test]
async fn a_file_version_conflict_comes_back_to_the_model() {
    let harness = Harness::new();
    let executor = harness.permissive(vec![Arc::new(WriteTool::new())]);
    let (session, run) = harness.open_run().await;
    std::fs::write(harness.dir.path().join("a.txt"), "old\n").unwrap();

    let calls = harness
        .record_round(
            &run,
            &[(
                "write",
                serde_json::json!({ "path": "a.txt", "content": "new\n" }),
            )],
        )
        .await;
    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    let result = &outcome.results[0];
    assert!(result.is_error);
    assert!(result.content.contains("版本冲突"), "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(harness.dir.path().join("a.txt")).unwrap(),
        "old\n"
    );
}

/// ⑥ 未知工具名无法调用——作为错误内容回给模型。
#[tokio::test]
async fn an_unknown_tool_name_cannot_be_called() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::shell());
    let executor = harness.permissive(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("rm_rf", serde_json::json!({}))])
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert!(outcome.stop.is_none());
    let result = &outcome.results[0];
    assert!(result.is_error);
    assert!(
        result.content.contains("没有名为 rm_rf 的工具"),
        "{}",
        result.content
    );
    assert_eq!(tool.ran(), 0);
    // 账本里没有为它写下任何计划或尝试。
    assert!(
        harness.ledger.events().iter().all(|event| !matches!(
            event.payload,
            komo_kernel::events::EventPayload::ToolPlanned(_)
        )),
        "未知工具不该留下 tool.planned"
    );
}

/// ⑦ 恢复执行：started 而无结果 + 核对答不上来 → **不重跑**，交人。
#[tokio::test]
async fn a_started_call_whose_verification_is_unknown_is_not_rerun() {
    let harness = Harness::new();
    let tool = Arc::new(
        RecordingTool::shell()
            .with_recovery(RecoveryMode::VerifyTarget)
            .with_verdict(Verification::Unknown {
                reason: "远端没给幂等键".into(),
            }),
    );
    let executor = harness.permissive(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let mut calls = harness
        .record_round(&run, &[("shell", serde_json::json!({ "command": "x" }))])
        .await;
    calls[0].resumed = Some(resumed_from(
        ToolCallState::Started,
        Some(AttemptId::from_raw("attempt-1")),
        1,
    ));

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert_eq!(tool.ran(), 0, "核对不出结论就不能重跑");
    assert_eq!(tool.verified(), 1);
    let Some(RoundStop::Attention { reason, .. }) = &outcome.stop else {
        panic!("{:?}", outcome.stop)
    };
    assert!(reason.contains("核对不出结论"), "{reason}");
}

/// ⑦（另一半）planned = 确定尚未执行 → 直接执行一次，不核对。
#[tokio::test]
async fn a_planned_call_is_known_not_to_have_run_and_executes_once() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::shell().with_recovery(RecoveryMode::VerifyTarget));
    let executor = harness.permissive(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let mut calls = harness
        .record_round(&run, &[("shell", serde_json::json!({ "command": "x" }))])
        .await;
    calls[0].resumed = Some(resumed_from(ToolCallState::Planned, None, 1));

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
    assert_eq!(tool.ran(), 1);
    assert_eq!(tool.verified(), 0, "确定没跑过就不必核对");
}

/// 核对说"目标已满足"→ 报告结论，**不重做**。
#[tokio::test]
async fn a_satisfied_target_is_reported_not_redone() {
    let harness = Harness::new();
    let tool = Arc::new(
        RecordingTool::shell()
            .with_recovery(RecoveryMode::VerifyTarget)
            .with_verdict(Verification::AlreadySatisfied {
                evidence: "内容哈希已是预期值".into(),
            }),
    );
    let executor = harness.permissive(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let mut calls = harness
        .record_round(&run, &[("shell", serde_json::json!({ "command": "x" }))])
        .await;
    calls[0].resumed = Some(resumed_from(
        ToolCallState::Started,
        Some(AttemptId::from_raw("attempt-1")),
        1,
    ));

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();
    assert_eq!(tool.ran(), 0);
    assert!(!outcome.results[0].is_error);
    assert!(
        outcome.results[0].content.contains("核对后目标已满足"),
        "{}",
        outcome.results[0].content
    );
}

/// 可安全重做的读取不必核对——§8.6 表的第一行。
#[tokio::test]
async fn a_safe_reread_is_redone_without_a_verification() {
    let harness = Harness::new();
    let tool = Arc::new(
        RecordingTool::new("read", Operation::ReadFile).with_recovery(RecoveryMode::SafeReread),
    );
    let executor = harness.permissive(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let mut calls = harness
        .record_round(&run, &[("read", serde_json::json!({ "path": "a.txt" }))])
        .await;
    calls[0].resumed = Some(resumed_from(
        ToolCallState::Started,
        Some(AttemptId::from_raw("attempt-1")),
        1,
    ));

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();
    assert!(outcome.stop.is_none());
    assert_eq!(tool.ran(), 1);
    assert_eq!(tool.verified(), 0);
}

/// 结果不明不能被当成失败重试掉——它停下来交给人（§6、§8.6）。
#[tokio::test]
async fn an_uncertain_result_stops_for_a_human_instead_of_retrying() {
    let harness = Harness::new();
    let tool = Arc::new(
        RecordingTool::shell().with_outcome(Err(ToolError::Uncertain {
            message: "请求已发出，响应丢了".into(),
        })),
    );
    let executor = harness.permissive(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("shell", serde_json::json!({ "command": "x" }))])
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert_eq!(tool.ran(), 1, "跑过一次");
    assert!(
        matches!(outcome.stop, Some(RoundStop::Attention { .. })),
        "{:?}",
        outcome.stop
    );
    // 这段窗口留在账本里：状态是 uncertain，不是 failed。
    let uncertain = harness.ledger.events().iter().any(|event| {
        matches!(&event.payload, komo_kernel::events::EventPayload::ToolResult(result)
            if result.status == ToolResultStatus::Uncertain)
    });
    assert!(uncertain, "uncertain 必须留在账本上");
}

/// ⑩ 预览 ≤ 1 KiB，完整输出走 `ToolOutputStore`。
#[tokio::test]
async fn the_preview_is_bounded_and_the_whole_output_lives_in_the_store() {
    let harness = Harness::new();
    let body = "x".repeat(100_000);
    let tool = Arc::new(RecordingTool::shell().with_outcome(Ok(ToolOutput {
        status: ToolResultStatus::Completed,
        result: serde_json::json!({ "stdout": body.clone() }),
        exit_code: Some(0),
        artifacts: vec![],
        preview: Some(body.clone()),
    })));
    let executor = harness.permissive(vec![tool]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("shell", serde_json::json!({ "command": "x" }))])
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    // 账本里的预览有上限。
    let published = harness
        .ledger
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            komo_kernel::events::EventPayload::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("有一条 tool.result");
    let preview = published.preview.expect("有预览");
    assert!(
        preview.len() <= PREVIEW_LIMIT_BYTES,
        "预览 {} 字节，超过 1 KiB",
        preview.len()
    );

    // 交给模型的正文也有界，而且指得出去哪读全的。
    let content = &outcome.results[0].content;
    assert!(
        content.len() <= PREVIEW_LIMIT_BYTES + 128,
        "{}",
        content.len()
    );
    assert!(content.contains("[完整输出："), "{content}");

    // 完整正文确实在输出存储里。
    let stored = harness
        .outputs
        .published_body(published.output_ref.path())
        .expect("输出存储里有它");
    assert_eq!(stored.result["stdout"], serde_json::json!(body));
}

/// 取消停在当场：后面的调用一个都不派发。
#[tokio::test]
async fn cancelling_leaves_the_rest_of_the_round_undispatched() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::shell());
    let executor = harness.permissive(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[
                ("shell", serde_json::json!({ "command": "a" })),
                ("shell", serde_json::json!({ "command": "b" })),
            ],
        )
        .await;
    let cancel = CancelToken::new();
    cancel.cancel();

    let outcome = executor
        .execute_round(calls, &harness.env_with_cancel(&session, &run, cancel))
        .await
        .unwrap();
    assert!(matches!(outcome.stop, Some(RoundStop::Cancelled)));
    assert_eq!(tool.ran(), 0);
    assert_eq!(outcome.remaining.len(), 2);
}

/// 一轮里的多个调用**顺序**执行（§6：减少文件操作顺序歧义）。
#[tokio::test]
async fn several_calls_in_one_round_run_in_order() {
    let harness = Harness::new();
    let executor = harness.permissive(vec![Arc::new(WriteTool::new()), Arc::new(ReadTool::new())]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[
                (
                    "write",
                    serde_json::json!({ "path": "a.txt", "content": "one\n" }),
                ),
                ("read", serde_json::json!({ "path": "a.txt" })),
            ],
        )
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();
    assert_eq!(outcome.results.len(), 2);
    assert!(
        !outcome.results[1].is_error,
        "{}",
        outcome.results[1].content
    );
    assert!(
        outcome.results[1].content.contains("共 1 行"),
        "第二个调用读到了第一个写下的东西：{}",
        outcome.results[1].content
    );
}

/// 放行是**哪一条授权**给的，要记进账本——事后答得出"这次凭什么跑的"。
#[tokio::test]
async fn the_grant_that_allowed_a_call_is_recorded_on_the_attempt() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::shell());
    let executor = harness.initial(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[("shell", serde_json::json!({ "command": "rm -rf /tmp/x" }))],
        )
        .await;
    let env = harness.env(&session, &run);

    let stopped = executor.execute_round(calls, &env).await.unwrap();
    let Some(RoundStop::Approval { approval, .. }) = stopped.stop.clone() else {
        panic!()
    };
    // 批准，并挂一条 Run 范围授权。
    let grant = GrantId::from_raw("g-1");
    harness.approvals.add_grant(Grant {
        id: grant.clone(),
        approval: approval.clone(),
        scope: GrantScope::Run {
            run: run.clone(),
            matcher: Matcher::operations([OperationMatch::ShellCommand]),
            versions: Default::default(),
        },
        granted_at: harness.clock.now(),
        valid_until: None,
        consumed: false,
        reason: "本次 Run".into(),
    });
    harness
        .approvals
        .decide(
            &approval,
            ApprovalDecisionRecord {
                approved: true,
                scope: ApprovalScope::Run,
                by: None,
                decided_at: harness.clock.now(),
                grant: Some(grant.clone()),
                consumed: false,
            },
        )
        .await
        .unwrap();

    let mut resumed = stopped.remaining;
    resumed[0].approval = Some(approval);
    resumed[0].resumed = Some(resumed_from(ToolCallState::Planned, None, 1));
    executor.execute_round(resumed, &env).await.unwrap();

    let recorded = harness
        .ledger
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            komo_kernel::events::EventPayload::ToolStarted(started) => started.grant.clone(),
            _ => None,
        });
    assert_eq!(recorded, Some(grant));
}

/// 工具的普通失败是**结果**，不是执行器的错误。
#[tokio::test]
async fn an_ordinary_tool_failure_is_a_result_the_model_can_act_on() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::shell().with_outcome(Err(ToolError::Failed {
        message: "命令不存在".into(),
    })));
    let executor = harness.permissive(vec![tool]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("shell", serde_json::json!({ "command": "nope" }))])
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();
    assert!(outcome.stop.is_none());
    assert!(outcome.results[0].is_error);
    assert!(outcome.results[0].content.contains("命令不存在"));
}

/// `prepare` 失败也是结果——模型改了参数再来。
#[tokio::test]
async fn bad_arguments_come_back_as_a_result_without_touching_the_ledger() {
    let harness = Harness::new();
    let executor = harness.permissive(vec![Arc::new(ReadTool::new())]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("read", serde_json::json!({ "nope": 1 }))])
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();
    assert!(outcome.results[0].is_error);
    assert!(outcome.results[0].content.contains("参数不合法"));
    assert_eq!(
        harness
            .ledger
            .events()
            .iter()
            .filter(|event| matches!(
                event.payload,
                komo_kernel::events::EventPayload::ToolStarted(_)
            ))
            .count(),
        0,
        "没放行就没有 tool.started"
    );
}

/// 执行顺序：`tool.planned` → `tool.started` → `tool.result`，**先发布输出再写结果**。
#[tokio::test]
async fn the_ledger_sees_planned_then_started_then_result() {
    let harness = Harness::new();
    let executor = harness.permissive(vec![Arc::new(RecordingTool::shell())]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("shell", serde_json::json!({ "command": "x" }))])
        .await;
    executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    let order: Vec<&str> = harness
        .ledger
        .events()
        .iter()
        .filter_map(|event| match &event.payload {
            komo_kernel::events::EventPayload::ToolPlanned(_) => Some("planned"),
            komo_kernel::events::EventPayload::ToolStarted(_) => Some("started"),
            komo_kernel::events::EventPayload::ToolResult(_) => Some("result"),
            _ => None,
        })
        .collect();
    assert_eq!(order, vec!["planned", "started", "result"]);
    assert_eq!(tool_executions(&harness), 1, "一个调用一次尝试");
}

fn tool_executions(harness: &Harness) -> usize {
    harness
        .ledger
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                komo_kernel::events::EventPayload::ToolStarted(_)
            )
        })
        .count()
}

/// 计划在 `start_call` 之前就已落盘——"确定尚未执行"是一个可分辨的状态（§8.4 第 6 行）。
#[tokio::test]
async fn an_ask_leaves_a_planned_call_behind_with_no_attempt() {
    let harness = Harness::new();
    let executor = harness.initial(vec![Arc::new(RecordingTool::shell())]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("shell", serde_json::json!({ "command": "x" }))])
        .await;
    executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    let surface = harness.ledger.surface();
    let call = surface.calls.values().next().expect("有一个调用");
    assert_eq!(call.state, ToolCallState::Planned);
    assert!(call.attempt.is_none());
    assert_eq!(call.attempts, 0, "停在审批上的调用一次尝试都没有过");
}

/// 恢复执行沿用**同一份**计划：重新 prepare 会换掉计划哈希，原授权就覆盖不到了。
#[tokio::test]
async fn a_resumed_call_reuses_the_plan_it_was_approved_for() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::shell());
    let executor = harness.initial(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("shell", serde_json::json!({ "command": "x" }))])
        .await;
    let env = harness.env(&session, &run);

    let stopped = executor.execute_round(calls, &env).await.unwrap();
    let pending = harness
        .approvals
        .list_pending(Some(&session))
        .await
        .unwrap();
    let approved_hash = pending[0].plan_hash.clone();

    harness
        .gate
        .decide(&pending[0].approval, true, ApprovalScope::Once, None)
        .await
        .unwrap();

    let mut resumed = stopped.remaining;
    resumed[0].approval = Some(pending[0].approval.clone());
    resumed[0].plan = Some(pending[0].plan.clone());
    resumed[0].resumed = Some(resumed_from(ToolCallState::Planned, None, 1));
    executor.execute_round(resumed, &env).await.unwrap();

    assert_eq!(tool.ran(), 1);
    let started_hash = harness
        .ledger
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            komo_kernel::events::EventPayload::ToolStarted(started) => {
                Some(started.plan_hash.clone())
            }
            _ => None,
        })
        .expect("有 tool.started");
    assert_eq!(started_hash, approved_hash, "跑的就是批过的那份计划");
    assert_eq!(
        harness
            .ledger
            .events()
            .iter()
            .filter(|event| matches!(
                event.payload,
                komo_kernel::events::EventPayload::ToolPlanned(_)
            ))
            .count(),
        1,
        "续跑不重新写一份计划"
    );
}

#[tokio::test]
async fn the_executor_publishes_the_definitions_of_what_it_holds() {
    let harness = Harness::new();
    let executor = harness.permissive(vec![Arc::new(ReadTool::new()), Arc::new(WriteTool::new())]);
    let names: Vec<String> = executor
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert_eq!(names, vec!["read".to_string(), "write".to_string()]);
}
