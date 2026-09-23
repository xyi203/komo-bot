//! Anthropic **Messages API** 的 [`LlmClient`]（流式 SSE + 工具调用 + thinking）。
//!
//! 四条来自协议的硬性质，各在下面有一段代码和一个测试：
//!
//! - **`max_tokens` 必填**，而且开着 thinking 时它必须大于 `budget_tokens`——effort 与
//!   `max_tokens` 因此绑成一张表（[`thinking_budget`]，§13.3）。
//! - **回放靠自己携带的完整 content 块数组**：assistant 这一轮的全部块（含
//!   `thinking` / `redacted_thinking`）原样进 [`Round::provider_blocks`]，下一轮原样放回
//!   那条 assistant 消息（同 Responses 的做法，§13.2）。没有存过块的历史（跨协议来的、
//!   或 §8.3 只发布用户正文与最终答复的历史 Run）从文字与调用记录重建，绝不把别的协议
//!   的私有块发过来。
//! - **API 要求 user / assistant 交替**：连续同角色消息合并成一条（[`append_message`]），
//!   同一轮的多个工具结果并进同一条 user 消息。
//! - **提示缓存断点是请求级的标记，不进历史**：只在发出去的那份请求体上打
//!   `cache_control`，从不写回 `self.messages`（[`messages_with_cache`]）。

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

use super::sse::SseDecoder;
use super::transport::{HttpRequest, HttpTransport, TransportError};
use super::wire;
use crate::config::EffortCapabilities;

const ANTHROPIC_VERSION_HEADER: &str = "anthropic-version";
const ANTHROPIC_VERSION: &str = "2023-06-01";

const MESSAGE_START: &str = "message_start";
const CONTENT_BLOCK_START: &str = "content_block_start";
const CONTENT_BLOCK_DELTA: &str = "content_block_delta";
const CONTENT_BLOCK_STOP: &str = "content_block_stop";
const MESSAGE_DELTA: &str = "message_delta";
const MESSAGE_STOP: &str = "message_stop";
const PING: &str = "ping";
const ERROR_EVENT: &str = "error";

const STOP_MAX_TOKENS: &str = "max_tokens";

/// 未开 thinking 时的输出预算（未配置 effort、或显式 `none`）。要装得下 `write` 一整份
/// 文件的工具参数——截在半截的调用只能丢掉；按实际输出计费，给大不多花钱。
const DEFAULT_MAX_TOKENS: u32 = 32_000;
/// thinking 预算之外再留给最终答案与工具调用 JSON 的余量——`max_tokens` 必须大于
/// `budget_tokens`（协议硬性要求），这段余量就是那道算式的另一半。`max` 档合计 64000，
/// 是当前几代 Claude 的输出上限。
const RESPONSE_HEADROOM: u32 = 32_000;

/// effort → thinking 预算（§13.3）。四档之外的取值（含 `none`）不开 thinking；这张表
/// 与 [`EffortCapabilities::builtin`] 给 `anthropic_messages` 声明的档位逐一对应——校验
/// 已经在请求前把没声明过的档位拦下，这里只管把立得住的那几档换算成 `budget_tokens`。
fn thinking_budget(effort: &Effort) -> Option<u32> {
    match effort.as_str() {
        Effort::LOW => Some(4096),
        Effort::MEDIUM => Some(10_000),
        Effort::HIGH => Some(24_000),
        Effort::MAX => Some(32_000),
        _ => None,
    }
}

/// 一个讲 Anthropic Messages API 的后端。
pub struct AnthropicMessagesLlm {
    config: ModelConfig,
    role: ModelRole,
    api_key: Option<String>,
    transport: Arc<dyn HttpTransport>,
    caps: EffortCapabilities,
}

impl std::fmt::Debug for AnthropicMessagesLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 凭证不进 Debug。
        f.debug_struct("AnthropicMessagesLlm")
            .field("role", &self.role)
            .field("model", &self.config.model)
            .field("base_url", &self.config.base_url)
            .field("has_key", &self.api_key.is_some())
            .finish()
    }
}

impl AnthropicMessagesLlm {
    pub fn new(
        config: ModelConfig,
        role: ModelRole,
        api_key: Option<String>,
        transport: Arc<dyn HttpTransport>,
        caps: EffortCapabilities,
    ) -> Result<Self, LlmError> {
        check_effort(&config, &caps)?;
        Ok(AnthropicMessagesLlm {
            config,
            role,
            api_key,
            transport,
            caps,
        })
    }

    fn endpoint(&self, model: &ModelConfig) -> String {
        format!("{}/messages", model.base_url.trim_end_matches('/'))
    }
}

fn check_effort(config: &ModelConfig, caps: &EffortCapabilities) -> Result<(), LlmError> {
    caps.check(config).map_err(|_| LlmError::UnsupportedEffort {
        model: config.model.clone(),
        effort: config
            .effort
            .as_ref()
            .map(Effort::to_string)
            .unwrap_or_default(),
    })
}

#[async_trait]
impl LlmClient for AnthropicMessagesLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        // 本次 Run 固定的是 `req.model`（§6），不是构造这个客户端时那份。
        check_effort(&req.model, &self.caps)?;

        // 记忆段已经在 `req.system_prompt` 里（Gateway 装配时拼好，§13.2）。
        let system = req.system_prompt.clone();

        Ok(Box::new(AnthropicDriver {
            endpoint: self.endpoint(&req.model),
            api_key: self.api_key.clone(),
            transport: Arc::clone(&self.transport),
            role: self.role,
            model: req.model,
            system,
            tools: req.tools,
            messages: replay(&req.messages),
            usage: TokenUsage::default(),
            round: 0,
        }))
    }
}

// ---------------------------------------------------------------- 回放

/// 回放窗口 → `messages`。assistant 那一轮**优先用存下来的原始 content 块**；没有时从
/// 规范化的正文与调用记录重建——跨协议切模型时别家的私有块（`thinking` 的签名一类）不
/// 会被带过来。相邻同角色消息按协议要求合并（§13.2）。
fn replay(messages: &[ReplayMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        let built = match message.role {
            Role::User => user_message(message.text.as_deref().unwrap_or_default()),
            Role::Assistant => assistant_message(message),
            Role::Tool => tool_results_message(&message.tool_results),
        };
        append_message(&mut out, built);
    }
    out
}

