//! W2b 验收：提交订阅 → SSE 帧（`feed`）与事件补读。
//!
//! 起一台真的 Durable Gateway（真钩子、`FakeLlm` 当 `ModelStream`），自己 `feed::spawn`
//! （`start()` 里的接线由主控合并时补；`spawn` 对同一个 core 幂等，补上之后这里再调不会推两遍）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use komo_kernel::durable::{
    AssistantMessage, ContentBlock, ConversationId, StopReason, SubmissionId, TaskId,
};
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::traits::Shutdown;
use komo_kernel::types::ids::{RunId, Seq, SessionId};
use komo_kernel::types::status::RunState;
use komo_runtime::durable::harness::live::{self, GenerationLive, LiveState, RunControl};
use tokio::sync::broadcast::Receiver;

use super::feed;
use super::sessions::NewSession;
use super::submit::InputRequest;
use crate::service::test_support::harness::{FakeLlm, call_round, text_round};
use crate::service::test_support::{TestGateway, config_toml};

async fn gateway(llm: Arc<FakeLlm>) -> (TestGateway, Shutdown) {
    let gateway = TestGateway::durable(&config_toml(""), llm).await;
    let shutdown = Shutdown::new();
    let state = gateway.state();
    let core = state.core().expect("Durable");
    // 已由 `start()` 接好时返回 `false`（幂等），不当错误。
    let _ = feed::spawn(core, &state.hub, &shutdown).await;
    (gateway, shutdown)
}

async fn new_session(gateway: &TestGateway) -> String {
    let core = gateway.state().core().unwrap();
    core.create_session(NewSession::new("assistant"))
        .await
        .expect("建会话")
        .id
}

/// 等到 `done` 为真（最多 10 秒），期间把收到的帧都攒下来。
async fn gather(
    rx: &mut Receiver<SseFrame>,
    frames: &mut Vec<SseFrame>,
    what: &str,
    done: impl Fn(&[SseFrame]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(frames) {
        let left = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("等了 10 秒还没有：{what}\n{frames:#?}"));
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(frame)) => frames.push(frame),
            Ok(Err(error)) => panic!("订阅断了：{error}"),
            Err(_) => panic!("等了 10 秒还没有：{what}\n{frames:#?}"),
        }
    }
}

fn events_of(frames: &[SseFrame]) -> Vec<Event> {
    frames
        .iter()
        .filter_map(|frame| match &frame.event {
            SseEvent::Event(event) => Some((**event).clone()),
            _ => None,
        })
        .collect()
}

fn has_terminal(frames: &[SseFrame]) -> bool {
    frames.iter().any(|frame| {
        matches!(
            &frame.event,
            SseEvent::Event(event) if matches!(
                event.payload,
                EventPayload::RunCompleted(_) | EventPayload::RunFailed(_)
            )
        )
    })
}

#[tokio::test]
async fn live_frames_equal_backfill_frames() {
    let llm = FakeLlm::new(vec![
        vec![
            call_round(1, "c1", "bash", serde_json::json!({"command": "echo hi"})),
            text_round(2, "读完了"),
        ],
        vec![text_round(1, "第二段")],
    ]);
    let (gateway, shutdown) = gateway(llm).await;
    let session = new_session(&gateway).await;
    let core = gateway.state().core().unwrap();
    let mut rx = gateway
        .state()
        .hub
        .subscribe(&SessionId::from_raw(session.clone()));

    let mut frames = Vec::new();
    for (n, text) in ["先跑个命令", "再来一条"].into_iter().enumerate() {
        let accepted = core
            .submit_input(
                &session,
                InputRequest::new(text).request_id(format!("k-{n}")),
            )
            .await
            .expect("受理");
        gateway.wait_run(&accepted.run).await;
        let wanted = n + 1;
        gather(&mut rx, &mut frames, "run 终态", |frames| {
            frames
                .iter()
                .filter(|frame| {
                    matches!(
                        &frame.event,
                        SseEvent::Event(event) if matches!(event.payload, EventPayload::RunCompleted(_))
                    )
                })
                .count()
                >= wanted
        })
        .await;
    }
    core.boundary(&session, None).await.expect("boundary");
    gather(&mut rx, &mut frames, "boundary", |frames| {
        events_of(frames)
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ConversationBoundary(_)))
    })
    .await;

    let live = events_of(&frames);
    let backfill = feed::backfill(&*core.storage, &session, Seq::ZERO)
        .await
        .expect("补读");
    assert!(live.len() > 8, "帧数不对：{}", live.len());
    assert_eq!(
        serde_json::to_value(&live).unwrap(),
        serde_json::to_value(&backfill).unwrap(),
        "直播的帧与补读的帧逐个相同"
    );
    let seqs: Vec<u64> = live.iter().map(|event| event.seq.0).collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");

    // 状态帧照旧派生。输入 entry 的 ID 比放置它的 submission 小（`admit` 先铸 entry），帧按记录 ID
    // 排序，所以 `running` 排在 `accepted` / `queued` 之前（见 §10 W2b 的备注）。
    let states: Vec<RunState> = frames
        .iter()
        .filter_map(|frame| match &frame.event {
            SseEvent::RunStatus { state, .. } => Some(*state),
            _ => None,
        })
        .collect();
    assert_eq!(
        &states[..4],
        [
            RunState::Running,
            RunState::Accepted,
            RunState::Queued,
            RunState::Completed
        ],
        "{states:?}"
    );
    shutdown.cancel();
}

