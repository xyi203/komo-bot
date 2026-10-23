//! step 规则 1–6、runtime 能力。对应 pi `test/harness-tasks.test.ts` 的 "task phases" 与
//! "task runtime"。

use komo_kernel::durable::records::TaskOutcome;
use komo_kernel::durable::{DocumentAddress, DocumentScope, Storage};
use serde_json::json;

use super::super::{DurableError, EntryDraft};
use super::support::*;

/// pi: "continues one invocation through checkpoint progress and completes with a typed result"。
#[tokio::test(start_paused = true)]
async fn a_task_runs_through_checkpoint_progress_and_completes_with_its_result() {
    let env = Env::new();
    let phases = Log::default();
    let (a, b, c) = (phases.clone(), phases.clone(), phases.clone());
    let steps = kind("steps")
        .phase("start", move |_task, rt| {
            let log = a.clone();
            async move {
                log.push("start");
                rt.commit_next(go("middle")).await?;
                Ok(())
            }
        })
        .phase("middle", move |_task, rt| {
            let log = b.clone();
            async move {
                log.push("middle");
                rt.commit_next(go_with(json!({"phase": "end", "n": 2})))
                    .await?;
                Ok(())
            }
        })
        .phase("end", move |task, rt| {
            let log = c.clone();
            async move {
                log.push(format!("end n={}", task.checkpoint["n"]));
                rt.commit_next(done(json!("ok"))).await?;
                Ok(())
            }
        })
        .build();
    env.install(&steps);
    let engine = env.open_running().await;
    let id = spawn(&engine, &steps, json!({})).await;
    let task = settle(&engine, id).await;
    assert_eq!(
        outcome_of(&task),
        &TaskOutcome::Completed {
            result: json!("ok")
        }
    );
    assert_eq!(phases.lines(), ["start", "middle", "end n=2"]);
    engine.close().await.unwrap();
}

/// pi: "faults a phase without durable progress and a throwing phase"（规则 6、规则 4）。
#[tokio::test(start_paused = true)]
async fn rule_6_a_phase_that_leaves_the_checkpoint_unchanged_faults_the_task() {
    let env = Env::new();
    let idle = kind("idle")
        .phase("start", |_task, _rt| async move { Ok(()) })
        .build();
    env.install(&idle);
    let engine = env.open_running().await;
    let id = spawn(&engine, &idle, json!({})).await;
    let task = settle(&engine, id).await;
    assert!(
        faulted_message(&task).contains("without durable progress"),
        "{task:?}"
    );
    engine.close().await.unwrap();
}

/// pi: "compares checkpoints by value, including arrays"：同样的值（键序不同）也算没有进展。
#[tokio::test(start_paused = true)]
async fn rule_6_compares_checkpoints_by_value_whatever_the_key_order() {
    let env = Env::new();
    let same = kind("same")
        .initial(|_| json!({"phase": "start", "items": [1, {"a": 1, "b": 2}]}))
        .phase("start", |_task, rt| async move {
            rt.commit_next(go_with(
                json!({"items": [1, {"b": 2, "a": 1}], "phase": "start"}),
            ))
            .await?;
            Ok(())
        })
        .build();
    env.install(&same);
    let engine = env.open_running().await;
    let id = spawn(&engine, &same, json!({})).await;
    assert!(faulted_message(&settle(&engine, id).await).contains("without durable progress"));
    engine.close().await.unwrap();
}

/// pi: 同一测试的后半（规则 4：phase 抛错即 faulted），外加 panic。
#[tokio::test(start_paused = true)]
async fn rule_4_a_phase_that_returns_an_error_or_panics_faults_the_task() {
    let env = Env::new();
    let failing = kind("failing")
        .phase("start", |_task, _rt| async move { Err("boom".into()) })
        .build();
    let panicking = kind("panicking")
        .phase("start", |_task, _rt| async move { panic!("kaboom") })
        .build();
    env.install(&failing);
    env.install(&panicking);
    let engine = env.open_running().await;
    let first = spawn(&engine, &failing, json!({})).await;
    let second = spawn(&engine, &panicking, json!({})).await;
    assert_eq!(faulted_message(&settle(&engine, first).await), "boom");
    assert_eq!(faulted_message(&settle(&engine, second).await), "kaboom");
    engine.close().await.unwrap();
}

