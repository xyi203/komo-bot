//! executor 的验收（§14 阶段 2、3）。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use komo_kernel::traits::{Clock, Ledger, OutputWriter, Tool, ToolOutputStore};
use komo_kernel::types::ids::{AttemptId, OperationId, SessionId, ToolCallId};
use komo_kernel::types::plan::{
    ExecutionPlan, Operation, PlanSource, PlanTarget, PlanVersions, RecoveryMode, Replay,
    TargetAccess,
};
use komo_kernel::types::refs::{InterruptedOutput, PREVIEW_LIMIT_BYTES, ToolResultStatus};
use komo_kernel::types::status::ToolCallState;
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::{
    CancelToken, ToolContext, ToolDefinition, ToolError, ToolImage, ToolOutput,
};

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
use crate::tools::{ReadTool, WriteTool};

/// ④ 文件版本冲突作为结果看得见。
#[tokio::test]
async fn a_file_version_conflict_comes_back_to_the_model() {
    let harness = Harness::new();
    let executor = harness.executor(vec![Arc::new(WriteTool::new())]);
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

/// edit 直接执行：没有 Policy、没有审批（§7）。
#[tokio::test]
async fn an_edit_runs_directly() {
    let harness = Harness::new();
    let executor = harness.executor(vec![Arc::new(crate::tools::EditTool::new())]);
    let (session, run) = harness.open_run().await;
    std::fs::write(harness.dir.path().join("a.txt"), "alpha\n").unwrap();

    let calls = harness
        .record_round(
            &run,
            &[(
                "edit",
                serde_json::json!({ "path": "a.txt", "match_text": "alpha", "replace_text": "ALPHA" }),
            )],
        )
        .await;
    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
    assert!(
        !outcome.results[0].is_error,
        "{}",
        outcome.results[0].content
    );
    assert_eq!(
        std::fs::read_to_string(harness.dir.path().join("a.txt")).unwrap(),
        "ALPHA\n"
    );
}

/// 同一轮里对同一个文件连发几个 edit：后一个在前一个改完之后才做计划、绑定版本，
/// 不会因为前一个改过文件而版本冲突（2026-10-21 起系统提示让模型改多处用一次 edit 带
/// `edits[]`，但模型照旧连发的这样仍要成立）。
#[tokio::test]
async fn consecutive_edits_to_one_file_in_a_round_all_apply() {
    let harness = Harness::new();
    let executor = harness.executor(vec![Arc::new(crate::tools::EditTool::new())]);
    let (session, run) = harness.open_run().await;
    std::fs::write(harness.dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();

    let calls = harness
        .record_round(
            &run,
            &[
                (
                    "edit",
                    serde_json::json!({ "path": "a.txt", "match_text": "alpha", "replace_text": "ALPHA" }),
                ),
                (
                    "edit",
                    serde_json::json!({ "path": "a.txt", "match_text": "ALPHA\nbeta", "replace_text": "ALPHA\nBETA" }),
                ),
            ],
        )
        .await;
    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
    for result in &outcome.results {
        assert!(!result.is_error, "{}", result.content);
    }
    assert_eq!(
        std::fs::read_to_string(harness.dir.path().join("a.txt")).unwrap(),
        "ALPHA\nBETA\n"
    );
}

/// 热重载撤掉的工具（照 pi 的 `hideTools`，`docs/mcp.md` §2.2）：目录里没了，新 Run 看不到；
/// 在跑的 Run 能力面里还有它，再调拿到撤的原因，不执行。同名的挂回来就又能调。
#[tokio::test]
async fn a_withdrawn_tool_says_why_and_comes_back_when_reinstalled() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::bash());
    let executor = harness.executor(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let env = harness.env(&session, &run);

    let removed = executor.withdraw(vec![(
        "bash".into(),
        "MCP 服务器 demo 已从配置移除，它的工具不能再调".into(),
    )]);
    assert_eq!(removed, vec!["bash".to_string()]);
    assert!(!executor.tool_names().contains(&"bash".to_string()));
    assert!(executor.definitions_for(&env.surface).is_empty());

    let calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "true" }))])
        .await;
    let outcome = executor.execute_round(calls, &env).await.unwrap();
    assert!(outcome.stop.is_none());
    let result = &outcome.results[0];
    assert!(result.is_error);
    assert!(
        result.content.contains("MCP 服务器 demo 已从配置移除"),
        "{}",
        result.content
    );
    assert_eq!(tool.ran(), 0);

    let again = Arc::new(RecordingTool::bash());
    assert_eq!(
        executor.install(vec![again.clone()]),
        vec!["bash".to_string()]
    );
    let calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "true" }))])
        .await;
    let outcome = executor.execute_round(calls, &env).await.unwrap();
    assert!(
        !outcome.results[0].is_error,
        "{}",
        outcome.results[0].content
    );
    assert_eq!(again.ran(), 1);
}

/// ⑥ 未知工具名无法调用——作为错误内容回给模型。
#[tokio::test]
async fn an_unknown_tool_name_cannot_be_called() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::bash());
    let executor = harness.executor(vec![tool.clone()]);
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
    let executor = harness.executor(vec![reader.clone(), search.clone()]);
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

/// ⑦ 恢复执行：started 而无结果、重放策略是 unsafe → **不重跑**，中断结果连同中断前已落盘
/// 的部分输出交给模型（§8.6，pi-durable 的 `interrupted`）。
#[tokio::test]
async fn an_unsafe_started_call_is_not_rerun_and_the_model_gets_its_partial_output() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::bash());
    let executor = harness.executor(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let mut calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
        .await;
    let previous = harness
        .crashed_attempt(
            &calls[0].call,
            &plan_of(&*tool, &harness, &session, &run, &calls[0]).await,
        )
        .await;
    harness.outputs.leave_interrupted(
        previous.as_str(),
        InterruptedOutput {
            stdout_bytes: 4096,
            stdout_tail: "step 2 done\nstep 3 ".into(),
            stderr_bytes: 0,
            stderr_tail: String::new(),
        },
    );
    calls[0].resumed = Some(resumed_from(
        ToolCallState::Started,
        Some(previous.clone()),
        1,
    ));

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert_eq!(tool.ran(), 0, "unsafe 的调用重启后执行器不重跑");
    // 不停下来等人：结果不明作为这次调用的结果交给模型，由它核对之后决定（§8.6）。
    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
    assert_eq!(outcome.results.len(), 1);
    let fed = &outcome.results[0];
    assert!(fed.is_error);
    for expected in [
        "被 Gateway 重启打断",
        "先核对目标的实际状态",
        "中断前已落盘的部分输出",
        "[stdout 共 4096 字节，末尾 19 字节]",
        "step 2 done\nstep 3 ",
    ] {
        assert!(
            fed.content.contains(expected),
            "缺 {expected}：{}",
            fed.content
        );
    }

    // 上一世那条 `tool.started` 配上了一条明确的 uncertain——不写下来它会永远悬着。
    let results = harness.results_for(&previous);
    assert_eq!(results.len(), 1, "有且只有一条结果：{results:?}");
    assert_eq!(results[0].status, ToolResultStatus::Uncertain);
    assert_eq!(
        harness.ledger.surface().calls[&results[0].call_id].state,
        ToolCallState::Uncertain
    );
}

/// 中断前什么都没落下，也要说清楚，而不是让模型以为漏看了什么。
#[tokio::test]
async fn an_interrupted_call_without_output_says_so() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::bash());
    let executor = harness.executor(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let mut calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
        .await;
    let previous = harness
        .crashed_attempt(
            &calls[0].call,
            &plan_of(&*tool, &harness, &session, &run, &calls[0]).await,
        )
        .await;
    calls[0].resumed = Some(resumed_from(ToolCallState::Started, Some(previous), 1));

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();
    assert_eq!(tool.ran(), 0);
    assert!(
        outcome.results[0]
            .content
            .contains("中断前没有落下任何输出"),
        "{}",
        outcome.results[0].content
    );
}

/// ⑦（另一半）planned = 确定尚未执行 → 直接执行一次，不看重放策略。
#[tokio::test]
async fn a_planned_call_is_known_not_to_have_run_and_executes_once() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::bash());
    let executor = harness.executor(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let mut calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
        .await;
    calls[0].resumed = Some(resumed_from(ToolCallState::Planned, None, 1));

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
    assert_eq!(tool.ran(), 1, "确定没跑过，unsafe 也照跑一次");

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

/// 工具现在声明 unsafe，能否决计划里记下的 safe（升级改了声明）：不重跑。
#[tokio::test]
async fn a_current_unsafe_declaration_vetoes_a_stored_safe_replay() {
    let harness = Harness::new();
    let tool = Arc::new(
        RecordingTool::new("read", Operation::ReadFile)
            .with_recovery(RecoveryMode::SafeReread)
            .with_replay(Replay::Unsafe),
    );
    let executor = harness.executor(vec![tool.clone()]);
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
    assert_eq!(tool.ran(), 0);
    assert!(outcome.results[0].is_error);
    let results = harness.results_for(&previous);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0].status, ToolResultStatus::Uncertain);
}

