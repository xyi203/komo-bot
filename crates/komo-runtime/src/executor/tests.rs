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
use super::{CallRequest, OperatorVerdict, RoundStop, resumed_from};

/// 这个调用现在这一刻的计划——`crashed_attempt` 要用它写 `tool.planned`。
async fn plan_of(
    tool: &dyn Tool,
    harness: &Harness,
    session: &komo_kernel::types::ids::SessionId,
    run: &komo_kernel::types::ids::RunId,
    request: &CallRequest,
) -> komo_kernel::types::plan::ExecutionPlan {
    tool.prepare(
        request.arguments.clone(),
        &harness.tool_context(session, run, &request.call),
    )
    .await
    .expect("现做一份计划")
}
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
    let previous = harness
        .crashed_attempt(
            &calls[0].call,
            &plan_of(&*tool, &harness, &session, &run, &calls[0]).await,
        )
        .await;
    calls[0].resumed = Some(resumed_from(
        ToolCallState::Started,
        Some(previous.clone()),
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

    // 上一世那条 `tool.started` 配上了一条明确的 uncertain——不写下来它会永远悬着。
    let results = harness.results_for(&previous);
    assert_eq!(results.len(), 1, "有且只有一条结果：{results:?}");
    assert_eq!(results[0].status, ToolResultStatus::Uncertain);
    assert_eq!(
        harness.ledger.surface().calls[&results[0].call_id].state,
        ToolCallState::Uncertain
    );
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

    // 只 planned 过，没有上一次尝试要收尾；这一次自己的那条结果照常落下。
    let started: Vec<AttemptId> = harness
        .ledger
        .events()
        .iter()
        .filter_map(|event| match &event.payload {
            komo_kernel::events::EventPayload::ToolStarted(started) => {
                Some(started.attempt_id.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 1, "只有这一次尝试");
    assert_eq!(harness.results_for(&started[0]).len(), 1);
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
    let previous = harness
        .crashed_attempt(
            &calls[0].call,
            &plan_of(&*tool, &harness, &session, &run, &calls[0]).await,
        )
        .await;
    calls[0].resumed = Some(resumed_from(
        ToolCallState::Started,
        Some(previous.clone()),
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

    // 核对结论就是那次尝试的结果：一条，completed。
    let results = harness.results_for(&previous);
    assert_eq!(results.len(), 1, "有且只有一条结果：{results:?}");
    assert_eq!(results[0].status, ToolResultStatus::Completed);
    assert_eq!(
        harness.ledger.surface().calls[&results[0].call_id].state,
        ToolCallState::Completed
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
    let previous = harness
        .crashed_attempt(
            &calls[0].call,
            &plan_of(&*tool, &harness, &session, &run, &calls[0]).await,
        )
        .await;
    calls[0].resumed = Some(resumed_from(
        ToolCallState::Started,
        Some(previous.clone()),
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

/// §7.5：操作者对一次「结果不明」的调用下结论——**给那次尝试补一条结果，不重跑工具**。
///
/// 这条测试盯的是三个可观察的结果：工具只跑过一次（核对不是重做）、那次尝试上多了一条
/// 结果（不再悬着）、正文里带着**证据**（事后答得出这一条凭什么算完了）。
#[tokio::test]
async fn an_operator_verdict_settles_the_uncertain_attempt_without_rerunning_it() {
    let harness = Harness::new();
    let tool = Arc::new(
        RecordingTool::shell().with_outcome(Err(ToolError::Uncertain {
            message: "解释器没有写下结果".into(),
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
    assert_eq!(tool.ran(), 1);
    let call = match &outcome.stop {
        Some(RoundStop::Attention { call, .. }) => call.clone(),
        other => panic!("{other:?}"),
    };
    let attempt = harness.ledger.surface().calls[&call]
        .attempt
        .clone()
        .expect("那次尝试记在账上");

    let status = executor
        .settle_by_operator(
            &session,
            &run,
            &call,
            &attempt,
            OperatorVerdict::AlreadySatisfied {
                evidence: "HA 读回来是 off".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(status, ToolResultStatus::Completed);
    assert_eq!(tool.ran(), 1, "核对不是重跑");
    // 账本是**追加**的（§8.3）：原来那条 `uncertain` 留着不删，结论写在它后面，
    // 由读者取最后一条。所以这里数的是"最后一条是谁"，而不是"只有一条"。
    let results = harness.results_for(&attempt);
    assert_eq!(results.len(), 2, "原来那条结果不会被擦掉：{results:?}");
    let settled = results.last().expect("有结论");
    assert_eq!(settled.status, ToolResultStatus::Completed);
    assert_eq!(
        settled.attempt_id, attempt,
        "结论落在**原来那次尝试**上，不是新开一次（§8.6）"
    );
    assert_eq!(
        harness.ledger.surface().calls[&call].state,
        ToolCallState::Completed,
        "这条调用不再悬着"
    );
    let stored = harness
        .outputs
        .published_body(settled.output_ref.path())
        .expect("输出存储里有它");
    assert!(
        stored.error.is_none()
            && stored.result["verdict"] == serde_json::json!("already_satisfied"),
        "正文里留得下这是谁下的什么结论：{stored:?}"
    );
    assert!(
        settled
            .preview
            .as_deref()
            .unwrap_or_default()
            .contains("HA 读回来是 off"),
        "证据要进正文：{:?}",
        settled.preview
    );
}

/// 「确定没执行」是**失败**收尾：重做要走新的一次调用，因此照常过 Policy——旧的一次性
/// 授权不会被当成重试许可（§7.4）。
#[tokio::test]
async fn not_performed_closes_the_call_as_failed_so_a_retry_is_a_new_call() {
    let harness = Harness::new();
    let tool = Arc::new(
        RecordingTool::shell().with_outcome(Err(ToolError::Uncertain {
            message: "结果不明".into(),
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
    let call = match &outcome.stop {
        Some(RoundStop::Attention { call, .. }) => call.clone(),
        other => panic!("{other:?}"),
    };
    let attempt = harness.ledger.surface().calls[&call]
        .attempt
        .clone()
        .unwrap();

    let verdict = OperatorVerdict::NotPerformed {
        evidence: "远端没有这条记录".into(),
    };
    assert_eq!(verdict.status(), ToolResultStatus::Failed);
    assert_eq!(
        verdict.as_verification(),
        Verification::NotPerformed {
            evidence: "远端没有这条记录".into()
        },
        "续跑时它读的形状就是 §8.6 的核对结论"
    );

    let status = executor
        .settle_by_operator(&session, &run, &call, &attempt, verdict)
        .await
        .unwrap();
    assert_eq!(status, ToolResultStatus::Failed);
    let results = harness.results_for(&attempt);
    let settled = results.last().expect("有结论");
    assert_eq!(
        settled.status,
        ToolResultStatus::Failed,
        "最后一条才是这次尝试的结论（前面那条是工具报的 uncertain）"
    );
    assert!(
        settled
            .preview
            .as_deref()
            .unwrap_or_default()
            .contains("确定没有执行"),
        "{:?}",
        settled.preview
    );
    assert_eq!(
        harness.ledger.surface().calls[&call].state,
        ToolCallState::Failed,
        "读账本的人看到的是这个调用已经收口"
    );
    assert_eq!(tool.ran(), 1, "结论只写账，不去重做");
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

/// 委派（§4、§8.4 的 `dependency`）：一次子任务怎么变成一条子 Run、父调用怎么收尾。
///
/// 这一组全部用 [`MemLedger`]，因为要断言的正是**账本里发生了什么**——子 Run 的受理事件、
/// 父调用那条悬着的 `tool.started`、落回那次尝试的结果。折出来的视图与事件都查得到。
mod delegation {
    use super::*;

    use komo_kernel::events::EventPayload;
    use komo_kernel::traits::{Ledger, ToolOutputStore};
    use komo_kernel::types::delegate::{DelegateSpec, SchemaMode};
    use komo_kernel::types::ids::{RunId, ToolCallId};
    use komo_kernel::types::plan::ExecutionPlan;
    use komo_kernel::types::status::RunEnd;
    use serde_json::json;

    use crate::tools::DelegateTool;

    /// 账本里落过盘的那份计划——续跑时调用方手里拿到的就是它（§7.4：重新 prepare 会
    /// 换一个哈希，原授权就覆盖不到了）。
    fn recorded_plan(harness: &Harness, call: &ToolCallId) -> ExecutionPlan {
        harness
            .ledger
            .events()
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::ToolPlanned(planned) if &planned.call_id == call => {
                    planned.plan.as_deref().cloned()
                }
                _ => None,
            })
            .expect("计划已经落过盘")
    }

    /// 账本里全部**子 Run**。派一条子 Run 恰好写一条带 `delegate` 的 `run.accepted`，
    /// 所以它是"派出去几条"的权威答案——比数内存里的什么列表都强。
    fn child_runs(harness: &Harness) -> Vec<RunId> {
        harness
            .ledger
            .events()
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::RunAccepted(accepted) if accepted.delegate.is_some() => {
                    event.run.clone()
                }
                _ => None,
            })
            .collect()
    }

    fn results_for_call(harness: &Harness, call: &ToolCallId) -> usize {
        harness
            .ledger
            .events()
            .iter()
            .filter(|event| {
                matches!(&event.payload, EventPayload::ToolResult(result) if &result.call_id == call)
            })
            .count()
    }

    /// 一条父 Run + 一次 `delegate` 调用，策略全放行。
    async fn delegate_round(
        harness: &Harness,
        args: serde_json::Value,
    ) -> (komo_kernel::types::ids::SessionId, RunId, CallRequest) {
        let (session, run) = harness.open_run().await;
        let calls = harness.record_round(&run, &[("delegate", args)]).await;
        (session, run, calls.into_iter().next().expect("一个调用"))
    }

    /// 从"派出去"走到"回来收口"之间那一步：把父调用的形状补成账本会给的形状。
    fn resumed_request(
        harness: &Harness,
        request: &CallRequest,
        attempt: &AttemptId,
    ) -> CallRequest {
        let mut resumed = request.clone();
        resumed.plan = Some(recorded_plan(harness, &request.call));
        resumed.resumed = Some(resumed_from(
            ToolCallState::Started,
            Some(attempt.clone()),
            1,
        ));
        resumed
    }

    /// 那次已经 `start_call` 过的尝试。
    fn attempt_of(harness: &Harness, call: &ToolCallId) -> AttemptId {
        harness.ledger.surface().calls[call]
            .attempt
            .clone()
            .expect("已经 start_call 过")
    }

    /// ① 一次委派 = 一条子 Run + 父调用进入等待。**这条调用没有结果**：它在等子 Run，
    /// 而"在等"既不是失败也不是"做完了但什么都没写"。
    #[tokio::test]
    async fn a_delegated_task_becomes_a_child_run_and_the_call_waits() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let (session, run, request) = delegate_round(
            &harness,
            json!({
                "task": "把 a.txt 里的小数点都改成逗号",
                "rounds": 3,
                "output_schema": { "type": "object" }
            }),
        )
        .await;

        let outcome = executor
            .execute_round(vec![request.clone()], &harness.env(&session, &run))
            .await
            .unwrap();

        let Some(RoundStop::Dependency { run: child, call }) = outcome.stop.clone() else {
            panic!("{:?}", outcome.stop)
        };
        assert_eq!(call, request.call);
        assert!(
            outcome.results.is_empty(),
            "在等的调用没有结果：{:?}",
            outcome.results
        );
        assert_eq!(
            results_for_call(&harness, &request.call),
            0,
            "没有 tool.result"
        );

        // 受理事件带着**整份** spec：重启之后它是子代理唯一的"结果要长什么样"的依据。
        let specs: Vec<DelegateSpec> = harness
            .ledger
            .accepted()
            .into_iter()
            .filter_map(|input| input.delegate)
            .collect();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].parent, run);
        assert_eq!(specs[0].call, request.call);
        assert_eq!(specs[0].task, "把 a.txt 里的小数点都改成逗号");
        assert_eq!(specs[0].rounds, 3);
        assert!(specs[0].contract.is_some(), "契约跟着事件一起落盘");

        // 折出来的视图认得出这条边：父视图靠它知道"哪些 Run 是我的子代理"，
        // 子 Run 靠它拿到结果契约。
        let surface = harness.ledger.surface();
        assert_eq!(surface.runs[&child].delegate.as_ref(), Some(&specs[0]));
        assert_eq!(child_runs(&harness), vec![child.clone()]);
        // 调用已经 `tool.started`、还没有结果——它悬着是对的：子 Run 会把它收口。
        assert_eq!(surface.calls[&request.call].state, ToolCallState::Started);
        assert!(surface.calls[&request.call].output.is_none());
    }

    /// ② 子 Run 完成 + 结果合契约 → 调用按 Completed 收尾，结果里带子 Run id 与数据。
    #[tokio::test]
    async fn a_completed_child_settles_the_call_with_its_result() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let (session, run, request) = delegate_round(
            &harness,
            json!({
                "task": "数一下 /tmp 下有几个文件",
                "output_schema": {
                    "type": "object",
                    "required": ["count"],
                    "properties": { "count": { "type": "integer" } }
                }
            }),
        )
        .await;
        let env = harness.env(&session, &run);
        let outcome = executor
            .execute_round(vec![request.clone()], &env)
            .await
            .unwrap();
        let Some(RoundStop::Dependency { run: child, .. }) = outcome.stop else {
            panic!("{:?}", outcome.stop)
        };
        let attempt = attempt_of(&harness, &request.call);

        // 子代理把结果放进**最后一条回复**：围栏与前后闲话都要容忍。
        harness
            .ledger
            .complete(
                &child,
                RunEnd::Completed {
                    final_message: Some("数完了：\n```json\n{\"count\": 3}\n```\n以上".into()),
                    rounds: 2,
                },
            )
            .await
            .unwrap();

        let done = executor
            .execute_round(vec![resumed_request(&harness, &request, &attempt)], &env)
            .await
            .unwrap();

        assert!(done.stop.is_none(), "{:?}", done.stop);
        assert_eq!(done.results.len(), 1);
        assert!(!done.results[0].is_error);
        assert!(
            done.results[0].content.contains(child.as_str()),
            "父侧要看得见是哪条子 Run：{}",
            done.results[0].content
        );

        // 结果落回**那次尝试**上：子 Run id 与数据都在正文里——父侧的模型只看得见这条
        // 工具结果，它自己去读那条 Run 的完整过程。
        let results = harness.results_for(&attempt);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, ToolResultStatus::Completed);
        let body = harness
            .outputs
            .open(&results[0].output_ref)
            .await
            .unwrap()
            .body;
        assert_eq!(body.result["run"], json!(child));
        assert_eq!(body.result["status"], json!("completed"));
        assert_eq!(body.result["result"]["count"], json!(3));
        assert!(body.error.is_none());
    }

    /// ③ 结果不合契约：permissive 放行但**标记出来**，strict 判这次委派失败。
    /// 同一份结果、同一份校验器，差的只是怎么收口。
    #[tokio::test]
    async fn an_off_contract_result_is_marked_in_permissive_and_fails_in_strict() {
        for mode in [SchemaMode::Permissive, SchemaMode::Strict] {
            let harness = Harness::new();
            let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
            let (session, run, request) = delegate_round(
                &harness,
                json!({
                    "task": "数一下 /tmp 下有几个文件",
                    "output_schema": {
                        "type": "object",
                        "required": ["count"],
                        "properties": { "count": { "type": "integer" } }
                    },
                    "schema_mode": mode
                }),
            )
            .await;
            let env = harness.env(&session, &run);
            let outcome = executor
                .execute_round(vec![request.clone()], &env)
                .await
                .unwrap();
            let Some(RoundStop::Dependency { run: child, .. }) = outcome.stop else {
                panic!("{:?}", outcome.stop)
            };
            let attempt = attempt_of(&harness, &request.call);
            harness
                .ledger
                .complete(
                    &child,
                    RunEnd::Completed {
                        final_message: Some("{\"files\": []}".into()),
                        rounds: 1,
                    },
                )
                .await
                .unwrap();

            let done = executor
                .execute_round(vec![resumed_request(&harness, &request, &attempt)], &env)
                .await
                .unwrap();
            let body = harness
                .outputs
                .open(&harness.results_for(&attempt)[0].output_ref)
                .await
                .unwrap()
                .body;
            let content = &done.results[0].content;

            match mode {
                SchemaMode::Permissive => {
                    assert_eq!(body.status, ToolResultStatus::Completed, "放行");
                    assert_eq!(body.result["schema_overridden"], json!(true), "标记出来");
                    assert!(!done.results[0].is_error);
                    assert!(content.contains("不符合契约"), "{content}");
                }
                SchemaMode::Strict => {
                    assert_eq!(body.status, ToolResultStatus::Failed);
                    assert!(done.results[0].is_error);
                    assert_ne!(body.result["schema_overridden"], json!(true));
                    assert!(
                        body.error.as_deref().unwrap_or_default().contains("契约"),
                        "{:?}",
                        body.error
                    );
                    assert!(content.contains(child.as_str()), "{content}");
                }
            }
        }
    }

    /// ④ 子 Run 没跑成 → 调用跟着失败，理由里写明子 Run 是**怎么**结束的。
    #[tokio::test]
    async fn a_child_that_did_not_finish_fails_the_call_with_its_ending() {
        let endings = [
            (
                RunEnd::Failed {
                    reason: "工具连续失败".into(),
                },
                "failed",
            ),
            (RunEnd::Cancelled { by: None }, "cancelled"),
            (
                RunEnd::Abandoned {
                    by: None,
                    reason: Some("没人再管它了".into()),
                },
                "abandoned",
            ),
        ];

        for (end, kind) in endings {
            let harness = Harness::new();
            let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
            let (session, run, request) =
                delegate_round(&harness, json!({ "task": "跑一遍检查" })).await;
            let env = harness.env(&session, &run);
            let outcome = executor
                .execute_round(vec![request.clone()], &env)
                .await
                .unwrap();
            let Some(RoundStop::Dependency { run: child, .. }) = outcome.stop else {
                panic!("{:?}", outcome.stop)
            };
            let attempt = attempt_of(&harness, &request.call);
            harness.ledger.complete(&child, end).await.unwrap();

            let done = executor
                .execute_round(vec![resumed_request(&harness, &request, &attempt)], &env)
                .await
                .unwrap();

            assert!(done.stop.is_none(), "{kind}：{:?}", done.stop);
            assert!(done.results[0].is_error, "{kind}");
            assert!(done.results[0].content.contains(child.as_str()), "{kind}");
            let body = harness
                .outputs
                .open(&harness.results_for(&attempt)[0].output_ref)
                .await
                .unwrap()
                .body;
            assert_eq!(body.status, ToolResultStatus::Failed, "{kind}");
            assert_eq!(body.result["status"], json!(kind));
            assert_eq!(body.result["run"], json!(child));
            // "失败了"这三个字既不说谁失败、也不说下一步该做什么——所以原因要写全。
            assert!(
                body.error
                    .as_deref()
                    .unwrap_or_default()
                    .contains(child.as_str()),
                "{kind}：{:?}",
                body.error
            );
        }
        // 子 Run 失败的原因要能读到（这里只看最后一条：三条路径共用同一段折算）。
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let (session, run, request) = delegate_round(&harness, json!({ "task": "干活" })).await;
        let env = harness.env(&session, &run);
        let outcome = executor
            .execute_round(vec![request.clone()], &env)
            .await
            .unwrap();
        let Some(RoundStop::Dependency { run: child, .. }) = outcome.stop else {
            panic!()
        };
        let attempt = attempt_of(&harness, &request.call);
        harness
            .ledger
            .complete(
                &child,
                RunEnd::Failed {
                    reason: "工具连续失败".into(),
                },
            )
            .await
            .unwrap();
        let done = executor
            .execute_round(vec![resumed_request(&harness, &request, &attempt)], &env)
            .await
            .unwrap();
        assert!(
            done.results[0].content.contains("工具连续失败"),
            "{:?}",
            done.results[0]
        );
    }

    /// ⑤ 子 Run 还在跑 → **又一次等待**，不是失败。它可能正停在一条审批上，也可能刚被
    /// 别的执行实例领走；这两种都不该把父侧叫醒成一个失败。
    #[tokio::test]
    async fn a_child_that_is_still_running_keeps_the_call_waiting() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let (session, run, request) = delegate_round(&harness, json!({ "task": "干活" })).await;
        let env = harness.env(&session, &run);
        let outcome = executor
            .execute_round(vec![request.clone()], &env)
            .await
            .unwrap();
        let Some(RoundStop::Dependency { run: child, .. }) = outcome.stop else {
            panic!("{:?}", outcome.stop)
        };
        let attempt = attempt_of(&harness, &request.call);

        // 子 Run 一条事件都还没有（没完成、没失败）——它还在跑。
        let again = executor
            .execute_round(vec![resumed_request(&harness, &request, &attempt)], &env)
            .await
            .unwrap();

        let Some(RoundStop::Dependency { run: waiting, .. }) = again.stop else {
            panic!("{:?}", again.stop)
        };
        assert_eq!(waiting, child, "等的还是同一条子 Run");
        assert!(again.results.is_empty());
        assert_eq!(results_for_call(&harness, &request.call), 0, "还是没有结果");
    }

    /// ⑥ 受理按 `request_key` 幂等：同一件委派再受理一次拿回的是**同一条**子 Run。
    /// "受理了但还没 start 就崩了"的续跑走的正是这条路——账本给的形状是 `planned`
    /// （确定没跑过），于是重走一遍首次路径。
    #[tokio::test]
    async fn accepting_the_same_delegation_twice_reuses_the_child_run() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let (session, run, request) =
            delegate_round(&harness, json!({ "task": "跑一遍检查" })).await;
        let env = harness.env(&session, &run);

        let first = executor
            .execute_round(vec![request.clone()], &env)
            .await
            .unwrap();
        let Some(RoundStop::Dependency {
            run: first_child, ..
        }) = first.stop
        else {
            panic!("{:?}", first.stop)
        };

        let mut again = request.clone();
        again.plan = Some(recorded_plan(&harness, &request.call));
        again.resumed = Some(resumed_from(ToolCallState::Planned, None, 1));
        let second = executor.execute_round(vec![again], &env).await.unwrap();

        let Some(RoundStop::Dependency {
            run: second_child, ..
        }) = second.stop
        else {
            panic!("{:?}", second.stop)
        };
        assert_eq!(second_child, first_child, "重来一次拿回的是同一条子 Run");
        assert_eq!(child_runs(&harness), vec![first_child], "没有多派出一条");
    }

    /// ⑦ 派出去这件事本身要过 Policy 与审批（§7.1）：没放行就不该在账本里留下一条
    /// 没人认领的子 Run。
    #[tokio::test]
    async fn an_unapproved_delegation_creates_no_child_run() {
        let harness = Harness::new();
        // §7.1 那张初始建议表里没有 delegate 这一行——它是默认的 Ask。
        let executor = harness.initial(vec![Arc::new(DelegateTool::new())]);
        let (session, run, request) = delegate_round(&harness, json!({ "task": "干活" })).await;
        let env = harness.env(&session, &run);

        let stopped = executor
            .execute_round(vec![request.clone()], &env)
            .await
            .unwrap();
        let Some(RoundStop::Approval { approval, .. }) = stopped.stop.clone() else {
            panic!("{:?}", stopped.stop)
        };
        assert!(
            child_runs(&harness).is_empty(),
            "审批之前一条子 Run 都不该有"
        );

        // 批准之后才受理。审批绑定的是那份计划，所以续跑沿用同一份（重新 prepare 会
        // 换一个哈希，原授权就覆盖不到了）。
        harness
            .gate
            .decide(&approval, true, ApprovalScope::Once, None)
            .await
            .unwrap();
        let mut resumed = request.clone();
        resumed.approval = Some(approval);
        resumed.plan = Some(recorded_plan(&harness, &request.call));
        resumed.resumed = Some(resumed_from(ToolCallState::Planned, None, 1));

        let done = executor.execute_round(vec![resumed], &env).await.unwrap();
        let Some(RoundStop::Dependency { run: child, .. }) = done.stop else {
            panic!("{:?}", done.stop)
        };
        assert_eq!(child_runs(&harness), vec![child]);
    }

    /// ⑧ 深度只有一层。守卫在**编排里**而不是在工具表里：把 `delegate` 从子代理的工具表
    /// 摘掉是 UX，模型自己拼出这个名字就绕过去了，而能被绕过的约束等于没有。
    #[tokio::test]
    async fn a_child_run_cannot_delegate_again() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        // 这条 Run 是被派的——`delegated` 说得出它是谁派出来的。
        let (session, run, request) =
            delegate_round(&harness, json!({ "task": "再派一层出去" })).await;
        let env = harness.child_env(
            &session,
            &run,
            DelegateSpec::new(
                RunId::from_raw("run-上层"),
                ToolCallId::from_raw("call-上层"),
                "上层任务",
            ),
        );

        let outcome = executor.execute_round(vec![request], &env).await.unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert_eq!(outcome.results.len(), 1);
        assert!(outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains("一层"),
            "{}",
            outcome.results[0].content
        );
        assert!(child_runs(&harness).is_empty(), "被派的 Run 不能再派一条");
    }
}
