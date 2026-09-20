//! §7.5 的 `verify` 结论：**核对 → 按 §8.4 行事，没有"我说它发生了"。**
//!
//! 三类结论各自的落点是这一组要钉住的：
//!
//! - `satisfied`：给那次调用补一条"核对后目标已满足"的结果（`completed`），原 Run 继续
//!   ——**动作不重跑**（副作用次数就是判据）；
//! - `not_performed`：把那次调用按**失败**收尾，Run 重新入队（重做是**新的一次调用**，
//!   照常过 Policy，§7.4）；
//! - `abandon`：终态 `abandoned`（§8.4：与 `cancelled` 分开记）。
//!
//! 衬里是 §14 故障注入表第 6 行那一段：副作用已经发生、完整输出还没落盘，重启之后
//! `tool.started` 没有配对的结果——于是它停在一条 `verify` 上等人核对（§8.6）。

use komo_kernel::protocol::http::{
    InterventionAnswerResponse, InterventionKind, InterventionVerdict,
};
use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::status::{RunState, WaitReason};

use crate::harness::*;

/// 一条停在"结果不明"上的 Run，以及回答它需要的那几样东西。
struct Uncertain {
    session: SessionId,
    run: RunId,
    call: ToolCallId,
    attempt: AttemptId,
    counter: Counter,
    gateway: Gw,
}

/// 造一条 `verify`：审批放行 → shell 真的跑一次 → 拉掉那次尝试的输出 → 重启。
///
/// 与端到端验收里 6b 那一段同一个做法：**外部副作用已经发生，而完整输出没有落盘**，
/// 这是 §8.6 说的那段窗口——它只能由人来核对，不能靠重跑掩盖。
async fn an_uncertain_call(home: &Home) -> Uncertain {
    let counter = Counter::new(home, "verdict.count");
    let fault = home.inject(Fault::BeforeFinishCall);
    let gateway = home
        .start(FakeLlm::new(vec![vec![
            call_round(
                1,
                "pc-v",
                "shell",
                serde_json::json!({ "command": counter.append_command() }),
            ),
            text_round(2, "跑过了。"),
        ]]))
        .await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "verdict-1", "跑一条命令")
        .await
        .run;
    let record = gateway.wait_approval().await;
    gateway.decide(&record.approval, true).await;
    fault.wait_tripped().await;
    gateway.stop().await;
    assert_eq!(counter.count(), 1, "动作发出去了，结果丢了");

    let events = home.events(&session);
    let started = tool_started(&events)
        .last()
        .map(|started| (*started).clone())
        .expect("有这次尝试");
    home.drop_attempt_output(&session, &run, &started);
    home.clear_injection();

    let gateway = home.start(FakeLlm::finisher("接着往下写。")).await;
    let why = gateway.wait_waiting(&run).await;
    assert!(
        matches!(why, WaitReason::Intervention { .. }),
        "结果不明要人核对：{why:?}"
    );

    Uncertain {
        session,
        run,
        call: started.call_id.clone(),
        attempt: started.attempt_id.clone(),
        counter,
        gateway,
    }
}

/// 那条 Run 上现在待处理的那一条——`verify` 的句柄就是 Run ID（§7.5 那张表）。
async fn pending_verify(gateway: &Gw, run: &RunId) -> String {
    let listed = gateway.interventions().await;
    let entry = listed
        .iter()
        .find(|one| one.run.as_ref() == Some(run))
        .unwrap_or_else(|| panic!("这一条要在清单里：{listed:?}"));
    assert_eq!(entry.kind, InterventionKind::Verify, "{entry:?}");
    assert_eq!(entry.handle, run.to_string(), "`verify` 的句柄是 Run ID");
    assert!(
        entry.verdicts.contains(&InterventionVerdict::Satisfied),
        "{entry:?}"
    );
    entry.handle.clone()
}

