//! Anthropic Messages 的无状态入口（[`ModelStream`]）。
//!
//! 拼体与解析**全部复用**父模块里旧 driver 的函数：回放的单条构造（`user_message` /
//! `assistant_message` / `tool_results_message` / `append_message`）、[`request_body`]（含
//! 三处缓存断点）、[`Assembled`]（SSE 拼装与收尾）。这里新写的只有把 `&[Message]` 摆成那些
//! 函数要的形状，以及从 [`Round`] 投影出 [`AssistantMessage`]。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use komo_kernel::durable::{AssistantMessage, ContentBlock, Message};
use komo_kernel::types::model::{EffortSetting, ModelConfig};
use komo_kernel::types::turn::{LlmError, Round};
use serde_json::{Map, Value, json};

use super::{
    ANTHROPIC_VERSION, ANTHROPIC_VERSION_HEADER, AnthropicMessagesLlm, Assembled,
    DEFAULT_MAX_TOKENS, PartialBlock, RESPONSE_HEADROOM, append_message, assistant_message,
    check_effort, request_body, thinking_budget, tool_results_message, user_message,
};
use crate::llm::model_stream::{
    API_ANTHROPIC, ImageResolver, ModelRequest, ModelStream, UserPart, assistant_replay,
    final_message, lenient_arguments, partial_message, plain_text, replay_system, stop_reason,
    thinking_block, tool_result_run, transport_error, user_parts,
};
use crate::llm::sse::SseDecoder;
use crate::llm::transport::HttpRequest;
use crate::llm::wire;

/// 一条 user 消息，带图片时是文字块 + base64 图片块（和工具结果里的图片同一个表达）。
fn user_with_images(parts: &[UserPart]) -> Value {
    let content: Vec<Value> = parts
        .iter()
        .map(|part| match part {
            UserPart::Text(text) => json!({ "type": "text", "text": text }),
            UserPart::Image(image) => json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.mime,
                    "data": crate::llm::image_base64(image),
                },
            }),
        })
        .collect();
    json!({ "role": "user", "content": content })
}

/// `messages`（system 消息在别处回放）→ Anthropic 的 `messages`，以及 `revised_from`
/// （`messages` 下标）换算成合并后的下标。
fn build_messages(
    messages: &[Message],
    resolver: Option<&dyn ImageResolver>,
    revised_from: Option<usize>,
) -> (Vec<Value>, Option<usize>) {
    let mut out: Vec<Value> = Vec::new();
    // `messages[i]` 并进了 `out` 的第几条（system 消息不进）。
    let mut landed: Vec<Option<usize>> = vec![None; messages.len()];
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::System(_) => {}
            Message::User(user) => {
                let parts = user_parts(user, resolver);
                let built = match plain_text(&parts) {
                    Some(text) => user_message(&text),
                    None => user_with_images(&parts),
                };
                append_message(&mut out, built);
                landed[i] = Some(out.len() - 1);
            }
            Message::Assistant(assistant) => {
                let replay = assistant_replay(assistant, i, API_ANTHROPIC, Value::is_array);
                append_message(&mut out, assistant_message(&replay));
                landed[i] = Some(out.len() - 1);
            }
            Message::ToolResult(_) => {
                // 一轮的结果并进同一条 user 消息（旧 driver 同）。
                let (results, end) = tool_result_run(messages, i, resolver);
                append_message(&mut out, tool_results_message(&results));
                for slot in &mut landed[i..end] {
                    *slot = Some(out.len() - 1);
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    let revised = revised_from.and_then(|from| landed.get(from).copied().flatten());
    (out, revised)
}

#[async_trait]
impl ModelStream for AnthropicMessagesLlm {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
    ) -> Result<AssistantMessage, LlmError> {
        let model = request.model;
        check_effort(model, &self.caps)?;

        let system = replay_system(request.messages);
        let (messages, revised_from) = build_messages(
            request.messages,
            request.options.images,
            request.options.revised_from,
        );
        let effort = EffortSetting::from_option(model.effort.clone());
        let thinking_tokens = effort.as_option().and_then(thinking_budget);
        let max_tokens = request.options.max_output_tokens.unwrap_or_else(|| {
            thinking_tokens.map_or(DEFAULT_MAX_TOKENS, |budget| budget + RESPONSE_HEADROOM)
        });
        let body = request_body(
            model,
            &system.prompt,
            &messages,
            &system.tools,
            revised_from,
            thinking_tokens,
            max_tokens,
        );

        let mut http = HttpRequest::new(self.endpoint(model), body)
            .with_header(ANTHROPIC_VERSION_HEADER, ANTHROPIC_VERSION)
            .with_timeout(Duration::from_secs(model.timeout_secs.max(1)));
        if let Some(key) = &self.api_key {
            http = http.with_header("x-api-key", key.clone());
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
                api_backend = "anthropic_messages",
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
                api_backend = "anthropic_messages",
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

impl AnthropicMessagesLlm {
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
                    API_ANTHROPIC,
                    assembled.content_so_far(),
                ));
            }
        }
        if !assembled.settled() {
            tracing::warn!(
                mid_frame = decoder.has_trailing_bytes(),
                "流结束时没有收到 message_stop"
            );
            return Err(LlmError::Incomplete);
        }
        let round = assembled.finish(1)?;
        Ok(message_from_round(model, round))
    }
}

