//! 压缩（D4c）：阻塞与后台两种 `komo.compaction` 任务、衰减写成 `edits`、上下文超长时压缩重试。
//! 对应 pi 的 `harness-compaction` / `harness-generation` 里的 threshold 与 overflow 用例，
//! 旧 `tests/compaction`（gateway）的场景改写到新 harness 上：窗口保护压缩、不贴窗口不压、
//! 溢出救一次、救完又超长就失败。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::compaction::CompactionEconomics;
use komo_kernel::durable::Storage;
use komo_kernel::durable::compaction::{COMPACTION_ENTRY_KIND, DECAY_ENTRY_KIND, wrap_summary};
use komo_kernel::durable::{
    AssistantMessage, InputState, Message, StopReason, SubmissionKind, TaskOutcome, TaskState,
    WriteState,
};
use komo_kernel::projection::{DEFAULT_MODEL_RESULT_BYTES, DecayPolicy, ProjectionContext};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::turn::LlmError;
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::super::hooks::HookError;
use super::super::{COMPACTION_KIND, CompactionSettings, GENERATION_KIND, SummaryPrompt};
use super::support::*;

// ------------------------------------------------------------------ 脚手架

/// 回复的时间戳取世界时钟的"现在"：缓存冷不冷看的是上一条回复离现在多久。
fn stamped(world: &World, mut message: AssistantMessage) -> AssistantMessage {
    message.timestamp = world.clock.now_ms();
    message
}

fn say(world: &World, text: &str) -> Step {
    Step::Reply(stamped(world, text_reply(text)))
}

fn call(world: &World, id: &str, tool: &str) -> Step {
    Step::Reply(stamped(world, call_reply(&[(id, tool, json!({}))])))
}

fn too_long() -> Step {
    Step::Error(LlmError::Rejected {
        status: 400,
        message: "prompt is too long: 213462 tokens > 200000 maximum".into(),
    })
}

fn settings(keep_recent: u64, reserve: u64, memo: u64) -> CompactionSettings {
    CompactionSettings {
        economics: CompactionEconomics {
            window_reserve_tokens: reserve,
            ..CompactionEconomics::default()
        },
        cache_write_read_ratio: Some(12.5),
        keep_recent_tokens: keep_recent,
        memo_tokens: memo,
        max_context_tokens: 10_000_000,
        background_tokens: 0,
    }
}

/// 模型窗口 `window`：阻塞阈值 = `window - reserve`。
fn windowed(world: &World, window: u64) {
    world.hooks.set_model(Some(ModelConfig {
        context_window: Some(window),
        ..model_config()
    }));
}

/// 两个只读工具，输出各 `bytes` 字节（约 `bytes / 4` token），内容不同，好认摘要收了哪一个。
fn readers(world: &World, bytes: usize) -> (Arc<TestTool>, Arc<TestTool>) {
    let read = Arc::new(TestTool::reader("read", &world.log).with_text(&"A".repeat(bytes)));
    let rg = Arc::new(TestTool::reader("rg", &world.log).with_text(&"B".repeat(bytes)));
    world.tools(vec![read.clone(), rg.clone()]);
    (read, rg)
}

fn input_state(record: &komo_kernel::durable::SubmissionRecord) -> InputState {
    match &record.kind {
        SubmissionKind::Input { state } => state.clone(),
        other => panic!("an input submission expected, got {other:?}"),
    }
}

fn unanswered(record: &komo_kernel::durable::SubmissionRecord) -> (String, String) {
    match input_state(record) {
        InputState::Unanswered { reason, detail, .. } => (
            reason,
            detail
                .map(|d| d.as_str().map_or_else(|| d.to_string(), str::to_owned))
                .unwrap_or_default(),
        ),
        other => panic!("unanswered expected, got {other:?}"),
    }
}

fn outcome(task: &komo_kernel::durable::TaskRecord) -> &TaskOutcome {
    match &task.state {
        TaskState::Terminal { outcome } => outcome,
        other => panic!("terminal expected, got {other:?}"),
    }
}

fn text_of_entry(entry: &komo_kernel::durable::EntryRecord) -> String {
    text_of(&entry.model.as_ref().expect("a model message")[0])
}

fn system_text(message: &Message) -> String {
    match message {
        Message::System(system) => match &system.content {
            komo_kernel::durable::Content::Text(text) => text.clone(),
            other => panic!("text expected, got {other:?}"),
        },
        other => panic!("system expected, got {other:?}"),
    }
}

