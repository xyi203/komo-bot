//! 替身自己的测试——它们要"够真"，所以这里断言的是它们真的按 seq 追加、真的幂等、
//! 真的核对计划哈希。

use super::*;
use crate::events::{Event, EventPayload};
use crate::traits::*;
use crate::types::ids::*;
use crate::types::model::{InputKind, TokenUsage};
use crate::types::plan::EnvVersion;
use crate::types::refs::{AttemptRef, ToolResultBody, ToolResultStatus};
use crate::types::status::RunEnd;
use crate::types::tool::{CancelToken, PyError, PythonJob, PythonResult};
use crate::types::turn::{
    AcceptInput, AssistantRound, ProviderToolCall, Round, RoundInput, ToolCallRequest, TurnRequest,
};

fn accept(session: &SessionId, key: &str, text: &str, clock: &TestClock) -> AcceptInput {
    AcceptInput {
        session: session.clone(),
        request_key: RequestKey::new(key),
        text: text.into(),
        source: crate::types::plan::PlanSource::Interactive {
            session: session.clone(),
        },
        peer: None,
        model: sample_model(),
        workdir: None,
        delegate: None,
        at: clock.now(),
    }
}

#[test]
fn the_mem_ledger_writes_a_log_that_actually_folds() {
    block_on(async {
        let clock = TestClock::fixed();
        let ledger = MemLedger::new(clock.clone());
        let session = SessionId::from_raw("sess-1");

        let accepted = ledger
            .accept_input(accept(&session, "api:1", "把 1 + 1 算出来", &clock))
            .await
            .unwrap();
        assert!(!accepted.deduplicated);

        let call = ToolCallId::from_raw("call-7");
        let calls = ledger
            .record_round(
                &accepted.run,
                AssistantRound {
                    round: 1,
                    text: Some("我算一下".into()),
                    text_ref: None,
                    tool_calls: vec![ToolCallRequest {
                        call_id: call.clone(),
                        provider_call_id: "pc-7".into(),
                        name: "python".into(),
                        arguments: serde_json::json!({"mode":"code","code":"result = 1 + 1"}),
                        arguments_ref: None,
                    }],
                    provider_blocks: None,
                    usage: TokenUsage::default(),
                },
            )
            .await
            .unwrap();
        assert_eq!(calls, vec![call.clone()]);

        let plan = sample_plan("python", &session);
        ledger.plan_call(&call, &plan).await.unwrap();
        let attempt = ledger.start_call(&call, &plan, None).await.unwrap();

        let store = MemOutputStore::new();
        let writer = store
            .begin(&AttemptRef {
                session: session.clone(),
                run: accepted.run.clone(),
                call: call.clone(),
                attempt: attempt.clone(),
            })
            .await
            .unwrap();
        let published = store
            .publish(
                writer,
                ToolResultBody {
                    status: ToolResultStatus::Completed,
                    result: serde_json::json!(2),
                    error: None,
                    exit_code: Some(0),
                    artifacts: vec![],
                    preview: None,
                },
            )
            .await
            .unwrap();
        ledger
            .finish_call(&attempt, published.clone())
            .await
            .unwrap();

        ledger
            .record_round(
                &accepted.run,
                AssistantRound {
                    round: 2,
                    text: Some("等于 2".into()),
                    text_ref: None,
                    tool_calls: vec![],
                    provider_blocks: None,
                    usage: TokenUsage::default(),
                },
            )
            .await
            .unwrap();
        ledger
            .complete(
                &accepted.run,
                RunEnd::Completed {
                    final_message: Some("等于 2".into()),
                    rounds: 2,
                },
            )
            .await
            .unwrap();

        let surface = ledger.surface();
        assert_eq!(surface.violations, vec![], "写出来的日志自己是交替的");
        assert_eq!(
            surface.runs[&accepted.run].status,
            crate::types::status::RunState::Completed
        );
        assert_eq!(
            surface.calls[&call].state,
            crate::types::status::ToolCallState::Completed
        );
        assert!(surface.replay_alternates());

        // 每一行都是合法 JSONL，且读回来一模一样。
        for line in ledger.to_jsonl().lines() {
            assert!(!line.is_empty());
            Event::from_line(line).expect("读得回来");
        }

        // 发布过的输出读得回来，哈希也对得上。
        let verified = store.open(&published.output).await.unwrap();
        assert_eq!(verified.body.result, serde_json::json!(2));
    });
}