/// pi: "keeps a committed terminal outcome when the handler throws afterwards"（规则 1 先于规则 4）。
#[tokio::test(start_paused = true)]
async fn rule_1_a_committed_outcome_beats_an_error_thrown_afterwards() {
    let env = Env::new();
    let late = kind("late")
        .phase("start", |_task, rt| async move {
            rt.commit_next(done(json!(1))).await?;
            Err("too late".into())
        })
        .build();
    env.install(&late);
    let engine = env.open_running().await;
    let id = spawn(&engine, &late, json!({})).await;
    let task = settle(&engine, id).await;
    assert_eq!(status(outcome_of(&task)), "completed");
    engine.close().await.unwrap();
}

/// 规则 5：同一个 phase 里 checkpoint 变了也是进展（pi: "including progress within the same phase"）。
#[tokio::test(start_paused = true)]
async fn rule_5_progress_inside_the_same_phase_runs_the_phase_again() {
    let env = Env::new();
    let counter = kind("counter")
        .initial(|_| json!({"phase": "count", "n": 0}))
        .phase("count", |task, rt| async move {
            let n = task.checkpoint["n"].as_u64().unwrap();
            if n == 2 {
                rt.commit_next(done(json!(n))).await?;
            } else {
                rt.commit_next(go_with(json!({"phase": "count", "n": n + 1})))
                    .await?;
            }
            Ok(())
        })
        .build();
    env.install(&counter);
    let engine = env.open_running().await;
    let id = spawn(&engine, &counter, json!({})).await;
    let task = settle(&engine, id).await;
    assert_eq!(
        outcome_of(&task),
        &TaskOutcome::Completed { result: json!(2) }
    );
    engine.close().await.unwrap();
}