fn partial(text: &str) -> serde_json::Value {
    serde_json::to_value(AssistantMessage {
        content: vec![ContentBlock::text(text)],
        stop_reason: StopReason::Stop,
        timestamp: 0,
        extra: Default::default(),
    })
    .unwrap()
}

async fn write_live(
    gateway: &TestGateway,
    conversation: ConversationId,
    message: Option<serde_json::Value>,
    rounds: u32,
) {
    let core = gateway.state().core().unwrap();
    core.engine
        .commit(move |tx| {
            Box::pin(async move {
                let progress = tx
                    .document(&live::live_address(conversation))
                    .await?
                    .is_some();
                if progress {
                    tx.mark_progress();
                }
                let state = LiveState {
                    run: Some(RunControl {
                        task: TaskId::new(1),
                        inputs: vec![SubmissionId::new(7)],
                        rounds,
                        tokens: 0,
                    }),
                    generation: Some(GenerationLive {
                        attempt: 1,
                        message,
                        retry: None,
                    }),
                    ..LiveState::default()
                };
                live::store(tx, conversation, &state).await
            })
        })
        .await
        .expect("提交 komo.live");
}

#[tokio::test]
async fn assistant_delta_is_the_suffix_of_the_throttled_partial() {
    let (gateway, shutdown) = gateway(FakeLlm::finisher("好")).await;
    let id = new_session(&gateway).await;
    let core = gateway.state().core().unwrap();
    let conversation = core.session(&id).await.unwrap().root_conversation;
    let mut rx = gateway
        .state()
        .hub
        .subscribe(&SessionId::from_raw(id.clone()));

    let mut deltas: Vec<(RunId, u32, String)> = Vec::new();
    let mut frames = Vec::new();
    // 节流提交的部分回复：每次是到目前为止的全文。
    for (step, text) in ["你", "你好", "你好吗", "我换了说法"]
        .into_iter()
        .enumerate()
    {
        write_live(&gateway, conversation, Some(partial(text)), 0).await;
        gather(&mut rx, &mut frames, "增量", |frames| {
            frames
                .iter()
                .filter(|frame| matches!(frame.event, SseEvent::AssistantDelta { .. }))
                .count()
                > step
        })
        .await;
    }
    // 同样的内容再提交一次不重复发；下一轮（rounds + 1）从头开始。
    write_live(&gateway, conversation, Some(partial("我换了说法")), 0).await;
    write_live(&gateway, conversation, None, 1).await;
    write_live(&gateway, conversation, Some(partial("第二轮")), 1).await;
    gather(&mut rx, &mut frames, "第二轮的增量", |frames| {
        frames
            .iter()
            .filter(|frame| matches!(frame.event, SseEvent::AssistantDelta { .. }))
            .count()
            > 4
    })
    .await;
    for frame in &frames {
        if let SseEvent::AssistantDelta { run, round, text } = &frame.event {
            deltas.push((run.clone(), *round, text.clone()));
        }
    }
    let run = RunId::from_raw("7");
    assert_eq!(
        deltas,
        [
            (run.clone(), 1, "你".to_owned()),
            (run.clone(), 1, "好".to_owned()),
            (run.clone(), 1, "吗".to_owned()),
            (run.clone(), 1, "我换了说法".to_owned()),
            (run, 2, "第二轮".to_owned()),
        ],
        "后缀、前缀对不上从头发、不重复、下一轮 round + 1"
    );
    shutdown.cancel();
}

