//! OpenAI Chat Completions adapter：`messages` 回放、流式 delta 与函数调用。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use komo_kernel::traits::{LlmClient, TurnDriver};
use komo_kernel::types::model::{EffortSetting, ModelConfig, ModelRole, TokenUsage};
use komo_kernel::types::tool::ToolDefinition;
use komo_kernel::types::turn::{
    LlmError, ProviderToolCall, ReplayMessage, Role, Round, RoundInput, ToolResultForModel,
    TurnRequest,
};
use serde_json::{Map, Value, json};

use super::sse::SseDecoder;
use super::transport::{HttpRequest, HttpTransport, TransportError};
use super::wire;
use crate::config::EffortCapabilities;

pub struct ChatCompletionsLlm {
    config: ModelConfig,
    role: ModelRole,
    api_key: Option<String>,
    transport: Arc<dyn HttpTransport>,
    caps: EffortCapabilities,
}

impl std::fmt::Debug for ChatCompletionsLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatCompletionsLlm")
            .field("role", &self.role)
            .field("model", &self.config.model)
            .field("base_url", &self.config.base_url)
            .field("has_key", &self.api_key.is_some())
            .finish()
    }
}

impl ChatCompletionsLlm {
    pub fn new(
        config: ModelConfig,
        role: ModelRole,
        api_key: Option<String>,
        transport: Arc<dyn HttpTransport>,
        caps: EffortCapabilities,
    ) -> Result<Self, LlmError> {
        check_effort(&config, &caps)?;
        Ok(Self {
            config,
            role,
            api_key,
            transport,
            caps,
        })
    }
}

fn check_effort(config: &ModelConfig, caps: &EffortCapabilities) -> Result<(), LlmError> {
    caps.check(config).map_err(|_| LlmError::UnsupportedEffort {
        model: config.model.clone(),
        effort: config
            .effort
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
    })
}

#[async_trait]
impl LlmClient for ChatCompletionsLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        check_effort(&req.model, &self.caps)?;
        // 记忆段已经在 `req.system_prompt` 里（Gateway 装配时拼好，§13.2）。
        let instructions = req.system_prompt.clone();
        let mut messages = vec![json!({ "role": "system", "content": instructions })];
        messages.extend(replay(&req.messages));
        Ok(Box::new(ChatDriver {
            endpoint: format!(
                "{}/chat/completions",
                req.model.base_url.trim_end_matches('/')
            ),
            api_key: self.api_key.clone(),
            transport: Arc::clone(&self.transport),
            role: self.role,
            model: req.model,
            tools: req.tools,
            messages,
            usage: TokenUsage::default(),
            round: 0,
        }))
    }
}

fn replay(messages: &[ReplayMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            Role::User => out.push(json!({
                "role": "user",
                "content": message.text.as_deref().unwrap_or_default(),
            })),
            Role::Assistant => {
                if let Some(native) = message
                    .provider_blocks
                    .as_ref()
                    .filter(|value| value.get("role").and_then(Value::as_str) == Some("assistant"))
                {
                    out.push(native.clone());
                } else {
                    out.push(assistant_message(
                        message.text.as_deref(),
                        message.tool_calls.iter().map(|call| {
                            (
                                call.provider_call_id.as_str(),
                                call.name.as_str(),
                                call.arguments.to_string(),
                            )
                        }),
                    ));
                }
            }
            Role::Tool => out.extend(message.tool_results.iter().map(tool_message)),
        }
    }
    out
}

fn assistant_message<'a>(
    text: Option<&str>,
    calls: impl Iterator<Item = (&'a str, &'a str, String)>,
) -> Value {
    let tool_calls: Vec<Value> = calls
        .map(|(id, name, arguments)| {
            json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            })
        })
        .collect();
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert(
        "content".into(),
        text.map_or(Value::Null, |text| json!(text)),
    );
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    Value::Object(message)
}

fn tool_message(result: &ToolResultForModel) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": result.provider_call_id,
        "content": result.content,
    })
}

struct ChatDriver {
    endpoint: String,
    api_key: Option<String>,
    transport: Arc<dyn HttpTransport>,
    role: ModelRole,
    model: ModelConfig,
    tools: Vec<ToolDefinition>,
    messages: Vec<Value>,
    usage: TokenUsage,
    round: u32,
}

