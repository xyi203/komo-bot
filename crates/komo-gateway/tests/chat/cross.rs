//! 跨场景：HTTP 提交的排队与取消、介入清单与答复（§8.4、§7.5）——Durable 存储版
//! （D5 W3a）：与 `../cross.rs` 同名同断言，Gateway 跑在新存储上。
//!
//! 聊天渠道删除后（`docs/redesign.md` P17）这里只留与渠道无关的那几条：消息一律走
//! `POST /v1/sessions/{id}/runs`。准入、命令表、投递目标、重启补发、会话归属这些渠道专属的测试随渠道删除。
//!
//! 与 Legacy 不同之处各在对应测试的注释里写了理由：
//!
//! - `a_faulted_run_is_listed_as_an_intervention_and_answered_once`：意图（要人处理的 Run 进清单、
//!   第二个答复得到"已经答复过了"）新存储里仍然成立，只是触发它的不再是"结果不明"而是 **faulted
//!   任务**。写成 Durable 版（用 `poison_agent_doc` 让下一条输入的生成任务 fault）；
//! - `a_round_stopped_at_its_first_call_still_finishes_the_rest`：**不迁**。它守的是"第一个调用停在介入上时，
//!   续跑要把后面没有计划的调用也跑完，否则转写里有调用没输出、provider 400"。新存储没有"停在半轮"这个
//!   状态：harness 在一轮的全部调用都有结果（结果不明的也是一条结果）之后才发下一次请求，runtime 的
//!   `harness/tests/recovery.rs::aborting_a_round_answers_every_call_and_the_next_request_is_fully_paired` /
//!   `a_faulted_tool_is_answered_by_derivation_rule_8_and_the_run_continues` 钉着。网关层换成一个仍然成立的
//!   版本 `a_round_with_two_calls_gets_both_results_before_the_next_request`（一轮两个调用、第一个停在 gate 上、
//!   排队的下一条输入不夹进去）。

use std::sync::Arc;

use komo_gateway::service::test_support::Gate;
use komo_kernel::types::status::RunState;

use crate::harness::{FakeLlm, GatewayBuilder, TestGateway, call_round, config_toml, text_round};

