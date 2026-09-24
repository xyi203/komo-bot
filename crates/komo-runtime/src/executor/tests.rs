//! executor 的验收（§14 阶段 2、3）。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use komo_kernel::policy::{Grant, GrantScope, Matcher, OperationMatch, RuleTable};
use komo_kernel::protocol::http::ApprovalDecisionRecord;
use komo_kernel::traits::{ApprovalRepo, Clock, OutputWriter, Tool};
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::ids::{ApprovalId, AttemptId, GrantId, OperationId, SessionId, ToolCallId};
use komo_kernel::types::plan::{
    ApprovedPlan, ExecutionPlan, Operation, PlanSource, PlanTarget, PlanVersions, RecoveryMode,
    TargetAccess, Verification,
};
use komo_kernel::types::refs::{PREVIEW_LIMIT_BYTES, ToolResultStatus};
use komo_kernel::types::status::ToolCallState;
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::{CancelToken, ToolContext, ToolDefinition, ToolError, ToolOutput};

use super::harness::{Harness, RecordingTool};
use super::{CallRequest, OperatorVerdict, RoundStop, is_recall, resumed_from};

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
        grant_proof: false,
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
                mounts: env.mounts.clone(),
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
    // 名字在能力面里、执行器手里却没有——这是**装配错误**（目录与能力面不同源），
    // 单独一条路，报的话也不一样。正常路径上它不该出现（§4 末）。
    let mut env = harness.env(&session, &run);
    env.surface = AgentSurface::new(["rm_rf"]);

    let outcome = executor.execute_round(calls, &env).await.unwrap();

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

