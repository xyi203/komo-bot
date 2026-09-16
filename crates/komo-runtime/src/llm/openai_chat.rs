//! OpenAI 兼容 Chat Completions 的 [`LlmClient`]（流式 + tool calling）。
//!
//! 三条来自文档的硬性质，各在下面有一段代码和一个测试：
//!
//! - **每次请求都流式**，而「流没收到终止帧」是一次**可重试的失败**，不是一个短回答
//!   （§6：参数未收齐时不能开始执行）。
//! - **保留协议回放所需的消息块**（§13.2）：assistant 那条消息按收到的样子存进
//!   [`Round::provider_blocks`]，reasoning 原样带回，下一轮逐字送回去。
//! - **effort 未配置就不发这个字段**；不支持的档位在**请求前**拒绝；模型 400
//!   **不触发**静默删参重试（§13.3）。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use komo_kernel::traits::{LlmClient, TurnDriver};
use komo_kernel::types::model::{Effort, EffortSetting, ModelConfig, ModelRole, TokenUsage};
use komo_kernel::types::tool::ToolDefinition;
use komo_kernel::types::turn::{
    LlmError, ProviderToolCall, ReplayMessage, Role, Round, RoundInput, ToolResultForModel,
    TurnRequest,
};
use serde_json::{Value, json};

use super::sse::{Frame, SseDecoder};
use super::transport::{HttpRequest, HttpTransport, TransportError};
use super::wire::{self, ChatChunk};
use crate::config::EffortCapabilities;

/// 记忆注入的接口（§9.4 的注入由 MemoryManager 决定内容，这里只留位置）。
///
/// 它不在 [`TurnRequest`] 里自己拼：`TurnRequest::memories` 是**审计证据**（注入了哪
/// 些条目的哪个版本，§9.7），正文该长什么样是记忆那边的事。
pub trait SystemPreamble: Send + Sync {
    /// 放在系统提示**之后**的一段注入正文；`None` = 这一轮不注入。
    fn preamble(&self, request: &TurnRequest) -> Option<String>;
}

/// 一个讲 Chat Completions 的后端。
pub struct OpenAiChatLlm {
    config: ModelConfig,
    role: ModelRole,
    api_key: Option<String>,
    transport: Arc<dyn HttpTransport>,
    caps: EffortCapabilities,
    preamble: Option<Arc<dyn SystemPreamble>>,
}

impl std::fmt::Debug for OpenAiChatLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 凭证不进 Debug。
        f.debug_struct("OpenAiChatLlm")
            .field("role", &self.role)
            .field("model", &self.config.model)
            .field("base_url", &self.config.base_url)
            .field("has_key", &self.api_key.is_some())
            .finish()
    }
}

impl OpenAiChatLlm {
    pub fn new(
        config: ModelConfig,
        role: ModelRole,
        api_key: Option<String>,
        transport: Arc<dyn HttpTransport>,
        caps: EffortCapabilities,
    ) -> Result<Self, LlmError> {
        // §13.3：不支持的档位在**请求前**拒绝。构造时先拒一次，于是一个带着不可用
        // 档位的客户端根本不存在。
        check_effort(&config, &caps)?;
        Ok(OpenAiChatLlm {
            config,
            role,
            api_key,
            transport,
            caps,
            preamble: None,
        })
    }

    pub fn with_preamble(mut self, preamble: Arc<dyn SystemPreamble>) -> Self {
        self.preamble = Some(preamble);
        self
    }

    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    fn endpoint(&self, model: &ModelConfig) -> String {
        format!("{}/chat/completions", model.base_url.trim_end_matches('/'))
    }
}

fn check_effort(config: &ModelConfig, caps: &EffortCapabilities) -> Result<(), LlmError> {
    if let Err(problem) = caps.check(config) {
        return Err(LlmError::UnsupportedEffort {
            model: config.model.clone(),
            effort: config
                .effort
                .as_ref()
                .map(Effort::to_string)
                .unwrap_or_default(),
        })
        .inspect_err(|_: &LlmError| {
            tracing::warn!(model = %config.model, problem = %problem, "effort 档位不可用");
        });
    }
    Ok(())
}

