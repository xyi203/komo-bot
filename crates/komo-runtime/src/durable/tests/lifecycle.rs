//! 关闭、存储错误让 Session 失败、事务规则（spec §4、§5.1 close、§10.1）。对应 pi
//! `harness-tasks.test.ts` 的 "task close" / "task scheduling"，与 `harness-lifecycle.test.ts`。

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use komo_kernel::durable::{
    DocumentAddress, DocumentScope, ROOT_CONVERSATION_ID, TaskId, TaskState, TaskStatus,
};
use serde_json::json;

use super::super::{ConversationOwnership, DurableError, EntryDraft, TaskInspectionState};
use super::support::*;

/// pi: "stops without outcomes and starts no fresh phase or abort invocation while closing"：关闭写不下任何
/// 结局；正在跑的 phase 即使不听信号，它返回之后也不会起下一个阶段；重开后从 checkpoint 继续。
#[tokio::test(start_paused = true)]
async fn close_writes_no_outcome_and_starts_no_fresh_phase() {
    let env = Env::new();
    let log = Log::default();
    let (first_log, second_log) = (log.clone(), log.clone());
    let k = kind("closing")
        .phase("start", move |_task, rt| {
            let log = first_log.clone();
            async move {
                rt.commit_next(go("second")).await?;
                log.push("first committed");
                // 不听信号地做完手头的事，close 要等它。
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
    env.install(&k);
    let engine = env.open_running().await;
    let id = spawn(&engine, &k, json!({})).await;
    until(|| log.count("first committed") == 1).await;
    engine.close().await.unwrap();
    assert_eq!(log.count("second ran"), 0);
    let stored = record(&env, id).await;
    assert!(
        matches!(stored.state, TaskState::Running { .. }),
        "{stored:?}"
    );
    assert!(!stored.abort_requested);

    // 重开：running 回 pending，checkpoint 不变；恢复运行后从 `second` 继续。
    let engine = env.open().await;
    assert_eq!(record(&env, id).await.state.status(), TaskStatus::Pending);
    assert_eq!(
        engine.inspect()[0].state,
        TaskInspectionState::Ready { migrates: false }
    );
    engine.resume();
    assert_eq!(status(outcome_of(&settle(&engine, id).await)), "completed");
    assert_eq!(log.count("second ran"), 1);
    engine.close().await.unwrap();
}

/// pi: "rejects task waits cancelled or closed while queued on the line, and pending waits on close" 与
/// "rejects conversation and Harness operations once close begins"。
#[tokio::test(start_paused = true)]
async fn close_rejects_pending_waits_and_later_commits() {
    let env = Env::new();
    let clock = env.clock.clone();
    let k = kind("long")
        .phase("start", move |_task, rt| {
            let until = clock.now_ms() + 3_600_000;
            async move {
                rt.sleep_until(until).await?;
                Ok(())
            }
        })
        .build();
    env.install(&k);
    let engine = Arc::new(env.open_running().await);
    let id = spawn(&engine, &k, json!({})).await;
    let waiting = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.wait_for_task(id).await })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    engine.close().await.unwrap();
    assert!(matches!(waiting.await.unwrap(), Err(DurableError::Closed)));
    assert!(matches!(
        engine.commit(|_tx| Box::pin(async move { Ok(()) })).await,
        Err(DurableError::Closed)
    ));
    assert!(matches!(
        engine.wait_for_task(id).await,
        Err(DurableError::Closed)
    ));
    assert!(engine.failure().is_none(), "closing is not a failure");
}

/// pi: "fails the Harness on a failed reservation commit; reopening runs the task" 与 spec §10.1：
/// 存储错误让整条线失败，之后不再受理；重开后任务照常运行。
#[tokio::test(start_paused = true)]
async fn a_storage_error_fails_the_session_and_reopening_recovers() {
    let env = Env::new();
    let k = kind("simple")
        .phase("start", |_task, rt| async move {
            rt.commit_next(done(json!(1))).await?;
            Ok(())
        })
        .build();
    env.install(&k);
    let engine = env.open().await;
    let id = spawn(&engine, &k, json!({})).await;
    env.storage.fail_commits.store(true, Ordering::SeqCst);
    engine.resume(); // reservation 提交失败
    until(|| engine.failure().is_some()).await;
    assert!(matches!(engine.failure(), Some(DurableError::Storage(_))));
    let error = engine
        .commit(|_tx| Box::pin(async move { Ok(()) }))
        .await
        .unwrap_err();
    assert!(matches!(error, DurableError::Failed(_)), "{error:?}");
    assert!(matches!(
        engine.wait_for_task(id).await,
        Err(DurableError::Failed(_))
    ));
    engine.close().await.unwrap();
    assert_eq!(record(&env, id).await.state.status(), TaskStatus::Pending);

    env.storage.fail_commits.store(false, Ordering::SeqCst);
    let engine = env.open_running().await;
    assert_eq!(status(outcome_of(&settle(&engine, id).await)), "completed");
    engine.close().await.unwrap();
}

/// pi: "fails the Harness instead of running a task again when its fault write fails"：step 写 fault 失败
/// 时线失败，不会再把这个任务当作还在跑去重跑。
#[tokio::test(start_paused = true)]
async fn a_failed_fault_write_fails_the_session_instead_of_rerunning_the_task() {
    let env = Env::new();
    let log = Log::default();
    let run_log = log.clone();
    let storage = env.storage.clone();
    let k = kind("faulty")
        .phase("start", move |_task, _rt| {
            let (log, storage) = (run_log.clone(), storage.clone());
            async move {
                log.push("ran");
                // 返回前让之后的提交（step 写的 fault）失败。
                storage.fail_commits.store(true, Ordering::SeqCst);
                Err("boom".into())
            }
        })
        .build();
    env.install(&k);
    let engine = env.open_running().await;
    let id = spawn(&engine, &k, json!({})).await;
    until(|| engine.failure().is_some()).await;
    assert_eq!(log.count("ran"), 1);
    engine.close().await.unwrap();
    env.storage.fail_commits.store(false, Ordering::SeqCst);
    let stored = record(&env, id).await;
    assert!(matches!(stored.state, TaskState::Running { .. }));
}

/// spec §4 表规则："Any table read after the first table write throws `ReadAfterWrite`. Document access and
/// read-your-writes remain available after table writes."
#[tokio::test(start_paused = true)]
async fn table_reads_after_a_table_write_are_rejected_but_documents_stay_available() {
    let env = Env::new();
    let engine = env.open().await;
    let address = DocumentAddress {
        kind: "test.doc".into(),
        scope: DocumentScope::Session,
        key: None,
    };
    engine
        .commit(move |tx| {
            Box::pin(async move {
                assert!(
                    tx.conversation(ROOT_CONVERSATION_ID).await?.is_some(),
                    "table reads come first"
                );
                tx.append_entry(ROOT_CONVERSATION_ID, EntryDraft::new("test.entry"))
                    .await?;
                let read = tx.conversation(ROOT_CONVERSATION_ID).await;
                assert!(
                    matches!(read, Err(DurableError::ReadAfterWrite(_))),
                    "{read:?}"
                );
                assert!(matches!(
                    tx.task(TaskId::new(5)).await,
                    Err(DurableError::ReadAfterWrite(_))
                ));
                assert_eq!(tx.document(&address).await?, None);
                tx.put_document(address.clone(), 1, json!({"n": 1})).await?;
                assert_eq!(tx.document(&address).await?, Some((1, json!({"n": 1}))));
                tx.put_document(address.clone(), 1, json!({"n": 2})).await?;
                assert_eq!(tx.document(&address).await?, Some((1, json!({"n": 2}))));
                Ok(())
            })
        })
        .await
        .unwrap();
    engine.close().await.unwrap();
}

/// 交回结果要在写过自己的 conversation 之后读**别的** conversation 的请求键与 head 标记：这次提交
/// 没碰过它（也没碰过它的 fork 祖先）时已提交的读是精确的，放行；碰过就仍然拒绝。
#[tokio::test(start_paused = true)]
async fn reading_another_conversation_after_a_write_is_allowed_only_when_untouched() {
    let env = Env::new();
    let engine = env.open().await;
    let other = engine
        .commit(|tx| {
            Box::pin(async move {
                Ok(tx
                    .create_conversation(ConversationOwnership::Ownerless)
                    .await?
                    .id)
            })
        })
        .await
        .unwrap();
    engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.append_entry(ROOT_CONVERSATION_ID, EntryDraft::new("test.entry"))
                    .await?;
                // 别的 conversation 没被碰过：读得了请求键、head 标记。
                assert!(tx.submission_by_request(other, "k").await?.is_none());
                assert!(tx.latest_head_marker(other).await?.is_none());
                assert!(tx.conversation(other).await?.is_some());
                // 自己写过的：仍然不许读。
                assert!(matches!(
                    tx.submission_by_request(ROOT_CONVERSATION_ID, "k").await,
                    Err(DurableError::ReadAfterWrite(_))
                ));
                // 碰过 `other` 之后它也不行了。
                tx.append_entry(other, EntryDraft::new("test.entry"))
                    .await?;
                assert!(matches!(
                    tx.latest_head_marker(other).await,
                    Err(DurableError::ReadAfterWrite(_))
                ));
                Ok(())
            })
        })
        .await
        .unwrap();
    engine.close().await.unwrap();
}