#[tokio::test]
async fn frames_of_other_sessions_are_filtered() {
    let llm = FakeLlm::new(vec![vec![text_round(1, "甲的回答")]]);
    let (gateway, shutdown) = gateway(llm).await;
    let a = new_session(&gateway).await;
    let b = new_session(&gateway).await;
    let core = gateway.state().core().unwrap();
    let hub = &gateway.state().hub;
    let mut rx_a = hub.subscribe(&SessionId::from_raw(a.clone()));
    let mut rx_b = hub.subscribe(&SessionId::from_raw(b.clone()));

    let accepted = core
        .submit_input(&a, InputRequest::new("只给甲").request_id("k"))
        .await
        .unwrap();
    gateway.wait_run(&accepted.run).await;
    let mut frames = Vec::new();
    gather(&mut rx_a, &mut frames, "甲的终态", has_terminal).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame.session.as_str() == a.as_str()),
        "甲的订阅里只有甲的帧"
    );
    // 给 feed 一点时间把可能错投的帧推出来，再确认乙什么也没收到。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(rx_b.try_recv().is_err(), "乙的订阅里没有任何帧");

    // HTTP 补读同样只有自己的会话。
    let (status, body) = gateway
        .get(&format!("/v1/sessions/{b}/events?from=0"))
        .await;
    assert_eq!(status, 200, "{body}");
    let page: komo_kernel::protocol::http::EventPage = serde_json::from_str(&body).unwrap();
    assert!(page.events.is_empty(), "{body}");
    let (status, _) = gateway.get("/v1/sessions/nope/events?from=0").await;
    assert_eq!(status, 404);
    shutdown.cancel();
}

/// 冒烟：20 个并发补读不拖慢提交（`docs/storage-durable-d5.md` §9 的风险：`TursoStorage` 读只有一条连接）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_backfills_do_not_stall_commits() {
    let (gateway, shutdown) = gateway(FakeLlm::finisher("好")).await;
    let id = new_session(&gateway).await;
    let core = gateway.state().core().unwrap();
    let conversation = core.session(&id).await.unwrap().root_conversation;
    // 一段不短的历史：300 条 entry。
    for _ in 0..3 {
        core.engine
            .commit(move |tx| {
                Box::pin(async move {
                    for n in 0..100 {
                        tx.append_entry(
                            conversation,
                            komo_runtime::durable::EntryDraft::new("label").with_data(
                                serde_json::json!({"targetId": "x", "label": format!("{n}")}),
                            ),
                        )
                        .await?;
                    }
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    async fn commits(
        core: &super::DurableCore,
        conversation: ConversationId,
        n: usize,
    ) -> Duration {
        let started = Instant::now();
        for _ in 0..n {
            core.engine
                .commit(move |tx| {
                    Box::pin(async move {
                        tx.append_entry(
                            conversation,
                            komo_runtime::durable::EntryDraft::new("label")
                                .with_data(serde_json::json!({"targetId": "x", "label": "y"})),
                        )
                        .await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
        }
        started.elapsed()
    }

    const N: usize = 40;
    let quiet = commits(core, conversation, N).await;

    let url = format!(
        "{}/v1/sessions/{id}/events?from=0&limit=5",
        gateway.base_url()
    );
    let token = gateway.token().to_owned();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut readers = Vec::new();
    for _ in 0..20 {
        let (url, token, stop, reads) = (url.clone(), token.clone(), stop.clone(), reads.clone());
        readers.push(tokio::spawn(async move {
            let client = reqwest::Client::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let response = client.get(&url).bearer_auth(&token).send().await.unwrap();
                assert_eq!(response.status(), 200);
                response.bytes().await.unwrap();
                reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let busy = commits(core, conversation, N).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for reader in readers {
        reader.await.unwrap();
    }
    let total = reads.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "并发补读冒烟：{N} 次提交，无读 {quiet:?}（{:?}/次）；20 路并发补读时 {busy:?}（{:?}/次），期间共完成 {total} 次补读",
        quiet / N as u32,
        busy / N as u32
    );
    assert!(total >= 20, "补读没有真正并发起来：{total}");
    assert!(
        busy < quiet * 10 + Duration::from_secs(2),
        "并发补读把提交拖慢太多：无读 {quiet:?}，并发 {busy:?}"
    );
    shutdown.cancel();
}
