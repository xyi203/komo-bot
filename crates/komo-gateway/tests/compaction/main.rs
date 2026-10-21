//! §6 的在线压缩，端到端：真数据目录、真 `service::start`、脚本化模型。
//!
//! - **窗口保护**：`context_window` 调得很小，几次大 `read` 把估计的上下文推过线——摘要
//!   请求没有工具；同一个 Run 接着跑，下一次主请求是任务 → 摘要 → 切点起原样；中途停在
//!   一条介入上、重启再答，回放的那一段逐字节不变。
//! - **不贴窗口不压**：开着压缩、短任务——不多发任何请求，driver 原样接着跑，模型拿到的
//!   轮输入与关着压缩时一模一样，日志里没有 `context.compacted`。
//!
//! 断言的是**模型收到了什么**（`FakeLlm` 记下的请求与轮输入）和日志里的 `context.compacted`。

use std::sync::Arc;

use komo_agent::context::COMPACTION_PREFIX;
use komo_agent::context::compaction::price;
use komo_gateway::service::test_support::Unsure;
use komo_gateway::service::test_support::harness::{
    FakeLlm, Home, call_round, config_toml, text_round,
};
use komo_kernel::compaction::CompactionReason;
use komo_kernel::events::{ContextCompacted, Event, EventPayload};
use komo_kernel::traits::LlmClient;
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::{Role, RoundInput, TurnRequest};

fn compactions(events: &[Event]) -> Vec<ContextCompacted> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ContextCompacted(body) => Some(body.clone()),
            _ => None,
        })
        .collect()
}

/// 模型拿到的每一份轮输入，去掉运行时自己发的 `call_id` 与抬头里的耗时：两台 Gateway
/// 之间可比。
fn fed(inputs: &[RoundInput]) -> Vec<Option<Vec<(String, String, bool)>>> {
    inputs
        .iter()
        .map(|input| match input {
            RoundInput::First => None,
            RoundInput::ToolResults { results, revised } => {
                assert!(revised.is_empty());
                Some(
                    results
                        .iter()
                        .map(|r| {
                            (
                                r.provider_call_id.clone(),
                                without_elapsed(&r.content),
                                r.is_error,
                            )
                        })
                        .collect(),
                )
            }
        })
        .collect()
}

/// 抬头 `[工具 · 状态 · 耗时 …]` 的第三段是墙钟：两台 Gateway 各跑各的，负载下差几毫秒。
fn without_elapsed(content: &str) -> String {
    let Some((head, body)) = content.split_once('\n') else {
        return content.to_string();
    };
    let mut fields: Vec<&str> = head.split(" · ").collect();
    if let Some(elapsed) = fields.get_mut(2) {
        *elapsed = if elapsed.ends_with(']') {
            "…]"
        } else {
            "…"
        };
    }
    format!("{}\n{body}", fields.join(" · "))
}

/// 两条小命令 → 收尾。
fn short_script()
-> Vec<Vec<Result<komo_kernel::types::turn::Round, komo_kernel::types::turn::LlmError>>> {
    vec![vec![
        call_round(
            1,
            "pc-b1",
            "bash",
            serde_json::json!({ "command": "echo a" }),
        ),
        call_round(
            2,
            "pc-b2",
            "bash",
            serde_json::json!({ "command": "echo b" }),
        ),
        text_round(3, "都做完了。"),
    ]]
}

async fn run_short(config: &str) -> (Arc<FakeLlm>, Vec<Event>) {
    let home = Home::with_config(config);
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    let llm = FakeLlm::new(short_script());
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "short-1", "跑两条命令").await.run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let events = home.events(&session);
    gateway.stop().await;
    (llm, events)
}

/// 开着压缩、不贴窗口：什么都不记、不多发请求，模型拿到的与关着时一样。
#[tokio::test]
async fn a_short_task_is_never_compacted_and_the_driver_goes_on_unchanged() {
    let (off, off_events) = run_short(&config_toml("")).await;
    let (on, on_events) = run_short(&config_toml(
        "[compaction]\nenabled = true\nkeep_recent_tokens = 1\nmemo_token_estimate = 1\n",
    ))
    .await;

    assert_eq!(on.turns(), 1, "没有摘要请求，也没有重新装配");
    assert_eq!(on.turns(), off.turns());
    assert_eq!(
        fed(&on.inputs.lock().unwrap()),
        fed(&off.inputs.lock().unwrap()),
        "模型拿到的轮输入逐字相同"
    );
    let first = |llm: &FakeLlm| -> TurnRequest { llm.requests.lock().unwrap()[0].clone() };
    assert_eq!(
        serde_json::to_string(&first(&on).messages).unwrap(),
        serde_json::to_string(&first(&off).messages).unwrap()
    );
    assert_eq!(first(&on).tools, first(&off).tools);

    assert!(compactions(&off_events).is_empty());
    assert!(
        compactions(&on_events).is_empty(),
        "{:?}",
        compactions(&on_events)
    );
}