#[test]
fn the_same_request_key_returns_the_original_run() {
    block_on(async {
        let clock = TestClock::fixed();
        let ledger = MemLedger::new(clock.clone());
        let session = SessionId::from_raw("sess-1");

        let first = ledger
            .accept_input(accept(&session, "telegram:42", "你好", &clock))
            .await
            .unwrap();
        let again = ledger
            .accept_input(accept(&session, "telegram:42", "你好", &clock))
            .await
            .unwrap();
        assert_eq!(first.run, again.run);
        assert!(again.deduplicated);

        let conflict = ledger
            .accept_input(accept(&session, "telegram:42", "别的话", &clock))
            .await
            .unwrap_err();
        assert!(matches!(conflict, LedgerError::RequestKeyConflict { .. }));
    });
}

#[test]
fn an_audit_backfill_is_idempotent_and_keeps_the_original_time() {
    block_on(async {
        let clock = TestClock::fixed();
        let ledger = MemLedger::new(clock.clone());
        let session = SessionId::from_raw("sess-1");
        let event_id = EventId::from_raw("outbox-1");
        let decided_at = time::macros::datetime!(2026-09-15 07:30:00 UTC);
        let payload = EventPayload::ApprovalDecided(crate::events::ApprovalDecided {
            approval: ApprovalId::from_raw("ap-1"),
            approved: true,
            scope: crate::types::chat::ApprovalScope::Once,
            by: None,
            decided_at,
            grant: None,
        });

        clock.advance(time::Duration::hours(1));
        let first = ledger
            .append_audit(&session, &event_id, payload.clone(), decided_at)
            .await
            .unwrap();
        let again = ledger
            .append_audit(&session, &event_id, payload, decided_at)
            .await
            .unwrap();
        assert_eq!(first, again, "同一 event_id 复用原事件位置");
        assert_eq!(ledger.events().len(), 1);
        assert_eq!(ledger.events()[0].ts, decided_at, "保留原始发生时间");
    });
}

#[test]
fn the_scripted_driver_hands_back_the_rounds_in_order() {
    block_on(async {
        let llm = ScriptedLlm::once(vec![
            Round {
                round: 1,
                text: None,
                tool_calls: vec![ProviderToolCall {
                    provider_call_id: "pc-1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "a.txt"}),
                }],
                provider_blocks: None,
                usage: TokenUsage {
                    input: Some(10),
                    output: Some(5),
                    reasoning: None,
                },
                truncated: false,
            },
            Round {
                round: 2,
                text: Some("好了".into()),
                tool_calls: vec![],
                provider_blocks: None,
                usage: TokenUsage {
                    input: Some(20),
                    output: Some(3),
                    reasoning: None,
                },
                truncated: false,
            },
        ]);

        let mut driver = llm
            .begin_turn(TurnRequest {
                session: SessionId::from_raw("sess-1"),
                run: RunId::from_raw("run-1"),
                model: sample_model(),
                system_prompt: String::new(),
                messages: vec![],
                tools: vec![],
                memories: vec![],
                covers: None,
            })
            .await
            .unwrap();

        let first = driver.next(RoundInput::First).await.unwrap();
        assert_eq!(first.tool_calls.len(), 1);
        let second = driver
            .next(RoundInput::ToolResults { results: vec![] })
            .await
            .unwrap();
        assert!(second.tool_calls.is_empty());
        assert_eq!(driver.usage().input, Some(30));
        assert!(driver.next(RoundInput::First).await.is_err(), "脚本演完了");
    });
}

#[test]
fn the_fixed_embedding_client_is_deterministic_and_normalized() {
    block_on(async {
        let client = FixedEmbeddingClient::new(8);
        let a = client
            .embed(InputKind::Document, &["空调滤芯换过了".to_string()])
            .await
            .unwrap();
        let b = client
            .embed(InputKind::Query, &["空调滤芯换过了".to_string()])
            .await
            .unwrap();
        assert_eq!(a, b);
        assert!(a[0].is_usable(8));

        client.set_down(true);
        assert!(
            client
                .embed(InputKind::Query, &["x".to_string()])
                .await
                .is_err()
        );
    });
}

