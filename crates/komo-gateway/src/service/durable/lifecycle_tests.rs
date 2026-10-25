//! 生命周期（W4a）：delete / purge / 启动补做 / 重启后 ID 不复用在网关一层的行为。
//! 起一台真 Gateway（Durable），HTTP 调 `/v1/sessions/{id}/delete|purge`。

use std::time::Duration;

use komo_kernel::durable::{InputState, SessionLifecycle, SubmissionKind, SubmissionStatus};
use komo_runtime::durable::harness::SubmissionDraft;
use serde_json::json;

use crate::service::test_support::Gate;
use crate::service::test_support::driver_stream::{core, open_session, submit};
use crate::service::test_support::harness::{
    FakeLlm, GatewayBuilder, TestGateway, call_round, telegram_config, text_round,
};

/// 一条 run 停在 `gate` 工具上，收件箱里再排一条用户输入与一条内部输入。
struct Busy {
    gateway: TestGateway,
    gate: std::sync::Arc<Gate>,
    llm: std::sync::Arc<FakeLlm>,
    session: String,
    first: komo_kernel::types::ids::RunId,
    queued: komo_kernel::durable::SubmissionId,
    internal: komo_kernel::durable::SubmissionId,
}

async fn busy() -> Busy {
    let gate = Gate::new();
    let llm = FakeLlm::new(vec![
        vec![
            call_round(1, "c1", "gate", json!({})),
            text_round(2, "第一条做完了"),
        ],
        vec![text_round(1, "不该有第二条 run")],
    ]);
    let gateway = GatewayBuilder::new(&telegram_config("111"))
        .durable()
        .fake_llm(llm.clone())
        .tool(gate.tool())
        .start()
        .await;
    let session = open_session(&gateway).await;
    let first = submit(&gateway, &session, "k1", "先跑").await;
    gate.wait_entered(1).await;
    let queued = submit(&gateway, &session, "k2", "排在后面的").await;
    let core = core(&gateway);
    let branch = core.session(&session).await.unwrap().current_branch;
    let internal = core
        .harness
        .submit(
            &core.engine,
            branch,
            SubmissionDraft::input("某个后台任务的结果")
                .internal()
                .with_request_id("task-result:test"),
        )
        .await
        .expect("内部输入排得进去");
    Busy {
        gateway,
        gate,
        llm,
        session,
        first: first.run,
        queued: queued.submission,
        internal,
    }
}

async fn aborted(gateway: &TestGateway, id: komo_kernel::durable::SubmissionId) -> bool {
    let record = core(gateway).storage.submission(id).await.unwrap().unwrap();
    matches!(
        record.kind,
        SubmissionKind::Input {
            state: InputState::Unanswered { ref reason, .. }
        } if reason == "aborted"
    )
}

async fn eventually(what: &str, mut done: impl AsyncFnMut() -> bool) {
    for _ in 0..200 {
        if done().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("等了 10 秒：{what}");
}

/// `now`：abort 在飞的 run，撤回排队的**用户与内部**输入，之后不起后继 run，进 `deleted`。
#[tokio::test]
async fn delete_now_aborts_the_run_and_withdraws_every_queued_input_including_internal() {
    let busy = busy().await;
    let (code, body) = busy
        .gateway
        .post_json(
            &format!("/v1/sessions/{}/delete", busy.session),
            json!({ "now": true }),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["state"], "deleted", "{body}");
    // 取消清单是"没结束的 run"：在飞的那条，加上排队的用户输入与内部输入各自将开的 run。
    let cancelled: Vec<String> = serde_json::from_value(body["cancelled"].clone()).unwrap();
    assert_eq!(
        cancelled,
        vec![
            busy.first.to_string(),
            busy.queued.get().to_string(),
            busy.internal.get().to_string()
        ],
        "{body}"
    );

    assert!(
        aborted(&busy.gateway, busy.queued).await,
        "排队的用户输入被撤回"
    );
    assert!(
        aborted(&busy.gateway, busy.internal).await,
        "排队的内部输入也被撤回：abort 之后不会因它起后继 run"
    );
    let core = core(&busy.gateway);
    assert!(
        core.work_remaining(&busy.session).await.unwrap().is_empty(),
        "什么都不剩"
    );
    assert_eq!(busy.llm.turns(), 1, "没有后继 run");
    assert_eq!(
        core.session(&busy.session).await.unwrap().lifecycle,
        SessionLifecycle::Deleted
    );
    // 幂等：再删一次还是 deleted，什么都没取消。
    let (code, body) = busy
        .gateway
        .post_json(
            &format!("/v1/sessions/{}/delete", busy.session),
            json!({ "now": true }),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["state"], "deleted");
    assert_eq!(body["cancelled"], json!([]));
}

/// 不带 `now`：`closing`（run 照跑完），排队的输入（含内部）被撤回所以不起后继 run；跑完后补做推进到 `deleted`。
#[tokio::test]
async fn a_closing_session_lets_the_run_finish_but_never_starts_a_successor() {
    let busy = busy().await;
    let (code, body) = busy
        .gateway
        .post_json(&format!("/v1/sessions/{}/delete", busy.session), json!({}))
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["state"], "closing", "{body}");
    assert!(aborted(&busy.gateway, busy.queued).await);
    assert!(aborted(&busy.gateway, busy.internal).await);
    // 手里的 run 还在跑。
    let core = core(&busy.gateway);
    assert!(!core.work_remaining(&busy.session).await.unwrap().is_empty());

    busy.gate.release();
    let record = busy.gateway.wait_run(&busy.first).await;
    assert_eq!(
        record.status(),
        SubmissionStatus::Done,
        "closing 只是不收新活"
    );
    eventually("补做把 closing 推进到 deleted", async || {
        core.finish_closing().await.unwrap();
        core.session(&busy.session).await.unwrap().lifecycle == SessionLifecycle::Deleted
    })
    .await;
    assert_eq!(busy.llm.turns(), 1, "没有后继 run");
}

