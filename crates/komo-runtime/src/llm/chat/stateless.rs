//! Chat Completions 的无状态入口（[`ModelStream`]）。
//!
//! 复用父模块里旧 driver 的 `replay`（原生 assistant message 的取舍、`role: "tool"` 消息，
//! 工具结果里的图片并成**紧跟其后**的一条 user 消息）、`request_body` 与 [`ChatAssembled`]。
//! 连续的 toolResult 消息按"一轮"交给它（图片那条 user 消息按轮出一条，同旧 driver）。

use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use komo_kernel::durable::{AssistantMessage, ContentBlock, Message};
use komo_kernel::types::model::{EffortSetting, ModelConfig};
use komo_kernel::types::turn::{LlmError, ReplayMessage, Role, Round};
use serde_json::{Map, Value, json};

use super::{ChatAssembled, ChatCompletionsLlm, check_effort, replay, request_body};
use crate::llm::model_stream::{
    API_CHAT, ImageResolver, ModelRequest, ModelStream, UserPart, assistant_replay, final_message,
    lenient_arguments, partial_message, plain_text, replay_system, stop_reason, thinking_block,
    tool_result_run, transport_error, user_parts,
};
use crate::llm::sse::SseDecoder;
use crate::llm::transport::HttpRequest;
use crate::llm::wire;

/// 原生 assistant message 的形状：一个 `role: "assistant"` 的对象。
fn is_native(value: &Value) -> bool {
    value.get("role").and_then(Value::as_str) == Some("assistant")
}

fn user_with_images(parts: &[UserPart]) -> Value {
    let content: Vec<Value> = parts
        .iter()
        .map(|part| match part {
            UserPart::Text(text) => json!({ "type": "text", "text": text }),
            UserPart::Image(image) => json!({
                "type": "image_url",
                "image_url": {
                    "url": format!(
                        "data:{};base64,{}",
                        image.mime,
                        crate::llm::image_base64(image)
                    ),
                },
            }),
        })
        .collect();
    json!({ "role": "user", "content": content })
}

/// 系统提示（恒有一条，哪怕是空串，同旧 driver）+ 其余消息。
fn build_messages(
    system_prompt: &str,
    messages: &[Message],
    resolver: Option<&dyn ImageResolver>,
) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": system_prompt })];
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::System(_) => {}
            Message::User(user) => {
                let parts = user_parts(user, resolver);
                match plain_text(&parts) {
                    Some(text) => out.extend(replay(&[ReplayMessage {
                        role: Role::User,
                        seq: komo_kernel::types::ids::Seq(i as u64),
                        text: Some(text),
                        tool_calls: vec![],
                        tool_results: vec![],
                        provider_blocks: None,
                    }])),
                    None => out.push(user_with_images(&parts)),
                }
            }
            Message::Assistant(assistant) => {
                out.extend(replay(&[assistant_replay(
                    assistant, i, API_CHAT, is_native,
                )]));
            }
            Message::ToolResult(_) => {
                let (results, end) = tool_result_run(messages, i, resolver);
                out.extend(replay(&[ReplayMessage {
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
    out
}

#[async_trait]
impl ModelStream for ChatCompletionsLlm {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
    ) -> Result<AssistantMessage, LlmError> {
        let model = request.model;
        check_effort(model, &self.caps)?;

        let system = replay_system(request.messages);
        let messages = build_messages(&system.prompt, request.messages, request.options.images);
        let effort = EffortSetting::from_option(model.effort.clone());
        let body = request_body(
            model,
            &messages,
            &system.tools,
            effort.as_option().map(|effort| effort.as_str()),
        );
        let http = HttpRequest::new(
            format!("{}/chat/completions", model.base_url.trim_end_matches('/')),
            body,
        )
        .with_key(self.api_key.clone())
        .with_timeout(Duration::from_secs(model.timeout_secs.max(1)));

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
                api_backend = "chat_completions",
                stateless = true,
                tool_calls = message.tool_calls().count(),
                elapsed_ms,
                "model stream completed"
            ),
            // first_byte_ms 只在收到过数据时出现：没有它就是零字节失败。
            Err(error) => tracing::warn!(
                first_byte_ms = stats.0,
                bytes = stats.1,
                role = %self.role,
                model = %model.model,
                api_backend = "chat_completions",
                stateless = true,
                elapsed_ms,
                error = %error,
                "model stream failed"
            ),
        }
        outcome
    }
}

impl ChatCompletionsLlm {
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
            if !chunk.is_empty() {
                stats.0.get_or_insert(started.elapsed().as_millis() as u64);
                stats.1 += chunk.len();
            }
            let mut changed = false;
            for frame in decoder.push(&chunk) {
                assembled.absorb(&frame.data);
                changed = true;
            }
            if assembled.completed {
                break;
            }
            if changed {
                progress(&partial_message(
                    model,
                    API_CHAT,
                    assembled.content_so_far(),
                ));
            }
        }
        if !assembled.completed {
            return Err(LlmError::Incomplete);
        }
        let round = assembled.finish(1)?;
        Ok(message_from_round(model, round))
    }
}

/// `reasoning_details` 里能读出文字的部分（`text`，或 `summary`）。
fn reasoning_text(details: &Value) -> String {
    details
        .as_array()
        .map(|details| {
            details
                .iter()
                .filter_map(|detail| {
                    detail
                        .get("text")
                        .or_else(|| detail.get("summary"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

fn message_from_round(model: &ModelConfig, round: Round) -> AssistantMessage {
    let mut content = Vec::new();
    if let Some(native) = &round.provider_blocks {
        let thinking = native
            .get("reasoning_details")
            .map(reasoning_text)
            .unwrap_or_default();
        if !thinking.is_empty() {
            content.push(thinking_block(thinking, Map::new()));
        }
    }
    if let Some(text) = &round.text {
        content.push(ContentBlock::text(text.clone()));
    }
    for call in &round.tool_calls {
        content.push(ContentBlock::tool_call(
            call.provider_call_id.clone(),
            call.name.clone(),
            call.arguments.clone(),
        ));
    }
    final_message(
        model,
        API_CHAT,
        content,
        stop_reason(round.truncated, !round.tool_calls.is_empty()),
        &round.usage,
        round.provider_blocks,
    )
}

impl ChatAssembled {
    fn content_so_far(&self) -> Vec<ContentBlock> {
        let mut content = Vec::new();
        let thinking = reasoning_text(&Value::Array(self.reasoning_details.clone()));
        if !thinking.is_empty() {
            content.push(thinking_block(thinking, Map::new()));
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::text(self.text.clone()));
        }
        for call in self.calls.values() {
            content.push(ContentBlock::tool_call(
                call.id.clone(),
                call.name.clone(),
                lenient_arguments(&call.arguments),
            ));
        }
        content
    }
}
