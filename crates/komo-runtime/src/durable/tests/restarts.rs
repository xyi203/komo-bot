//! 崩溃循环保护（pi `a30c2a81d` crash-loop guard，spec §5.4 "Restarts"；`docs/storage-durable-perf.md` §6）：
//! 上一代没有干净关闭时，幸存的 `running` 任务崩溃计数加一；到上限就停住，不再 reserve；任务自己
//! 提交新状态就归零；干净关闭与第一次升级都不算崩溃；停住的任务可以放行或放弃。
//!
//! "崩溃"在这里就是不调 `close`、直接丢掉引擎再在同一个库上开新的：旧引擎里挂着的 phase 再也不会
//! 提交任何东西，和进程被杀之后留下的库一样。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use komo_kernel::durable::task::{BlockedReason, RESTART_LIMIT};
use komo_kernel::durable::{
    ConversationId, ROOT_CONVERSATION_ID, Storage, StorageWrite, TaskId, TaskRecord, TaskState,
    TaskStatus,
};
use serde_json::json;

use super::super::engine::{crashed_before, engine_address};
use super::super::{Engine, TaskInspectionState};
use super::support::*;

fn inspected(engine: &Engine, id: TaskId) -> TaskInspectionState {
    engine
        .inspect()
        .into_iter()
        .find(|t| t.record.id == id)
        .expect("task is live")
        .state
}

/// 一个每次起跑都记一笔、然后挂住直到收到关闭信号的任务；`progress` 开着时先提交一个新 checkpoint。
fn hanging(log: &Log, progress: &Arc<AtomicBool>) -> Arc<Kind> {
    let (log, progress) = (log.clone(), progress.clone());
    let step = move |_task, rt: super::super::TaskRuntime| {
        let (log, progress) = (log.clone(), progress.clone());
        async move {
            log.push("run");
            if progress.swap(false, Ordering::SeqCst) {
                rt.commit_next(go("again")).await?;
            }
            rt.cancelled().cancelled().await;
            Ok(())
        }
    };
    kind("hanging")
        .phase("start", step.clone())
        .phase("again", step)
        .build()
}

/// 起来、等它跑起来，然后"崩溃"（不关引擎）。
async fn run_then_crash(env: &Env, log: &Log, runs: usize) {
    let engine = env.open_running().await;
    until(|| log.count("run") == runs).await;
    drop(engine);
}

fn reported(env: &Env) -> usize {
    env.reports
        .lines()
        .iter()
        .filter(|line| line.contains("restart_limit"))
        .count()
}

async fn restarts(env: &Env, id: TaskId) -> u32 {
    record(env, id).await.restarts
}

