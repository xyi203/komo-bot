//! 编排工具 `task` / `monitor` / `monitor_stop` 的执行路径（设计 §4.6、§12.4；
//! `docs/background-tasks.md`、`docs/monitor.md`；pi spec §5.4–§5.5、§7.3 的 subagent 两节）。
//!
//! 工具是真的 `TaskTool` / `MonitorTool` / `MonitorStopTool`（`prepare` 的校验按配置快照走），
//! 子 conversation 与父共用一个脚本模型：子的步骤用 `Script::push_for`（按最后一条用户消息认领）
//! 出牌，免得和父的请求抢次序；"子什么时候收尾"用 `Step::Gate` 控制，让父那边的请求次数是确定的。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::durable::storage::SubmissionQuery;
use komo_kernel::durable::{
    ConversationId, InputState, Message, SubmissionKind, SubmissionRecord, TaskOutcome, TaskState,
};
use komo_kernel::traits::Tool;
use komo_kernel::types::model::Effort;
use serde_json::json;
use tokio::sync::Notify;

use super::super::orchestration::monitor::{self, Reconciled};
use super::super::orchestration::{
    ANCHOR_KIND, FORK_NOTICE, LaunchMode, MonitorState, TASK_RESULT_ENTRY, task_doc,
};
use super::super::{InboxItem, SubmissionDraft};
use super::support::*;
use crate::config::testing::Fixture;
use crate::tools::{MonitorStopTool, MonitorTool, TaskTool};

// ------------------------------------------------------------ 脚手架

/// 装上真的编排工具（外加一个只读的 `read`、一个会卡住的写工具 `slow`）。`Fixture` 要活到测试
/// 结束（配置在它的临时目录里）。
fn install(world: &World) -> (Fixture, Arc<TestTool>, Arc<Notify>) {
    let fixture = Fixture::valid();
    let holder = Arc::new(fixture.holder());
    let gate = Arc::new(Notify::new());
    let read = Arc::new(TestTool::reader("read", &world.log).with_text("read output"));
    let slow = Arc::new(TestTool::writer("slow", &world.log).with_gate(&gate));
    world.hooks.set_tools(vec![
        Arc::new(TaskTool::new(holder)) as Arc<dyn Tool>,
        Arc::new(MonitorTool::new()),
        Arc::new(MonitorStopTool::new()),
        read.clone(),
        slow.clone(),
    ]);
    (fixture, read, gate)
}

fn gated(gate: &Arc<Notify>, text: &str) -> Step {
    Step::Gate(gate.clone(), Box::new(Step::Reply(text_reply(text))))
}

fn spawn_call(prompt: &str, title: &str) -> serde_json::Value {
    json!({ "prompt": prompt, "title": title })
}

/// 一次调用的结果正文：去掉投影加的 `[tool · 状态 · 耗时]` 标题行与末尾换行。
fn result_text(messages: &[Message], call: &str) -> String {
    let full = messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == call => Some(text_of(message)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {call}"));
    match full.split_once('\n') {
        Some((head, body)) if head.starts_with('[') => body.trim_end().to_owned(),
        _ => full,
    }
}

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.is_user())
        .map(text_of)
        .collect()
}

fn input_state(record: &SubmissionRecord) -> InputState {
    match &record.kind {
        SubmissionKind::Input { state } => state.clone(),
        other => panic!("an input submission expected, got {other:?}"),
    }
}

/// 父 conversation 里请求键以 `prefix` 开头的 submission。
async fn submissions_with(
    world: &World,
    conversation: ConversationId,
    prefix: &str,
) -> Vec<SubmissionRecord> {
    let page = world
        .storage_handle()
        .scan_submissions(
            &SubmissionQuery {
                conversation: Some(conversation),
                ..SubmissionQuery::default()
            },
            1000,
            None,
        )
        .await
        .unwrap();
    page.items
        .into_iter()
        .filter(|record| {
            record
                .request_id
                .as_deref()
                .is_some_and(|id| id.starts_with(prefix))
        })
        .collect()
}

fn terminal_completed(task: &komo_kernel::durable::TaskRecord) -> bool {
    matches!(
        task.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    )
}

// ------------------------------------------------------------ task

/// 设计 §4.6：派出 = 一次提交里建锚任务 + 子 conversation + 第一条输入；子 run 跑完，结果以
/// `task-result:{子}:{首个输入}` 交回父，父接着跑一轮。
#[tokio::test(start_paused = true)]
async fn a_dispatched_task_runs_in_a_child_and_its_result_reaches_the_parent() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    let child_gate = Arc::new(Notify::new());
    world
        .script
        .calls(&[("c1", "task", spawn_call("CHILD check the AC", "查空调"))]);
    world.script.reply("dispatched"); // 父：派出之后
    world.script.reply("the task says it is on"); // 父：收到交回之后
    world
        .script
        .push_for("CHILD", gated(&child_gate, "AC is on"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    // 子会话已经在了，父的 run 已经收尾：父不等它。
    let child = world.child().await;
    assert!(
        !world.live_of(child).await.run.is_none(),
        "the child is still running"
    );
    let handled = world.messages().await;
    assert_eq!(
        result_text(&handled, "c1"),
        format!("已派出 #{child}：查空调")
    );
    assert_eq!(
        outline(&handled),
        [
            "system",
            "user: go",
            "assistant calls task(c1)",
            "result(c1)",
            "assistant: dispatched",
        ]
    );

    child_gate.notify_one();
    world
        .until(|| async {
            outline(&world.messages().await)
                .contains(&"assistant: the task says it is on".to_owned())
        })
        .await;

    // 父收到的是一条带前缀的内部输入，原文是子的最终回复。
    let parent = world.messages().await;
    assert_eq!(
        user_texts(&parent)[1],
        format!(
            "[后台任务 #{child}「查空调」已完成]\n\nAC is on\n\n\
             （这是后台任务交回的结果，不是用户发来的消息；用户看不到上面这段，请把结论转告用户。）"
        )
    );
    let results = world.entries_of(TASK_RESULT_ENTRY).await;
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].data.as_ref().unwrap()["task"]["conversation"],
        json!(child)
    );
    // 请求键 `task-result:{子}:{首个输入}`：一条，已经答完。
    let delivered = submissions_with(&world, CONV, &format!("task-result:{child}:")).await;
    assert_eq!(delivered.len(), 1);
    assert!(matches!(
        input_state(&delivered[0]),
        InputState::Done { .. }
    ));
    // 子自己的对话：系统段、任务、回复；输入 done。
    assert_eq!(
        outline(&world.messages_in(child).await),
        ["system", "user: CHILD check the AC", "assistant: AC is on"]
    );
    // 锚任务 completed，子 conversation 的 owner 就是它。
    let anchors = world.tasks(ANCHOR_KIND).await;
    assert_eq!(anchors.len(), 1);
    assert!(anchors[0].background);
    world
        .until(|| async { terminal_completed(&world.tasks(ANCHOR_KIND).await[0]) })
        .await;
    engine.close().await.unwrap();
}

