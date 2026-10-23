//! 所有权与结构化并发（spec §5.4、§5.5）：`completing`、取消意图级联、后台边界、自有
//! conversation。对应 pi `test/harness-ownership.test.ts`。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::durable::task::NextTaskState;
use komo_kernel::durable::{
    DocumentAddress, DocumentScope, ROOT_CONVERSATION_ID, Storage, TaskId, TaskState,
};
use serde_json::json;

use super::super::{ConversationOwnership, Ownership, TaskInspectionState, TaskOptions};
use super::support::*;

/// 睡一小时（听信号）；被取消时记 "aborted <name>" 并写 aborted。
fn long_sleeper(env: &Env, log: &Log, name: &'static str) -> Arc<Kind> {
    let clock = env.clock.clone();
    let log = log.clone();
    kind(name)
        .phase("start", move |_task, rt| {
            let until = clock.now_ms() + 3_600_000;
            async move {
                rt.sleep_until(until).await?;
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .on_abort(move |_task, rt| {
            let log = log.clone();
            async move {
                log.push(format!("aborted {name}"));
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        })
        .build()
}

fn running(engine: &super::super::Engine, id: TaskId) -> bool {
    engine
        .inspect()
        .iter()
        .any(|t| t.record.id == id && t.state == TaskInspectionState::Running)
}

fn task_doc(task: TaskId) -> DocumentAddress {
    DocumentAddress {
        kind: "test.scratch".into(),
        scope: DocumentScope::Task { task },
        key: None,
    }
}

/// 创建一个子任务后在 `finish` 里按 `input.end` 结束自己，结束的那笔提交还写一个任务文档。
fn owner(child: Arc<Kind>, with_children: usize) -> Arc<Kind> {
    kind("owner")
        .phase("start", move |task, rt| {
            let child = child.clone();
            async move {
                let me = task.id();
                rt.commit(move |tx, _| {
                    Box::pin(async move {
                        for _ in 0..with_children {
                            tx.create_task(
                                &*child,
                                json!({}),
                                Ownership::Task(me),
                                TaskOptions::default(),
                            )
                            .await?;
                        }
                        Ok(Some(go("finish")))
                    })
                })
                .await?;
                Ok(())
            }
        })
        .phase("finish", |task, rt| async move {
            let me = task.id();
            let end = match task.input()["end"].as_str() {
                Some("failed") => failed("owner failed"),
                _ => done(json!("owner done")),
            };
            rt.commit(move |tx, _| {
                Box::pin(async move {
                    tx.put_document(task_doc(me), 1, json!({"note": "final"}))
                        .await?;
                    Ok(Some(end))
                })
            })
            .await?;
            Ok(())
        })
        .build()
}

/// pi: "keeps a conversation busy while its owned foreground subtree has live work, holding the completed
/// owner"（spec §5.5 规则 1、4、5）：`completed` 结局在名下有活工作时先 `completing`，工作正常结束后才
/// 写最终 terminal；任务文档随最终提交退役；`completed` 不级联取消。
#[tokio::test(start_paused = true)]
async fn a_completed_owner_is_held_as_completing_until_its_child_finishes() {
    let env = Env::new();
    let log = Log::default();
    let child = long_sleeper(&env, &log, "child");
    let owner = owner(child.clone(), 1);
    env.install(&child);
    env.install(&owner);
    let engine = env.open_running().await;
    let id = spawn(&engine, &owner, json!({"end": "done"})).await;
    until(|| matches!(futures_util::FutureExt::now_or_never(env.storage.task(id)), Some(Ok(Some(t))) if matches!(t.state, TaskState::Completing { .. }))).await;
    let held = record(&env, id).await;
    assert_eq!(status(outcome_of(&held)), "completed");
    assert!(
        env.storage
            .find_document(&task_doc(id))
            .await
            .unwrap()
            .is_some(),
        "the document retires with the final commit, not the hold"
    );
    assert!(held.ended_at.is_none());
    assert!(
        log.lines().is_empty(),
        "a held `completed` outcome does not cancel the work below"
    );
    // 孩子是活的普通工作：会话不空闲。
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            engine.wait_for_idle(Some(ROOT_CONVERSATION_ID))
        )
        .await
        .is_err()
    );
    // 孩子睡的是一小时；快进之后它正常完成，所有者随即终结。
    let task = settle(&engine, id).await;
    assert!(matches!(task.state, TaskState::Terminal { .. }));
    assert_eq!(status(outcome_of(&task)), "completed");
    assert!(
        env.storage
            .find_document(&task_doc(id))
            .await
            .unwrap()
            .is_none()
    );
    engine
        .wait_for_idle(Some(ROOT_CONVERSATION_ID))
        .await
        .unwrap();
    engine.close().await.unwrap();
}

/// pi: "cascades a failed owner but not a completed one"（spec §5.4/§5.5 规则 3）：握着的失败结局是
/// 取消意图，名下的工作被 abort，之后所有者才 terminal。
#[tokio::test(start_paused = true)]
async fn a_held_failure_aborts_the_work_below_before_the_owner_becomes_terminal() {
    let env = Env::new();
    let log = Log::default();
    let child = long_sleeper(&env, &log, "child");
    let owner = owner(child.clone(), 2);
    env.install(&child);
    env.install(&owner);
    let engine = env.open_running().await;
    let id = spawn(&engine, &owner, json!({"end": "failed"})).await;
    let task = settle(&engine, id).await;
    assert_eq!(status(outcome_of(&task)), "failed");
    assert_eq!(log.lines(), ["aborted child", "aborted child"]);
    assert!(engine.inspect().is_empty(), "nothing is left live");
    engine.close().await.unwrap();
}

/// pi: "rejects a conversation created in the commit that ends its owner task" 与 spec §5.5："a task
/// cannot create owned work in the commit that finishes it"。
#[tokio::test(start_paused = true)]
async fn owned_work_cannot_be_created_in_the_commit_that_finishes_its_owner() {
    let env = Env::new();
    let log = Log::default();
    let child = long_sleeper(&env, &log, "child");
    let probe_log = log.clone();
    let probe_child = child.clone();
    let probe = kind("probe")
        .phase("start", move |task, rt| {
            let (log, child) = (probe_log.clone(), probe_child.clone());
            async move {
                let me = task.id();
                let rejected = rt
                    .commit(move |tx, _| {
                        Box::pin(async move {
                            tx.create_task(
                                &*child,
                                json!({}),
                                Ownership::Task(me),
                                TaskOptions::default(),
                            )
                            .await?;
                            Ok(Some(done(json!(1))))
                        })
                    })
                    .await;
                log.push(format!("task: {}", rejected.unwrap_err()));
                let rejected = rt
                    .commit(move |tx, _| {
                        Box::pin(async move {
                            tx.create_conversation(ConversationOwnership::Task(me))
                                .await?;
                            Ok(Some(done(json!(1))))
                        })
                    })
                    .await;
                log.push(format!("conversation: {}", rejected.unwrap_err()));
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .build();
    env.install(&child);
    env.install(&probe);
    let engine = env.open_running().await;
    let id = spawn(&engine, &probe, json!({})).await;
    settle(&engine, id).await;
    let lines: Vec<String> = log.lines().iter().map(|l| l.to_lowercase()).collect();
    assert!(lines[0].contains("completing"), "{lines:?}");
    assert!(lines[1].contains("terminal"), "{lines:?}");
    assert!(
        engine.inspect().is_empty(),
        "the rejected commits left no child behind"
    );
    engine.close().await.unwrap();
}

/// pi: "aborts a conversation: ... foreground work aborted, background kept"、"cascades through ended
/// tasks into an owned background subtree when aborting across background"：
/// `abort_conversation` 默认不跨后台边界；`background: true` 才穿过。
#[tokio::test(start_paused = true)]
async fn conversation_abort_stops_foreground_work_and_crosses_background_only_when_asked() {
    let env = Env::new();
    let log = Log::default();
    let foreground = long_sleeper(&env, &log, "foreground");
    let background = long_sleeper(&env, &log, "background");
    env.install(&foreground);
    env.install(&background);
    let engine = env.open_running().await;
    let fg = spawn(&engine, &foreground, json!({})).await;
    let bg = spawn_background(&engine, &background, json!({})).await;
    until(|| running(&engine, fg) && running(&engine, bg)).await;

    engine
        .abort_conversation(ROOT_CONVERSATION_ID, false)
        .await
        .unwrap();
    assert_eq!(status(outcome_of(&record(&env, fg).await)), "aborted");
    assert_eq!(log.lines(), ["aborted foreground"]);
    assert!(running(&engine, bg), "background work is a boundary");

    engine
        .abort_conversation(ROOT_CONVERSATION_ID, true)
        .await
        .unwrap();
    assert_eq!(status(outcome_of(&record(&env, bg).await)), "aborted");
    assert_eq!(log.lines(), ["aborted foreground", "aborted background"]);
    engine.close().await.unwrap();
}

/// pi: "waits for Harness and conversation idleness, counting blocked work and ignoring background tasks"
/// 与 "stops idle traversal at a background owner"。
#[tokio::test(start_paused = true)]
async fn idle_ignores_background_tasks_but_waits_for_foreground_ones() {
    let env = Env::new();
    let log = Log::default();
    let long = long_sleeper(&env, &log, "long");
    env.install(&long);
    let engine = env.open_running().await;
    let bg = spawn_background(&engine, &long, json!({})).await;
    until(|| running(&engine, bg)).await;
    engine.wait_for_idle(None).await.unwrap();
    engine
        .wait_for_idle(Some(ROOT_CONVERSATION_ID))
        .await
        .unwrap();
    let fg = spawn(&engine, &long, json!({})).await;
    until(|| running(&engine, fg)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(10), engine.wait_for_idle(None))
            .await
            .is_err()
    );
    engine.abort_task(fg).await.unwrap();
    engine.wait_for_idle(None).await.unwrap();
    assert!(running(&engine, bg), "the background task is still live");
    engine.close().await.unwrap();
}

/// pi: "aborts a background task directly with its ordinary subtree"：直接 abort 后台任务会带上它名下
/// 的普通子树，子先于父。
#[tokio::test(start_paused = true)]
async fn aborting_a_background_task_directly_includes_its_ordinary_subtree_bottom_up() {
    let env = Env::new();
    let log = Log::default();
    let child = long_sleeper(&env, &log, "child");
    let parent_log = log.clone();
    let parent_child = child.clone();
    let parent = kind("parent")
        .phase("start", move |task, rt| {
            let child = parent_child.clone();
            async move {
                let me = task.id();
                rt.commit(move |tx, _| {
                    Box::pin(async move {
                        let id = tx
                            .create_task(
                                &*child,
                                json!({}),
                                Ownership::Task(me),
                                TaskOptions::default(),
                            )
                            .await?;
                        Ok(Some(NextTaskState::Waiting {
                            checkpoint: json!({"phase": "after"}),
                            on: vec![id],
                            policy: komo_kernel::durable::records::JoinPolicy::AllSettled,
                        }))
                    })
                })
                .await?;
                Ok(())
            }
        })
        .phase("after", |_task, rt| async move {
            rt.commit_next(done(json!(1))).await?;
            Ok(())
        })
        .on_abort(move |_task, rt| {
            let log = parent_log.clone();
            async move {
                log.push("aborted parent");
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        })
        .build();
    env.install(&child);
    env.install(&parent);
    let engine = env.open_running().await;
    let id = spawn_background(&engine, &parent, json!({})).await;
    until(|| engine.inspect().len() == 2).await;
    until(|| {
        engine
            .inspect()
            .iter()
            .any(|t| t.record.owner.is_some() && t.state == TaskInspectionState::Running)
    })
    .await;
    engine.abort_task(id).await.unwrap();
    settle(&engine, id).await;
    assert_eq!(log.lines(), ["aborted child", "aborted parent"]);
    engine.close().await.unwrap();
}

/// pi: "reaches work below ended tasks across two owned conversations" 的核心：任务拥有的 conversation 里
/// 的工作随所有者被取消，先于所有者的 abort handler。
#[tokio::test(start_paused = true)]
async fn work_in_an_owned_conversation_is_aborted_with_its_owner_first() {
    let env = Env::new();
    let log = Log::default();
    let inner = long_sleeper(&env, &log, "inner");
    let supervisor_log = log.clone();
    let supervisor_inner = inner.clone();
    let supervisor = kind("supervisor")
        .phase("start", move |task, rt| {
            let inner = supervisor_inner.clone();
            async move {
                let me = task.id();
                rt.commit(move |tx, _| {
                    Box::pin(async move {
                        let conversation = tx
                            .create_conversation(ConversationOwnership::Task(me))
                            .await?;
                        tx.create_task(
                            &*inner,
                            json!({}),
                            Ownership::Conversation,
                            TaskOptions {
                                conversation: Some(conversation.id),
                                ..TaskOptions::default()
                            },
                        )
                        .await?;
                        Ok(Some(go("watch")))
                    })
                })
                .await?;
                Ok(())
            }
        })
        .phase("watch", |_task, rt| async move {
            let signal = rt.cancelled();
            signal.cancelled().await;
            Err("signalled".into())
        })
        .on_abort(move |_task, rt| {
            let log = supervisor_log.clone();
            async move {
                log.push("aborted supervisor");
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        })
        .build();
    env.install(&inner);
    env.install(&supervisor);
    let engine = env.open_running().await;
    let id = spawn(&engine, &supervisor, json!({})).await;
    until(|| {
        engine
            .inspect()
            .iter()
            .any(|t| t.record.kind == "inner" && t.state == TaskInspectionState::Running)
    })
    .await;
    engine.abort_task(id).await.unwrap();
    settle(&engine, id).await;
    assert_eq!(log.lines(), ["aborted inner", "aborted supervisor"]);
    engine.close().await.unwrap();
}