fn user_message(text: &str) -> Value {
    json!({ "role": "user", "content": [{ "type": "text", "text": text }] })
}

fn assistant_message(message: &ReplayMessage) -> Value {
    match &message.provider_blocks {
        // 一轮的全部 content 块，原样。
        Some(Value::Array(blocks)) => json!({ "role": "assistant", "content": blocks }),
        // 没有存过块的历史：从文字与调用记录重建，不冒充有 thinking。
        Some(_) | None => {
            let mut content = Vec::new();
            if let Some(text) = message.text.as_deref().filter(|t| !t.is_empty()) {
                content.push(json!({ "type": "text", "text": text }));
            }
            for call in &message.tool_calls {
                content.push(json!({
                    "type": "tool_use",
                    "id": call.provider_call_id,
                    "name": call.name,
                    "input": call.arguments,
                }));
            }
            json!({ "role": "assistant", "content": content })
        }
    }
}

fn tool_results_message(results: &[ToolResultForModel]) -> Value {
    json!({
        "role": "user",
        "content": results.iter().map(tool_result_block).collect::<Vec<_>>(),
    })
}

fn tool_result_block(result: &ToolResultForModel) -> Value {
    let mut block = json!({
        "type": "tool_result",
        "tool_use_id": result.provider_call_id,
        "content": result.content,
    });
    if result.is_error {
        block["is_error"] = json!(true);
    }
    block
}

/// 往消息列表里追加一条：跟上一条同角色就并进它的 `content`（协议要求交替，§13.2）——
/// 同一轮的多个工具结果、以及两个历史 Run 之间缺了中间那句答复时留下的相邻 user 消息，
/// 都靠它合并。
fn append_message(list: &mut Vec<Value>, message: Value) {
    let same_role = list
        .last()
        .is_some_and(|last| last["role"] == message["role"]);
    if same_role {
        let more = message["content"].as_array().cloned().unwrap_or_default();
        list.last_mut().expect("刚判断过非空")["content"]
            .as_array_mut()
            .expect("这里的消息 content 总是数组")
            .extend(more);
    } else {
        list.push(message);
    }
}

// ---------------------------------------------------------------- 请求体

fn request_body(
    config: &ModelConfig,
    system: &str,
    messages: &[Value],
    tools: &[ToolDefinition],
    thinking_tokens: Option<u32>,
    max_tokens: u32,
) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(config.model));
    body.insert("max_tokens".into(), json!(max_tokens));
    body.insert(
        "system".into(),
        json!([{
            "type": "text",
            "text": system,
            "cache_control": { "type": "ephemeral" },
        }]),
    );
    body.insert(
        "messages".into(),
        Value::Array(messages_with_cache(messages)),
    );
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools_with_cache(tools)));
    }
    body.insert("stream".into(), json!(true));
    // §13.3：未配置 / 显式 `none` = **不发送这个字段**。开着 thinking 时 API 不允许自定义
    // temperature——我们本来就不发。
    if let Some(budget_tokens) = thinking_tokens {
        body.insert(
            "thinking".into(),
            json!({ "type": "enabled", "budget_tokens": budget_tokens }),
        );
    }
    Value::Object(body)
}

fn tools_with_cache(tools: &[ToolDefinition]) -> Vec<Value> {
    let last = tools.len().saturating_sub(1);
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let mut value = json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            });
            if index == last {
                value["cache_control"] = json!({ "type": "ephemeral" });
            }
            value
        })
        .collect()
}

/// 给**这一次要发**的 messages 打上最后一条 user 消息末块的缓存断点——操作的是副本，
/// `self.messages` 自己不带这个标记，回放时才不会把它当成内容的一部分存下去。
fn messages_with_cache(messages: &[Value]) -> Vec<Value> {
    let mut out = messages.to_vec();
    if let Some(last) = out.last_mut()
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
        && let Some(block) = content.last_mut()
    {
        block["cache_control"] = json!({ "type": "ephemeral" });
    }
    out
}

// ---------------------------------------------------------------- driver

struct AnthropicDriver {
    endpoint: String,
    api_key: Option<String>,
    transport: Arc<dyn HttpTransport>,
    role: ModelRole,
    model: ModelConfig,
    system: String,
    tools: Vec<ToolDefinition>,
    /// 到目前为止的全部消息——assistant 那一轮存的是**上一轮原样的 content 块**。
    messages: Vec<Value>,
    usage: TokenUsage,
    round: u32,
}

#[async_trait]
impl TurnDriver for AnthropicDriver {
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError> {
        if let RoundInput::ToolResults { results } = &input {
            append_message(&mut self.messages, tool_results_message(results));
        }
        self.round += 1;

        let effort = EffortSetting::from_option(self.model.effort.clone());
        let thinking_tokens = effort.as_option().and_then(thinking_budget);
        let max_tokens =
            thinking_tokens.map_or(DEFAULT_MAX_TOKENS, |budget| budget + RESPONSE_HEADROOM);
        let body = request_body(
            &self.model,
            &self.system,
            &self.messages,
            &self.tools,
            thinking_tokens,
            max_tokens,
        );

        let started = Instant::now();
        let outcome = self.round_trip(body).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let effort_label = match &effort {
            EffortSetting::ProviderDefault => "provider_default".to_string(),
            EffortSetting::Explicit(level) => level.to_string(),
        };
        match &outcome {
            Ok(round) => tracing::info!(
                role = %self.role,
                model = %self.model.model,
                api_backend = "anthropic_messages",
                effort = %effort_label,
                round = self.round,
                tool_calls = round.tool_calls.len(),
                truncated = round.truncated,
                elapsed_ms,
                "model round completed"
            ),
            Err(error) => tracing::warn!(
                role = %self.role,
                model = %self.model.model,
                api_backend = "anthropic_messages",
                effort = %effort_label,
                round = self.round,
                elapsed_ms,
                error = %error,
                "model round failed"
            ),
        }

        let round = outcome?;
        // 下一轮要把这一轮的 content 块原样放回同一条 assistant 消息——不打 cache_control。
        if let Some(Value::Array(blocks)) = &round.provider_blocks {
            append_message(
                &mut self.messages,
                json!({ "role": "assistant", "content": blocks }),
            );
        }
        accumulate(&mut self.usage, &round.usage);
        Ok(round)
    }