#[tokio::test]
async fn satisfied_writes_a_result_and_requeues_without_rerunning() {
    let home = Home::new();
    let fixture = an_uncertain_call(&home).await;
    let handle = pending_verify(&fixture.gateway, &fixture.run).await;

    // 结论与种类不符：**422**，而且一个字都不改（§7.5 第 3 条：结论按种类分派）。
    let (code, body) = fixture
        .gateway
        .post_json(
            &format!("/v1/interventions/{handle}/answer"),
            serde_json::json!({ "verdict": "approve" }),
        )
        .await;
    assert_eq!(code, 422, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request", "{body}");
    assert_eq!(
        fixture.gateway.db_state(&fixture.run).await,
        RunState::Waiting,
        "答不上的结论不该动这条 Run"
    );

    let body = fixture.gateway.answer(&handle, "satisfied", None).await;
    let answered: InterventionAnswerResponse = serde_json::from_str(&body).expect("回执");
    assert_eq!(answered.verdict, InterventionVerdict::Satisfied, "{body}");
    assert!(!answered.already_answered, "{body}");

    // 那次尝试配上了结果：`completed`，**动作没有重跑**（§7.5）。
    let events = home.events(&fixture.session);
    assert!(
        unpaired_attempts(&events).is_empty(),
        "started 要配一条结果：{:?}",
        unpaired_attempts(&events)
    );
    // **最后一条**才是核对写下的那条：那次尝试先前已经有一条 `uncertain` 的结果
    // （§14 的目录验收要求每个 `started` 都配一条明确的结果），核对是给它再补一条。
    let settled = tool_results(&events)
        .into_iter()
        .rfind(|result| result.attempt_id == fixture.attempt)
        .expect("配上了");
    assert_eq!(settled.status, ToolResultStatus::Completed);
    assert_eq!(fixture.counter.count(), 1, "核对不是重跑");

    // 回到队列，接着往下跑；答过之后它不在清单里了。
    assert_eq!(
        fixture
            .gateway
            .wait_terminal(&fixture.run)
            .await
            .summary
            .state,
        RunState::Completed
    );
    let listed = fixture.gateway.interventions().await;
    assert!(
        listed
            .iter()
            .all(|one| one.run.as_ref() != Some(&fixture.run)),
        "{listed:?}"
    );

    // 再答一次：**已答复的返回原结论，不报错**（§11.3）。
    let body = fixture.gateway.answer(&handle, "satisfied", None).await;
    let again: InterventionAnswerResponse = serde_json::from_str(&body).expect("回执");
    assert!(again.already_answered, "{body}");
    assert_eq!(again.verdict, InterventionVerdict::Satisfied, "{body}");
    let _ = &fixture.call;
}

#[tokio::test]
async fn not_performed_closes_the_call_as_failed_and_requeues() {
    let home = Home::new();
    let fixture = an_uncertain_call(&home).await;
    let handle = pending_verify(&fixture.gateway, &fixture.run).await;

    let body = fixture.gateway.answer(&handle, "not_performed", None).await;
    let answered: InterventionAnswerResponse = serde_json::from_str(&body).expect("回执");
    assert_eq!(
        answered.verdict,
        InterventionVerdict::NotPerformed,
        "{body}"
    );

    // 「确定没执行」写成一条**失败**结果：重做是一次新调用，照常过 Policy（§7.4）。
    let events = home.events(&fixture.session);
    assert!(
        unpaired_attempts(&events).is_empty(),
        "{:?}",
        unpaired_attempts(&events)
    );
    let settled = tool_results(&events)
        .into_iter()
        .rfind(|result| result.attempt_id == fixture.attempt)
        .expect("配上了");
    assert_eq!(settled.status, ToolResultStatus::Failed);
    assert_eq!(fixture.counter.count(), 1, "它确实没有执行过第二次");

    // 回到队列接着跑：模型拿到的是那次调用的失败结果。
    assert_eq!(
        fixture
            .gateway
            .wait_terminal(&fixture.run)
            .await
            .summary
            .state,
        RunState::Completed
    );
    assert_eq!(fixture.counter.count(), 1);
}

#[tokio::test]
async fn abandon_is_its_own_terminal_state() {
    let home = Home::new();
    let fixture = an_uncertain_call(&home).await;
    let handle = pending_verify(&fixture.gateway, &fixture.run).await;

    let body = fixture.gateway.answer(&handle, "abandon", None).await;
    let answered: InterventionAnswerResponse = serde_json::from_str(&body).expect("回执");
    assert_eq!(answered.verdict, InterventionVerdict::Abandon, "{body}");
    assert_eq!(answered.run_state, Some(RunState::Abandoned), "{body}");

    // 终态是 `abandoned`，**不是** `cancelled`：不是用户不想跑了，而是这件事不会再有下文
    // （§8.4 的四个终态各自是什么）。事件里也留得下这一条。
    let detail = fixture.gateway.wait_terminal(&fixture.run).await;
    assert_eq!(detail.summary.state, RunState::Abandoned);
    let events = home.events(&fixture.session);
    assert!(
        events.iter().any(|event| matches!(
            &event.payload,
            komo_kernel::events::EventPayload::RunAbandoned(_)
        )),
        "要有 run.abandoned：{:?}",
        home.event_types(&fixture.session)
    );

    // 清单里不再有它，而且没有为了让谁好过而重跑过命令。
    let listed = fixture.gateway.interventions().await;
    assert!(
        listed
            .iter()
            .all(|one| one.run.as_ref() != Some(&fixture.run)),
        "{listed:?}"
    );
    assert_eq!(fixture.counter.count(), 1);
}

/// 这条 Run 上那条 `uncertain` 的调用就是 `verify` 的落点（§8.6 的窗口在**调用**上，
/// 不在 Run 上）——详情里看得到工具名与原因。
#[tokio::test]
async fn the_verify_detail_names_the_uncertain_call() {
    let home = Home::new();
    let fixture = an_uncertain_call(&home).await;
    let handle = pending_verify(&fixture.gateway, &fixture.run).await;

    let (code, detail) = fixture
        .gateway
        .get_json(&format!("/v1/interventions/{handle}"))
        .await;
    assert_eq!(code, 200, "{detail}");
    assert_eq!(detail["kind"], "verify", "{detail}");
    assert_eq!(detail["tool"], "shell", "{detail}");
    assert_eq!(
        detail["summary"]["call"],
        serde_json::json!(fixture.call.to_string()),
        "停在哪一次调用上是权威表回答的：{detail}"
    );
    assert!(
        detail["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "要说得出为什么不清楚：{detail}"
    );
}
