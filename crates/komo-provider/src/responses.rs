//! The OpenAI Responses API codec.
//!
//! One codec, four providers: OpenAI, Codex (the ChatGPT backend speaks the same
//! surface), DeepSeek, and OpenRouter. That is the whole reason komo targets
//! Responses rather than Chat Completions — the differences between those four
//! collapse into a base URL and an auth mode, and the two things komo cares most
//! about are first-class here instead of bolted on:
//!
//! - **`instructions` is a top-level field.** The system prompt is not a message,
//!   so it never competes with history for a slot and never has to be lifted out
//!   of `input` by rewriting the request body (which is exactly what komo used to
//!   do to satisfy the Codex backend).
//! - **Reasoning round-trips.** `include: ["reasoning.encrypted_content"]` with
//!   `store: false` hands the model its own prior reasoning back on the next
//!   round of a tool loop without komo storing anything server-side, so a
//!   reasoning model keeps its chain of thought across tool calls.

use serde_json::{Value, json};

use super::error::{LlmError, LlmErrorKind, classify_status, retry_after_from_message};
use super::transport::SseStream;
use super::types::{AssistantBlock, Completion, Reasoning, ToolSchema, Turn, Usage, UserBlock};

/// Build the request body for one round.
///
/// `extra` carries the per-turn knobs the caller resolved (reasoning effort,
/// `prompt_cache_key`, `max_output_tokens`) and is merged in last so a caller can
/// override anything here.
pub fn request(
    model: &str,
    instructions: &str,
    history: &[Turn],
    tools: &[ToolSchema],
    extra: Option<&Value>,
) -> Value {
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": input_items(history),
        "stream": true,
        // komo replays the whole conversation every turn, which is what lets a
        // session switch models (even across providers) mid-conversation. Asking
        // the provider to also retain it would be a second, divergent source of
        // truth — and the Codex backend refuses server-side storage outright.
        "store": false,
        // Hand the model its own reasoning back across tool rounds. Costs
        // nothing when the model does not reason.
        "include": ["reasoning.encrypted_content"],
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.iter().map(tool_schema).collect());
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(true);
    }
    // Merged last, so a caller's explicit knob always wins over the defaults
    // above. A non-object `extra` is ignored rather than replacing the body.
    if let Some(extra) = extra.and_then(Value::as_object)
        && let Some(base) = body.as_object_mut()
    {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    body
}

/// A tool declaration in Responses shape: flat, not nested under `function` the
/// way Chat Completions nests it.
fn tool_schema(tool: &ToolSchema) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    })
}

/// Render komo's conversation as Responses `input` items.
///
/// Every block becomes its own item — a tool call and its result are siblings at
/// the top level here, not nested inside messages the way Chat Completions does
/// it. That flatness is what makes the round-trip faithful: whatever the model
/// emitted comes back in the same order it emitted it.
fn input_items(history: &[Turn]) -> Vec<Value> {
    let mut items = Vec::new();
    for turn in history {
        match turn {
            Turn::User(blocks) => {
                // Text blocks merge into one message item; tool results are
                // their own items (the API has no "user message containing a
                // tool result" shape).
                let mut text_parts = Vec::new();
                for block in blocks {
                    match block {
                        UserBlock::Text(text) => {
                            text_parts.push(json!({ "type": "input_text", "text": text }));
                        }
                        UserBlock::ToolResult { call_id, id, text } => {
                            items.push(json!({
                                "type": "function_call_output",
                                // The Responses API correlates on `call_id`;
                                // `id` is the fallback for a provider that only
                                // gave us the one handle.
                                "call_id": call_id.clone().unwrap_or_else(|| id.clone()),
                                "output": text,
                            }));
                        }
                    }
                }
                if !text_parts.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": text_parts,
                    }));
                }
            }
            Turn::Assistant { id, blocks } => {
                // Emit in block order, flushing accumulated text before any
                // non-text item. Deferring the text to the end (the old shape)
                // reordered "narrate, then call" into "call, then narrate",
                // which put a message item between a function_call and its
                // function_call_output — DeepSeek rejects that adjacency break
                // with "No tool output found for tool call".
                let mut text_parts = Vec::new();
                let mut message_id = id.clone();
                let flush = |items: &mut Vec<Value>,
                             text_parts: &mut Vec<Value>,
                             message_id: &mut Option<String>| {
                    if text_parts.is_empty() {
                        return;
                    }
                    let mut item = json!({
                        "type": "message",
                        "role": "assistant",
                        "content": std::mem::take(text_parts),
                    });
                    if let Some(id) = message_id.take() {
                        item["id"] = json!(id);
                    }
                    items.push(item);
                };
                for block in blocks {
                    match block {
                        AssistantBlock::Text(text) => {
                            text_parts.push(json!({ "type": "output_text", "text": text }));
                        }
                        AssistantBlock::ToolCall {
                            id,
                            call_id,
                            name,
                            args,
                        } => {
                            flush(&mut items, &mut text_parts, &mut message_id);
                            let mut item = json!({
                                "type": "function_call",
                                "name": name,
                                "arguments": args,
                                "call_id": call_id.clone().unwrap_or_else(|| id.clone()),
                            });
                            if !id.is_empty() {
                                item["id"] = json!(id);
                            }
                            items.push(item);
                        }
                        AssistantBlock::Reasoning(reasoning) => {
                            flush(&mut items, &mut text_parts, &mut message_id);
                            items.push(reasoning_item(reasoning));
                        }
                    }
                }
                flush(&mut items, &mut text_parts, &mut message_id);
            }
        }
    }
    items
}

