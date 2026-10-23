//! 无状态入口（[`ModelStream`]）与旧有状态 driver 的**字节等价**与**解析等价**。
//!
//! 每个场景在两条路上各跑一遍，两边的"服务端"是各自一份脚本相同的 [`ScriptedTransport`]：
//!
//! - 旧：`begin_turn` + `next(First)` + `next(ToolResults { results, revised })`……
//!   driver 在内存里攒着对话，衰减靠 `revised` 就地换；
//! - 新：每一轮拿**到目前为止的完整 `Message` 序列**调 `stream`，把返回的 assistant
//!   消息原样追加、工具结果追加成 toolResult 消息；衰减 = 直接改那条 toolResult 消息的正文
//!   （Anthropic 另给 `revised_from`）。
//!
//! 断言：第 k 次请求的 JSON **序列化后逐字节相同**（提示缓存命中的前提），而且每一轮解析出的
//! 文本 / 调用 / 用量 / stopReason / 原生块与旧 driver 的 [`Round`] 一致。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use komo_kernel::durable::{
    AssistantMessage, Content, ContentBlock, Message, Sections, StopReason, SystemMessage,
    ToolResultMessage,
};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::model::{Effort, ModelConfig, ModelRole};
use komo_kernel::types::tool::{ToolDefinition, ToolImage};
use komo_kernel::types::turn::{
    LlmError, ReplayMessage, Role, Round, RoundInput, ToolCallRequest, ToolResultForModel,
    TurnRequest,
};
use serde_json::{Map, Value, json};

use super::transport::testing::{Reply, ScriptedTransport};
use super::*;
use crate::config::{EffortCapabilities, Secrets};

// ---------------------------------------------------------------- 三家的夹具

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum P {
    Anthropic,
    Responses,
    Chat,
}

const ALL: [P; 3] = [P::Anthropic, P::Responses, P::Chat];

fn line(value: Value) -> String {
    serde_json::to_string(&value).unwrap()
}

fn event(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {}\n\n", line(data))
}

fn data(value: Value) -> String {
    format!("data: {}\n\n", line(value))
}

/// 一轮回复里模型"说"了什么。
#[derive(Clone, Default)]
struct Say {
    thinking: Option<&'static str>,
    text: Option<&'static str>,
    /// `(call_id, name, arguments)`
    calls: Vec<(&'static str, &'static str, Value)>,
}

fn say_text(text: &'static str) -> Say {
    Say {
        text: Some(text),
        ..Say::default()
    }
}

fn say_call(call_id: &'static str, name: &'static str, arguments: Value) -> Say {
    Say {
        calls: vec![(call_id, name, arguments)],
        ..Say::default()
    }
}

impl P {
    fn config(self, effort: Option<&str>) -> ModelConfig {
        ModelConfig {
            provider: match self {
                P::Anthropic => ANTHROPIC_MESSAGES,
                P::Responses => RESPONSES,
                P::Chat => CHAT_COMPLETIONS,
            }
            .into(),
            base_url: "https://llm.example.com/v1".into(),
            model: "m-1".into(),
            api_key_env: "KOMO_LLM_API_KEY".into(),
            auth: None,
            effort: effort.map(Effort::new),
            efforts: None,
            timeout_secs: 10,
            context_window: None,
        }
    }

    fn api(self) -> &'static str {
        match self {
            P::Anthropic => API_ANTHROPIC,
            P::Responses => API_RESPONSES,
            P::Chat => API_CHAT,
        }
    }

    /// 一轮正常收尾的回复，分成逐帧的块（半帧、跨帧拼接另有测试）。
    fn reply(self, say: &Say, round: u32) -> Reply {
        let frames = match self {
            P::Anthropic => anthropic_frames(say, round, false),
            P::Responses => responses_frames(say, round, false),
            P::Chat => chat_frames(say, round, false),
        };
        Reply::raw(200, &frames)
    }

    /// 被截断的一轮：有文字，并且有一个只到了一半的工具调用。
    fn truncated(self, text: &'static str) -> Reply {
        let say = Say {
            text: Some(text),
            calls: vec![(
                "call_cut",
                "write",
                json!({ "path": "a", "content": "一半" }),
            )],
            ..Say::default()
        };
        Reply::raw(
            200,
            &match self {
                P::Anthropic => anthropic_frames(&say, 0, true),
                P::Responses => responses_frames(&say, 0, true),
                P::Chat => chat_frames(&say, 0, true),
            },
        )
    }
}

