//! 适配器的端到端断言：请求体长什么样、流断在半路会怎样、拒绝之后会不会偷偷再发一次。
//!
//! "服务端"是 [`ScriptedTransport`]——它按脚本吐 SSE 字节并记下收到的请求体，所以这些
//! 断言不需要网络，也不需要进程里先装好 TLS provider（§13.4 那是 `main` 的事）。

use std::sync::Arc;

use komo_kernel::traits::LlmClient;
use komo_kernel::types::ids::{RunId, SessionId, ToolCallId};
use komo_kernel::types::model::{Effort, ModelConfig, ModelRole};
use komo_kernel::types::tool::ToolDefinition;
use komo_kernel::types::turn::{LlmError, RoundInput, ToolResultForModel, TurnRequest};
use serde_json::{Value, json};

use super::transport::testing::{Reply, ScriptedTransport};
use super::*;
use crate::config::{EffortCapabilities, Secrets};

fn model(effort: Option<&str>) -> ModelConfig {
    ModelConfig {
        provider: RESPONSES.into(),
        base_url: "https://llm.example.com/v1".into(),
        model: "chat-a".into(),
        api_key_env: "KOMO_LLM_API_KEY".into(),
        effort: effort.map(Effort::new),
        efforts: None,
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

/// 一帧 SSE：`event: <name>` + `data: <json>`。
///
/// 负载先过一遍 serde 压成一行——SSE 的 `data:` 是**行**，夹层里带换行的 JSON 是发不
/// 出去的，顺手也把夹具里的 JSON 拼写错误挡在这里。
fn frame(name: &str, data: &str) -> String {
    let compact: Value = serde_json::from_str(data).expect("夹具里的 JSON");
    format!("event: {name}\ndata: {compact}\n\n")
}

/// 最短的一次成功回复：一段文本 + 终止帧。
fn one_word() -> Reply {
    Reply::raw(
        200,
        &[
            frame(
                "response.output_text.delta",
                r#"{"output_index":0,"delta":"好"}"#,
            ),
            frame(
                "response.completed",
                r#"{"response":{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"好"}]}],"usage":{"input_tokens":4,"output_tokens":1}}}"#,
            ),
        ],
    )
}

#[tokio::test]
async fn an_unset_effort_leaves_the_reasoning_field_out_of_the_request() {
    let transport = ScriptedTransport::new(vec![one_word()]);
    let config = model(None);
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();
    let round = driver.next(RoundInput::First).await.unwrap();

    assert_eq!(round.text.as_deref(), Some("好"));
    let body = &transport.bodies()[0];
    assert!(
        body.get("reasoning").is_none(),
        "未设置 effort 时请求体里没有这个字段：{body}"
    );
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["store"], json!(false), "不依赖服务端保存");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert!(body.get("previous_response_id").is_none());
    assert_eq!(body["instructions"], json!("你是 komo"));
    assert_eq!(
        transport.requests()[0].url,
        "https://llm.example.com/v1/responses"
    );
}

#[tokio::test]
async fn an_explicit_effort_rides_along_as_reasoning_effort() {
    let transport = ScriptedTransport::new(vec![one_word()]);
    let config = model(Some("high"));
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();
    driver.next(RoundInput::First).await.unwrap();
    assert_eq!(
        transport.bodies()[0]["reasoning"],
        json!({ "effort": "high" })
    );
}

/// gpt-5.1 起 `none` 也是一档——它和"没配置"不是一回事（§13.3）。
#[tokio::test]
async fn effort_none_is_a_level_that_gets_sent() {
    let transport = ScriptedTransport::new(vec![one_word()]);
    let config = model(Some("none"));
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();
    driver.next(RoundInput::First).await.unwrap();
    assert_eq!(
        transport.bodies()[0]["reasoning"],
        json!({ "effort": "none" })
    );
}

#[tokio::test]
async fn tools_are_sent_flat_with_a_tool_choice() {
    let transport = ScriptedTransport::new(vec![one_word()]);
    let config = model(None);
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut req = request(&config);
    req.tools = vec![ToolDefinition {
        name: "read".into(),
        description: "读文件".into(),
        parameters: json!({ "type": "object", "properties": {} }),
    }];
    let mut driver = llm.begin_turn(req).await.unwrap();
    driver.next(RoundInput::First).await.unwrap();

    let body = &transport.bodies()[0];
    assert_eq!(body["tools"][0]["type"], json!("function"));
    assert_eq!(body["tools"][0]["name"], json!("read"));
    assert_eq!(body["tools"][0]["parameters"]["type"], json!("object"));
    assert_eq!(body["tool_choice"], json!("auto"));
}

/// §13.3：不支持的 effort 在**请求前**拒绝。
#[tokio::test]
async fn an_unsupported_effort_is_refused_before_anything_is_sent() {
    let transport = ScriptedTransport::new(vec![one_word()]);
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
    let transport = ScriptedTransport::new(vec![one_word()]);
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
            r#"{"error":{"message":"unsupported value for reasoning.effort","code":"invalid_request_error"}}"#,
        ),
        one_word(),
    ]);
    let config = model(Some("high"));
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();
    let Err(error) = driver.next(RoundInput::First).await else {
        panic!("400 要原样报上来")
    };

    assert!(
        matches!(&error, LlmError::Rejected { status: 400, message } if message.contains("reasoning.effort")),
        "{error:?}"
    );
    assert!(!is_retryable(&error), "400 不是可重试的");
    let sent = transport.bodies();
    assert_eq!(sent.len(), 1, "只发了一次");
    assert_eq!(
        sent[0]["reasoning"],
        json!({ "effort": "high" }),
        "参数没有被悄悄删掉"
    );
}

