//! submission、收件箱与 boundary（设计 §4.3；pi spec §6）。
//! 对应 pi 的 `harness-inbox` / `harness-submissions` / `harness-input-entries`。

use std::sync::Arc;

use komo_kernel::durable::{InputState, SubmissionKind, SubmissionStatus, WriteState};
use serde_json::json;
use tokio::sync::Notify;

use super::super::inbox::{Admitted, InboxItem, WriteHead};
use super::super::{
    Harness, HarnessSettings, HookError, InputEntry, QueueMode, RunBudget, SubmissionDraft,
    SubmitError, WithdrawResult, WriteDraft,
};
use super::support::*;
use crate::durable::tx::ConversationOwnership;

fn input_state(record: &komo_kernel::durable::SubmissionRecord) -> InputState {
    match &record.kind {
        SubmissionKind::Input { state } => state.clone(),
        other => panic!("an input submission expected, got {other:?}"),
    }
}

fn write_state(record: &komo_kernel::durable::SubmissionRecord) -> WriteState {
    match &record.kind {
        SubmissionKind::Write { state } => state.clone(),
        other => panic!("a write submission expected, got {other:?}"),
    }
}

/// 让 conversation 忙起来：第一个工具等 `gate`，起一条输入，等它真的开始跑。
async fn busy_world(
    extra_steps: impl FnOnce(&World),
) -> (World, crate::durable::Engine, Arc<Notify>) {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    let slow = Arc::new(TestTool::writer("slow", &world.log).with_gate(&gate));
    world.tools(vec![slow]);
    world.script.calls(&[("c1", "slow", json!({}))]);
    extra_steps(&world);
    let engine = world.open().await;
    (world, engine, gate)
}

/// pi harness-tools "runs input, tool call, tool result, and answer, and settles the input"；
/// harness-generation "answers an input and settles its submission"。设计 §4.3 第一行：空闲且
/// 收件箱为空，输入直接 `placed` 并起 run。
#[tokio::test(start_paused = true)]
async fn an_idle_input_runs_several_tool_rounds_and_answers() {
    let world = World::new();
    let read = Arc::new(TestTool::reader("read", &world.log));
    let bash = Arc::new(TestTool::writer("bash", &world.log).with_text("built"));
    world.tools(vec![read.clone(), bash.clone()]);
    world
        .script
        .calls(&[("c1", "read", json!({})), ("c2", "bash", json!({}))]);
    world.script.calls(&[("c3", "read", json!({}))]);
    world.script.reply("all done");
    let engine = world.open().await;

    let id = world.submit(&engine, "build it").await;
    let settled = world.harness.wait(&engine, id).await.unwrap();

    let InputState::Done { entry, answer } = input_state(&settled) else {
        panic!("the input should be done: {settled:?}");
    };
    let assistants = world.entries_of("komo.assistant").await;
    assert_eq!(answer, assistants.last().unwrap().id);
    assert_eq!(entry, world.entries_of("komo.user").await[0].id);
    assert_eq!(
        outline(&world.script.requests()[2]),
        [
            "system",
            "user: build it",
            "assistant calls read(c1),bash(c2)",
            "result(c1)",
            "result(c2)",
            "assistant calls read(c3)",
            "result(c3)",
        ]
    );
    // 只读的 read 先收尾，bash 是屏障，之后才轮到它。
    assert_eq!(
        world.log.lines(),
        [
            "prepare read",
            "start read #1",
            "finish read #1",
            "prepare bash",
            "start bash #1",
            "finish bash #1",
            "prepare read",
            "start read #2",
            "finish read #2",
        ]
    );
    let live = world.live().await;
    assert!(live.run.is_none() && live.tools.is_none() && live.generation.is_none());
    for task in world.tasks("komo.tool").await {
        assert!(matches!(
            task.state,
            komo_kernel::durable::TaskState::Terminal { .. }
        ));
    }
    engine.close().await.unwrap();
}

/// pi harness-inbox "adds steers to the run at the postTools boundary and holds follow-ups for the
/// final boundary"、"queues busy submissions and places writes before user items at the final
/// boundary, one follow-up per run"；harness-input-entries "appends a data-only write mid-run in
/// time order, leaving the model's messages unchanged"。
#[tokio::test(start_paused = true)]
async fn a_steer_joins_at_post_tools_and_a_follow_up_runs_after_the_final_answer() {
    let (world, engine, gate) = busy_world(|world| {
        world.script.reply("after the steer");
        world.script.reply("after the follow-up");
    })
    .await;
    let first = world.submit(&engine, "first").await;
    world.until_log("start slow #1").await;

    let steer = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::input("change course").steer(),
        )
        .await
        .unwrap();
    let follow = world.submit(&engine, "and then this").await;
    let note = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::write(WriteDraft::display("komo.label", json!({"label": "x"}))),
        )
        .await
        .unwrap();

    // 展示用的 write 忙时也立刻落，不排队；两条输入排在收件箱里，次序就是 ID 序。
    assert_eq!(
        write_state(&world.submission(note).await),
        WriteState::Done {
            entry: world.entries_of("komo.label").await[0].id
        }
    );
    let inbox = Harness::inbox(&*world.storage, CONV).await.unwrap();
    assert_eq!(
        inbox.items.iter().map(InboxItem::id).collect::<Vec<_>>(),
        [steer, follow]
    );
    assert!(matches!(inbox.items[0], InboxItem::Steer { .. }));
    assert!(matches!(inbox.items[1], InboxItem::FollowUp { .. }));
    assert_eq!(
        input_state(&world.submission(steer).await),
        InputState::Queued
    );

    gate.notify_one();
    world.harness.wait(&engine, follow).await.unwrap();

    assert_eq!(
        outline(&world.script.requests()[1]),
        [
            "system",
            "user: first",
            "assistant calls slow(c1)",
            "result(c1)",
            "user: change course",
        ],
        "the steer lands after the tool results, before the next reply; the label adds nothing"
    );
    assert_eq!(
        outline(&world.script.requests()[2]),
        [
            "system",
            "user: first",
            "assistant calls slow(c1)",
            "result(c1)",
            "user: change course",
            "assistant: after the steer",
            "user: and then this",
        ]
    );
    // steer 并进了当前 run：两条输入用同一个回答 settle；follow-up 是下一条输入，自己的回答。
    let mut seen = Vec::new();
    for id in [first, steer, follow] {
        seen.push(
            input_state(&world.submission(id).await)
                .clone_answer()
                .unwrap(),
        );
    }
    assert_eq!(seen[0], seen[1]);
    assert_ne!(seen[0], seen[2]);
    assert!(
        Harness::inbox(&*world.storage, CONV)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    engine.close().await.unwrap();
}