#[test]
fn the_fake_python_host_records_calls_and_streams_stdout() {
    block_on(async {
        let host = FakePythonHost::new();
        host.push_stdout("hello\n");
        host.push_result(PythonResult {
            status: ToolResultStatus::Completed,
            result: serde_json::json!(2),
            error: None,
            artifacts: vec![],
            env_version: EnvVersion("py-test-1".into()),
        });

        let mut writer = MemOutputWriter::new(AttemptRef {
            session: SessionId::from_raw("s"),
            run: RunId::from_raw("r"),
            call: ToolCallId::from_raw("c"),
            attempt: AttemptId::from_raw("a"),
        });
        let result = host
            .run(
                PythonJob::Code {
                    code: "result = 1 + 1".into(),
                },
                &mut writer,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.result, serde_json::json!(2));
        assert_eq!(writer.stdout, b"hello\n");
        assert_eq!(host.calls().len(), 1);

        let cancelled = CancelToken::new();
        cancelled.cancel();
        assert!(matches!(
            host.run(PythonJob::Code { code: "x".into() }, &mut writer, cancelled)
                .await,
            Err(PyError::Cancelled)
        ));
    });
}

#[test]
fn the_test_clock_can_be_wound_forward() {
    let clock = TestClock::fixed();
    let before = clock.now();
    clock.advance(time::Duration::hours(2));
    assert_eq!(clock.now() - before, time::Duration::hours(2));
}

#[test]
fn reading_a_session_comes_back_one_page_at_a_time() {
    block_on(async {
        let clock = TestClock::fixed();
        let ledger = MemLedger::new(clock.clone());
        let session = SessionId::from_raw("sess-1");

        // 三条输入 = 六条事件（每条 accepted + queued）。
        for n in 0..3 {
            ledger
                .accept_input(accept(&session, &format!("api:{n}"), "你好", &clock))
                .await
                .unwrap();
        }

        let mut seen = Vec::new();
        let mut cursor = Seq::ZERO;
        let mut pages = 0;
        loop {
            let batch = ledger.read(&session, cursor, 2).await.unwrap();
            pages += 1;
            assert!(batch.events.len() <= 2, "一页不超过 limit");
            seen.extend(batch.events.iter().map(|e| e.seq));
            match batch.next {
                Some(next) => {
                    assert!(batch.has_more());
                    cursor = next;
                }
                None => {
                    assert!(!batch.has_more(), "读到头了");
                    break;
                }
            }
        }
        assert_eq!(pages, 3);
        assert_eq!(
            seen,
            (1..=6).map(Seq).collect::<Vec<_>>(),
            "翻完之后一条不多一条不少"
        );

        // 读到末尾之后再读，是一页空 + next=None，不是"又一页"。
        let tail = ledger.read(&session, Seq(6), 2).await.unwrap();
        assert!(tail.events.is_empty());
        assert!(!tail.has_more());
    });
}

#[test]
fn a_claimed_run_is_written_down_as_started() {
    block_on(async {
        let clock = TestClock::fixed();
        let ledger = MemLedger::new(clock.clone());
        let queue = MemRunQueue::new();
        let session = SessionId::from_raw("sess-1");

        let accepted = ledger
            .accept_input(accept(&session, "api:1", "跑一下测试", &clock))
            .await
            .unwrap();
        queue.enqueue(accepted.run.clone());

        let executor = ExecutorId::from_raw("ex-1");
        let claimed = queue.claim(&executor).await.unwrap().unwrap();
        ledger
            .start_run(&claimed.run, &executor, claimed.generation)
            .await
            .unwrap();

        let surface = ledger.surface();
        let run = &surface.runs[&accepted.run];
        assert_eq!(run.status, crate::types::status::RunState::Running);
        assert_eq!(
            run.generation,
            Some(claimed.generation),
            "领取代次跟着 run.started 进日志——JSONL 追加和状态提交都要校验它"
        );
        assert!(
            ledger
                .events()
                .iter()
                .any(|e| e.type_name() == "run.started")
        );
    });
}
