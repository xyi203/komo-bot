//! 适配器的端到端断言：请求体长什么样、流断在半路会怎样、拒绝之后会不会偷偷再发一次。
//!
//! "服务端"是 [`ScriptedTransport`]——它按脚本吐字节并记下收到的请求体，所以这些断言
//! 不需要网络，也不需要进程里先装好 TLS provider（§13.4 那是 `main` 的事）。

use std::sync::Arc;

use komo_kernel::traits::LlmClient;
use komo_kernel::types::ids::{RunId, SessionId, ToolCallId};
use komo_kernel::types::model::{Effort, ModelConfig, ModelRole};
use komo_kernel::types::turn::{LlmError, RoundInput, ToolResultForModel, TurnRequest};
use serde_json::json;

use super::transport::testing::{Reply, ScriptedTransport};
use super::*;
use crate::config::{EffortCapabilities, Secrets};

fn model(effort: Option<&str>) -> ModelConfig {
    ModelConfig {
        provider: OPENAI_COMPATIBLE.into(),
        base_url: "https://llm.example.com/v1".into(),
        model: "chat-a".into(),
        api_key_env: "KOMO_LLM_API_KEY".into(),
        effort: effort.map(Effort::new),
        timeout_secs: 10,
    }
}

fn factory(transport: &ScriptedTransport) -> LlmFactory {
    LlmFactory::new(
        Arc::new(Secrets::from_pairs([(
            "KOMO_LLM_API_KEY",
            "sk-test-not-logged",
        )])),
        EffortCapabilities::builtin(),
    )
    .with_transport(Arc::new(transport.clone()))
}

fn request(config: &ModelConfig) -> TurnRequest {
    TurnRequest {
        session: SessionId::from_raw("s-1"),
        run: RunId::from_raw("r-1"),
        model: config.clone(),
        system_prompt: "你是 komo".into(),
        messages: vec![],
        tools: vec![],
        memories: vec![],
        covers: None,
    }
}

const ONE_WORD: &str = r#"{"choices":[{"delta":{"content":"好"},"finish_reason":"stop"}]}"#;

#[tokio::test]
async fn an_unset_effort_is_absent_from_the_request_body() {
    let transport = ScriptedTransport::new(vec![Reply::sse(&[ONE_WORD], true)]);
    let config = model(None);
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();
    let round = driver.next(RoundInput::First).await.unwrap();

    assert_eq!(round.text.as_deref(), Some("好"));
    let body = &transport.bodies()[0];
    assert!(
        body.get("reasoning_effort").is_none(),
        "未设置 effort 时请求体里没有这个字段：{body}"
    );
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["messages"][0]["content"], json!("你是 komo"));
    assert_eq!(
        transport.requests()[0].url,
        "https://llm.example.com/v1/chat/completions"
    );
}

#[tokio::test]
async fn an_explicit_effort_rides_along_as_reasoning_effort() {
    let transport = ScriptedTransport::new(vec![Reply::sse(&[ONE_WORD], true)]);
    let config = model(Some("high"));
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();
    driver.next(RoundInput::First).await.unwrap();
    assert_eq!(transport.bodies()[0]["reasoning_effort"], json!("high"));
}

/// §13.3：不支持的 effort 在**请求前**拒绝。
#[tokio::test]
async fn an_unsupported_effort_is_refused_before_anything_is_sent() {
    let transport = ScriptedTransport::new(vec![Reply::sse(&[ONE_WORD], true)]);
    let Err(error) = factory(&transport).build(&model(Some("ultra")), ModelRole::Main) else {
        panic!("不支持的档位必须在请求前被拒绝")
    };
    assert!(
        matches!(
            error,
            LlmBuildError::Model(LlmError::UnsupportedEffort { .. })
        ),
        "{error:?}"
    );
    assert!(transport.requests().is_empty(), "一个字节都不该发出去");
}

/// 同上，但这次是 Run 自己那份快照里的档位不可用——`begin_turn` 也要拦。
#[tokio::test]
async fn a_runs_own_snapshot_is_checked_too() {
    let transport = ScriptedTransport::new(vec![Reply::sse(&[ONE_WORD], true)]);
    let llm = factory(&transport)
        .build(&model(Some("high")), ModelRole::Main)
        .unwrap();

    let mut req = request(&model(Some("high")));
    req.model.effort = Some(Effort::new("ultra"));
    let Err(error) = llm.begin_turn(req).await else {
        panic!("Run 自己那份快照里的档位也要拦")
    };
    assert!(
        matches!(error, LlmError::UnsupportedEffort { .. }),
        "{error:?}"
    );
    assert!(transport.requests().is_empty());
    assert!(!is_retryable(&error), "重试只会再发一次同样的参数");
}

/// §13.3：模型 400 **不触发**静默删参重试。
#[tokio::test]
async fn a_400_about_the_effort_is_reported_and_never_retried_without_it() {
    let transport = ScriptedTransport::new(vec![
        Reply::json(
            400,
            r#"{"error":{"message":"unsupported reasoning_effort","code":"invalid_request_error"}}"#,
        ),
        Reply::sse(&[ONE_WORD], true),
    ]);
    let config = model(Some("high"));
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();
    let Err(error) = driver.next(RoundInput::First).await else {
        panic!("400 要原样报上来")
    };

    assert!(
        matches!(&error, LlmError::Rejected { status: 400, message } if message.contains("reasoning_effort")),
        "{error:?}"
    );
    assert!(!is_retryable(&error), "400 不是可重试的");
    let sent = transport.bodies();
    assert_eq!(sent.len(), 1, "只发了一次");
    assert_eq!(
        sent[0]["reasoning_effort"],
        json!("high"),
        "参数没有被悄悄删掉"
    );
}