/// **能力面之外的工具，模型自己拼出名字也调不动**（§4 末）。
///
/// 这是"给模型看的 schema"与"执行时允许的名字"同源的那条不变量的验收：schema 里没有
/// 这个名字，执行器就不认它——回退到全局工具目录会让能力边界变成一句建议。
#[tokio::test]
async fn a_tool_outside_the_surface_cannot_be_called() {
    let harness = Harness::new();
    let reader = Arc::new(RecordingTool::new("read", Operation::ReadFile));
    let search = Arc::new(RecordingTool::new("rg", Operation::ReadFile));
    let executor = harness.permissive(vec![reader.clone(), search.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("rg", serde_json::json!({ "pattern": "x" }))])
        .await;
    let mut env = harness.env(&session, &run);
    env.surface = AgentSurface::new(["read"]);

    let outcome = executor.execute_round(calls, &env).await.unwrap();

    assert!(outcome.stop.is_none());
    let result = &outcome.results[0];
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("工具集里没有 rg") && result.content.contains("可用的是：read"),
        "{}",
        result.content
    );
    assert_eq!(search.ran(), 0, "面外的工具一次都不能跑");
    assert!(
        harness.ledger.events().iter().all(|event| !matches!(
            event.payload,
            komo_kernel::events::EventPayload::ToolPlanned(_)
        )),
        "面外的工具不该留下 tool.planned"
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

/// ⑩ 两个预算各管各的：账本里的预览 ≤ 1 KiB，交给模型的正文按上下文预算收；完整输出
/// 始终在 `ToolOutputStore` 里。
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

    // 交给模型的正文按**上下文预算**收（默认 8 KiB），而不是账本那 1 KiB：两件事分开之后，
    // 目录小了不再等于模型只能看见 1 KiB。
    let content = &outcome.results[0].content;
    assert!(
        content.len() <= komo_kernel::projection::DEFAULT_MODEL_RESULT_BYTES + 256,
        "{}",
        content.len()
    );
    assert!(
        content.contains("中间省略"),
        "截了就要说省了多少：{content}"
    );
    assert!(content.contains("完整输出："), "{content}");

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

/// 写之后的读看见写留下的东西（§6：写是屏障，它后面那条不会和它抢跑道）。
#[tokio::test]
async fn the_read_after_a_write_sees_what_the_write_left() {
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
async fn the_executor_keeps_a_catalog_and_renders_a_surface_from_it() {
    let harness = Harness::new();
    let executor = harness.permissive(vec![Arc::new(ReadTool::new()), Arc::new(WriteTool::new())]);
    let names: Vec<String> = executor
        .catalog()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert_eq!(names, vec!["read".to_string(), "write".to_string()]);

    // 交给模型的 schema 按**能力面**来：面外的工具一个都不出现，顺序听能力面的。
    let surface = AgentSurface::new(["write"]);
    let rendered: Vec<String> = executor
        .definitions_for(&surface)
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert_eq!(rendered, vec!["write".to_string()]);
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

    /// 在**指定 Session 里一条新的 Run**上发起一次委派（§4「同一 Session 里后面的任何
    /// 一条 Run 都可以续跑」）。`open_run` 用的是固定请求键，两次拿到的是同一条 Run，
    /// 续跑的验收要的是真的两条不同的 Run。
    async fn delegate_round_on(
        harness: &Harness,
        session: &komo_kernel::types::ids::SessionId,
        request_key: &str,
        args: serde_json::Value,
    ) -> (RunId, CallRequest) {
        let accepted = harness
            .ledger
            .accept_input(komo_kernel::types::turn::AcceptInput {
                session: session.clone(),
                request_key: komo_kernel::types::ids::RequestKey::new(request_key),
                text: "续跑测试的另一条 Run".into(),
                source: PlanSource::Interactive {
                    session: session.clone(),
                },
                peer: None,
                model: komo_kernel::test_support::sample_model(),
                workdir: None,
                delegate: None,
                snapshot: None,
                skip_memory: false,
                at: harness.clock.now(),
            })
            .await
            .expect("接收输入");
        let run = accepted.run;
        // 调用号带上 `request_key`：默认的 `record_round` 每轮都从 "call-0" 编号，两条
        // 不同的 Run 各自的第一次委派会撞成同一个字面量 ToolCallId，`recorded_plan` 这类
        // 按 call_id 找计划的帮助函数会翻到别的 Run 那一条上去。
        let call = ToolCallId::from_raw(format!("call-{request_key}"));
        let request = CallRequest::fresh(
            call.clone(),
            format!("pc-{request_key}"),
            "delegate",
            args.clone(),
        );
        let round = komo_kernel::types::turn::AssistantRound {
            round: 1,
            text: None,
            text_ref: None,
            tool_calls: vec![komo_kernel::types::turn::ToolCallRequest {
                call_id: call.clone(),
                provider_call_id: request.provider_call_id.clone(),
                name: "delegate".into(),
                arguments: args,
                arguments_ref: None,
            }],
            provider_blocks: None,
            usage: Default::default(),
        };
        harness
            .ledger
            .record_round(&run, round)
            .await
            .expect("记录回合");
        (run, request)
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

    // ------------------------------------------------------------ 续跑（§4）

    /// ⑨ 续跑一条已经 `completed` 的子 Run：新派出去的那条带着 `resumes`，而且是**新的
    /// 一条子 Run**（不是把旧的那条叫醒）——账本上派出去两条，`resumes` 指回第一条。
    #[tokio::test]
    async fn resuming_a_completed_child_spawns_a_new_child_that_points_back_to_it() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let (session, run, request) = delegate_round(&harness, json!({ "task": "查 A" })).await;
        let env = harness.env(&session, &run);
        let outcome = executor
            .execute_round(vec![request.clone()], &env)
            .await
            .unwrap();
        let Some(RoundStop::Dependency {
            run: first_child, ..
        }) = outcome.stop
        else {
            panic!("{:?}", outcome.stop)
        };
        harness
            .ledger
            .complete(
                &first_child,
                RunEnd::Completed {
                    final_message: Some("A 查完了".into()),
                    rounds: 1,
                },
            )
            .await
            .unwrap();

        // 同一 Session 里**后面的任何一条 Run**都可以续跑——这里换一条全新的父 Run。
        let (run2, resume_request) = delegate_round_on(
            &harness,
            &session,
            "api:resume",
            json!({ "task": "接着查 B", "resume": first_child.as_str() }),
        )
        .await;
        let outcome = executor
            .execute_round(vec![resume_request.clone()], &harness.env(&session, &run2))
            .await
            .unwrap();
        let Some(RoundStop::Dependency {
            run: second_child, ..
        }) = outcome.stop
        else {
            panic!("{:?}", outcome.stop)
        };
        assert_ne!(
            second_child, first_child,
            "续跑是再派一条，不是叫醒旧的那条"
        );

        let surface = harness.ledger.surface();
        let spec = surface.runs[&second_child]
            .delegate
            .clone()
            .expect("子 Run 带着 spec");
        assert_eq!(spec.resumes, Some(first_child.clone()));
        assert_eq!(spec.task, "接着查 B", "每次续跑是自己的任务");
        let mut children: Vec<RunId> = child_runs(&harness);
        children.sort();
        let mut expected = vec![first_child, second_child.clone()];
        expected.sort();
        assert_eq!(children, expected, "账本上是两条子 Run");

        // 结果回到**第二轮那次调用**上：父侧核对的永远是这次调用派出去的那一条（§8.6）。
        harness
            .ledger
            .complete(
                &second_child,
                RunEnd::Completed {
                    final_message: Some("B 也查完了".into()),
                    rounds: 1,
                },
            )
            .await
            .unwrap();
        let attempt = attempt_of(&harness, &resume_request.call);
        let env2 = harness.env(&session, &run2);
        let done = executor
            .execute_round(
                vec![resumed_request(&harness, &resume_request, &attempt)],
                &env2,
            )
            .await
            .unwrap();
        assert!(done.stop.is_none(), "{:?}", done.stop);
        assert_eq!(done.results.len(), 1);
        assert!(
            done.results[0].content.contains(second_child.as_str()),
            "{}",
            done.results[0].content
        );
    }

    /// ⑩ 目标不在这个 Session 里：拒绝，而且这次调用**有一条落盘的结果**，不悬着。
    #[tokio::test]
    async fn resuming_a_run_from_another_session_is_refused_with_a_ledger_result() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let other_session = komo_kernel::types::ids::SessionId::from_raw("sess-别处");
        let elsewhere = harness
            .ledger
            .accept_input(komo_kernel::types::turn::AcceptInput {
                session: other_session.clone(),
                request_key: komo_kernel::types::ids::RequestKey::new("other:1"),
                text: "别的会话里的一条 Run".into(),
                source: komo_kernel::types::plan::PlanSource::Interactive {
                    session: other_session.clone(),
                },
                peer: None,
                model: komo_kernel::test_support::sample_model(),
                workdir: None,
                delegate: None,
                snapshot: None,
                skip_memory: false,
                at: harness.clock.now(),
            })
            .await
            .unwrap()
            .run;

        let (session, run, request) = delegate_round(
            &harness,
            json!({ "task": "接着做", "resume": elsewhere.as_str() }),
        )
        .await;
        let outcome = executor
            .execute_round(vec![request.clone()], &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert_eq!(outcome.results.len(), 1);
        assert!(outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains("同一个 Session"),
            "{}",
            outcome.results[0].content
        );
        assert!(child_runs(&harness).is_empty(), "没放行就不该有子 Run");
        assert_eq!(
            results_for_call(&harness, &request.call),
            1,
            "拒绝也要有一条落盘的结果，不能悬着"
        );
    }

    /// ⑪ 目标是主对话的 Run（不是子代理）：拒绝。
    #[tokio::test]
    async fn resuming_a_main_conversation_run_is_refused() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let (main_session, main_run) = harness.open_run().await;

        let (session, run, request) = delegate_round(
            &harness,
            json!({ "task": "接着做", "resume": main_run.as_str() }),
        )
        .await;
        assert_eq!(session, main_session);
        let outcome = executor
            .execute_round(vec![request.clone()], &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains("不是子代理"),
            "{}",
            outcome.results[0].content
        );
        assert!(child_runs(&harness).is_empty());
    }

    /// ⑫ 目标还在跑（没有终态）：拒绝，不是"等它"——这次调用本身还没有派出任何东西。
    #[tokio::test]
    async fn resuming_a_run_that_has_not_finished_is_refused() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let (session, run, request) = delegate_round(&harness, json!({ "task": "查 A" })).await;
        let outcome = executor
            .execute_round(vec![request], &harness.env(&session, &run))
            .await
            .unwrap();
        let Some(RoundStop::Dependency { run: child, .. }) = outcome.stop else {
            panic!("{:?}", outcome.stop)
        };
        // 不完成它——它还在跑。

        let (session2, run2, resume_request) = delegate_round(
            &harness,
            json!({ "task": "接着做", "resume": child.as_str() }),
        )
        .await;
        let outcome = executor
            .execute_round(vec![resume_request.clone()], &harness.env(&session2, &run2))
            .await
            .unwrap();
        assert!(outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains("还没有结束"),
            "{}",
            outcome.results[0].content
        );
        // 唯一的子 Run 还是那条还在跑的——续跑请求没有再派出第二条。
        assert_eq!(child_runs(&harness), vec![child]);
    }

    /// ⑬ 目标是 `cancelled` / `abandoned`：拒绝——那是操作者说过"这件事到此为止"，模型
    /// 不能替人把它捡回来。
    #[tokio::test]
    async fn resuming_a_cancelled_or_abandoned_run_is_refused() {
        for end in [
            RunEnd::Cancelled { by: None },
            RunEnd::Abandoned {
                by: None,
                reason: Some("没人再管了".into()),
            },
        ] {
            let harness = Harness::new();
            let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
            let (session, run, request) = delegate_round(&harness, json!({ "task": "查 A" })).await;
            let outcome = executor
                .execute_round(vec![request], &harness.env(&session, &run))
                .await
                .unwrap();
            let Some(RoundStop::Dependency { run: child, .. }) = outcome.stop else {
                panic!("{:?}", outcome.stop)
            };
            harness.ledger.complete(&child, end).await.unwrap();

            let (session2, run2, resume_request) = delegate_round(
                &harness,
                json!({ "task": "接着做", "resume": child.as_str() }),
            )
            .await;
            let outcome = executor
                .execute_round(vec![resume_request], &harness.env(&session2, &run2))
                .await
                .unwrap();
            assert!(outcome.results[0].is_error);
            assert!(
                outcome.results[0].content.contains("到此为止"),
                "{}",
                outcome.results[0].content
            );
        }
    }

    /// ⑭ 目标在最新一次 `/new` 之前：拒绝——那条线属于上一段对话。
    #[tokio::test]
    async fn resuming_a_run_before_the_latest_boundary_is_refused() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let (session, run, request) = delegate_round(&harness, json!({ "task": "查 A" })).await;
        let outcome = executor
            .execute_round(vec![request], &harness.env(&session, &run))
            .await
            .unwrap();
        let Some(RoundStop::Dependency { run: child, .. }) = outcome.stop else {
            panic!("{:?}", outcome.stop)
        };
        harness
            .ledger
            .complete(
                &child,
                RunEnd::Completed {
                    final_message: Some("A 查完了".into()),
                    rounds: 1,
                },
            )
            .await
            .unwrap();

        harness.ledger.boundary(&session).await.unwrap();

        let (session2, run2, resume_request) = delegate_round(
            &harness,
            json!({ "task": "接着做", "resume": child.as_str() }),
        )
        .await;
        let outcome = executor
            .execute_round(vec![resume_request], &harness.env(&session2, &run2))
            .await
            .unwrap();
        assert!(outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains("/new"),
            "{}",
            outcome.results[0].content
        );
    }

    /// ⑮ 续跑同一条子 Run 两次：第二次被拒，理由里写出末端是哪一条——线只往后接，
    /// 不分叉。
    #[tokio::test]
    async fn resuming_the_same_child_twice_is_refused_naming_the_tail() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let (session, run, request) = delegate_round(&harness, json!({ "task": "查 A" })).await;
        let outcome = executor
            .execute_round(vec![request], &harness.env(&session, &run))
            .await
            .unwrap();
        let Some(RoundStop::Dependency { run: target, .. }) = outcome.stop else {
            panic!("{:?}", outcome.stop)
        };
        harness
            .ledger
            .complete(
                &target,
                RunEnd::Completed {
                    final_message: Some("A 查完了".into()),
                    rounds: 1,
                },
            )
            .await
            .unwrap();

        let (run2, first_resume) = delegate_round_on(
            &harness,
            &session,
            "api:resume-1",
            json!({ "task": "接着查 B", "resume": target.as_str() }),
        )
        .await;
        let outcome = executor
            .execute_round(vec![first_resume], &harness.env(&session, &run2))
            .await
            .unwrap();
        let Some(RoundStop::Dependency { run: tail, .. }) = outcome.stop else {
            panic!("{:?}", outcome.stop)
        };

        let (run3, second_resume) = delegate_round_on(
            &harness,
            &session,
            "api:resume-2",
            json!({ "task": "接着查 C", "resume": target.as_str() }),
        )
        .await;
        let outcome = executor
            .execute_round(vec![second_resume], &harness.env(&session, &run3))
            .await
            .unwrap();
        assert!(outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains(tail.as_str()),
            "理由里要点名末端是哪一条：{}",
            outcome.results[0].content
        );
        // 第二次续跑没有再派出第三条子 Run。
        let mut children = child_runs(&harness);
        children.sort();
        let mut expected = vec![target, tail];
        expected.sort();
        assert_eq!(children, expected);
    }

    /// ⑯ 续跑出来的子 Run 一样调不动 `delegate`：深度只有一层不因为续跑而放宽。
    #[tokio::test]
    async fn a_resumed_child_cannot_delegate_again() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![Arc::new(DelegateTool::new())]);
        let (session, run, request) = delegate_round(&harness, json!({ "task": "查 A" })).await;
        let outcome = executor
            .execute_round(vec![request], &harness.env(&session, &run))
            .await
            .unwrap();
        let Some(RoundStop::Dependency { run: target, .. }) = outcome.stop else {
            panic!("{:?}", outcome.stop)
        };
        harness
            .ledger
            .complete(
                &target,
                RunEnd::Completed {
                    final_message: Some("A 查完了".into()),
                    rounds: 1,
                },
            )
            .await
            .unwrap();
        let (run2, resume_request) = delegate_round_on(
            &harness,
            &session,
            "api:resume",
            json!({ "task": "接着查 B", "resume": target.as_str() }),
        )
        .await;
        let outcome = executor
            .execute_round(vec![resume_request], &harness.env(&session, &run2))
            .await
            .unwrap();
        let Some(RoundStop::Dependency {
            run: resumed_child, ..
        }) = outcome.stop
        else {
            panic!("{:?}", outcome.stop)
        };
        let spec = harness.ledger.surface().runs[&resumed_child]
            .delegate
            .clone()
            .unwrap();

        // 续跑出来的子 Run 自己发起一次 delegate：能力面上它已经没有这个名字，这里验的是
        // 编排里的第二道（有人把含 `delegate` 的能力面塞给了它）。
        let inner_request = harness
            .record_round(
                &resumed_child,
                &[("delegate", json!({ "task": "再派一层" }))],
            )
            .await;
        let child_env = harness.child_env(&session, &resumed_child, spec);
        let outcome = executor
            .execute_round(inner_request, &child_env)
            .await
            .unwrap();
        assert!(outcome.results[0].is_error);
        assert!(outcome.results[0].content.contains("一层"));
        assert!(
            !child_runs(&harness).contains(&resumed_child) || child_runs(&harness).len() == 2,
            "没有第三条子 Run 被派出去"
        );
    }
}

