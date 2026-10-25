//! 替身自己的测试：脚本化的模型、固定的嵌入、可拨的时钟。

use super::*;
use crate::traits::*;
use crate::types::ids::*;
use crate::types::model::{InputKind, TokenUsage};
use crate::types::turn::{ProviderToolCall, Round, RoundInput, TurnRequest};

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
                    ..Default::default()
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
                    ..Default::default()
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
                cache_session: None,
            })
            .await
            .unwrap();

        let first = driver.next(RoundInput::First).await.unwrap();
        assert_eq!(first.tool_calls.len(), 1);
        let second = driver
            .next(RoundInput::ToolResults {
                results: vec![],
                revised: Vec::new(),
            })
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
fn the_test_clock_can_be_wound_forward() {
    let clock = TestClock::fixed();
    let before = clock.now();
    clock.advance(time::Duration::hours(2));
    assert_eq!(clock.now() - before, time::Duration::hours(2));
}
