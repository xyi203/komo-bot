//! submission、收件箱与 boundary（设计 §4.3；pi spec §6）。
//! 对应 pi 的 `harness-inbox` / `harness-submissions` / `harness-input-entries`。

use std::sync::Arc;

use komo_kernel::durable::{InputState, SubmissionKind, SubmissionStatus, WriteState};
use serde_json::json;
use tokio::sync::Notify;

use super::super::inbox::{InboxItem, WriteHead};
use super::super::{
    Harness, HarnessSettings, QueueMode, SubmissionDraft, SubmitError, WithdrawResult, WriteDraft,
};
use super::support::*;

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

/// pi harness-inbox "leaves queued items after a failed run until the next submission places them
/// in order"；设计 §4.3 最后一条：失败的 run 只把自己的输入记 `unanswered`，收件箱不动。
#[tokio::test(start_paused = true)]
async fn a_failed_run_leaves_the_inbox_until_the_next_submission_runs_a_boundary() {
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
    gate.notify_one();
    let failed = world.harness.wait(&engine, first).await.unwrap();
    let InputState::Unanswered { reason, detail, .. } = input_state(&failed) else {
        panic!("unanswered expected: {failed:?}");
    };
    assert_eq!(reason, "model_error");
    assert!(detail.unwrap().to_string().contains("400"));
    // 失败的 run 没动收件箱：`two` 还在排队，conversation 又空闲了。
    assert!(world.live().await.run.is_none());
    assert_eq!(
        input_state(&world.submission(second).await),
        InputState::Queued
    );

    // 下一条输入排在它后面，并在同一次提交里跑一个 final boundary：`two` 先跑。
    let third = world.submit(&engine, "three").await;
    world.harness.wait(&engine, second).await.unwrap();
    // `two` 的 final boundary 在同一次提交里放了 `three`。
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