/// 一个原生 content 块 → provider 无关的块。认不得的块（别的 provider 的私有块）不进
/// `content`，但仍在 `providerBlocks` 里原样带着。
fn content_block(block: &Value) -> Option<ContentBlock> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => Some(ContentBlock::text(block.get("text")?.as_str()?)),
        "thinking" => {
            let mut extra = Map::new();
            if let Some(signature) = block.get("signature") {
                extra.insert("thinkingSignature".into(), signature.clone());
            }
            Some(thinking_block(
                block.get("thinking")?.as_str()?.to_string(),
                extra,
            ))
        }
        "redacted_thinking" => {
            let mut extra = Map::new();
            extra.insert("redacted".into(), json!(true));
            Some(thinking_block(String::new(), extra))
        }
        "tool_use" => {
            // 流进行中的块还带着没解析的原始 JSON 文本。
            let arguments = match block.get("__raw_input").and_then(Value::as_str) {
                Some(raw) => lenient_arguments(raw),
                None => block.get("input").cloned().unwrap_or_else(|| json!({})),
            };
            Some(ContentBlock::tool_call(
                block.get("id")?.as_str()?,
                block.get("name")?.as_str()?,
                arguments,
            ))
        }
        _ => None,
    }
}

fn message_from_round(model: &ModelConfig, round: Round) -> AssistantMessage {
    let content = match &round.provider_blocks {
        Some(Value::Array(blocks)) => blocks.iter().filter_map(content_block).collect(),
        _ => Vec::new(),
    };
    final_message(
        model,
        API_ANTHROPIC,
        content,
        stop_reason(round.truncated, !round.tool_calls.is_empty()),
        &round.usage,
        round.provider_blocks,
    )
}

impl Assembled {
    /// 到目前为止收到的块（已收尾的 + 还在拼的），按 provider 的块序。
    fn content_so_far(&self) -> Vec<ContentBlock> {
        let mut by_index: BTreeMap<usize, ContentBlock> = BTreeMap::new();
        for (index, item) in &self.items {
            if let Some(block) = content_block(item) {
                by_index.insert(*index, block);
            }
        }
        for (index, partial) in &self.blocks {
            let block = match partial {
                PartialBlock::Text(text) => ContentBlock::text(text.clone()),
                PartialBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    let mut extra = Map::new();
                    if !signature.is_empty() {
                        extra.insert("thinkingSignature".into(), json!(signature));
                    }
                    thinking_block(thinking.clone(), extra)
                }
                PartialBlock::ToolUse {
                    id,
                    name,
                    partial_json,
                } => ContentBlock::tool_call(
                    id.clone(),
                    name.clone(),
                    lenient_arguments(partial_json),
                ),
                PartialBlock::Verbatim(value) => match content_block(value) {
                    Some(block) => block,
                    None => continue,
                },
            };
            by_index.insert(*index, block);
        }
        by_index.into_values().collect()
    }
}