// ---------------------------------------------------------------------------
// 同轮并发（§6 的修订）：只读的在飞，其余是屏障。
// ---------------------------------------------------------------------------

/// 一个只读的假工具：执行过程可以被观察和编排。
///
/// 同轮并发的**唯一**候选是 `read` / `rg` 那一类（`Operation::ReadFile`），所以"两条读
/// 是不是真的同时在飞""写是不是屏障"都只能在它身上验。
struct Probe {
    name: &'static str,
    operation: Operation,
    /// 进入与离开各记一行，按真实发生的顺序追加。
    log: Arc<Mutex<Vec<String>>>,
    /// 两条调用互相等：凑齐才继续。一条一条跑的时候这里等不到。
    barrier: Option<Arc<tokio::sync::Barrier>>,
    /// 一进来就取消整条 Run（测"取消要收齐已经在飞的那几条"）。
    cancel_on_entry: Option<CancelToken>,
    /// 进入时顺手数一眼"现在有几条待审批"（测"审批行在这批收完之后才落"）。
    approvals: Option<MemoApprovals>,
    pending_seen: Arc<Mutex<Vec<usize>>>,
}

type MemoApprovals = komo_kernel::test_support::MemApprovalRepo;

impl Probe {
    fn new(name: &'static str, operation: Operation, log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name,
            operation,
            log,
            barrier: None,
            cancel_on_entry: None,
            approvals: None,
            pending_seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_barrier(mut self, barrier: Arc<tokio::sync::Barrier>) -> Self {
        self.barrier = Some(barrier);
        self
    }

    fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel_on_entry = Some(cancel);
        self
    }

