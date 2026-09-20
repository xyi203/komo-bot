//! §8.9：**对账是只读观察 + 只写状态，不改写任何内容。**
//!
//! 这一条钉的是一个真 bug：把会话目录搬走之后跑一次对账，`read` 那条路顺手把目录
//! **建了回来**，于是"内容读不出来"永远判不出来——一条停在 `waiting + retry` 上的 Run
//! 会被当成"内容在"，照空上下文继续。
//!
//! 判据是目录本身：观察之后它必须**还是不在**。搬走目录这一步要先停机——同一个进程里
//! `SessionLedgers` 已经把那个会话开过了，走缓存就不会触发建目录那条路，只有新进程才
//! 会真的去碰磁盘（也正是线上那次冒烟的形状：停机、搬走、重启）。
//!
//! 顺带钉住第二次对账的幂等：判的还是同一件事、目录还是不建、清单上还是那一条。

use komo_kernel::types::ids::{RequestKey, RunId, SessionId};
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::status::{RetryCause, RunState, WaitReason};

use crate::harness::*;

/// 直接往库里落一条 `waiting + retry` 的 Run。
///
/// 走 store 而不是让模型真的失败一轮：要造的正是"模型端点不可达、让出名额等退避"那一刻
/// 的现场，而到点时刻由测试给定（落在将来）——不用等真的退避，也不会被对账的"到点放行"
/// （`release_due_waits`）顺手放回队列。
async fn seed_waiting_retry(
    gw: &Gw,
    session: &SessionId,
    key: &str,
    not_before: time::OffsetDateTime,
) -> RunId {
    let state = gw.state();
    let now = state.clock.now();
    let run = RunId::new_at(now);
    let new = komo_store::repos::runs::NewRun {
        run: run.clone(),
        session: session.clone(),
        request_key: RequestKey::new(key),
        input_hash: "seed".into(),
        source: PlanSource::Interactive {
            session: session.clone(),
        },
        peer: None,
        model: state.snapshot().model.clone(),
        effort: None,
        // 这条 Run 不是被派出来的（§4）：委派那条路要带 DelegateSpec。
        delegate: None,
        at: now,
    };
    let wait = WaitReason::Retry {
        attempts: 1,
        not_before,
        cause: RetryCause::Transport,
    };
    let db = state.db.clone();
    let for_write = run.clone();
    db.with_write_retry(move |ex| {
        let (new, wait, for_write) = (new.clone(), wait.clone(), for_write.clone());
        Box::pin(async move {
            komo_store::repos::runs::reserve_in(ex, &new).await?;
            komo_store::repos::runs::mark_waiting_in(ex, &for_write, &wait, None, now).await
        }) as komo_store::db::BoxFuture<'_, Result<(), komo_kernel::traits::StoreError>>
    })
    .await
    .expect("落一条等退避的 Run");
    run
}

/// 会话目录没了：对账把它判成 `waiting + intervention`，**并且一个字都不写回去**。
#[tokio::test]
async fn reconcile_never_recreates_a_missing_session_directory() {
    let home = Home::with_config(&config_toml(""));
    let gw = home.start(FakeLlm::finisher("好。")).await;
    let session = gw.open_session().await;

    // 先让这个会话真的有过内容：一轮正常对话把事件写进 `sessions/<id>/events.jsonl`。
    let done = gw.submit(&session, "before", "先聊一轮").await.run;
    gw.wait_terminal(&done).await;
    assert!(
        home.events_path(&session).exists(),
        "会话内容先要在，这样「搬走」才是一次真的内容缺失"
    );

    // 再落一条停在 `waiting + retry` 上的 Run（模型端点不可达那一刻的现场）。
    let now = gw.state().clock.now();
    let run = seed_waiting_retry(&gw, &session, "stuck", now + time::Duration::HOUR).await;
    assert_eq!(gw.run_state(&run).await, RunState::Waiting);

    // 停机、搬走会话目录、再起来——新进程的 `SessionLedgers` 是空的，读会话会真的去碰
    // 磁盘。这正是线上冒烟的形状（§8.10：数据库说有、内容说没有）。
    gw.stop().await;
    std::fs::remove_dir_all(home.session_dir(&session)).expect("搬走会话目录");
    assert!(!home.session_dir(&session).exists());

    // 重启：启动对账跑在 `service::start` 里，就落在这一句上。
    let gw = home.start(FakeLlm::finisher("好。")).await;

    // 对账判成等人判断，理由说得出来是"内容"。
    let (code, body) = gw.post_json("/v1/reconcile", serde_json::json!({})).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["blocked"].as_u64(), Some(1), "停在等人判断上：{body}");
    assert_eq!(gw.run_state(&run).await, RunState::Waiting);
    let wait = gw.db_wait(&run).await.expect("waiting 说得出来在等什么");
    assert!(
        matches!(wait, WaitReason::Intervention { .. }),
        "要停在干预上：{wait:?}"
    );
    let listed = gw.interventions().await;
    let entry = listed
        .iter()
        .find(|one| one.run.as_ref() == Some(&run))
        .unwrap_or_else(|| panic!("这一条要在清单里：{listed:?}"));
    assert!(
        entry.question.contains("内容"),
        "理由要说清是内容读不出来：{entry:?}"
    );

    // **观察不改写**：这一趟没有任何东西把搬走的目录建回来。
    assert!(
        !home.session_dir(&session).exists(),
        "对账是只读观察，不能把搬走的会话目录又建回来（§8.9）"
    );
    assert!(
        !home.events_path(&session).exists(),
        "连日志文件名都不该被重新落下"
    );

    // 幂等：再跑一次，判的还是同一件事，目录还是不建，清单上还是那一条。
    let (code, body) = gw.post_json("/v1/reconcile", serde_json::json!({})).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["blocked"].as_u64(), Some(1), "{body}");
    assert!(
        !home.session_dir(&session).exists(),
        "第二次对账同样不该建目录"
    );
    let again = gw.interventions().await;
    assert_eq!(
        again
            .iter()
            .filter(|one| one.run.as_ref() == Some(&run))
            .count(),
        1,
        "一个 Run 上至多停着一条要人判断的 Intervention（§7.5）：{again:?}"
    );
}
