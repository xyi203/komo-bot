//! OpenAI Responses 的无状态入口（[`ModelStream`]）。
//!
//! 复用父模块里旧 driver 的 `replay`（每条消息转成它要的 [`ReplayMessage`] 再交给它，原生
//! output items 的取舍、`function_call_output` 的形状都是它的）、`wire::responses_request`
//! （`store: false` + `include` 加密推理）与 [`Assembled`]（SSE 拼装）。新写的：消息摆放、
//! 凭证与 `session_id` 头、[`Round`] → [`AssistantMessage`]。

use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use komo_kernel::durable::{AssistantMessage, ContentBlock, Message};
use komo_kernel::types::model::{EffortSetting, ModelConfig};
use komo_kernel::types::turn::{LlmError, ReplayMessage, Role, Round};
use serde_json::{Map, Value, json};

use super::{Assembled, Credential, OpenAiResponsesLlm, check_effort, replay, wire};
use crate::llm::model_stream::{
    API_RESPONSES, ModelRequest, ModelStream, UserPart, assistant_replay, final_message,
    lenient_arguments, partial_message, plain_text, replay_system, stop_reason, thinking_block,
    tool_result_run, transport_error, user_parts,
};
use crate::llm::sse::SseDecoder;
use crate::llm::transport::HttpRequest;

/// 带图片的 user 消息：`input_text` / `input_image`（data URL）按次序。
fn user_with_images(parts: &[UserPart]) -> Value {
    let content: Vec<Value> = parts
        .iter()
        .map(|part| match part {
            UserPart::Text(text) => json!({ "type": "input_text", "text": text }),
            UserPart::Image(image) => json!({
                "type": "input_image",
                "image_url": format!(
                    "data:{};base64,{}",
                    image.mime,
                    crate::llm::image_base64(image)
                ),
            }),
        })
        .collect();
    json!({ "type": "message", "role": "user", "content": content })
}

/// `messages` → `input` items。
fn build_input(
    messages: &[Message],
    resolver: Option<&dyn crate::llm::model_stream::ImageResolver>,
) -> Vec<Value> {
    let mut input = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::System(_) => {}
            Message::User(user) => {
                let parts = user_parts(user, resolver);
                match plain_text(&parts) {
                    Some(text) => input.extend(replay(&[ReplayMessage {
                        role: Role::User,
                        seq: komo_kernel::types::ids::Seq(i as u64),
                        text: Some(text),
                        tool_calls: vec![],
                        tool_results: vec![],
                        provider_blocks: None,
                    }])),
                    None => input.push(user_with_images(&parts)),
                }
            }
            Message::Assistant(assistant) => {
                input.extend(replay(&[assistant_replay(
                    assistant,
                    i,
                    API_RESPONSES,
                    Value::is_array,
                )]));
            }
            Message::ToolResult(_) => {
                let (results, end) = tool_result_run(messages, i, resolver);
                input.extend(replay(&[ReplayMessage {
                    role: Role::Tool,
                    seq: komo_kernel::types::ids::Seq(i as u64),
                    text: None,
                    tool_calls: vec![],
                    tool_results: results,
                    provider_blocks: None,
                }]));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    input
}

#[async_trait]
impl ModelStream for OpenAiResponsesLlm {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
    ) -> Result<AssistantMessage, LlmError> {
        let model = request.model;
        check_effort(model, &self.caps)?;

        let system = replay_system(request.messages);
        let input = build_input(request.messages, request.options.images);
        let effort = EffortSetting::from_option(model.effort.clone());
        let mut body = wire::responses_request(
            model,
            &system.prompt,
            &input,
            &system.tools,
            effort.as_option(),
            request.options.max_output_tokens.or(self.max_output_tokens),
        );
        if let Some(key) = request.options.cache_key {
            wire::with_prompt_cache_key(&mut body, key);
        }

        let (api_key, mut headers) = self.credential.resolve(&model.api_key_env).await?;
        if matches!(self.credential, Credential::ChatGpt(_))
            && let Some(key) = request.options.cache_key
        {
            headers.push(("session_id".to_string(), key.to_string()));
        }
        let mut http = HttpRequest::new(self.endpoint(model), body)
            .with_key(Some(api_key))
            .with_timeout(Duration::from_secs(model.timeout_secs.max(1)));
        for (name, value) in headers {
            http = http.with_header(name, value);
        }

        let started = Instant::now();
        let mut stats = (None, 0usize);
        let outcome = self
            .stream_round_trip(model, http, progress, started, &mut stats)
            .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &outcome {
            Ok(message) => tracing::info!(
                role = %self.role,
                model = %model.model,
                api_backend = "responses",
                stateless = true,
                tool_calls = message.tool_calls().count(),
                elapsed_ms,
                "model stream completed"
            ),
            Err(error) if stats.0.is_some() => tracing::warn!(
                first_byte_ms = stats.0.unwrap(),
                bytes = stats.1,
                role = %self.role,
                model = %model.model,
                api_backend = "responses",
                stateless = true,
                elapsed_ms,
                error = %error,
                "model stream failed"
            ),
            Err(error) => tracing::warn!(
                role = %self.role, model = %model.model, bytes = stats.1, elapsed_ms,
                error = %error, "model stream failed"
            ),
        }
        outcome
    }
}

