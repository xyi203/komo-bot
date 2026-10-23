//! 无状态的模型调用（`docs/storage-durable.md` §12.3 的 `stream`）。
//!
//! pi 的形状：**每次给完整 context，返回一条 assistant 消息**，driver 不攒任何东西。
//! 对照旧的有状态 [`TurnDriver`](komo_kernel::traits::TurnDriver)——它在内存里攒着 provider
//! 原生的对话，衰减靠 `RoundInput::ToolResults.revised` 就地换；这里没有那些：衰减就是
//! 下一次请求给了一份换过视图的消息。
//!
//! 本文件是三家 provider 共用的那一半：入口 trait、请求值、system 消息回放（pi 的
//! sections / toolsAdded / toolsRemoved 语义）、图片引用 → 字节、`Message` → 旧回放面
//! （[`ReplayMessage`] / [`ToolResultForModel`]）的转换。三家各自的拼体与解析在
//! `anthropic/stateless.rs`、`responses/stateless.rs`、`chat/stateless.rs`：它们直接复用旧
//! driver 的拼体与解析函数，所以"今天有状态 driver 发出的请求体"与"这里发出的"逐字节
//! 相同是**构造出来的**，不是两份实现碰巧一致（`stream_tests.rs` 逐家验证）。
//!
//! # provider 原生块放在哪
//!
//! 返回的 [`AssistantMessage`] 的 `content` 是 provider 无关的投影（text / thinking /
//! toolCall），**原生块整份原样放在 `extra["providerBlocks"]`**（Anthropic / Responses 是
//! content / output items 数组，Chat Completions 是那条原生 assistant message），同时
//! `extra["api"]` 记下它来自哪个协议。下一次请求回放时：`api` 对得上（或旧记录没有
//! `api` 但形状对得上）就把 `providerBlocks` 原样放回，否则只从 `content` 重建文字与调用
//! ——和旧 driver 的 `ReplayMessage::provider_blocks` 同一条规则。

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use komo_kernel::durable::{
    AssistantMessage, Content, ContentBlock, Message, StopReason, ToolResultMessage, UserMessage,
};
use komo_kernel::types::ids::{Seq, ToolCallId};
use komo_kernel::types::model::{ModelConfig, ModelRole, TokenUsage};
use komo_kernel::types::tool::{ToolDefinition, ToolImage};
use komo_kernel::types::turn::{
    LlmError, ReplayMessage, Role, ToolCallRequest, ToolResultForModel,
};
use serde_json::{Map, Value, json};
use std::sync::Arc;

use super::transport::TransportError;
use super::{
    ANTHROPIC_MESSAGES, AnthropicMessagesLlm, CHAT_COMPLETIONS, CHATGPT_AUTH, ChatCompletionsLlm,
    LEGACY_OPENAI_RESPONSES, LlmBuildError, LlmFactory, OpenAiResponsesLlm, RESPONSES,
};

/// `extra["providerBlocks"]`：provider 原生块（见模块文档）。
pub const PROVIDER_BLOCKS: &str = "providerBlocks";

/// 三个协议在 `extra["api"]` 里的名字（pi 的 `Api` 取值）。
pub const API_ANTHROPIC: &str = "anthropic-messages";
pub const API_RESPONSES: &str = "openai-responses";
pub const API_CHAT: &str = "openai-completions";

/// 无状态的模型调用：`(model, context, options) -> AssistantMessage`。
///
/// `progress` 在流进行中被反复调用，拿到的是**到目前为止**的部分消息（`content` 是已经
/// 收到的文字 / 推理 / 调用，`stopReason` 不作数，没有 `providerBlocks`）；调用方自己节流
/// （D4 用它写 `komo.live`）。最终结果是返回值，不会再对 `progress` 重复一次。
///
/// 没有收齐终止帧、被拒绝、超时都是 `Err`（同旧 driver：[`LlmError`] 分类决定重试）；被
/// 截断的一轮是 `Ok`，`stopReason = length`，且**不含任何工具调用**。
#[async_trait]
pub trait ModelStream: Send + Sync {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
    ) -> Result<AssistantMessage, LlmError>;
}