#[async_trait]
impl LlmClient for OpenAiChatLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        // 本次 Run 固定的是 `req.model`（§6），不是构造这个客户端时那份——热重载不在
        // 半路换模型。
        check_effort(&req.model, &self.caps)?;
        let Some(api_key) = self.api_key.clone() else {
            return Err(LlmError::Rejected {
                status: 401,
                message: format!(
                    "没有凭证：环境里找不到 `{}`（config.toml 里写的是变量名）",
                    req.model.api_key_env
                ),
            });
        };

        let mut messages = Vec::new();
        messages.push(json!({ "role": "system", "content": req.system_prompt }));
        if let Some(preamble) = self.preamble.as_ref().and_then(|p| p.preamble(&req)) {
            messages.push(json!({ "role": "system", "content": preamble }));
        }
        messages.extend(replay(&req.messages));

        Ok(Box::new(ChatDriver {
            endpoint: self.endpoint(&req.model),
            api_key,
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

/// 回放窗口 → 协议消息。
///
/// assistant 那条**优先用存下来的原始块**：reasoning 与签名一类的东西只有原样送回去
/// 才继续成立，重新拼一个"等价"的消息就是把它们丢了。
fn replay(messages: &[ReplayMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            Role::User => out.push(json!({
                "role": "user",
                "content": message.text.clone().unwrap_or_default(),
            })),
            Role::Assistant => match &message.provider_blocks {
                Some(blocks) => out.push(blocks.clone()),
                None => {
                    let mut assistant = serde_json::Map::new();
                    assistant.insert("role".into(), json!("assistant"));
                    assistant.insert(
                        "content".into(),
                        message
                            .text
                            .clone()
                            .map(Value::String)
                            .unwrap_or(Value::Null),
                    );
                    if !message.tool_calls.is_empty() {
                        assistant.insert(
                            "tool_calls".into(),
                            Value::Array(
                                message
                                    .tool_calls
                                    .iter()
                                    .map(|call| {
                                        json!({
                                            "id": call.provider_call_id,
                                            "type": "function",
                                            "function": {
                                                "name": call.name,
                                                "arguments": arguments_text(&call.arguments),
                                            }
                                        })
                                    })
                                    .collect(),
                            ),
                        );
                    }
                    out.push(Value::Object(assistant));
                }
            },
            // 工具结果在这个协议里是 `role="tool"` 的独立消息，一次调用一条。
            Role::Tool => out.extend(message.tool_results.iter().map(tool_message)),
        }
    }
    out
}

fn tool_message(result: &ToolResultForModel) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": result.provider_call_id,
        "content": result.content,
    })
}