/// pi harness-submissions "deduplicates request IDs per conversation before any write"。
#[tokio::test(start_paused = true)]
async fn a_repeated_request_id_returns_the_original_submission() {
    let world = World::new();
    world.script.reply("hello");
    let engine = world.open().await;

    let draft = || SubmissionDraft::input("hi").with_request_id("req-1");
    let first = world.harness.submit(&engine, CONV, draft()).await.unwrap();
    let again = world.harness.submit(&engine, CONV, draft()).await.unwrap();
    assert_eq!(first, again);
    world.harness.wait(&engine, first).await.unwrap();
    let after = world.harness.submit(&engine, CONV, draft()).await.unwrap();
    assert_eq!(after, first, "still deduplicated once it has settled");
    assert_eq!(world.entries_of("komo.user").await.len(), 1);
    assert_eq!(world.script.request_count(), 1);

    // 同一个键不能给另一种 submission。
    let clash = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::write(WriteDraft::display("komo.label", json!({})))
                .with_request_id("req-1"),
        )
        .await;
    assert!(matches!(clash, Err(SubmitError::Durable(_))), "{clash:?}");
    engine.close().await.unwrap();
}

/// pi harness-submissions "places idle input, and rejects busy input with whenBusy reject without
/// writing"。
#[tokio::test(start_paused = true)]
async fn a_busy_input_with_reject_writes_nothing() {
    let (world, engine, gate) = busy_world(|world| world.script.reply("done")).await;
    let first = world.submit(&engine, "first").await;
    world.until_log("start slow #1").await;
    let before = world.entries().await.len();

    let rejected = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::input("nope").reject_when_busy(),
        )
        .await;
    assert!(
        matches!(rejected, Err(SubmitError::Busy(CONV))),
        "{rejected:?}"
    );
    assert_eq!(world.entries().await.len(), before);
    assert!(
        Harness::inbox(&*world.storage, CONV)
            .await
            .unwrap()
            .items
            .is_empty()
    );

    gate.notify_one();
    world.harness.wait(&engine, first).await.unwrap();
    engine.close().await.unwrap();
}

/// pi harness-inbox "ends the run at a queued reset after tools and runs earlier follow-ups in the
/// new context"。
#[tokio::test(start_paused = true)]
async fn a_queued_reset_ends_the_run_and_earlier_follow_ups_run_in_the_new_context() {
    let (world, engine, gate) = busy_world(|world| world.script.reply("fresh answer")).await;
    let first = world.submit(&engine, "A").await;
    world.until_log("start slow #1").await;
    let follow = world.submit(&engine, "B").await;
    let reset = world.harness.reset(&engine, CONV, None).await.unwrap();
    assert_eq!(
        input_state(&world.submission(follow).await),
        InputState::Queued
    );
    assert_eq!(
        write_state(&world.submission(reset).await),
        WriteState::Queued
    );

    gate.notify_one();
    world.harness.wait(&engine, follow).await.unwrap();

    assert_eq!(
        input_state(&world.submission(first).await),
        InputState::Unanswered {
            entry: Some(world.entries_of("komo.user").await[0].id),
            reason: "reset".into(),
            detail: None,
        },
        "the reset cut A's context before an answer"
    );
    assert!(matches!(
        write_state(&world.submission(reset).await),
        WriteState::Done { .. }
    ));
    assert!(matches!(
        input_state(&world.submission(follow).await),
        InputState::Done { .. }
    ));
    assert_eq!(
        outline(&world.script.requests()[1]),
        ["system", "user: B"],
        "B runs in the context the reset started"
    );
    engine.close().await.unwrap();
}

/// pi harness-inbox "makes a queued head write stale when it targets an entry before the active
/// range"、"settles a stale write at once while idle"。
#[tokio::test(start_paused = true)]
async fn a_head_write_before_the_active_range_is_stale_at_once_while_idle() {
    let world = World::new();
    world.script.reply("hello");
    let engine = world.open().await;
    let first = world.submit(&engine, "hi").await;
    world.harness.wait(&engine, first).await.unwrap();
    let old = world.entries_of("komo.user").await[0].id;

    world.harness.reset(&engine, CONV, None).await.unwrap();
    let stale = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::write(WriteDraft {
                kind: "komo.compaction".into(),
                model: Some(vec![komo_kernel::durable::Message::user("summary", 1)]),
                data: None,
                head: Some(WriteHead::At(old)),
                edits: None,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        write_state(&world.submission(stale).await),
        WriteState::Unanswered {
            reason: "stale".into(),
            detail: None
        }
    );
    assert!(world.entries_of("komo.compaction").await.is_empty());
    engine.close().await.unwrap();
}

/// pi harness-inbox "makes a head write stale behind a reset placed earlier in the same boundary"。
#[tokio::test(start_paused = true)]
async fn a_head_write_behind_a_reset_in_the_same_boundary_is_stale() {
    let (world, engine, gate) = busy_world(|world| world.script.reply("ok")).await;
    let first = world.submit(&engine, "A").await;
    world.until_log("start slow #1").await;
    let old = world.entries_of("komo.user").await[0].id;
    let reset = world.harness.reset(&engine, CONV, None).await.unwrap();
    let head = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::write(WriteDraft {
                kind: "komo.compaction".into(),
                model: Some(vec![komo_kernel::durable::Message::user("summary", 1)]),
                data: None,
                head: Some(WriteHead::At(old)),
                edits: None,
            }),
        )
        .await
        .unwrap();

    gate.notify_one();
    // 没有后继用户项：reset 收了 run，boundary 不开新 run。
    world.harness.wait(&engine, head).await.unwrap();
    assert!(matches!(
        write_state(&world.submission(reset).await),
        WriteState::Done { .. }
    ));
    assert_eq!(
        write_state(&world.submission(head).await),
        WriteState::Unanswered {
            reason: "stale".into(),
            detail: None
        }
    );
    assert_eq!(
        input_state(&world.submission(first).await),
        InputState::Unanswered {
            entry: Some(old),
            reason: "reset".into(),
            detail: None
        }
    );
    engine.close().await.unwrap();
}