fn reasoning_item(reasoning: &Reasoning) -> Value {
    let mut item = json!({ "type": "reasoning" });
    // An item carrying plain text is DeepSeek's shape, and DeepSeek documents
    // `summary` as unsupported on input — so it must not receive the empty
    // `summary: []` every other item still gets. The condition is "has text"
    // rather than "summary is empty" because an OpenAI/Codex item routinely has
    // an empty summary beside its encrypted blob (komo never asks for one), and
    // `summary` is a required field of that wire's reasoning item.
    if reasoning.text.is_empty() {
        item["summary"] = reasoning
            .summary
            .iter()
            .map(|text| json!({ "type": "summary_text", "text": text }))
            .collect();
    }
    if !reasoning.text.is_empty() {
        item["content"] = reasoning
            .text
            .iter()
            .map(|text| json!({ "type": "reasoning_text", "text": text }))
            .collect();
    }
    if let Some(id) = &reasoning.id {
        item["id"] = json!(id);
    }
    if let Some(encrypted) = &reasoning.encrypted {
        item["encrypted_content"] = json!(encrypted);
    }
    item
}

/// What one SSE frame told us. The caller ([`collect`]) folds these into a
/// [`Completion`]; a future streaming surface can act on the deltas as they
/// arrive without re-parsing anything.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// A chunk of the assistant's visible answer.
    TextDelta(String),
    /// A chunk of the model's reasoning summary.
    ReasoningDelta(String),
    /// A chunk of a tool call's arguments, as it is being written.
    ToolArgsDelta {
        call_id: Option<String>,
        delta: String,
    },
    /// A finished output item (message / function_call / reasoning).
    ItemDone(Value),
    /// The response finished; carries the id and usage.
    Completed { id: Option<String>, usage: Usage },
    /// The provider reported a failure mid-stream.
    Failed(LlmError),
    /// A frame this codec does not act on.
    Ignored,
}

/// Interpret one SSE frame.
pub fn event(frame: &Value) -> Event {
    let kind = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "response.output_text.delta" => Event::TextDelta(delta_text(frame)),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            Event::ReasoningDelta(delta_text(frame))
        }
        "response.function_call_arguments.delta" => Event::ToolArgsDelta {
            call_id: frame
                .get("item_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            delta: delta_text(frame),
        },
        "response.output_item.done" => match frame.get("item") {
            Some(item) => Event::ItemDone(item.clone()),
            None => Event::Ignored,
        },
        "response.completed" => {
            let response = frame.get("response");
            Event::Completed {
                id: response
                    .and_then(|r| r.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                usage: usage(response.and_then(|r| r.get("usage"))),
            }
        }
        // `response.incomplete` is a truncated answer (hit a token cap or a
        // filter). Treated as failed rather than silently returning a partial
        // reply: the round did not produce the answer it was asked for.
        "response.failed" | "response.incomplete" => {
            let error = frame
                .get("response")
                .and_then(|r| r.get("error"))
                .or_else(|| frame.get("error"));
            Event::Failed(stream_error(error, kind))
        }
        "error" => Event::Failed(stream_error(frame.get("error").or(Some(frame)), kind)),
        _ => Event::Ignored,
    }
}