/// D5 契约：派出的同一次提交里给子 conversation 写一条 komo Session 行——`kind=task`、UUIDv7、
/// `parent_session` 是父 conversation 的 Session、title 是任务标题、根与当前分支都是子 conversation；
/// 子 conversation 属于这个新 Session（fork 出来的也是，不继承父的）。
#[tokio::test(start_paused = true)]
async fn a_spawned_task_has_its_own_session_row() {
    use komo_kernel::durable::storage::ConversationQuery;
    use komo_kernel::durable::{SessionKind, SessionRecord};

    let world = World::new();
    let (_fixture, ..) = install(&world);
    let child_gate = Arc::new(Notify::new());
    world.script.calls(&[
        ("c1", "task", spawn_call("CHILD plain", "查空调")),
        (
            "c2",
            "task",
            json!({ "prompt": "CHILD forked", "title": "分支", "context": "fork" }),
        ),
    ]);
    world.script.reply("dispatched");
    world.script.push_for("CHILD", gated(&child_gate, "ok"));
    world.script.push_for("CHILD", gated(&child_gate, "ok"));
    let engine = world.open().await;
    // 父是一个属于 komo Session `sess-parent` 的 conversation。
    let parent = engine
        .commit(|tx| {
            Box::pin(async move {
                let parent = tx
                    .create_conversation_in(
                        "sess-parent",
                        crate::durable::tx::ConversationOwnership::Ownerless,
                    )
                    .await?
                    .id;
                tx.put_session(SessionRecord::new("sess-parent", parent));
                Ok(parent)
            })
        })
        .await
        .unwrap();
    let id = world
        .harness
        .submit(&engine, parent, SubmissionDraft::input("go"))
        .await
        .unwrap();
    world.harness.wait(&engine, id).await.unwrap();

    let children = world.children().await;
    assert_eq!(
        children.len(),
        2,
        "{}",
        result_text(&world.messages_in(parent).await, "c2")
    );
    let storage = world.storage_handle();
    let mut ids = Vec::new();
    for (index, child) in children.iter().enumerate() {
        let record = storage.conversation(*child).await.unwrap().unwrap();
        let session_id = record
            .session
            .clone()
            .expect("a task conversation has a session");
        assert_ne!(session_id, "sess-parent", "not the parent's session");
        assert_eq!(
            record.parent.is_some(),
            index == 1,
            "the second task is the fork"
        );
        let session = storage
            .session(&session_id)
            .await
            .unwrap()
            .expect("its Session row");
        assert_eq!(session.kind, SessionKind::Task);
        assert_eq!(session.parent_session.as_deref(), Some("sess-parent"));
        assert_eq!(session.root_conversation, *child);
        assert_eq!(session.current_branch, *child);
        assert_eq!(
            session.lifecycle,
            komo_kernel::durable::SessionLifecycle::Active
        );
        assert_eq!(
            session.title.as_deref(),
            Some(["查空调", "分支"][index]),
            "the session is named after the task"
        );
        assert!(session.created_at > 0 && session.changed_at == session.created_at);
        // UUIDv7 的字符串。
        let uuid = uuid::Uuid::parse_str(&session_id).unwrap();
        assert_eq!(uuid.get_version_num(), 7);
        ids.push(session_id.clone());
        // 按 Session 能找到它的 conversation。
        let found = storage
            .scan_conversations(
                &ConversationQuery {
                    session: Some(session_id),
                    ..Default::default()
                },
                10,
                None,
            )
            .await
            .unwrap();
        assert_eq!(found.items.len(), 1);
    }
    assert_ne!(ids[0], ids[1]);
    // 父会话的 Session 行没动。
    let parent_session = storage.session("sess-parent").await.unwrap().unwrap();
    assert_eq!(parent_session.kind, SessionKind::Normal);
    child_gate.notify_waiters();
    engine.close().await.unwrap();
}

/// 设计 §4.6 / spec §5.4：子会话是 background 的，不挡父的 idle——子还在跑，父的
/// `wait_for_idle`（含全局）照样返回；锚任务停在 `completing`，子安静了才 terminal。
#[tokio::test(start_paused = true)]
async fn a_running_child_does_not_hold_the_parent_idle() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    let child_gate = Arc::new(Notify::new());
    world
        .script
        .calls(&[("c1", "task", spawn_call("CHILD wait", "等"))]);
    world.script.reply("dispatched");
    world.script.reply("noted");
    world
        .script
        .push_for("CHILD", gated(&child_gate, "finally"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    tokio::time::timeout(Duration::from_secs(5), engine.wait_for_idle(Some(CONV)))
        .await
        .expect("the parent is idle")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), engine.wait_for_idle(None))
        .await
        .expect("the whole session is idle")
        .unwrap();
    let child = world.child().await;
    assert!(world.live_of(child).await.run.is_some());
    let anchor = world.tasks(ANCHOR_KIND).await.remove(0);
    assert!(
        matches!(anchor.state, TaskState::Completing { .. }),
        "the anchor holds while the child works: {:?}",
        anchor.state
    );

    child_gate.notify_one();
    world
        .until(|| async { terminal_completed(&world.tasks(ANCHOR_KIND).await[0]) })
        .await;
    engine.close().await.unwrap();
}

/// 背景任务文档 §2b：`context: "fork"` 的子会话带着父到那一刻的对话（含同一轮里前面已经收尾的
/// 调用结果），发起这次调用的消息里没结果的调用在子里补一条结果，任务前面是 fork 的说明。
#[tokio::test(start_paused = true)]
async fn a_fork_child_starts_from_the_parents_conversation_at_that_moment() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    let child_gate = Arc::new(Notify::new());
    world.script.calls(&[
        ("c0", "read", json!({})),
        (
            "c1",
            "task",
            json!({ "prompt": "CHILD fix B1", "title": "修 B1", "context": "fork" }),
        ),
        ("c2", "read", json!({})),
    ]);
    world.script.reply("forked");
    world.script.reply("it got fixed");
    world
        .script
        .push_for("CHILD fix B1", gated(&child_gate, "B1 fixed"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let child = world.child().await;
    let seen = world.messages_in(child).await;
    assert_eq!(
        outline(&seen),
        [
            "system",
            "user: go",
            "assistant calls read(c0),task(c1),read(c2)",
            "result(c0)",
            "result(c1)",
            "result(c2)",
            format!("user: {FORK_NOTICE}\n\nCHILD fix B1").as_str(),
        ]
    );
    // c0 是父真的跑出来的结果；c1 / c2 是只进 fork 的补充。
    assert_eq!(result_text(&seen, "c0"), "read output");
    assert!(result_text(&seen, "c1").contains("派出的就是你（fork）"));
    assert!(result_text(&seen, "c2").contains("你看不到它的结果"));
    // 父的账里没有这些补充：c1 的结果是"已派出"。
    assert_eq!(
        result_text(&world.messages().await, "c1"),
        format!("已派出 #{child}：修 B1")
    );
    // 提示缓存的路由键是父。
    assert!(
        world
            .hooks
            .setups()
            .contains(&(child, Some(format!("conversation-{CONV}")))),
        "{:?}",
        world.hooks.setups()
    );
    child_gate.notify_one();
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: it got fixed".to_owned())
        })
        .await;
    engine.close().await.unwrap();
}

/// 背景任务文档 §2a：`task_id` 接着做 = 往已有的子 conversation 提交新输入，它带着自己的历史接着
/// 做，结果再交回一次（各自一个请求键）。
#[tokio::test(start_paused = true)]
async fn a_follow_up_goes_into_the_same_child_and_reports_again() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    let (first, second) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    world
        .script
        .calls(&[("c1", "task", spawn_call("CHILD one", "活"))]);
    world.script.reply("dispatched");
    world
        .script
        .push_for("CHILD one", gated(&first, "first done"));
    world
        .script
        .push_for("FOLLOW more", gated(&second, "second done"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let child = world.child().await;

    // 第一份结果到父（空闲）→ 父跑一轮，这一轮里接着做。
    world.script.calls(&[(
        "c2",
        "task",
        json!({ "task_id": format!("#{child}"), "prompt": "FOLLOW more" }),
    )]);
    world.script.reply("forwarded");
    first.notify_one();
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: forwarded".to_owned())
        })
        .await;
    assert_eq!(
        result_text(&world.messages().await, "c2"),
        format!("已转给 #{child}")
    );
    assert_eq!(
        world.children().await,
        vec![child],
        "no second child was made"
    );

    world.script.reply("all done");
    second.notify_one();
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: all done".to_owned())
        })
        .await;
    assert_eq!(
        outline(&world.messages_in(child).await),
        [
            "system",
            "user: CHILD one",
            "assistant: first done",
            "user: FOLLOW more",
            "assistant: second done",
        ]
    );
    let delivered = submissions_with(&world, CONV, &format!("task-result:{child}:")).await;
    assert_eq!(delivered.len(), 2, "one hand-back per run");
    let users = user_texts(&world.messages().await);
    assert!(users[1].starts_with(&format!("[后台任务 #{child}「活」已完成]\n\nfirst done")));
    assert!(users[2].starts_with(&format!("[后台任务 #{child}「活」已完成]\n\nsecond done")));
    engine.close().await.unwrap();
}