/// pi harness-inbox "withdraws a queued submission and removes its item"；harness-submissions
/// "reports abort results"。
#[tokio::test(start_paused = true)]
async fn withdrawing_a_queued_input_removes_its_item_and_reports_the_rest() {
    let (world, engine, gate) = busy_world(|world| world.script.reply("done")).await;
    let first = world.submit(&engine, "first").await;
    world.until_log("start slow #1").await;
    let keep = world.submit(&engine, "keep").await;
    let drop = world.submit(&engine, "drop").await;

    assert_eq!(
        world
            .harness
            .withdraw(&engine, drop, Some(CONV))
            .await
            .unwrap(),
        WithdrawResult::Aborted
    );
    assert_eq!(
        input_state(&world.submission(drop).await),
        InputState::Unanswered {
            entry: None,
            reason: "aborted".into(),
            detail: None
        }
    );
    let inbox = Harness::inbox(&*world.storage, CONV).await.unwrap();
    assert_eq!(
        inbox.items.iter().map(InboxItem::id).collect::<Vec<_>>(),
        [keep]
    );
    assert_eq!(
        world.harness.withdraw(&engine, first, None).await.unwrap(),
        WithdrawResult::AlreadyPlaced
    );
    assert_eq!(
        world.harness.withdraw(&engine, drop, None).await.unwrap(),
        WithdrawResult::Settled
    );
    assert_eq!(
        world
            .harness
            .withdraw(&engine, komo_kernel::durable::SubmissionId::new(9999), None)
            .await
            .unwrap(),
        WithdrawResult::NotFound
    );
    world.script.reply("kept answer");
    gate.notify_one();
    world.harness.wait(&engine, keep).await.unwrap();
    engine.close().await.unwrap();
}

/// 设计 §4.3 最后一条（D5 决定 8 改）：失败的 run 只把自己的输入记 `unanswered`，收件箱里排着的
/// 用户输入不再留着等下一条输入——同一次提交里放置、起后继 run，前一轮怎么结束都不挡后面的消息
/// （旧 komo 的行为；pi 的"失败后留在收件箱"在这里有意不照搬，见 `docs/storage-durable.md` §13.2）。
/// 对应 pi harness-inbox "leaves queued items after a failed run until the next submission places
/// them in order" 的反面。
#[tokio::test(start_paused = true)]
async fn a_failed_run_is_followed_by_the_queued_user_input() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    world.script.push(super::support::Step::Gate(
        gate.clone(),
        Box::new(super::support::Step::Error(rejected(400))),
    ));
    world.script.reply("second answer");
    world.script.reply("third answer");
    let engine = world.open().await;

    let first = world.submit(&engine, "one").await;
    let second = world.submit(&engine, "two").await;
    assert_eq!(
        input_state(&world.submission(second).await),
        InputState::Queued
    );
    gate.notify_one();
    let failed = world.harness.wait(&engine, first).await.unwrap();
    let InputState::Unanswered { reason, detail, .. } = input_state(&failed) else {
        panic!("unanswered expected: {failed:?}");
    };
    assert_eq!(reason, "model_error");
    assert!(detail.unwrap().to_string().contains("400"));

    // 失败的 run 收尾的同一次提交把 `two` 放置了、起了后继 run：它照常跑完并回答。
    let settled = world.harness.wait(&engine, second).await.unwrap();
    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    assert!(world.inbox().await.items.is_empty());
    let requests = world.script.requests();
    assert_eq!(requests.len(), 2, "the failed run and its successor");
    assert_eq!(outline(&requests[1]).last().unwrap(), "user: two");
    assert!(world.live().await.run.is_none());

    // 之后的输入照常：空闲、收件箱空，直接放置。
    let third = world.submit(&engine, "three").await;
    assert!(matches!(
        input_state(&world.submission(third).await),
        InputState::Placed { .. } | InputState::Done { .. }
    ));
    world.harness.wait(&engine, third).await.unwrap();
    assert_eq!(
        outline(&world.script.requests()[2]).last().unwrap(),
        "user: three"
    );
    engine.close().await.unwrap();
}

/// pi harness-inbox "keeps queued submissions across reopen and settles them afterwards"；
/// 设计 §5："submission `queued`：在收件箱里，下一个 boundary 放"。
#[tokio::test(start_paused = true)]
async fn queued_submissions_survive_a_restart_and_run_at_the_final_boundary() {
    let world = World::new();
    let hang = Arc::new(TestTool::writer("slow", &world.log).hanging_first(1));
    world.tools(vec![hang]);
    world.script.calls(&[("c1", "slow", json!({}))]);
    world.script.reply("first answer");
    world.script.reply("second answer");
    let engine = world.open().await;
    let first = world.submit(&engine, "first").await;
    world.until_log("start slow #1").await;
    let second = world.submit(&engine, "second").await;

    let engine = world.restart(engine).await;
    assert_eq!(
        input_state(&world.submission(second).await),
        InputState::Queued
    );
    world.harness.wait(&engine, second).await.unwrap();
    assert!(matches!(
        input_state(&world.submission(first).await),
        InputState::Done { .. }
    ));
    assert_eq!(
        outline(&world.script.requests()[2]).last().unwrap(),
        "user: second"
    );
    engine.close().await.unwrap();
}