/// 没收到 `response.completed` = 可重试的失败，不是一个短回答。
#[tokio::test]
async fn a_stream_without_its_terminal_event_is_a_retryable_failure() {
    let transport = ScriptedTransport::new(vec![Reply::raw(
        200,
        &[frame(
            "response.output_text.delta",
            r#"{"output_index":0,"delta":"半句"}"#,
        )],
    )]);
    let config = model(None);
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();
    let Err(error) = driver.next(RoundInput::First).await else {
        panic!("没有终止事件就不是一个完整回复")
    };

    assert_eq!(error, LlmError::Incomplete);
    assert!(is_retryable(&error), "没收齐可以再来一次");
}

/// `response.incomplete` → 截断，**不构造任何工具调用**。
#[tokio::test]
async fn an_incomplete_response_truncates_the_round_and_builds_no_calls() {
    let transport = ScriptedTransport::new(vec![Reply::raw(
        200,
        &[
            frame(
                "response.output_item.added",
                r#"{"output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"read"}}"#,
            ),
            frame(
                "response.incomplete",
                r#"{"response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"pa"}],"usage":{"input_tokens":9,"output_tokens":64}}}"#,
            ),
        ],
    )]);
    let config = model(None);
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();
    let round = driver.next(RoundInput::First).await.unwrap();

    assert!(round.truncated);
    assert!(round.tool_calls.is_empty(), "半轮调用不能执行");
    assert_eq!(round.usage.output, Some(64));
}

#[tokio::test]
async fn a_second_round_replays_the_output_items_and_the_tool_result() {
    let transport = ScriptedTransport::new(vec![
        Reply::raw(
            200,
            &[
                frame(
                    "response.output_item.added",
                    r#"{"output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"read","arguments":""}}"#,
                ),
                frame(
                    "response.function_call_arguments.delta",
                    r#"{"output_index":1,"delta":"{\"path\":"}"#,
                ),
                frame(
                    "response.function_call_arguments.done",
                    r#"{"output_index":1,"arguments":"{\"path\":\"a.txt\"}"}"#,
                ),
                frame(
                    "response.completed",
                    r#"{"response":{"status":"completed","output":[
                        {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"ENC"},
                        {"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.txt\"}"}
                    ],"usage":{"input_tokens":9,"output_tokens":4,"output_tokens_details":{"reasoning_tokens":3}}}}"#,
                ),
            ],
        ),
        Reply::raw(
            200,
            &[
                frame(
                    "response.output_text.delta",
                    r#"{"output_index":0,"delta":"读到了"}"#,
                ),
                frame(
                    "response.completed",
                    r#"{"response":{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"读到了"}]}],"usage":{"input_tokens":11,"output_tokens":2}}}"#,
                ),
            ],
        ),
    ]);
    let config = model(None);
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut driver = llm.begin_turn(request(&config)).await.unwrap();

    let first = driver.next(RoundInput::First).await.unwrap();
    assert_eq!(first.tool_calls.len(), 1);
    assert_eq!(first.tool_calls[0].provider_call_id, "call_1");
    assert_eq!(first.tool_calls[0].arguments["path"], json!("a.txt"));

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

    // 第二次请求的 input：上一轮的 output items **原样**在前（reasoning 带着
    // encrypted_content），function_call_output 紧跟其后。
    let body = &transport.bodies()[1];
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3, "reasoning + function_call + output：{body}");
    assert_eq!(input[0]["type"], json!("reasoning"));
    assert_eq!(input[0]["encrypted_content"], json!("ENC"));
    assert_eq!(input[1]["type"], json!("function_call"));
    assert_eq!(input[1]["call_id"], json!("call_1"));
    assert_eq!(input[2]["type"], json!("function_call_output"));
    assert_eq!(input[2]["call_id"], json!("call_1"));
    assert_eq!(input[2]["output"], json!("文件内容"));

    // 用量按轮累加，不是最后一轮盖掉前面的。
    assert_eq!(driver.usage().input, Some(20));
    assert_eq!(driver.usage().output, Some(6));
    assert_eq!(driver.usage().reasoning, Some(3));
}