    fn usage(&self) -> TokenUsage {
        self.usage
    }
}

impl AnthropicDriver {
    async fn round_trip(&self, body: Value) -> Result<Round, LlmError> {
        let mut request = HttpRequest::new(&self.endpoint, body)
            .with_header(ANTHROPIC_VERSION_HEADER, ANTHROPIC_VERSION)
            .with_timeout(Duration::from_secs(self.model.timeout_secs.max(1)));
        if let Some(key) = &self.api_key {
            request = request.with_header("x-api-key", key.clone());
        }
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
        let mut assembled = Assembled::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(transport_error)?;
            for frame in decoder.push(&chunk) {
                assembled.absorb(frame.name.as_deref(), &frame.data);
            }
            if assembled.settled() {
                break;
            }
        }
        if !assembled.settled() {
            // 流断在半路。**这是可重试的失败，不是一个短回答**。
            tracing::warn!(
                mid_frame = decoder.has_trailing_bytes(),
                "流结束时没有收到 message_stop"
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
}

// ---------------------------------------------------------------- SSE 拼装

/// 逐帧拼起来的一轮回复。索引按 `content_block_start` / `_delta` / `_stop` 的
/// `index` 走，与 provider 的块顺序一一对应。
#[derive(Debug, Default)]
struct Assembled {
    /// 还没收到 `content_block_stop` 的块。
    blocks: BTreeMap<usize, PartialBlock>,
    /// 已经收尾、按 index 排好的块——这就是这一轮 `content` 数组的来源。
    items: BTreeMap<usize, Value>,
    usage: TokenUsage,
    stop_reason: Option<String>,
    message_stop: bool,
    failure: Option<LlmError>,
}

#[derive(Debug)]
enum PartialBlock {
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        partial_json: String,
    },
    /// `redacted_thinking` 一次性整块给出，没有增量——原样存住即可。未来遇到的其它
    /// 未知块类型也走这条路：不认识就原样带回去，不替 provider 决定该丢什么。
    Verbatim(Value),
}

impl PartialBlock {
    /// 块收尾时的样子。`ToolUse` 的 `input` 还是原始 JSON 文本（`__raw_input`）——解析
    /// 挪到 [`Assembled::finish`]，拼不出 JSON 时要能整轮判成 [`LlmError::Incomplete`]，
    /// 而不是在这里悄悄吞掉。
    fn into_value(self) -> Value {
        match self {
            PartialBlock::Text(text) => json!({ "type": "text", "text": text }),
            PartialBlock::Thinking {
                thinking,
                signature,
            } => {
                json!({ "type": "thinking", "thinking": thinking, "signature": signature })
            }
            PartialBlock::ToolUse {
                id,
                name,
                partial_json,
            } => json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": {},
                "__raw_input": partial_json,
            }),
            PartialBlock::Verbatim(value) => value,
        }
    }
}

impl Assembled {
    /// 这一轮有结论了吗（`message_stop`，或失败）。**没收到 `message_stop` 就是没收齐**。
    fn settled(&self) -> bool {
        self.message_stop || self.failure.is_some()
    }

    fn absorb(&mut self, name: Option<&str>, data: &str) {
        // 一帧解析不了就丢掉这一帧：真正的"没收齐"由终止事件判定。
        let Ok(payload) = serde_json::from_str::<Value>(data) else {
            tracing::debug!(bytes = data.len(), "跳过一帧解析不了的 SSE 负载");
            return;
        };
        let Some(name) = name.or_else(|| payload.get("type").and_then(Value::as_str)) else {
            return;
        };
        let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;

        match name {
            MESSAGE_START => {
                if let Some(tokens) = payload
                    .pointer("/message/usage/input_tokens")
                    .and_then(Value::as_u64)
                {
                    self.usage.input = Some(tokens);
                }
            }
            CONTENT_BLOCK_START => {
                if let Some(block) = payload.get("content_block") {
                    self.start_block(index, block);
                }
            }
            CONTENT_BLOCK_DELTA => {
                if let Some(delta) = payload.get("delta") {
                    self.apply_delta(index, delta);
                }
            }
            CONTENT_BLOCK_STOP => self.finish_block(index),
            MESSAGE_DELTA => {
                if let Some(reason) = payload
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(tokens) = payload
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                {
                    self.usage.output = Some(tokens);
                }
            }
            MESSAGE_STOP => self.message_stop = true,
            ERROR_EVENT => {
                let detail = payload.get("error").unwrap_or(&payload);
                let message = detail
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("上游报错")
                    .to_string();
                let kind = detail
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // 没有 HTTP 状态码随附的流内错误：按错误类型换算成同一档退避（§8.4）
                // 认得出的状态；不认识的按 500（分不出是谁的问题）。
                let status = match kind {
                    "overloaded_error" => 529,
                    "rate_limit_error" => 429,
                    "authentication_error" | "permission_error" => 401,
                    _ => 500,
                };
                self.failure = Some(LlmError::Rejected {
                    status,
                    message: format!("{message}（type={kind}）"),
                });
            }
            PING => {}
            _ => {}
        }
    }

    fn start_block(&mut self, index: usize, block: &Value) {
        let partial = match block.get("type").and_then(Value::as_str) {
            Some("text") => PartialBlock::Text(String::new()),
            Some("thinking") => {
                // 中转的实现会在 `content_block_start` 里就给签名（真实观测：
                // claude-sonnet-4.6），官方 API 则留空、末尾用 `signature_delta` 补上——
                // 两种都要接得住。
                let signature = block
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                PartialBlock::Thinking {
                    thinking: String::new(),
                    signature,
                }
            }
            Some("tool_use") => PartialBlock::ToolUse {
                id: block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                partial_json: String::new(),
            },
            _ => PartialBlock::Verbatim(block.clone()),
        };
        self.blocks.insert(index, partial);
    }