fn delta_text(frame: &Value) -> String {
    frame
        .get("delta")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Build a typed error out of a `response.failed` / `error` frame.
///
/// The same classification the HTTP boundary applies, because the provider
/// reports the same failures both ways: a context overflow can arrive as a 400
/// *or* as a `response.failed` frame carrying `context_length_exceeded`, and both
/// have to reach the driver's degrade path.
fn stream_error(error: Option<&Value>, frame_kind: &str) -> LlmError {
    let code = error
        .and_then(|e| e.get("code").or_else(|| e.get("type")))
        .and_then(Value::as_str)
        .map(str::to_string);
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(frame_kind)
        .to_string();
    // No HTTP status here — the stream had already been accepted — so the kind
    // comes from the code and the prose. 200 is the "status" that was actually
    // on the response.
    let kind = classify_status(200, code.as_deref(), &message);
    // A mid-stream failure with no recognizable code is still a broken round,
    // which is retryable; `classify_status` would call it InvalidRequest.
    let kind = if code.is_none() && kind == LlmErrorKind::InvalidRequest {
        LlmErrorKind::Stream
    } else {
        kind
    };
    LlmError::new(kind, message.clone())
        .with_code(code)
        .with_retry_after(retry_after_from_message(&message))
}

fn usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    let field = |name: &str| usage.get(name).and_then(Value::as_i64).unwrap_or(0);
    Usage {
        input: field("input_tokens"),
        output: field("output_tokens"),
        cached_input: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
    }
}

/// Drain `stream` into one assistant message.
///
/// The terminal-event rule lives here: a stream that ends without
/// `response.completed` is a [`LlmErrorKind::Stream`] failure, never a short
/// answer. That is what makes "retry the round" safe as a blanket response to a
/// dropped connection — the alternative, treating whatever arrived as the reply,
/// silently truncates answers and loses tool calls.
pub async fn collect(
    stream: &mut SseStream,
    on_delta: Option<super::OnDelta<'_>>,
) -> Result<Completion, LlmError> {
    let mut blocks: Vec<AssistantBlock> = Vec::new();
    let mut id = None;
    let mut usage = Usage::default();
    let mut completed = false;

    while let Some(frame) = stream.next().await? {
        match event(&frame) {
            Event::ItemDone(item) => {
                // The *message item's* id, not the response id: this is what
                // `input_items` echoes back as the item's `id` next round, and
                // the API validates the prefix there ("Invalid 'input[n].id':
                // 'resp_…'. Expected an ID that begins with 'msg'").
                if id.is_none() && item.get("type").and_then(Value::as_str) == Some("message") {
                    id = item.get("id").and_then(Value::as_str).map(str::to_string);
                }
                if let Some(block) = item_to_block(&item) {
                    blocks.push(block);
                }
            }
            // The response id in this frame is deliberately dropped: `store:
            // false` means nothing correlates on it, and it is not a valid
            // input-item id.
            Event::Completed {
                usage: reported, ..
            } => {
                usage = reported;
                completed = true;
                // The provider may keep the connection open briefly after the
                // terminal frame; the answer is complete either way.
                break;
            }
            Event::Failed(error) => return Err(error),
            // Deltas go to the watcher (when there is one) and are otherwise
            // dropped: the finished items carry the same content, so the
            // returned completion is identical either way.
            Event::TextDelta(delta) => {
                if let Some(on_delta) = on_delta {
                    on_delta(super::Delta::Text(&delta));
                }
            }
            Event::ReasoningDelta(delta) => {
                if let Some(on_delta) = on_delta {
                    on_delta(super::Delta::Reasoning(&delta));
                }
            }
            Event::ToolArgsDelta { .. } | Event::Ignored => {}
        }
    }

    if !completed {
        return Err(LlmError::stream(
            "the provider closed the stream before completing the response",
        ));
    }
    Ok(Completion { id, blocks, usage })
}