fn anthropic_frames(say: &Say, round: u32, truncated: bool) -> Vec<String> {
    let mut frames = vec![event(
        "message_start",
        json!({ "message": { "usage": {
            "input_tokens": 10 + round, "cache_read_input_tokens": 100 * round,
            "cache_creation_input_tokens": 7, "output_tokens": 1 } } }),
    )];
    let mut index = 0;
    if let Some(thinking) = say.thinking {
        frames.push(event(
            "content_block_start",
            json!({ "index": index, "content_block": { "type": "thinking", "thinking": "" } }),
        ));
        frames.push(event(
            "content_block_delta",
            json!({ "index": index, "delta": { "type": "thinking_delta", "thinking": thinking } }),
        ));
        frames.push(event(
            "content_block_delta",
            json!({ "index": index, "delta": { "type": "signature_delta", "signature": format!("SIG-{round}") } }),
        ));
        frames.push(event("content_block_stop", json!({ "index": index })));
        index += 1;
    }
    if let Some(text) = say.text {
        frames.push(event(
            "content_block_start",
            json!({ "index": index, "content_block": { "type": "text", "text": "" } }),
        ));
        frames.push(event(
            "content_block_delta",
            json!({ "index": index, "delta": { "type": "text_delta", "text": text } }),
        ));
        frames.push(event("content_block_stop", json!({ "index": index })));
        index += 1;
    }
    for (id, name, arguments) in &say.calls {
        frames.push(event(
            "content_block_start",
            json!({ "index": index, "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }),
        ));
        let raw = arguments.to_string();
        let (head, tail) = raw.split_at(raw.len() / 2);
        let (head, tail) = if truncated { (head, "") } else { (head, tail) };
        for part in [head, tail].into_iter().filter(|part| !part.is_empty()) {
            frames.push(event(
                "content_block_delta",
                json!({ "index": index, "delta": { "type": "input_json_delta", "partial_json": part } }),
            ));
        }
        if !truncated {
            frames.push(event("content_block_stop", json!({ "index": index })));
        }
        index += 1;
    }
    let stop = if truncated {
        "max_tokens"
    } else if say.calls.is_empty() {
        "end_turn"
    } else {
        "tool_use"
    };
    frames.push(event(
        "message_delta",
        json!({ "delta": { "stop_reason": stop }, "usage": { "output_tokens": 5 + round } }),
    ));
    frames.push(event("message_stop", json!({})));
    frames
}

fn responses_frames(say: &Say, round: u32, truncated: bool) -> Vec<String> {
    let mut frames = Vec::new();
    let mut output = Vec::new();
    if let Some(thinking) = say.thinking {
        frames.push(event(
            "response.reasoning_summary_text.delta",
            json!({ "output_index": 0, "delta": thinking }),
        ));
        output.push(json!({
            "type": "reasoning", "id": format!("rs_{round}"),
            "summary": [{ "type": "summary_text", "text": thinking }],
            "encrypted_content": format!("ENC-{round}"),
        }));
    }
    if let Some(text) = say.text {
        frames.push(event(
            "response.output_text.delta",
            json!({ "output_index": output.len(), "delta": text }),
        ));
        output.push(json!({
            "type": "message", "id": format!("msg_{round}"), "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        }));
    }
    for (id, name, arguments) in &say.calls {
        let at = output.len();
        let raw = arguments.to_string();
        frames.push(event(
            "response.output_item.added",
            json!({ "output_index": at, "item": { "type": "function_call", "call_id": id, "name": name, "arguments": "" } }),
        ));
        frames.push(event(
            "response.function_call_arguments.done",
            json!({ "output_index": at, "arguments": raw }),
        ));
        output.push(json!({
            "type": "function_call", "id": format!("fc_{id}"), "call_id": id,
            "name": name, "arguments": raw,
        }));
    }
    let usage = json!({
        "input_tokens": 50 + round, "output_tokens": 5 + round,
        "input_tokens_details": { "cached_tokens": 32 },
        "output_tokens_details": { "reasoning_tokens": 2 },
    });
    if truncated {
        frames.push(event(
            "response.incomplete",
            json!({ "response": { "status": "incomplete",
                "incomplete_details": { "reason": "max_output_tokens" },
                "output": output, "usage": usage } }),
        ));
    } else {
        frames.push(event(
            "response.completed",
            json!({ "response": { "status": "completed", "output": output, "usage": usage } }),
        ));
    }
    frames
}

fn chat_frames(say: &Say, round: u32, truncated: bool) -> Vec<String> {
    let mut frames = Vec::new();
    if let Some(thinking) = say.thinking {
        frames.push(data(json!({ "choices": [{ "delta": {
            "reasoning_details": [{ "type": "reasoning.text", "text": thinking, "signature": format!("S{round}") }]
        }, "finish_reason": null }] })));
    }
    if let Some(text) = say.text {
        frames.push(data(
            json!({ "choices": [{ "delta": { "content": text }, "finish_reason": null }] }),
        ));
    }
    for (at, (id, name, arguments)) in say.calls.iter().enumerate() {
        let raw = arguments.to_string();
        let (head, tail) = raw.split_at(raw.len() / 2);
        let tail = if truncated { "" } else { tail };
        frames.push(data(json!({ "choices": [{ "delta": { "tool_calls": [{
            "index": at, "id": id, "function": { "name": name, "arguments": head } }] },
            "finish_reason": null }] })));
        if !tail.is_empty() {
            frames.push(data(json!({ "choices": [{ "delta": { "tool_calls": [{
                "index": at, "function": { "arguments": tail } }] }, "finish_reason": null }] })));
        }
    }
    let finish = if truncated {
        "length"
    } else if say.calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    frames.push(data(
        json!({ "choices": [{ "delta": {}, "finish_reason": finish }] }),
    ));
    frames.push(data(json!({ "choices": [], "usage": {
        "prompt_tokens": 40 + round, "completion_tokens": 4 + round,
        "completion_tokens_details": { "reasoning_tokens": 1 },
        "prompt_tokens_details": { "cached_tokens": 16 } } })));
    frames.push("data: [DONE]\n\n".to_string());
    frames
}

// ---------------------------------------------------------------- 场景与双跑

/// 一条工具结果。
#[derive(Clone)]
struct Res {
    id: &'static str,
    content: &'static str,
    is_error: bool,
    images: Vec<&'static [u8]>,
}

fn res(id: &'static str, content: &'static str) -> Res {
    Res {
        id,
        content,
        is_error: false,
        images: vec![],
    }
}

/// 一轮：服务端回复，以及这一轮之后（下一轮之前）交回去的结果与衰减。
#[derive(Clone)]
struct Step {
    reply: Reply,
    results: Vec<Res>,
    revised: Vec<Res>,
}

fn step(reply: Reply, results: Vec<Res>, revised: Vec<Res>) -> Step {
    Step {
        reply,
        results,
        revised,
    }
}

struct Case {
    p: P,
    effort: Option<&'static str>,
    system: &'static str,
    tools: bool,
    users: Vec<&'static str>,
    steps: Vec<Step>,
}

impl Case {
    fn new(p: P, steps: Vec<Step>) -> Self {
        Case {
            p,
            effort: None,
            system: "你是 komo",
            tools: true,
            users: vec!["看看 a.txt"],
            steps,
        }
    }
}

fn tool_defs(on: bool) -> Vec<ToolDefinition> {
    if !on {
        return vec![];
    }
    vec![
        ToolDefinition {
            name: "read".into(),
            description: "读文件".into(),
            parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            doc: None,
        },
        ToolDefinition {
            name: "bash".into(),
            description: "跑命令".into(),
            parameters: json!({ "type": "object" }),
            doc: None,
        },
    ]
}

fn factory(transport: &ScriptedTransport) -> LlmFactory {
    LlmFactory::new(
        Arc::new(Secrets::from_pairs([("KOMO_LLM_API_KEY", "sk-test")])),
        EffortCapabilities::builtin(),
    )
    .with_transport(Arc::new(transport.clone()))
}

/// 路径 → 字节。
#[derive(Default, Clone)]
struct MemImages(Arc<Mutex<HashMap<String, Vec<u8>>>>);

impl ImageResolver for MemImages {
    fn load(&self, image: &ImageRef) -> Result<Vec<u8>, ImageLoadError> {
        self.0
            .lock()
            .unwrap()
            .get(&image.path)
            .cloned()
            .ok_or_else(|| ImageLoadError {
                path: image.path.clone(),
                reason: "没有这个文件".into(),
            })
    }
}

fn image_block(path: &str, bytes: &[u8]) -> ContentBlock {
    let mut extra = Map::new();
    extra.insert("mime".into(), json!("image/png"));
    extra.insert("path".into(), json!(path));
    extra.insert("hash".into(), json!(ContentHash::of_bytes(bytes).as_str()));
    ContentBlock::Image { extra }
}

fn system_message(case: &Case) -> Message {
    Message::System(SystemMessage {
        content: Content::Text(case.system.into()),
        sections: None,
        tools_added: case.tools.then(|| {
            tool_defs(true)
                .into_iter()
                .map(|tool| {
                    json!({ "name": tool.name, "description": tool.description, "parameters": tool.parameters })
                })
                .collect()
        }),
        tools_removed: None,
        timestamp: 0,
        extra: Map::new(),
    })
}

fn tool_result_message(images: &MemImages, result: &Res) -> Message {
    let mut content = vec![ContentBlock::text(result.content)];
    for (n, bytes) in result.images.iter().enumerate() {
        let path = format!("img/{}-{n}.png", result.id);
        images
            .0
            .lock()
            .unwrap()
            .insert(path.clone(), bytes.to_vec());
        content.push(image_block(&path, bytes));
    }
    Message::ToolResult(ToolResultMessage {
        tool_call_id: result.id.into(),
        tool_name: "read".into(),
        content,
        is_error: result.is_error,
        details: None,
        timestamp: 0,
        extra: Map::new(),
    })
}

fn old_result(result: &Res) -> ToolResultForModel {
    ToolResultForModel {
        provider_call_id: result.id.into(),
        call_id: ToolCallId::from_raw(format!("tc-{}", result.id)),
        content: result.content.into(),
        is_error: result.is_error,
        images: result
            .images
            .iter()
            .map(|bytes| ToolImage {
                mime: "image/png".into(),
                bytes: bytes.to_vec(),
            })
            .collect(),
    }
}

fn replay_user(text: &str, at: u64) -> ReplayMessage {
    ReplayMessage {
        role: Role::User,
        seq: Seq(at),
        text: Some(text.into()),
        tool_calls: vec![],
        tool_results: vec![],
        provider_blocks: None,
    }
}

struct Outcome {
    old_bodies: Vec<Value>,
    new_bodies: Vec<Value>,
    old_rounds: Vec<Round>,
    new_messages: Vec<AssistantMessage>,
    old_requests: Vec<super::transport::HttpRequest>,
    new_requests: Vec<super::transport::HttpRequest>,
}

async fn run_both(case: &Case) -> Outcome {
    let config = case.p.config(case.effort);
    let replies: Vec<Reply> = case.steps.iter().map(|s| s.reply.clone()).collect();

    // ---- 旧：有状态 driver
    let old_transport = ScriptedTransport::new(replies.clone());
    let llm = factory(&old_transport)
        .build(&config, ModelRole::Main)
        .unwrap();
    let mut driver = llm
        .begin_turn(TurnRequest {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("r-1"),
            model: config.clone(),
            system_prompt: case.system.into(),
            messages: case
                .users
                .iter()
                .enumerate()
                .map(|(i, text)| replay_user(text, i as u64))
                .collect(),
            tools: tool_defs(case.tools),
            memories: vec![],
            covers: None,
            cache_session: None,
        })
        .await
        .unwrap();
    let mut old_rounds = Vec::new();
    let mut input = RoundInput::First;
    for step in &case.steps {
        old_rounds.push(driver.next(input).await.unwrap_or_else(|error| {
            panic!("旧 driver 失败：{error}");
        }));
        input = RoundInput::ToolResults {
            results: step.results.iter().map(old_result).collect(),
            revised: step.revised.iter().map(old_result).collect(),
        };
    }

    // ---- 新：无状态 stream，每轮给完整 context
    let new_transport = ScriptedTransport::new(replies);
    let stream = factory(&new_transport)
        .build_stream(&config, ModelRole::Main)
        .unwrap();
    let images = MemImages::default();
    let mut messages = vec![system_message(case)];
    messages.extend(case.users.iter().map(|text| Message::user(*text, 0)));
    let mut new_messages = Vec::new();
    let mut revised_from: Option<usize> = None;
    for step in &case.steps {
        let request = ModelRequest::new(&config, &messages).with_options(RequestOptions {
            images: Some(&images),
            cache_key: Some("s-1"),
            max_output_tokens: None,
            revised_from,
        });
        let assistant = stream
            .stream(&request, &mut |_| {})
            .await
            .unwrap_or_else(|error| panic!("新 stream 失败：{error}"));
        messages.push(Message::Assistant(assistant.clone()));
        new_messages.push(assistant);

        // 衰减：直接给换过视图的消息。
        revised_from = None;
        for revised in &step.revised {
            let found = messages.iter().position(
                |message| matches!(message, Message::ToolResult(r) if r.tool_call_id == revised.id),
            );
            if let Some(at) = found {
                if let Message::ToolResult(result) = &mut messages[at] {
                    if let Some(ContentBlock::Text { text, .. }) = result
                        .content
                        .iter_mut()
                        .find(|block| matches!(block, ContentBlock::Text { .. }))
                    {
                        *text = revised.content.into();
                    }
                }
                revised_from = Some(revised_from.map_or(at, |have: usize| have.min(at)));
            }
        }
        messages.extend(step.results.iter().map(|r| tool_result_message(&images, r)));
    }

    Outcome {
        old_bodies: old_transport.bodies(),
        new_bodies: new_transport.bodies(),
        old_rounds,
        new_messages,
        old_requests: old_transport.requests(),
        new_requests: new_transport.requests(),
    }
}

fn text_of(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn calls_of(message: &AssistantMessage) -> Vec<(String, String, Value)> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some((id.clone(), name.clone(), arguments.clone())),
            _ => None,
        })
        .collect()
}

fn assert_equivalent(case: &Case, outcome: &Outcome) {
    let label = format!("{:?} effort={:?}", case.p, case.effort);
    assert_eq!(outcome.old_bodies.len(), case.steps.len(), "{label}");
    assert_eq!(outcome.new_bodies.len(), case.steps.len(), "{label}");
    for (k, (old, new)) in outcome
        .old_bodies
        .iter()
        .zip(&outcome.new_bodies)
        .enumerate()
    {
        // 逐字节：序列化出来的串相同（reqwest 就是 `.json(body)`）。
        assert_eq!(
            serde_json::to_string(old).unwrap(),
            serde_json::to_string(new).unwrap(),
            "{label} 第 {k} 次请求体不是逐字节相同"
        );
    }
    // URL、鉴权、附加头也一样。
    for (old, new) in outcome.old_requests.iter().zip(&outcome.new_requests) {
        assert_eq!(old.url, new.url, "{label}");
        assert_eq!(old.api_key, new.api_key, "{label}");
        assert_eq!(old.headers, new.headers, "{label}");
    }
    for (k, (round, message)) in outcome
        .old_rounds
        .iter()
        .zip(&outcome.new_messages)
        .enumerate()
    {
        assert_same_parse(&label, k, round, message, case.p);
    }
}

fn assert_same_parse(label: &str, k: usize, round: &Round, message: &AssistantMessage, p: P) {
    assert_eq!(
        text_of(message),
        round.text.clone().unwrap_or_default(),
        "{label} 第 {k} 轮文本"
    );
    let calls: Vec<_> = round
        .tool_calls
        .iter()
        .map(|call| {
            (
                call.provider_call_id.clone(),
                call.name.clone(),
                call.arguments.clone(),
            )
        })
        .collect();
    assert_eq!(calls_of(message), calls, "{label} 第 {k} 轮调用");
    let expected_stop = if round.truncated {
        StopReason::Length
    } else if calls.is_empty() {
        StopReason::Stop
    } else {
        StopReason::ToolUse
    };
    assert_eq!(
        message.stop_reason, expected_stop,
        "{label} 第 {k} 轮 stopReason"
    );
    assert_eq!(usage_of(message), round.usage, "{label} 第 {k} 轮用量");
    assert_eq!(
        message.extra.get(PROVIDER_BLOCKS),
        round.provider_blocks.as_ref(),
        "{label} 第 {k} 轮原生块原样放进 extra"
    );
    assert_eq!(message.extra["api"], json!(p.api()));
    assert_eq!(message.extra["model"], json!("m-1"));
}

async fn check(case: Case) -> Outcome {
    let outcome = run_both(&case).await;
    assert_equivalent(&case, &outcome);
    outcome
}

// ---------------------------------------------------------------- 字节等价：每家 × 场景

/// 纯文本多轮：同一段 Run 里先调工具再答，之后第二个用户输入接着问（user 消息夹在 assistant 之间）。
#[tokio::test]
async fn plain_text_turns_are_byte_identical() {
    for p in ALL {
        let mut case = Case::new(
            p,
            vec![
                step(
                    p.reply(&say_call("call_1", "read", json!({ "path": "a.txt" })), 0),
                    vec![res("call_1", "文件内容")],
                    vec![],
                ),
                step(p.reply(&say_text("读到了"), 1), vec![], vec![]),
            ],
        );
        case.users = vec!["看看 a.txt", "顺便说一句：别改它"];
        check(case).await;
    }
}

#[tokio::test]
async fn tool_calls_and_results_are_byte_identical_including_parallel_calls() {
    for p in ALL {
        let two = Say {
            text: Some("两个一起读"),
            calls: vec![
                ("call_a", "read", json!({ "path": "a" })),
                ("call_b", "read", json!({ "path": "b" })),
            ],
            ..Say::default()
        };
        let mut failed = res("call_b", "找不到 b");
        failed.is_error = true;
        let case = Case::new(
            p,
            vec![
                step(p.reply(&two, 0), vec![res("call_a", "甲"), failed], vec![]),
                step(
                    p.reply(&say_call("call_c", "bash", json!({ "command": "ls" })), 1),
                    vec![res("call_c", "a b")],
                    vec![],
                ),
                step(p.reply(&say_text("完"), 2), vec![], vec![]),
            ],
        );
        check(case).await;
    }
}

#[tokio::test]
async fn thinking_and_reasoning_blocks_replay_byte_identically() {
    for p in ALL {
        let thought = Say {
            thinking: Some("先看看文件"),
            text: Some("我来读"),
            calls: vec![("call_1", "read", json!({ "path": "a.txt" }))],
        };
        let mut case = Case::new(
            p,
            vec![
                step(p.reply(&thought, 0), vec![res("call_1", "内容")], vec![]),
                step(
                    p.reply(
                        &Say {
                            thinking: Some("再想想"),
                            calls: vec![("call_2", "read", json!({ "path": "b.txt" }))],
                            ..Say::default()
                        },
                        1,
                    ),
                    vec![res("call_2", "内容二")],
                    vec![],
                ),
                step(p.reply(&say_text("好了"), 2), vec![], vec![]),
            ],
        );
        // effort 打开：Anthropic 带 thinking 预算，Responses / Chat 带 reasoning effort。
        case.effort = Some("high");
        let outcome = check(case).await;
        // 原生块真的在：第二次请求里带着第一轮的签名 / 加密推理。
        let second = outcome.new_bodies[1].to_string();
        let marker = match p {
            P::Anthropic => "SIG-0",
            P::Responses => "ENC-0",
            P::Chat => "reasoning_details",
        };
        assert!(second.contains(marker), "{p:?} 缺 {marker}：{second}");
    }
}

#[tokio::test]
async fn images_in_tool_results_are_byte_identical() {
    for p in ALL {
        let two = Say {
            calls: vec![
                ("call_a", "read", json!({ "path": "shot.png" })),
                ("call_b", "read", json!({ "path": "b" })),
            ],
            ..Say::default()
        };
        let mut shot = res("call_a", "shot.png 是一张图片");
        shot.images = vec![b"\x89PNG-one", b"\x89PNG-two"];
        let case = Case::new(
            p,
            vec![
                step(
                    p.reply(&two, 0),
                    vec![shot, res("call_b", "纯文字")],
                    vec![],
                ),
                step(p.reply(&say_text("看到了"), 1), vec![], vec![]),
            ],
        );
        let outcome = check(case).await;
        // 请求里确实带了图：base64（"\x89PNG-one" 的编码）出现在第二次请求里。
        assert!(
            outcome.new_bodies[1].to_string().contains("iVBORy1vbmU="),
            "{p:?} 没带图：{}",
            outcome.new_bodies[1]
        );
    }
}

/// 衰减：更早的工具结果换成短视图（旧 driver 的 `revised`，新入口直接给换过的消息）。
#[tokio::test]
async fn decayed_tool_results_are_byte_identical() {
    for p in ALL {
        let mut shot = res("call_2", "图片说明");
        shot.images = vec![b"\x89PNG-keep"];
        let case = Case::new(
            p,
            vec![
                step(
                    p.reply(&say_call("call_1", "read", json!({ "path": "big" })), 0),
                    vec![res("call_1", "很长很长的完整输出")],
                    vec![],
                ),
                step(
                    p.reply(&say_call("call_2", "read", json!({ "path": "img" })), 1),
                    vec![shot],
                    vec![],
                ),
                step(
                    p.reply(&say_call("call_3", "read", json!({ "path": "c" })), 2),
                    vec![res("call_3", "三")],
                    // 衰减同时换掉纯文字那条和带图片那条的正文；图片留着。
                    vec![res("call_1", "[已折叠]"), res("call_2", "[图片说明已折叠]")],
                ),
                step(
                    p.reply(&say_call("call_4", "read", json!({ "path": "d" })), 3),
                    vec![res("call_4", "四")],
                    // 下一轮没有衰减：之前换过的视图一直在。
                    vec![],
                ),
                step(p.reply(&say_text("完"), 4), vec![], vec![]),
            ],
        );
        let outcome = check(case).await;
        let late = outcome.new_bodies[3].to_string();
        assert!(
            late.contains("[已折叠]") && !late.contains("很长很长"),
            "{p:?}"
        );
        assert!(late.contains("[图片说明已折叠]"), "{p:?}");
    }
}

#[tokio::test]
async fn an_unset_effort_and_an_empty_toolset_are_byte_identical() {
    for p in ALL {
        let mut case = Case::new(p, vec![step(p.reply(&say_text("好"), 0), vec![], vec![])]);
        case.tools = false;
        case.system = "";
        case.effort = None;
        check(case).await;
    }
}

/// 截断的一轮：`stopReason = length`、不含任何调用，与旧 driver 的 `Round.truncated` 一致，
/// 半截的调用也不在原生块里。
#[tokio::test]
async fn a_truncated_round_builds_no_calls_and_replays_identically() {
    for p in ALL {
        let case = Case::new(p, vec![step(p.truncated("写到一半"), vec![], vec![])]);
        let outcome = check(case).await;
        let first = &outcome.new_messages[0];
        assert_eq!(first.stop_reason, StopReason::Length, "{p:?}");
        assert!(calls_of(first).is_empty(), "{p:?}");
        assert!(outcome.old_rounds[0].truncated, "{p:?}");
    }
}

/// 第二个 Run：历史里已有 assistant 消息（带原生块）。旧路是 `begin_turn(messages)`，
/// 新路是消息序列里的 assistant 消息。
#[tokio::test]
async fn a_replayed_history_window_is_byte_identical() {
    for p in ALL {
        let config = p.config(Some("high"));
        let say = Say {
            thinking: Some("想"),
            text: Some("答一"),
            calls: vec![("call_1", "read", json!({ "path": "a" }))],
        };
        // 先用无状态入口造出一条真实的 assistant 消息（含原生块），再让旧 driver 回放它。
        let seed = ScriptedTransport::new(vec![p.reply(&say, 0)]);
        let stream = factory(&seed)
            .build_stream(&config, ModelRole::Main)
            .unwrap();
        let first_user = Message::user("第一问", 0);
        let assistant = stream
            .stream(
                &ModelRequest::new(&config, std::slice::from_ref(&first_user)),
                &mut |_| {},
            )
            .await
            .unwrap();
        let native = assistant.extra.get(PROVIDER_BLOCKS).cloned();

        let old_transport = ScriptedTransport::new(vec![p.reply(&say_text("答二"), 1)]);
        let llm = factory(&old_transport)
            .build(&config, ModelRole::Main)
            .unwrap();
        let mut driver = llm
            .begin_turn(TurnRequest {
                session: SessionId::from_raw("s-1"),
                run: RunId::from_raw("r-2"),
                model: config.clone(),
                system_prompt: "你是 komo".into(),
                messages: vec![
                    replay_user("第一问", 1),
                    ReplayMessage {
                        role: Role::Assistant,
                        seq: Seq(2),
                        text: Some("答一".into()),
                        tool_calls: vec![ToolCallRequest {
                            call_id: ToolCallId::from_raw("tc-1"),
                            provider_call_id: "call_1".into(),
                            name: "read".into(),
                            arguments: json!({ "path": "a" }),
                            arguments_ref: None,
                        }],
                        tool_results: vec![],
                        provider_blocks: native,
                    },
                    ReplayMessage {
                        role: Role::Tool,
                        seq: Seq(3),
                        text: None,
                        tool_calls: vec![],
                        tool_results: vec![old_result(&res("call_1", "甲"))],
                        provider_blocks: None,
                    },
                    replay_user("第二问", 4),
                ],
                tools: tool_defs(true),
                memories: vec![],
                covers: None,
                cache_session: None,
            })
            .await
            .unwrap();
        driver.next(RoundInput::First).await.unwrap();

        let new_transport = ScriptedTransport::new(vec![p.reply(&say_text("答二"), 1)]);
        let stream = factory(&new_transport)
            .build_stream(&config, ModelRole::Main)
            .unwrap();
        let images = MemImages::default();
        let messages = vec![
            system_message(&Case::new(p, vec![])),
            first_user,
            Message::Assistant(assistant),
            tool_result_message(&images, &res("call_1", "甲")),
            Message::user("第二问", 0),
        ];
        stream
            .stream(
                &ModelRequest::new(&config, &messages).with_options(RequestOptions {
                    cache_key: Some("s-1"),
                    ..RequestOptions::default()
                }),
                &mut |_| {},
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_string(&old_transport.bodies()[0]).unwrap(),
            serde_json::to_string(&new_transport.bodies()[0]).unwrap(),
            "{p:?}"
        );
    }
}

/// 没有原生块的 assistant 消息（旧记录、跨协议）：从 content 的文字与调用重建，和旧 driver
/// 对 `provider_blocks: None` 的回放逐字节相同；`api` 标着别家的原生块一律不带过来。
#[tokio::test]
async fn an_assistant_without_native_blocks_is_rebuilt_like_the_old_replay() {
    for p in ALL {
        let config = p.config(None);
        let content = vec![
            ContentBlock::text("我读一下"),
            ContentBlock::Thinking {
                thinking: "不进请求".into(),
                extra: Map::new(),
            },
            ContentBlock::tool_call("call_1", "read", json!({ "path": "a" })),
        ];
        let foreign = |api: &str, blocks: Value| {
            let mut message = AssistantMessage {
                content: content.clone(),
                stop_reason: StopReason::ToolUse,
                timestamp: 0,
                extra: Map::new(),
            };
            message.extra.insert("api".into(), json!(api));
            message.extra.insert(PROVIDER_BLOCKS.into(), blocks);
            Message::Assistant(message)
        };
        let bare = Message::assistant(content.clone(), StopReason::ToolUse, 0);
        // 别家的原生块：形状对得上也不能用。
        let other_api = match p {
            P::Anthropic => API_RESPONSES,
            _ => API_ANTHROPIC,
        };
        let from_other = foreign(
            other_api,
            json!([{ "type": "reasoning", "encrypted_content": "X" }]),
        );

        let mut bodies = Vec::new();
        for assistant in [bare, from_other] {
            let transport = ScriptedTransport::new(vec![p.reply(&say_text("好"), 1)]);
            let stream = factory(&transport)
                .build_stream(&config, ModelRole::Main)
                .unwrap();
            let images = MemImages::default();
            let messages = vec![
                system_message(&Case::new(p, vec![])),
                Message::user("看看 a", 0),
                assistant,
                tool_result_message(&images, &res("call_1", "甲")),
            ];
            stream
                .stream(
                    &ModelRequest::new(&config, &messages).with_options(RequestOptions {
                        cache_key: Some("s-1"),
                        ..RequestOptions::default()
                    }),
                    &mut |_| {},
                )
                .await
                .unwrap();
            bodies.push(transport.bodies()[0].clone());
        }
        assert_eq!(bodies[0], bodies[1], "{p:?}：别家的块不能进请求");
        assert!(!bodies[1].to_string().contains("不进请求"));

        let old_transport = ScriptedTransport::new(vec![p.reply(&say_text("好"), 1)]);
        let llm = factory(&old_transport)
            .build(&config, ModelRole::Main)
            .unwrap();
        let mut driver = llm
            .begin_turn(TurnRequest {
                session: SessionId::from_raw("s-1"),
                run: RunId::from_raw("r"),
                model: config.clone(),
                system_prompt: "你是 komo".into(),
                messages: vec![
                    replay_user("看看 a", 0),
                    ReplayMessage {
                        role: Role::Assistant,
                        seq: Seq(1),
                        text: Some("我读一下".into()),
                        tool_calls: vec![ToolCallRequest {
                            call_id: ToolCallId::from_raw("tc-1"),
                            provider_call_id: "call_1".into(),
                            name: "read".into(),
                            arguments: json!({ "path": "a" }),
                            arguments_ref: None,
                        }],
                        tool_results: vec![],
                        provider_blocks: None,
                    },
                    ReplayMessage {
                        role: Role::Tool,
                        seq: Seq(2),
                        text: None,
                        tool_calls: vec![],
                        tool_results: vec![old_result(&res("call_1", "甲"))],
                        provider_blocks: None,
                    },
                ],
                tools: tool_defs(true),
                memories: vec![],
                covers: None,
                cache_session: None,
            })
            .await
            .unwrap();
        driver.next(RoundInput::First).await.unwrap();
        let old = old_transport.bodies()[0].clone();
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            serde_json::to_string(&bodies[0]).unwrap(),
            "{p:?}"
        );
    }
}

// ---------------------------------------------------------------- 其余行为

/// Anthropic 的缓存断点：衰减那一轮补的第二个断点由 `revised_from` 决定，不给就没有。
#[tokio::test]
async fn anthropic_revised_from_adds_the_extra_cache_breakpoint_only_when_given() {
    let p = P::Anthropic;
    let case = || {
        Case::new(
            p,
            vec![
                step(
                    p.reply(&say_call("call_1", "read", json!({})), 0),
                    vec![res("call_1", "长")],
                    vec![],
                ),
                step(
                    p.reply(&say_call("call_2", "read", json!({})), 1),
                    vec![res("call_2", "二")],
                    vec![res("call_1", "短")],
                ),
                step(p.reply(&say_text("完"), 2), vec![], vec![]),
            ],
        )
    };
    let outcome = check(case()).await;
    let count = |body: &Value| {
        body["messages"]
            .to_string()
            .matches("cache_control")
            .count()
    };
    assert_eq!(count(&outcome.new_bodies[1]), 1, "没衰减：只有末尾断点");
    assert_eq!(
        count(&outcome.new_bodies[2]),
        2,
        "衰减那轮：末尾 + 衰减前最后一个 user"
    );
}

#[tokio::test]
async fn progress_sees_partial_messages_and_the_final_one_is_only_returned() {
    for p in ALL {
        let config = p.config(None);
        let say = Say {
            thinking: Some("想"),
            text: Some("你好世界"),
            calls: vec![("call_1", "read", json!({ "path": "a.txt" }))],
        };
        let transport = ScriptedTransport::new(vec![p.reply(&say, 0)]);
        let stream = factory(&transport)
            .build_stream(&config, ModelRole::Main)
            .unwrap();
        let messages = vec![Message::user("hi", 0)];
        let mut seen: Vec<AssistantMessage> = Vec::new();
        let done = stream
            .stream(&ModelRequest::new(&config, &messages), &mut |partial| {
                seen.push(partial.clone())
            })
            .await
            .unwrap();
        assert!(!seen.is_empty(), "{p:?}");
        assert!(
            seen.iter().all(|m| !m.extra.contains_key(PROVIDER_BLOCKS)),
            "部分消息没有原生块"
        );
        assert!(seen.iter().any(|m| text_of(m).contains("你好")), "{p:?}");
        assert_eq!(calls_of(&done).len(), 1);
        assert!(done.extra.contains_key(PROVIDER_BLOCKS));
    }
}

#[tokio::test]
async fn failures_keep_their_classification() {
    for p in ALL {
        let config = p.config(None);
        let messages = vec![Message::user("hi", 0)];
        // 400：原样报上去，且只发一次请求（不会偷偷删参重试）。
        let transport = ScriptedTransport::new(vec![Reply::json(
            400,
            r#"{"error":{"message":"bad","code":"invalid_request_error"}}"#,
        )]);
        let stream = factory(&transport)
            .build_stream(&config, ModelRole::Main)
            .unwrap();
        let error = stream
            .stream(&ModelRequest::new(&config, &messages), &mut |_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(error, LlmError::Rejected { status: 400, .. }),
            "{p:?}"
        );
        assert_eq!(transport.requests().len(), 1);

        // 没有终止帧：可重试的 Incomplete，不是一个短回答。
        let cut = match p {
            P::Anthropic => event(
                "content_block_start",
                json!({ "index": 0, "content_block": { "type": "text", "text": "" } }),
            ),
            P::Responses => event("response.output_text.delta", json!({ "delta": "半" })),
            P::Chat => data(json!({ "choices": [{ "delta": { "content": "半" } }] })),
        };
        let transport = ScriptedTransport::new(vec![Reply::raw(200, &[cut])]);
        let stream = factory(&transport)
            .build_stream(&config, ModelRole::Main)
            .unwrap();
        let error = stream
            .stream(&ModelRequest::new(&config, &messages), &mut |_| {})
            .await
            .unwrap_err();
        assert_eq!(error, LlmError::Incomplete, "{p:?}");
    }
}

#[tokio::test]
async fn an_unsupported_effort_is_refused_before_anything_is_sent() {
    for p in ALL {
        let config = p.config(Some("warp-9"));
        // 构造阶段就拒了（同旧入口）。
        let transport = ScriptedTransport::new(vec![]);
        let built = factory(&transport).build_stream(&config, ModelRole::Main);
        assert!(
            matches!(
                built,
                Err(LlmBuildError::Model(LlmError::UnsupportedEffort { .. }))
            ),
            "{p:?}"
        );
    }
}

/// 一次请求的 effort 是请求里那份配置的，不是造客户端时那份（热重载 / `prepareRequest` 换档）。
#[tokio::test]
async fn effort_comes_from_the_request_not_the_client() {
    let config = P::Responses.config(None);
    let transport = ScriptedTransport::new(vec![
        P::Responses.reply(&say_text("好"), 0),
        P::Responses.reply(&say_text("好"), 0),
    ]);
    let stream = factory(&transport)
        .build_stream(&config, ModelRole::Main)
        .unwrap();
    let messages = vec![Message::user("hi", 0)];
    let high = P::Responses.config(Some("high"));
    stream
        .stream(&ModelRequest::new(&high, &messages), &mut |_| {})
        .await
        .unwrap();
    let bad = P::Responses.config(Some("warp-9"));
    let error = stream
        .stream(&ModelRequest::new(&bad, &messages), &mut |_| {})
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::UnsupportedEffort { .. }));
    assert_eq!(transport.bodies().len(), 1);
    assert_eq!(
        transport.bodies()[0]["reasoning"],
        json!({ "effort": "high" })
    );
}