#[tokio::test(start_paused = true)]
async fn each_crash_while_running_counts_once_and_a_clean_close_does_not() {
    let env = Env::new();
    let log = Log::default();
    let k = hanging(&log, &Arc::new(AtomicBool::new(false)));
    env.install(&k);
    let engine = env.open_running().await;
    let id = spawn(&engine, &k, json!({})).await;
    until(|| log.count("run") == 1).await;
    drop(engine);

    run_then_crash(&env, &log, 2).await;
    run_then_crash(&env, &log, 3).await;
    // 第四次开库：前三代各在它跑着时崩了一次。
    let engine = env.open().await;
    assert_eq!(restarts(&env, id).await, 3);
    engine.resume();
    until(|| log.count("run") == 4).await;
    // 干净关闭：任务留在 running，但下一次开库不算它被打断。
    engine.close().await.unwrap();
    assert!(matches!(
        record(&env, id).await.state,
        TaskState::Running { .. }
    ));
    let engine = env.open().await;
    assert_eq!(record(&env, id).await.state.status(), TaskStatus::Pending);
    assert_eq!(restarts(&env, id).await, 3, "a clean close is not a crash");
    assert_eq!(
        inspected(&engine, id),
        TaskInspectionState::Ready { migrates: false }
    );
    engine.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reaching_the_limit_blocks_the_task_until_it_is_retried() {
    let env = Env::new();
    let log = Log::default();
    let k = hanging(&log, &Arc::new(AtomicBool::new(false)));
    env.install(&k);
    let engine = env.open_running().await;
    let id = spawn(&engine, &k, json!({})).await;
    until(|| log.count("run") == 1).await;
    drop(engine);
    for runs in 2..=RESTART_LIMIT as usize {
        run_then_crash(&env, &log, runs).await;
    }
    assert_eq!(reported(&env), 0);

    // 第 RESTART_LIMIT 次被打断：停住，报告一次，调度器不再起它，会话也因此一直忙着。
    let engine = env.open_running().await;
    assert_eq!(restarts(&env, id).await, RESTART_LIMIT);
    assert_eq!(
        inspected(&engine, id),
        TaskInspectionState::Blocked {
            reason: BlockedReason::RestartLimit
        }
    );
    assert_eq!(reported(&env), 1);
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    assert_eq!(log.count("run"), RESTART_LIMIT as usize, "never reserved");
    assert_eq!(record(&env, id).await.state.status(), TaskStatus::Pending);

    // 停住的任务再重开也不跑、也不再计数（它没在跑，崩溃打断不到它）。
    drop(engine);
    let engine = env.open_running().await;
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    assert_eq!(log.count("run"), RESTART_LIMIT as usize);
    assert_eq!(restarts(&env, id).await, RESTART_LIMIT);
    assert_eq!(reported(&env), 1, "reported once");

    // 放行：计数归零、接着跑。没被停住的任务放行不了。
    assert!(engine.retry_task(id).await.unwrap());
    until(|| log.count("run") == RESTART_LIMIT as usize + 1).await;
    assert_eq!(restarts(&env, id).await, 0);
    assert!(!engine.retry_task(id).await.unwrap());
    assert!(!engine.retry_task(TaskId::new(9_999)).await.unwrap());
    engine.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn committing_a_new_state_resets_the_count() {
    let env = Env::new();
    let log = Log::default();
    let progress = Arc::new(AtomicBool::new(false));
    let k = hanging(&log, &progress);
    env.install(&k);
    let engine = env.open_running().await;
    let id = spawn(&engine, &k, json!({})).await;
    until(|| log.count("run") == 1).await;
    drop(engine);
    run_then_crash(&env, &log, 2).await;
    run_then_crash(&env, &log, 3).await;
    assert_eq!(restarts(&env, id).await, 2);

    // 这一代任务提交了新 checkpoint：有进展，计数归零；之后再崩从 1 记起。
    progress.store(true, Ordering::SeqCst);
    let engine = env.open_running().await;
    assert_eq!(restarts(&env, id).await, 3);
    until(|| log.count("run") == 4).await;
    until(|| false_unless_progressed(&env, id)).await;
    assert_eq!(restarts(&env, id).await, 0);
    drop(engine);
    let engine = env.open().await;
    assert_eq!(restarts(&env, id).await, 1);
    engine.close().await.unwrap();
}

fn false_unless_progressed(env: &Env, id: TaskId) -> bool {
    futures_util::FutureExt::now_or_never(env.storage.task(id))
        .and_then(Result::ok)
        .flatten()
        .is_some_and(|t| {
            matches!(&t.state, TaskState::Running { checkpoint } if checkpoint["phase"] == "again")
        })
}

/// 第一次升级到带这个保护的版本：库里从没有过引擎标记，存量 `running` 不知道上一代怎么停的，不算崩溃。
#[tokio::test(start_paused = true)]
async fn the_first_open_without_a_marker_does_not_count_surviving_running_tasks() {
    let env = Env::new();
    let log = Log::default();
    let k = hanging(&log, &Arc::new(AtomicBool::new(false)));
    env.install(&k);
    // 老版本留下的库：根 conversation、一条 running 任务，没有 `komo.engine`。
    let mut stale = TaskRecord::pending(
        TaskId::new(50),
        ROOT_CONVERSATION_ID,
        "hanging",
        json!({}),
        json!({"phase": "start"}),
    );
    stale.state = TaskState::Running {
        checkpoint: json!({"phase": "start"}),
    };
    env.storage
        .commit(&[
            StorageWrite::Conversation(komo_kernel::durable::ConversationRecord::root(
                ConversationId::new(1),
            )),
            StorageWrite::Task(stale),
        ])
        .await
        .unwrap();
    assert!(
        env.storage
            .find_document(&engine_address())
            .await
            .unwrap()
            .is_none()
    );

    let engine = env.open_running().await;
    let id = TaskId::new(50);
    assert_eq!(restarts(&env, id).await, 0);
    let marker = env
        .storage
        .find_document(&engine_address())
        .await
        .unwrap()
        .expect("the open writes the marker");
    assert!(crashed_before(Some(&marker.value)), "cleared while open");
    until(|| log.count("run") == 1).await;
    // 从这一代起照常计数。
    drop(engine);
    let engine = env.open().await;
    assert_eq!(restarts(&env, id).await, 1);
    engine.close().await.unwrap();
    let marker = env
        .storage
        .find_document(&engine_address())
        .await
        .unwrap()
        .unwrap();
    assert!(!crashed_before(Some(&marker.value)), "set on a clean close");
}

/// 线失败之后的关闭不写干净关闭的标记（存储不可信）：下一次开库按崩溃算，偏保守。
#[tokio::test(start_paused = true)]
async fn a_close_after_the_line_failed_counts_as_a_crash() {
    let env = Env::new();
    let log = Log::default();
    let k = hanging(&log, &Arc::new(AtomicBool::new(false)));
    env.install(&k);
    let engine = env.open_running().await;
    let id = spawn(&engine, &k, json!({})).await;
    until(|| log.count("run") == 1).await;
    env.storage.fail_commits.store(true, Ordering::SeqCst);
    let failed = engine
        .commit(|tx| {
            Box::pin(async move {
                tx.append_entry(
                    ROOT_CONVERSATION_ID,
                    super::super::EntryDraft::new("test.entry"),
                )
                .await
                .map(|_| ())
            })
        })
        .await;
    assert!(failed.is_err());
    assert!(engine.failure().is_some());
    engine.close().await.unwrap();
    env.storage.fail_commits.store(false, Ordering::SeqCst);
    let engine = env.open().await;
    assert_eq!(restarts(&env, id).await, 1);
    engine.close().await.unwrap();
}

/// 放弃停住的任务：不跑它的代码，直接写 `faulted`（Gateway 的 intervention `abandon` 走这条）。
#[tokio::test(start_paused = true)]
async fn a_blocked_task_can_be_faulted_without_running_its_code() {
    let env = Env::new();
    let log = Log::default();
    let k = hanging(&log, &Arc::new(AtomicBool::new(false)));
    env.install(&k);
    let engine = env.open_running().await;
    let id = spawn(&engine, &k, json!({})).await;
    until(|| log.count("run") == 1).await;
    drop(engine);
    for runs in 2..=RESTART_LIMIT as usize {
        run_then_crash(&env, &log, runs).await;
    }
    let engine = env.open_running().await;
    assert!(matches!(
        inspected(&engine, id),
        TaskInspectionState::Blocked { .. }
    ));
    assert!(engine.fault_idle_task(id, "放弃").await.unwrap());
    let settled = settle(&engine, id).await;
    assert_eq!(faulted_message(&settled), "放弃");
    assert_eq!(log.count("run"), RESTART_LIMIT as usize);
    engine.close().await.unwrap();
}