    fn counting_approvals(mut self, approvals: MemoApprovals) -> Self {
        self.approvals = Some(approvals);
        self
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().expect("日志").clone()
    }

    fn pending_seen(&self) -> Vec<usize> {
        self.pending_seen.lock().expect("待审批").clone()
    }
}

#[async_trait::async_trait]
impl Tool for Probe {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "测试用".into(),
            parameters: serde_json::json!({ "type": "object" }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let path = args
            .get("path")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .ok_or_else(|| ToolError::InvalidArguments {
                message: "probe 需要一个 path".into(),
            })?;
        Ok(ExecutionPlan {
            operation_id: OperationId::from_raw(format!("op-{}", ctx.call)),
            source: ctx.source.clone(),
            tool: self.name.into(),
            operation: self.operation.clone(),
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args,
            cwd: Some(ctx.cwd.clone()),
            // 目标就是参数里那个路径——规则的路径匹配只看这里（§7.1）。
            targets: vec![PlanTarget::local(ctx.cwd.join(path), TargetAccess::Read)],
            versions: PlanVersions::default(),
            resources: vec![],
            // 读取可以安全重做：超时是失败，不是"结果不明"。
            recovery: RecoveryMode::SafeReread,
        })
    }

    async fn execute(
        &self,
        plan: ApprovedPlan,
        ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let tag = format!("{}:{}", self.name, ctx.call);
        self.log.lock().expect("日志").push(format!("{tag}:进入"));
        if let Some(cancel) = &self.cancel_on_entry {
            cancel.cancel();
        }
        if let Some(approvals) = &self.approvals {
            let pending = approvals.list_pending(None).await.expect("待审批").len();
            self.pending_seen.lock().expect("待审批").push(pending);
        }
        if let Some(barrier) = &self.barrier
            && tokio::time::timeout(Duration::from_secs(1), barrier.wait())
                .await
                .is_err()
        {
            // 另一条读一直没进来 = 它们没在同时在飞。
            return Err(ToolError::Timeout { after_secs: 1 });
        }
        let delay = plan
            .plan()
            .args
            .get("delay_ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        self.log.lock().expect("日志").push(format!("{tag}:离开"));
        Ok(ToolOutput {
            status: ToolResultStatus::Completed,
            result: serde_json::json!({ "call": ctx.call }),
            exit_code: None,
            artifacts: vec![],
            preview: Some(format!("{tag} 完成")),
        })
    }
}

fn read_call(path: &str) -> (&'static str, serde_json::Value) {
    ("read", serde_json::json!({ "path": path }))
}

/// 同一轮里的两条只读调用**同时在飞**：一条一条跑的话，第二条永远进不来。
#[tokio::test]
async fn two_reads_in_one_round_run_at_the_same_time() {
    let harness = Harness::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let probe = Arc::new(
        Probe::new("read", Operation::ReadFile, Arc::clone(&log))
            .with_barrier(Arc::new(tokio::sync::Barrier::new(2))),
    );
    let executor = harness.permissive(vec![probe.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[read_call("a.txt"), read_call("b.txt")])
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
    assert_eq!(outcome.results.len(), 2);
    let bodies: Vec<&String> = outcome.results.iter().map(|r| &r.content).collect();
    assert!(
        outcome.results.iter().all(|result| !result.is_error),
        "{bodies:?}"
    );
    // 两条都进来之后才可能都离开：顺序正是"进入、进入、离开、离开"。
    let log = probe.log();
    assert_eq!(log.len(), 4, "{log:?}");
    assert!(
        log[0].ends_with(":进入") && log[1].ends_with(":进入"),
        "第二条读没有和第一条同时在飞：{log:?}"
    );
}

/// 写是屏障：它前面的读先收尾，之后的读等它跑完（§6）。
#[tokio::test]
async fn a_write_waits_for_the_reads_that_come_before_it() {
    let harness = Harness::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let reader = Arc::new(Probe::new("read", Operation::ReadFile, Arc::clone(&log)));
    let writer = Arc::new(Probe::new("write", Operation::WriteFile, Arc::clone(&log)));
    let executor = harness.permissive(vec![reader, writer]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[
                (
                    "read",
                    serde_json::json!({ "path": "a.txt", "delay_ms": 40 }),
                ),
                ("write", serde_json::json!({ "path": "b.txt" })),
            ],
        )
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
    assert_eq!(outcome.results.len(), 2);
    let log = log.lock().expect("日志").clone();
    assert_eq!(log.len(), 4, "{log:?}");
    assert!(
        log[0].starts_with("read:") && log[1].ends_with(":离开"),
        "读没有先收尾：{log:?}"
    );
    assert!(
        log[2].starts_with("write:"),
        "写在读收尾之前就开始了：{log:?}"
    );
}

/// 要审批的调用：**后面的调用一条都不启动**，而且审批行是在这一批收完之后才落的。
///
/// 后半句是这次并发改动唯一需要额外小心的地方（§7.4）：审批行要是比 Run 的挂起早出现
/// 一整个批次，操作者就可能答得比 Run 停下还早，那条答复落进空窗。
#[tokio::test]
async fn an_ask_stops_the_round_and_lands_only_after_the_reads_finish() {
    let harness = Harness::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let probe = Arc::new(
        Probe::new("read", Operation::ReadFile, log).counting_approvals(harness.approvals.clone()),
    );
    let executor = harness.initial(vec![probe.clone()]);
    let (session, run) = harness.open_run().await;
    // 第一条在 workspace 里（初始规则表 Allow），第二条在范围外（Ask）。
    let calls = harness
        .record_round(&run, &[read_call("a.txt"), read_call("/etc/shadow")])
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    let Some(RoundStop::Approval { call, .. }) = outcome.stop.clone() else {
        panic!("{:?}", outcome.stop)
    };
    assert_eq!(call, ToolCallId::from_raw("call-1"));
    assert_eq!(outcome.results.len(), 1, "第一条已经收尾");
    assert_eq!(
        outcome.remaining.len(),
        1,
        "停在审批上的那条还在 remaining 里"
    );
    assert_eq!(
        probe.pending_seen(),
        vec![0],
        "第一条执行时第二条的审批行就已经在了——它比 Run 的挂起还早"
    );
    let pending = harness
        .approvals
        .list_pending(Some(&session))
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "这一轮收完之后它才落下来");
    assert_eq!(pending[0].plan.tool, "read");
}

/// 取消要收齐已经在飞的那几条：它们的结论一个都不能丢（§8.6）。
#[tokio::test]
async fn cancelling_collects_the_reads_that_are_already_in_flight() {
    let harness = Harness::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let cancel = CancelToken::new();
    let reader = Arc::new(
        Probe::new("read", Operation::ReadFile, Arc::clone(&log)).with_cancel(cancel.clone()),
    );
    let writer = Arc::new(Probe::new("write", Operation::WriteFile, log));
    let executor = harness.permissive(vec![reader, writer]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[
                (
                    "read",
                    serde_json::json!({ "path": "a.txt", "delay_ms": 50 }),
                ),
                (
                    "read",
                    serde_json::json!({ "path": "b.txt", "delay_ms": 50 }),
                ),
                ("write", serde_json::json!({ "path": "c.txt" })),
            ],
        )
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env_with_cancel(&session, &run, cancel))
        .await
        .unwrap();