/// purge 只从 `deleted` 起步；之后一次提交抹掉全部 conversation、墓碑 `purged`、再删目录；幂等。
#[tokio::test]
async fn purge_requires_deleted_then_removes_rows_and_directory_and_is_idempotent() {
    let gateway = GatewayBuilder::new(&telegram_config("111"))
        .durable()
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;
    let session = open_session(&gateway).await;
    let run = submit(&gateway, &session, "k1", "说句话").await.run;
    gateway.wait_run(&run).await;
    let dir = gateway.state().snapshot().paths.sessions_dir.join(&session);
    std::fs::create_dir_all(dir.join("tool-output")).unwrap();
    std::fs::write(dir.join("tool-output/blob"), b"0123456789").unwrap();

    let (code, body) = gateway
        .post_json(&format!("/v1/sessions/{session}/purge"), json!({}))
        .await;
    assert_eq!(code, 409, "没删就 purge：{body}");

    let (code, _) = gateway
        .post_json(&format!("/v1/sessions/{session}/delete"), json!({}))
        .await;
    assert_eq!(code, 200);
    let (code, body) = gateway
        .post_json(&format!("/v1/sessions/{session}/purge"), json!({}))
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["state"], "purged", "{body}");
    assert!(body["removed_bytes"].as_u64().unwrap() >= 10, "{body}");
    assert!(!dir.exists(), "目录删了");

    let core = core(&gateway);
    let conversations = core
        .storage
        .scan_conversations(
            &komo_kernel::durable::storage::ConversationQuery {
                session: Some(session.clone()),
                ..Default::default()
            },
            10,
            None,
        )
        .await
        .unwrap();
    assert!(conversations.items.is_empty(), "conversation 行都没了");
    assert_eq!(
        core.session(&session).await.unwrap().lifecycle,
        SessionLifecycle::Purged,
        "墓碑还在"
    );

    let (code, body) = gateway
        .post_json(&format!("/v1/sessions/{session}/purge"), json!({}))
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["removed_bytes"], 0, "{body}");
    let (code, body) = gateway.get_json(&format!("/v1/sessions/{session}")).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["state"], "purged");
}

/// 还有没做完的工作时 `work_remaining` 说得出是什么（purge 的 409 正文用它）。
#[tokio::test]
async fn unfinished_work_is_listed_as_blockers() {
    let busy = busy().await;
    let blockers = core(&busy.gateway)
        .work_remaining(&busy.session)
        .await
        .unwrap();
    let whats: Vec<&str> = blockers.iter().map(|b| b.what.as_str()).collect();
    assert!(whats.contains(&"unfinished_run"), "{whats:?}");
    assert!(whats.contains(&"queued_input"), "{whats:?}");
    assert!(whats.contains(&"task"), "{whats:?}");
    busy.gate.release();
}

/// 上一代停在 `closing` 的会话，这一代启动时做完（也收掉墓碑已落而目录还在的）。
#[tokio::test]
async fn a_closing_session_left_by_the_previous_generation_is_finished_on_start() {
    let mut first = GatewayBuilder::new(&telegram_config("111"))
        .durable()
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;
    let session = open_session(&first).await;
    let purged = open_session(&first).await;
    for (id, to) in [
        (session.clone(), SessionLifecycle::Closing),
        (purged.clone(), SessionLifecycle::Purged),
    ] {
        core(&first)
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    let mut record = tx.session(&id).await?.expect("会话在");
                    record.lifecycle = to;
                    tx.put_session(record);
                    Ok(())
                })
            })
            .await
            .unwrap();
    }
    let leftover = first.state().snapshot().paths.sessions_dir.join(&purged);
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("x"), b"x").unwrap();
    first.stop().await;

    let second = GatewayBuilder::new(&telegram_config("111"))
        .durable()
        .fake_llm(FakeLlm::finisher("好"))
        .at(&first.home)
        .start()
        .await;
    let core = core(&second);
    eventually("启动补做把 closing 推进到 deleted", async || {
        core.session(&session).await.unwrap().lifecycle == SessionLifecycle::Deleted
    })
    .await;
    assert!(!leftover.exists(), "墓碑已落的目录在启动时收掉");
}

/// 重启之后新铸的 ID 不复用 purge 掉的：gateway 层的回归（存储层的在 komo-store）。
#[tokio::test]
async fn ids_after_a_purge_and_restart_are_never_reused() {
    let mut first = GatewayBuilder::new(&telegram_config("111"))
        .durable()
        .fake_llm(FakeLlm::finisher("好"))
        .start()
        .await;
    let session = open_session(&first).await;
    let run = submit(&first, &session, "k1", "说句话").await.run;
    first.wait_run(&run).await;
    let seen = core(&first).storage.mint_id().await.unwrap();
    first
        .post_json(
            &format!("/v1/sessions/{session}/delete"),
            json!({"now": true}),
        )
        .await;
    let (code, body) = first
        .post_json(&format!("/v1/sessions/{session}/purge"), json!({}))
        .await;
    assert_eq!(code, 200, "{body}");
    first.stop().await;

    let second = GatewayBuilder::new(&telegram_config("111"))
        .durable()
        .fake_llm(FakeLlm::finisher("好"))
        .at(&first.home)
        .start()
        .await;
    let next = core(&second).storage.mint_id().await.unwrap();
    assert!(next > seen, "{next} 不该复用已发出的 {seen}");
}
