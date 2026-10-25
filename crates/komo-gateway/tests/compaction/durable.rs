//! §6 的在线压缩，Durable 存储版（`docs/storage-durable-d5.md` W4b；与 `../main.rs` 同名的 5 个）。
//!
//! 压缩现在是 harness 的 `komo.compaction` 任务（D4c）：钩子 `compaction` 交回 `[compaction]` 的旋钮
//! 与摘要提示，runtime 在请求前按缓存账决定压不压（阻塞压缩：摘要请求走同一个 `ModelStream`，结果是
//! 一条 `komo.compaction` entry，之后的上下文 = 摘要 + 切点起的原样轮次）；provider 报超长时压缩关着也
//! 救一次（溢出救援，记成窗口保护、`context_overflow = true`）。
//!
//! 与 Legacy 不同的断言与理由：
//!
//! - **事件**：`context.compacted` 来自合成事件流（`events`，由 `komo.compaction` entry 的 `data`
//!   投影），不再读 JSONL；`first_kept` 是切点（head）那条 entry 的 seq，同样等于那一轮 `MessageAssistant`
//!   的 seq；
//! - **"停在介入上、重启再答、回放逐字节不变"**：新存储没有介入状态。回放逐字节相同的论点换成"压缩之后
//!   接着往下说、重启 Gateway，再起的新一轮请求的前缀逐字节等于压缩后那次请求的消息"（`komo.compaction` 与
//!   之后的 entry 都是只追加的存储事实，回放读的就是它们）；
//! - **窗口常量**：估价尺子与 Legacy 是同一把（`price`），`BASE_TOKENS` 只用来确认空会话的估价没漂远；
//! - **同一个 Run 接着跑**：Legacy 数 `run.started` 事件只有一个；新存储里一条输入 submission 就是一个 Run，
//!   断言改成"只有一条输入 submission、`RunStarted` 一次"。

use std::sync::Arc;

use komo_gateway::service::test_support::harness::{
    FakeLlm, Home, call_round, config_toml, text_round,
};
use komo_kernel::compaction::CompactionReason;
use komo_kernel::durable::compaction::wrap_summary;
use komo_kernel::events::{ContextCompacted, Event, EventPayload};
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::{LlmError, Role, Round, TurnRequest};

use super::{compactions, fed, priced, short_script};

async fn run_short(config: &str) -> (Arc<FakeLlm>, Vec<Event>) {
    let home = Home::with_config(config);
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    let llm = FakeLlm::new(short_script());
    let gateway = home.start(Arc::clone(&llm)).await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "short-1", "跑两条命令").await.run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let events = gateway.events(&session).await;
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

/// 每次 `read` 交回约 24 KiB（600 行），50 KiB 预览能完整容纳。
const READ_TOKENS: u64 = 6_050;
/// 空会话第一次请求（系统提示 + 工具表 + 任务）的估价，约数：窗口按它定，差不到半次
/// `read` 就不影响哪一轮过线。
const BASE_TOKENS: u64 = 2_740;
const READS: u32 = 5;
const RESERVE: u64 = 1_000;

/// 窗口很小：第五次大 `read` 之后估计过线，压；摘要请求没有工具；同一个 Run 接着跑，下一次主请求是
/// 摘要 → 切点起原样；重启之后再起的一轮，前缀逐字节不变。
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
        // 压完之后的主请求：收尾。
        vec![text_round(1, "都做完了。")],
    ]);
    let gateway = home.start(Arc::clone(&llm)).await;
    let session = gateway.open_session().await;
    let task = "把五个大文件读一遍，最后跑一条命令";
    let run = gateway.submit(&session, "window-1", task).await.run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

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

    let events = gateway.events(&session).await;
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

    // 重新装配后的主请求：摘要 → 切点起原样。
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
            // §8.11：压缩与 pi 一致，摘要取代切点之前的一切，任务原文随摘要请求一起被概括。摘要消息的包装是
            // pi 的 `SUMMARY_PREFIX` / `SUMMARY_SUFFIX`（Legacy 是 komo 自己的 `COMPACTION_PREFIX`）。
            (Role::User, Some(wrap_summary(summary)), vec![]),
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

    // 重启 → 同一个会话再说一句：新一轮请求的前缀逐字节等于压缩后那次请求的消息。
    gateway.stop().await;
    let llm = FakeLlm::finisher("好。");
    let gateway = home.start(Arc::clone(&llm)).await;
    let run = gateway.submit(&session, "window-2", "再说一句").await.run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let replayed = llm.requests.lock().unwrap().last().cloned().unwrap();
    assert!(
        replayed.messages.len() > after.messages.len(),
        "多了上一轮的收尾与这一句"
    );
    assert_eq!(
        serde_json::to_string(&replayed.messages[..after.messages.len()]).unwrap(),
        serde_json::to_string(&after.messages).unwrap(),
        "重启之后回放的那一段逐字节相同"
    );
    assert_eq!(
        compactions(&gateway.events(&session).await).len(),
        1,
        "压过之后没有再压"
    );
    gateway.stop().await;
}

// ------------------------------------------------------------------ 溢出恢复

fn too_long() -> Result<Round, LlmError> {
    Err(LlmError::Rejected {
        status: 400,
        message: "prompt is too long: 213462 tokens > 200000 maximum".into(),
    })
}

/// 压缩关着、两次大 `read` 之后 provider 说超长：起一个 Gateway，跑到终态，交回日志。
async fn run_into_overflow(
    after_summary: Vec<Result<Round, LlmError>>,
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
    let gateway = home.start(Arc::clone(&llm)).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "overflow-1", "把两个大文件读一遍")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    let events = gateway.events(&session).await;
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
    assert!(
        after.contains("compacted into the following summary"),
        "带着摘要重新装配"
    );
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