/// pi harness-inbox "places every follow-up in one successor run with followUpMode all"、
/// "reads queue modes when the final boundary's commit runs"。
#[tokio::test(start_paused = true)]
async fn follow_up_mode_all_places_every_follow_up_in_one_successor_run() {
    let (world, engine, gate) = busy_world(|world| {
        world.script.reply("first answer");
        world.script.reply("batch answer");
    })
    .await;
    let first = world.submit(&engine, "one").await;
    world.until_log("start slow #1").await;
    let a = world.submit(&engine, "two").await;
    let b = world.submit(&engine, "three").await;
    // 设置在 boundary 的提交里现读：排队之后才改也生效。
    world.harness.deps().set_settings(HarnessSettings {
        follow_up_mode: QueueMode::All,
        ..HarnessSettings::default()
    });

    gate.notify_one();
    world.harness.wait(&engine, b).await.unwrap();
    assert!(matches!(
        input_state(&world.submission(first).await),
        InputState::Done { .. }
    ));
    let (a, b) = (world.submission(a).await, world.submission(b).await);
    assert_eq!(a.status(), SubmissionStatus::Done);
    assert_eq!(
        input_state(&a).clone_answer(),
        input_state(&b).clone_answer()
    );
    assert_eq!(
        outline(&world.script.requests()[2]).split_at(5).1,
        ["user: two", "user: three"]
    );
    engine.close().await.unwrap();
}

/// pi harness-input-entries "places an idle input as its entry: the given kind and data, with the
/// content as its user message"、"rejects an input entry whose kind is not a non-empty string or
/// starts with pi., writing nothing"、"queues busy inputs with their entries in the inbox and
/// places them as those entries at boundaries"。
#[tokio::test(start_paused = true)]
async fn an_input_can_be_placed_as_its_own_entry_kind_with_data() {
    let (world, engine, gate) = busy_world(|world| {
        world.script.reply("first answer");
        world.script.reply("second answer");
    })
    .await;
    let rejected = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::input("x")
                .with_entry(super::super::InputEntry::new("komo.user", None)),
        )
        .await;
    assert!(
        matches!(rejected, Err(SubmitError::Invalid(_))),
        "{rejected:?}"
    );
    assert!(world.entries().await.is_empty(), "nothing was written");

    let first = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::input("hello").with_entry(super::super::InputEntry::new(
                "channel.message",
                Some(json!({"from": "alice"})),
            )),
        )
        .await
        .unwrap();
    world.until_log("start slow #1").await;
    let placed = world.entries_of("channel.message").await;
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].data, Some(json!({"from": "alice"})));
    assert_eq!(text_of(&placed[0].model.as_ref().unwrap()[0]), "hello");
    assert!(world.entries_of("komo.user").await.is_empty());

    let second = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::input("later").with_entry(super::super::InputEntry::new(
                "channel.message",
                Some(json!({"from": "bob"})),
            )),
        )
        .await
        .unwrap();
    let inbox = Harness::inbox(&*world.storage, CONV).await.unwrap();
    assert!(matches!(
        &inbox.items[0],
        InboxItem::FollowUp { entry: Some(entry), .. } if entry.kind == "channel.message"
    ));
    gate.notify_one();
    world.harness.wait(&engine, second).await.unwrap();
    let entries = world.entries_of("channel.message").await;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].data, Some(json!({"from": "bob"})));
    assert!(matches!(
        input_state(&world.submission(first).await),
        InputState::Done { .. }
    ));
    engine.close().await.unwrap();
}

/// pi harness-input-entries "queues a data-only write behind a queued reset, so it lands after the
/// reset and stays visible"：排在 reset 前面的展示用 write 会落在 reset 开始的上下文之外，所以它
/// 排在 reset 后面，在同一个 boundary 放。
#[tokio::test(start_paused = true)]
async fn a_display_only_write_behind_a_queued_reset_lands_after_it() {
    let (world, engine, gate) = busy_world(|_| {}).await;
    let first = world.submit(&engine, "A").await;
    world.until_log("start slow #1").await;
    let reset = world.harness.reset(&engine, CONV, None).await.unwrap();
    let note = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::write(WriteDraft::display("komo.label", json!({"label": "later"}))),
        )
        .await
        .unwrap();
    assert_eq!(
        write_state(&world.submission(note).await),
        WriteState::Queued
    );
    assert!(world.entries_of("komo.label").await.is_empty());

    gate.notify_one();
    world.harness.wait(&engine, note).await.unwrap();
    let kinds: Vec<_> = world.entries().await.into_iter().map(|e| e.kind).collect();
    let reset_at = kinds.iter().position(|k| k == "komo.reset").unwrap();
    let label_at = kinds.iter().position(|k| k == "komo.label").unwrap();
    assert!(label_at > reset_at, "{kinds:?}");
    assert!(matches!(
        write_state(&world.submission(reset).await),
        WriteState::Done { .. }
    ));
    assert!(matches!(
        input_state(&world.submission(first).await),
        InputState::Unanswered { ref reason, .. } if reason == "reset"
    ));
    engine.close().await.unwrap();
}

// ------------------------------------------------------------ 内部输入

/// 系统递的内部输入（后台任务交回的结果、监听消息的形状）：steer，请求键去重。
fn internal_steer(text: &str, key: &str) -> SubmissionDraft {
    SubmissionDraft::input(text)
        .steer()
        .internal()
        .with_request_id(key)
}

fn unanswered_reason(record: &komo_kernel::durable::SubmissionRecord) -> String {
    match input_state(record) {
        InputState::Unanswered { reason, .. } => reason,
        other => panic!("unanswered expected, got {other:?}"),
    }
}

/// 旧数据没有 `internal` 字段：按用户输入读；用户输入写回时不带它，旧读者照样认得。
#[test]
fn an_inbox_item_without_the_internal_field_reads_as_a_user_input() {
    let old: InboxItem =
        serde_json::from_value(json!({ "mode": "steer", "id": 5, "content": "x" })).unwrap();
    assert!(!old.is_internal());
    let value = serde_json::to_value(&old).unwrap();
    assert!(value.get("internal").is_none(), "{value}");
    let internal: InboxItem = serde_json::from_value(
        json!({ "mode": "followUp", "id": 6, "content": "y", "internal": true }),
    )
    .unwrap();
    assert!(internal.is_internal());
    assert_eq!(serde_json::to_value(&internal).unwrap()["internal"], true);
}