/// 一次请求要的全部：模型（含 effort）、完整消息序列、选项。
#[derive(Clone, Copy)]
pub struct ModelRequest<'a> {
    /// 这一次用的模型配置——effort 就是 `model.effort`（pi 的 `prepareRequest` 要换 effort，
    /// 就给一份换过的配置）。
    pub model: &'a ModelConfig,
    /// 完整 context，system 消息在内：按 transcript 次序回放出最终提示与工具表
    /// （[`replay_system`]），其余消息按序转成各家的请求。
    pub messages: &'a [Message],
    pub options: RequestOptions<'a>,
}

impl<'a> ModelRequest<'a> {
    pub fn new(model: &'a ModelConfig, messages: &'a [Message]) -> Self {
        ModelRequest {
            model,
            messages,
            options: RequestOptions::default(),
        }
    }

    pub fn with_options(mut self, options: RequestOptions<'a>) -> Self {
        self.options = options;
        self
    }
}

/// 请求选项。全部可省。
#[derive(Clone, Copy, Default)]
pub struct RequestOptions<'a> {
    /// 图片引用 → 字节。不给就一张图也不附（正文照旧）。
    pub images: Option<&'a dyn ImageResolver>,
    /// 提示缓存的路由键（Responses 的 `prompt_cache_key` 与 ChatGPT 的 `session_id` 头）：
    /// 通常是会话 id，fork 任务给派它的会话（前缀相同键也得相同）。
    pub cache_key: Option<&'a str>,
    /// 单轮输出上限。Responses 的 `max_output_tokens`、Anthropic 的 `max_tokens`；不给就
    /// 按旧 driver 的默认。
    pub max_output_tokens: Option<u32>,
    /// 只对 Anthropic 有意义：这一轮**刚把** `messages[i]`（及之后）里的工具结果换了视图
    /// （衰减）时，给最早被换的那条的下标。旧 driver 的 `revised_from`：缓存断点要补在它
    /// 之前最后一条 user 消息上，换视图那一轮才不会整段缓存重写。不给 = 没有衰减发生。
    pub revised_from: Option<usize>,
}

/// 消息里的图片引用 `{mime, path, hash}`（§3.5 第 4 条：base64 不进任何记录）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub mime: String,
    /// 相对 Session 目录的路径（解析规则由 resolver 定）。
    pub path: String,
    /// 字节的 SHA-256，十六进制小写。
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("读不回图片 {path}：{reason}")]
pub struct ImageLoadError {
    pub path: String,
    pub reason: String,
}

/// 按引用把图片读回来。哈希由调用方（[`load_image`]）核对，resolver 只管读。
pub trait ImageResolver: Send + Sync {
    fn load(&self, image: &ImageRef) -> Result<Vec<u8>, ImageLoadError>;
}

impl ImageRef {
    /// 从一个 `image` 内容块的 `extra` 里取出引用；缺 `mime` / `path` 就不是引用。
    pub fn from_extra(extra: &Map<String, Value>) -> Option<Self> {
        let text = |key: &str| extra.get(key).and_then(Value::as_str).map(str::to_string);
        Some(ImageRef {
            mime: text("mime")?,
            path: text("path")?,
            hash: text("hash").unwrap_or_default(),
        })
    }
}