impl OpenAiResponsesLlm {
    async fn stream_round_trip<F: for<'m> FnMut(&'m AssistantMessage) + Send + ?Sized>(
        &self,
        model: &ModelConfig,
        http: HttpRequest,
        progress: &mut F,
        started: Instant,
        stats: &mut (Option<u64>, usize),
    ) -> Result<AssistantMessage, LlmError> {
        let response = self.transport.post(http).await.map_err(transport_error)?;
        let status = response.status;
        if !(200..300).contains(&status) {
            let mut body = response.body;
            let mut bytes = Vec::new();
            let mut failed = false;
            while let Some(chunk) = body.next().await {
                match chunk {
                    Ok(chunk) => {
                        if !chunk.is_empty() {
                            stats.0.get_or_insert(started.elapsed().as_millis() as u64);
                            stats.1 += chunk.len();
                            bytes.extend_from_slice(&chunk);
                        }
                    }
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            let text = if failed {
                String::new()
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            };
            // §13.3：模型 400 不触发静默删参重试。
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
            if !chunk.is_empty() {
                stats.0.get_or_insert(started.elapsed().as_millis() as u64);
                stats.1 += chunk.len();
            }
            let mut changed = false;
            for frame in decoder.push(&chunk) {
                assembled.absorb(frame.name.as_deref(), &frame.data);
                changed = true;
            }
            if assembled.settled() {
                break;
            }
            if changed {
                progress(&partial_message(
                    model,
                    API_RESPONSES,
                    assembled.content_so_far(),
                ));
            }
        }
        if !assembled.settled() {
            tracing::warn!(
                mid_frame = decoder.has_trailing_bytes(),
                "流结束时没有收到 response.completed"
            );
            return Err(LlmError::Incomplete);
        }
        let round = assembled.finish(1)?;
        Ok(message_from_round(model, round))
    }
}

fn parse_arguments(item: &Value) -> Value {
    lenient_arguments(
        item.get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )
}

/// `output` items → provider 无关的块：reasoning 的摘要、assistant 文字、函数调用。
fn content_from_items(items: &[Value]) -> Vec<ContentBlock> {
    let mut content = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                let summary = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();
                content.push(thinking_block(summary, Map::new()));
            }
            Some("message") => {
                let text: String = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter(|part| {
                                part.get("type").and_then(Value::as_str) == Some("output_text")
                            })
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect()
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    content.push(ContentBlock::text(text));
                }
            }
            _ => {
                if let Some((call_id, name)) = wire::function_call_of(item) {
                    content.push(ContentBlock::tool_call(
                        call_id,
                        name,
                        parse_arguments(item),
                    ));
                }
            }
        }
    }
    content
}

fn message_from_round(model: &ModelConfig, round: Round) -> AssistantMessage {
    let mut content = match &round.provider_blocks {
        Some(Value::Array(items)) => content_from_items(items),
        _ => Vec::new(),
    };
    // 服务端的 `output` 里没有 message item、而增量里有文字：文字不能丢。放在调用之前。
    if let Some(text) = &round.text
        && !content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { .. }))
    {
        let at = content
            .iter()
            .position(|block| matches!(block, ContentBlock::ToolCall { .. }))
            .unwrap_or(content.len());
        content.insert(at, ContentBlock::text(text.clone()));
    }
    final_message(
        model,
        API_RESPONSES,
        content,
        stop_reason(round.truncated, !round.tool_calls.is_empty()),
        &round.usage,
        round.provider_blocks,
    )
}

impl Assembled {
    /// 到目前为止收到的：推理摘要、文字、调用（参数没收齐是空对象）。
    fn content_so_far(&self) -> Vec<ContentBlock> {
        let mut content = Vec::new();
        if !self.reasoning_summary.is_empty() {
            content.push(thinking_block(self.reasoning_summary.clone(), Map::new()));
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::text(self.text.clone()));
        }
        for call in self.calls.values() {
            content.push(ContentBlock::tool_call(
                call.call_id.clone(),
                call.name.clone(),
                lenient_arguments(&call.arguments),
            ));
        }
        content
    }
}