#[async_trait]
impl TurnDriver for ChatDriver {
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError> {
        if let RoundInput::ToolResults { results } = &input {
            self.messages.extend(results.iter().map(tool_message));
        }
        self.round += 1;
        let effort = EffortSetting::from_option(self.model.effort.clone());
        let body = request_body(
            &self.model,
            &self.messages,
            &self.tools,
            effort.as_option().map(|effort| effort.as_str()),
        );
        let started = Instant::now();
        let outcome = self.round_trip(body).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &outcome {
            Ok(round) => tracing::info!(
                role = %self.role,
                model = %self.model.model,
                api_backend = "chat_completions",
                round = self.round,
                tool_calls = round.tool_calls.len(),
                truncated = round.truncated,
                elapsed_ms,
                "model round completed"
            ),
            Err(error) => tracing::warn!(
                role = %self.role,
                model = %self.model.model,
                api_backend = "chat_completions",
                round = self.round,
                elapsed_ms,
                error = %error,
                "model round failed"
            ),
        }
        let round = outcome?;
        if let Some(blocks) = &round.provider_blocks {
            self.messages.push(blocks.clone());
        }
        accumulate(&mut self.usage, &round.usage);
        Ok(round)
    }

    fn usage(&self) -> TokenUsage {
        self.usage
    }
}

impl ChatDriver {
    async fn round_trip(&self, body: Value) -> Result<Round, LlmError> {
        let request = HttpRequest::new(&self.endpoint, body)
            .with_key(self.api_key.clone())
            .with_timeout(Duration::from_secs(self.model.timeout_secs.max(1)));
        let response = self
            .transport
            .post(request)
            .await
            .map_err(transport_error)?;
        let status = response.status;
        if !(200..300).contains(&status) {
            let text = response.text().await.unwrap_or_default();
            return Err(LlmError::Rejected {
                status,
                message: wire::error_message(status, &text),
            });
        }

        let mut stream = response.body;
        let mut decoder = SseDecoder::new();
        let mut assembled = ChatAssembled::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(transport_error)?;
            for frame in decoder.push(&chunk) {
                assembled.absorb(&frame.data);
            }
            if assembled.completed {
                break;
            }
        }
        if !assembled.completed {
            return Err(LlmError::Incomplete);
        }
        assembled.finish(self.round)
    }
}

fn request_body(
    config: &ModelConfig,
    messages: &[Value],
    tools: &[ToolDefinition],
    effort: Option<&str>,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(config.model));
    body.insert("messages".into(), Value::Array(messages.to_vec()));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    if !tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters,
                            }
                        })
                    })
                    .collect(),
            ),
        );
        body.insert("tool_choice".into(), json!("auto"));
    }
    if let Some(effort) = effort {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    Value::Object(body)
}

#[derive(Debug, Default)]
struct ChatAssembled {
    text: String,
    calls: BTreeMap<usize, PartialCall>,
    reasoning_details: Vec<Value>,
    finish_reason: Option<String>,
    usage: TokenUsage,
    completed: bool,
    failure: Option<LlmError>,
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl ChatAssembled {
    fn absorb(&mut self, data: &str) {
        if data.trim() == "[DONE]" {
            self.completed = true;
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if let Some(error) = chunk.get("error") {
            self.failure = Some(LlmError::Rejected {
                status: 502,
                message: error.to_string(),
            });
        }
        if let Some(usage) = chunk.get("usage") {
            self.usage.input = usage.get("prompt_tokens").and_then(Value::as_u64);
            self.usage.output = usage.get("completion_tokens").and_then(Value::as_u64);
            self.usage.reasoning = usage
                .pointer("/completion_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64);
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return;
        };
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(content);
        }
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            self.reasoning_details.extend(details.iter().cloned());
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let partial = self.calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    partial.id = id.to_string();
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    partial.name.push_str(name);
                }
                if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str)
                {
                    partial.arguments.push_str(arguments);
                }
            }
        }
    }

    fn finish(self, round: u32) -> Result<Round, LlmError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let truncated = self
            .finish_reason
            .as_deref()
            .is_some_and(|reason| !matches!(reason, "stop" | "tool_calls"));
        let mut tool_calls = Vec::new();
        let mut native_calls = Vec::new();
        if !truncated {
            for call in self.calls.into_values() {
                let arguments = if call.arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&call.arguments).map_err(|_| LlmError::Incomplete)?
                };
                tool_calls.push(ProviderToolCall {
                    provider_call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments,
                });
                native_calls.push(json!({
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments },
                }));
            }
        }
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        message.insert(
            "content".into(),
            if self.text.is_empty() {
                Value::Null
            } else {
                json!(self.text)
            },
        );
        if !native_calls.is_empty() {
            message.insert("tool_calls".into(), Value::Array(native_calls));
        }
        if !self.reasoning_details.is_empty() {
            message.insert(
                "reasoning_details".into(),
                Value::Array(self.reasoning_details),
            );
        }
        Ok(Round {
            round,
            text: (!self.text.is_empty()).then_some(self.text),
            tool_calls,
            provider_blocks: Some(Value::Object(message)),
            usage: self.usage,
            truncated,
        })
    }
}