/// 流没收到终止帧 = 可重试的失败，不是一个短回答。
#[tokio::test]
async fn a_stream_without_its_terminal_frame_is_a_retryable_failure() {
    let transport = ScriptedTransport::new(vec![Reply::sse(
        &[r#"{"choices":[{"delta":{"content":"半句"}}]}"#],
        false,
    )]);
    let config = model(None);
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();
    let Err(error) = driver.next(RoundInput::First).await else {
        panic!("没有终止帧就不是一个完整回复")
    };

    assert_eq!(error, LlmError::Incomplete);
    assert!(is_retryable(&error), "没收齐可以再来一次");
}

#[tokio::test]
async fn a_second_round_replays_the_assistant_block_and_the_tool_result() {
    let transport = ScriptedTransport::new(vec![
        Reply::sse(
            &[
                r#"{"choices":[{"delta":{"reasoning_content":"想"}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":9,"completion_tokens":4}}"#,
            ],
            true,
        ),
        Reply::sse(
            &[
                r#"{"choices":[{"delta":{"content":"读到了"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":2}}"#,
            ],
            true,
        ),
    ]);
    let config = model(None);
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();

    let first = driver.next(RoundInput::First).await.unwrap();
    assert_eq!(first.tool_calls.len(), 1);

    let second = driver
        .next(RoundInput::ToolResults {
            results: vec![ToolResultForModel {
                provider_call_id: "call_1".into(),
                call_id: ToolCallId::from_raw("tc-1"),
                content: "文件内容".into(),
                is_error: false,
            }],
        })
        .await
        .unwrap();
    assert_eq!(second.text.as_deref(), Some("读到了"));

    // 第二次请求里，assistant 那条是**原样**带回去的，工具结果按 call_id 配对。
    let body = &transport.bodies()[1];
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "system + assistant + tool：{body}");
    assert_eq!(messages[1]["role"], json!("assistant"));
    assert_eq!(messages[1]["reasoning_content"], json!("想"));
    assert_eq!(messages[2]["role"], json!("tool"));
    assert_eq!(messages[2]["tool_call_id"], json!("call_1"));

    // 用量按轮累加，不是最后一轮盖掉前面的。
    assert_eq!(driver.usage().input, Some(20));
    assert_eq!(driver.usage().output, Some(6));
}

#[tokio::test]
async fn a_missing_credential_is_refused_with_the_variable_name_not_a_value() {
    let transport = ScriptedTransport::new(vec![Reply::sse(&[ONE_WORD], true)]);
    let config = model(None);
    let llm = LlmFactory::new(Arc::new(Secrets::new()), EffortCapabilities::builtin())
        .with_transport(Arc::new(transport.clone()))
        .build(&config, ModelRole::Main)
        .unwrap();

    let Err(error) = llm.begin_turn(request(&config)).await else {
        panic!("没有凭证就不该开得了这一轮")
    };
    let LlmError::Rejected { status, message } = &error else {
        panic!("{error:?}")
    };
    assert_eq!(*status, 401);
    assert!(message.contains("KOMO_LLM_API_KEY"), "{message}");
    assert!(transport.requests().is_empty());
}

#[tokio::test]
async fn an_unknown_provider_is_refused_at_construction() {
    let transport = ScriptedTransport::new(vec![]);
    let mut config = model(None);
    config.provider = "anthropic_messages".into();
    let Err(error) = factory(&transport).build(&config, ModelRole::Main) else {
        panic!("不认识的 provider 不该造得出客户端")
    };
    assert!(matches!(error, LlmBuildError::UnknownProvider { .. }));
}

#[test]
fn retryability_is_decided_variant_by_variant() {
    assert!(is_retryable(&LlmError::Timeout));
    assert!(is_retryable(&LlmError::Incomplete));
    assert!(is_retryable(&LlmError::Transport("断了".into())));
    assert!(is_retryable(&LlmError::Rejected {
        status: 503,
        message: String::new()
    }));
    assert!(is_retryable(&LlmError::Rejected {
        status: 429,
        message: String::new()
    }));
    assert!(!is_retryable(&LlmError::Rejected {
        status: 401,
        message: String::new()
    }));
    assert!(!is_retryable(&LlmError::Unknown("结果不明".into())));
    assert!(!is_retryable(&LlmError::UnsupportedEffort {
        model: "m".into(),
        effort: "ultra".into()
    }));
}

/// §13.3：切换聊天模型不影响独立配置的记忆模型。
#[tokio::test]
async fn the_router_hands_each_run_the_instance_its_own_snapshot_names() {
    let transport = ScriptedTransport::new(vec![
        Reply::sse(&[ONE_WORD], true),
        Reply::sse(&[ONE_WORD], true),
    ]);
    let mut snapshot = crate::config::testing::snapshot_fixture();
    snapshot.model = model(None);
    snapshot.memory.model = ModelConfig {
        model: "memory-a".into(),
        effort: Some(Effort::new("low")),
        ..model(None)
    };

    let router = RoutingLlm::from_snapshot(&snapshot, factory(&transport)).unwrap();

    let mut main = router.begin_turn(request(&snapshot.model)).await.unwrap();
    main.next(RoundInput::First).await.unwrap();

    let mut memory = router
        .begin_turn(request(&snapshot.memory.model))
        .await
        .unwrap();
    memory.next(RoundInput::First).await.unwrap();

    let sent = transport.bodies();
    assert_eq!(sent[0]["model"], json!("chat-a"));
    assert!(sent[0].get("reasoning_effort").is_none());
    assert_eq!(
        sent[1]["model"],
        json!("memory-a"),
        "换聊天模型不影响记忆模型"
    );
    assert_eq!(
        sent[1]["reasoning_effort"],
        json!("low"),
        "记忆模型用自己的 effort"
    );
}