    assert!(matches!(outcome.stop, Some(RoundStop::Cancelled)));
    assert_eq!(outcome.results.len(), 2, "已经在飞的两条都要有结论");
    assert_eq!(outcome.remaining.len(), 1);
    assert_eq!(
        outcome.remaining[0].call,
        ToolCallId::from_raw("call-2"),
        "屏障后面的那条一个字节都没派发"
    );
}

/// 后一条先跑完也不改配对的顺序：交给模型的正文按**原始调用顺序**排（§6）。
#[tokio::test]
async fn a_faster_later_read_does_not_reorder_the_results() {
    let harness = Harness::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let probe = Arc::new(Probe::new("read", Operation::ReadFile, Arc::clone(&log)));
    let executor = harness.permissive(vec![probe.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[
                (
                    "read",
                    serde_json::json!({ "path": "a.txt", "delay_ms": 60 }),
                ),
                ("read", serde_json::json!({ "path": "b.txt" })),
            ],
        )
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    let call_of = |index: usize| ToolCallId::from_raw(format!("call-{index}"));
    assert_eq!(outcome.results.len(), 2);
    assert_eq!(outcome.results[0].call_id, call_of(0));
    assert_eq!(outcome.results[1].call_id, call_of(1));
    assert!(outcome.results[0].content.contains("call-0"));
    // 先完成的确实是后面那条：完成事件按真实顺序落账。
    let finished: Vec<String> = probe
        .log()
        .into_iter()
        .filter(|line| line.ends_with(":离开"))
        .collect();
    assert!(finished[0].contains("call-1"), "{finished:?}");
}

// ---------------------------------------------------------------------------
// §47 / §48：投影与 recall 的字节账。
// ---------------------------------------------------------------------------

/// 掐住 trace 输出——这两条指标**只进 trace，不进事件流**（§47）。
#[derive(Clone, Default)]
struct TraceCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for TraceCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("trace").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'w> tracing_subscriber::fmt::MakeWriter<'w> for TraceCapture {
    type Writer = TraceCapture;

    fn make_writer(&'w self) -> Self::Writer {
        self.clone()
    }
}

impl TraceCapture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("trace")).into_owned()
    }

    /// `field=123` 里那个数。
    fn number(&self, field: &str) -> u64 {
        let text = self.text();
        let rest = text
            .split_once(&format!("{field}="))
            .unwrap_or_else(|| panic!("trace 里没有 {field}：{text}"))
            .1;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits
            .parse()
            .unwrap_or_else(|_| panic!("{field} 不是数：{rest}"))
    }
}