    fn apply_delta(&mut self, index: usize, delta: &Value) {
        let Some(block) = self.blocks.get_mut(&index) else {
            return;
        };
        match (delta.get("type").and_then(Value::as_str), block) {
            (Some("text_delta"), PartialBlock::Text(text)) => {
                if let Some(chunk) = delta.get("text").and_then(Value::as_str) {
                    text.push_str(chunk);
                }
            }
            (Some("thinking_delta"), PartialBlock::Thinking { thinking, .. }) => {
                if let Some(chunk) = delta.get("thinking").and_then(Value::as_str) {
                    thinking.push_str(chunk);
                }
            }
            (Some("signature_delta"), PartialBlock::Thinking { signature, .. }) => {
                if let Some(chunk) = delta.get("signature").and_then(Value::as_str) {
                    signature.push_str(chunk);
                }
            }
            (Some("input_json_delta"), PartialBlock::ToolUse { partial_json, .. }) => {
                if let Some(chunk) = delta.get("partial_json").and_then(Value::as_str) {
                    partial_json.push_str(chunk);
                }
            }
            _ => {}
        }
    }

    fn finish_block(&mut self, index: usize) {
        if let Some(block) = self.blocks.remove(&index) {
            self.items.insert(index, block.into_value());
        }
    }

    fn finish(mut self, round: u32) -> Result<Round, LlmError> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        // `max_tokens` 截断可能断在一个块中间，没有等到它的 `content_block_stop`——
        // 收尾时把还没结束的块也收进来，读到的 thinking / 文本才不会凭空消失。
        for index in self.blocks.keys().copied().collect::<Vec<_>>() {
            self.finish_block(index);
        }

        let truncated = self.stop_reason.as_deref() == Some(STOP_MAX_TOKENS);
        let mut content = Vec::new();
        let mut tool_calls = Vec::new();
        let mut text = String::new();