/// 不认识的 `task_id`（别人的、没有的、不是数字）：失败结果，不往任何地方提交。
#[tokio::test(start_paused = true)]
async fn a_follow_up_to_an_unknown_task_is_refused() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world.script.calls(&[
        ("c1", "task", json!({ "task_id": "999", "prompt": "hi" })),
        ("c2", "task", json!({ "task_id": "#abcd", "prompt": "hi" })),
        ("c3", "task", json!({ "task_id": "1" })),
    ]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let messages = world.messages().await;
    assert_eq!(
        result_text(&messages, "c1"),
        "转不过去：#999 不是这个会话派出去的任务"
    );
    assert_eq!(
        result_text(&messages, "c2"),
        "转不过去：#abcd 不是这个会话派出去的任务"
    );
    assert_eq!(
        result_text(&messages, "c3"),
        "查不到进度：#1 不是这个会话派出去的任务"
    );
    assert!(world.children().await.is_empty());
    engine.close().await.unwrap();
}

/// 任务会话里不能再派任务（schema 里有 `task` 的 fork 也一样）：结果只交回一层。
#[tokio::test(start_paused = true)]
async fn a_task_conversation_cannot_spawn_another_task() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world
        .script
        .calls(&[("c1", "task", spawn_call("CHILD nested", "套娃"))]);
    world.script.reply("dispatched");
    world.script.reply("noted");
    world.script.calls_for(
        "CHILD nested",
        &[("k1", "task", spawn_call("again", "deeper"))],
    );
    world.script.reply_for("CHILD nested", "I did it myself");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let child = world.child().await;
    world
        .until(|| async {
            outline(&world.messages_in(child).await)
                .last()
                .is_some_and(|l| l == "assistant: I did it myself")
        })
        .await;
    assert!(
        result_text(&world.messages_in(child).await, "k1")
            .contains("这里已经是一个后台任务了，不能再派任务")
    );
    assert_eq!(world.children().await, vec![child]);
    engine.close().await.unwrap();
}

/// 设计 §4.3：父忙时交回按收件箱规则排队——steer，在 postTools boundary 并进当前 run。
#[tokio::test(start_paused = true)]
async fn a_result_for_a_busy_parent_waits_in_the_inbox_for_the_boundary() {
    let world = World::new();
    let (_fixture, _read, slow_gate) = install(&world);
    world.script.calls(&[
        ("c1", "task", spawn_call("CHILD quick", "快")),
        ("c2", "slow", json!({})),
    ]);
    world.script.reply("saw the result");
    world.script.reply_for("CHILD quick", "quick answer");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;

    // 子很快跑完；父卡在 `slow` 上，交回的输入在收件箱里排着。
    world
        .until(|| async {
            let inbox = world.inbox().await;
            !inbox.items.is_empty()
        })
        .await;
    let inbox = world.inbox().await;
    assert_eq!(inbox.items.len(), 1);
    let queued = match &inbox.items[0] {
        InboxItem::Steer {
            id,
            content,
            entry,
            internal,
        } => {
            assert!(*internal, "交回的结果是内部输入");
            assert_eq!(entry.as_ref().unwrap().kind, TASK_RESULT_ENTRY);
            assert!(
                matches!(content, komo_kernel::durable::Content::Text(text) if text.starts_with("[后台任务")),
                "{content:?}"
            );
            *id
        }
        other => panic!("a steer item expected, got {other:?}"),
    };
    assert!(matches!(
        world.submission(queued).await.kind,
        SubmissionKind::Input {
            state: InputState::Queued
        }
    ));
    // 放行：postTools boundary 把它放进当前 run，下一次请求看得见。
    slow_gate.notify_one();
    world.harness.wait(&engine, id).await.unwrap();
    let outline_now = outline(&world.messages().await);
    assert_eq!(
        [&outline_now[..5], &outline_now[6..]].concat(),
        [
            "system",
            "user: go",
            "assistant calls task(c1),slow(c2)",
            "result(c1)",
            "result(c2)",
            "assistant: saw the result",
        ]
    );
    assert!(outline_now[5].starts_with("user: [后台任务 #"));
    assert!(world.inbox().await.items.is_empty());
    assert!(matches!(
        input_state(&world.submission(queued).await),
        InputState::Done { .. }
    ));
    engine.close().await.unwrap();
}

/// 设计 §5 / spec §6：重启不会让同一次收尾交两次。子 run 在飞时重启 → 重启后跑完交一次；交回
/// 之后再重启 → 还是一条；同一个请求键再受理拿回原来那条。
#[tokio::test(start_paused = true)]
async fn a_restart_never_hands_the_same_result_back_twice() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world
        .script
        .calls(&[("c1", "task", spawn_call("CHILD flaky", "不稳"))]);
    world.script.reply("dispatched");
    world.script.reply("noted once");
    world.script.push_for("CHILD flaky", Step::Hang);
    world
        .script
        .reply_for("CHILD flaky", "survived the restart");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let child = world.child().await;
    world
        .until(|| async { world.script.requests_for("CHILD flaky").len() == 1 })
        .await;

    // 子的请求在飞时重启：重启后重发，跑完，交回一次。
    let engine = world.restart(engine).await;
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: noted once".to_owned())
        })
        .await;
    let delivered = submissions_with(&world, CONV, &format!("task-result:{child}:")).await;
    assert_eq!(delivered.len(), 1);
    let key = delivered[0].request_id.clone().unwrap();

    // 交回之后再重启：什么都不会补交。
    let engine = world.restart(engine).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        submissions_with(&world, CONV, "task-result:").await.len(),
        1
    );
    assert_eq!(world.entries_of(TASK_RESULT_ENTRY).await.len(), 1);
    // 同一个请求键再来一次，拿回原来那条，不新增。
    let again = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::input("dup").with_request_id(key),
        )
        .await
        .unwrap();
    assert_eq!(again, delivered[0].id);
    assert_eq!(world.entries_of(TASK_RESULT_ENTRY).await.len(), 1);
    engine.close().await.unwrap();
}

/// 背景任务文档 §2a、`prepare` 的校验：不认识的 alias、模型不支持的档位在 `prepare` 被拒，
/// 什么都不派出去（不建锚任务、不建子 conversation）。
#[tokio::test(start_paused = true)]
async fn an_unknown_model_or_an_unsupported_effort_is_refused_before_anything_is_spawned() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world.script.calls(&[
        (
            "c1",
            "task",
            json!({ "prompt": "x", "title": "t", "model": "gpt-9" }),
        ),
        (
            "c2",
            "task",
            json!({ "prompt": "x", "title": "t", "model": "chat", "effort": "ultra" }),
        ),
    ]);
    world.script.reply("refused");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let messages = world.messages().await;
    let first = result_text(&messages, "c1");
    assert!(first.contains("gpt-9") && first.contains("chat"), "{first}");
    assert!(result_text(&messages, "c2").contains("ultra"));
    assert!(world.children().await.is_empty());
    assert!(world.tasks(ANCHOR_KIND).await.is_empty());
    engine.close().await.unwrap();
}