/// 读回一张图并核对哈希。读不回、哈希对不上都返回 `None`（并留一条 warn）：**少附一张，
/// 正文照旧**，与今天执行器读不回图片时的做法一致。
pub fn load_image(resolver: Option<&dyn ImageResolver>, image: &ImageRef) -> Option<ToolImage> {
    let resolver = resolver?;
    let bytes = match resolver.load(image) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(path = %image.path, %error, "图片读不回，这次请求少附一张");
            return None;
        }
    };
    if !image.hash.is_empty()
        && komo_kernel::types::digest::ContentHash::of_bytes(&bytes).as_str() != image.hash
    {
        tracing::warn!(path = %image.path, "图片哈希对不上，这次请求少附一张");
        return None;
    }
    Some(ToolImage {
        mime: image.mime.clone(),
        bytes,
    })
}

// ---------------------------------------------------------------- system 回放

/// 回放完所有 system 消息之后的当前提示与工具表。
#[derive(Debug, Clone, PartialEq)]
pub struct SystemState {
    pub prompt: String,
    pub tools: Vec<ToolDefinition>,
}

/// 按 transcript 次序回放全部 system 消息（照 pi `getCurrentSystemMessage`）：后来的
/// `content` 追加在基础提示后，`sections` 按名字补丁（同名替换保持原位，`null` 删除，
/// 删后再加排到最后），工具先删后加。提示 = `content` 然后各段，空的略去，`\n\n` 连接
/// （pi `getSystemMessageText`）。没有 system 消息 = 空提示、空工具表。
pub fn replay_system(messages: &[Message]) -> SystemState {
    let mut content: Vec<String> = Vec::new();
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut tools: Vec<(String, Value)> = Vec::new();
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        let text = content_text(&system.content);
        if !text.is_empty() {
            content.push(text);
        }
        if let Some(patch) = &system.sections {
            for (name, value) in &patch.0 {
                match value {
                    Some(text) => match sections.iter_mut().find(|(have, _)| have == name) {
                        Some(slot) => slot.1 = text.clone(),
                        None => sections.push((name.clone(), text.clone())),
                    },
                    None => sections.retain(|(have, _)| have != name),
                }
            }
        }
        for removed in system.tools_removed.iter().flatten() {
            if let Some(name) = tool_name(removed) {
                tools.retain(|(have, _)| have != name);
            }
        }
        for added in system.tools_added.iter().flatten() {
            let Some(name) = tool_name(added) else {
                continue;
            };
            match tools.iter_mut().find(|(have, _)| have == name) {
                Some(slot) => slot.1 = added.clone(),
                None => tools.push((name.to_string(), added.clone())),
            }
        }
    }
    let prompt = std::iter::once(content.join("\n\n"))
        .chain(sections.into_iter().map(|(_, text)| text))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    SystemState {
        prompt,
        tools: tools
            .into_iter()
            .map(|(_, definition)| tool_definition(&definition))
            .collect(),
    }
}

fn tool_name(tool: &Value) -> Option<&str> {
    tool.get("name").and_then(Value::as_str)
}

fn tool_definition(tool: &Value) -> ToolDefinition {
    ToolDefinition {
        name: tool_name(tool).unwrap_or_default().to_string(),
        description: tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        parameters: tool
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" })),
        doc: None,
    }
}

fn content_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

// ---------------------------------------------------------------- Message → 旧回放面

/// user 消息的一段内容。
pub(super) enum UserPart {
    Text(String),
    Image(ToolImage),
}

/// user 消息 → 有序的文字 / 图片段。`Content::Text` 恒为一段文字；图片读不回的略去。
pub(super) fn user_parts(
    message: &UserMessage,
    resolver: Option<&dyn ImageResolver>,
) -> Vec<UserPart> {
    match &message.content {
        Content::Text(text) => vec![UserPart::Text(text.clone())],
        Content::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(UserPart::Text(text.clone())),
                ContentBlock::Image { extra } => ImageRef::from_extra(extra)
                    .and_then(|image| load_image(resolver, &image))
                    .map(UserPart::Image),
                _ => None,
            })
            .collect(),
    }
}