        for (_, mut item) in self.items {
            let kind = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if kind == "tool_use" {
                // §6：被截断的一轮**不构造任何工具调用**，半个 tool_use 也不回放。
                if truncated {
                    continue;
                }
                let raw = item
                    .as_object_mut()
                    .and_then(|obj| obj.remove("__raw_input"))
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default();
                // 拼不成 JSON 就是没收齐——不能按半轮调用执行（同 Responses 的口径）。
                let input: Value = if raw.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&raw).map_err(|_| LlmError::Incomplete)?
                };
                item["input"] = input.clone();
                tool_calls.push(ProviderToolCall {
                    provider_call_id: item["id"].as_str().unwrap_or_default().to_string(),
                    name: item["name"].as_str().unwrap_or_default().to_string(),
                    arguments: input,
                });
            } else if kind == "text"
                && let Some(chunk) = item["text"].as_str()
            {
                text.push_str(chunk);
            }
            content.push(item);
        }

        Ok(Round {
            round,
            text: (!text.is_empty()).then_some(text),
            tool_calls,
            provider_blocks: Some(Value::Array(content)),
            usage: self.usage,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::{RunId, Seq, SessionId, ToolCallId};
    use komo_kernel::types::turn::ToolCallRequest;

    use super::super::transport::testing::{Reply, ScriptedTransport};

    fn model(effort: Option<&str>) -> ModelConfig {
        ModelConfig {
            provider: super::super::ANTHROPIC_MESSAGES.into(),
            base_url: "https://relay.example.com/v1".into(),
            model: "claude-sonnet-4.6".into(),
            api_key_env: "KEY".into(),
            auth: None,
            effort: effort.map(Effort::new),
            efforts: None,
            timeout_secs: 10,
        }
    }

    fn factory(transport: &ScriptedTransport) -> super::super::LlmFactory {
        super::super::LlmFactory::new(
            Arc::new(crate::config::Secrets::from_pairs([("KEY", "sk-test")])),
            EffortCapabilities::builtin(),
        )
        .with_transport(Arc::new(transport.clone()))
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

    fn frame(data: &str) -> String {
        format!("data: {data}\n\n")
    }

    fn event(name: &str, data: &str) -> String {
        format!("event: {name}\n{}", frame(data))
    }

    // ---- 纯文本 ----

    #[tokio::test]
    async fn a_plain_text_stream_is_assembled() {
        let transport = ScriptedTransport::new(vec![Reply::raw(
            200,
            &[
                event(
                    MESSAGE_START,
                    r#"{"message":{"usage":{"input_tokens":12}}}"#,
                ),
                event(
                    CONTENT_BLOCK_START,
                    r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
                ),
                event(
                    CONTENT_BLOCK_DELTA,
                    r#"{"index":0,"delta":{"type":"text_delta","text":"你好"}}"#,
                ),
                event(CONTENT_BLOCK_STOP, r#"{"index":0}"#),
                event(
                    MESSAGE_DELTA,
                    r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
                ),
                event(MESSAGE_STOP, r#"{}"#),
            ],
        )]);
        let config = model(None);
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let round = driver.next(RoundInput::First).await.unwrap();

        assert_eq!(round.text.as_deref(), Some("你好"));
        assert!(!round.truncated);
        assert_eq!(round.usage.input, Some(12));
        assert_eq!(round.usage.output, Some(3));

        let body = &transport.bodies()[0];
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        assert!(
            body.get("thinking").is_none(),
            "未配置 effort 不发 thinking"
        );
        assert_eq!(
            transport.requests()[0].url,
            "https://relay.example.com/v1/messages"
        );
        let headers = &transport.requests()[0].headers;
        assert!(headers.contains(&(
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string()
        )));
    }

    // ---- 工具调用（中转实测夹具） ----

    #[tokio::test]
    async fn a_tool_call_stream_is_assembled_with_parsed_input() {
        let transport = ScriptedTransport::new(vec![Reply::raw(
            200,
            &[
                event(
                    MESSAGE_START,
                    r#"{"message":{"usage":{"input_tokens":120,"output_tokens":0}}}"#,
                ),
                event(
                    CONTENT_BLOCK_START,
                    r#"{"index":0,"content_block":{"type":"tool_use","id":"toolu_bdrk_01LU","name":"get_time","input":{}}}"#,
                ),
                event(
                    CONTENT_BLOCK_DELTA,
                    r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\": \"北京\"}"}}"#,
                ),
                event(CONTENT_BLOCK_STOP, r#"{"index":0}"#),
                event(
                    MESSAGE_DELTA,
                    r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":0}}"#,
                ),
                event(MESSAGE_STOP, r#"{}"#),
            ],
        )]);
        let config = model(None);
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let round = driver.next(RoundInput::First).await.unwrap();

        assert_eq!(round.tool_calls.len(), 1);
        assert_eq!(round.tool_calls[0].provider_call_id, "toolu_bdrk_01LU");
        assert_eq!(round.tool_calls[0].name, "get_time");
        assert_eq!(round.tool_calls[0].arguments["city"], json!("北京"));
        let Some(Value::Array(blocks)) = &round.provider_blocks else {
            panic!("{:?}", round.provider_blocks)
        };
        assert_eq!(blocks[0]["type"], json!("tool_use"));
        assert_eq!(
            blocks[0]["input"]["city"],
            json!("北京"),
            "input 是对象，不是字符串"
        );
        assert!(
            blocks[0].get("__raw_input").is_none(),
            "内部拼装字段不能漏进最终块：{:?}",
            blocks[0]
        );
    }

    // ---- thinking：签名在 start 里就给（中转实测：claude-sonnet-4.6） ----

    #[tokio::test]
    async fn thinking_with_a_signature_given_at_block_start_is_kept() {
        let transport = ScriptedTransport::new(vec![Reply::raw(
            200,
            &[
                event(
                    CONTENT_BLOCK_START,
                    r#"{"index":0,"content_block":{"type":"thinking","thinking":"","signature":"sig_3c1ecbb4"}}"#,
                ),
                event(
                    CONTENT_BLOCK_DELTA,
                    r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"先想想"}}"#,
                ),
                event(CONTENT_BLOCK_STOP, r#"{"index":0}"#),
                event(
                    CONTENT_BLOCK_START,
                    r#"{"index":1,"content_block":{"type":"text","text":""}}"#,
                ),
                event(
                    CONTENT_BLOCK_DELTA,
                    r#"{"index":1,"delta":{"type":"text_delta","text":"答案"}}"#,
                ),
                event(CONTENT_BLOCK_STOP, r#"{"index":1}"#),
                event(MESSAGE_DELTA, r#"{"delta":{"stop_reason":"end_turn"}}"#),
                event(MESSAGE_STOP, r#"{}"#),
            ],
        )]);
        let config = model(Some("low"));
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let round = driver.next(RoundInput::First).await.unwrap();

        assert_eq!(round.text.as_deref(), Some("答案"));
        let Some(Value::Array(blocks)) = &round.provider_blocks else {
            panic!()
        };
        assert_eq!(blocks[0]["type"], json!("thinking"));
        assert_eq!(blocks[0]["thinking"], json!("先想想"));
        assert_eq!(blocks[0]["signature"], json!("sig_3c1ecbb4"));
    }

    // ---- thinking：官方形态，签名靠末尾的 signature_delta ----

    #[tokio::test]
    async fn thinking_with_a_trailing_signature_delta_is_kept() {
        let transport = ScriptedTransport::new(vec![Reply::raw(
            200,
            &[
                event(
                    CONTENT_BLOCK_START,
                    r#"{"index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                ),
                event(
                    CONTENT_BLOCK_DELTA,
                    r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"思考中"}}"#,
                ),
                event(
                    CONTENT_BLOCK_DELTA,
                    r#"{"index":0,"delta":{"type":"signature_delta","signature":"sig_official"}}"#,
                ),
                event(CONTENT_BLOCK_STOP, r#"{"index":0}"#),
                event(MESSAGE_DELTA, r#"{"delta":{"stop_reason":"end_turn"}}"#),
                event(MESSAGE_STOP, r#"{}"#),
            ],
        )]);
        let config = model(Some("low"));
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let round = driver.next(RoundInput::First).await.unwrap();

        let Some(Value::Array(blocks)) = &round.provider_blocks else {
            panic!()
        };
        assert_eq!(blocks[0]["thinking"], json!("思考中"));
        assert_eq!(blocks[0]["signature"], json!("sig_official"));
    }

    // ---- 开了 thinking 但这一轮没有 thinking 块（中转实测：claude-sonnet-5） ----

    #[tokio::test]
    async fn thinking_enabled_but_no_thinking_block_is_fine() {
        let transport = ScriptedTransport::new(vec![Reply::raw(
            200,
            &[
                event(
                    CONTENT_BLOCK_START,
                    r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
                ),
                event(
                    CONTENT_BLOCK_DELTA,
                    r#"{"index":0,"delta":{"type":"text_delta","text":"直接答"}}"#,
                ),
                event(CONTENT_BLOCK_STOP, r#"{"index":0}"#),
                event(MESSAGE_DELTA, r#"{"delta":{"stop_reason":"end_turn"}}"#),
                event(MESSAGE_STOP, r#"{}"#),
            ],
        )]);
        let config = model(Some("high"));
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let round = driver.next(RoundInput::First).await.unwrap();
        assert_eq!(round.text.as_deref(), Some("直接答"));
        assert_eq!(
            transport.bodies()[0]["thinking"]["budget_tokens"],
            json!(24_000)
        );
    }

    // ---- redacted_thinking 原样保留 ----

    #[tokio::test]
    async fn a_redacted_thinking_block_is_kept_verbatim() {
        let transport = ScriptedTransport::new(vec![Reply::raw(
            200,
            &[
                event(
                    CONTENT_BLOCK_START,
                    r#"{"index":0,"content_block":{"type":"redacted_thinking","data":"OPAQUE_BASE64"}}"#,
                ),
                event(CONTENT_BLOCK_STOP, r#"{"index":0}"#),
                event(MESSAGE_DELTA, r#"{"delta":{"stop_reason":"end_turn"}}"#),
                event(MESSAGE_STOP, r#"{}"#),
            ],
        )]);
        let config = model(None);
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let round = driver.next(RoundInput::First).await.unwrap();

        let Some(Value::Array(blocks)) = &round.provider_blocks else {
            panic!()
        };
        assert_eq!(blocks[0]["type"], json!("redacted_thinking"));
        assert_eq!(blocks[0]["data"], json!("OPAQUE_BASE64"));
    }

    // ---- max_tokens 截断 ----

    #[tokio::test]
    async fn a_max_tokens_stop_truncates_and_drops_the_half_built_call() {
        let transport = ScriptedTransport::new(vec![Reply::raw(
            200,
            &[
                event(
                    CONTENT_BLOCK_START,
                    r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
                ),
                event(
                    CONTENT_BLOCK_DELTA,
                    r#"{"index":0,"delta":{"type":"text_delta","text":"写到一半"}}"#,
                ),
                event(
                    CONTENT_BLOCK_START,
                    r#"{"index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
                ),
                event(
                    CONTENT_BLOCK_DELTA,
                    r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
                ),
                event(
                    MESSAGE_DELTA,
                    r#"{"delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":64}}"#,
                ),
                event(MESSAGE_STOP, r#"{}"#),
            ],
        )]);
        let config = model(None);
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let round = driver.next(RoundInput::First).await.unwrap();

        assert!(round.truncated);
        assert!(round.tool_calls.is_empty(), "半轮调用不能执行");
        assert_eq!(round.text.as_deref(), Some("写到一半"));
        assert_eq!(round.usage.output, Some(64));
        let Some(Value::Array(blocks)) = &round.provider_blocks else {
            panic!()
        };
        assert!(
            blocks.iter().all(|b| b["type"] != json!("tool_use")),
            "没收齐的调用不进回放：{blocks:?}"
        );
    }

    // ---- 没有 message_stop = 没收齐 ----

    #[tokio::test]
    async fn a_stream_without_message_stop_is_incomplete() {
        let transport = ScriptedTransport::new(vec![Reply::raw(
            200,
            &[event(
                CONTENT_BLOCK_DELTA,
                r#"{"index":0,"delta":{"type":"text_delta","text":"半句"}}"#,
            )],
        )]);
        let config = model(None);
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let error = driver.next(RoundInput::First).await.unwrap_err();
        assert_eq!(error, LlmError::Incomplete);
        assert!(super::super::is_retryable(&error));
    }

    // ---- 错误分类 ----

    #[tokio::test]
    async fn a_401_is_rejected_and_not_retried() {
        let transport = ScriptedTransport::new(vec![Reply::json(
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        )]);
        let config = model(None);
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let error = driver.next(RoundInput::First).await.unwrap_err();
        let LlmError::Rejected { status, message } = &error else {
            panic!("{error:?}")
        };
        assert_eq!(*status, 401);
        assert!(message.contains("invalid x-api-key"), "{message}");
        assert!(!super::super::is_retryable(&error));
    }

    #[tokio::test]
    async fn a_429_is_retryable() {
        let transport = ScriptedTransport::new(vec![Reply::json(
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"慢一点"}}"#,
        )]);
        let config = model(None);
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let error = driver.next(RoundInput::First).await.unwrap_err();
        assert!(matches!(&error, LlmError::Rejected { status: 429, .. }));
        assert!(super::super::is_retryable(&error));
    }

    #[tokio::test]
    async fn a_529_overloaded_is_retryable() {
        let transport = ScriptedTransport::new(vec![Reply::json(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        )]);
        let config = model(None);
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let error = driver.next(RoundInput::First).await.unwrap_err();
        assert!(matches!(&error, LlmError::Rejected { status: 529, .. }));
        assert!(super::super::is_retryable(&error));
    }

    #[tokio::test]
    async fn a_400_carries_its_message_and_is_not_retried() {
        let transport = ScriptedTransport::new(vec![Reply::json(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens must be greater than thinking.budget_tokens"}}"#,
        )]);
        let config = model(None);
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let error = driver.next(RoundInput::First).await.unwrap_err();
        let LlmError::Rejected { status, message } = &error else {
            panic!("{error:?}")
        };
        assert_eq!(*status, 400);
        assert!(message.contains("budget_tokens"), "{message}");
        assert!(!super::super::is_retryable(&error));
    }

    /// 流中途的 `event: error`（没有独立 HTTP 状态码，靠 `type` 换算）。
    #[tokio::test]
    async fn a_mid_stream_error_event_ends_the_round() {
        let transport = ScriptedTransport::new(vec![Reply::raw(
            200,
            &[event(
                ERROR_EVENT,
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            )],
        )]);
        let config = model(None);
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        let error = driver.next(RoundInput::First).await.unwrap_err();
        assert!(
            matches!(&error, LlmError::Rejected { status: 529, .. }),
            "{error:?}"
        );
        assert!(super::super::is_retryable(&error));
    }

    // ---- effort → thinking 预算 ----

    #[tokio::test]
    async fn explicit_none_sends_no_thinking_field() {
        let transport = ScriptedTransport::new(vec![Reply::raw(
            200,
            &[
                event(MESSAGE_DELTA, r#"{"delta":{"stop_reason":"end_turn"}}"#),
                event(MESSAGE_STOP, r#"{}"#),
            ],
        )]);
        let config = model(Some("none"));
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut driver = llm.begin_turn(request(&config)).await.unwrap();
        driver.next(RoundInput::First).await.unwrap();
        let body = &transport.bodies()[0];
        assert!(body.get("thinking").is_none());
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
    }

    #[tokio::test]
    async fn each_explicit_level_maps_to_its_budget_and_max_tokens() {
        for (level, budget) in [
            ("low", 4096u32),
            ("medium", 10_000),
            ("high", 24_000),
            ("max", 32_000),
        ] {
            let transport = ScriptedTransport::new(vec![Reply::raw(
                200,
                &[
                    event(MESSAGE_DELTA, r#"{"delta":{"stop_reason":"end_turn"}}"#),
                    event(MESSAGE_STOP, r#"{}"#),
                ],
            )]);
            let config = model(Some(level));
            let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
            let mut driver = llm.begin_turn(request(&config)).await.unwrap();
            driver.next(RoundInput::First).await.unwrap();
            let body = &transport.bodies()[0];
            assert_eq!(
                body["thinking"],
                json!({ "type": "enabled", "budget_tokens": budget }),
                "{level}"
            );
            assert_eq!(
                body["max_tokens"],
                json!(budget + RESPONSE_HEADROOM),
                "{level}"
            );
        }
    }

    #[tokio::test]
    async fn an_unsupported_effort_is_refused_before_anything_is_sent() {
        let transport = ScriptedTransport::new(vec![]);
        let Err(error) = factory(&transport).build(&model(Some("ultra")), ModelRole::Main) else {
            panic!("不支持的档位必须在请求前被拒绝")
        };
        assert!(matches!(
            error,
            super::super::LlmBuildError::Model(LlmError::UnsupportedEffort { .. })
        ));
        assert!(transport.requests().is_empty());
    }

    // ---- cache_control ----

    #[tokio::test]
    async fn cache_control_marks_system_tools_and_the_last_user_block_but_not_history() {
        let transport = ScriptedTransport::new(vec![
            Reply::raw(
                200,
                &[
                    event(
                        CONTENT_BLOCK_START,
                        r#"{"index":0,"content_block":{"type":"tool_use","id":"call_1","name":"read","input":{}}}"#,
                    ),
                    event(
                        CONTENT_BLOCK_DELTA,
                        r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
                    ),
                    event(CONTENT_BLOCK_STOP, r#"{"index":0}"#),
                    event(MESSAGE_DELTA, r#"{"delta":{"stop_reason":"tool_use"}}"#),
                    event(MESSAGE_STOP, r#"{}"#),
                ],
            ),
            Reply::raw(
                200,
                &[
                    event(
                        CONTENT_BLOCK_START,
                        r#"{"index":0,"content_block":{"type":"tool_use","id":"call_2","name":"read","input":{}}}"#,
                    ),
                    event(
                        CONTENT_BLOCK_DELTA,
                        r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
                    ),
                    event(CONTENT_BLOCK_STOP, r#"{"index":0}"#),
                    event(MESSAGE_DELTA, r#"{"delta":{"stop_reason":"tool_use"}}"#),
                    event(MESSAGE_STOP, r#"{}"#),
                ],
            ),
            Reply::raw(
                200,
                &[
                    event(MESSAGE_DELTA, r#"{"delta":{"stop_reason":"end_turn"}}"#),
                    event(MESSAGE_STOP, r#"{}"#),
                ],
            ),
        ]);
        let config = model(None);
        let llm = factory(&transport).build(&config, ModelRole::Main).unwrap();
        let mut req = request(&config);
        req.tools = vec![ToolDefinition {
            name: "read".into(),
            description: "读文件".into(),
            parameters: json!({ "type": "object" }),
        }];
        let mut driver = llm.begin_turn(req).await.unwrap();

        driver.next(RoundInput::First).await.unwrap();
        let first = &transport.bodies()[0];
        assert_eq!(
            first["system"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        assert_eq!(
            first["tools"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        assert!(first["messages"][0]["content"][0]["cache_control"].is_null());

        driver
            .next(RoundInput::ToolResults {
                results: vec![ToolResultForModel {
                    provider_call_id: "call_1".into(),
                    call_id: ToolCallId::from_raw("tc-1"),
                    content: "结果1".into(),
                    is_error: false,
                }],
            })
            .await
            .unwrap();
        let second = &transport.bodies()[1];
        let second_messages = second["messages"].as_array().unwrap();
        let last = second_messages.last().unwrap();
        assert_eq!(
            last["content"].as_array().unwrap().last().unwrap()["cache_control"],
            json!({ "type": "ephemeral" }),
            "最后一条 user 消息的末块要有断点"
        );

        driver
            .next(RoundInput::ToolResults {
                results: vec![ToolResultForModel {
                    provider_call_id: "call_2".into(),
                    call_id: ToolCallId::from_raw("tc-2"),
                    content: "结果2".into(),
                    is_error: false,
                }],
            })
            .await
            .unwrap();
        let third = &transport.bodies()[2];
        let third_messages = third["messages"].as_array().unwrap();
        // 上一轮那条工具结果消息现在在中间：它自己保存的那份不该带着 cache_control——
        // 标记只在“这一次要发”的副本上打，`self.messages` 从没被改过。
        let earlier_tool_result = &third_messages[third_messages.len() - 2];
        assert!(
            earlier_tool_result["content"][0]
                .get("cache_control")
                .is_none(),
            "历史消息不该带上一次请求打的缓存标记：{earlier_tool_result:?}"
        );
    }

    // ---- 回放：存过块就原样用 ----

    #[test]
    fn a_stored_content_array_is_replayed_verbatim() {
        let blocks = json!([
            { "type": "thinking", "thinking": "想", "signature": "sig" },
            { "type": "tool_use", "id": "call_1", "name": "read", "input": {} }
        ]);
        let replayed = replay(&[ReplayMessage {
            role: Role::Assistant,
            seq: Seq(1),
            text: Some("被忽略的重拼版本".into()),
            tool_calls: vec![],
            tool_results: vec![],
            provider_blocks: Some(blocks.clone()),
        }]);
        assert_eq!(replayed[0]["content"], blocks);
    }

    #[test]
    fn an_assistant_without_stored_blocks_is_rebuilt_with_an_object_input() {
        let replayed = replay(&[ReplayMessage {
            role: Role::Assistant,
            seq: Seq(1),
            text: Some("我来读一下".into()),
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
        assert_eq!(replayed[0]["content"][0]["type"], json!("text"));
        assert_eq!(replayed[0]["content"][1]["type"], json!("tool_use"));
        assert_eq!(
            replayed[0]["content"][1]["input"],
            json!({"path": "a.txt"}),
            "input 是对象，不是字符串"
        );
    }

    #[test]
    fn multiple_tool_results_land_in_one_user_message() {
        let replayed = replay(&[ReplayMessage {
            role: Role::Tool,
            seq: Seq(1),
            text: None,
            tool_calls: vec![],
            tool_results: vec![
                ToolResultForModel {
                    provider_call_id: "call_1".into(),
                    call_id: ToolCallId::from_raw("tc-1"),
                    content: "文件内容".into(),
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
        assert_eq!(replayed.len(), 1, "同一轮的多个结果并进一条消息");
        let content = replayed[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["tool_use_id"], json!("call_1"));
        assert_eq!(content[1]["is_error"], json!(true));
        assert!(content[0].get("is_error").is_none(), "没出错就不带这个键");
    }

    /// 两个历史 Run 之间缺了中间那句答复时，相邻的 user 消息要合并（协议要求交替）。
    #[test]
    fn adjacent_user_messages_from_two_runs_are_merged() {
        let replayed = replay(&[
            ReplayMessage {
                role: Role::User,
                seq: Seq(1),
                text: Some("第一句".into()),
                tool_calls: vec![],
                tool_results: vec![],
                provider_blocks: None,
            },
            ReplayMessage {
                role: Role::User,
                seq: Seq(2),
                text: Some("第二句".into()),
                tool_calls: vec![],
                tool_results: vec![],
                provider_blocks: None,
            },
        ]);
        assert_eq!(replayed.len(), 1);
        let content = replayed[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], json!("第一句"));
        assert_eq!(content[1]["text"], json!("第二句"));
    }
}

#[cfg(test)]
mod live {
    //! 唯一会真的打中转的测试——两轮：第一轮开 thinking 让 claude-sonnet-4.6 调
    //! 一个工具，第二轮把 thinking 原样回放 + tool_result 发回去拿最终答复。
    //!
    //! ```text
    //! KOMO_ANTHROPIC_LIVE_PROBE=1 cargo test -p komo-runtime --lib -- --ignored live_probe
    //! ```
    //!
    //! 才会打到真的中转。凭证从 `~/.komo/.env` 的 `OPENCODE_RELAY_TOKEN` 读，**不打印**。

    use komo_kernel::types::ids::{RunId, Seq, SessionId};
    use komo_kernel::types::model::Effort;
    use komo_kernel::types::tool::ToolDefinition;
    use komo_kernel::types::turn::{ReplayMessage, RoundInput, ToolResultForModel, TurnRequest};
    use serde_json::json;

    use super::*;

    fn relay_token() -> Option<String> {
        let home = std::env::var("HOME").ok()?;
        let content =
            std::fs::read_to_string(std::path::Path::new(&home).join(".komo/.env")).ok()?;
        content.lines().find_map(|line| {
            let value = line.trim().strip_prefix("OPENCODE_RELAY_TOKEN=")?;
            let value = value.trim().trim_matches('"').trim_matches('\'');
            (!value.is_empty()).then(|| value.to_string())
        })
    }

    #[tokio::test]
    #[ignore = "真机：要网络与 ~/.komo/.env 里的 OPENCODE_RELAY_TOKEN"]
    async fn live_probe_two_rounds_with_thinking_and_a_tool_call() {
        if std::env::var("KOMO_ANTHROPIC_LIVE_PROBE").as_deref() != Ok("1") {
            eprintln!("跳过：没有 KOMO_ANTHROPIC_LIVE_PROBE=1");
            return;
        }
        let Some(token) = relay_token() else {
            panic!("~/.komo/.env 里没有 OPENCODE_RELAY_TOKEN");
        };
        // `reqwest` 是 rustls-no-provider：装 provider 是进程的事（§13.4）。测试进程
        // 自己装一次，装过就算了。
        let _ = rustls::crypto::ring::default_provider().install_default();

        let config = ModelConfig {
            provider: super::super::ANTHROPIC_MESSAGES.into(),
            base_url:
                "http://igw-traefik.internal.svc.cluster.local/service/opencode-relay/relay/v1"
                    .into(),
            model: "claude-sonnet-4.6".into(),
            api_key_env: "OPENCODE_RELAY_TOKEN".into(),
            auth: None,
            effort: Some(Effort::new("low")),
            efforts: None,
            timeout_secs: 60,
        };
        let llm = AnthropicMessagesLlm::new(
            config.clone(),
            ModelRole::Main,
            Some(token),
            crate::llm::default_transport(),
            EffortCapabilities::builtin(),
        )
        .expect("effort=low 是声明过的档位");

        let request = TurnRequest {
            session: SessionId::from_raw("live-probe"),
            run: RunId::from_raw("live-probe"),
            model: config,
            system_prompt:
                "你是一个工具调用测试助手：第一次回复必须调用 get_time 工具查询北京当前时间；\
                 拿到工具结果后就直接用那个结果回答用户，不要再次调用工具。"
                    .into(),
            messages: vec![ReplayMessage {
                role: Role::User,
                seq: Seq(1),
                text: Some("现在北京几点？".into()),
                tool_calls: vec![],
                tool_results: vec![],
                provider_blocks: None,
            }],
            tools: vec![ToolDefinition {
                name: "get_time".into(),
                description: "返回指定城市当前时间".into(),
                parameters: json!({
                    "type": "object",
                    "properties": { "city": { "type": "string" } },
                    "required": ["city"],
                }),
            }],
            memories: vec![],
            covers: None,
        };

        let mut driver = llm.begin_turn(request).await.expect("这一路要通");
        let first = driver
            .next(RoundInput::First)
            .await
            .expect("第一轮：中转要给出 200");
        let has_thinking = matches!(&first.provider_blocks, Some(Value::Array(blocks))
            if blocks.iter().any(|b| b["type"] == json!("thinking")));
        println!(
            "round1: tool_calls={} has_thinking={has_thinking}",
            first.tool_calls.len()
        );
        assert!(
            !first.tool_calls.is_empty(),
            "该模型该调 get_time：{first:?}"
        );

        let call = &first.tool_calls[0];
        let second = driver
            .next(RoundInput::ToolResults {
                results: vec![ToolResultForModel {
                    provider_call_id: call.provider_call_id.clone(),
                    call_id: komo_kernel::types::ids::ToolCallId::from_raw("live-probe-call"),
                    content: "北京时间 2026-09-23 15:00".into(),
                    is_error: false,
                }],
            })
            .await
            .expect("第二轮：带 thinking 回放 + tool_result 也要 200");
        println!("round2 text: {:?}", second.text);
        assert!(second.text.is_some(), "该有最终答复：{second:?}");
    }
}