/// Convert a finished output item into an assistant block. Items komo does not
/// model (web search calls, images) yield `None` and are dropped: they are not
/// something komo asked for, and echoing an item back that we cannot render
/// would corrupt the next round's history.
fn item_to_block(item: &Value) -> Option<AssistantBlock> {
    match item.get("type").and_then(Value::as_str)? {
        "message" => {
            let text = item
                .get("content")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect::<String>();
            (!text.is_empty()).then_some(AssistantBlock::Text(text))
        }
        "function_call" => {
            let str_field = |name: &str| {
                item.get(name)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let call_id = str_field("call_id");
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(&call_id)
                .to_string();
            Some(AssistantBlock::ToolCall {
                id,
                call_id: (!call_id.is_empty()).then_some(call_id),
                name: str_field("name"),
                args: str_field("arguments"),
            })
        }
        "reasoning" => Some(AssistantBlock::Reasoning(Reasoning {
            id: item.get("id").and_then(Value::as_str).map(str::to_string),
            summary: item
                .get("summary")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            encrypted: item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(str::to_string),
            // DeepSeek returns the chain of thought itself here instead of a
            // summary or a blob; without this the item parses to nothing.
            text: item
                .get("content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|p| p.get("type").and_then(Value::as_str) == Some("reasoning_text"))
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> ToolSchema {
        ToolSchema {
            name: "read".into(),
            description: "read a file".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    #[test]
    fn the_system_prompt_is_a_field_not_a_message() {
        // The whole reason for targeting Responses: no lifting a system message
        // out of `input` (which is what the Codex backend forced komo to do by
        // rewriting the request body).
        let body = request("gpt-5", "you are komo", &[Turn::user("hi")], &[], None);
        assert_eq!(body["instructions"], "you are komo");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(body["store"], false, "komo owns the transcript");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn tools_are_declared_flat_and_only_when_present() {
        let none = request("m", "s", &[Turn::user("hi")], &[], None);
        assert!(
            none.get("tools").is_none(),
            "a tool-less call must advertise no tools at all"
        );

        let with = request("m", "s", &[Turn::user("hi")], &[schema()], None);
        assert_eq!(with["tools"][0]["type"], "function");
        assert_eq!(with["tools"][0]["name"], "read", "flat, not nested");
        assert_eq!(with["tool_choice"], "auto");
    }

    #[test]
    fn extra_params_are_merged_last_so_a_caller_can_override() {
        let body = request(
            "m",
            "s",
            &[Turn::user("hi")],
            &[],
            Some(&json!({ "reasoning": { "effort": "high" }, "store": true })),
        );
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["store"], true, "an explicit override wins");
    }

    #[test]
    fn a_tool_call_and_its_result_round_trip_as_sibling_items() {
        let history = vec![
            Turn::user("read foo"),
            Turn::Assistant {
                id: Some("msg_1".into()),
                blocks: vec![
                    AssistantBlock::Text("checking".into()),
                    AssistantBlock::ToolCall {
                        id: "fc_1".into(),
                        call_id: Some("call_1".into()),
                        name: "read".into(),
                        args: r#"{"path":"foo"}"#.into(),
                    },
                ],
            },
            Turn::User(vec![UserBlock::ToolResult {
                id: "fc_1".into(),
                call_id: Some("call_1".into()),
                text: "contents".into(),
            }]),
        ];
        let body = request("m", "s", &history, &[schema()], None);
        let input = body["input"].as_array().unwrap();
        let types: Vec<&str> = input.iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            vec![
                "message",
                "message",
                "function_call",
                "function_call_output"
            ],
            "each block is its own top-level item, in the model's own order — \
             nothing may sit between a function_call and its output"
        );
        // The call and its output must agree on the handle the API keys on.
        let call = input.iter().find(|i| i["type"] == "function_call").unwrap();
        let output = input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap();
        assert_eq!(call["call_id"], output["call_id"]);
        assert_eq!(call["arguments"], r#"{"path":"foo"}"#);
    }

    /// The regression behind DeepSeek's `No tool output found for tool call`
    /// 400: narration emitted *before* the calls must stay before them, or a
    /// message item lands between a function_call and its
    /// function_call_output and strict providers reject the request.
    #[test]
    fn narration_before_tool_calls_keeps_its_place() {
        let history = vec![
            Turn::Assistant {
                id: None,
                blocks: vec![
                    AssistantBlock::Text("let me check two things".into()),
                    AssistantBlock::ToolCall {
                        id: "fc_1".into(),
                        call_id: Some("call_1".into()),
                        name: "read".into(),
                        args: "{}".into(),
                    },
                    AssistantBlock::ToolCall {
                        id: "fc_2".into(),
                        call_id: Some("call_2".into()),
                        name: "read".into(),
                        args: "{}".into(),
                    },
                ],
            },
            Turn::User(vec![
                UserBlock::ToolResult {
                    id: "fc_1".into(),
                    call_id: Some("call_1".into()),
                    text: "a".into(),
                },
                UserBlock::ToolResult {
                    id: "fc_2".into(),
                    call_id: Some("call_2".into()),
                    text: "b".into(),
                },
            ]),
        ];
        let body = request("m", "s", &history, &[schema()], None);
        let input = body["input"].as_array().unwrap();
        let types: Vec<&str> = input.iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            vec![
                "message",
                "function_call",
                "function_call",
                "function_call_output",
                "function_call_output"
            ],
            "only outputs may follow the round's function_calls"
        );
    }

    #[test]
    fn reasoning_rides_back_verbatim() {
        // The point of `include: reasoning.encrypted_content`: the blob we were
        // handed goes back untouched, or the model loses its chain of thought
        // between tool rounds.
        let history = vec![Turn::Assistant {
            id: None,
            blocks: vec![AssistantBlock::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                summary: vec!["thinking about it".into()],
                encrypted: Some("OPAQUE".into()),
                text: Vec::new(),
            })],
        }];
        let body = request("m", "s", &history, &[], None);
        let item = &body["input"][0];
        assert_eq!(item["type"], "reasoning");
        assert_eq!(item["id"], "rs_1");
        assert_eq!(item["encrypted_content"], "OPAQUE");
        assert_eq!(item["summary"][0]["type"], "summary_text");
        assert!(item.get("content").is_none(), "no plain text to send back");
    }

    #[test]
    fn a_summaryless_openai_item_still_carries_an_empty_summary() {
        // komo never asks for a reasoning summary, so this — an encrypted blob
        // and nothing else — is the ordinary OpenAI/Codex item. `summary` is a
        // required field of that wire's reasoning item, so it stays.
        let history = vec![Turn::Assistant {
            id: None,
            blocks: vec![AssistantBlock::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                summary: Vec::new(),
                encrypted: Some("OPAQUE".into()),
                text: Vec::new(),
            })],
        }];
        let body = request("m", "s", &history, &[], None);
        assert_eq!(body["input"][0]["summary"], json!([]));
    }

    #[test]
    fn deepseek_reasoning_is_plain_text_in_both_directions() {
        // DeepSeek emits neither a summary nor an encrypted blob — the chain of
        // thought itself arrives as `content[].reasoning_text`, and parsing only
        // the other two fields left an empty block that carried nothing back.
        let block = item_to_block(&json!({
            "type": "reasoning",
            "id": "rs_1",
            "content": [{ "type": "reasoning_text", "text": "think" }],
        }));
        let Some(AssistantBlock::Reasoning(reasoning)) = block else {
            panic!("expected a reasoning block, got {block:?}");
        };
        assert_eq!(reasoning.text, ["think"]);
        assert!(reasoning.summary.is_empty());
        assert_eq!(reasoning.encrypted, None);

        // Going back: the text rides along and no empty `summary` is sent, which
        // DeepSeek does not accept on input.
        let history = vec![Turn::Assistant {
            id: None,
            blocks: vec![AssistantBlock::Reasoning(reasoning)],
        }];
        let item = &request("deepseek-v4-flash", "s", &history, &[], None)["input"][0];
        assert_eq!(item["content"][0]["type"], "reasoning_text");
        assert_eq!(item["content"][0]["text"], "think");
        assert!(item.get("summary").is_none(), "unsupported by deepseek");
        assert_eq!(item["id"], "rs_1");
    }

    #[test]
    fn events_are_recognised_by_type() {
        assert_eq!(
            event(&json!({ "type": "response.output_text.delta", "delta": "hel" })),
            Event::TextDelta("hel".into())
        );
        assert_eq!(
            event(&json!({ "type": "response.reasoning_summary_text.delta", "delta": "hm" })),
            Event::ReasoningDelta("hm".into())
        );
        assert_eq!(
            event(&json!({ "type": "response.created" })),
            Event::Ignored
        );
        match event(&json!({
            "type": "response.completed",
            "response": { "id": "resp_1", "usage": {
                "input_tokens": 10, "output_tokens": 4,
                "input_tokens_details": { "cached_tokens": 8 }
            }}
        })) {
            Event::Completed { id, usage } => {
                assert_eq!(id.as_deref(), Some("resp_1"));
                assert_eq!(usage.input, 10);
                assert_eq!(usage.output, 4);
                assert_eq!(usage.cached_input, 8, "cache hits are worth knowing");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// A mid-stream overflow has to classify the same as an HTTP-status one, or
    /// the driver's degrade path never runs for providers that report it here.
    #[test]
    fn a_mid_stream_overflow_still_reads_as_overflow() {
        let failed = json!({
            "type": "response.failed",
            "response": { "error": {
                "code": "context_length_exceeded",
                "message": "too long"
            }}
        });
        match event(&failed) {
            Event::Failed(error) => {
                assert!(error.is_context_overflow());
                assert!(
                    !error.is_retryable(),
                    "re-sending the same bytes cannot fit"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// An unlabelled mid-stream failure is a broken round, not a bad request:
    /// classifying it as terminal would throw away a turn that a retry recovers.
    #[test]
    fn an_unlabelled_stream_failure_is_retryable() {
        match event(&json!({ "type": "response.failed" })) {
            Event::Failed(error) => {
                assert_eq!(error.kind, LlmErrorKind::Stream);
                assert!(error.is_retryable());
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_rate_limit_frame_carries_the_servers_own_delay() {
        match event(&json!({
            "type": "error",
            "error": { "code": "rate_limit_exceeded", "message": "Please try again in 2.5s" }
        })) {
            Event::Failed(error) => {
                assert_eq!(error.kind, LlmErrorKind::RateLimited);
                assert_eq!(
                    error.retry_after,
                    Some(std::time::Duration::from_millis(2500))
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// The regression behind `Invalid 'input[2].id': 'resp_…'. Expected an ID
    /// that begins with 'msg'`: the assistant turn's id is echoed back as the
    /// message item's own `id` next round, so it has to come from the message
    /// item — never from `response.completed`, whose id names the response.
    #[tokio::test]
    async fn the_kept_id_is_the_message_items_own_not_the_responses() {
        let frames = [
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "id": "msg_1",
                    "content": [{ "type": "output_text", "text": "hi" }],
                },
            }),
            json!({
                "type": "response.completed",
                "response": { "id": "resp_1", "usage": { "input_tokens": 1, "output_tokens": 1 } },
            }),
        ];
        let body = frames
            .iter()
            .map(|f| format!("data: {f}\n\n"))
            .collect::<String>();
        let mut stream = SseStream::from_body(&body);
        let completion = collect(&mut stream, None).await.unwrap();
        assert_eq!(completion.id.as_deref(), Some("msg_1"));

        // And the round-trip the API actually validates.
        let history = vec![Turn::Assistant {
            id: completion.id,
            blocks: completion.blocks,
        }];
        let body = request("m", "s", &history, &[], None);
        assert_eq!(body["input"][0]["id"], "msg_1");
    }

    #[test]
    fn finished_items_become_blocks() {
        let text = item_to_block(&json!({
            "type": "message",
            "content": [{ "type": "output_text", "text": "answer" }]
        }));
        assert_eq!(text, Some(AssistantBlock::Text("answer".into())));

        let call = item_to_block(&json!({
            "type": "function_call",
            "id": "fc_1", "call_id": "call_1",
            "name": "read", "arguments": "{}"
        }));
        assert_eq!(
            call,
            Some(AssistantBlock::ToolCall {
                id: "fc_1".into(),
                call_id: Some("call_1".into()),
                name: "read".into(),
                args: "{}".into(),
            })
        );

        // Items komo does not model are dropped rather than guessed at.
        assert_eq!(item_to_block(&json!({ "type": "web_search_call" })), None);
    }
}