#[tokio::test]
async fn the_replay_window_becomes_input_items_in_order() {
    let transport = ScriptedTransport::new(vec![one_word()]);
    let config = model(None);
    let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
    let mut req = request(&config);
    req.messages = vec![
        komo_kernel::types::turn::ReplayMessage {
            role: komo_kernel::types::turn::Role::User,
            seq: komo_kernel::types::ids::Seq(1),
            text: Some("看看这个文件".into()),
            tool_calls: vec![],
            tool_results: vec![],
            provider_blocks: None,
        },
        komo_kernel::types::turn::ReplayMessage {
            role: komo_kernel::types::turn::Role::Assistant,
            seq: komo_kernel::types::ids::Seq(2),
            text: None,
            tool_calls: vec![],
            tool_results: vec![],
            provider_blocks: Some(json!([
                { "type": "reasoning", "id": "rs_0", "encrypted_content": "OLD" }
            ])),
        },
    ];
    let mut driver = llm.begin_turn(req).await.unwrap();
    driver.next(RoundInput::First).await.unwrap();

    let input = transport.bodies()[0]["input"].as_array().unwrap().clone();
    assert_eq!(input[0]["type"], json!("message"));
    assert_eq!(input[0]["content"][0]["type"], json!("input_text"));
    assert_eq!(input[1]["encrypted_content"], json!("OLD"));
}

#[tokio::test]
async fn a_missing_credential_is_refused_with_the_variable_name_not_a_value() {
    let transport = ScriptedTransport::new(vec![one_word()]);
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

#[test]
fn chat_completions_is_supported_and_unimplemented_protocols_are_refused() {
    let transport = ScriptedTransport::new(vec![]);
    let mut chat = model(None);
    chat.provider = CHAT_COMPLETIONS.into();
    factory(&transport)
        .build(&chat, ModelRole::Main)
        .expect("chat_completions 应该有独立适配器");

    for provider in ["openai_compatible", "openai_chat", "anthropic_messages"] {
        let mut config = model(None);
        config.provider = provider.into();
        let Err(error) = factory(&transport).build(&config, ModelRole::Main) else {
            panic!("{provider} 不该造得出客户端")
        };
        assert!(
            matches!(error, LlmBuildError::UnknownProvider { .. }),
            "{provider}: {error:?}"
        );
    }
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

/// §13.3：切换聊天模型或 effort 不影响独立配置的记忆模型。
#[tokio::test]
async fn the_router_hands_each_run_the_instance_its_own_snapshot_names() {
    let transport = ScriptedTransport::new(vec![one_word(), one_word()]);
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

    let sent: Vec<Value> = transport.bodies();
    assert_eq!(sent[0]["model"], json!("chat-a"));
    assert!(sent[0].get("reasoning").is_none());
    assert_eq!(
        sent[1]["model"],
        json!("memory-a"),
        "换聊天模型不影响记忆模型"
    );
    assert_eq!(
        sent[1]["reasoning"],
        json!({ "effort": "low" }),
        "记忆模型用自己的 effort"
    );
}