/// 只给 `effort` 时选定的模型 `prepare` 不知道：在派出那一步问钩子校验；拒绝就什么都不派，
/// 放行就照常派出，并把问到的记下来。
#[tokio::test(start_paused = true)]
async fn an_effort_alone_is_checked_at_dispatch_through_the_hook() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    *world.orch.reject_effort.lock().unwrap() = Some("这个模型没有 low 档".into());
    world.script.calls(&[(
        "c1",
        "task",
        json!({ "prompt": "CHILD e", "title": "t", "effort": "low" }),
    )]);
    world.script.reply("refused");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(
        result_text(&world.messages().await, "c1"),
        "派任务失败：effort 立不住：这个模型没有 low 档"
    );
    assert!(world.children().await.is_empty());
    assert_eq!(
        world.orch.effort_checks.lock().unwrap().clone(),
        vec![(CONV, None, "low".to_owned())]
    );
    engine.close().await.unwrap();
}

/// `model` / `effort` 透传进子 run：写进 `komo.task`，子的生成按名字解析（这里的钩子替身把
/// alias 映射成模型配置）；接着做时只给 effort = 模型沿用、档位换掉。
#[tokio::test(start_paused = true)]
async fn the_model_and_effort_reach_the_child_run() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    let mut chat = model_config();
    chat.model = "chat-model".into();
    world.hooks.add_alias("chat", chat);
    let (first, second) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    world.script.calls(&[(
        "c1",
        "task",
        json!({ "prompt": "CHILD m", "title": "t", "model": "chat", "effort": "low" }),
    )]);
    world.script.reply("dispatched");
    world.script.push_for("CHILD m", gated(&first, "done"));
    world
        .script
        .push_for("FOLLOW e", gated(&second, "done again"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let child = world.child().await;
    world
        .until(|| async { !world.script.requests_for("CHILD m").is_empty() })
        .await;
    let doc = task_doc(&*world.storage_handle(), child)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.model.as_deref(), Some("chat"));
    assert_eq!(doc.effort.as_ref().map(Effort::as_str), Some("low"));
    let used = world
        .script
        .models()
        .into_iter()
        .find(|model| model.model == "chat-model")
        .expect("the child ran on the chosen model");
    assert_eq!(used.effort.as_ref().map(Effort::as_str), Some("low"));

    // 接着做，只给 effort：钩子拿到的是沿用的 alias。
    world.script.calls(&[(
        "c2",
        "task",
        json!({ "task_id": child.to_string(), "prompt": "FOLLOW e", "effort": "high" }),
    )]);
    world.script.reply("sent follow-up");
    first.notify_one();
    world
        .until(|| async { world.script.requests_for("FOLLOW e").len() == 1 })
        .await;
    assert_eq!(
        world.orch.effort_checks.lock().unwrap().last().cloned(),
        Some((child, Some("chat".to_owned()), "high".to_owned()))
    );
    let doc = task_doc(&*world.storage_handle(), child)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.model.as_deref(), Some("chat"));
    assert_eq!(doc.effort.as_ref().map(Effort::as_str), Some("high"));
    second.notify_one();
    engine.close().await.unwrap();
}

/// 现行规则（`docs/background-tasks.md`）：任务是独立的会话，取消父的 run 不连带它；子跑完照样
/// 交回，父空闲了就开一轮。
#[tokio::test(start_paused = true)]
async fn cancelling_the_parent_leaves_the_child_running() {
    let world = World::new();
    let (_fixture, _read, slow_gate) = install(&world);
    let child_gate = Arc::new(Notify::new());
    world.script.calls(&[
        ("c1", "task", spawn_call("CHILD long", "长")),
        ("c2", "slow", json!({})),
    ]);
    world.script.reply("noted the late result");
    world
        .script
        .push_for("CHILD long", gated(&child_gate, "long result"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;

    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    let settled = world.harness.wait(&engine, id).await.unwrap();
    assert!(matches!(
        input_state(&settled),
        InputState::Unanswered { ref reason, .. } if reason == "aborted"
    ));
    let child = world.child().await;
    assert!(
        world.live_of(child).await.run.is_some(),
        "the child was not cancelled"
    );
    let anchor = world.tasks(ANCHOR_KIND).await.remove(0);
    assert!(!anchor.abort_requested);

    child_gate.notify_one();
    drop(slow_gate);
    world
        .until(|| async {
            outline(&world.messages().await)
                .contains(&"assistant: noted the late result".to_owned())
        })
        .await;
    assert!(
        user_texts(&world.messages().await)
            .last()
            .unwrap()
            .contains("long result")
    );
    engine.close().await.unwrap();
}

/// 显式取消子任务走引擎的 abort：子的 run 以 aborted 收尾，父收到一条"没有完成"。
#[tokio::test(start_paused = true)]
async fn aborting_the_child_reports_that_it_did_not_finish() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world
        .script
        .calls(&[("c1", "task", spawn_call("CHILD stuck", "卡住"))]);
    world.script.reply("dispatched");
    world.script.reply("it was cancelled");
    world.script.push_for("CHILD stuck", Step::Hang);
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let child = world.child().await;
    world
        .until(|| async { world.script.requests_for("CHILD stuck").len() == 1 })
        .await;

    world
        .harness
        .abort_conversation(&engine, child)
        .await
        .unwrap();
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: it was cancelled".to_owned())
        })
        .await;
    let text = user_texts(&world.messages().await).remove(1);
    assert!(
        text.starts_with(&format!("[后台任务 #{child}「卡住」没有完成]\n\n已取消")),
        "{text}"
    );
    assert!(world.live_of(child).await.run.is_none());
    engine.close().await.unwrap();
}

/// 查进度：只给 `task_id`，只读，不往子里提交任何东西；运行中与已完成都说得清。
#[tokio::test(start_paused = true)]
async fn a_status_query_reads_the_child_without_disturbing_it() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    let child_gate = Arc::new(Notify::new());
    world
        .script
        .calls(&[("c1", "task", spawn_call("CHILD slow work", "慢活"))]);
    world.script.reply("dispatched");
    world
        .script
        .push_for("CHILD slow work", gated(&child_gate, "AC is on"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let child = world.child().await;
    world
        .until(|| async { world.script.requests_for("CHILD slow work").len() == 1 })
        .await;
    let before = world.entries_in(child).await.len();

    world
        .script
        .calls(&[("s1", "task", json!({ "task_id": child.to_string() }))]);
    world.script.reply("it is running");
    let again = world.submit(&engine, "how is it?").await;
    world.harness.wait(&engine, again).await.unwrap();
    let running = result_text(&world.messages().await, "s1");
    assert!(
        running.starts_with(&format!("#{child}「慢活」：运行中")),
        "{running}"
    );
    assert_eq!(
        world.entries_in(child).await.len(),
        before,
        "nothing was submitted"
    );

    world.script.reply("noted the result");
    child_gate.notify_one();
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: noted the result".to_owned())
        })
        .await;
    world
        .script
        .calls(&[("s2", "task", json!({ "task_id": format!("#{child}") }))]);
    world.script.reply("it is done");
    let last = world.submit(&engine, "again?").await;
    world.harness.wait(&engine, last).await.unwrap();
    let done = result_text(&world.messages().await, "s2");
    assert!(
        done.contains("已完成") && done.contains("最终回复：AC is on"),
        "{done}"
    );
    engine.close().await.unwrap();
}

/// 输出存储的替身：第一次发布完之后让存储开始失败——"结果落盘了，结果 entry 没提交就崩了"。
struct CrashAfterPublish {
    inner: komo_kernel::test_support::MemOutputStore,
    crash: std::sync::Mutex<Option<Arc<RecordingStorage>>>,
}