async fn body_of(
    p: P,
    config: ModelConfig,
    messages: Vec<Message>,
    images: Option<&dyn ImageResolver>,
) -> String {
    let transport = ScriptedTransport::new(vec![p.reply(&say_text("好"), 0)]);
    let stream = factory(&transport)
        .build_stream(&config, ModelRole::Main)
        .unwrap();
    stream
        .stream(
            &ModelRequest::new(&config, &messages).with_options(RequestOptions {
                images,
                ..RequestOptions::default()
            }),
            &mut |_| {},
        )
        .await
        .unwrap();
    transport.bodies()[0].to_string()
}

/// 图片读不回（没有这个文件、哈希对不上、没给 resolver）：少附一张，正文照旧。
#[tokio::test]
async fn an_unreadable_image_is_left_out_and_the_text_stays() {
    for p in ALL {
        let config = p.config(None);
        let call = Message::assistant(
            vec![ContentBlock::tool_call("call_1", "read", json!({}))],
            StopReason::ToolUse,
            0,
        );
        let result = |blocks: Vec<ContentBlock>| {
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call_1".into(),
                tool_name: "read".into(),
                content: blocks,
                is_error: false,
                details: None,
                timestamp: 0,
                extra: Map::new(),
            })
        };
        let body_with = |blocks: Vec<ContentBlock>, images: Option<&'static dyn ImageResolver>| {
            let messages = vec![Message::user("hi", 0), call.clone(), result(blocks)];
            body_of(p, config.clone(), messages, images)
        };
        let plain = body_with(vec![ContentBlock::text("文字")], None).await;

        // 读不回。
        let images: &'static MemImages = Box::leak(Box::default());
        let missing = body_with(
            vec![
                ContentBlock::text("文字"),
                image_block("img/none.png", b"x"),
            ],
            Some(images),
        )
        .await;
        assert_eq!(missing, plain, "{p:?} 缺文件");

        // 哈希对不上。
        images
            .0
            .lock()
            .unwrap()
            .insert("img/bad.png".into(), b"other".to_vec());
        let bad_hash = body_with(
            vec![
                ContentBlock::text("文字"),
                image_block("img/bad.png", b"expected"),
            ],
            Some(images),
        )
        .await;
        assert_eq!(bad_hash, plain, "{p:?} 哈希错");

        // 没给 resolver。
        let unresolved = body_with(
            vec![
                ContentBlock::text("文字"),
                image_block("img/bad.png", b"other"),
            ],
            None,
        )
        .await;
        assert_eq!(unresolved, plain, "{p:?} 无 resolver");

        // 读得回：带图，和上面都不同。
        let ok = body_with(
            vec![
                ContentBlock::text("文字"),
                image_block("img/bad.png", b"other"),
            ],
            Some(images),
        )
        .await;
        assert_ne!(ok, plain, "{p:?}");
    }
}