fn json_of(messages: &[Message]) -> String {
    serde_json::to_string(messages).unwrap()
}

fn data_of(entry: &komo_kernel::durable::EntryRecord) -> &Value {
    entry.data.as_ref().expect("entry data")
}

// ------------------------------------------------------------------ 阻塞压缩

/// 窗口保护（旧 gateway 的 `window_protection_compacts_and_the_run_goes_on_with_task_summary_tail`）；
/// pi spec §8.3 的"阻塞阈值"：生成建一个自己拥有的压缩并 `waiting`，压缩完回到 `prepare`
/// 接着这一轮——这一轮的请求在摘要之后才发。
#[tokio::test(start_paused = true)]
async fn a_blocking_compaction_holds_the_generation_and_the_turn_goes_on_with_the_summary() {
    let world = World::new();
    readers(&world, 2000);
    // 一次 2000 字节的结果约 520 token：两次之后约 1130，越过 1100 - 100。
    windowed(&world, 1100);
    world.hooks.set_compaction(true, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(call(&world, "c2", "rg"));
    let gate = Arc::new(Notify::new());
    world.script.push_summary(Step::Gate(
        gate.clone(),
        Box::new(Step::Reply(text_reply("SUMMARY OF THE EARLY WORK"))),
    ));
    world.script.push(say(&world, "all done"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;

    // 摘要请求在飞：生成 waiting，run 还在，主请求一个都没多发。
    world
        .until(|| async { world.script.summary_requests().len() == 1 })
        .await;
    let live = world.live().await;
    let status = &live.compactions.as_ref().expect("a compaction status")[0];
    assert!(status.blocking, "{status:?}");
    assert_eq!(status.reason, "threshold");
    assert!(live.run.is_some());
    let generation = world.tasks(GENERATION_KIND).await.pop().unwrap();
    let TaskState::Waiting { checkpoint, on, .. } = &generation.state else {
        panic!("the generation should be waiting: {:?}", generation.state);
    };
    assert_eq!(checkpoint["phase"], "prepare");
    assert_eq!(checkpoint["threshold"], true);
    assert_eq!(on.len(), 1);
    assert_eq!(world.script.request_count(), 2, "no request while it waits");
    let compaction = world.tasks(COMPACTION_KIND).await.pop().unwrap();
    assert_eq!(compaction.owner, Some(generation.id));
    assert!(!compaction.background);

    gate.notify_one();
    let settled = world.harness.wait(&engine, id).await.unwrap();
    assert!(matches!(input_state(&settled), InputState::Done { .. }));

    // 摘要请求：两条消息，没有工具；摘要收了第一次 read，没收最后一轮的 rg。
    let summary = &world.script.summary_requests()[0];
    assert_eq!(summary.len(), 2);
    assert_eq!(system_text(&summary[0]), SummaryPrompt::default().system);
    let transcript = text_of(&summary[1]);
    assert!(transcript.contains("[User]: go"), "{transcript}");
    assert!(
        transcript.contains("AAAA") && !transcript.contains("BBBB"),
        "{transcript}"
    );
    assert!(
        transcript.contains("[Assistant tool calls]: read()"),
        "{transcript}"
    );

    // 摘要 entry：head 是第一条保留的 assistant，data 记着理由、债与决策。
    let entries = world.entries().await;
    let summaries = world.entries_of(COMPACTION_ENTRY_KIND).await;
    assert_eq!(summaries.len(), 1);
    let kept = entries
        .iter()
        .find(|entry| Some(entry.id) == summaries[0].head)
        .expect("head points at an entry");
    assert_eq!(
        outline(kept.model.as_ref().unwrap()),
        ["assistant calls rg(c2)"]
    );
    let data = data_of(&summaries[0]);
    assert_eq!(data["reason"], "threshold");
    assert_eq!(data["decision"]["reason"], "window_protection");
    assert!(data["debt"]["repayment_tokens"].as_u64().unwrap() > 0);

    // 摘要之后的那次请求：基线 system、摘要、从切点起原样。
    let requests = world.script.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        outline(&requests[2]),
        [
            "system".to_owned(),
            format!("user: {}", wrap_summary("SUMMARY OF THE EARLY WORK")),
            "assistant calls rg(c2)".to_owned(),
            "result(c2)".to_owned(),
        ]
    );
    // 压缩任务以 entryId 完成；状态清掉了，没有失败记录。
    let compaction = world.tasks(COMPACTION_KIND).await.pop().unwrap();
    let TaskOutcome::Completed { result } = outcome(&compaction) else {
        panic!("completed expected: {:?}", compaction.state);
    };
    assert_eq!(result["entryId"], json!(summaries[0].id));
    let live = world.live().await;
    assert!(live.compactions.is_none() && live.compaction_failure.is_none());
    engine.close().await.unwrap();
}

/// 旧 `a_short_task_is_never_compacted…`：开着压缩、不贴窗口——什么都不建、不多发请求。
#[tokio::test(start_paused = true)]
async fn a_short_task_is_never_compacted_even_with_compaction_on() {
    let world = World::new();
    readers(&world, 400);
    windowed(&world, 100_000);
    world.hooks.set_compaction(true, settings(1, 1_000, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(say(&world, "done"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    assert!(world.tasks(COMPACTION_KIND).await.is_empty());
    assert!(world.entries_of(COMPACTION_ENTRY_KIND).await.is_empty());
    assert!(world.entries_of(DECAY_ENTRY_KIND).await.is_empty());
    assert_eq!(world.script.request_count(), 2);
    assert!(world.script.summary_requests().is_empty());
    engine.close().await.unwrap();
}

/// pi spec §8.3："a compaction that failed ... the generation fails with `model_error` without a
/// request"：越过阻塞阈值而压缩失败，输入以 `model_error` 收场，不发请求；失败记在
/// `komo.live.compactionFailure`。
#[tokio::test(start_paused = true)]
async fn a_failed_blocking_compaction_fails_the_run_without_a_request() {
    let world = World::new();
    readers(&world, 2000);
    windowed(&world, 1100);
    world.hooks.set_compaction(true, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(call(&world, "c2", "rg"));
    world.script.push_summary(Step::Error(rejected(400)));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();
    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "model_error");
    assert!(
        detail.starts_with("Compaction failed: Summarization failed"),
        "{detail}"
    );
    assert_eq!(
        world.script.request_count(),
        2,
        "no request after the failed compaction"
    );
    let live = world.live().await;
    let failure = live.compaction_failure.expect("the failure is recorded");
    assert_eq!(failure.reason, "threshold");
    assert!(failure.error.contains("Summarization failed"));
    assert!(live.compactions.is_none() && live.run.is_none());
    let compaction = world.tasks(COMPACTION_KIND).await.pop().unwrap();
    assert!(matches!(outcome(&compaction), TaskOutcome::Failed { .. }));
    engine.close().await.unwrap();
}

/// pi §8.7 summarize：可重试的错误按有界退避再来一次，重发的是同一个钉住的请求。
#[tokio::test(start_paused = true)]
async fn a_retryable_summary_error_backs_off_and_resends_the_same_request() {
    let world = World::new();
    readers(&world, 2000);
    windowed(&world, 1100);
    world.hooks.set_compaction(true, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(call(&world, "c2", "rg"));
    world.script.push_summary(Step::Error(rejected(503)));
    world.script.push_summary(Step::Reply(text_reply("S")));
    world.script.push(say(&world, "done"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world
        .until(|| async {
            world
                .live()
                .await
                .compactions
                .is_some_and(|c| c[0].retry.is_some())
        })
        .await;
    let status = world.live().await.compactions.unwrap().remove(0);
    assert_eq!(status.attempt, 1);
    let retry = status.retry.unwrap();
    assert!(retry.error.contains("503"), "{retry:?}");
    world.harness.wait(&engine, id).await.unwrap();
    let requests = world.script.summary_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(json_of(&requests[0]), json_of(&requests[1]));
    assert_eq!(world.entries_of(COMPACTION_ENTRY_KIND).await.len(), 1);
    engine.close().await.unwrap();
}

// ------------------------------------------------------------------ 后台压缩

/// pi §8.3：到了后台阈值，同一次提交里建一个 conversation 所有的后台压缩，不等它——这一轮
/// 与整个 run 照常跑完；摘要之后在空闲的 conversation 里直接落（写 submission `compaction:<id>`），
/// 下一条输入从摘要起算。
#[tokio::test(start_paused = true)]
async fn a_background_compaction_does_not_block_the_turn_and_lands_when_idle() {
    let world = World::new();
    readers(&world, 2200);
    // 阻塞阈值 1000；后台阈值 1000 - 400 = 600。一次 2200 字节的结果后约 700：只到后台线。
    windowed(&world, 1100);
    let mut with_background = settings(1, 100, 50);
    with_background.background_tokens = 400;
    world.hooks.set_compaction(true, with_background);
    world.script.push(call(&world, "c1", "read"));
    world.script.push(say(&world, "ok"));
    let gate = Arc::new(Notify::new());
    world.script.push_summary(Step::Gate(
        gate.clone(),
        Box::new(Step::Reply(text_reply("BACKGROUND SUMMARY"))),
    ));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;

    // run 在摘要还没回来的时候就答完了。
    let settled = world.harness.wait(&engine, id).await.unwrap();
    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    let live = world.live().await;
    assert!(live.run.is_none());
    let status = &live.compactions.as_ref().expect("still summarizing")[0];
    assert!(!status.blocking, "{status:?}");
    let compaction = world.tasks(COMPACTION_KIND).await.pop().unwrap();
    assert!(compaction.background && compaction.owner.is_none());
    assert_eq!(world.script.request_count(), 2, "the turn never waited");
    assert!(world.entries_of(COMPACTION_ENTRY_KIND).await.is_empty());

    gate.notify_one();
    world
        .until(|| async { !world.entries_of(COMPACTION_ENTRY_KIND).await.is_empty() })
        .await;
    let summaries = world.entries_of(COMPACTION_ENTRY_KIND).await;
    assert_eq!(data_of(&summaries[0])["reason"], "threshold");
    let placed = world
        .storage
        .submission_by_request(CONV, &format!("compaction:{}", compaction.id))
        .await
        .unwrap()
        .expect("the summary was admitted as a write submission");
    assert_eq!(
        placed.kind,
        SubmissionKind::Write {
            state: WriteState::Done {
                entry: summaries[0].id
            }
        }
    );
    world
        .until(|| async { world.live().await.compactions.is_none() })
        .await;

    // 下一条输入：从摘要起算。后台压缩在 `prepare` 那一刻选的切点：当时的上下文只到 c1 的
    // 结果，所以留下的是最后一轮（c1 的调用起），摘要收的只是它之前的输入与系统段。
    world.script.push(say(&world, "again"));
    let id = world.submit(&engine, "next").await;
    world.harness.wait(&engine, id).await.unwrap();
    let requests = world.script.requests();
    assert_eq!(
        outline(requests.last().unwrap()),
        [
            "system".to_owned(),
            format!("user: {}", wrap_summary("BACKGROUND SUMMARY")),
            "assistant calls read(c1)".to_owned(),
            "result(c1)".to_owned(),
            "assistant: ok".to_owned(),
            "user: next".to_owned(),
        ]
    );
    engine.close().await.unwrap();
}

/// `Harness::compact`（pi `Conversation.compact()`）：手动压缩不是 background；摘要在空闲的
/// conversation 里直接落，data 记着 `manual`。
#[tokio::test(start_paused = true)]
async fn a_manual_compaction_summarizes_the_history_and_lands_as_a_write() {
    let world = World::new();
    readers(&world, 400);
    world.hooks.set_compaction(false, settings(1, 100, 10));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(say(&world, "first answer"));
    world
        .script
        .push_summary(Step::Reply(text_reply("MANUAL SUMMARY")));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let task = world
        .harness
        .compact(&engine, CONV, Some("focus on files".into()))
        .await
        .unwrap();
    world
        .until(|| async {
            world
                .tasks(COMPACTION_KIND)
                .await
                .iter()
                .any(|t| t.id == task && matches!(t.state, TaskState::Terminal { .. }))
        })
        .await;
    let record = world.tasks(COMPACTION_KIND).await.pop().unwrap();
    assert!(!record.background, "a manual compaction is ordinary work");
    let summaries = world.entries_of(COMPACTION_ENTRY_KIND).await;
    assert_eq!(summaries.len(), 1);
    assert_eq!(data_of(&summaries[0])["reason"], "manual");
    assert_eq!(text_of_entry(&summaries[0]), wrap_summary("MANUAL SUMMARY"));
    let request = text_of(&world.script.summary_requests()[0][1]);
    assert!(
        request.ends_with("Additional focus: focus on files"),
        "{request}"
    );
    engine.close().await.unwrap();
}

// ------------------------------------------------------------------ 衰减

fn decaying_projection() -> ProjectionContext {
    ProjectionContext {
        model_result_bytes: DEFAULT_MODEL_RESULT_BYTES,
        decay: Some(DecayPolicy {
            threshold_bytes: 500,
            full_sends: 1,
            head_bytes: 100,
            tail_bytes: 100,
        }),
    }
}

/// 60 行 × 60 字节：投影超过阈值，decayed 视图（首尾整行）比 full 短得多。
fn long_output() -> String {
    (0..60)
        .map(|n| format!("line {n:03} {}\n", "x".repeat(50)))
        .collect()
}

fn decay_world() -> World {
    let world = World::new();
    world.hooks.set_projection(decaying_projection());
    let read = Arc::new(TestTool::reader("read", &world.log).with_text(&long_output()));
    let rg = Arc::new(TestTool::reader("rg", &world.log).with_text(&long_output()));
    world.tools(vec![read, rg]);
    world
}

fn result_text(message: &Message) -> Option<String> {
    matches!(message, Message::ToolResult(_)).then(|| text_of(message))
}

/// 设计 §4.4：贴着窗口时先换短视图——一条只带 `edits` 的 write entry，每个目标 `replace` 成
/// `data.decayed`；推导按 edits 选视图，所以"刚跑完"与"重启之后"发出去的逐字节相同；一批
/// 只失效一次。压缩关着也换（衰减不受 `enabled` 管）。
#[tokio::test(start_paused = true)]
async fn decay_is_one_edits_entry_and_the_short_view_is_identical_after_a_restart() {
    let world = decay_world();
    // 一次约 900 token：两次之后约 1900，越过 1500 - 100。
    windowed(&world, 1500);
    world.hooks.set_compaction(false, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(call(&world, "c2", "rg"));
    world.script.push(say(&world, "done"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let entries = world.entries().await;
    let first_result = entries
        .iter()
        .find(|entry| entry.kind == "komo.tool-result")
        .unwrap();
    let decayed = data_of(first_result)["decayed"]
        .as_str()
        .unwrap()
        .to_owned();
    let full = text_of_entry(first_result);
    assert!(decayed.len() + 1024 <= full.len(), "a real saving");

    let decays = world.entries_of(DECAY_ENTRY_KIND).await;
    assert_eq!(decays.len(), 1, "one batch, one invalidation");
    assert!(decays[0].model.is_none() && decays[0].head.is_none());
    assert_eq!(
        decays[0].edits.len(),
        1,
        "only c1 had been sent enough times"
    );
    assert_eq!(decays[0].edits[0].target, first_result.id);
    assert_eq!(data_of(&decays[0])["decayed"], json!([first_result.id]));
    assert!(data_of(&decays[0])["debt"].is_object());

    // 压缩关着：没有任何摘要请求。
    assert!(world.script.summary_requests().is_empty());
    // 第三次请求带的是短视图；c2（刚给过一次）还是完整的。
    let requests = world.script.requests();
    let results: Vec<String> = requests[2].iter().filter_map(result_text).collect();
    let second_result = entries
        .iter()
        .filter(|entry| entry.kind == "komo.tool-result")
        .nth(1)
        .unwrap();
    assert_eq!(results, [decayed.clone(), text_of_entry(second_result)]);
    assert_ne!(results[0], full);
    // Anthropic 的缓存断点要补在第一条被换的结果之前：请求里它的下标（system、user、调用、结果）。
    assert_eq!(
        world.script.revised_froms(),
        [None, None, Some(3)],
        "{:?}",
        outline(&requests[2])
    );

    // 重启、再来一条输入：同一段前缀逐字节相同，也没有第二次衰减。
    let engine = world.restart(engine).await;
    world.script.push(say(&world, "again"));
    let id = world.submit(&engine, "next").await;
    world.harness.wait(&engine, id).await.unwrap();
    let after = world.script.requests().pop().unwrap();
    assert_eq!(
        json_of(&after[..requests[2].len()]),
        json_of(&requests[2]),
        "replay after a restart is byte-identical"
    );
    assert_eq!(world.entries_of(DECAY_ENTRY_KIND).await.len(), 1);
    engine.close().await.unwrap();
}

/// 缓存已冷：改写不欠债，有资格的全换，不需要贴着窗口。
#[tokio::test(start_paused = true)]
async fn a_cold_cache_decays_every_eligible_result_without_window_pressure() {
    let world = decay_world();
    world.hooks.set_compaction(false, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    // 第二次请求悬着的时候过了 6 分钟：回复时上一条回复写下的缓存早就过期（TTL 5 分钟）。
    let gate = Arc::new(Notify::new());
    world
        .script
        .push(Step::Gate(gate.clone(), Box::new(call(&world, "c2", "rg"))));
    world.script.push(say(&world, "done"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world
        .until(|| async { world.script.request_count() == 2 })
        .await;
    tokio::time::advance(Duration::from_secs(360)).await;
    gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();
    let decays = world.entries_of(DECAY_ENTRY_KIND).await;
    assert_eq!(decays.len(), 1);
    let data = data_of(&decays[0]);
    assert_eq!(data["decision"]["reason"], "cache_cold");
    assert_eq!(
        data["debt"]["debt_tokens"], 0.0,
        "a cold cache owes nothing"
    );
    engine.close().await.unwrap();
}

// ------------------------------------------------------------------ 溢出

/// 旧 gateway `an_overflow_is_compacted_once_even_with_compaction_off_and_the_run_completes`；
/// pi §8.3：超长 → 追加错误 entry、建阻塞压缩、记 `context_overflow`，压完回到 `prepare`
/// 重发——恢复这一次不占重试预算。压缩关着也救。
#[tokio::test(start_paused = true)]
async fn an_overflow_is_compacted_once_and_the_request_is_resent() {
    let world = World::new();
    readers(&world, 2000);
    world.hooks.set_compaction(false, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(call(&world, "c2", "rg"));
    world.script.push(too_long());
    world.script.push(say(&world, "finished"));
    world
        .script
        .push_summary(Step::Reply(text_reply("OVERFLOW SUMMARY")));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();
    assert!(
        matches!(input_state(&settled), InputState::Done { .. }),
        "{settled:?}"
    );

    let requests = world.script.requests();
    assert_eq!(
        requests.len(),
        4,
        "the overflowing request, then the resend"
    );
    assert!(json_of(&requests[3][..2]).contains("OVERFLOW SUMMARY"));
    assert_eq!(
        outline(&requests[3]),
        [
            "system".to_owned(),
            format!("user: {}", wrap_summary("OVERFLOW SUMMARY")),
            "assistant calls rg(c2)".to_owned(),
            "result(c2)".to_owned(),
        ]
    );
    // 摘要只收了更早的部分。
    let transcript = text_of(&world.script.summary_requests()[0][1]);
    assert!(transcript.contains("AAAA") && !transcript.contains("BBBB"));

    let summaries = world.entries_of(COMPACTION_ENTRY_KIND).await;
    assert_eq!(summaries.len(), 1);
    let data = data_of(&summaries[0]);
    assert_eq!(data["reason"], "overflow");
    assert_eq!(data["decision"]["context_overflow"], true);
    assert_eq!(data["decision"]["reason"], "window_protection");
    // 超长的那次请求留下一条失败的 assistant entry，被上下文推导挡在请求之外。
    let errors: Vec<_> = world
        .entries_of("komo.assistant")
        .await
        .into_iter()
        .filter(|entry| matches!(&entry.model.as_ref().unwrap()[0], Message::Assistant(m) if m.stop_reason == StopReason::Error))
        .collect();
    assert_eq!(errors.len(), 1);
    // 没有退避：重发不占重试预算，时钟没有走。
    let times = world.script.request_times();
    assert_eq!(times[2], times[3]);
    engine.close().await.unwrap();
}

/// pi §8.3："Any other overflow, including a second one, is never retried"；旧 gateway
/// `an_overflow_right_after_the_overflow_compaction_fails_the_run`：压完又超长，照旧失败，
/// 不再压第二次、不无限循环。
#[tokio::test(start_paused = true)]
async fn an_overflow_right_after_the_overflow_compaction_fails_the_run() {
    let world = World::new();
    readers(&world, 2000);
    world.hooks.set_compaction(false, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(call(&world, "c2", "rg"));
    world.script.push(too_long());
    world.script.push(too_long());
    world.script.push_summary(Step::Reply(text_reply("S")));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();
    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "model_error");
    assert!(detail.contains("prompt is too long"), "{detail}");
    assert_eq!(
        world.script.summary_requests().len(),
        1,
        "one compaction, never a second"
    );
    assert_eq!(world.script.request_count(), 4);
    assert_eq!(world.entries_of(COMPACTION_ENTRY_KIND).await.len(), 1);
    assert!(world.live().await.run.is_none());
    engine.close().await.unwrap();
}

/// 没有可收的一段（只有一条输入）：救不了，照旧以 `model_error` 失败，不建压缩任务。
#[tokio::test(start_paused = true)]
async fn an_overflow_with_nothing_to_compact_fails_like_before() {
    let world = World::new();
    world.hooks.set_compaction(false, settings(1, 100, 50));
    world.script.push(too_long());
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(unanswered(&settled).0, "model_error");
    assert!(world.tasks(COMPACTION_KIND).await.is_empty());
    engine.close().await.unwrap();
}

/// 超长救不回来：压缩本身失败，run 以原来的超长文字失败（pi："the run fails with model_error
/// and the overflow text as detail"）。
#[tokio::test(start_paused = true)]
async fn an_overflow_whose_compaction_fails_fails_with_the_overflow_text() {
    let world = World::new();
    readers(&world, 2000);
    world.hooks.set_compaction(false, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(call(&world, "c2", "rg"));
    world.script.push(too_long());
    world.script.push_summary(Step::Error(rejected(400)));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();
    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "model_error");
    assert!(detail.contains("prompt is too long"), "{detail}");
    assert_eq!(
        world.live().await.compaction_failure.unwrap().reason,
        "overflow"
    );
    engine.close().await.unwrap();
}

// ------------------------------------------------------------------ 重启

/// 设计 §5：压缩任务中途重启，从 checkpoint 所在阶段继续——重发的是钉住的同一个摘要请求
/// （逐字节相同），之后生成接着这一轮，只有一条摘要 entry。
#[tokio::test(start_paused = true)]
async fn a_restart_during_summarization_resumes_from_the_checkpoint() {
    let world = World::new();
    readers(&world, 2000);
    windowed(&world, 1100);
    world.hooks.set_compaction(true, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(call(&world, "c2", "rg"));
    world.script.push_summary(Step::Hang);
    world
        .script
        .push_summary(Step::Reply(text_reply("RESUMED SUMMARY")));
    world.script.push(say(&world, "done"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world
        .until(|| async { world.script.summary_requests().len() == 1 })
        .await;
    let compaction = world.tasks(COMPACTION_KIND).await.pop().unwrap();
    let TaskState::Running { checkpoint } = &compaction.state else {
        panic!("running expected: {:?}", compaction.state);
    };
    assert_eq!(checkpoint["phase"], "summarize");

    let engine = world.restart(engine).await;
    let settled = world.harness.wait(&engine, id).await.unwrap();
    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    let requests = world.script.summary_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        json_of(&requests[0]),
        json_of(&requests[1]),
        "the same pinned request"
    );
    assert_eq!(world.entries_of(COMPACTION_ENTRY_KIND).await.len(), 1);
    let last = world.script.requests().pop().unwrap();
    assert_eq!(
        text_of(&last[1]),
        wrap_summary("RESUMED SUMMARY"),
        "the turn went on with the summary"
    );
    engine.close().await.unwrap();
}

// ------------------------------------------------------------------ 取消与失败的清理

/// 阻塞压缩在飞时取消 conversation：压缩随生成一起 abort（先于生成自己的 abort handler），
/// 不写 entry，状态移除，输入 `unanswered{aborted}`。
#[tokio::test(start_paused = true)]
async fn aborting_during_a_blocking_compaction_aborts_both_without_a_summary() {
    let world = World::new();
    readers(&world, 2000);
    windowed(&world, 1100);
    world.hooks.set_compaction(true, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(call(&world, "c2", "rg"));
    world.script.push_summary(Step::Hang);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world
        .until(|| async { world.script.summary_requests().len() == 1 })
        .await;
    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    let settled = world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(unanswered(&settled).0, "aborted");
    let compaction = world.tasks(COMPACTION_KIND).await.pop().unwrap();
    assert!(matches!(outcome(&compaction), TaskOutcome::Aborted { .. }));
    assert!(world.entries_of(COMPACTION_ENTRY_KIND).await.is_empty());
    let live = world.live().await;
    assert!(live.compactions.is_none() && live.run.is_none());
    assert!(
        live.compaction_failure.is_none(),
        "an abort is not a failure"
    );
    engine.close().await.unwrap();
}

/// 压缩任务自己出错（钩子失败 → faulted）：调度器写的结局由清理移除状态、记下失败；等它的
/// 生成读到 faulted，以 `model_error` 收场。
#[tokio::test(start_paused = true)]
async fn a_faulted_compaction_is_cleaned_up_and_fails_the_waiting_run() {
    let world = World::new();
    readers(&world, 2000);
    windowed(&world, 1100);
    world.hooks.set_compaction(true, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(call(&world, "c2", "rg"));
    // 生成的三次 `prepare` 各问一次（放过），第四次问的是压缩任务的 `select`：让它失败。
    world
        .hooks
        .fail_compaction_after(3, HookError::Failed("hook exploded".into()));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();
    let (reason, detail) = unanswered(&settled);
    assert_eq!(reason, "model_error");
    assert!(detail.contains("hook exploded"), "{detail}");
    let compaction = world.tasks(COMPACTION_KIND).await.pop().unwrap();
    assert!(matches!(outcome(&compaction), TaskOutcome::Faulted { .. }));
    let live = world.live().await;
    assert!(live.compactions.is_none());
    assert_eq!(live.compaction_failure.unwrap().reason, "threshold");
    engine.close().await.unwrap();
}

// ------------------------------------------------------------------ Turso

/// 同一个超长救援场景跑在 komo-store 的 Turso `Storage` 上（真文件、真 fsync）：提交落了盘，
/// 关库重开之后推导出的上下文与关库之前逐字节相同——摘要 entry 的 `head`、系统基线都在里面。
#[tokio::test]
async fn overflow_recovery_runs_on_the_turso_storage_and_replays_after_a_reopen() {
    use komo_kernel::durable::storage::EntryQuery;
    use komo_kernel::durable::{ScanOrder, Storage, derive_context};

    use crate::durable::{Engine, EngineOptions};

    let dir = tempfile::tempdir().unwrap();
    let world = World::new();
    readers(&world, 2000);
    world.hooks.set_compaction(false, settings(1, 100, 50));
    world.script.push(call(&world, "c1", "read"));
    world.script.push(call(&world, "c2", "rg"));
    world.script.push(too_long());
    world.script.push(say(&world, "finished"));
    world
        .script
        .push_summary(Step::Reply(text_reply("TURSO SUMMARY")));

    let open = |storage: Arc<dyn Storage>| {
        let registry = world.registry.clone();
        let clock = world.clock.clone();
        let hooks = world.harness.hooks();
        async move {
            let engine = Engine::open(
                storage,
                registry,
                clock,
                EngineOptions {
                    close_storage: true,
                    hooks,
                    ..EngineOptions::default()
                },
            )
            .await
            .unwrap();
            engine
                .commit(|tx| {
                    Box::pin(async move {
                        if tx.conversation(CONV).await?.is_none() {
                            tx.create_root_conversation().await?;
                        }
                        Ok(())
                    })
                })
                .await
                .unwrap();
            engine.resume();
            engine
        }
    };
    let entries_of = |storage: Arc<dyn Storage>| async move {
        let page = storage
            .scan_entries(
                &EntryQuery {
                    order: Some(ScanOrder::Ascending),
                    ..EntryQuery::new(CONV)
                },
                1000,
                None,
            )
            .await
            .unwrap();
        page.items
    };

    let storage: Arc<dyn Storage> = Arc::new(
        komo_store::durable::TursoStorage::open(dir.path())
            .await
            .unwrap(),
    );
    let engine = open(storage.clone()).await;
    let id = world.submit(&engine, "go").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();
    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    let before = derive_context(&entries_of(storage.clone()).await, None).messages;
    assert_eq!(world.script.summary_requests().len(), 1);
    engine.close().await.unwrap();

    let reopened: Arc<dyn Storage> = Arc::new(
        komo_store::durable::TursoStorage::open(dir.path())
            .await
            .unwrap(),
    );
    let entries = entries_of(reopened.clone()).await;
    let summaries: Vec<_> = entries
        .iter()
        .filter(|entry| entry.kind == COMPACTION_ENTRY_KIND)
        .collect();
    assert_eq!(summaries.len(), 1);
    assert_eq!(data_of(summaries[0])["decision"]["context_overflow"], true);
    assert_eq!(
        json_of(&derive_context(&entries, None).messages),
        json_of(&before)
    );
    assert_eq!(
        text_of(&before[1]),
        wrap_summary("TURSO SUMMARY"),
        "the summary leads the context after the baseline system message"
    );
    reopened.close().await.unwrap();
}