#[async_trait::async_trait]
impl komo_kernel::traits::ToolOutputStore for CrashAfterPublish {
    async fn begin(
        &self,
        attempt: &komo_kernel::types::refs::AttemptRef,
    ) -> Result<Box<dyn komo_kernel::traits::OutputWriter>, komo_kernel::traits::StoreError> {
        self.inner.begin(attempt).await
    }

    async fn publish(
        &self,
        writer: Box<dyn komo_kernel::traits::OutputWriter>,
        result: komo_kernel::types::refs::ToolResultBody,
    ) -> Result<komo_kernel::types::refs::PublishedOutput, komo_kernel::traits::StoreError> {
        let published = self.inner.publish(writer, result).await;
        if let Some(storage) = self.crash.lock().unwrap().take() {
            storage
                .fail_commits
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        published
    }

    async fn open(
        &self,
        output: &komo_kernel::types::refs::OutputRef,
    ) -> Result<komo_kernel::types::refs::VerifiedOutput, komo_kernel::traits::StoreError> {
        self.inner.open(output).await
    }

    async fn interrupted(
        &self,
        attempt: &komo_kernel::types::refs::AttemptRef,
        tail_bytes: usize,
    ) -> Result<komo_kernel::types::refs::InterruptedOutput, komo_kernel::traits::StoreError> {
        self.inner.interrupted(attempt, tail_bytes).await
    }

    async fn find_published(
        &self,
        attempt: &komo_kernel::types::refs::AttemptRef,
    ) -> Result<Option<komo_kernel::types::refs::PublishedOutput>, komo_kernel::traits::StoreError>
    {
        self.inner.find_published(attempt).await
    }
}

/// 派出提交之后、结果 entry 之前崩溃：恢复从 `orchestrate` 接着落结果，**不会再派一遍**。
#[tokio::test(start_paused = true)]
async fn a_crash_between_the_spawn_and_its_result_does_not_spawn_twice() {
    let mem = komo_kernel::test_support::MemOutputStore::new();
    let outputs = Arc::new(CrashAfterPublish {
        inner: mem,
        crash: std::sync::Mutex::new(None),
    });
    let world = World::with_outputs(outputs.clone());
    let (_fixture, ..) = install(&world);
    world
        .script
        .calls(&[("c1", "task", spawn_call("CHILD once", "一次"))]);
    world.script.reply("dispatched");
    world.script.reply("noted");
    world.script.reply_for("CHILD once", "done");
    *outputs.crash.lock().unwrap() = Some(world.storage.clone());
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until(|| async { engine.failure().is_some() }).await;
    assert_eq!(world.children().await.len(), 1, "the spawn was committed");

    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();
    let child = world.child().await;
    assert_eq!(world.children().await, vec![child], "still one child");
    assert_eq!(world.tasks(ANCHOR_KIND).await.len(), 1);
    assert_eq!(
        result_text(&world.messages().await, "c1"),
        format!("已派出 #{child}：一次")
    );
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: noted".to_owned())
        })
        .await;
    assert_eq!(
        submissions_with(&world, CONV, "task-result:").await.len(),
        1
    );
    engine.close().await.unwrap();
}

// ------------------------------------------------------------ monitor

fn monitor_call(title: &str, resumable: bool, minutes: u64) -> serde_json::Value {
    json!({
        "command": "poll-status",
        "title": title,
        "timeout_minutes": minutes,
        "resume_after_restart": resumable,
    })
}

async fn monitors(world: &World) -> Vec<monitor::MonitorRecord> {
    monitor::list(&*world.storage_handle()).await.unwrap()
}

/// 登记在新存储里：谁挂的、命令、时限、截止、`resume_after_restart`；重启后照样读得出来。
#[tokio::test(start_paused = true)]
async fn a_monitor_is_registered_started_and_readable_after_a_restart() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world
        .script
        .calls(&[("m1", "monitor", monitor_call("盯日志", true, 30))]);
    world.script.reply("watching");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();

    let records = monitors(&world).await;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.state, MonitorState::Running);
    assert!(record.resume_after_restart);
    assert_eq!(record.conversation, CONV);
    assert_eq!(record.command, "poll-status");
    assert_eq!(record.deadline - record.started_at, 30 * 60_000);
    assert_eq!(
        result_text(&world.messages().await, "m1"),
        format!(
            "已开始监听 #{}：盯日志（最长 30 分钟）。事件会以 [监听 #{}…] 开头的消息交回。",
            record.id, record.id
        )
    );
    assert_eq!(
        world.orch.launches(),
        vec![(LaunchMode::Fresh, record.id, 30 * 60_000)]
    );

    let before = records.clone();
    let engine = world.restart(engine).await;
    assert_eq!(monitors(&world).await, before);
    engine.close().await.unwrap();
}

/// 起不来：登记收成 `ended{start_failed}`，结果是失败。
#[tokio::test(start_paused = true)]
async fn a_monitor_that_cannot_start_is_recorded_as_ended() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    *world.orch.fail_start.lock().unwrap() = Some("bwrap 不见了".into());
    world
        .script
        .calls(&[("m1", "monitor", monitor_call("x", false, 5))]);
    world.script.reply("sorry");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(
        result_text(&world.messages().await, "m1"),
        "挂不上监听：bwrap 不见了"
    );
    assert!(matches!(
        monitors(&world).await[0].state,
        MonitorState::Ended { ref reason, .. } if reason == "start_failed"
    ));
    engine.close().await.unwrap();
}

/// 登记之后、结果之前崩溃，进程还活着（同一进程里重开引擎）：恢复时认回，不起第二个。
#[tokio::test(start_paused = true)]
async fn a_restart_after_the_process_started_recognizes_it() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    let hold = Arc::new(Notify::new());
    *world.orch.start_gate.lock().unwrap() = Some(hold);
    world
        .script
        .calls(&[("m1", "monitor", monitor_call("盯", false, 30))]);
    world.script.reply("watching");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world
        .until(|| async { !world.orch.launches().is_empty() })
        .await;

    *world.orch.start_gate.lock().unwrap() = None;
    let engine = world.restart(engine).await;
    world.harness.wait(&engine, id).await.unwrap();
    let modes: Vec<_> = world.orch.launches().into_iter().map(|l| l.0).collect();
    assert_eq!(modes, [LaunchMode::Fresh, LaunchMode::Recover]);
    assert!(result_text(&world.messages().await, "m1").starts_with("已开始监听 #"));
    assert_eq!(monitors(&world).await[0].state, MonitorState::Running);
    engine.close().await.unwrap();
}

/// 登记之后、结果之前崩溃，进程没了（gateway 重启）：没声明 `resume_after_restart` 的判中断，
/// 不重跑（§8.6）；声明了的按原句柄重挂。
#[tokio::test(start_paused = true)]
async fn a_restart_with_the_process_gone_interrupts_unless_declared_resumable() {
    for resumable in [false, true] {
        let world = World::new();
        let (_fixture, ..) = install(&world);
        *world.orch.start_gate.lock().unwrap() = Some(Arc::new(Notify::new()));
        world
            .script
            .calls(&[("m1", "monitor", monitor_call("盯", resumable, 30))]);
        world.script.reply("ok");
        let engine = world.open().await;
        let id = world.submit(&engine, "go").await;
        world
            .until(|| async { !world.orch.launches().is_empty() })
            .await;

        *world.orch.start_gate.lock().unwrap() = None;
        world.orch.forget_processes();
        let engine = world.restart(engine).await;
        world.harness.wait(&engine, id).await.unwrap();
        let text = result_text(&world.messages().await, "m1");
        let modes: Vec<_> = world.orch.launches().into_iter().map(|l| l.0).collect();
        let record = monitors(&world).await.remove(0);
        if resumable {
            assert_eq!(
                modes,
                [LaunchMode::Fresh, LaunchMode::Recover, LaunchMode::Resume]
            );
            assert!(
                text.contains("已开始监听") && text.contains("已按原句柄重挂"),
                "{text}"
            );
            assert_eq!(record.state, MonitorState::Running);
        } else {
            assert_eq!(modes, [LaunchMode::Fresh, LaunchMode::Recover]);
            assert!(text.contains("中断了，没有自动重挂"), "{text}");
            assert!(matches!(
                record.state,
                MonitorState::Ended { ref reason, .. } if reason == "interrupted"
            ));
        }
        engine.close().await.unwrap();
    }
}