/// 一次请求的估价：与 Gateway 判断时同一把尺子。
fn priced(request: &TurnRequest) -> u64 {
    let context = komo_agent::context::AgentContext {
        system_prompt: request.system_prompt.clone(),
        messages: request.messages.clone(),
        run_from: 0,
        decay_candidates: Vec::new(),
    };
    price(&context, &request.tools).write_tokens
}

/// 每次 `read` 交回约 24 KiB（600 行），50 KiB 预览能完整容纳。
const READ_TOKENS: u64 = 6_050;
/// 空会话第一次请求（系统提示 + 工具表 + 任务）的估价，约数：窗口按它定，差不到半次
/// `read` 就不影响哪一轮过线。
const BASE_TOKENS: u64 = 2_740;
const READS: u32 = 5;
const RESERVE: u64 = 1_000;

/// 窗口很小：第五次大 `read` 之后估计过线，压；摘要请求没有工具；同一个 Run 接着跑，下一次主请求是任务 → 摘要 → 切点起原样；停在一条介入上
/// （测试工具 `unsure`）、重启再答，那一段逐字节不变。
#[tokio::test]
async fn window_protection_compacts_and_the_run_goes_on_with_task_summary_tail() {
    let window = BASE_TOKENS + READ_TOKENS * (2 * READS as u64 - 1) / 2 + RESERVE;
    let config = config_toml(&format!(
        "[execution]\nmodel_result_bytes = 65536\ndecay_threshold_bytes = 0\n\
         [compaction]\nenabled = true\nwindow_reserve_tokens = {RESERVE}\n\
         keep_recent_tokens = 1\nmemo_token_estimate = 50\n"
    ))
    .replace(
        "model = \"gpt-test\"\n",
        &format!("model = \"gpt-test\"\ncontext_window = {window}\n"),
    );
    let home = Home::with_config(&config);
    home.install_tools(vec![Unsure::new().tool()]);
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    for n in 1..=READS {
        let lines: String = (0..600)
            .map(|i| format!("{n}-{i:04} {}\n", "x".repeat(32)))
            .collect();
        std::fs::write(home.workspace().join(format!("big{n}.txt")), lines).expect("写文件");
    }
    let read = |n: u32| serde_json::json!({ "path": format!("big{n}.txt") });
    let summary = "摘要：读过 big1–big4，内容是编号行。";
    let mut first = Vec::new();
    for n in 1..=READS {
        first.push(call_round(n, &format!("pc-r{n}"), "read", read(n)));
    }
    first.push(text_round(9, "不该走到这一轮：上面那一轮之后就该压了"));
    let llm = FakeLlm::new(vec![
        first,
        vec![text_round(1, summary)],
        vec![call_round(5, "pc-s1", "unsure", serde_json::json!({}))],
    ]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;
    let task = "把五个大文件读一遍，最后跑一条命令";
    let run = gateway.submit(&session, "window-1", task).await.run;
    let pending = gateway.wait_intervention(&run).await;

    let requests: Vec<TurnRequest> = llm.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3, "主请求、摘要请求、重新装配后的主请求");
    let base = priced(&requests[0]);
    assert!(
        base.abs_diff(BASE_TOKENS) < READ_TOKENS / 3,
        "空会话的估价漂得太远（{base}），调一下 BASE_TOKENS"
    );

    // 摘要请求：没有工具、一条纯文本。
    let summary_request = &requests[1];
    assert!(summary_request.tools.is_empty());
    assert_eq!(summary_request.messages.len(), 1);
    let text = summary_request.messages[0].text.as_deref().unwrap();
    assert!(text.contains(task));
    assert!(
        text.contains("1-0000") && text.contains("4-0000"),
        "前四次 read 收进了摘要"
    );
    assert!(!text.contains("5-0000"), "切点之后的那一轮不收");

    let events = home.events(&session);
    let recorded = compactions(&events);
    let [
        ContextCompacted::Compacted {
            first_kept,
            decision,
            summary: Some(written),
            ..
        },
    ] = recorded.as_slice()
    else {
        panic!("{recorded:?}");
    };
    assert_eq!(decision.reason, CompactionReason::WindowProtection);
    assert_eq!(written, summary);
    let rounds: Vec<_> = events
        .iter()
        .filter(|event| {
            event.run.as_ref() == Some(&run)
                && matches!(event.payload, EventPayload::MessageAssistant(_))
        })
        .map(|event| event.seq)
        .collect();
    assert_eq!(
        *first_kept,
        rounds[READS as usize - 1],
        "切在最后一次 read 那一轮开头"
    );
    let started = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::RunStarted(_)))
        .count();
    assert_eq!(started, 1, "同一个 Run 接着跑，没有重新领取");

    // 重新装配后的主请求：任务 → 摘要 → 切点起原样。
    let after = &requests[2];
    let shape: Vec<(Role, Option<String>, Vec<String>)> = after
        .messages
        .iter()
        .map(|message| {
            let ids = message
                .tool_calls
                .iter()
                .map(|call| call.provider_call_id.clone())
                .chain(
                    message
                        .tool_results
                        .iter()
                        .map(|r| r.provider_call_id.clone()),
                )
                .collect();
            (message.role, message.text.clone(), ids)
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (Role::User, Some(task.into()), vec![]),
            (
                Role::User,
                Some(format!("{COMPACTION_PREFIX}\n\n{summary}")),
                vec![]
            ),
            (Role::Assistant, None, vec![format!("pc-r{READS}")]),
            (Role::Tool, None, vec![format!("pc-r{READS}")]),
        ]
    );
    assert_eq!(after.tools, requests[0].tools, "工具表不变");
    assert_eq!(
        after.system_prompt, requests[0].system_prompt,
        "系统提示不变"
    );
    assert!(priced(after) < window - RESERVE, "压完离开了窗口");

    // 重启 → 答 → 下一段：同一段前缀逐字节不变。
    gateway.stop().await;
    let llm = FakeLlm::finisher("都做完了。");
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    gateway.answer(&pending.handle, "satisfied").await;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let replayed = llm.requests.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        replayed.messages.len(),
        after.messages.len() + 2,
        "多了 s1 那一轮与它核对后的结果"
    );
    assert_eq!(
        serde_json::to_string(&replayed.messages[..after.messages.len()]).unwrap(),
        serde_json::to_string(&after.messages).unwrap(),
        "重启之后回放的那一段逐字节相同"
    );
    assert_eq!(
        compactions(&home.events(&session)).len(),
        1,
        "压过之后没有再压"
    );
    gateway.stop().await;
}