/// 反过来不行：计划里记下的是 unsafe，工具现在声明 safe 也不重跑。
#[tokio::test]
async fn a_current_safe_declaration_never_upgrades_a_stored_unsafe_one() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::bash().with_replay(Replay::Safe));
    let executor = harness.executor(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let mut calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
        .await;
    let previous = harness
        .crashed_attempt(
            &calls[0].call,
            &plan_of(&*tool, &harness, &session, &run, &calls[0]).await,
        )
        .await;
    calls[0].resumed = Some(resumed_from(ToolCallState::Started, Some(previous), 1));

    executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();
    assert_eq!(tool.ran(), 0);
}

/// 计划与工具都说 safe 的读取，重启后带原计划重跑一次（§8.6）。
#[tokio::test]
async fn a_safe_reread_is_replayed_after_a_restart() {
    let harness = Harness::new();
    let tool = Arc::new(
        RecordingTool::new("read", Operation::ReadFile).with_recovery(RecoveryMode::SafeReread),
    );
    let executor = harness.executor(vec![tool.clone()]);
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
}

/// 结果不明不能被当成失败重试掉，也不停下来等人：它作为结果交给模型，正文里带着判断方法
/// （§6、§8.6）。
#[tokio::test]
async fn an_uncertain_result_goes_to_the_model_instead_of_retrying() {
    let harness = Harness::new();
    let tool = Arc::new(
        RecordingTool::bash().with_outcome(Err(ToolError::Uncertain {
            message: "请求已发出，响应丢了".into(),
        })),
    );
    let executor = harness.executor(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    assert_eq!(tool.ran(), 1, "跑过一次，执行器不自动重跑");
    assert!(outcome.stop.is_none(), "不停下来等人：{:?}", outcome.stop);
    let result = outcome.results.first().expect("模型拿到这次调用的结果");
    assert!(
        result.content.contains("响应丢了") && result.content.contains("先核对"),
        "原因与判断方法都要进模型看到的正文：{}",
        result.content
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
        RecordingTool::bash().with_outcome(Err(ToolError::Uncertain {
            message: "解释器没有写下结果".into(),
        })),
    );
    let executor = harness.executor(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();
    assert_eq!(tool.ran(), 1);
    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
    let call = harness
        .ledger
        .surface()
        .calls
        .keys()
        .next()
        .cloned()
        .expect("这一轮只有一条调用");
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

/// 「确定没执行」是**失败**收尾：重做是模型发起的新的一次调用，不是这一次的重跑。
#[tokio::test]
async fn not_performed_closes_the_call_as_failed_so_a_retry_is_a_new_call() {
    let harness = Harness::new();
    let tool = Arc::new(
        RecordingTool::bash().with_outcome(Err(ToolError::Uncertain {
            message: "结果不明".into(),
        })),
    );
    let executor = harness.executor(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
        .await;
    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();
    assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
    let call = harness
        .ledger
        .surface()
        .calls
        .keys()
        .next()
        .cloned()
        .expect("这一轮只有一条调用");
    let attempt = harness.ledger.surface().calls[&call]
        .attempt
        .clone()
        .unwrap();

    let verdict = OperatorVerdict::NotPerformed {
        evidence: "远端没有这条记录".into(),
    };
    assert_eq!(verdict.status(), ToolResultStatus::Failed);

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

/// ⑩ 两个预算各管各的：界面摘要 ≤ 1 KiB；完整 body 内联账本，交给模型的正文按上下文预算收；完整输出
/// 始终在 `ToolOutputStore` 里。
#[tokio::test]
async fn the_preview_is_bounded_and_the_whole_output_lives_in_the_store() {
    let harness = Harness::new();
    let body = "x".repeat(100_000);
    let tool = Arc::new(RecordingTool::bash().with_outcome(Ok(ToolOutput {
        status: ToolResultStatus::Completed,
        result: serde_json::json!({ "stdout": body.clone() }),
        exit_code: Some(0),
        artifacts: vec![],
        images: vec![],
        preview: Some(body.clone()),
    })));
    let executor = harness.executor(vec![tool]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
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

    // 交给模型的正文按**上下文预算**收（默认 50 KiB / 2000 行），而不是账本那 1 KiB：两件事分开之后，
    // 摘要小不再等于模型只能看见 1 KiB。
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

/// 工具交出来的图片存成文件、账里只留引用，这一次直接附给模型；超限的那张不附，正文说明。
#[tokio::test]
async fn a_tool_image_is_stored_by_reference_and_attached_for_the_model() {
    let harness = Harness::new();
    let png = b"\x89PNG\r\n\x1a\nfake".to_vec();
    let mut huge = png.clone();
    huge.resize(komo_kernel::types::tool::IMAGE_LIMIT_BYTES + 1, 0);
    let tool = Arc::new(RecordingTool::bash().with_outcome(Ok(ToolOutput {
        status: ToolResultStatus::Completed,
        result: serde_json::Value::Null,
        exit_code: Some(0),
        artifacts: vec![],
        images: vec![
            ToolImage {
                mime: "image/png".into(),
                bytes: png.clone(),
            },
            ToolImage {
                mime: "image/png".into(),
                bytes: huge,
            },
        ],
        preview: Some("截了一张图".into()),
    })));
    let executor = harness.executor(vec![tool]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env(&session, &run))
        .await
        .unwrap();

    let result = &outcome.results[0];
    assert_eq!(result.images.len(), 1, "超限的那张不附");
    assert_eq!(result.images[0].bytes, png);
    assert!(
        result.content.contains("图片：image/png"),
        "{}",
        result.content
    );
    assert!(result.content.contains("超过上限"), "{}", result.content);

    let published = harness
        .ledger
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            komo_kernel::events::EventPayload::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("有一条 tool.result");
    let stored = harness
        .outputs
        .published_body(published.output_ref.path())
        .expect("输出存储里有它");
    assert_eq!(stored.images.len(), 1);
    let bytes = harness
        .outputs
        .open_image(&stored.images[0])
        .await
        .expect("按引用读得回来");
    assert_eq!(bytes, png);
}

/// 取消停在当场：后面的调用一个都不派发。
#[tokio::test]
async fn cancelling_leaves_the_rest_of_the_round_undispatched() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::bash());
    let executor = harness.executor(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[
                ("bash", serde_json::json!({ "command": "a" })),
                ("bash", serde_json::json!({ "command": "b" })),
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
    let executor = harness.executor(vec![Arc::new(WriteTool::new()), Arc::new(ReadTool::new())]);
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

/// 工具的普通失败是**结果**，不是执行器的错误。
#[tokio::test]
async fn an_ordinary_tool_failure_is_a_result_the_model_can_act_on() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::bash().with_outcome(Err(ToolError::Failed {
        message: "命令不存在".into(),
    })));
    let executor = harness.executor(vec![tool]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "nope" }))])
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
    let executor = harness.executor(vec![Arc::new(ReadTool::new())]);
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
    let executor = harness.executor(vec![Arc::new(RecordingTool::bash())]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
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

/// 恢复执行沿用**同一份**计划：重新 prepare 会换掉计划哈希，账本上的 `tool.planned` 就
/// 对不上 `tool.started` 了（§8.4）。
#[tokio::test]
async fn a_resumed_call_reuses_the_plan_it_was_planned_with() {
    let harness = Harness::new();
    let tool = Arc::new(RecordingTool::bash());
    let executor = harness.executor(vec![tool.clone()]);
    let (session, run) = harness.open_run().await;
    let mut resumed = harness
        .record_round(&run, &[("bash", serde_json::json!({ "command": "x" }))])
        .await;
    let env = harness.env(&session, &run);

    // 上一世只落了计划就停了（停在旧的审批上，或者崩在 start 之前）。
    let plan = plan_of(&*tool, &harness, &session, &run, &resumed[0]).await;
    let approved_hash = plan.plan_hash();
    harness
        .ledger
        .plan_call(&resumed[0].call, &plan)
        .await
        .unwrap();

    resumed[0].plan = Some(plan);
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
    assert_eq!(started_hash, approved_hash, "跑的就是落过盘的那份计划");
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
    let executor = harness.executor(vec![Arc::new(ReadTool::new()), Arc::new(WriteTool::new())]);
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

/// 只在 codemode 里调的工具（`docs/mcp.md` §3）。
#[derive(Debug)]
struct CodemodeOnly;

#[async_trait::async_trait]
impl Tool for CodemodeOnly {
    fn definition(&self) -> komo_kernel::types::tool::ToolDefinition {
        komo_kernel::types::tool::ToolDefinition {
            name: "mcp__demo__bump".into(),
            description: "[MCP demo] 加一".into(),
            parameters: serde_json::json!({ "type": "object" }),
        }
    }

    async fn prepare(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        Err(ToolError::Failed {
            message: "测试里不调".into(),
        })
    }

    async fn execute(
        &self,
        _plan: ExecutionPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        unreachable!("测试里不调")
    }

    fn exposure(&self) -> komo_kernel::types::tool::ToolExposure {
        komo_kernel::types::tool::ToolExposure::Codemode
    }
}

/// codemode 里调的工具留在能力面里（`tool://` 读得到），但不进交给模型的 schema；这台执行器
/// 没有接 codemode，服务器段也就不列它。
#[tokio::test]
async fn codemode_only_tools_stay_callable_but_out_of_the_schema() {
    let harness = Harness::new();
    let executor = harness.executor(vec![Arc::new(ReadTool::new()), Arc::new(CodemodeOnly)]);
    let surface = AgentSurface::new(["read", "mcp__demo__bump"]);
    let names = |definitions: Vec<komo_kernel::types::tool::ToolDefinition>| -> Vec<String> {
        definitions
            .into_iter()
            .map(|definition| definition.name)
            .collect()
    };
    assert_eq!(
        names(executor.definitions_for(&surface)),
        vec!["read".to_string()]
    );
    assert_eq!(
        names(executor.callable_definitions_for(&surface)),
        vec!["read".to_string(), "mcp__demo__bump".to_string()]
    );
    assert!(executor.namespaces_for(&surface).is_empty());
}

/// 委派（§4、§8.4 的 `dependency`）：一次子任务怎么变成一条子 Run、父调用怎么收尾。
///
/// 2026-10-16 同步委派删掉，`delegate` 工具已经不在目录里：这一组验的是**升级前落账的**
/// 调用照删之前的路径收尾。调用都以"计划已落账、还没开始"的形状交给执行器
/// （[`legacy_planned`]），续跑走首次那条路。
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

    /// 升级前落账的一次 `delegate` 调用：计划照当时 `DelegateTool::prepare` 的样子写进账本
    /// （`tool.planned`），交回执行器时是"确定没开始过"的形状——工具已经删掉，账上只剩它。
    pub(super) async fn legacy_planned(
        harness: &Harness,
        run: &RunId,
        request: CallRequest,
    ) -> CallRequest {
        let args = &request.arguments;
        let mut spec = DelegateSpec::new(
            run.clone(),
            request.call.clone(),
            args["task"].as_str().expect("task"),
        );
        if let Some(schema) = args.get("output_schema") {
            let mode = args
                .get("schema_mode")
                .map(|mode| serde_json::from_value(mode.clone()).expect("schema_mode"))
                .unwrap_or_default();
            spec = spec.with_contract(schema.clone(), mode);
        }
        if let Some(target) = args.get("resume").and_then(|value| value.as_str()) {
            spec = spec.with_resumes(RunId::from_raw(target));
        }
        let session = komo_kernel::types::ids::SessionId::from_raw("sess-1");
        let plan = ExecutionPlan {
            operation: Operation::Delegate { spec },
            run: Some(run.clone()),
            tool_call: Some(request.call.clone()),
            args: args.clone(),
            recovery: komo_kernel::types::plan::RecoveryMode::VerifyTarget,
            ..komo_kernel::test_support::sample_plan("delegate", &session)
        };
        harness
            .ledger
            .plan_call(&request.call, &plan)
            .await
            .expect("落计划");
        CallRequest {
            plan: Some(plan),
            resumed: Some(resumed_from(ToolCallState::Planned, None, 0)),
            ..request
        }
    }

    /// 账本里落过盘的那份计划——续跑时调用方手里拿到的就是它（§8.4：重新 prepare 会
    /// 换一个哈希，就不是账本上那次调用了）。
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
        let request = calls.into_iter().next().expect("一个调用");
        let request = legacy_planned(harness, &run, request).await;
        (session, run, request)
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
                interject: false,
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
        let request = legacy_planned(harness, &run, request).await;
        (run, request)
    }

    /// 从"派出去"走到"回来收口"之间那一步：把父调用的形状补成账本会给的形状。
    pub(super) fn resumed_request(
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
    pub(super) fn attempt_of(harness: &Harness, call: &ToolCallId) -> AttemptId {
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
        let executor = harness.executor(vec![]);
        let (session, run, request) = delegate_round(
            &harness,
            json!({
                "task": "把 a.txt 里的小数点都改成逗号",
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
        let executor = harness.executor(vec![]);
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
            let executor = harness.executor(vec![]);
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
            let executor = harness.executor(vec![]);
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
        let executor = harness.executor(vec![]);
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

    /// ⑤ 子 Run 还在跑 → **又一次等待**，不是失败。它可能正停在一条介入上，也可能刚被
    /// 别的执行实例领走；这两种都不该把父侧叫醒成一个失败。
    #[tokio::test]
    async fn a_child_that_is_still_running_keeps_the_call_waiting() {
        let harness = Harness::new();
        let executor = harness.executor(vec![]);
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
        let executor = harness.executor(vec![]);
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

    /// 委派是屏障：前面在飞的读先收尾，才受理子 Run、`start_call`（§6）。
    #[tokio::test]
    async fn delegate_is_a_barrier() {
        let harness = Harness::new();
        let probe = Arc::new(Probe::new(
            "read",
            Operation::ReadFile,
            Arc::new(Mutex::new(Vec::new())),
        ));
        let executor = harness.executor(vec![probe]);
        let (session, run) = harness.open_run().await;
        let mut calls = harness
            .record_round(
                &run,
                &[
                    ("read", json!({ "path": "a.txt", "delay_ms": 40 })),
                    ("delegate", json!({ "task": "跑一遍检查" })),
                ],
            )
            .await;
        let delegate = calls.pop().expect("委派那一条");
        calls.push(legacy_planned(&harness, &run, delegate).await);

        let outcome = executor
            .execute_round(calls, &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(
            matches!(outcome.stop, Some(RoundStop::Dependency { .. })),
            "{:?}",
            outcome.stop
        );
        assert_eq!(outcome.results.len(), 1, "前面的读已经收尾");
        let accepted = harness
            .ledger
            .events()
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::RunAccepted(accepted) if accepted.delegate.is_some() => {
                    Some(event.seq.0)
                }
                _ => None,
            })
            .expect("受理了一条子 Run");
        assert!(
            seq_of(&harness, "call-0", "result") < accepted,
            "前面的读还没收尾，子 Run 就受理了"
        );
        assert!(accepted < seq_of(&harness, "call-1", "started"));
    }

    /// ⑧ 深度只有一层。守卫在**编排里**而不是在工具表里：把 `delegate` 从子代理的工具表
    /// 摘掉是 UX，模型自己拼出这个名字就绕过去了，而能被绕过的约束等于没有。
    #[tokio::test]
    async fn a_child_run_cannot_delegate_again() {
        let harness = Harness::new();
        let executor = harness.executor(vec![]);
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
        let executor = harness.executor(vec![]);
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
        let executor = harness.executor(vec![]);
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
                interject: false,
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
        let executor = harness.executor(vec![]);
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
        let executor = harness.executor(vec![]);
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
            let executor = harness.executor(vec![]);
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
        let executor = harness.executor(vec![]);
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
        let executor = harness.executor(vec![]);
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
        let executor = harness.executor(vec![]);
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
        let mut inner_request = harness
            .record_round(
                &resumed_child,
                &[("delegate", json!({ "task": "再派一层" }))],
            )
            .await;
        let inner = inner_request.pop().expect("一个调用");
        inner_request.push(legacy_planned(&harness, &resumed_child, inner).await);
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
}

impl Probe {
    fn new(name: &'static str, operation: Operation, log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name,
            operation,
            log,
            barrier: None,
            cancel_on_entry: None,
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

    fn log(&self) -> Vec<String> {
        self.log.lock().expect("日志").clone()
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
            // 读取可以安全重做：超时是失败，不是"结果不明"。
            recovery: RecoveryMode::SafeReread,
            then_run: None,
        })
    }

    async fn execute(
        &self,
        plan: ExecutionPlan,
        ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let tag = format!("{}:{}", self.name, ctx.call);
        self.log.lock().expect("日志").push(format!("{tag}:进入"));
        if let Some(cancel) = &self.cancel_on_entry {
            cancel.cancel();
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
            images: vec![],
            preview: Some(format!("{tag} 完成")),
        })
    }
}

/// 这次调用的 `tool.started` / `tool.result` 落在账本的第几条：屏障的先后就看它。
fn seq_of(harness: &Harness, call: &str, kind: &str) -> u64 {
    use komo_kernel::events::EventPayload;
    let call = ToolCallId::from_raw(call);
    harness
        .ledger
        .events()
        .iter()
        .find_map(|event| match (&event.payload, kind) {
            (EventPayload::ToolStarted(body), "started") if body.call_id == call => {
                Some(event.seq.0)
            }
            (EventPayload::ToolResult(body), "result") if body.call_id == call => Some(event.seq.0),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{call} 没有 {kind}"))
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
    let executor = harness.executor(vec![probe.clone()]);
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
    let executor = harness.executor(vec![reader, writer]);
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

/// 在 `begin` 里就落了结论的调用（这里是能力面里没有的工具名）不是"还没轮到"：前面在飞的
/// 读停下来时，它已经落账的结果照样交给模型，不放回 `remaining`。
#[tokio::test]
async fn a_call_settled_before_a_stopping_read_keeps_its_result() {
    let harness = Harness::new();
    let cancel = CancelToken::new();
    let reader = Arc::new(
        Probe::new(
            "read",
            Operation::ReadFile,
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_cancel(cancel.clone()),
    );
    let executor = harness.executor(vec![reader]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[
                (
                    "read",
                    serde_json::json!({ "path": "a.txt", "delay_ms": 50 }),
                ),
                ("nope", serde_json::json!({})),
            ],
        )
        .await;

    let outcome = executor
        .execute_round(calls, &harness.env_with_cancel(&session, &run, cancel))
        .await
        .unwrap();

    assert!(
        matches!(outcome.stop, Some(RoundStop::Cancelled)),
        "{:?}",
        outcome.stop
    );
    assert!(
        seq_of(&harness, "call-1", "result") > 0,
        "不认识的工具名在账上有结论"
    );
    let order: Vec<&str> = outcome
        .results
        .iter()
        .map(|result| result.provider_call_id.as_str())
        .collect();
    assert_eq!(order, ["pc-0", "pc-1"], "已经落账的结论没交给模型");
    assert!(outcome.results[1].is_error);
    assert!(
        outcome.remaining.is_empty(),
        "已经有结论的调用被放回了 remaining：{:?}",
        outcome
            .remaining
            .iter()
            .map(|request| &request.call)
            .collect::<Vec<_>>()
    );
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
    let executor = harness.executor(vec![reader, writer]);
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
    let executor = harness.executor(vec![probe.clone()]);
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
            images: vec![],
            preview: Some(preview.clone()),
        })),
    );
    let executor = harness.executor(vec![tool]);
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
    let executor = harness.executor(vec![probe]);
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
            "bash",
            Operation::ShellCommand {
                command: "make-report".into(),
            },
        )
        .with_recovery(RecoveryMode::SafeReread)
        .with_outcome(Ok(ToolOutput {
            status: ToolResultStatus::Completed,
            result: serde_json::json!({ "ok": true }),
            exit_code: Some(0),
            artifacts: vec![report],
            images: vec![],
            preview: Some("跑完了\n".into()),
        })),
    );
    let executor = harness.executor(vec![tool]);
    let (session, run) = harness.open_run().await;
    let calls = harness
        .record_round(
            &run,
            &[("bash", serde_json::json!({ "command": "make-report" }))],
        )
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
        recovery: RecoveryMode::SafeReread,
        then_run: None,
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
    let executor = harness.executor(vec![probe.clone()]);
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

/// `task`：`docs/background-tasks.md` 的验收（2026-10-16 之前是 `dispatch` / `follow`）。
///
/// 与 `delegation` 那一组不同，这里不需要真的在账本里建一条子 Run——`TaskSpawner`
/// 把"建会话、提交输入"整个封在接缝后面，测试只关心 executor 这一侧的编排：放行之后
/// 立刻调它、立刻收尾（不是 `RoundStop::Dependency`），以及幂等重放不重复派任务。
mod task {
    use super::*;

    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicU32, Ordering};

    use komo_kernel::traits::{SpawnError, TaskSpawner};
    use komo_kernel::types::ids::{RequestKey, RunId};
    use komo_kernel::types::task::{FollowOutcome, TaskHandle, TaskOptions, TaskSpec};

    use crate::config::testing::Fixture;
    use crate::tools::TaskTool;

    pub(super) fn task_tool() -> Arc<TaskTool> {
        Arc::new(TaskTool::new(Arc::new(Fixture::valid().holder())))
    }

    /// 一份记账的假 `TaskSpawner`：`spawn` 按 `request_key` 幂等（同一个键第二次拿回
    /// 同一个任务句柄，不建"第二个"），`follow` 对配置过的"未知短号"答
    /// [`SpawnError::UnknownTask`]。
    #[derive(Default)]
    struct FakeSpawnerState {
        spawn_calls: Vec<(RunId, RequestKey, TaskSpec)>,
        sessions_by_key: HashMap<RequestKey, TaskHandle>,
        follow_calls: Vec<(RunId, RequestKey, String, String, TaskOptions)>,
        unknown: HashSet<String>,
        queued_behind: HashSet<String>,
        progress: HashMap<String, komo_kernel::types::task::TaskProgress>,
        status_calls: Vec<String>,
    }

    pub(super) struct FakeSpawner {
        state: Mutex<FakeSpawnerState>,
        next_id: AtomicU32,
    }

    impl FakeSpawner {
        pub(super) fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(FakeSpawnerState::default()),
                next_id: AtomicU32::new(0),
            })
        }

        pub(super) fn with_unknown(self: Arc<Self>, task_id: &str) -> Arc<Self> {
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

        fn with_progress(
            self: Arc<Self>,
            task_id: &str,
            progress: komo_kernel::types::task::TaskProgress,
        ) -> Arc<Self> {
            self.state
                .lock()
                .expect("假分发器")
                .progress
                .insert(task_id.to_string(), progress);
            self
        }

        fn status_calls(&self) -> Vec<String> {
            self.state.lock().expect("假分发器").status_calls.clone()
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

        fn spawned(&self) -> Vec<(RunId, RequestKey, TaskSpec)> {
            self.state.lock().expect("假分发器").spawn_calls.clone()
        }

        fn followed(&self) -> Vec<(RunId, RequestKey, String, String, TaskOptions)> {
            self.state.lock().expect("假分发器").follow_calls.clone()
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
            options: TaskOptions,
        ) -> Result<FollowOutcome, SpawnError> {
            let mut state = self.state.lock().expect("假分发器");
            state.follow_calls.push((
                from.clone(),
                request_key.clone(),
                task_id.to_string(),
                text.to_string(),
                options,
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

        async fn status(
            &self,
            _from: &RunId,
            task_id: &str,
        ) -> Result<komo_kernel::types::task::TaskProgress, SpawnError> {
            let mut state = self.state.lock().expect("假分发器");
            state.status_calls.push(task_id.to_string());
            state.progress.get(task_id).cloned().ok_or_else(|| {
                SpawnError::UnknownTask(format!("#{task_id} 不是这个 home 名下正在跑或最近的任务"))
            })
        }
    }

    /// 一条父 Run + 一次调用。
    async fn round(
        harness: &Harness,
        tool: &str,
        args: serde_json::Value,
    ) -> (SessionId, komo_kernel::types::ids::RunId, CallRequest) {
        let (session, run) = harness.open_run().await;
        let calls = harness.record_round(&run, &[(tool, args)]).await;
        (session, run, calls.into_iter().next().expect("一个调用"))
    }

    /// ① 新派一放行就立刻收尾：不是 `RoundStop::Dependency`，结果里带着短号与标题；
    /// model / effort / contract 原样交给 spawner，请求键是 `task:{run}:{call}`。
    #[tokio::test]
    async fn a_new_task_settles_immediately_with_the_returned_short_id() {
        let harness = Harness::new();
        let fake = FakeSpawner::new();
        let executor =
            harness.executor_with_spawner(vec![task_tool()], fake.clone() as Arc<dyn TaskSpawner>);
        let (session, run, request) = round(
            &harness,
            "task",
            serde_json::json!({
                "prompt": "查一下空调状态",
                "title": "查空调",
                "model": "chat",
                "effort": "low",
                "contract": { "type": "object" }
            }),
        )
        .await;

        let outcome = executor
            .execute_round(vec![request.clone()], &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(
            outcome.stop.is_none(),
            "task 不等任务跑完，不该停在任何等待上：{:?}",
            outcome.stop
        );
        assert_eq!(outcome.results.len(), 1);
        assert!(!outcome.results[0].is_error, "{:?}", outcome.results[0]);
        assert!(
            outcome.results[0].content.contains("已派出 #0000：查空调"),
            "{}",
            outcome.results[0].content
        );
        let spawned = fake.spawned();
        assert_eq!(spawned.len(), 1);
        let (_, key, spec) = &spawned[0];
        assert_eq!(key.as_str(), format!("task:{run}:{}", request.call));
        assert_eq!(spec.task, "查一下空调状态");
        assert_eq!(spec.options.model.as_deref(), Some("chat"));
        assert_eq!(
            spec.options.effort.as_ref().map(|effort| effort.as_str()),
            Some("low")
        );
        assert!(spec.options.contract.is_some());
    }

    /// 同一轮派几个就是几个：各自一个请求键、各自一个任务会话（§14 `task` 验收 ②）。
    #[tokio::test]
    async fn three_tasks_in_one_round_are_three_task_sessions() {
        let harness = Harness::new();
        let fake = FakeSpawner::new();
        let executor =
            harness.executor_with_spawner(vec![task_tool()], fake.clone() as Arc<dyn TaskSpawner>);
        let (session, run) = harness.open_run().await;
        let calls = harness
            .record_round(
                &run,
                &[
                    (
                        "task",
                        serde_json::json!({ "prompt": "查 A", "title": "A" }),
                    ),
                    (
                        "task",
                        serde_json::json!({ "prompt": "查 B", "title": "B" }),
                    ),
                    (
                        "task",
                        serde_json::json!({ "prompt": "查 C", "title": "C" }),
                    ),
                ],
            )
            .await;

        let outcome = executor
            .execute_round(calls, &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert_eq!(outcome.results.len(), 3);
        assert_eq!(fake.distinct_sessions(), 3);
    }

    /// ② 幂等：上一世已经 `start_call` 过（崩在"派出去"与"写结果"之间）的续跑，不重新
    /// 放行、不重新 `start_call`，直接把结果落回那次尝试——`spawner.spawn` 本身按
    /// `request_key` 幂等，即便两次收到同一个键也只认得出一个任务会话。
    #[tokio::test]
    async fn a_replayed_task_call_does_not_spawn_twice() {
        let harness = Harness::new();
        let fake = FakeSpawner::new();
        let tool = task_tool();
        let executor =
            harness.executor_with_spawner(vec![tool.clone()], fake.clone() as Arc<dyn TaskSpawner>);
        let (session, run, request) = round(
            &harness,
            "task",
            serde_json::json!({ "prompt": "查一下空调状态", "title": "查空调" }),
        )
        .await;
        let env = harness.env(&session, &run);
        let plan = plan_of(&*tool, &harness, &session, &run, &request).await;
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

    /// ③ 接着做解析不到短号：干净失败，不是"结果不明"，也不悬着。
    #[tokio::test]
    async fn following_up_an_unknown_task_id_fails_cleanly() {
        let harness = Harness::new();
        let fake = FakeSpawner::new().with_unknown("dead");
        let executor =
            harness.executor_with_spawner(vec![task_tool()], fake.clone() as Arc<dyn TaskSpawner>);
        let (session, run, request) = round(
            &harness,
            "task",
            serde_json::json!({ "task_id": "dead", "prompt": "再看看" }),
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

    /// 一个正在跑 bash 的任务的快照。
    fn running_bash_progress() -> komo_kernel::types::task::TaskProgress {
        use komo_kernel::types::ids::ToolCallId;
        use komo_kernel::types::task::{RunProgress, RunningCall, TaskProgress};
        let now = time::OffsetDateTime::now_utc();
        TaskProgress {
            short_id: "3f2a".into(),
            title: "查空调".into(),
            run: Some(RunProgress {
                run: RunId::from_raw("task-run-1"),
                state: komo_kernel::types::status::RunState::Running,
                accepted_at: now - time::Duration::minutes(5),
                last_at: now,
                rounds: 2,
                calls: 3,
                running: vec![RunningCall {
                    call: ToolCallId::from_raw("call-1"),
                    attempt: None,
                    tool: "bash".into(),
                    summary: "make test".into(),
                    started_at: Some(now - time::Duration::minutes(2)),
                    output_tail: Some("compiling foo\nrunning 12 tests".into()),
                }],
                note: None,
                final_message: None,
                reason: None,
                waiting_on: None,
            }),
        }
    }

    /// 查进度：只给 task_id，结果是渲染好的快照；不提交任何东西（不 spawn、不 follow）。
    #[tokio::test]
    async fn a_status_query_returns_the_snapshot_without_submitting() {
        let harness = Harness::new();
        let fake = FakeSpawner::new().with_progress("3f2a", running_bash_progress());
        let executor =
            harness.executor_with_spawner(vec![task_tool()], fake.clone() as Arc<dyn TaskSpawner>);
        let (session, run, request) =
            round(&harness, "task", serde_json::json!({ "task_id": "#3f2a" })).await;

        let outcome = executor
            .execute_round(vec![request], &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert_eq!(outcome.results.len(), 1);
        let result = &outcome.results[0];
        assert!(!result.is_error, "{result:?}");
        assert!(
            result.content.contains("正在执行：bash"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("running 12 tests"),
            "{}",
            result.content
        );
        assert_eq!(fake.status_calls(), vec!["3f2a".to_string()]);
        assert_eq!(fake.spawn_calls(), 0);
        assert_eq!(fake.follow_calls(), 0);
    }

    /// 查不存在的短号：干净失败，开头是"查不到进度"。
    #[tokio::test]
    async fn a_status_query_for_an_unknown_task_fails_cleanly() {
        let harness = Harness::new();
        let fake = FakeSpawner::new();
        let executor =
            harness.executor_with_spawner(vec![task_tool()], fake.clone() as Arc<dyn TaskSpawner>);
        let (session, run, request) =
            round(&harness, "task", serde_json::json!({ "task_id": "dead" })).await;

        let outcome = executor
            .execute_round(vec![request], &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        let result = &outcome.results[0];
        assert!(result.is_error, "{result:?}");
        assert!(
            result.content.contains("查不到进度："),
            "{}",
            result.content
        );
        assert_eq!(fake.spawn_calls() + fake.follow_calls(), 0);
    }

    /// ④ 接着做命中一个正在跑的任务：措辞改成"排在后面"，不是"已转给"。没给 model 就不带
    /// （沿用那个任务上一条 Run 的，由 spawner 去读）。
    #[tokio::test]
    async fn following_up_a_busy_task_says_it_is_queued_behind() {
        let harness = Harness::new();
        let fake = FakeSpawner::new().with_queued_behind("3f2a");
        let executor =
            harness.executor_with_spawner(vec![task_tool()], fake.clone() as Arc<dyn TaskSpawner>);
        let (session, run, request) = round(
            &harness,
            "task",
            serde_json::json!({ "task_id": "#3f2a", "prompt": "再看看" }),
        )
        .await;

        let outcome = executor
            .execute_round(vec![request.clone()], &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(!outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains("正在跑"),
            "{}",
            outcome.results[0].content
        );
        let followed = fake.followed();
        let (_, key, task_id, text, options) = &followed[0];
        assert_eq!(key.as_str(), format!("task:{run}:{}", request.call));
        assert_eq!((task_id.as_str(), text.as_str()), ("3f2a", "再看看"));
        assert_eq!(options, &TaskOptions::default());
    }

    /// 升级前落账的 `dispatch` / `follow`：工具已经不在目录里，started 而没有结果的那次照样
    /// 按请求键幂等收尾，键沿用它们自己的前缀（§8.6），不是"工具名没了"的中断。
    #[tokio::test]
    async fn a_legacy_dispatch_or_follow_settles_through_the_spawner_by_its_old_key() {
        for (tool, operation, prefix) in [
            (
                "dispatch",
                Operation::Dispatch {
                    task: "查一下空调状态".into(),
                    title: "查空调".into(),
                },
                "dispatch",
            ),
            (
                "follow",
                Operation::Follow {
                    task_id: "3f2a".into(),
                    text: "再看看".into(),
                },
                "follow",
            ),
        ] {
            let harness = Harness::new();
            let fake = FakeSpawner::new();
            let executor =
                harness.executor_with_spawner(vec![], fake.clone() as Arc<dyn TaskSpawner>);
            let (session, run, mut request) = round(&harness, tool, serde_json::json!({})).await;
            let plan = ExecutionPlan {
                operation,
                run: Some(run.clone()),
                tool_call: Some(request.call.clone()),
                recovery: RecoveryMode::VerifyTarget,
                ..komo_kernel::test_support::sample_plan(tool, &session)
            };
            let attempt = harness.crashed_attempt(&request.call, &plan).await;
            request.plan = Some(plan);
            request.resumed = Some(resumed_from(ToolCallState::Started, Some(attempt), 1));

            let outcome = executor
                .execute_round(vec![request.clone()], &harness.env(&session, &run))
                .await
                .unwrap();

            assert!(outcome.stop.is_none(), "{tool}：{:?}", outcome.stop);
            assert!(
                !outcome.results[0].is_error,
                "{tool}：{:?}",
                outcome.results[0]
            );
            let expected = format!("{prefix}:{run}:{}", request.call);
            let keys: Vec<String> = fake
                .spawned()
                .into_iter()
                .map(|(_, key, _)| key.as_str().to_string())
                .chain(
                    fake.followed()
                        .into_iter()
                        .map(|(_, key, ..)| key.as_str().to_string()),
                )
                .collect();
            assert_eq!(keys, [expected], "{tool}");
        }
    }

    /// ⑤ 没有装配 `TaskSpawner` 时两个操作都干净失败（不悬着），不是一个悬空的调用。
    #[tokio::test]
    async fn without_a_spawner_task_fails_cleanly() {
        let harness = Harness::new();
        let executor = harness.executor(vec![task_tool()]);
        let (session, run, request) = round(
            &harness,
            "task",
            serde_json::json!({ "prompt": "干活", "title": "标题" }),
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

    /// ⑥ 计划哈希覆盖任务正文：内容变了，就是另一次调用。
    #[tokio::test]
    async fn the_plan_hash_changes_when_the_task_text_changes() {
        let harness = Harness::new();
        let ctx = harness.tool_context(
            &SessionId::from_raw("sess-1"),
            &komo_kernel::types::ids::RunId::from_raw("run-1"),
            &ToolCallId::from_raw("call-1"),
        );
        let tool = task_tool();
        let a = tool
            .prepare(
                serde_json::json!({ "prompt": "查一下空调状态", "title": "查空调" }),
                &ctx,
            )
            .await
            .unwrap();
        let b = tool
            .prepare(
                serde_json::json!({ "prompt": "查一下热水器状态", "title": "查空调" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_ne!(a.plan_hash(), b.plan_hash());
    }

    /// ⑦ 前面在飞的读停下来（这里是取消），排在它后面的 task 就是"还没轮到"：交回
    /// `remaining` 的它不能已经派出去、也不能已经 `start_call` 过——否则续跑时交回来的是
    /// 一条已经有结果的调用，而它的结果这一轮也没交给模型。
    #[tokio::test]
    async fn a_task_behind_a_stopping_read_is_not_sent() {
        let harness = Harness::new();
        let fake = FakeSpawner::new();
        let cancel = CancelToken::new();
        let reader = Arc::new(
            Probe::new(
                "read",
                Operation::ReadFile,
                Arc::new(Mutex::new(Vec::new())),
            )
            .with_cancel(cancel.clone()),
        );
        let executor = harness.executor_with_spawner(
            vec![reader, task_tool()],
            fake.clone() as Arc<dyn TaskSpawner>,
        );
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
                        "task",
                        serde_json::json!({ "prompt": "查一下空调状态", "title": "查空调" }),
                    ),
                ],
            )
            .await;

        let outcome = executor
            .execute_round(calls, &harness.env_with_cancel(&session, &run, cancel))
            .await
            .unwrap();

        assert!(
            matches!(outcome.stop, Some(RoundStop::Cancelled)),
            "{:?}",
            outcome.stop
        );
        let remaining: Vec<&ToolCallId> = outcome.remaining.iter().map(|r| &r.call).collect();
        assert_eq!(remaining, [&ToolCallId::from_raw("call-1")]);
        assert_eq!(fake.spawn_calls(), 0, "还没轮到的 task 已经派出去了");
        let started = harness.ledger.events().iter().any(|event| {
            matches!(
                &event.payload,
                komo_kernel::events::EventPayload::ToolStarted(body)
                    if body.call_id == ToolCallId::from_raw("call-1")
            )
        });
        assert!(!started, "还没轮到的 task 已经 start_call 过");
    }

    /// ⑧ task 是屏障：前面在飞的读先收尾它才派出去，后面的读等它收完才开始（§6）。
    #[tokio::test]
    async fn task_is_a_barrier() {
        let harness = Harness::new();
        let fake = FakeSpawner::new();
        let probe = Arc::new(Probe::new(
            "read",
            Operation::ReadFile,
            Arc::new(Mutex::new(Vec::new())),
        ));
        let executor = harness.executor_with_spawner(
            vec![probe.clone(), task_tool()],
            fake.clone() as Arc<dyn TaskSpawner>,
        );
        let (session, run) = harness.open_run().await;
        let calls = harness
            .record_round(
                &run,
                &[
                    (
                        "read",
                        serde_json::json!({ "path": "a.txt", "delay_ms": 40 }),
                    ),
                    (
                        "task",
                        serde_json::json!({ "prompt": "查一下空调状态", "title": "查空调" }),
                    ),
                    read_call("b.txt"),
                ],
            )
            .await;

        let outcome = executor
            .execute_round(calls, &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert_eq!(outcome.results.len(), 3);
        assert_eq!(fake.spawn_calls(), 1);
        assert!(
            seq_of(&harness, "call-0", "result") < seq_of(&harness, "call-1", "started"),
            "前面的读还没收尾，task 就派出去了"
        );
        assert!(
            seq_of(&harness, "call-1", "result") < seq_of(&harness, "call-2", "started"),
            "task 还没收尾，后面的读就开始了"
        );
        assert_eq!(
            probe.log(),
            [
                "read:call-0:进入",
                "read:call-0:离开",
                "read:call-2:进入",
                "read:call-2:离开"
            ]
        );
    }

    /// ⑨ 接着做同样是屏障。
    #[tokio::test]
    async fn following_up_is_a_barrier() {
        let harness = Harness::new();
        let fake = FakeSpawner::new();
        let probe = Arc::new(Probe::new(
            "read",
            Operation::ReadFile,
            Arc::new(Mutex::new(Vec::new())),
        ));
        let executor = harness.executor_with_spawner(
            vec![probe.clone(), task_tool()],
            fake.clone() as Arc<dyn TaskSpawner>,
        );
        let (session, run) = harness.open_run().await;
        let calls = harness
            .record_round(
                &run,
                &[
                    (
                        "read",
                        serde_json::json!({ "path": "a.txt", "delay_ms": 40 }),
                    ),
                    (
                        "task",
                        serde_json::json!({ "task_id": "3f2a", "prompt": "再看看" }),
                    ),
                    read_call("b.txt"),
                ],
            )
            .await;

        let outcome = executor
            .execute_round(calls, &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert_eq!(outcome.results.len(), 3);
        assert_eq!(fake.follow_calls(), 1);
        assert!(
            seq_of(&harness, "call-0", "result") < seq_of(&harness, "call-1", "started"),
            "前面的读还没收尾，接着做就转出去了"
        );
        assert!(
            seq_of(&harness, "call-1", "result") < seq_of(&harness, "call-2", "started"),
            "接着做还没收尾，后面的读就开始了"
        );
        assert_eq!(
            probe.log(),
            [
                "read:call-0:进入",
                "read:call-0:离开",
                "read:call-2:进入",
                "read:call-2:离开"
            ]
        );
    }
}

/// 「刚跑完」与「重启后回放」是同一个字符串（§8.3）：每条收尾路径交给模型的正文，都与
/// 只从账本事件 + 输出存储读回事实、再投影一次的结果逐字节相同。
mod one_projection {
    use super::*;

    use komo_kernel::projection::{ToolResultFacts, project};
    use komo_kernel::protocol::config::ExecutionConfig;
    use komo_kernel::traits::{Ledger, TaskSpawner, ToolOutputStore};
    use komo_kernel::types::status::RunEnd;
    use komo_kernel::types::turn::ToolResultForModel;

    use super::delegation::{attempt_of, legacy_planned, resumed_request};
    use super::task::{FakeSpawner, task_tool};

    /// 回放那一侧拿得到的全部东西：`fold` 出来的那条结果与那次调用，加上 `output.json`
    /// 里的正文与产物——与 `komo-agent` 的 `to_replay_messages` 读的是同几格。
    async fn replayed(harness: &Harness, live: &ToolResultForModel) -> String {
        let surface = harness.ledger.surface();
        let tool = surface
            .messages
            .iter()
            .flat_map(|message| message.tool_calls.iter())
            .find(|call| call.call_id == live.call_id)
            .map(|call| call.name.clone())
            .expect("调用在账本上");
        let result = surface
            .messages
            .iter()
            .flat_map(|message| message.tool_results.iter())
            .find(|result| result.call == live.call_id)
            .cloned()
            .expect("结果落了盘");
        let body = harness
            .outputs
            .open(&result.output)
            .await
            .expect("output.json 读得回来")
            .body;
        project(
            &ToolResultFacts {
                tool: &tool,
                status: result.status,
                elapsed_ms: result.elapsed_ms,
                text: body.preview.as_deref(),
                output: &result.output,
                stdout: result.stdout.as_ref(),
                stderr: result.stderr.as_ref(),
                artifacts: &body.artifacts,
                images: &body.images,
            },
            &ExecutionConfig::default().projection(),
        )
    }

    async fn assert_one_projection(harness: &Harness, path: &str, live: &ToolResultForModel) {
        assert!(
            live.content.starts_with('['),
            "{path} 没过投影：{}",
            live.content
        );
        assert_eq!(
            live.content,
            replayed(harness, live).await,
            "{path}：刚跑完与回放要逐字节相同"
        );
    }

    /// 一条父 Run + 一个调用。
    async fn round(
        harness: &Harness,
        tool: &str,
        args: serde_json::Value,
    ) -> (SessionId, komo_kernel::types::ids::RunId, CallRequest) {
        let (session, run) = harness.open_run().await;
        let calls = harness.record_round(&run, &[(tool, args)]).await;
        (session, run, calls.into_iter().next().expect("一个调用"))
    }

    #[tokio::test]
    async fn every_settlement_path_renders_through_the_one_projection() {
        // 执行过的调用。
        {
            let harness = Harness::new();
            let executor = harness.executor(vec![Arc::new(RecordingTool::bash())]);
            let (session, run, request) =
                round(&harness, "bash", serde_json::json!({ "command": "x" })).await;
            let outcome = executor
                .execute_round(vec![request], &harness.env(&session, &run))
                .await
                .unwrap();
            assert_one_projection(&harness, "执行", &outcome.results[0]).await;
        }

        // 没有执行的调用：不认识的工具名、参数准备不出来。
        for (path, tool, args) in [
            ("未知工具", "nope", serde_json::json!({})),
            ("参数不对", "read", serde_json::json!({ "nope": 1 })),
        ] {
            let harness = Harness::new();
            let executor = harness.executor(vec![Arc::new(ReadTool::new())]);
            let (session, run, request) = round(&harness, tool, args).await;
            let outcome = executor
                .execute_round(vec![request], &harness.env(&session, &run))
                .await
                .unwrap();
            assert!(outcome.results[0].is_error, "{path}");
            assert_one_projection(&harness, path, &outcome.results[0]).await;
        }

        // task 放行即收尾；接着做转不过去也是一条落了盘的失败。
        for (path, tool, args) in [
            (
                "task",
                "task",
                serde_json::json!({ "prompt": "查一下空调状态", "title": "查空调" }),
            ),
            (
                "接着做失败",
                "task",
                serde_json::json!({ "task_id": "beef", "prompt": "再看一眼" }),
            ),
        ] {
            let harness = Harness::new();
            let executor = harness.executor_with_spawner(
                vec![task_tool()],
                FakeSpawner::new().with_unknown("beef") as Arc<dyn TaskSpawner>,
            );
            let (session, run, request) = round(&harness, tool, args).await;
            let outcome = executor
                .execute_round(vec![request], &harness.env(&session, &run))
                .await
                .unwrap();
            assert_eq!(outcome.results.len(), 1, "{path}：{:?}", outcome.stop);
            assert_one_projection(&harness, path, &outcome.results[0]).await;
        }

        // 子 Run 的终态回到父侧那次调用上。
        {
            let harness = Harness::new();
            let executor = harness.executor(vec![]);
            let (session, run, request) = round(
                &harness,
                "delegate",
                serde_json::json!({ "task": "数一下 /tmp 下有几个文件" }),
            )
            .await;
            let request = legacy_planned(&harness, &run, request).await;
            let env = harness.env(&session, &run);
            let waiting = executor
                .execute_round(vec![request.clone()], &env)
                .await
                .unwrap();
            let Some(RoundStop::Dependency { run: child, .. }) = waiting.stop else {
                panic!("{:?}", waiting.stop)
            };
            let attempt = attempt_of(&harness, &request.call);
            harness
                .ledger
                .complete(
                    &child,
                    RunEnd::Completed {
                        final_message: Some("三个".into()),
                        rounds: 1,
                    },
                )
                .await
                .unwrap();
            let done = executor
                .execute_round(vec![resumed_request(&harness, &request, &attempt)], &env)
                .await
                .unwrap();
            assert_one_projection(&harness, "委派收口", &done.results[0]).await;
        }

        // 恢复时被重启打断、不能重跑：中断结果，带部分输出。
        {
            let harness = Harness::new();
            let tool = Arc::new(RecordingTool::bash());
            let executor = harness.executor(vec![tool.clone()]);
            let (session, run, mut request) =
                round(&harness, "bash", serde_json::json!({ "command": "x" })).await;
            let previous = harness
                .crashed_attempt(
                    &request.call,
                    &plan_of(&*tool, &harness, &session, &run, &request).await,
                )
                .await;
            harness.outputs.leave_interrupted(
                previous.as_str(),
                InterruptedOutput {
                    stdout_bytes: 12,
                    stdout_tail: "step 1 done\n".into(),
                    stderr_bytes: 0,
                    stderr_tail: String::new(),
                },
            );
            request.resumed = Some(resumed_from(ToolCallState::Started, Some(previous), 1));
            let outcome = executor
                .execute_round(vec![request], &harness.env(&session, &run))
                .await
                .unwrap();
            assert_eq!(tool.ran(), 0);
            assert_one_projection(&harness, "重启打断", &outcome.results[0]).await;
        }
    }
}

/// `edit` / `write` 带 `then_run`（Action Fusion，§4、§7.1）：两步一次放行、一条结果，
/// 恢复时 started 一律交给人。
mod fused {
    use super::*;

    use komo_kernel::events::EventPayload;
    use komo_kernel::traits::Ledger;
    use komo_kernel::types::refs::ContentRef;
    use komo_store::session_log::SessionPaths;
    use komo_store::tool_output::FileToolOutputStore;

    use crate::executor::ToolExecutor;
    use crate::tools::fusion::{THEN_RUN_SKIPPED, THEN_RUN_SUCCEEDED};
    use crate::tools::{BashTool, EditTool};

    fn tools() -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(EditTool::new()),
            Arc::new(WriteTool::new()),
            Arc::new(BashTool::new()),
        ]
    }

    /// 工作目录里一份 `a.txt`，内容 `old`。
    fn seeded(harness: &Harness) -> PathBuf {
        let root = std::fs::canonicalize(harness.dir.path()).expect("真实路径");
        std::fs::write(root.join("a.txt"), "old\n").expect("写 a.txt");
        root
    }

    fn edit_then(command: &str) -> (&'static str, serde_json::Value) {
        (
            "edit",
            serde_json::json!({
                "path": "a.txt",
                "match_text": "old",
                "replace_text": "new",
                "then_run": { "command": command },
            }),
        )
    }

    /// 每跑一次命令就往 `ran.txt` 里记一行——"跑了几次"数行数就知道。
    const COUNTING: &str = "echo ran >> ran.txt";

    fn runs(root: &std::path::Path) -> usize {
        std::fs::read_to_string(root.join("ran.txt"))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    fn content(root: &std::path::Path) -> String {
        std::fs::read_to_string(root.join("a.txt")).expect("读 a.txt")
    }

    fn started(harness: &Harness) -> Vec<komo_kernel::events::ToolStarted> {
        harness
            .ledger
            .events()
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::ToolStarted(started) => Some(started.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_successful_fused_edit_returns_one_result_with_both_outputs() {
        let harness = Harness::new();
        let root = seeded(&harness);
        // 能力面按这几个工具登记；执行器自己装一份真的文件输出存储，好看 stdout 落在哪。
        let _ = harness.executor(tools());
        let (session, run) = harness.open_run().await;
        let sessions = tempfile::tempdir().unwrap();
        let store = Arc::new(FileToolOutputStore::new(
            SessionPaths::new(sessions.path(), &session),
            session.clone(),
        ));
        let executor = ToolExecutor::new(
            tools(),
            harness.ledger.clone(),
            store.clone(),
            Arc::new(harness.clock.clone()),
        );
        let calls = harness
            .record_round(&run, &[edit_then("echo hello-from-then-run; cat a.txt")])
            .await;

        let outcome = executor
            .execute_round(calls, &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert_eq!(outcome.results.len(), 1, "一个调用一条结果");
        let result = &outcome.results[0];
        assert!(!result.is_error, "{}", result.content);
        let (head, tail) = result
            .content
            .split_once(THEN_RUN_SUCCEEDED)
            .unwrap_or_else(|| panic!("投影里要有标记：{}", result.content));
        assert!(head.contains("替换了 1 处"), "{}", result.content);
        assert!(tail.contains("hello-from-then-run"), "{}", result.content);
        assert_eq!(content(&root), "new\n");
        assert_eq!(started(&harness).len(), 1, "一次尝试");

        // 命令的输出流进了**这一次尝试**的 stdout。
        let event = harness
            .ledger
            .events()
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::ToolResult(result) => Some(result.clone()),
                _ => None,
            })
            .expect("有一条 tool.result");
        let stdout: ContentRef = event.stdout.clone().expect("有 stdout 引用");
        let on_disk = std::fs::read_to_string(store.paths().root().join(&stdout.path))
            .expect("读 stdout.txt");
        assert!(on_disk.contains("hello-from-then-run"), "{on_disk}");
        assert!(on_disk.contains("new"), "命令看见的是改完的文件：{on_disk}");
        let preview = event.preview.expect("账本里有预览");
        assert!(preview.contains(THEN_RUN_SUCCEEDED), "{preview}");
        assert!(preview.len() <= PREVIEW_LIMIT_BYTES);
    }

    #[tokio::test]
    async fn a_started_fused_call_is_uncertain_on_resume_and_never_reruns() {
        let harness = Harness::new();
        let root = seeded(&harness);
        let executor = harness.executor(tools());
        let (session, run) = harness.open_run().await;
        let mut calls = harness.record_round(&run, &[edit_then(COUNTING)]).await;
        let plan = plan_of(&EditTool::new(), &harness, &session, &run, &calls[0]).await;
        let previous = harness.crashed_attempt(&calls[0].call, &plan).await;
        calls[0].plan = Some(plan);
        calls[0].resumed = Some(resumed_from(
            ToolCallState::Started,
            Some(previous.clone()),
            1,
        ));

        let outcome = executor
            .execute_round(calls, &harness.env(&session, &run))
            .await
            .unwrap();

        // 组合调用的重放策略是 unsafe：改动与命令都不重做，结果不明交给模型，Run 不停。
        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert_eq!(outcome.results.len(), 1);
        assert!(
            outcome.results[0].content.contains("被 Gateway 重启打断"),
            "{}",
            outcome.results[0].content
        );
        assert_eq!(content(&root), "old\n", "不重做改动");
        assert_eq!(runs(&root), 0, "不重跑命令");
        let results = harness.results_for(&previous);
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].status, ToolResultStatus::Uncertain);
        assert_eq!(started(&harness).len(), 1, "没有新的尝试");
    }

    /// 一次在途调用的恢复：计划落了账、上一世 `started` 而无结果，但工具名后来从工具表里没了
    /// （改名、删除）。结果不明、交给模型，不重跑，也不报"没有这个工具"。
    async fn interrupted_call_of_retired_tool(old_name: &str, operation: Option<Operation>) {
        let harness = Harness::new();
        let root = seeded(&harness);
        let executor = harness.executor(tools());
        let (session, run) = harness.open_run().await;
        let mut calls = harness
            .record_round(
                &run,
                &[(old_name, serde_json::json!({ "command": COUNTING }))],
            )
            .await;
        let mut plan = plan_of(&BashTool::new(), &harness, &session, &run, &calls[0]).await;
        plan.tool = old_name.into();
        if let Some(operation) = operation {
            plan.operation = operation;
        }
        let previous = harness.crashed_attempt(&calls[0].call, &plan).await;
        calls[0].plan = Some(plan);
        calls[0].resumed = Some(resumed_from(
            ToolCallState::Started,
            Some(previous.clone()),
            1,
        ));

        let outcome = executor
            .execute_round(calls, &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert!(
            outcome.results[0].content.contains("被 Gateway 重启打断"),
            "{}",
            outcome.results[0].content
        );
        assert_eq!(runs(&root), 0, "不重跑命令");
        let results = harness.results_for(&previous);
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].status, ToolResultStatus::Uncertain);
        assert_eq!(started(&harness).len(), 1, "没有新的尝试");
    }

    #[tokio::test]
    async fn an_interrupted_call_to_the_renamed_shell_tool_settles_as_uncertain() {
        interrupted_call_of_retired_tool("shell", None).await;
    }

    #[tokio::test]
    async fn an_interrupted_call_to_the_removed_python_tool_settles_as_uncertain() {
        interrupted_call_of_retired_tool("python", Some(Operation::PythonCode)).await;
    }

    /// 工具后来没了、但上一世只落了计划没 `started` 的调用：确定没执行过，照旧被拒，
    /// 不补开尝试、不记 uncertain。
    #[tokio::test]
    async fn a_planned_only_call_to_a_retired_tool_is_refused_not_uncertain() {
        let harness = Harness::new();
        let root = seeded(&harness);
        let executor = harness.executor(tools());
        let (session, run) = harness.open_run().await;
        let mut calls = harness
            .record_round(
                &run,
                &[("python", serde_json::json!({ "command": COUNTING }))],
            )
            .await;
        let mut plan = plan_of(&BashTool::new(), &harness, &session, &run, &calls[0]).await;
        plan.tool = "python".into();
        plan.operation = Operation::PythonCode;
        harness
            .ledger
            .plan_call(&calls[0].call, &plan)
            .await
            .unwrap();
        calls[0].plan = Some(plan);
        calls[0].resumed = Some(resumed_from(ToolCallState::Planned, None, 1));

        let outcome = executor
            .execute_round(calls, &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert!(outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains("可用的是"),
            "{}",
            outcome.results[0].content
        );
        assert_eq!(runs(&root), 0);
        assert!(started(&harness).is_empty(), "没有补开尝试");
    }

    /// 只有带着落账计划的在途调用走"结果不明"：模型这一轮新发的 `shell` / `python` 没有
    /// 计划，仍被拒，并列出这次能用的工具。
    #[tokio::test]
    async fn a_fresh_call_to_a_retired_tool_is_refused_with_the_available_tools() {
        for old_name in ["shell", "python"] {
            let harness = Harness::new();
            let root = seeded(&harness);
            let executor = harness.executor(tools());
            let (session, run) = harness.open_run().await;
            let calls = harness
                .record_round(
                    &run,
                    &[(old_name, serde_json::json!({ "command": COUNTING }))],
                )
                .await;

            let outcome = executor
                .execute_round(calls, &harness.env(&session, &run))
                .await
                .unwrap();

            assert!(outcome.results[0].is_error);
            let content = &outcome.results[0].content;
            assert!(
                content.contains(old_name) && content.contains("可用的是"),
                "{content}"
            );
            assert_eq!(runs(&root), 0);
            assert!(started(&harness).is_empty());
        }
    }

    #[tokio::test]
    async fn a_planned_only_fused_call_runs_once() {
        let harness = Harness::new();
        let root = seeded(&harness);
        let executor = harness.executor(tools());
        let (session, run) = harness.open_run().await;
        let mut calls = harness.record_round(&run, &[edit_then(COUNTING)]).await;
        let plan = plan_of(&EditTool::new(), &harness, &session, &run, &calls[0]).await;
        harness
            .ledger
            .plan_call(&calls[0].call, &plan)
            .await
            .unwrap();
        calls[0].plan = Some(plan);
        calls[0].resumed = Some(resumed_from(ToolCallState::Planned, None, 1));

        let outcome = executor
            .execute_round(calls, &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert!(
            !outcome.results[0].is_error,
            "{}",
            outcome.results[0].content
        );
        assert_eq!(content(&root), "new\n");
        assert_eq!(runs(&root), 1);
        assert_eq!(started(&harness).len(), 1);
    }

    #[tokio::test]
    async fn a_fused_call_without_bash_on_the_surface_is_refused() {
        let harness = Harness::new();
        let root = seeded(&harness);
        let executor = harness.executor(tools());
        let (session, run) = harness.open_run().await;
        let calls = harness.record_round(&run, &[edit_then(COUNTING)]).await;
        let surface = AgentSurface::new(["edit", "write"]);
        let env = super::super::CallEnv {
            surface: surface.clone(),
            ..harness.env(&session, &run)
        };

        let outcome = executor.execute_round(calls, &env).await.unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert!(outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains("没有 bash"),
            "{}",
            outcome.results[0].content
        );
        assert_eq!(content(&root), "old\n");
        assert_eq!(runs(&root), 0);
        assert!(started(&harness).is_empty());

        // 模型在这个面上根本看不见 then_run；面上有 bash 时看得见。
        let has_then_run = |surface: &AgentSurface| {
            executor
                .definitions_for(surface)
                .iter()
                .filter(|definition| ["edit", "write"].contains(&definition.name.as_str()))
                .map(|definition| {
                    definition.parameters["properties"]
                        .get("then_run")
                        .is_some()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(has_then_run(&surface), [false, false]);
        assert_eq!(
            has_then_run(&AgentSurface::new(["edit", "write", "bash"])),
            [true, true]
        );
    }

    /// 读一下 `marker` 在不在，进门、出门各记一次。
    struct Peek {
        marker: PathBuf,
        log: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Tool for Peek {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "read".into(),
                description: "测试用".into(),
                parameters: serde_json::json!({ "type": "object" }),
            }
        }

        async fn prepare(
            &self,
            args: serde_json::Value,
            ctx: &ToolContext,
        ) -> Result<ExecutionPlan, ToolError> {
            Ok(ExecutionPlan {
                operation_id: OperationId::from_raw(format!("op-{}", ctx.call)),
                source: ctx.source.clone(),
                tool: "read".into(),
                operation: Operation::ReadFile,
                run: Some(ctx.run.clone()),
                tool_call: Some(ctx.call.clone()),
                args,
                cwd: Some(ctx.cwd.clone()),
                targets: vec![PlanTarget::local(ctx.cwd.join("a.txt"), TargetAccess::Read)],
                versions: PlanVersions::default(),
                recovery: RecoveryMode::SafeReread,
                then_run: None,
            })
        }

        async fn execute(
            &self,
            plan: ExecutionPlan,
            ctx: &ToolContext,
            _sink: &mut dyn OutputWriter,
        ) -> Result<ToolOutput, ToolError> {
            let seen = |when: &str| format!("{}:{when}:{}", ctx.call, self.marker.exists());
            self.log.lock().expect("日志").push(seen("进入"));
            let delay = plan.args["delay_ms"].as_u64().unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            self.log.lock().expect("日志").push(seen("离开"));
            Ok(ToolOutput {
                status: ToolResultStatus::Completed,
                result: serde_json::json!({}),
                exit_code: None,
                artifacts: vec![],
                images: vec![],
                preview: Some("看过了".into()),
            })
        }
    }

    /// 组合调用是**屏障**：它前面在飞的读先收尾，它跑完后面的读才开始（§6）。
    #[tokio::test]
    async fn fused_calls_are_barriers() {
        let harness = Harness::new();
        let root = seeded(&harness);
        let log = Arc::new(Mutex::new(Vec::new()));
        let peek = Arc::new(Peek {
            marker: root.join("marker"),
            log: Arc::clone(&log),
        });
        let mut all = tools();
        all.push(peek);
        let executor = harness.executor(all);
        let (session, run) = harness.open_run().await;
        let calls = harness
            .record_round(
                &run,
                &[
                    ("read", serde_json::json!({ "delay_ms": 150 })),
                    edit_then("touch marker"),
                    ("read", serde_json::json!({ "delay_ms": 0 })),
                ],
            )
            .await;

        let outcome = executor
            .execute_round(calls, &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert_eq!(outcome.results.len(), 3);
        assert_eq!(
            log.lock().expect("日志").clone(),
            [
                "call-0:进入:false",
                "call-0:离开:false",
                "call-2:进入:true",
                "call-2:离开:true",
            ],
            "组合调用没有和前后的读同时在飞"
        );
    }

    /// 一个跳过的组合调用也只有一条结果，标记在正文里。
    #[tokio::test]
    async fn a_failed_mutation_skips_the_command() {
        let harness = Harness::new();
        let root = seeded(&harness);
        let executor = harness.executor(tools());
        let (session, run) = harness.open_run().await;
        let calls = harness
            .record_round(
                &run,
                &[(
                    "write",
                    serde_json::json!({
                        "path": "a.txt",
                        "content": "new\n",
                        "expected_version": "not-the-hash",
                        "then_run": { "command": COUNTING },
                    }),
                )],
            )
            .await;

        let outcome = executor
            .execute_round(calls, &harness.env(&session, &run))
            .await
            .unwrap();

        assert!(outcome.stop.is_none(), "{:?}", outcome.stop);
        assert!(outcome.results[0].is_error);
        assert!(
            outcome.results[0].content.contains(THEN_RUN_SKIPPED),
            "{}",
            outcome.results[0].content
        );
        assert_eq!(content(&root), "old\n");
        assert_eq!(runs(&root), 0, "改动失败，命令没跑");
    }
}