/// `Conversation.abort()` 只撤用户的输入；内部输入（任务结果）留着，run 一收尾就在同一次提交里用它
/// 起后继 run，模型看得见它并回复——旧 komo 取消 home 的 Run 不动任务结果的行为。
#[tokio::test(start_paused = true)]
async fn cancelling_a_busy_run_keeps_internal_inputs_and_runs_them_next() {
    let (world, engine, _gate) = busy_world(|world| world.script.reply("relayed")).await;
    let first = world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    let steer = world
        .harness
        .submit(&engine, CONV, SubmissionDraft::input("user steer").steer())
        .await
        .unwrap();
    let follow = world.submit(&engine, "user follow").await;
    let result = world
        .harness
        .submit(
            &engine,
            CONV,
            internal_steer("task finished: 42", "task-result:t:1"),
        )
        .await
        .unwrap();

    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    let settled = world.harness.wait(&engine, result).await.unwrap();

    // 用户的 steer / followUp 撤回了，没有放置过；任务结果是后继 run 的输入。
    assert_eq!(unanswered_reason(&world.submission(first).await), "aborted");
    for id in [steer, follow] {
        assert!(matches!(
            input_state(&world.submission(id).await),
            InputState::Unanswered { entry: None, ref reason, .. } if reason == "aborted"
        ));
    }
    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    assert!(world.inbox().await.items.is_empty());
    let requests = world.script.requests();
    assert_eq!(requests.len(), 2, "the original run and its successor");
    assert_eq!(
        outline(&requests[1]).last().unwrap(),
        "user: task finished: 42"
    );
    assert!(
        !user_texts(&requests[1])
            .iter()
            .any(|text| text.starts_with("user")),
        "the withdrawn user inputs never reached the model"
    );
    assert!(world.live().await.run.is_none());
    engine.close().await.unwrap();
}

fn user_texts(messages: &[komo_kernel::durable::Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.is_user())
        .map(text_of)
        .collect()
}

/// 取消时排队的 write 照旧留着；有内部输入要起后继 run 时，write 先放、内部输入后放（boundary 的
/// 次序），所以排在内部输入前的 reset 把它放进了新上下文。
#[tokio::test(start_paused = true)]
async fn the_successor_run_places_queued_writes_before_the_internal_input() {
    let (world, engine, _gate) = busy_world(|world| world.script.reply("relayed")).await;
    world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    let reset = world.harness.reset(&engine, CONV, None).await.unwrap();
    let result = world
        .harness
        .submit(
            &engine,
            CONV,
            internal_steer("late result", "task-result:t:2"),
        )
        .await
        .unwrap();

    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    world.harness.wait(&engine, result).await.unwrap();

    assert!(matches!(
        write_state(&world.submission(reset).await),
        WriteState::Done { .. }
    ));
    let kinds: Vec<_> = world.entries().await.into_iter().map(|e| e.kind).collect();
    let reset_at = kinds.iter().position(|k| k == "komo.reset").unwrap();
    let last_user = kinds.iter().rposition(|k| k == "komo.user").unwrap();
    assert!(reset_at < last_user, "{kinds:?}");
    assert_eq!(
        outline(&world.script.requests()[1]),
        ["system", "user: late result"]
    );
    engine.close().await.unwrap();
}

/// 模型失败（`model_error`）后，用户排队的 followUp 与内部输入一起在收尾的同一次提交里放置（D5
/// 决定 8；此前只放内部输入、用户的留着）：各按 ID 序，所以先提交的用户 followUp 排在内部的结果前，
/// 两条都是后继 run 的起始输入，一个请求里模型同时看到。
#[tokio::test(start_paused = true)]
async fn a_failed_run_places_the_queued_user_and_internal_inputs_together_in_id_order() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    world.script.push(super::support::Step::Gate(
        gate.clone(),
        Box::new(super::support::Step::Error(rejected(400))),
    ));
    let release = Arc::new(Notify::new());
    world.script.push(super::support::Step::Gate(
        release.clone(),
        Box::new(super::support::Step::Reply(text_reply("relayed"))),
    ));
    let engine = world.open().await;

    let first = world.submit(&engine, "one").await;
    let user = world.submit(&engine, "user follow").await;
    let result = world
        .harness
        .submit(&engine, CONV, internal_steer("job done", "task-result:t:3"))
        .await
        .unwrap();
    gate.notify_one();
    let failed = world.harness.wait(&engine, first).await.unwrap();
    assert_eq!(unanswered_reason(&failed), "model_error");

    // 后继 run 已经起来（卡在模型请求上）：两条输入都放置了，收件箱空。
    world
        .until(|| async { world.script.request_count() == 2 })
        .await;
    for id in [user, result] {
        assert!(matches!(
            input_state(&world.submission(id).await),
            InputState::Placed { .. }
        ));
    }
    assert!(world.inbox().await.items.is_empty());
    release.notify_one();
    world.harness.wait(&engine, user).await.unwrap();
    world.harness.wait(&engine, result).await.unwrap();

    let requests = world.script.requests();
    assert_eq!(requests.len(), 2, "one successor run for both");
    let tail: Vec<_> = outline(&requests[1]).into_iter().rev().take(2).collect();
    assert_eq!(tail, ["user: job done", "user: user follow"]);
    engine.close().await.unwrap();
}

/// 预算（轮数）收尾同样走后继放置。
#[tokio::test(start_paused = true)]
async fn a_run_stopped_by_the_budget_runs_the_queued_internal_input() {
    let (world, engine, gate) = busy_world(|world| world.script.reply("relayed")).await;
    world.hooks.set_budget(RunBudget {
        max_rounds: Some(1),
        ..RunBudget::default()
    });
    let first = world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    // followUp 形状的内部输入：postTools boundary 不放，等 final / 后继放置。
    let result = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::input("budget result")
                .internal()
                .with_request_id("monitor-end:7"),
        )
        .await
        .unwrap();
    gate.notify_one();
    let stopped = world.harness.wait(&engine, first).await.unwrap();
    assert_eq!(unanswered_reason(&stopped), "budget");
    world.harness.wait(&engine, result).await.unwrap();

    assert_eq!(
        outline(world.script.requests().last().unwrap())
            .last()
            .unwrap(),
        "user: budget result"
    );
    engine.close().await.unwrap();
}