// ------------------------------------------------------------------ 溢出恢复

fn too_long() -> Result<komo_kernel::types::turn::Round, komo_kernel::types::turn::LlmError> {
    Err(komo_kernel::types::turn::LlmError::Rejected {
        status: 400,
        message: "prompt is too long: 213462 tokens > 200000 maximum".into(),
    })
}

/// 压缩关着、两次大 `read` 之后 provider 说超长：起一个 Gateway，跑到终态，交回日志。
async fn run_into_overflow(
    after_summary: Vec<Result<komo_kernel::types::turn::Round, komo_kernel::types::turn::LlmError>>,
) -> (Arc<FakeLlm>, RunState, Vec<Event>) {
    let home = Home::with_config(&config_toml(
        "[execution]\nmodel_result_bytes = 65536\n\
         [compaction]\nenabled = false\nkeep_recent_tokens = 1\nmemo_token_estimate = 50\n",
    ));
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    for n in 1..=2 {
        let lines: String = (0..600)
            .map(|i| format!("{n}-{i:04} {}\n", "x".repeat(32)))
            .collect();
        std::fs::write(home.workspace().join(format!("big{n}.txt")), lines).expect("写文件");
    }
    let read = |n: u32| serde_json::json!({ "path": format!("big{n}.txt") });
    let llm = FakeLlm::new(vec![
        vec![
            call_round(1, "pc-r1", "read", read(1)),
            call_round(2, "pc-r2", "read", read(2)),
            too_long(),
        ],
        vec![text_round(1, "摘要：读过 big1，内容是编号行。")],
        after_summary,
    ]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "overflow-1", "把两个大文件读一遍")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    let events = home.events(&session);
    gateway.stop().await;
    (llm, detail.summary.state, events)
}

/// 压缩关着也救：超长 → 强制压一次（记成窗口保护、标明来自溢出）→ 重新装配接着跑完。
#[tokio::test]
async fn an_overflow_is_compacted_once_even_with_compaction_off_and_the_run_completes() {
    let (llm, state, events) = run_into_overflow(vec![text_round(3, "都读完了。")]).await;
    assert_eq!(state, RunState::Completed);

    let requests: Vec<TurnRequest> = llm.requests.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        3,
        "超长的主请求、摘要请求、重新装配后的主请求"
    );
    assert!(requests[1].tools.is_empty());
    let text = requests[1].messages[0].text.as_deref().unwrap();
    assert!(text.contains("1-0000"), "第一次 read 收进了摘要");
    assert!(!text.contains("2-0000"), "最后一轮留着");
    let after = serde_json::to_string(&requests[2].messages).unwrap();
    assert!(after.contains(COMPACTION_PREFIX), "带着摘要重新装配");
    assert!(
        after.contains("pc-r2") && !after.contains("pc-r1"),
        "{after}"
    );

    let recorded = compactions(&events);
    let [ContextCompacted::Compacted { decision, .. }] = recorded.as_slice() else {
        panic!("{recorded:?}");
    };
    assert_eq!(decision.reason, CompactionReason::WindowProtection);
    assert!(decision.compact && decision.context_overflow);
}

/// 只救一次：刚压过、一次成功的请求都没有就又超长，Run 按原错误失败，不再压。
#[tokio::test]
async fn an_overflow_right_after_the_overflow_compaction_fails_the_run() {
    let (llm, state, events) = run_into_overflow(vec![too_long()]).await;
    assert_eq!(state, RunState::Failed);
    assert_eq!(llm.requests.lock().unwrap().len(), 3, "没有第二次摘要请求");
    assert!(
        matches!(
            compactions(&events).as_slice(),
            [ContextCompacted::Compacted { .. }]
        ),
        "{:?}",
        compactions(&events)
    );
}