/// §8.4：前一个 Run 还在跑时，**同一 Session 的下一条消息不越过它**。
///
/// 越过去不是"顺序不好看"：停住的那半轮里那次调用还没有结果，回放窗口只能带着一个没有输出的
/// `function_call` 发给模型，provider 直接 400（`No tool output found for tool call …`）。
///
/// 改了断言：Legacy 让前一条停在介入上（`waiting + intervention`），后一条是 `waiting + dependency`
/// 并说出在等哪一条；新存储里后一条是收件箱里排着的 follow-up（`queued`），前一条收尾的边界才把它放进
/// 上下文——前一条这里忙在 `gate` 上。其余不变：排着的那条不走到模型，前一条跑完后它才轮到，
/// 交给模型的每一份转写都不把用户消息夹在没有输出的调用中间，后一条的窗口里看得见前一条说了什么。
#[tokio::test]
async fn a_message_behind_a_waiting_run_waits_instead_of_overtaking() {
    let gate = Gate::new();
    let llm = FakeLlm::new(vec![
        // 第一段：调 `gate` 停在门上，门开之后收尾。
        vec![
            call_round(1, "pc-1", "gate", serde_json::json!({})),
            text_round(2, "跑完了。"),
        ],
        // 排队那条 Run 的那一段。
        vec![text_round(1, "y")],
    ]);
    let gateway = GatewayBuilder::new(&config_toml(""))
        .fake_llm(Arc::clone(&llm))
        .tool(gate.tool())
        .start()
        .await;
    let (_, body) = gateway.get("/v1/home-session").await;
    let session = serde_json::from_str::<komo_kernel::protocol::http::SessionSummary>(&body)
        .expect("home session")
        .session;
    let submit = |key: &'static str, text: &'static str| {
        let session = session.clone();
        let gateway = &gateway;
        async move {
            let (code, body) = gateway
                .post(
                    &format!("/v1/sessions/{session}/runs"),
                    serde_json::json!({ "request_key": key, "text": text }),
                )
                .await;
            assert_eq!(code, 200, "{body}");
            serde_json::from_str::<komo_kernel::protocol::http::SubmitRunResponse>(&body)
                .expect("回执")
                .run
        }
    };

    let first = submit("overtake-1", "跑一下 echo").await;
    gate.wait_entered(1).await;
    let turns_before = llm.turns();

    // 同一个 Session 再来一条：前一个没结束，它不许越过。
    let second = submit("overtake-2", "y").await;
    let queued = gateway
        .state()
        .core()
        .unwrap()
        .submission_state(komo_kernel::durable::legacy_ids::submission_of(&second).unwrap())
        .await
        .unwrap();
    assert_eq!(
        queued,
        RunState::Queued,
        "前一个还在跑，后一个只在收件箱里排着"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(llm.turns(), turns_before, "排队的这条不该走到模型");

    // 前一个跑完，后一个才轮到。
    gate.release();
    gateway.wait_run(&first).await;
    gateway.wait_run(&second).await;

    // 而且交给模型的每一份转写都过得了 provider 那一关。
    let requests = llm.requests.lock().expect("脚本模型").clone();
    assert_transcripts_close_every_call(&requests);

    // §8.3：后一条 Run 的窗口里要看得见前一条说了什么、最后答了什么——用户那句"接着上面
    // 说"指的就是它，少了这两句，模型只能靠猜。
    let turns: Vec<&str> = requests
        .iter()
        .filter_map(|request| {
            let texts: Vec<&str> = request
                .messages
                .iter()
                .filter_map(|message| message.text.as_deref())
                .collect();
            texts.contains(&"y").then_some(texts)
        })
        .next_back()
        .expect("第二条 Run 的这一轮到过模型");
    assert!(
        turns.contains(&"跑一下 echo"),
        "上一轮的用户输入：{turns:?}"
    );
    assert!(turns.contains(&"跑完了。"), "上一轮的最后回复：{turns:?}");
    assert!(turns.contains(&"y"), "这一轮的输入：{turns:?}");
}

/// 转写里**调用还没有输出之前，不能出现下一条用户消息**。
///
/// 这就是 provider 判的那条（`No tool output found for tool call …`）：夹在中间的用户消息
/// 会让那次调用看起来永远没有输出。排队那条 Run 的输入恰好会长在这个位置——接收即落盘
/// （§8.5），而它开跑要等前一个结束。
fn assert_transcripts_close_every_call(requests: &[komo_kernel::types::turn::TurnRequest]) {
    for (index, request) in requests.iter().enumerate() {
        let mut open = 0usize;
        for message in &request.messages {
            let shape = format!(
                "{:?}({}){:?}",
                message.role,
                message.tool_calls.len(),
                message.text.as_deref().unwrap_or("")
            );
            match message.role {
                komo_kernel::types::turn::Role::User => assert_eq!(
                    open,
                    0,
                    "第 {index} 份转写把用户消息夹在了没有输出的调用中间：{shape}；\n{}",
                    transcript(request)
                ),
                komo_kernel::types::turn::Role::Assistant => open += message.tool_calls.len(),
                komo_kernel::types::turn::Role::Tool => open = 0,
            }
        }
    }
}

fn transcript(request: &komo_kernel::types::turn::TurnRequest) -> String {
    request
        .messages
        .iter()
        .map(|message| {
            format!(
                "  {:?} calls={} results={} text={:?}",
                message.role,
                message.tool_calls.len(),
                message.tool_results.len(),
                message.text.as_deref().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 同一会话里，前一条停在 `gate` 上（`running`、占着名额）时，取消（`POST /v1/runs/{id}/cancel`）
/// 取消的是它，门不必开——取消不能卡在一次还没返回的调用上。
#[tokio::test]
async fn cancel_stops_a_run_that_is_still_inside_a_call() {
    let gate = Gate::new();
    let gateway = GatewayBuilder::new(&config_toml(""))
        .fake_llm(FakeLlm::new(vec![vec![
            call_round(1, "pc-1", "gate", serde_json::json!({})),
            text_round(2, "不该走到这里。"),
        ]]))
        .tool(gate.tool())
        .start()
        .await;
    let (_, run) = gateway.say("等着", "cancel-1").await;
    gate.wait_entered(1).await;

    let (code, body) = gateway
        .post(&format!("/v1/runs/{run}/cancel"), serde_json::json!({}))
        .await;
    assert_eq!(code, 200, "{body}");
    // 取消是异步的：等这条 run 的输入结算，它的状态是已取消（`aborted`），门没有开过。
    let record = gateway.wait_run(&run).await;
    assert_eq!(
        komo_gateway::service::durable::sessions::runs::state_of_submission(&record),
        RunState::Cancelled,
        "没取消掉：{record:?}"
    );
}

// ---------------------------------------------------------------- W4b：intervention 一组

/// 让这个 Session 当前分支的下一条输入 fault：把 `komo.agent` 写成读不出来的值，钩子 `setup` 读它失败
/// （`HookError::Failed`）→ 生成任务 faulted。（faulted 是新存储里"要人处理"的唯一来源。）
async fn poison_agent_doc(gateway: &TestGateway, session: &str) {
    use komo_gateway::service::durable::sessions::agent_doc::{AGENT_DOC_VERSION, address};
    let core = Arc::clone(gateway.state().core().expect("Durable 模式"));
    let branch = core.session(session).await.expect("会话").current_branch;
    core.engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.put_document(
                    address(branch),
                    AGENT_DOC_VERSION,
                    serde_json::json!("坏掉的 komo.agent"),
                )
                .await
            })
        })
        .await
        .expect("写坏 komo.agent");
}

/// 验证列：要人处理的 Run 进待处理清单；第二个答复得到「已经答复过了」。
///
/// 改了做法：Legacy 用 `unsure` 停住 Run（`verify`，答 `satisfied`）；新存储里要人处理的只有 faulted
/// 任务，可答的结论只有 `abandon`（决定 13）。让第二条输入的生成任务 fault，等它出现在清单里，答两次 `abandon`。
/// （Legacy 的这一条还断言介入"投到来源会话 + home chat"；投递随聊天渠道一起删除，这里不再断言。）
#[tokio::test]
async fn a_faulted_run_is_listed_as_an_intervention_and_answered_once() {
    let gateway = GatewayBuilder::new(&config_toml(""))
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;

    // 先说一句建出会话，再让它 fault。
    let (session, run) = gateway.say("在吗", "iv-1").await;
    gateway.wait_run(&run).await;
    poison_agent_doc(&gateway, session.as_str()).await;
    let run = gateway.say_in(&session, "跑一下", "iv-2", false).await.run;
    let pending = gateway.wait_intervention(&run).await;

    // 第一个答复。
    let first: komo_kernel::protocol::http::InterventionAnswerResponse =
        serde_json::from_str(&gateway.answer(&pending.handle, "abandon").await).expect("回执");
    assert!(!first.already_answered);

    // 第二个答复得到「已经答复过了」。
    let second: komo_kernel::protocol::http::InterventionAnswerResponse =
        serde_json::from_str(&gateway.answer(&pending.handle, "abandon").await).expect("回执");
    assert!(
        second.already_answered,
        "第二个答复应当得到原结论：{second:?}"
    );
}

/// 一轮要了两个调用、**第一个停在 `gate` 上**时：两个调用都有结果之后才发下一次请求，排队的下一条输入
/// 不夹在没有输出的调用中间。
///
/// 对应 Legacy 的 `a_round_stopped_at_its_first_call_still_finishes_the_rest`，换成新存储里仍然成立的版本：
/// Legacy 守的是"第一个调用停在介入上、续跑要把后面连计划都没有的调用也跑完"；新存储没有停在半轮的状态，
/// 一轮的调用都收尾（含结果不明）才发下一次请求，这里证网关层看到的就是这样——两个调用各有一条结果，
/// 每份转写都不把用户消息夹在没有输出的调用中间。
#[tokio::test]
async fn a_round_with_two_calls_gets_both_results_before_the_next_request() {
    use komo_gateway::service::test_support::harness::multi_call_round;
    let gate = Gate::new();
    let llm = FakeLlm::new(vec![
        vec![
            multi_call_round(
                1,
                &[
                    ("call_00_one", "gate", serde_json::json!({})),
                    (
                        "call_01_two",
                        "bash",
                        serde_json::json!({"command": "echo two"}),
                    ),
                ],
            ),
            text_round(2, "两条都跑完了。"),
        ],
        // 排队那条输入的那一段。
        vec![text_round(1, "y")],
    ]);
    let gateway = GatewayBuilder::new(&config_toml(""))
        .fake_llm(Arc::clone(&llm))
        .tool(gate.tool())
        .start()
        .await;
    let (_, body) = gateway.get("/v1/home-session").await;
    let session = serde_json::from_str::<komo_kernel::protocol::http::SessionSummary>(&body)
        .expect("home session")
        .session;
    let submit = |key: &'static str, text: &'static str| {
        let session = session.clone();
        let gateway = &gateway;
        async move {
            let (code, body) = gateway
                .post(
                    &format!("/v1/sessions/{session}/runs"),
                    serde_json::json!({ "request_key": key, "text": text }),
                )
                .await;
            assert_eq!(code, 200, "{body}");
            serde_json::from_str::<komo_kernel::protocol::http::SubmitRunResponse>(&body)
                .expect("回执")
                .run
        }
    };

    let first = submit("two-calls", "跑两条命令").await;
    gate.wait_entered(1).await;
    let second = submit("two-calls-2", "y").await;
    gate.release();
    gateway.wait_run(&first).await;
    gateway.wait_run(&second).await;

    // 两个调用都有结果。
    let events = gateway.events(session.as_str()).await;
    let called: std::collections::BTreeSet<String> = events
        .iter()
        .filter_map(|event| match &event.payload {
            komo_kernel::events::EventPayload::ToolResult(result) => {
                Some(result.call_id.to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        called,
        ["call_00_one", "call_01_two"]
            .iter()
            .map(ToString::to_string)
            .collect(),
        "一轮里的两次调用都要有结果"
    );

    // 而且每一步交给模型的转写都过得了 provider 那一关。
    let requests = llm.requests.lock().expect("脚本模型").clone();
    assert_transcripts_close_every_call(&requests);
}