/// 一次观察落了多少字节、投影给模型多少——两个数都要真的算出来（§48）。
#[tokio::test]
async fn the_projection_reports_how_many_bytes_it_kept_and_stored() {
    let capture = TraceCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let harness = Harness::new();
    let preview = "甲".repeat(500);
    let tool = Arc::new(
        RecordingTool::new("read", Operation::ReadFile).with_outcome(Ok(ToolOutput {
            status: ToolResultStatus::Completed,
            result: serde_json::json!({}),
            exit_code: None,
            artifacts: vec![],
            preview: Some(preview.clone()),
        })),
    );
    let executor = harness.permissive(vec![tool]);
    let (session, run) = harness.open_run().await;
    let calls = harness.record_round(&run, &[read_call("a.txt")]).await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();
    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);

    assert!(
        capture.text().contains("observation.projected"),
        "{}",
        capture.text()
    );
    assert!(
        capture.number("tool_output_bytes") >= preview.len() as u64,
        "落盘的字节至少要装得下正文"
    );
    // 投影进模型的那一份**小于**完整正文：超出的部分靠"完整输出在哪"那行去读回。
    assert!(
        capture.number("projected_bytes") < capture.number("tool_output_bytes"),
        "投影没有省下任何东西"
    );
}

/// 读回我们自己落盘的观察才算 recall（§48）。
#[tokio::test]
async fn reading_back_an_observation_counts_as_a_recall() {
    let capture = TraceCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let harness = Harness::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let probe = Arc::new(Probe::new("read", Operation::ReadFile, log));
    let executor = harness.permissive(vec![probe]);
    let (session, run) = harness.open_run().await;
    let observations = harness.dir.path().join("sessions").join(session.as_str());
    let calls = harness
        .record_round(
            &run,
            &[read_call(
                &observations
                    .join("tool-output/run-1/call-1/attempt-1/output.json")
                    .display()
                    .to_string(),
            )],
        )
        .await;

    let mut env = harness.env(&session, &run);
    env.mounts.session_root = Some(observations);
    let outcome = executor.execute_round(calls, &env).await.unwrap();
    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);

    assert!(
        capture.text().contains("observation.recalled"),
        "{}",
        capture.text()
    );
}

/// 一轮产出的文件要出现在交给模型的正文里：`artifact://files/…` 入口 + 大小，**正文不抄**
/// ——模型按引用去 `read`（§4.7）。没有这一句，它不知道自己产出了什么。
#[tokio::test]
async fn an_artifact_of_the_round_is_named_in_what_the_model_sees() {
    let harness = Harness::new();
    let report = komo_kernel::types::refs::ContentRef {
        path: "artifacts/run-1/报告.md".into(),
        size: "产物正文\n".len() as u64,
        hash: komo_kernel::types::digest::ContentHash::of_str("产物正文\n"),
        pointer: None,
    };
    let tool = Arc::new(
        RecordingTool::new(
            "python",
            Operation::PythonCall {
                module: "main".into(),
                function: "run".into(),
            },
        )
        .with_recovery(RecoveryMode::SafeReread)
        .with_outcome(Ok(ToolOutput {
            status: ToolResultStatus::Completed,
            result: serde_json::json!({ "ok": true }),
            exit_code: Some(0),
            artifacts: vec![report],
            preview: Some("跑完了\n".into()),
        })),
    );
    let executor = harness.permissive(vec![tool]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("python", serde_json::json!({ "code": "…" }))])
        .await;
    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();
    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);

    let content = &outcome.results[0].content;
    assert!(content.contains("跑完了"), "{content}");
    assert!(
        content.contains("产物：artifact://files/run-1/报告.md（13 B）"),
        "{content}"
    );
    assert!(
        !content.contains("产物正文"),
        "产物的正文按引用去读，不抄进上下文：{content}"
    );
}

/// recall 只在"读的是我们自己落盘的那两个子目录"时成立（§48）。
#[tokio::test]
async fn only_reads_of_our_own_output_are_recalls() {
    let harness = Harness::new();
    let (session, run) = harness.open_run().await;
    let observations = harness.dir.path().join("sessions").join(session.as_str());
    let mut env = harness.env(&session, &run);
    env.mounts.session_root = Some(observations.clone());

    let output = observations.join("tool-output/run-1/call-1/attempt-1/output.json");
    let artifacts = observations.join("artifacts/report.md");
    assert!(is_recall(
        &plan_touching(&[output], Operation::ReadFile),
        &env
    ));
    assert!(is_recall(
        &plan_touching(&[artifacts], Operation::ReadFile),
        &env
    ));
    // 工作目录里的文件不是"读回观察"。
    assert!(!is_recall(
        &plan_touching(&[env.cwd.join("a.txt")], Operation::ReadFile),
        &env
    ));
    // 写那两个目录也不是 recall——recall 说的是"读回来"。
    assert!(!is_recall(
        &plan_touching(
            &[observations.join("artifacts/report.md")],
            Operation::WriteFile
        ),
        &env
    ));
    // 没有 Session 目录（精简装配）时不算：无从判断读的是不是我们落的盘。
    let mut plain = harness.env(&session, &run);
    plain.mounts.session_root = None;
    assert!(!is_recall(
        &plan_touching(
            &[observations.join("tool_output.json")],
            Operation::ReadFile
        ),
        &plain
    ));
}

/// 一份只用来问"路径落在哪"的计划。
fn plan_touching(paths: &[PathBuf], operation: Operation) -> ExecutionPlan {
    ExecutionPlan {
        operation_id: OperationId::from_raw("op-1"),
        source: PlanSource::Interactive {
            session: SessionId::from_raw("sess-1"),
        },
        tool: "read".into(),
        operation,
        run: None,
        tool_call: None,
        args: serde_json::Value::Null,
        cwd: None,
        targets: paths
            .iter()
            .map(|path| PlanTarget::local(path.clone(), TargetAccess::Read))
            .collect(),
        versions: PlanVersions::default(),
        resources: vec![],
        recovery: RecoveryMode::SafeReread,
    }
}