/// `monitor_stop`：先登记停掉、再让进程停；已经停掉的不算失败；短号对不上时列出这个会话在跑的。
#[tokio::test(start_paused = true)]
async fn stopping_a_monitor_records_it_and_stopping_twice_is_not_an_error() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world
        .script
        .calls(&[("m1", "monitor", monitor_call("盯", false, 30))]);
    world.script.reply("watching");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let monitor_id = monitors(&world).await[0].id;

    world.script.calls(&[
        ("s0", "monitor_stop", json!({ "monitor_id": "9999" })),
        (
            "s1",
            "monitor_stop",
            json!({ "monitor_id": format!("#{monitor_id}") }),
        ),
        (
            "s2",
            "monitor_stop",
            json!({ "monitor_id": monitor_id.to_string() }),
        ),
        ("s3", "monitor_stop", json!({ "monitor_id": "9999" })),
    ]);
    world.script.reply("stopped");
    let again = world.submit(&engine, "stop it").await;
    world.harness.wait(&engine, again).await.unwrap();
    let messages = world.messages().await;
    // s0 在停之前：列出在跑的；s3 在停之后：没有在跑的了。
    assert_eq!(
        result_text(&messages, "s0"),
        format!("停不了：#9999 不是这个会话挂着的监听；这个会话在跑的：#{monitor_id}「盯」")
    );
    assert_eq!(
        result_text(&messages, "s1"),
        format!("已停止监听 #{monitor_id}：盯")
    );
    assert_eq!(
        result_text(&messages, "s2"),
        format!("不用停了：监听 #{monitor_id}「盯」已经停掉了")
    );
    assert_eq!(
        result_text(&messages, "s3"),
        "停不了：#9999 不是这个会话挂着的监听；这个会话没有在跑的监听"
    );
    assert_eq!(
        world.orch.stops(),
        vec![monitor_id],
        "only the real stop reached the process"
    );
    assert!(matches!(
        monitors(&world).await[0].state,
        MonitorState::Ended { ref reason, .. } if reason == "stopped"
    ));
    engine.close().await.unwrap();
}

/// 结束消息与登记状态同一次提交：只有第一个结束它的人赢；停掉之后再 `finish` 是空操作；事件按
/// 请求键幂等；结束后 `monitor_stop` 答"不用停了"并说清怎么结束的。
#[tokio::test(start_paused = true)]
async fn a_finished_monitor_hands_back_exactly_one_end_message() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world
        .script
        .calls(&[("m1", "monitor", monitor_call("盯", false, 30))]);
    world.script.reply("watching");
    world.script.reply("saw the event");
    world.script.reply("saw the end");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let monitor_id = monitors(&world).await[0].id;

    // 事件：同一个请求键再来一次拿回原来那条。
    let draft = || monitor::event_draft(monitor_id, "盯", "1:1", "[监听 #x「盯」]\nline");
    let event = world.harness.submit(&engine, CONV, draft()).await.unwrap();
    assert_eq!(
        world.harness.submit(&engine, CONV, draft()).await.unwrap(),
        event
    );
    world.harness.wait(&engine, event).await.unwrap();

    let message = format!("[监听 #{monitor_id}「盯」已结束]\n\nexit=0");
    assert!(
        world
            .harness
            .finish_monitor(
                &engine,
                monitor_id,
                "exit",
                Some("已结束：脚本退出（exit=0）"),
                message.clone()
            )
            .await
            .unwrap()
    );
    assert!(
        !world
            .harness
            .finish_monitor(&engine, monitor_id, "exit", None, message.clone())
            .await
            .unwrap(),
        "the second finish is a no-op"
    );
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: saw the end".to_owned())
        })
        .await;
    assert_eq!(
        submissions_with(&world, CONV, &format!("monitor-end:{monitor_id}"))
            .await
            .len(),
        1
    );
    assert!(user_texts(&world.messages().await).contains(&message));

    world.script.calls(&[(
        "s1",
        "monitor_stop",
        json!({ "monitor_id": monitor_id.to_string() }),
    )]);
    world.script.reply("it had ended");
    let last = world.submit(&engine, "stop").await;
    world.harness.wait(&engine, last).await.unwrap();
    assert_eq!(
        result_text(&world.messages().await, "s1"),
        format!("不用停了：监听 #{monitor_id}「盯」已结束：脚本退出（exit=0）")
    );
    assert!(world.orch.stops().is_empty(), "nothing to stop");
    engine.close().await.unwrap();
}

/// gateway 接手的、不是 `monitor` 挂的进程（`bash` 转后台，`docs/monitor.md` §8）：`register_monitor`
/// 登记之后，`monitor_stop` 停得掉它，结束消息与登记同一次提交，重启对账把没结束的判为中断。
#[tokio::test(start_paused = true)]
async fn a_registered_foreign_process_is_stoppable_finishable_and_reconciled() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    let engine = world.open().await;
    let record = |id: u64, title: &str| monitor::MonitorRecord {
        id,
        conversation: CONV,
        title: title.into(),
        command: "sleep 600".into(),
        cwd: ".".into(),
        timeout_minutes: 0,
        resume_after_restart: false,
        started_at: 0,
        deadline: 0,
        state: MonitorState::Running,
    };
    for (id, title) in [(9001, "转后台甲"), (9002, "转后台乙")] {
        world
            .harness
            .register_monitor(&engine, record(id, title))
            .await
            .unwrap();
    }
    assert_eq!(monitors(&world).await.len(), 2);

    // `monitor_stop` 认得它们，也真的把进程交给了 gateway 去停。
    world
        .script
        .calls(&[("s1", "monitor_stop", json!({ "monitor_id": "9001" }))]);
    world.script.reply("stopped");
    let stop = world.submit(&engine, "stop it").await;
    world.harness.wait(&engine, stop).await.unwrap();
    assert_eq!(
        result_text(&world.messages().await, "s1"),
        "已停止监听 #9001：转后台甲"
    );
    assert_eq!(world.orch.stops(), vec![9001]);

    // 另一条自己结束：结束消息交回一次，再结束是空操作。
    let message = "[后台命令 #9002「转后台乙」结束] 退出码 0".to_owned();
    world.script.reply("saw the end");
    assert!(
        world
            .harness
            .finish_monitor(&engine, 9002, "exit", Some("退出码 0"), message.clone())
            .await
            .unwrap()
    );
    assert!(
        !world
            .harness
            .finish_monitor(&engine, 9002, "exit", None, message)
            .await
            .unwrap()
    );

    // 没结束的重启之后判为中断，不重挂。
    world
        .harness
        .register_monitor(&engine, record(9003, "转后台丙"))
        .await
        .unwrap();
    world.script.reply("saw the interruption");
    let engine = world.restart(engine).await;
    let done = world.harness.reconcile_monitors(&engine).await.unwrap();
    assert_eq!(done, vec![Reconciled::Ended(9003, "interrupted".into())]);
    engine.close().await.unwrap();
}