/// spec §4："Only committed state is observable"：闭包出错整笔不生效、不发布；成功才发布，且发布时存储里
/// 已能读到。
#[tokio::test(start_paused = true)]
async fn only_committed_batches_are_published_and_a_failing_closure_writes_nothing() {
    let env = Env::new();
    let engine = env.open().await;
    let seen: Arc<Mutex<Vec<usize>>> = Default::default();
    let sink = seen.clone();
    engine.line().subscribe_commits(move |publication| {
        sink.lock().unwrap().push(publication.writes.len());
    });
    let failed = engine
        .commit(|tx| {
            Box::pin(async move {
                tx.append_entry(ROOT_CONVERSATION_ID, EntryDraft::new("test.lost"))
                    .await?;
                Err::<(), _>(DurableError::rejected("changed my mind"))
            })
        })
        .await;
    assert!(failed.is_err());
    assert!(seen.lock().unwrap().is_empty(), "nothing published");
    let entry = engine
        .commit(|tx| {
            Box::pin(async move {
                Ok(tx
                    .append_entry(ROOT_CONVERSATION_ID, EntryDraft::new("test.kept"))
                    .await?
                    .id)
            })
        })
        .await
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), [1]);
    let stored = futures_util::FutureExt::now_or_never(komo_kernel::durable::Storage::entry(
        &*env.storage,
        entry,
    ));
    assert!(stored.unwrap().unwrap().is_some());
    // 只读的提交不产生写入，也不发布。
    engine
        .commit(|_tx| Box::pin(async move { Ok(()) }))
        .await
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
    engine.close().await.unwrap();
}

/// spec §4："Mutation admission occurs on the Session line"：并发提交按到达顺序串行，铸出的 ID 递增。
#[tokio::test(start_paused = true)]
async fn concurrent_commits_run_serially_in_arrival_order() {
    let env = Env::new();
    let engine = Arc::new(env.open().await);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let engine = engine.clone();
        handles.push(tokio::spawn(async move {
            engine
                .commit(|tx| {
                    Box::pin(async move {
                        Ok(tx
                            .append_entry(ROOT_CONVERSATION_ID, EntryDraft::new("test.n"))
                            .await?
                            .id)
                    })
                })
                .await
                .unwrap()
        }));
        tokio::task::yield_now().await;
    }
    let mut ids = Vec::new();
    for handle in handles {
        ids.push(handle.await.unwrap());
    }
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
    engine.close().await.unwrap();
}