/// 续跑里"确定尚未执行"的只读调用照样可以和后面的同时在飞——它没有上一世要接，
/// 顺序不由账本钉死（§8.4 第 6 行）。
#[tokio::test]
async fn a_resumed_read_that_never_ran_still_runs_beside_its_siblings() {
    let harness = Harness::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let probe = Arc::new(
        Probe::new("read", Operation::ReadFile, Arc::clone(&log))
            .with_barrier(Arc::new(tokio::sync::Barrier::new(2))),
    );
    let executor = harness.permissive(vec![probe.clone()]);
    let (session, run) = harness.open_run().await;
    let mut calls = harness
        .record_round(&run, &[read_call("a.txt"), read_call("b.txt")])
        .await;
    // 上一世只是落了计划、一次尝试都没有。
    for request in calls.iter_mut() {
        request.resumed = Some(resumed_from(ToolCallState::Planned, None, 0));
    }

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
    assert_eq!(outcome.results.len(), 2);
    let log = probe.log();
    assert_eq!(log.len(), 4, "{log:?}");
    assert!(
        log[0].ends_with(":进入") && log[1].ends_with(":进入"),
        "续跑里那条没跑过的读没有和兄弟同时在飞：{log:?}"
    );
}

/// `dispatch` / `follow`：`docs/home-dispatcher.md` §4.2、§9 Phase 2 的验收。
///
/// 与 `delegation` 那一组不同，这里不需要真的在账本里建一条子 Run——`TaskSpawner`
/// 把"建会话、提交输入"整个封在接缝后面，测试只关心 executor 这一侧的编排：放行之后
/// 立刻调它、立刻收尾（不是 `RoundStop::Dependency`），以及幂等重放不重复派任务。
mod dispatch_and_follow {
    use super::*;

    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicU32, Ordering};

    use komo_kernel::traits::{SpawnError, TaskSpawner};
    use komo_kernel::types::ids::{RequestKey, RunId};
    use komo_kernel::types::task::{FollowOutcome, TaskHandle, TaskSpec};

    use crate::tools::{DispatchTool, FollowTool};

    /// 一份记账的假 `TaskSpawner`：`spawn` 按 `request_key` 幂等（同一个键第二次拿回
    /// 同一个任务句柄，不建"第二个"），`follow` 对配置过的"未知短号"答
    /// [`SpawnError::UnknownTask`]。
    #[derive(Default)]
    struct FakeSpawnerState {
        spawn_calls: Vec<(RunId, RequestKey, TaskSpec)>,
        sessions_by_key: HashMap<RequestKey, TaskHandle>,
        follow_calls: Vec<(RunId, RequestKey, String, String)>,
        unknown: HashSet<String>,
        queued_behind: HashSet<String>,
    }

    struct FakeSpawner {
        state: Mutex<FakeSpawnerState>,
        next_id: AtomicU32,
    }

    impl FakeSpawner {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(FakeSpawnerState::default()),
                next_id: AtomicU32::new(0),
            })
        }

        fn with_unknown(self: Arc<Self>, task_id: &str) -> Arc<Self> {
            self.state
                .lock()
                .expect("假分发器")
                .unknown
                .insert(task_id.to_string());
            self
        }

        fn with_queued_behind(self: Arc<Self>, task_id: &str) -> Arc<Self> {
            self.state
                .lock()
                .expect("假分发器")
                .queued_behind
                .insert(task_id.to_string());
            self
        }

        fn spawn_calls(&self) -> usize {
            self.state.lock().expect("假分发器").spawn_calls.len()
        }

        /// **不同的任务会话**有几个——幂等重放要的是这里恒为 1，而不是 `spawn_calls()`。
        fn distinct_sessions(&self) -> usize {
            self.state
                .lock()
                .expect("假分发器")
                .sessions_by_key
                .values()
                .map(|handle| handle.session.clone())
                .collect::<HashSet<_>>()
                .len()
        }

        fn follow_calls(&self) -> usize {
            self.state.lock().expect("假分发器").follow_calls.len()
        }
    }

    #[async_trait::async_trait]
    impl TaskSpawner for FakeSpawner {
        async fn spawn(
            &self,
            from: &RunId,
            request_key: RequestKey,
            spec: TaskSpec,
        ) -> Result<TaskHandle, SpawnError> {
            let mut state = self.state.lock().expect("假分发器");
            state
                .spawn_calls
                .push((from.clone(), request_key.clone(), spec.clone()));
            if let Some(existing) = state.sessions_by_key.get(&request_key) {
                return Ok(existing.clone());
            }
            let n = self.next_id.fetch_add(1, Ordering::SeqCst);
            let handle = TaskHandle {
                session: SessionId::from_raw(format!("task-session-{n}")),
                short_id: format!("{n:04x}"),
                title: spec.title.clone(),
            };
            state.sessions_by_key.insert(request_key, handle.clone());
            Ok(handle)
        }

        async fn follow(
            &self,
            from: &RunId,
            request_key: RequestKey,
            task_id: &str,
            text: &str,
        ) -> Result<FollowOutcome, SpawnError> {
            let mut state = self.state.lock().expect("假分发器");
            state.follow_calls.push((
                from.clone(),
                request_key.clone(),
                task_id.to_string(),
                text.to_string(),
            ));
            if state.unknown.contains(task_id) {
                return Err(SpawnError::UnknownTask(format!(
                    "#{task_id} 不是这个 home 名下正在跑或最近的任务"
                )));
            }
            Ok(FollowOutcome {
                session: SessionId::from_raw(format!("task-session-{task_id}")),
                short_id: task_id.to_string(),
                queued_behind: state.queued_behind.contains(task_id),
            })
        }
    }

    /// 一条父 Run + 一次调用，策略是 §7.1 的初始建议表——dispatch / follow 在它之下也是
    /// Allow（新增两行），不需要额外放宽。
    async fn round(
        harness: &Harness,
        tool: &str,
        args: serde_json::Value,
    ) -> (SessionId, komo_kernel::types::ids::RunId, CallRequest) {
        let (session, run) = harness.open_run().await;
        let calls = harness.record_round(&run, &[(tool, args)]).await;
        (session, run, calls.into_iter().next().expect("一个调用"))
    }

    /// ① dispatch 一放行就立刻收尾：不是 `RoundStop::Dependency`，结果里带着短号与标题。
    #[tokio::test]
    async fn dispatch_settles_immediately_with_the_returned_short_id() {
        let harness = Harness::new();
        let fake = FakeSpawner::new();
        let executor = harness.initial_with_spawner(
            vec![Arc::new(DispatchTool::new())],
            fake.clone() as Arc<dyn TaskSpawner>,
        );
        let (session, run, request) = round(
            &harness,
            "dispatch",
            serde_json::json!({ "task": "查一下空调状态", "title": "查空调" }),
        )
        .await;

        let outcome = executor
            .execute_round(vec![request.clone()], &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(
            outcome.stop.is_none(),
            "dispatch 不等任务跑完，不该停在任何等待上：{:?}",
            outcome.stop
        );
        assert_eq!(outcome.results.len(), 1);
        assert!(!outcome.results[0].is_error, "{:?}", outcome.results[0]);
        assert!(
            outcome.results[0].content.contains("已派出 #0000：查空调"),
            "{}",
            outcome.results[0].content
        );
        assert_eq!(fake.spawn_calls(), 1);
    }

    /// ② 幂等：上一世已经 `start_call` 过（崩在"派出去"与"写结果"之间）的续跑，不重新
    /// 授权、不重新 `start_call`，直接把结果落回那次尝试——`spawner.spawn` 本身按
    /// `request_key` 幂等，即便两次收到同一个键也只认得出一个任务会话。
    #[tokio::test]
    async fn a_replayed_dispatch_call_does_not_spawn_twice() {
        let harness = Harness::new();
        let fake = FakeSpawner::new();
        let tool = DispatchTool::new();
        let executor = harness.initial_with_spawner(
            vec![Arc::new(DispatchTool::new())],
            fake.clone() as Arc<dyn TaskSpawner>,
        );
        let (session, run, request) = round(
            &harness,
            "dispatch",
            serde_json::json!({ "task": "查一下空调状态", "title": "查空调" }),
        )
        .await;
        let env = harness.env(&session, &run);
        let plan = plan_of(&tool, &harness, &session, &run, &request).await;
        let attempt = harness.crashed_attempt(&request.call, &plan).await;

        let resumed = {
            let mut request = request.clone();
            request.plan = Some(plan.clone());
            request.resumed = Some(resumed_from(
                ToolCallState::Started,
                Some(attempt.clone()),
                1,
            ));
            request
        };

        // 两次"回来收口"都把同一条悬着的调用交回来——恢复扫描重复触发时会是这个样子。
        let first = executor
            .execute_round(vec![resumed.clone()], &env)
            .await
            .unwrap();
        assert!(first.stop.is_none(), "{:?}", first.stop);
        let second = executor.execute_round(vec![resumed], &env).await.unwrap();
        assert!(second.stop.is_none(), "{:?}", second.stop);

        assert_eq!(
            fake.spawn_calls(),
            2,
            "两次都调了 spawn（它自己按请求键幂等）"
        );
        assert_eq!(
            fake.distinct_sessions(),
            1,
            "同一个请求键两次拿回的是同一个任务会话，没有多派一条"
        );
    }

    /// ③ follow 解析不到短号：干净失败，不是"结果不明"，也不悬着。
    #[tokio::test]
    async fn follow_with_an_unknown_task_id_fails_cleanly() {
        let harness = Harness::new();
        let fake = FakeSpawner::new().with_unknown("dead");
        let executor = harness.initial_with_spawner(
            vec![Arc::new(FollowTool::new())],
            fake.clone() as Arc<dyn TaskSpawner>,
        );
        let (session, run, request) = round(
            &harness,
            "follow",
            serde_json::json!({ "task_id": "dead", "text": "再看看" }),
        )
        .await;

        let outcome = executor
            .execute_round(vec![request.clone()], &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert_eq!(outcome.results.len(), 1);
        assert!(outcome.results[0].is_error, "{:?}", outcome.results[0]);
        assert!(
            outcome.results[0].content.contains("dead"),
            "{}",
            outcome.results[0].content
        );
        assert_eq!(fake.follow_calls(), 1, "解析只问了一次，没有悄悄重试");
    }

    /// ④ follow 命中一个正在跑的任务：措辞改成"排在后面"，不是"已转给"。
    #[tokio::test]
    async fn follow_into_a_busy_task_says_it_is_queued_behind() {
        let harness = Harness::new();
        let fake = FakeSpawner::new().with_queued_behind("3f2a");
        let executor = harness.initial_with_spawner(
            vec![Arc::new(FollowTool::new())],
            fake.clone() as Arc<dyn TaskSpawner>,
        );
        let (session, run, request) = round(
            &harness,
            "follow",
            serde_json::json!({ "task_id": "3f2a", "text": "再看看" }),
        )
        .await;

        let outcome = executor
            .execute_round(vec![request], &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(!outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains("正在跑"),
            "{}",
            outcome.results[0].content
        );
    }

    /// ⑤ 没有装配 `TaskSpawner` 时两个操作都干净失败（不悬着），不是一个悬空的调用。
    #[tokio::test]
    async fn without_a_spawner_dispatch_and_follow_fail_cleanly() {
        let harness = Harness::new();
        let executor = harness.initial(vec![
            Arc::new(DispatchTool::new()),
            Arc::new(FollowTool::new()),
        ]);
        let (session, run, request) = round(
            &harness,
            "dispatch",
            serde_json::json!({ "task": "干活", "title": "标题" }),
        )
        .await;

        let outcome = executor
            .execute_round(vec![request], &harness.env(&session, &run))
            .await
            .unwrap();
        assert!(outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains("没有接任务分发"),
            "{}",
            outcome.results[0].content
        );
    }

    /// ⑥ 计划哈希覆盖任务正文：内容变了，绑在旧哈希上的审批不该覆盖到新的一次调用。
    #[tokio::test]
    async fn the_plan_hash_changes_when_the_task_text_changes() {
        let harness = Harness::new();
        let ctx = harness.tool_context(
            &SessionId::from_raw("sess-1"),
            &komo_kernel::types::ids::RunId::from_raw("run-1"),
            &ToolCallId::from_raw("call-1"),
        );
        let tool = DispatchTool::new();
        let a = tool
            .prepare(
                serde_json::json!({ "task": "查一下空调状态", "title": "查空调" }),
                &ctx,
            )
            .await
            .unwrap();
        let b = tool
            .prepare(
                serde_json::json!({ "task": "查一下热水器状态", "title": "查空调" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_ne!(a.plan_hash(), b.plan_hash());
    }
}