/// 调度器写的 `faulted`（钩子失败）经 `settle_scheduler_outcome` 收尾——那笔提交已经写过任务记录，
/// 放置照样能读 head 标记；内部输入因此成了后继 run 的输入（run 同样 fault，输入随它 settle）。
#[tokio::test(start_paused = true)]
async fn a_faulted_run_places_the_queued_internal_input_in_the_same_commit() {
    let (world, engine, gate) = busy_world(|_| {}).await;
    let first = world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    let result = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::input("after the fault")
                .internal()
                .with_request_id("monitor-end:8"),
        )
        .await
        .unwrap();
    world
        .hooks
        .set_failure(Some(HookError::Failed("the registry is on fire".into())));
    gate.notify_one();
    let faulted = world.harness.wait(&engine, first).await.unwrap();
    assert_eq!(unanswered_reason(&faulted), "faulted");
    let settled = world.harness.wait(&engine, result).await.unwrap();

    // 后继 run 也 fault 了，但内部输入确实被放置过（`entry` 有值），之后收件箱是空的：不会重放。
    assert!(matches!(
        input_state(&settled),
        InputState::Unanswered { entry: Some(_), ref reason, .. } if reason == "faulted"
    ));
    assert!(world.inbox().await.items.is_empty());
    assert!(world.live().await.run.is_none());
    engine.close().await.unwrap();
}

/// 显式的单条撤回对内部输入仍然有效。
#[tokio::test(start_paused = true)]
async fn an_explicit_withdraw_still_removes_an_internal_input() {
    let (world, engine, gate) = busy_world(|_| {}).await;
    let first = world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    let result = world
        .harness
        .submit(
            &engine,
            CONV,
            internal_steer("never mind", "task-result:t:4"),
        )
        .await
        .unwrap();

    assert_eq!(
        world
            .harness
            .withdraw(&engine, result, Some(CONV))
            .await
            .unwrap(),
        WithdrawResult::Aborted
    );
    assert_eq!(
        unanswered_reason(&world.submission(result).await),
        "aborted"
    );
    assert!(world.inbox().await.items.is_empty());

    gate.notify_one();
    world.script.reply("done");
    world.harness.wait(&engine, first).await.unwrap();
    assert_eq!(
        world.script.request_count(),
        2,
        "no successor run was started"
    );
    engine.close().await.unwrap();
}

/// 后继 run 再被取消不会死循环：内部输入放置后就是那条 run 的输入，随它 settle
/// （`unanswered{aborted}`），收件箱里没有了，第二次取消收尾时没有可放的东西。
#[tokio::test(start_paused = true)]
async fn cancelling_the_successor_run_does_not_loop() {
    let (world, engine, _gate) = busy_world(|world| world.script.push(Step::Hang)).await;
    world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    let result = world
        .harness
        .submit(&engine, CONV, internal_steer("result", "task-result:t:5"))
        .await
        .unwrap();

    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    // 后继 run 在跑（卡在模型请求上），输入已放置。
    world
        .until(|| async { world.script.request_count() == 2 })
        .await;
    assert!(world.live().await.run.is_some());
    assert!(matches!(
        input_state(&world.submission(result).await),
        InputState::Placed { .. }
    ));

    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    assert_eq!(
        unanswered_reason(&world.submission(result).await),
        "aborted"
    );
    assert!(world.live().await.run.is_none());
    assert!(world.inbox().await.items.is_empty());
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert_eq!(world.script.request_count(), 2, "no third run");
    assert!(world.live().await.run.is_none());
    engine.close().await.unwrap();
}

// ------------------------------------------------------------ abort_run（决定 8）

/// `abort_run`（`/cancel`、Esc）只停当前这一轮，不撤回收件箱：排队的用户 steer 与 followUp 在 run
/// 收尾的同一次提交里放置，成了后继 run 的起始输入（没有"当前 run"可并，所以 steer 在这里不是插话），
/// 内部输入一起；各按 ID 序，一个请求里模型同时看到。对比 `abort_conversation` 先撤用户输入。
#[tokio::test(start_paused = true)]
async fn abort_run_keeps_queued_user_inputs_and_runs_them_next() {
    let (world, engine, _gate) = busy_world(|world| world.script.push(Step::Hang)).await;
    // 队列模式取"全部"：steer 与 followUp 都不限个数（默认的 one-at-a-time 见下一个测试）。
    world.harness.deps().set_settings(HarnessSettings {
        steering_mode: QueueMode::All,
        follow_up_mode: QueueMode::All,
        ..HarnessSettings::default()
    });
    let first = world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    let steer = world
        .harness
        .submit(&engine, CONV, SubmissionDraft::input("user steer").steer())
        .await
        .unwrap();
    let follow = world.submit(&engine, "user follow").await;
    let result = world
        .harness
        .submit(
            &engine,
            CONV,
            internal_steer("task finished: 42", "task-result:t:9"),
        )
        .await
        .unwrap();

    assert!(world.harness.abort_run(&engine, CONV).await.unwrap());

    // 被取消的那条 run 收了尾；排队的东西一条没撤，全放进了后继 run。
    assert_eq!(unanswered_reason(&world.submission(first).await), "aborted");
    world
        .until(|| async { world.script.request_count() == 2 })
        .await;
    for id in [steer, follow, result] {
        assert!(
            matches!(
                input_state(&world.submission(id).await),
                InputState::Placed { .. }
            ),
            "{id}"
        );
    }
    assert!(world.inbox().await.items.is_empty());
    let live = world.live().await;
    assert_eq!(
        live.run.expect("the successor run").inputs,
        [steer, follow, result],
        "one run, inputs in ID order"
    );
    let requests = world.script.requests();
    let tail: Vec<_> = outline(&requests[1]).into_iter().rev().take(3).collect();
    assert_eq!(
        tail,
        [
            "user: task finished: 42",
            "user: user follow",
            "user: user steer"
        ]
    );
    // 后继 run 里新来的 steer 才是插话：并进它的下一个 postTools boundary，这里只看它排了队。
    let later = world
        .harness
        .submit(&engine, CONV, SubmissionDraft::input("later").steer())
        .await
        .unwrap();
    assert_eq!(
        input_state(&world.submission(later).await),
        InputState::Queued
    );
    // 空闲的会话上取消没有对象。
    assert!(world.harness.abort_run(&engine, CONV).await.unwrap());
    assert!(!world.harness.abort_run(&engine, CONV).await.unwrap());
    engine.close().await.unwrap();
}

