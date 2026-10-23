//! 重启、阻塞的任务、定义版本与 handover（spec §5.1、§5.4）。对应 pi
//! `test/harness-tasks-recovery.test.ts`：task recovery、blocked tasks、definition handover。

use std::time::Duration;

use komo_kernel::durable::records::{AbortReason, TaskOutcome};
use komo_kernel::durable::{
    DocumentAddress, DocumentScope, ROOT_CONVERSATION_ID, Storage, TaskId, TaskState, TaskStatus,
};
use serde_json::json;

use super::super::{Migrated, Ownership, TaskInspectionState, TaskOptions};
use super::support::*;
use komo_kernel::durable::task::BlockedReason;

fn inspected(engine: &super::super::Engine, id: TaskId) -> TaskInspectionState {
    engine
        .inspect()
        .into_iter()
        .find(|t| t.record.id == id)
        .expect("task is live")
        .state
}

/// pi: "resumes an intent/effect/outcome task interrupted after its intent across close and reopen"：
/// 意图先提交，效果之后做；重启后在意图阶段继续，用同一个幂等键重做效果。
#[tokio::test(start_paused = true)]
async fn a_task_interrupted_after_its_intent_resumes_with_the_same_key_after_reopen() {
    let env = Env::new();
    let log = Log::default();
    let (prepare_log, charge_log) = (log.clone(), log.clone());
    let clock = env.clock.clone();
    let payment = kind("payment")
        .phase("prepare", move |_task, rt| {
            let log = prepare_log.clone();
            async move {
                log.push("prepare");
                let key = format!("key-{}", rt.now()?);
                rt.commit_next(go_with(json!({"phase": "charge", "key": key})))
                    .await?;
                Ok(())
            }
        })
        .initial(|_| json!({"phase": "prepare"}))
        .phase("charge", move |task, rt| {
            let (log, clock) = (charge_log.clone(), clock.clone());
            async move {
                let key = task.checkpoint["key"].as_str().unwrap().to_owned();
                log.push(format!("charge {key}"));
                if log.count(&format!("charge {key}")) == 1 {
                    // 第一次：效果做到一半被打断（等到被发信号）。
                    rt.sleep_until(clock.now_ms() + 3_600_000).await?;
                }
                rt.commit_next(done(json!({"key": key}))).await?;
                Ok(())
            }
        })
        .build();
    env.install(&payment);
    let engine = env.open_running().await;
    let id = spawn(&engine, &payment, json!({})).await;
    until(|| log.lines().len() == 2).await;
    engine.close().await.unwrap();
    assert!(matches!(
        record(&env, id).await.state,
        TaskState::Running { .. }
    ));

    let engine = env.open_running().await;
    let task = settle(&engine, id).await;
    assert_eq!(status(outcome_of(&task)), "completed");
    let lines = log.lines();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[1], lines[2], "same key on the retry");
    assert_eq!(log.count("prepare"), 1, "the intent phase is not repeated");
    engine.close().await.unwrap();
}