/// 重启后对账（`docs/monitor.md` §5）：声明了 `resume_after_restart` 且还有 ≥1 分钟的重挂（同一个
/// id、时限接着算），没声明的交回"已中断"，临近截止的交回"时限已经到了"；再对一次不会重复。
#[tokio::test(start_paused = true)]
async fn reconciling_after_a_restart_resumes_the_declared_and_interrupts_the_rest() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world.script.calls(&[
        ("m1", "monitor", monitor_call("可重挂", true, 30)),
        ("m2", "monitor", monitor_call("不可重挂", false, 30)),
        ("m3", "monitor", monitor_call("快到期", true, 1)),
    ]);
    world.script.reply("three watchers");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let ids: Vec<u64> = monitors(&world).await.iter().map(|r| r.id).collect();
    assert_eq!(ids.len(), 3);
    // 之后的请求都卡住：这里只看登记与交回，不关心父怎么答。
    for _ in 0..4 {
        world.script.push(Step::Hang);
    }
    tokio::time::sleep(Duration::from_secs(20)).await;

    let engine = world.restart(engine).await;
    world.orch.forget_processes();
    let done = world.harness.reconcile_monitors(&engine).await.unwrap();
    assert_eq!(
        done,
        vec![
            Reconciled::Resumed(ids[0]),
            Reconciled::Ended(ids[1], "interrupted".into()),
            Reconciled::Ended(ids[2], "timeout".into()),
        ]
    );
    let resume = world
        .orch
        .launches()
        .into_iter()
        .find(|launch| launch.0 == LaunchMode::Resume)
        .unwrap();
    assert_eq!(resume.1, ids[0]);
    assert_eq!(
        resume.2,
        30 * 60_000 - 20_000,
        "the remaining time carries on"
    );
    let states: Vec<_> = monitors(&world)
        .await
        .into_iter()
        .map(|r| r.state)
        .collect();
    assert_eq!(states[0], MonitorState::Running);
    assert!(matches!(&states[1], MonitorState::Ended { reason, .. } if reason == "interrupted"));
    assert!(matches!(&states[2], MonitorState::Ended { reason, .. } if reason == "timeout"));
    for ended in [ids[1], ids[2]] {
        assert_eq!(
            submissions_with(&world, CONV, &format!("monitor-end:{ended}"))
                .await
                .len(),
            1
        );
    }
    // 再对一次：只剩还在跑的那个被再问一遍，已结束的不会再交回。
    let again = world.harness.reconcile_monitors(&engine).await.unwrap();
    assert_eq!(again, vec![Reconciled::Resumed(ids[0])]);
    assert_eq!(
        submissions_with(&world, CONV, "monitor-end:").await.len(),
        2
    );
    engine.close().await.unwrap();
}

/// 任务会话里别挂监听：任务这一轮一结束结果就交回去了。
#[tokio::test(start_paused = true)]
async fn a_task_conversation_cannot_start_a_monitor() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world
        .script
        .calls(&[("c1", "task", spawn_call("CHILD watch", "盯盘"))]);
    world.script.reply("dispatched");
    world.script.reply("noted");
    world.script.calls_for(
        "CHILD watch",
        &[("k1", "monitor", monitor_call("x", false, 5))],
    );
    world.script.reply_for("CHILD watch", "never mind");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let child = world.child().await;
    world
        .until(|| async {
            outline(&world.messages_in(child).await)
                .last()
                .is_some_and(|l| l == "assistant: never mind")
        })
        .await;
    assert!(
        result_text(&world.messages_in(child).await, "k1").contains("后台任务会话里不能挂监听")
    );
    assert!(monitors(&world).await.is_empty());
    assert!(world.orch.launches().is_empty());
    engine.close().await.unwrap();
}

// ------------------------------------------------------------ 取消与失败不丢内部输入

/// 父忙时排着的任务结果是内部输入：取消父不撤它。父的 run 一收尾就在同一次提交里用它起后继 run，
/// 模型看到 `task.result` 并把结论转告用户（旧 komo 取消 home 的 Run 不动任务结果；
/// `docs/background-tasks.md`）。
#[tokio::test(start_paused = true)]
async fn a_task_result_queued_for_a_busy_parent_survives_cancelling_the_parent() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world.script.calls(&[
        ("c1", "task", spawn_call("CHILD quick", "快")),
        ("c2", "slow", json!({})),
    ]);
    world.script.reply("told the user");
    world.script.reply_for("CHILD quick", "quick answer");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world
        .until(|| async { !world.inbox().await.items.is_empty() })
        .await;
    let queued = world.inbox().await.items[0].id();

    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    let settled = world.harness.wait(&engine, id).await.unwrap();
    assert!(matches!(
        input_state(&settled),
        InputState::Unanswered { ref reason, .. } if reason == "aborted"
    ));
    let delivered = world.harness.wait(&engine, queued).await.unwrap();

    assert!(matches!(input_state(&delivered), InputState::Done { .. }));
    assert!(world.inbox().await.items.is_empty());
    let entries = world.entries_of(TASK_RESULT_ENTRY).await;
    assert_eq!(entries.len(), 1, "the result became a task.result input");
    let last = world.script.requests().last().unwrap().clone();
    assert!(
        user_texts(&last).last().unwrap().contains("quick answer"),
        "the model saw the task result"
    );
    assert_eq!(
        outline(&world.messages().await).last().unwrap(),
        "assistant: told the user"
    );
    engine.close().await.unwrap();
}

/// 父的 run 模型失败（`model_error`）时排着的任务结果也紧接着跑，不卡到用户下次说话。
#[tokio::test(start_paused = true)]
async fn a_task_result_queued_for_a_parent_whose_model_fails_runs_right_after() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    let (model_gate, child_gate) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    world
        .script
        .calls(&[("c1", "task", spawn_call("CHILD slow", "慢"))]);
    world.script.push(Step::Gate(
        model_gate.clone(),
        Box::new(Step::Error(rejected(400))),
    ));
    world.script.reply("told the user");
    world
        .script
        .push_for("CHILD slow", gated(&child_gate, "late answer"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    // 父的第二次请求卡在 `model_gate` 上（父忙），这时子收尾：交回的结果排进收件箱。
    world
        .until(|| async { world.script.request_count() >= 2 })
        .await;
    child_gate.notify_one();
    world
        .until(|| async { !world.inbox().await.items.is_empty() })
        .await;
    let queued = world.inbox().await.items[0].id();

    model_gate.notify_one();
    let failed = world.harness.wait(&engine, id).await.unwrap();
    assert!(matches!(
        input_state(&failed),
        InputState::Unanswered { ref reason, .. } if reason == "model_error"
    ));
    let delivered = world.harness.wait(&engine, queued).await.unwrap();

    assert!(matches!(input_state(&delivered), InputState::Done { .. }));
    assert!(
        user_texts(&world.messages().await)
            .last()
            .unwrap()
            .contains("late answer")
    );
    assert_eq!(
        outline(&world.messages().await).last().unwrap(),
        "assistant: told the user"
    );
    engine.close().await.unwrap();
}

/// 监听的事件与结束消息同样是内部输入：父被取消后仍送达，父再跑一轮。
#[tokio::test(start_paused = true)]
async fn monitor_messages_queued_for_a_busy_parent_survive_cancelling_it() {
    let world = World::new();
    let (_fixture, ..) = install(&world);
    world.script.calls(&[
        ("m1", "monitor", monitor_call("盯", false, 30)),
        ("c2", "slow", json!({})),
    ]);
    world.script.reply("told the user");
    world.script.reply("and the rest");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.until_log("start slow #1").await;
    let monitor_id = monitors(&world).await[0].id;

    let event = world
        .harness
        .submit(
            &engine,
            CONV,
            monitor::event_draft(monitor_id, "盯", "1:1", "[监听 #x「盯」]\nline"),
        )
        .await
        .unwrap();
    let message = format!("[监听 #{monitor_id}「盯」已结束]\n\nexit=0");
    assert!(
        world
            .harness
            .finish_monitor(&engine, monitor_id, "exit", None, message.clone())
            .await
            .unwrap()
    );
    let end = submissions_with(&world, CONV, "monitor-end:")
        .await
        .remove(0)
        .id;
    assert_eq!(world.inbox().await.items.len(), 2);

    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    world.harness.wait(&engine, id).await.unwrap();
    for submission in [event, end] {
        let settled = world.harness.wait(&engine, submission).await.unwrap();
        assert!(
            matches!(input_state(&settled), InputState::Done { .. }),
            "{settled:?}"
        );
    }
    let texts = user_texts(&world.messages().await);
    assert!(texts.contains(&message), "{texts:?}");
    assert!(texts.iter().any(|text| text.contains("line")), "{texts:?}");
    assert_eq!(world.entries_of("monitor.event").await.len(), 1);
    assert_eq!(world.entries_of("monitor.end").await.len(), 1);
    assert!(world.inbox().await.items.is_empty());
    engine.close().await.unwrap();
}

// ------------------------------------------------------------ Turso 文件库

/// 派出 → 子 run → 交回，全在 Turso 的提交线上：锚任务 / 子 conversation / 第一条输入同一次提交，
/// 交回与子 run 的收尾同一次提交；关库重开之后一切都在，也不会补交第二次。
#[tokio::test]
async fn a_dispatched_task_reports_back_once_on_a_turso_database() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let (_fixture, ..) = install(&world);
    let child_gate = Arc::new(Notify::new());
    world
        .script
        .calls(&[("c1", "task", spawn_call("CHILD on disk", "落盘"))]);
    world.script.reply("dispatched");
    world.script.reply("got it");
    world
        .script
        .push_for("CHILD on disk", gated(&child_gate, "stored"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let child = world.child().await;
    assert!(world.live_of(child).await.run.is_some());

    child_gate.notify_one();
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: got it".to_owned())
        })
        .await;
    let delivered = submissions_with(&world, CONV, &format!("task-result:{child}:")).await;
    assert_eq!(delivered.len(), 1);

    let engine = world.restart(engine).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(world.children().await, vec![child]);
    assert_eq!(
        submissions_with(&world, CONV, "task-result:").await.len(),
        1
    );
    assert_eq!(world.entries_of(TASK_RESULT_ENTRY).await.len(), 1);
    assert_eq!(
        outline(&world.messages_in(child).await),
        ["system", "user: CHILD on disk", "assistant: stored"]
    );
    engine.close().await.unwrap();
}