/// 取消的是当前这一条 run，哪怕它已经换到第二轮的生成任务：`abort_run` 认 run 的身份，不是第一个
/// 任务。排队的 write 在同一次提交里放置；没有输入可放就不起后继 run。
#[tokio::test(start_paused = true)]
async fn abort_run_stops_a_run_in_a_later_round_and_places_a_queued_write_without_a_successor() {
    let world = World::new();
    let gate = Arc::new(Notify::new());
    let second_gate = Arc::new(Notify::new());
    world.tools(vec![
        Arc::new(TestTool::writer("slow", &world.log).with_gate(&gate)),
        Arc::new(TestTool::writer("slower", &world.log).with_gate(&second_gate)),
    ]);
    world.script.calls(&[("c1", "slow", json!({}))]);
    world.script.calls(&[("c2", "slower", json!({}))]);
    let engine = world.open().await;
    let first = world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    gate.notify_one();
    world.until_log("start slower #1").await;
    let reset = world.harness.reset(&engine, CONV, None).await.unwrap();
    assert_eq!(
        write_state(&world.submission(reset).await),
        WriteState::Queued
    );
    assert_eq!(world.tasks("komo.generation").await.len(), 2);

    assert!(world.harness.abort_run(&engine, CONV).await.unwrap());

    assert_eq!(unanswered_reason(&world.submission(first).await), "aborted");
    assert!(matches!(
        write_state(&world.submission(reset).await),
        WriteState::Done { .. }
    ));
    assert!(world.live().await.run.is_none());
    assert!(world.inbox().await.items.is_empty());
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert_eq!(world.script.request_count(), 2, "no successor run");
    engine.close().await.unwrap();
}

/// `abort_conversation` 的语义不变：先撤用户输入，再 abort；对比 `abort_run`。
#[tokio::test(start_paused = true)]
async fn abort_conversation_still_withdraws_the_queued_user_inputs() {
    let (world, engine, _gate) = busy_world(|_| {}).await;
    let first = world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    let follow = world.submit(&engine, "user follow").await;
    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    assert_eq!(unanswered_reason(&world.submission(first).await), "aborted");
    assert_eq!(
        unanswered_reason(&world.submission(follow).await),
        "aborted"
    );
    assert_eq!(world.script.request_count(), 1, "no successor run");
    engine.close().await.unwrap();
}

/// 非正常收尾的后继 boundary 按队列模式放置：`OneAtATime`（默认）时每种取第一项，余下的留给后继
/// run 自己的 boundary——每个后继 run 至少消耗一个排队的输入，链条有限。
#[tokio::test(start_paused = true)]
async fn the_successor_of_an_abnormal_end_follows_the_queue_modes_and_leaves_the_rest_queued() {
    let (world, engine, gate) = busy_world(|world| {
        world.script.push(Step::Error(rejected(400)));
        world.script.reply("first follow answer");
        world.script.reply("second follow answer");
    })
    .await;
    // 第一个请求（带 slow 调用）之后模型失败：用 postTools 之后的那次请求失败来结束 run。
    let first = world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    let one = world.submit(&engine, "follow one").await;
    let two = world.submit(&engine, "follow two").await;
    gate.notify_one();
    let failed = world.harness.wait(&engine, first).await.unwrap();
    assert_eq!(unanswered_reason(&failed), "model_error");

    // 后继 run 只带 `follow one`（followUp 模式 one-at-a-time）；`follow two` 排到它收尾时再跑。
    world.harness.wait(&engine, one).await.unwrap();
    world.harness.wait(&engine, two).await.unwrap();
    let requests = world.script.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(outline(&requests[2]).last().unwrap(), "user: follow one");
    assert_eq!(outline(&requests[3]).last().unwrap(), "user: follow two");
    assert!(world.inbox().await.items.is_empty());
    engine.close().await.unwrap();
}

// ------------------------------------------------------------ komo.settled

fn settled_data(entry: &komo_kernel::durable::EntryRecord) -> serde_json::Value {
    assert_eq!(entry.kind, "komo.settled");
    assert!(entry.model.is_none(), "只展示，模型看不到");
    entry.data.clone().expect("data")
}

/// D5 契约：`inbox::settle` 每结算一个输入 submission，同一次提交追加一条 `komo.settled`
/// `{submission, status, reason?, detail?, answer?}`：回答的 `done`、吸收的 steer（与 run 同时结算，
/// 各一条）、排队中被撤回的 `unanswered{aborted}`（没有放置过）、模型失败的 `unanswered`；write 不留。
#[tokio::test(start_paused = true)]
async fn every_input_settlement_leaves_one_settled_entry() {
    let (world, engine, gate) = busy_world(|world| {
        world.script.reply("done");
        world.script.push(Step::Error(rejected(400)));
    })
    .await;
    let first = world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    let steer = world
        .harness
        .submit(&engine, CONV, SubmissionDraft::input("also this").steer())
        .await
        .unwrap();
    let doomed = world.submit(&engine, "never mind").await;
    world
        .harness
        .withdraw(&engine, doomed, Some(CONV))
        .await
        .unwrap();
    gate.notify_one();
    world.harness.wait(&engine, first).await.unwrap();
    world.harness.wait(&engine, steer).await.unwrap();
    // 一条 write（展示用的 label）：submission 照常 `done`，不留结算锚点。
    let label = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::write(WriteDraft::display("label", json!({"label": "x"}))),
        )
        .await
        .unwrap();
    assert!(matches!(
        write_state(&world.submission(label).await),
        WriteState::Done { .. }
    ));
    let failing = world.submit(&engine, "break").await;
    world.harness.wait(&engine, failing).await.unwrap();

    let settled = world.entries_of("komo.settled").await;
    let by_submission = |id: komo_kernel::durable::SubmissionId| {
        let matching: Vec<_> = settled
            .iter()
            .filter(|entry| settled_data(entry)["submission"] == json!(id))
            .collect();
        assert_eq!(matching.len(), 1, "one anchor for {id}: {settled:?}");
        (matching[0].id, settled_data(matching[0]))
    };
    assert_eq!(
        settled.len(),
        4,
        "go, the steer, the withdrawn one, the failure"
    );
    let answer = {
        let InputState::Done { answer, .. } = input_state(&world.submission(first).await) else {
            panic!("done expected");
        };
        answer
    };
    let (go_at, go) = by_submission(first);
    assert_eq!(
        go,
        json!({"submission": first, "status": "done", "answer": answer})
    );
    let (steer_at, steer_data) = by_submission(steer);
    assert_eq!(
        steer_data,
        json!({"submission": steer, "status": "done", "answer": answer}),
        "an absorbed steer settles with its run's answer"
    );
    assert_eq!(
        steer_at.get().abs_diff(go_at.get()),
        1,
        "one run, its inputs settle side by side"
    );
    assert!(
        go_at > answer && steer_at > answer,
        "anchors follow the answer entry"
    );
    let (_, withdrawn) = by_submission(doomed);
    assert_eq!(
        withdrawn,
        json!({"submission": doomed, "status": "unanswered", "reason": "aborted"})
    );
    let (_, failed) = by_submission(failing);
    assert_eq!(failed["status"], "unanswered");
    assert_eq!(failed["reason"], "model_error");
    assert!(failed["detail"].to_string().contains("400"), "{failed}");
    // write submission 不留锚点。
    assert!(
        settled
            .iter()
            .all(|entry| settled_data(entry)["submission"] != json!(label))
    );
    // 它们不进模型的上下文。
    for request in world.script.requests() {
        assert!(
            !outline(&request)
                .iter()
                .any(|line| line.contains("settled")),
            "{request:?}"
        );
    }
    engine.close().await.unwrap();
}