/// user 消息里的图片块：三家各自的表达。
#[tokio::test]
async fn user_images_use_each_providers_own_shape() {
    let images = MemImages::default();
    images
        .0
        .lock()
        .unwrap()
        .insert("img/u.png".into(), b"png".to_vec());
    let user = Message::User(komo_kernel::durable::UserMessage {
        content: Content::Blocks(vec![
            ContentBlock::text("这张图"),
            image_block("img/u.png", b"png"),
        ]),
        timestamp: 0,
        extra: Map::new(),
    });
    let url = "data:image/png;base64,cG5n";
    for p in ALL {
        let config = p.config(None);
        let transport = ScriptedTransport::new(vec![p.reply(&say_text("好"), 0)]);
        let stream = factory(&transport)
            .build_stream(&config, ModelRole::Main)
            .unwrap();
        let messages = vec![user.clone()];
        stream
            .stream(
                &ModelRequest::new(&config, &messages).with_options(RequestOptions {
                    images: Some(&images),
                    ..RequestOptions::default()
                }),
                &mut |_| {},
            )
            .await
            .unwrap();
        let body = &transport.bodies()[0];
        match p {
            P::Anthropic => {
                let content = &body["messages"][0]["content"];
                assert_eq!(content[0]["text"], json!("这张图"));
                assert_eq!(content[1]["source"]["data"], json!("cG5n"));
                assert_eq!(content[1]["source"]["media_type"], json!("image/png"));
            }
            P::Responses => {
                let content = &body["input"][0]["content"];
                assert_eq!(
                    content[0],
                    json!({ "type": "input_text", "text": "这张图" })
                );
                assert_eq!(
                    content[1],
                    json!({ "type": "input_image", "image_url": url })
                );
            }
            P::Chat => {
                let content = &body["messages"][1]["content"];
                assert_eq!(content[0], json!({ "type": "text", "text": "这张图" }));
                assert_eq!(content[1]["image_url"]["url"], json!(url));
            }
        }
    }
}

