//! abort 协议（spec §5.4）：提交标记 → 结束并 join run invocation → 等自有工作不再活 →
//! 起 abort invocation → abort handler 提交结果。对应 pi `harness-tasks.test.ts` 的 "task abort"。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::durable::TaskId;
use komo_kernel::durable::records::JoinPolicy;
use serde_json::json;

use super::super::{AbortOutcome, DurableError};
use super::support::*;

/// 一个 run phase 睡很久（听信号）；abort handler 记日志后写 aborted。
fn sleepy(env: &Env, log: &Log) -> Arc<Kind> {
    let clock = env.clock.clone();
    let (run_log, abort_log) = (log.clone(), log.clone());
    kind("sleepy")
        .phase("start", move |_task, rt| {
            let log = run_log.clone();
            let until = clock.now_ms() + 3_600_000;
            async move {
                log.push("run");
                let slept = rt.sleep_until(until).await;
                log.push(format!("run signalled: {}", slept.is_err()));
                slept?;
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .on_abort(move |task, rt| {
            let log = abort_log.clone();
            async move {
                log.push(format!(
                    "abort handler, mark={}",
                    task.record.abort_requested
                ));
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        })
        .build()
}

async fn running(engine: &super::super::Engine, id: TaskId) {
    until(|| {
        engine
            .inspect()
            .iter()
            .any(|t| t.record.id == id && t.state == super::super::TaskInspectionState::Running)
    })
    .await;
}

/// pi: "signals and joins the run before returning, then runs the abort handler" 与
/// "rejects the run's commits ... after the mark, and settles through a fresh abort invocation"。
#[tokio::test(start_paused = true)]
async fn abort_signals_and_joins_the_run_then_a_fresh_invocation_runs_the_abort_handler() {
    let env = Env::new();
    let log = Log::default();
    let sleepy = sleepy(&env, &log);
    env.install(&sleepy);
    let engine = env.open_running().await;
    let id = spawn(&engine, &sleepy, json!({})).await;
    running(&engine, id).await;
    assert_eq!(engine.abort_task(id).await.unwrap(), AbortOutcome::Marked);
    // abort_task 返回时 run 已经 join：它看到了信号。
    assert_eq!(log.lines()[..2], ["run", "run signalled: true"]);
    let task = settle(&engine, id).await;
    assert_eq!(status(outcome_of(&task)), "aborted");
    assert!(task.abort_requested);
    assert_eq!(log.lines().last().unwrap(), "abort handler, mark=true");
    assert_eq!(log.count("run"), 1, "the run phase never starts again");
    assert_eq!(
        engine.abort_task(id).await.unwrap(),
        AbortOutcome::Terminal,
        "aborting a terminal task is a no-op"
    );
    engine.close().await.unwrap();
}

/// pi: "rejects the run's commits and memo writes after the mark"：不听信号的 run 在标记之后
/// 提交会被拒（durable abort mark），abort_task 等它结束。
#[tokio::test(start_paused = true)]
async fn a_run_that_ignores_its_signal_cannot_commit_after_the_mark() {
    let env = Env::new();
    let log = Log::default();
    let stubborn_log = log.clone();
    let stubborn = kind("stubborn")
        .phase("start", move |_task, rt| {
            let log = stubborn_log.clone();
            async move {
                // 不听信号：睡过标记再提交。
                tokio::time::sleep(Duration::from_secs(1)).await;
                let commit = rt.commit_next(done(json!(1))).await;
                log.push(format!("commit: {}", commit.unwrap_err()));
                let memo = rt.put_memo("k", json!(1)).await;
                log.push(format!("memo rejected: {}", memo.is_err()));
                Ok(())
            }
        })
        .build();
    env.install(&stubborn);
    let engine = env.open_running().await;
    let id = spawn(&engine, &stubborn, json!({})).await;
    let slow = Duration::from_millis(10);
    tokio::time::sleep(slow).await;
    engine.abort_task(id).await.unwrap();
    let lines = log.lines();
    assert!(lines[0].contains("durable abort mark"), "{lines:?}");
    assert_eq!(lines[1], "memo rejected: true");
    assert_eq!(status(outcome_of(&settle(&engine, id).await)), "aborted");
    engine.close().await.unwrap();
}

/// pi: "starts no further phase after a mark that lands during a phase with progress"（规则 3）。
#[tokio::test(start_paused = true)]
async fn rule_3_a_mark_that_lands_during_a_phase_with_progress_starts_no_further_phase() {
    let env = Env::new();
    let log = Log::default();
    let (first_log, second_log) = (log.clone(), log.clone());
    let progressing = kind("progressing")
        .phase("start", move |_task, rt| {
            let log = first_log.clone();
            async move {
                rt.commit_next(go("second")).await?;
                // 进展已提交；标记在 step 之前落下。
                log.push("progress committed");
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(())
            }
        })
        .phase("second", move |_task, rt| {
            let log = second_log.clone();
            async move {
                log.push("second ran");
                rt.commit_next(done(json!(1))).await?;
                Ok(())
            }
        })
        .build();
    env.install(&progressing);
    let engine = env.open_running().await;
    let id = spawn(&engine, &progressing, json!({})).await;
    until(|| log.count("progress committed") == 1).await;
    engine.abort_task(id).await.unwrap();
    assert_eq!(status(outcome_of(&settle(&engine, id).await)), "aborted");
    assert_eq!(log.count("second ran"), 0);
    engine.close().await.unwrap();
}

/// pi: "aborts waiting work before its wait ends"：有标记的等待者不等 `on`（不是自己拥有的），
/// 直接去 abort handler。
#[tokio::test(start_paused = true)]
async fn an_abort_mark_lets_a_waiting_task_leave_its_wait_early() {
    let env = Env::new();
    let log = Log::default();
    let sleepy = sleepy(&env, &log);
    let abort_log = log.clone();
    let waiter = kind("waiter")
        .phase("start", |task, rt| async move {
            let on = TaskId::new(task.input()["on"].as_u64().unwrap());
            rt.commit_next(wait("after", vec![on], JoinPolicy::AllSettled))
                .await?;
            Ok(())
        })
        .on_abort(move |_task, rt| {
            let log = abort_log.clone();
            async move {
                log.push("waiter abort handler");
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        })
        .build();
    env.install(&sleepy);
    env.install(&waiter);
    let engine = env.open_running().await;
    let target = spawn(&engine, &sleepy, json!({})).await;
    running(&engine, target).await;
    let id = spawn(&engine, &waiter, json!({"on": target.get()})).await;
    until(|| {
        engine.inspect().iter().any(|t| {
            t.record.id == id
                && matches!(t.state, super::super::TaskInspectionState::Waiting { .. })
        })
    })
    .await;
    engine.abort_task(id).await.unwrap();
    assert_eq!(status(outcome_of(&settle(&engine, id).await)), "aborted");
    assert!(
        engine.inspect().iter().any(|t| t.record.id == target),
        "the awaited task it does not own is still live"
    );
    engine.close().await.unwrap();
}

/// pi: "faults abort handlers that throw or settle nothing"（abort 阶段的规则 4）。
#[tokio::test(start_paused = true)]
async fn an_abort_handler_that_throws_or_settles_nothing_faults_the_task() {
    let env = Env::new();
    let clock = env.clock.clone();
    let sleep_phase = move |_task: komo_kernel::durable::task::RunningTask,
                            rt: super::super::TaskRuntime| {
        let until = clock.now_ms() + 3_600_000;
        async move {
            rt.sleep_until(until).await?;
            Ok(())
        }
    };
    let throws = kind("throws")
        .phase("start", sleep_phase.clone())
        .on_abort(|_task, _rt| async move { Err("compensation failed".into()) })
        .build();
    let silent = kind("silent")
        .phase("start", sleep_phase)
        .on_abort(|_task, _rt| async move { Ok(()) })
        .build();
    env.install(&throws);
    env.install(&silent);
    let engine = env.open_running().await;
    let a = spawn(&engine, &throws, json!({})).await;
    let b = spawn(&engine, &silent, json!({})).await;
    running(&engine, a).await;
    running(&engine, b).await;
    engine.abort_task(a).await.unwrap();
    engine.abort_task(b).await.unwrap();
    assert_eq!(
        faulted_message(&settle(&engine, a).await),
        "compensation failed"
    );
    assert!(faulted_message(&settle(&engine, b).await).contains("without a terminal outcome"));
    engine.close().await.unwrap();
}

/// pi: "keeps a terminal outcome committed by an abort handler that throws afterwards"。
#[tokio::test(start_paused = true)]
async fn an_outcome_committed_by_the_abort_handler_survives_a_later_error() {
    let env = Env::new();
    let clock = env.clock.clone();
    let k = kind("throws-after")
        .phase("start", move |_task, rt| {
            let until = clock.now_ms() + 3_600_000;
            async move {
                rt.sleep_until(until).await?;
                Ok(())
            }
        })
        .on_abort(|_task, rt| async move {
            rt.commit_next(aborted()).await?;
            Err("late".into())
        })
        .build();
    env.install(&k);
    let engine = env.open_running().await;
    let id = spawn(&engine, &k, json!({})).await;
    running(&engine, id).await;
    engine.abort_task(id).await.unwrap();
    assert_eq!(status(outcome_of(&settle(&engine, id).await)), "aborted");
    engine.close().await.unwrap();
}

/// spec §5.5："An abort handler may not return `waiting`"。
#[tokio::test(start_paused = true)]
async fn an_abort_handler_cannot_wait() {
    let env = Env::new();
    let log = Log::default();
    let abort_log = log.clone();
    let clock = env.clock.clone();
    let k = kind("no-wait")
        .phase("start", move |_task, rt| {
            let until = clock.now_ms() + 3_600_000;
            async move {
                rt.sleep_until(until).await?;
                Ok(())
            }
        })
        .on_abort(move |task, rt| {
            let log = abort_log.clone();
            async move {
                let wait_attempt = rt
                    .commit_next(wait("x", vec![task.id()], JoinPolicy::AllSettled))
                    .await;
                log.push(format!("{}", wait_attempt.unwrap_err()));
                rt.commit_next(aborted()).await?;
                Ok(())
            }
        })
        .build();
    env.install(&k);
    let engine = env.open_running().await;
    let id = spawn(&engine, &k, json!({})).await;
    running(&engine, id).await;
    engine.abort_task(id).await.unwrap();
    settle(&engine, id).await;
    assert!(log.lines()[0].contains("cannot wait"), "{:?}", log.lines());
    engine.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn aborting_an_unknown_task_is_an_error() {
    let env = Env::new();
    let engine = env.open_running().await;
    assert!(matches!(
        engine.abort_task(TaskId::new(424_242)).await,
        Err(DurableError::NoSuchTask(_))
    ));
    engine.close().await.unwrap();
}