// ------------------------------------------------------------ admit_in_tx

/// gateway 在一次提交里"建 / fork conversation + 受理"：fork 出来的 conversation 上放了输入、起了
/// run，上下文是父到 fork 点为止的历史加这条输入；同一个请求键再来原样返回；新建的根同样能用；
/// 违规的输入 entry kind 被拒。
#[tokio::test(start_paused = true)]
async fn admit_in_tx_forks_and_places_in_one_commit() {
    let world = World::new();
    world.script.reply("first answer");
    world.script.reply("branch answer");
    world.script.reply("root answer");
    let engine = world.open().await;
    let first = world.submit(&engine, "first question").await;
    world.harness.wait(&engine, first).await.unwrap();
    let user_entry = world.entries_of("komo.user").await[0].id;

    let harness = world.harness.clone();
    let (branch, admitted) = engine
        .commit(move |tx| {
            Box::pin(async move {
                let branch = tx
                    .fork_conversation(CONV, user_entry, ConversationOwnership::Ownerless)
                    .await?;
                let draft = SubmissionDraft::input("edited question").with_request_id("edit-1");
                let admitted = harness.admit_in_tx(tx, branch.id, &draft).await?;
                Ok((branch.id, admitted))
            })
        })
        .await
        .unwrap();
    engine.resume();
    let Admitted::Id(id) = admitted else {
        panic!("admitted: {admitted:?}");
    };
    // 同一次提交里：fork、输入 entry、submission（placed）、run 都在。
    assert!(matches!(
        input_state(&world.submission(id).await),
        InputState::Placed { .. } | InputState::Done { .. }
    ));
    assert!(
        world
            .storage_handle()
            .conversation(branch)
            .await
            .unwrap()
            .unwrap()
            .parent
            .is_some()
    );
    let settled = world.harness.wait(&engine, id).await.unwrap();
    assert!(matches!(input_state(&settled), InputState::Done { .. }));
    let requests = world.script.requests();
    assert_eq!(
        outline(&requests[1]),
        ["system", "user: first question", "user: edited question"]
            .map(String::from)
            .to_vec(),
        "the branch sees its parent up to the fork point plus the new input"
    );
    // 父 conversation 不受影响。
    assert_eq!(world.messages().await.len(), 3);

    // 同一个请求键在另一次提交里原样返回已有的那条，什么都不写。
    let harness = world.harness.clone();
    let again = engine
        .commit(move |tx| {
            Box::pin(async move {
                let draft = SubmissionDraft::input("edited question").with_request_id("edit-1");
                harness.admit_in_tx(tx, branch, &draft).await
            })
        })
        .await
        .unwrap();
    assert_eq!(again, Admitted::Id(id));

    // 新建的根 conversation 同样：建 + 受理 + 起 run 一次提交。
    let harness = world.harness.clone();
    let (root, root_input) = engine
        .commit(move |tx| {
            Box::pin(async move {
                let root = tx
                    .create_conversation_in("sess-x", ConversationOwnership::Ownerless)
                    .await?;
                let draft = SubmissionDraft::input("hello there").with_request_id("first");
                let Admitted::Id(id) = harness.admit_in_tx(tx, root.id, &draft).await? else {
                    unreachable!("not rejected");
                };
                Ok((root.id, id))
            })
        })
        .await
        .unwrap();
    engine.resume();
    world.harness.wait(&engine, root_input).await.unwrap();
    assert_eq!(
        world
            .storage_handle()
            .conversation(root)
            .await
            .unwrap()
            .unwrap()
            .session
            .as_deref(),
        Some("sess-x")
    );
    assert_eq!(
        outline(&world.script.requests()[2]),
        ["system", "user: hello there"].map(String::from).to_vec()
    );

    // 违规的输入 entry kind（`komo.` 前缀是 Harness 自己的）在提交里被拒，整笔不生效。
    let harness = world.harness.clone();
    let rejected = engine
        .commit(move |tx| {
            Box::pin(async move {
                let bad =
                    SubmissionDraft::input("x").with_entry(InputEntry::new("komo.user", None));
                harness.admit_in_tx(tx, CONV, &bad).await
            })
        })
        .await;
    assert!(rejected.is_err());
    engine.close().await.unwrap();
}

trait AnswerOf {
    fn clone_answer(&self) -> Option<komo_kernel::durable::EntryId>;
}

impl AnswerOf for InputState {
    fn clone_answer(&self) -> Option<komo_kernel::durable::EntryId> {
        match self {
            InputState::Done { answer, .. } => Some(*answer),
            _ => None,
        }
    }
}