// ---------------------------------------------------------------- system 回放

fn system(sections: Option<Sections>, content: &str) -> Message {
    Message::System(SystemMessage {
        content: Content::Text(content.into()),
        sections,
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
        extra: Map::new(),
    })
}

#[test]
fn system_messages_replay_into_one_prompt_and_toolset_like_pi() {
    let tool = |name: &str, description: &str| json!({ "name": name, "description": description, "parameters": { "type": "object" } });
    let mut first = system(
        Some(
            Sections::default()
                .set("env", "<env>a</env>")
                .set("mem", "<mem>m</mem>"),
        ),
        "基础提示",
    );
    if let Message::System(message) = &mut first {
        message.tools_added = Some(vec![tool("read", "读"), tool("bash", "跑")]);
    }
    let mut later = system(
        Some(
            Sections::default()
                .set("env", "<env>b</env>")
                .remove("mem")
                .set("new", "<new/>"),
        ),
        "追加说明",
    );
    if let Message::System(message) = &mut later {
        message.tools_removed = Some(vec![json!({ "name": "bash" })]);
        message.tools_added = Some(vec![tool("read", "读（新）"), tool("rg", "搜")]);
    }
    let state = replay_system(&[first, Message::user("hi", 0), later]);
    // content 按序连起来，然后是各段：env 原位替换，mem 删了，new 排在最后。
    assert_eq!(
        state.prompt,
        "基础提示\n\n追加说明\n\n<env>b</env>\n\n<new/>"
    );
    let tools: Vec<_> = state
        .tools
        .iter()
        .map(|t| (t.name.as_str(), t.description.as_str()))
        .collect();
    // read 被同名新定义原位替换，bash 删了，rg 在后。
    assert_eq!(tools, [("read", "读（新）"), ("rg", "搜")]);

    let empty = replay_system(&[Message::user("hi", 0)]);
    assert_eq!(empty.prompt, "");
    assert!(empty.tools.is_empty());
}