/// 取消父之后"重启"：排着的任务结果没丢——取消提交里它已经成了后继 run 的输入，重启后那条 run 照常
/// 接着跑（重放请求），父把结论转告用户；结果不会补交第二次。
#[tokio::test]
async fn a_task_result_kept_through_a_cancel_runs_on_after_reopening_a_turso_database() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let (_fixture, ..) = install(&world);
    let stuck = Arc::new(Notify::new());
    world.script.calls(&[
        ("c1", "task", spawn_call("CHILD on disk", "落盘")),
        ("c2", "slow", json!({})),
    ]);
    // 后继 run 的请求卡住，"重启"落在它进行中；重启后重放的请求拿下一步。
    world
        .script
        .push(Step::Gate(stuck.clone(), Box::new(Step::Hang)));
    world.script.reply("told the user");
    world.script.reply_for("CHILD on disk", "stored");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world
        .until(|| async { !world.inbox().await.items.is_empty() })
        .await;
    let queued = world.inbox().await.items[0].id();

    world
        .harness
        .abort_conversation(&engine, CONV)
        .await
        .unwrap();
    assert!(matches!(
        input_state(&world.harness.wait(&engine, id).await.unwrap()),
        InputState::Unanswered { ref reason, .. } if reason == "aborted"
    ));
    // 取消之后：结果是后继 run 的输入（已放置），收件箱空了。
    assert!(world.inbox().await.items.is_empty());
    assert!(matches!(
        input_state(&world.submission(queued).await),
        InputState::Placed { .. }
    ));
    assert!(world.live().await.run.is_some());

    let engine = world.restart(engine).await;
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: told the user".to_owned())
        })
        .await;
    let delivered = world.harness.wait(&engine, queued).await.unwrap();
    assert!(matches!(input_state(&delivered), InputState::Done { .. }));
    assert_eq!(world.entries_of(TASK_RESULT_ENTRY).await.len(), 1);
    assert_eq!(
        submissions_with(&world, CONV, "task-result:").await.len(),
        1
    );
    assert!(world.live().await.run.is_none());
    engine.close().await.unwrap();
}

/// fork 与接着做在 Turso 上：子 conversation 是父的 fork（沿父链读），补的结果只在子里；接着做
/// 往同一个子里提交，结果各交回一次。
#[tokio::test]
async fn a_fork_and_a_follow_up_work_on_a_turso_database() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let (_fixture, ..) = install(&world);
    let (first, second) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    world.script.calls(&[
        ("c0", "read", json!({})),
        (
            "c1",
            "task",
            json!({ "prompt": "CHILD forked", "title": "分叉", "context": "fork" }),
        ),
    ]);
    world.script.reply("dispatched");
    world.script.push_for("CHILD forked", gated(&first, "one"));
    world.script.push_for("MORE", gated(&second, "two"));
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let child = world.child().await;
    let seen = world.messages_in(child).await;
    assert_eq!(
        outline(&seen)[..5],
        [
            "system",
            "user: go",
            "assistant calls read(c0),task(c1)",
            "result(c0)",
            "result(c1)",
        ]
    );
    assert!(result_text(&seen, "c1").contains("派出的就是你（fork）"));
    let record = world
        .storage_handle()
        .conversation(child)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.parent.unwrap().conversation, CONV);
    assert!(record.owner.is_some());

    world.script.calls(&[(
        "c2",
        "task",
        json!({ "task_id": child.to_string(), "prompt": "MORE" }),
    )]);
    world.script.reply("forwarded");
    first.notify_one();
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: forwarded".to_owned())
        })
        .await;
    world.script.reply("all done");
    second.notify_one();
    world
        .until(|| async {
            outline(&world.messages().await).contains(&"assistant: all done".to_owned())
        })
        .await;
    assert_eq!(
        submissions_with(&world, CONV, &format!("task-result:{child}:"))
            .await
            .len(),
        2
    );
    engine.close().await.unwrap();
}

/// 监听的登记在 Turso 上：挂上、关库重开仍读得出 `resume_after_restart` 与截止；对账按它决定
/// 重挂还是交回"已中断"。
#[tokio::test]
async fn the_monitor_registry_survives_reopening_a_turso_database() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::turso(dir.path());
    let (_fixture, ..) = install(&world);
    world.script.calls(&[
        ("m1", "monitor", monitor_call("可重挂", true, 30)),
        ("m2", "monitor", monitor_call("不可重挂", false, 30)),
    ]);
    world.script.reply("two watchers");
    let engine = world.open().await;
    let id = world.submit(&engine, "go").await;
    world.harness.wait(&engine, id).await.unwrap();
    let before = monitors(&world).await;
    assert_eq!(before.len(), 2);

    let engine = world.restart(engine).await;
    world.orch.forget_processes();
    assert_eq!(monitors(&world).await, before);
    // 之后父的请求卡住：只看登记与交回。
    world.script.push(Step::Hang);
    let done = world.harness.reconcile_monitors(&engine).await.unwrap();
    assert_eq!(
        done,
        vec![
            Reconciled::Resumed(before[0].id),
            Reconciled::Ended(before[1].id, "interrupted".into()),
        ]
    );
    assert_eq!(
        submissions_with(&world, CONV, &format!("monitor-end:{}", before[1].id))
            .await
            .len(),
        1
    );
    engine.close().await.unwrap();
}