/// 没有图片时 user 消息就是一段文字（和旧 driver 的 `ReplayMessage::text` 同形）。
pub(super) fn plain_text(parts: &[UserPart]) -> Option<String> {
    if parts.iter().any(|part| matches!(part, UserPart::Image(_))) {
        return None;
    }
    Some(
        parts
            .iter()
            .filter_map(|part| match part {
                UserPart::Text(text) => Some(text.as_str()),
                UserPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// 工具结果 → 旧回放面的 [`ToolResultForModel`]：正文是文字块用 `\n` 连起来（D4 存的模型
/// 视图是单个文字块，所以通常就是它本身），图片按引用读回。
pub(super) fn tool_result_for_model(
    message: &ToolResultMessage,
    resolver: Option<&dyn ImageResolver>,
) -> ToolResultForModel {
    let content = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image { extra } => ImageRef::from_extra(extra),
            _ => None,
        })
        .filter_map(|image| load_image(resolver, &image))
        .collect();
    ToolResultForModel {
        provider_call_id: message.tool_call_id.clone(),
        call_id: ToolCallId::from_raw(message.tool_call_id.clone()),
        content,
        is_error: message.is_error,
        images,
    }
}

/// 一条 assistant 消息 → 旧回放面的 assistant [`ReplayMessage`]。
///
/// `native_ok` 判断 `providerBlocks` 的形状是不是这一家认的；`api` 标了别家的就一律不用。
pub(super) fn assistant_replay(
    message: &AssistantMessage,
    index: usize,
    api: &str,
    native_ok: impl Fn(&Value) -> bool,
) -> ReplayMessage {
    let from_other_api = message
        .extra
        .get("api")
        .and_then(Value::as_str)
        .is_some_and(|tagged| tagged != api);
    let provider_blocks = message
        .extra
        .get(PROVIDER_BLOCKS)
        .filter(|blocks| !from_other_api && native_ok(blocks))
        .cloned();
    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let tool_calls = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some(ToolCallRequest {
                call_id: ToolCallId::from_raw(id.clone()),
                provider_call_id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                arguments_ref: None,
            }),
            _ => None,
        })
        .collect();
    ReplayMessage {
        role: Role::Assistant,
        seq: Seq(index as u64),
        text: (!text.is_empty()).then_some(text),
        tool_calls,
        tool_results: Vec::new(),
        provider_blocks,
    }
}

/// 紧挨着的一串 toolResult 消息（一轮的全部结果）：`messages[start..]` 里连续的那段，
/// 返回它们转成的结果与段尾下标（不含）。
pub(super) fn tool_result_run(
    messages: &[Message],
    start: usize,
    resolver: Option<&dyn ImageResolver>,
) -> (Vec<ToolResultForModel>, usize) {
    let mut results = Vec::new();
    let mut end = start;
    while let Some(Message::ToolResult(result)) = messages.get(end) {
        results.push(tool_result_for_model(result, resolver));
        end += 1;
    }
    (results, end)
}

// ---------------------------------------------------------------- 结果

pub(super) fn transport_error(error: TransportError) -> LlmError {
    match error {
        TransportError::Timeout => LlmError::Timeout,
        TransportError::Failed(message) => LlmError::Transport(message),
    }
}