#[tokio::test]
async fn the_replayed_prompt_and_tools_are_what_the_request_carries() {
    let mut first = system(Some(Sections::default().set("a", "段 A")), "头");
    if let Message::System(message) = &mut first {
        message.tools_added = Some(vec![
            json!({ "name": "read", "description": "读", "parameters": { "type": "object" } }),
        ]);
    }
    let later = system(Some(Sections::default().set("a", "段 A2")), "");
    let messages = vec![first, Message::user("hi", 0), later];
    for p in ALL {
        let config = p.config(None);
        let transport = ScriptedTransport::new(vec![p.reply(&say_text("好"), 0)]);
        let stream = factory(&transport)
            .build_stream(&config, ModelRole::Main)
            .unwrap();
        stream
            .stream(&ModelRequest::new(&config, &messages), &mut |_| {})
            .await
            .unwrap();
        let body = &transport.bodies()[0];
        let prompt = match p {
            P::Anthropic => body["system"][0]["text"].clone(),
            P::Responses => body["instructions"].clone(),
            P::Chat => body["messages"][0]["content"].clone(),
        };
        assert_eq!(prompt, json!("头\n\n段 A2"), "{p:?}");
        let tool_name = match p {
            P::Anthropic | P::Responses => &body["tools"][0]["name"],
            P::Chat => &body["tools"][0]["function"]["name"],
        };
        assert_eq!(tool_name, &json!("read"), "{p:?}");
        // system 消息不会被当成对话消息发出去。
        let count = match p {
            P::Anthropic => body["messages"].as_array().unwrap().len(),
            P::Responses => body["input"].as_array().unwrap().len(),
            P::Chat => body["messages"].as_array().unwrap().len() - 1,
        };
        assert_eq!(count, 1, "{p:?}");
    }
}

#[test]
fn usage_round_trips_through_the_message_and_unknown_stays_unknown() {
    let usage = komo_kernel::types::model::TokenUsage {
        input: Some(10),
        output: None,
        reasoning: Some(0),
        cache_read: Some(4),
        cache_write: None,
    };
    let mut message = AssistantMessage {
        content: vec![],
        stop_reason: StopReason::Stop,
        timestamp: 0,
        extra: Map::new(),
    };
    message.extra.insert("usage".into(), usage_json(&usage));
    assert_eq!(usage_of(&message), usage);
    assert!(
        message.extra["usage"].get("output").is_none(),
        "未知不写成 0"
    );
}
