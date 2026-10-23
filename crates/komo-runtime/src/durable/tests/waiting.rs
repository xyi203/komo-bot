//! `waiting`、failFast / allSettled、等待环（spec §5.5）。对应 pi `harness-tasks.test.ts` 的
//! "resumes a waiting task once every task in `on` is terminal" 与 `harness-ownership` / spec 的等待规则。

use std::sync::Arc;

use komo_kernel::durable::TaskId;
use komo_kernel::durable::records::JoinPolicy;
use komo_kernel::durable::task::NextTaskState;
use serde_json::json;

use super::super::{Ownership, TaskOptions};
use super::support::*;

/// 睡 `input.ms` 毫秒后以 `input.end`（"done" / "failed"）结束；被取消时记一行。
fn sleeper(env: &Env, log: &Log) -> Arc<Kind> {
    let clock = env.clock.clone();
    let abort_log = log.clone();
    kind("sleeper")
        .phase("start", move |task, rt| {
            let until = clock.now_ms() + task.input()["ms"].as_i64().unwrap();
            async move {
                rt.sleep_until(until).await?;
                rt.commit_next(match task.input()["end"].as_str() {
                    Some("failed") => failed("nope"),
                    _ => done(json!(1)),
                })
                .await?;
                Ok(())
            }
        })
        .on_abort(move |task, rt| {
            let log = abort_log.clone();
            async move {
                log.push(format!("aborted {}", task.id()));
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        })
        .build()
}

/// 等 `input.on` 里的任务（allSettled），醒来读结局。
fn waiter(log: &Log, policy: JoinPolicy) -> Arc<Kind> {
    let log = log.clone();
    kind("waiter")
        .phase("start", move |task, rt| async move {
            let on: Vec<TaskId> = serde_json::from_value(task.input()["on"].clone()).unwrap();
            rt.commit_next(wait("after", on, policy)).await?;
            Ok(())
        })
        .phase("after", move |task, rt| {
            let log = log.clone();
            async move {
                let on: Vec<TaskId> = serde_json::from_value(task.input()["on"].clone()).unwrap();
                let outcomes = rt.outcomes(&on).await?;
                let names: Vec<&str> = outcomes.iter().map(status).collect();
                log.push(format!("woke: {}", names.join(",")));
                rt.commit_next(done(json!(names))).await?;
                Ok(())
            }
        })
        .build()
}

/// pi: "resumes a waiting task once every task in `on` is terminal, whatever the outcome"。
#[tokio::test(start_paused = true)]
async fn a_waiting_task_resumes_once_every_task_in_on_is_terminal_whatever_the_outcome() {
    let env = Env::new();
    let log = Log::default();
    let (sleeper, waiter) = (sleeper(&env, &log), waiter(&log, JoinPolicy::AllSettled));
    env.install(&sleeper);
    env.install(&waiter);
    let engine = env.open_running().await;
    let quick = spawn(&engine, &sleeper, json!({"ms": 100, "end": "done"})).await;
    let slow = spawn(&engine, &sleeper, json!({"ms": 300, "end": "failed"})).await;
    let finished = spawn(&engine, &sleeper, json!({"ms": 0, "end": "done"})).await;
    settle(&engine, finished).await;
    let id = spawn(&engine, &waiter, json!({"on": [quick, slow, finished]})).await;
    until(|| {
        engine.inspect().iter().any(|t| {
            t.record.id == id
                && matches!(t.state, super::super::TaskInspectionState::Waiting { .. })
        })
    })
    .await;
    assert!(
        log.lines().is_empty(),
        "it does not run while any member is live"
    );
    settle(&engine, id).await;
    assert_eq!(log.lines(), ["woke: completed,failed,completed"]);
    engine.close().await.unwrap();
}

/// spec §5.1："an empty `on` resumes at the next scheduling pass"。
#[tokio::test(start_paused = true)]
async fn an_empty_wait_resumes_at_the_next_scheduling_pass() {
    let env = Env::new();
    let log = Log::default();
    let waiter = waiter(&log, JoinPolicy::AllSettled);
    env.install(&waiter);
    let engine = env.open_running().await;
    let id = spawn(&engine, &waiter, json!({"on": []})).await;
    settle(&engine, id).await;
    assert_eq!(log.lines(), ["woke: "]);
    engine.close().await.unwrap();
}

/// spec §5.5：`allSettled` 下一个成员失败不会给其余成员打标记，等待者也不被标记。
#[tokio::test(start_paused = true)]
async fn all_settled_lets_the_other_members_finish_when_one_fails() {
    let env = Env::new();
    let log = Log::default();
    let sleeper = sleeper(&env, &log);
    let waiter = waiter(&log, JoinPolicy::AllSettled);
    env.install(&sleeper);
    env.install(&waiter);
    let engine = env.open_running().await;
    let bad = spawn(&engine, &sleeper, json!({"ms": 10, "end": "failed"})).await;
    let slow = spawn(&engine, &sleeper, json!({"ms": 500, "end": "done"})).await;
    let id = spawn(&engine, &waiter, json!({"on": [bad, slow]})).await;
    settle(&engine, id).await;
    assert_eq!(log.lines(), ["woke: failed,completed"]);
    assert!(!log.lines().iter().any(|l| l.starts_with("aborted")));
    engine.close().await.unwrap();
}

/// spec §5.5：failFast 只能等自己拥有的任务；`on` 不能指向不存在的任务。拒绝的提交不留痕迹。
#[tokio::test(start_paused = true)]
async fn a_wait_must_name_existing_tasks_and_fail_fast_only_tasks_the_waiter_owns() {
    let env = Env::new();
    let log = Log::default();
    let sleeper = sleeper(&env, &log);
    let probe_log = log.clone();
    let probe = kind("probe")
        .phase("start", move |task, rt| {
            let log = probe_log.clone();
            async move {
                let other: TaskId = serde_json::from_value(task.input()["other"].clone()).unwrap();
                let not_owned = rt
                    .commit_next(wait("end", vec![other], JoinPolicy::FailFast))
                    .await;
                log.push(format!("not owned: {}", not_owned.unwrap_err()));
                let missing = rt
                    .commit_next(wait(
                        "end",
                        vec![TaskId::new(999_999)],
                        JoinPolicy::AllSettled,
                    ))
                    .await;
                log.push(format!("missing: {}", missing.unwrap_err()));
                rt.commit_next(go("end")).await?;
                Ok(())
            }
        })
        .phase("end", |_task, rt| async move {
            rt.commit_next(done(json!(1))).await?;
            Ok(())
        })
        .build();
    env.install(&sleeper);
    env.install(&probe);
    let engine = env.open_running().await;
    let other = spawn(&engine, &sleeper, json!({"ms": 1000, "end": "done"})).await;
    let id = spawn(&engine, &probe, json!({"other": other})).await;
    settle(&engine, id).await;
    let lines = log.lines();
    assert!(
        lines[0].contains("can wait failFast only on tasks it owns"),
        "{lines:?}"
    );
    assert!(lines[1].contains("does not exist"), "{lines:?}");
    engine.close().await.unwrap();
}

/// spec §5.5 / §5.1："A task waiting on itself, or on an owner that holds it, closes the shortest such
/// cycles"；两个任务互等同样成环。
#[tokio::test(start_paused = true)]
async fn waits_that_close_a_cycle_are_rejected_and_name_the_cycle() {
    let env = Env::new();
    let log = Log::default();
    let (self_log, child_log) = (log.clone(), log.clone());
    let child = kind("child")
        .phase("start", move |task, rt| {
            let log = child_log.clone();
            async move {
                let owner = TaskId::new(task.input()["owner"].as_u64().unwrap());
                let outcome = rt
                    .commit_next(wait("end", vec![owner], JoinPolicy::AllSettled))
                    .await;
                log.push(format!("child: {}", outcome.unwrap_err()));
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .build();
    let parent_child = child.clone();
    let parent = kind("parent")
        .phase("start", move |task, rt| {
            let (log, child) = (self_log.clone(), parent_child.clone());
            async move {
                let me = task.id();
                let outcome = rt
                    .commit_next(wait("end", vec![me], JoinPolicy::AllSettled))
                    .await;
                log.push(format!("self: {}", outcome.unwrap_err()));
                // 子任务等它的所有者：所有者被子任务拖在 `completing`，互相等。
                rt.commit(move |tx, _| {
                    Box::pin(async move {
                        let id = tx
                            .create_task(
                                &*child,
                                json!({"owner": me.get()}),
                                Ownership::Task(me),
                                TaskOptions::default(),
                            )
                            .await?;
                        Ok(Some(NextTaskState::Waiting {
                            checkpoint: json!({"phase": "end"}),
                            on: vec![id],
                            policy: JoinPolicy::AllSettled,
                        }))
                    })
                })
                .await?;
                Ok(())
            }
        })
        .phase("end", |_task, rt| async move {
            rt.commit_next(done(json!(1))).await?;
            Ok(())
        })
        .build();
    env.install(&child);
    env.install(&parent);
    let engine = env.open_running().await;
    let id = spawn(&engine, &parent, json!({})).await;
    settle(&engine, id).await;
    let lines = log.lines();
    assert!(lines[0].contains("is a wait cycle"), "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("child:") && l.contains("is a wait cycle")),
        "{lines:?}"
    );
    engine.close().await.unwrap();
}

/// 两个互不拥有的任务互相等：第二个任务的等待成环，被拒绝。
#[tokio::test(start_paused = true)]
async fn two_tasks_waiting_on_each_other_close_a_cycle() {
    let env = Env::new();
    let log = Log::default();
    let slot: Arc<std::sync::Mutex<Option<TaskId>>> = Default::default();
    let (first_slot, second_log) = (slot.clone(), log.clone());
    let first = kind("first")
        .phase("start", move |_task, rt| {
            let slot = first_slot.clone();
            async move {
                while slot.lock().unwrap().is_none() {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                let second = slot.lock().unwrap().unwrap();
                rt.commit_next(wait("end", vec![second], JoinPolicy::AllSettled))
                    .await?;
                Ok(())
            }
        })
        .phase("end", |_task, rt| async move {
            rt.commit_next(done(json!(1))).await?;
            Ok(())
        })
        .build();
    let second = kind("second")
        .phase("start", move |task, rt| {
            let log = second_log.clone();
            async move {
                let first = TaskId::new(task.input()["first"].as_u64().unwrap());
                // 等第一个任务已经在等第二个。
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let outcome = rt
                    .commit_next(wait("end", vec![first], JoinPolicy::AllSettled))
                    .await;
                log.push(format!("second: {}", outcome.unwrap_err()));
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .build();
    env.install(&first);
    env.install(&second);
    let engine = env.open_running().await;
    let first_id = spawn(&engine, &first, json!({})).await;
    let second_id = spawn(&engine, &second, json!({"first": first_id.get()})).await;
    *slot.lock().unwrap() = Some(second_id);
    settle(&engine, second_id).await;
    settle(&engine, first_id).await;
    let lines = log.lines();
    assert!(
        lines[0].contains(&format!("{first_id} -> {second_id}"))
            || lines[0].contains("is a wait cycle"),
        "{lines:?}"
    );
    engine.close().await.unwrap();
}