/// `Round` 里没有 provider 的 stop_reason 原文，只有"截断了没有"；于是：截断 = `length`
/// （含 Chat 的 `content_filter`——旧 driver 同样当截断），有调用 = `toolUse`，其余 `stop`。
pub(super) fn stop_reason(truncated: bool, has_calls: bool) -> StopReason {
    if truncated {
        StopReason::Length
    } else if has_calls {
        StopReason::ToolUse
    } else {
        StopReason::Stop
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// 用量的记录形状：komo 的口径（`input` 是整个提示，`cacheRead` / `cacheWrite` 是它的子集），
/// 未知的字段不写（未知不能当成零，§8.5）。
pub fn usage_json(usage: &TokenUsage) -> Value {
    let mut out = Map::new();
    for (key, value) in [
        ("input", usage.input),
        ("output", usage.output),
        ("reasoning", usage.reasoning),
        ("cacheRead", usage.cache_read),
        ("cacheWrite", usage.cache_write),
    ] {
        if let Some(value) = value {
            out.insert(key.into(), json!(value));
        }
    }
    Value::Object(out)
}

/// 反过来：从 `extra["usage"]` 读回 [`TokenUsage`]（D4 累计用量时用）。
pub fn usage_of(message: &AssistantMessage) -> TokenUsage {
    let read = |key: &str| message.extra.get("usage")?.get(key)?.as_u64();
    TokenUsage {
        input: read("input"),
        output: read("output"),
        reasoning: read("reasoning"),
        cache_read: read("cacheRead"),
        cache_write: read("cacheWrite"),
    }
}

/// 最终的 assistant 消息：`content` 投影 + `api` / `provider` / `model` / `usage` /
/// `providerBlocks`。
pub(super) fn final_message(
    model: &ModelConfig,
    api: &str,
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
    usage: &TokenUsage,
    provider_blocks: Option<Value>,
) -> AssistantMessage {
    let mut extra = Map::new();
    extra.insert("api".into(), json!(api));
    extra.insert("provider".into(), json!(model.provider));
    extra.insert("model".into(), json!(model.model));
    extra.insert("usage".into(), usage_json(usage));
    if let Some(blocks) = provider_blocks {
        extra.insert(PROVIDER_BLOCKS.into(), blocks);
    }
    AssistantMessage {
        content,
        stop_reason,
        timestamp: now_ms(),
        extra,
    }
}

/// 流进行中的部分消息（见 [`ModelStream`]）。
pub(super) fn partial_message(
    model: &ModelConfig,
    api: &str,
    content: Vec<ContentBlock>,
) -> AssistantMessage {
    final_message(
        model,
        api,
        content,
        StopReason::Stop,
        &TokenUsage::default(),
        None,
    )
}

pub(super) fn thinking_block(thinking: String, extra: Map<String, Value>) -> ContentBlock {
    ContentBlock::Thinking { thinking, extra }
}

/// 参数 JSON 文本 → 值；还没收齐（流进行中）就给空对象。
pub(super) fn lenient_arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
}

// ---------------------------------------------------------------- 构造

impl LlmFactory {
    /// 与 [`LlmFactory::build`] 同一套凭证与档位规则，造一个无状态入口。
    pub fn build_stream(
        &self,
        config: &ModelConfig,
        role: ModelRole,
    ) -> Result<Arc<dyn ModelStream>, LlmBuildError> {
        match config.provider.as_str() {
            RESPONSES | LEGACY_OPENAI_RESPONSES => Ok(Arc::new(OpenAiResponsesLlm::new(
                config.clone(),
                role,
                self.credential_for(config)?,
                Arc::clone(&self.transport),
                self.caps.clone(),
            )?)),
            CHAT_COMPLETIONS | ANTHROPIC_MESSAGES => {
                if let Some(auth) = &config.auth {
                    return Err(if auth == CHATGPT_AUTH {
                        LlmBuildError::ChatGptRequiresResponses
                    } else {
                        LlmBuildError::UnknownAuth { auth: auth.clone() }
                    });
                }
                let key = self.secrets.get(&config.api_key_env).map(str::to_string);
                if config.provider == CHAT_COMPLETIONS {
                    Ok(Arc::new(ChatCompletionsLlm::new(
                        config.clone(),
                        role,
                        key,
                        Arc::clone(&self.transport),
                        self.caps.clone(),
                    )?))
                } else {
                    Ok(Arc::new(AnthropicMessagesLlm::new(
                        config.clone(),
                        role,
                        key,
                        Arc::clone(&self.transport),
                        self.caps.clone(),
                    )?))
                }
            }
            other => Err(LlmBuildError::UnknownProvider {
                provider: other.to_string(),
            }),
        }
    }
}