/// pi: "keeps a task's startedAt across close and reopen and stamps endedAt when it settles"。
#[tokio::test(start_paused = true)]
async fn started_at_survives_a_reopen() {
    let env = Env::new();
    let clock = env.clock.clone();
    let slow = kind("slow")
        .phase("start", move |_task, rt| {
            let until = clock.now_ms() + 1_000;
            async move {
                rt.sleep_until(until).await?;
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .build();
    env.install(&slow);
    let engine = env.open_running().await;
    let id = spawn(&engine, &slow, json!({})).await;
    until(|| false_unless_started(&env, id)).await;
    let started = record(&env, id).await.started_at.unwrap();
    engine.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let engine = env.open_running().await;
    let task = settle(&engine, id).await;
    assert_eq!(task.started_at, Some(started));
    assert!(task.ended_at.unwrap() >= started);
    engine.close().await.unwrap();
}

fn false_unless_started(env: &Env, id: TaskId) -> bool {
    futures_util::FutureExt::now_or_never(env.storage.task(id))
        .and_then(Result::ok)
        .flatten()
        .is_some_and(|t| t.started_at.is_some())
}

/// pi: "resumes abort work after close at every direct-task abort stage" / "crash after the mark before the
/// run joins: only the abort handler runs"：abort 标记是持久的，重开后只跑 abort handler。
#[tokio::test(start_paused = true)]
async fn a_durable_abort_mark_survives_reopen_and_only_the_abort_handler_runs() {
    let env = Env::new();
    let log = Log::default();
    let (run_log, abort_log) = (log.clone(), log.clone());
    let k = kind("marked")
        .phase("start", move |_task, rt| {
            let log = run_log.clone();
            async move {
                log.push("run");
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .on_abort(move |_task, rt| {
            let log = abort_log.clone();
            async move {
                log.push("abort handler");
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        })
        .build();
    env.install(&k);
    let engine = env.open().await; // 暂停：不调度
    let id = spawn(&engine, &k, json!({})).await;
    engine.abort_task(id).await.unwrap();
    assert!(record(&env, id).await.abort_requested);
    engine.close().await.unwrap();
    assert!(log.lines().is_empty());

    let engine = env.open_running().await;
    assert_eq!(status(outcome_of(&settle(&engine, id).await)), "aborted");
    assert_eq!(log.lines(), ["abort handler"]);
    engine.close().await.unwrap();
}

/// pi: "keeps a task with a missing definition pending, and live for idle waits, until registration"。
#[tokio::test(start_paused = true)]
async fn a_task_without_a_definition_stays_pending_and_blocks_idle_until_one_is_registered() {
    let env = Env::new();
    let k = kind("later")
        .phase("start", |_task, rt| async move {
            rt.commit_next(done(json!(1))).await?;
            Ok(())
        })
        .build();
    let engine = env.open_running().await;
    let id = spawn(&engine, &k, json!({})).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        inspected(&engine, id),
        TaskInspectionState::Blocked {
            reason: BlockedReason::MissingTask
        }
    );
    assert_eq!(record(&env, id).await.state.status(), TaskStatus::Pending);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            engine.wait_for_idle(Some(ROOT_CONVERSATION_ID))
        )
        .await
        .is_err(),
        "blocked work is still live"
    );
    env.install(&k);
    assert_eq!(status(outcome_of(&settle(&engine, id).await)), "completed");
    engine.close().await.unwrap();
}

/// pi: "keeps a task stored by a newer version pending until a fitting definition is registered"。
#[tokio::test(start_paused = true)]
async fn a_record_stored_by_a_newer_version_waits_for_a_fitting_definition() {
    let env = Env::new();
    let step = |version: u32| {
        kind("versioned")
            .version(version)
            .phase("start", |_task, rt| async move {
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            })
            .build()
    };
    let (v1, v2) = (step(1), step(2));
    env.install(&v1);
    let engine = env.open_running().await;
    let id = spawn(&engine, &v2, json!({})).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        inspected(&engine, id),
        TaskInspectionState::Blocked {
            reason: BlockedReason::TaskTooOld
        }
    );
    env.install(&v2);
    assert_eq!(status(outcome_of(&settle(&engine, id).await)), "completed");
    engine.close().await.unwrap();
}

/// pi: "migrates at reservation, leaves the record unchanged when migration fails, and retries only for a new
/// definition"。
#[tokio::test(start_paused = true)]
async fn migration_runs_at_reservation_and_a_failure_blocks_until_a_new_definition_arrives() {
    let env = Env::new();
    let log = Log::default();
    let old = kind("migrating")
        .initial(|input| json!({"phase": "start", "old": input["n"]}))
        .phase("start", |_t, _rt| async move { Ok(()) })
        .build();
    let new_phases = |log: &Log| {
        let log = log.clone();
        move |task: komo_kernel::durable::task::RunningTask, rt: super::super::TaskRuntime| {
            let log = log.clone();
            async move {
                log.push(format!(
                    "v2 ran: input={} checkpoint={}",
                    task.input(),
                    task.checkpoint
                ));
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        }
    };
    let broken = kind("migrating")
        .version(2)
        .migrate(|_, _, _| Err("cannot migrate".into()))
        .phase("migrated", new_phases(&log))
        .build();
    let fixed = kind("migrating")
        .version(2)
        .migrate(|input, checkpoint, from| {
            assert_eq!(from, 1);
            Ok(Migrated {
                input: json!({"n": input["n"], "migrated": true}),
                checkpoint: json!({"phase": "migrated", "was": checkpoint["old"]}),
            })
        })
        .phase("migrated", new_phases(&log))
        .build();
    let engine = env.open_running().await;
    let id = spawn(&engine, &old, json!({"n": 5})).await;
    let before = record(&env, id).await;
    env.install(&broken);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        inspected(&engine, id),
        TaskInspectionState::Blocked {
            reason: BlockedReason::MigrationFailed
        }
    );
    assert_eq!(
        record(&env, id).await,
        before,
        "a failed migration leaves the record unchanged"
    );
    assert_eq!(env.reports.lines().len(), 1, "reported once");
    // 同一个定义对象再装一次：不重试，也不重复报告。
    env.install(&broken);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(env.reports.lines().len(), 1);
    assert!(matches!(
        inspected(&engine, id),
        TaskInspectionState::Blocked { .. }
    ));
    // 换一个定义对象：迁移成功，迁移后的记录与 running 同一笔提交。
    env.install(&fixed);
    let task = settle(&engine, id).await;
    assert_eq!(task.version, 2);
    assert_eq!(
        log.lines(),
        [r#"v2 ran: input={"migrated":true,"n":5} checkpoint={"phase":"migrated","was":5}"#]
    );
    engine.close().await.unwrap();
}

/// pi: "blocks an older record whose newer definition has no migration"。
#[tokio::test(start_paused = true)]
async fn a_newer_definition_without_migrate_blocks_an_older_record() {
    let env = Env::new();
    let v1 = kind("nomigrate")
        .phase("start", |_t, _r| async move { Ok(()) })
        .build();
    let v2 = kind("nomigrate")
        .version(2)
        .phase("start", |_t, _r| async move { Ok(()) })
        .build();
    env.install(&v2);
    let engine = env.open_running().await;
    let id = spawn(&engine, &v1, json!({})).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        inspected(&engine, id),
        TaskInspectionState::Blocked {
            reason: BlockedReason::MigrationFailed
        }
    );
    engine.close().await.unwrap();
}

/// pi: "settles an aborted blocked task as orphaned and retires its documents"：没有定义能接手时，abort
/// 不跑 abort handler，直接 `orphaned`，reason 是阻塞原因。
#[tokio::test(start_paused = true)]
async fn aborting_a_blocked_task_settles_it_as_orphaned_and_retires_its_documents() {
    let env = Env::new();
    let missing = kind("never-registered").build();
    let engine = env.open_running().await;
    let id = spawn(&engine, &missing, json!({})).await;
    let address = DocumentAddress {
        kind: "test.scratch".into(),
        scope: DocumentScope::Task { task: id },
        key: None,
    };
    let doc = address.clone();
    engine
        .commit(move |tx| Box::pin(async move { tx.put_document(doc, 1, json!(1)).await }))
        .await
        .unwrap();
    assert!(env.storage.find_document(&address).await.unwrap().is_some());
    engine.abort_task(id).await.unwrap();
    let task = settle(&engine, id).await;
    assert_eq!(
        outcome_of(&task),
        &TaskOutcome::Orphaned {
            reason: "missing_task".into()
        }
    );
    assert!(env.storage.find_document(&address).await.unwrap().is_none());
    engine.close().await.unwrap();
}

/// pi: "hands over at the next phase boundary to a same-version replacement without overlap" 与
/// "keeps the memos across a handover"。
#[tokio::test(start_paused = true)]
async fn a_replacement_definition_takes_over_at_the_next_phase_boundary_keeping_memos() {
    let env = Env::new();
    let log = Log::default();
    let second_log = log.clone();
    let replacement = kind("swap")
        .phase("second", move |_task, rt| {
            let log = second_log.clone();
            async move {
                log.push(format!("B second memo={:?}", rt.memo("m").await?));
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .build();
    let (registry, first_log, old_second_log) = (env.registry.clone(), log.clone(), log.clone());
    let replacement_for_a = replacement.clone();
    let original = kind("swap")
        .phase("start", move |_task, rt| {
            let (registry, log, replacement) = (
                registry.clone(),
                first_log.clone(),
                replacement_for_a.clone(),
            );
            async move {
                log.push("A start");
                rt.put_memo("m", json!(7)).await?;
                registry.install(replacement);
                rt.commit_next(go("second")).await?;
                Ok(())
            }
        })
        .phase("second", move |_task, rt| {
            let log = old_second_log.clone();
            async move {
                log.push("A second");
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .build();
    env.install(&original);
    let engine = env.open_running().await;
    let id = spawn(&engine, &original, json!({})).await;
    assert_eq!(status(outcome_of(&settle(&engine, id).await)), "completed");
    assert_eq!(log.lines(), ["A start", "B second memo=Some(Number(7))"]);
    engine.close().await.unwrap();
}

/// pi: "keeps running under the old definition when the replacement is missing or cannot take the task"。
#[tokio::test(start_paused = true)]
async fn a_replacement_that_cannot_take_the_task_is_reported_once_and_the_old_definition_continues()
{
    let env = Env::new();
    let log = Log::default();
    let incompatible = kind("stay")
        .version(2)
        .phase("second", |_t, _r| async move { Ok(()) })
        .build();
    let (registry, first_log, second_log) = (env.registry.clone(), log.clone(), log.clone());
    let original = kind("stay")
        .phase("start", move |_task, rt| {
            let (registry, incompatible, log) =
                (registry.clone(), incompatible.clone(), first_log.clone());
            async move {
                log.push("A start");
                registry.install(incompatible);
                rt.commit_next(go("second")).await?;
                Ok(())
            }
        })
        .phase("second", move |_task, rt| {
            let log = second_log.clone();
            async move {
                log.push("A second");
                rt.commit_next(go("third")).await?;
                Ok(())
            }
        })
        .phase("third", |_task, rt| async move {
            rt.commit_next(done(json!(1))).await?;
            Ok(())
        })
        .build();
    env.install(&original);
    let engine = env.open_running().await;
    let id = spawn(&engine, &original, json!({})).await;
    assert_eq!(status(outcome_of(&settle(&engine, id).await)), "completed");
    assert_eq!(log.lines(), ["A start", "A second"]);
    assert_eq!(
        env.reports.lines().len(),
        1,
        "reported once per resolved definition"
    );
    engine.close().await.unwrap();
}

/// spec §5.4：`abandon_on_restart` 的任务在重启后第一轮 reservation 被打 `restart` 标记，只跑 abort
/// handler，handler 能从记录上看出这是放弃而不是请求。
#[tokio::test(start_paused = true)]
async fn a_task_abandoned_on_restart_gets_a_restart_mark_and_only_its_abort_handler_runs() {
    let env = Env::new();
    let log = Log::default();
    let (run_log, abort_log) = (log.clone(), log.clone());
    let clock = env.clock.clone();
    let k = kind("abandoned")
        .phase("start", move |_task, rt| {
            let (log, until) = (run_log.clone(), clock.now_ms() + 3_600_000);
            async move {
                log.push("run");
                rt.sleep_until(until).await?;
                Ok(())
            }
        })
        .on_abort(move |task, rt| {
            let log = abort_log.clone();
            async move {
                log.push(format!("abort reason={:?}", task.record.abort_reason));
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        })
        .build();
    env.install(&k);
    let engine = env.open_running().await;
    let kind_for_tx = k.clone();
    let id = engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.create_task(
                    &*kind_for_tx,
                    json!({}),
                    Ownership::Conversation,
                    TaskOptions {
                        conversation: Some(ROOT_CONVERSATION_ID),
                        abandon_on_restart: true,
                        ..TaskOptions::default()
                    },
                )
                .await
            })
        })
        .await
        .unwrap();
    until(|| log.count("run") == 1).await;
    engine.close().await.unwrap();
    assert_eq!(
        record(&env, id).await.abort_reason,
        None,
        "close marks nothing"
    );

    let engine = env.open().await;
    assert_eq!(
        record(&env, id).await.abort_reason,
        None,
        "opening alone marks nothing"
    );
    engine.resume();
    let task = settle(&engine, id).await;
    assert_eq!(status(outcome_of(&task)), "aborted");
    assert_eq!(task.abort_reason, Some(AbortReason::Restart));
    assert_eq!(log.lines(), ["run", "abort reason=Some(Restart)"]);
    engine.close().await.unwrap();
}