fn arguments_text(arguments: &Value) -> String {
    match arguments {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// 一次 Run 的一个执行段。
struct ChatDriver {
    endpoint: String,
    api_key: String,
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
        let body = wire::chat_request(&self.model, &self.messages, &self.tools, effort.as_option());
        let started = Instant::now();
        let outcome = self.round_trip(body).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        // §13.3：每次生成请求记录角色、模型身份、实际 effort 或 provider_default、
        // 耗时和错误；**不记密钥**。
        let effort_label = match &effort {
            EffortSetting::ProviderDefault => "provider_default".to_string(),
            EffortSetting::Explicit(level) => level.to_string(),
        };
        match &outcome {
            Ok(round) => tracing::info!(
                role = %self.role,
                model = %self.model.model,
                base_url = %self.model.base_url,
                effort = %effort_label,
                round = self.round,
                tool_calls = round.tool_calls.len(),
                text_chars = round.text.as_deref().map(str::len).unwrap_or(0),
                truncated = round.truncated,
                elapsed_ms,
                "model round completed"
            ),
            Err(error) => tracing::warn!(
                role = %self.role,
                model = %self.model.model,
                effort = %effort_label,
                round = self.round,
                elapsed_ms,
                error = %error,
                "model round failed"
            ),
        }

        let round = outcome?;
        // 下一轮要把这条 assistant 原样送回去。
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
            .with_key(Some(self.api_key.clone()))
            .with_timeout(Duration::from_secs(self.model.timeout_secs.max(1)));
        let response = self
            .transport
            .post(request)
            .await
            .map_err(transport_error)?;

        let status = response.status;
        if !(200..300).contains(&status) {
            let text = response.text().await.unwrap_or_default();
            // §13.3：模型 400 **不触发**静默删参重试——这里只把拒绝原样报上去，
            // 没有任何一条路径会去掉 effort 再发一次。
            return Err(LlmError::Rejected {
                status,
                message: wire::error_message(status, &text),
            });
        }

        let mut stream = response.body;
        let mut decoder = SseDecoder::new();
        let mut assembled = Assembled::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(transport_error)?;
            for frame in decoder.push(&chunk) {
                match frame {
                    Frame::Data(data) => assembled.absorb(&data),
                    Frame::Done => {}
                }
            }
        }
        if !decoder.terminated() {
            // 流断在半路。**这是可重试的失败，不是一个短回答**。
            tracing::warn!(
                mid_frame = decoder.has_trailing_bytes(),
                "流结束时没有收到终止帧"
            );
            return Err(LlmError::Incomplete);
        }
        assembled.finish(self.round)
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

/// 逐帧拼起来的一轮回复。
#[derive(Debug, Default)]
struct Assembled {
    text: String,
    reasoning: String,
    calls: BTreeMap<usize, PartialCall>,
    finish_reason: Option<String>,
    usage: TokenUsage,
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl Assembled {
    fn absorb(&mut self, data: &str) {
        // 一帧解析不了就丢掉这一帧：各家会插心跳和自定义字段，为它们中断整轮是过头的。
        // 真正的"没收齐"由终止帧与参数解析判定。
        let Ok(chunk) = serde_json::from_str::<ChatChunk>(data) else {
            tracing::debug!(bytes = data.len(), "跳过一帧解析不了的 SSE 负载");
            return;
        };
        if let Some(usage) = chunk.usage {
            self.usage = usage.to_kernel();
        }
        for choice in chunk.choices {
            if let Some(content) = choice.delta.content {
                self.text.push_str(&content);
            }
            if let Some(reasoning) = choice.delta.reasoning_content.or(choice.delta.reasoning) {
                self.reasoning.push_str(&reasoning);
            }
            for delta in choice.delta.tool_calls {
                let call = self.calls.entry(delta.index).or_default();
                if let Some(id) = delta.id {
                    call.id = id;
                }
                if let Some(function) = delta.function {
                    if let Some(name) = function.name {
                        call.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        call.arguments.push_str(&arguments);
                    }
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
        }
    }

    fn finish(self, round: u32) -> Result<Round, LlmError> {
        let mut tool_calls = Vec::new();
        let mut blocks = Vec::new();
        for call in self.calls.into_values() {
            // 参数是逐段到的 JSON 文本；拼不成 JSON 就是没收齐——不能按半轮调用执行。
            let arguments: Value = if call.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.arguments).map_err(|_| LlmError::Incomplete)?
            };
            blocks.push(json!({
                "id": call.id,
                "type": "function",
                "function": { "name": call.name, "arguments": call.arguments },
            }));
            tool_calls.push(ProviderToolCall {
                provider_call_id: call.id,
                name: call.name,
                arguments,
            });
        }

        // 回放用的原始 assistant 消息。reasoning 原样带回（§13.2）。
        let mut assistant = serde_json::Map::new();
        assistant.insert("role".into(), json!("assistant"));
        assistant.insert(
            "content".into(),
            if self.text.is_empty() {
                Value::Null
            } else {
                json!(self.text)
            },
        );
        if !self.reasoning.is_empty() {
            assistant.insert("reasoning_content".into(), json!(self.reasoning));
        }
        if !blocks.is_empty() {
            assistant.insert("tool_calls".into(), Value::Array(blocks));
        }

        Ok(Round {
            round,
            text: (!self.text.is_empty()).then_some(self.text),
            tool_calls,
            provider_blocks: Some(Value::Object(assistant)),
            usage: self.usage,
            // provider 说这次回复被截断了（§6）。
            truncated: self.finish_reason.as_deref() == Some("length"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::ToolCallId;
    use komo_kernel::types::turn::ToolCallRequest;

    fn assemble(frames: &[&str]) -> Result<Round, LlmError> {
        let mut assembled = Assembled::default();
        for frame in frames {
            assembled.absorb(frame);
        }
        assembled.finish(1)
    }

    #[test]
    fn deltas_across_frames_become_one_call_with_parsed_arguments() {
        let round = assemble(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
        ])
        .unwrap();

        assert_eq!(round.tool_calls.len(), 1);
        assert_eq!(round.tool_calls[0].provider_call_id, "call_1");
        assert_eq!(round.tool_calls[0].name, "read");
        assert_eq!(round.tool_calls[0].arguments["path"], json!("a.txt"));
        assert_eq!(round.usage.input, Some(5));
        assert!(!round.truncated);
    }

    #[test]
    fn half_an_argument_object_is_incomplete_not_an_empty_call() {
        let error = assemble(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,
        ])
        .unwrap_err();
        assert_eq!(error, LlmError::Incomplete);
    }

    #[test]
    fn reasoning_is_carried_back_verbatim_in_the_provider_blocks() {
        let round = assemble(&[
            r#"{"choices":[{"delta":{"reasoning_content":"想了想"}}]}"#,
            r#"{"choices":[{"delta":{"content":"好"},"finish_reason":"stop"}]}"#,
        ])
        .unwrap();
        let blocks = round.provider_blocks.unwrap();
        assert_eq!(blocks["reasoning_content"], json!("想了想"));
        assert_eq!(blocks["content"], json!("好"));
        assert_eq!(blocks["role"], json!("assistant"));
    }

    #[test]
    fn a_length_finish_reason_marks_the_round_truncated() {
        let round =
            assemble(&[r#"{"choices":[{"delta":{"content":"半"},"finish_reason":"length"}]}"#])
                .unwrap();
        assert!(round.truncated, "参数未收齐时不能开始执行（§6）");
    }

    #[test]
    fn an_unparseable_frame_is_skipped_rather_than_failing_the_round() {
        let round = assemble(&[
            "{not json}",
            r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
        ])
        .unwrap();
        assert_eq!(round.text.as_deref(), Some("ok"));
    }

    #[test]
    fn a_stored_assistant_block_is_replayed_verbatim() {
        let blocks = json!({
            "role": "assistant",
            "content": null,
            "reasoning_content": "链",
            "tool_calls": [{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]
        });
        let replayed = replay(&[ReplayMessage {
            role: Role::Assistant,
            seq: komo_kernel::types::ids::Seq(1),
            text: Some("被忽略的重拼版本".into()),
            tool_calls: vec![],
            tool_results: vec![],
            provider_blocks: Some(blocks.clone()),
        }]);
        assert_eq!(replayed, vec![blocks]);
    }

    #[test]
    fn an_assistant_without_stored_blocks_is_rebuilt_with_its_calls() {
        let replayed = replay(&[ReplayMessage {
            role: Role::Assistant,
            seq: komo_kernel::types::ids::Seq(1),
            text: None,
            tool_calls: vec![ToolCallRequest {
                call_id: ToolCallId::from_raw("tc-1"),
                provider_call_id: "call_1".into(),
                name: "read".into(),
                arguments: json!({"path": "a.txt"}),
                arguments_ref: None,
            }],
            tool_results: vec![],
            provider_blocks: None,
        }]);
        assert_eq!(replayed[0]["tool_calls"][0]["id"], json!("call_1"));
        assert_eq!(
            replayed[0]["tool_calls"][0]["function"]["arguments"],
            json!("{\"path\":\"a.txt\"}")
        );
    }

    #[test]
    fn tool_results_replay_as_one_message_per_call() {
        let replayed = replay(&[ReplayMessage {
            role: Role::Tool,
            seq: komo_kernel::types::ids::Seq(2),
            text: None,
            tool_calls: vec![],
            tool_results: vec![
                ToolResultForModel {
                    provider_call_id: "call_1".into(),
                    call_id: ToolCallId::from_raw("tc-1"),
                    content: "ok".into(),
                    is_error: false,
                },
                ToolResultForModel {
                    provider_call_id: "call_2".into(),
                    call_id: ToolCallId::from_raw("tc-2"),
                    content: "boom".into(),
                    is_error: true,
                },
            ],
            provider_blocks: None,
        }]);
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0]["role"], json!("tool"));
        assert_eq!(replayed[1]["tool_call_id"], json!("call_2"));
    }
}