/// pi: "commits results with entries atomically, keeps memos until terminal, and retires task documents"。
#[tokio::test(start_paused = true)]
async fn a_checkpoint_commits_atomically_with_entries_and_documents_and_memos_end_with_the_task() {
    let env = Env::new();
    let log = Log::default();
    let (first, second) = (log.clone(), log.clone());
    let doc = |task: komo_kernel::durable::TaskId| DocumentAddress {
        kind: "test.progress".into(),
        scope: DocumentScope::Task { task },
        key: None,
    };
    let kind = kind("atomic")
        .phase("start", move |task, rt| {
            let log = first.clone();
            async move {
                let address = doc(task.id());
                let conversation = task.record.conversation;
                let entry_log = log.clone();
                rt.commit(move |tx, _current| {
                    Box::pin(async move {
                        let entry = tx
                            .append_entry(
                                conversation,
                                EntryDraft::new("test.receipt").with_data(json!({"n": 1})),
                            )
                            .await?;
                        entry_log.push(format!("entry {}", entry.id));
                        tx.put_document(address, 1, json!({"n": 1})).await?;
                        Ok(Some(go("finish")))
                    })
                })
                .await?;
                let winner = rt.put_memo("key", json!("first")).await?;
                let loser = rt.put_memo("key", json!("second")).await?;
                log.push(format!("memo {winner} {loser}"));
                // 没有进展的 memo 不改 checkpoint；这里再前进一步。
                rt.commit_next(go("finish")).await?;
                Ok(())
            }
        })
        .phase("finish", move |task, rt| {
            let log = second.clone();
            async move {
                log.push(format!("read {:?}", rt.memo("key").await?));
                let address = doc(task.id());
                rt.commit(move |tx, _| {
                    Box::pin(async move {
                        let doc = tx.document(&address).await?;
                        assert_eq!(doc, Some((1, json!({"n": 1}))), "reads its own writes");
                        Ok(Some(done(json!("done"))))
                    })
                })
                .await?;
                Ok(())
            }
        })
        .build();
    env.install(&kind);
    let engine = env.open_running().await;
    let id = spawn(&engine, &kind, json!({})).await;
    let task = settle(&engine, id).await;
    assert_eq!(status(outcome_of(&task)), "completed");
    assert!(
        task.memos.is_none(),
        "memos disappear in the terminal replacement"
    );
    let lines = log.lines();
    assert!(lines[0].starts_with("entry "), "{lines:?}");
    assert_eq!(lines[1], "memo \"first\" \"first\"", "first writer wins");
    assert_eq!(lines[2], "read Some(String(\"first\"))");
    let entry_id: u64 = lines[0].trim_start_matches("entry ").parse().unwrap();
    let entry = env
        .storage
        .entry(komo_kernel::durable::EntryId::new(entry_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.entry.by_task, Some(id), "entries record the task");
    let address = DocumentAddress {
        kind: "test.progress".into(),
        scope: DocumentScope::Task { task: id },
        key: None,
    };
    assert!(
        env.storage.find_document(&address).await.unwrap().is_none(),
        "task documents retire with the task"
    );
    engine.close().await.unwrap();
}

/// pi: "stamps startedAt at the first run and endedAt when terminal, keeping startedAt through a wait"。
#[tokio::test(start_paused = true)]
async fn started_at_is_stamped_at_the_first_run_and_ended_at_when_terminal() {
    let env = Env::new();
    let clock = env.clock.clone();
    let napper = kind("napper")
        .phase("start", move |_task, rt| {
            let until = clock.now_ms() + 500;
            async move {
                rt.sleep_until(until).await?;
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .build();
    env.install(&napper);
    let engine = env.open().await;
    let id = spawn(&engine, &napper, json!({})).await;
    assert_eq!(record(&env, id).await.started_at, None, "never ran yet");
    let created = env.clock.now_ms();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    engine.resume();
    let task = settle(&engine, id).await;
    let (started, ended) = (task.started_at.unwrap(), task.ended_at.unwrap());
    assert!(
        started >= created + 100,
        "stamped at the reservation, not at creation"
    );
    assert!(ended >= started + 400, "the span includes the sleep");
    engine.close().await.unwrap();
}

/// pi: "rejects runtime operations after the invocation ends"。
#[tokio::test(start_paused = true)]
async fn runtime_operations_reject_once_the_invocation_has_ended() {
    let env = Env::new();
    let leaked: std::sync::Arc<std::sync::Mutex<Option<super::super::TaskRuntime>>> =
        Default::default();
    let slot = leaked.clone();
    let leaky = kind("leaky")
        .phase("start", move |_task, rt| {
            let slot = slot.clone();
            async move {
                *slot.lock().unwrap() = Some(rt.clone());
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .build();
    env.install(&leaky);
    let engine = env.open_running().await;
    let id = spawn(&engine, &leaky, json!({})).await;
    settle(&engine, id).await;
    let rt = leaked.lock().unwrap().clone().unwrap();
    until(|| rt.now().is_err()).await;
    assert!(matches!(
        rt.get_task(id).await,
        Err(DurableError::InvocationEnded(_))
    ));
    assert!(matches!(
        rt.commit_next(go("again")).await,
        Err(DurableError::InvocationEnded(_))
    ));
    assert!(matches!(
        rt.memo("x").await,
        Err(DurableError::InvocationEnded(_))
    ));
    assert!(rt.cancelled().is_cancelled());
    engine.close().await.unwrap();
}

/// pi: "sleeps until the Harness clock reaches the deadline"；"rejects a sleep when the invocation is signalled"。
#[tokio::test(start_paused = true)]
async fn sleep_waits_for_the_clock_and_rejects_when_the_task_is_aborted() {
    let env = Env::new();
    let log = Log::default();
    let (sleeper_log, abort_log) = (log.clone(), log.clone());
    let clock = env.clock.clone();
    let sleeper = kind("sleeper")
        .phase("start", move |_task, rt| {
            let log = sleeper_log.clone();
            let until = clock.now_ms() + 10_000;
            async move {
                let outcome = rt.sleep_until(until).await;
                log.push(format!("slept: {}", outcome.is_ok()));
                outcome?;
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
    env.install(&sleeper);
    let engine = env.open_running().await;
    let finished = spawn(&engine, &sleeper, json!({})).await;
    assert_eq!(
        status(outcome_of(&settle(&engine, finished).await)),
        "completed"
    );
    let cancelled = spawn(&engine, &sleeper, json!({})).await;
    until(|| {
        log.count("slept: true") == 1
            && engine.inspect().iter().any(|t| {
                t.record.id == cancelled && t.state == super::super::TaskInspectionState::Running
            })
    })
    .await;
    engine.abort_task(cancelled).await.unwrap();
    assert_eq!(
        status(outcome_of(&settle(&engine, cancelled).await)),
        "aborted"
    );
    assert_eq!(log.count("slept: false"), 1);
    assert_eq!(log.count("abort handler"), 1);
    engine.close().await.unwrap();
}