fn transport_error(error: TransportError) -> LlmError {
    match error {
        TransportError::Timeout => LlmError::Timeout,
        TransportError::Failed(message) => LlmError::Transport(message),
    }
}

fn accumulate(total: &mut TokenUsage, round: &TokenUsage) {
    fn add(total: &mut Option<u64>, more: Option<u64>) {
        if let Some(more) = more {
            *total = Some(total.unwrap_or(0) + more);
        }
    }
    add(&mut total.input, round.input);
    add(&mut total.output, round.output);
    add(&mut total.reasoning, round.reasoning);
}

#[cfg(test)]
mod tests {
    use super::super::transport::testing::{Reply, ScriptedTransport};
    use super::*;
    use komo_kernel::types::ids::{RunId, SessionId, ToolCallId};

    fn model() -> ModelConfig {
        ModelConfig {
            provider: super::super::CHAT_COMPLETIONS.into(),
            base_url: "https://openrouter.example/v1".into(),
            model: "vendor/chat".into(),
            api_key_env: "KEY".into(),
            auth: None,
            effort: None,
            efforts: None,
            timeout_secs: 10,
        }
    }

    fn frame(data: &str) -> String {
        format!("data: {data}\n\n")
    }

    fn request(config: &ModelConfig) -> TurnRequest {
        TurnRequest {
            session: SessionId::from_raw("s"),
            run: RunId::from_raw("r"),
            model: config.clone(),
            system_prompt: "system".into(),
            messages: vec![],
            tools: vec![],
            memories: vec![],
            covers: None,
        }
    }

    #[tokio::test]
    async fn text_tool_calls_and_usage_are_assembled_and_replayed() {
        let transport = ScriptedTransport::new(vec![
            Reply::raw(
                200,
                &[
                    frame(r#"{"choices":[{"delta":{"content":"看"},"finish_reason":null}]}"#),
                    frame(
                        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]},"finish_reason":null}]}"#,
                    ),
                    frame(
                        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":8,"completion_tokens":3}}"#,
                    ),
                    frame("[DONE]"),
                ],
            ),
            Reply::raw(
                200,
                &[
                    frame(r#"{"choices":[{"delta":{"content":"完"},"finish_reason":"stop"}]}"#),
                    frame("[DONE]"),
                ],
            ),
        ]);
        let config = model();
        let llm = ChatCompletionsLlm::new(
            config.clone(),
            ModelRole::Main,
            Some("secret".into()),
            Arc::new(transport.clone()),
            EffortCapabilities::builtin(),
        )
        .unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let first = driver.next(RoundInput::First).await.unwrap();
        assert_eq!(first.text.as_deref(), Some("看"));
        assert_eq!(first.tool_calls[0].arguments["path"], json!("a.txt"));
        assert_eq!(first.usage.input, Some(8));

        driver
            .next(RoundInput::ToolResults {
                results: vec![ToolResultForModel {
                    provider_call_id: "call_1".into(),
                    call_id: ToolCallId::from_raw("tc"),
                    content: "content".into(),
                    is_error: false,
                }],
            })
            .await
            .unwrap();
        let body = &transport.bodies()[1];
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], json!("assistant"));
        assert_eq!(messages[2]["role"], json!("tool"));
        assert_eq!(messages[2]["tool_call_id"], json!("call_1"));
    }

    #[tokio::test]
    async fn a_stream_without_done_is_incomplete() {
        let transport = ScriptedTransport::new(vec![Reply::raw(
            200,
            &[frame(
                r#"{"choices":[{"delta":{"content":"半句"},"finish_reason":"stop"}]}"#,
            )],
        )]);
        let config = model();
        let llm = ChatCompletionsLlm::new(
            config.clone(),
            ModelRole::Main,
            None,
            Arc::new(transport),
            EffortCapabilities::builtin(),
        )
        .unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        assert_eq!(
            driver.next(RoundInput::First).await.unwrap_err(),
            LlmError::Incomplete
        );
    }
}
