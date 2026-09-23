//! OpenAI **Responses API** 的 [`LlmClient`]（流式 SSE + 函数调用）。
//!
//! 四条来自文档的硬性质，各在下面有一段代码和一个测试：
//!
//! - **每次请求都流式**，而「没收到 `response.completed`」是一次**可重试的失败**，不是
//!   一个短回答（§6：参数未收齐时不能开始执行）。
//! - **保留协议回放所需的消息块**（§13.2）：这一轮的**全部 output items**——包括
//!   `reasoning` 项与它的 `encrypted_content`——原样进 [`Round::provider_blocks`]，下一轮
//!   逐字放回 `input`。请求里因此固定带 `store: false` + `include:
//!   ["reasoning.encrypted_content"]`：不依赖服务端保存，回放全靠自己携带。
//! - **effort 未配置就不发 `reasoning` 字段**；不支持的档位在**请求前**拒绝；模型 400
//!   **不触发**静默删参重试（§13.3）。
//! - **被截断的一轮不构造任何工具调用**：`response.incomplete` 只把
//!   [`Round::truncated`] 置位，把已有文本交回去，让 loop 决定怎么收场。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use komo_kernel::traits::{LlmClient, TurnDriver};
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::model::{Effort, EffortSetting, ModelConfig, ModelRole, TokenUsage};
use komo_kernel::types::tool::ToolDefinition;
use komo_kernel::types::turn::{
    LlmError, ProviderToolCall, ReplayMessage, Role, Round, RoundInput, ToolResultForModel,
    TurnRequest,
};
use serde_json::{Value, json};

use super::codex_auth::{CodexAuthError, CodexTokenSource};
use super::sse::SseDecoder;
use super::transport::{HttpRequest, HttpTransport, TransportError};
use super::wire::{self, EventPayload};
use crate::config::EffortCapabilities;

/// 这个客户端的凭证来源：`.env` 里的一个变量，或者 ChatGPT 账号 OAuth（§13.3）。
///
/// **每次请求都读当前的那份**——`Env` 是构造时就抓死的值（本来就来自不变的环境变量），
/// `ChatGpt` 则在每次 `round_trip` 时才向 [`CodexTokenSource`] 要一次，快过期时它自己
/// 刷新，不在造客户端时抓死。
#[derive(Clone)]
pub enum Credential {
    Env(Option<String>),
    ChatGpt(Arc<CodexTokenSource>),
}

fn codex_auth_error(error: CodexAuthError) -> LlmError {
    match error {
        CodexAuthError::NeedsLogin(message) => LlmError::Rejected {
            status: 401,
            message,
        },
        CodexAuthError::RateLimited(message) => LlmError::Rejected {
            status: 429,
            message,
        },
        CodexAuthError::Io { path, message } => LlmError::Rejected {
            status: 401,
            message: format!("凭证文件 {path}：{message}；跑 `komo auth codex login`"),
        },
        CodexAuthError::Transport(message) => LlmError::Transport(message),
    }
}

/// 记忆注入的接口（§9.4 的注入由 MemoryManager 决定内容，这里只留位置）。
///
/// 它不在 [`TurnRequest`] 里自己拼：`TurnRequest::memories` 是**审计证据**（注入了哪些
/// 条目的哪个版本，§9.7），正文该长什么样是记忆那边的事。
pub trait SystemPreamble: Send + Sync {
    /// 追加在系统提示（`instructions`）之后的一段注入正文；`None` = 这一轮不注入。
    fn preamble(&self, request: &TurnRequest) -> Option<String>;
}

/// 一个讲 Responses API 的后端。
pub struct OpenAiResponsesLlm {
    config: ModelConfig,
    role: ModelRole,
    credential: Credential,
    transport: Arc<dyn HttpTransport>,
    caps: EffortCapabilities,
    preamble: Option<Arc<dyn SystemPreamble>>,
    max_output_tokens: Option<u32>,
}

impl std::fmt::Debug for OpenAiResponsesLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 凭证不进 Debug。
        f.debug_struct("OpenAiResponsesLlm")
            .field("role", &self.role)
            .field("model", &self.config.model)
            .field("base_url", &self.config.base_url)
            .field(
                "has_key",
                &matches!(
                    &self.credential,
                    Credential::Env(Some(_)) | Credential::ChatGpt(_)
                ),
            )
            .finish()
    }
}

impl OpenAiResponsesLlm {
    pub fn new(
        config: ModelConfig,
        role: ModelRole,
        credential: Credential,
        transport: Arc<dyn HttpTransport>,
        caps: EffortCapabilities,
    ) -> Result<Self, LlmError> {
        // §13.3：不支持的档位在**请求前**拒绝。构造时先拒一次，于是一个带着不可用
        // 档位的客户端根本不存在。
        check_effort(&config, &caps)?;
        Ok(OpenAiResponsesLlm {
            config,
            role,
            credential,
            transport,
            caps,
            preamble: None,
            max_output_tokens: None,
        })
    }

    pub fn with_preamble(mut self, preamble: Arc<dyn SystemPreamble>) -> Self {
        self.preamble = Some(preamble);
        self
    }

    /// 单轮输出上限。不设就由服务端决定。
    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    fn endpoint(&self, model: &ModelConfig) -> String {
        format!("{}/responses", model.base_url.trim_end_matches('/'))
    }
}

fn check_effort(config: &ModelConfig, caps: &EffortCapabilities) -> Result<(), LlmError> {
    if let Err(problem) = caps.check(config) {
        tracing::warn!(model = %config.model, %problem, "effort 档位不可用");
        return Err(LlmError::UnsupportedEffort {
            model: config.model.clone(),
            effort: config
                .effort
                .as_ref()
                .map(Effort::to_string)
                .unwrap_or_default(),
        });
    }
    Ok(())
}

#[async_trait]
impl LlmClient for OpenAiResponsesLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        // 本次 Run 固定的是 `req.model`（§6），不是构造这个客户端时那份——热重载不在
        // 半路换模型。
        check_effort(&req.model, &self.caps)?;
        // `Env(None)` 在这里就能判死——它是一个不变的环境变量，不会在下一轮变得有效。
        // `ChatGpt` 那条路每次请求前才取（可能要刷新），留给 `round_trip`。
        if let Credential::Env(None) = &self.credential {
            return Err(LlmError::Rejected {
                status: 401,
                message: format!(
                    "没有凭证：环境里找不到 `{}`（config.toml 里写的是变量名）",
                    req.model.api_key_env
                ),
            });
        }

        // Responses 的系统提示是 `instructions` 一个字段，所以记忆注入接在它后面，
        // 而不是再造一条 role=system 的消息。
        let mut instructions = req.system_prompt.clone();
        if let Some(preamble) = self.preamble.as_ref().and_then(|p| p.preamble(&req)) {
            instructions.push_str("\n\n");
            instructions.push_str(&preamble);
        }

        Ok(Box::new(ResponsesDriver {
            endpoint: self.endpoint(&req.model),
            credential: self.credential.clone(),
            transport: Arc::clone(&self.transport),
            role: self.role,
            model: req.model,
            session: req.session,
            tools: req.tools,
            instructions,
            input: replay(&req.messages),
            max_output_tokens: self.max_output_tokens,
            usage: TokenUsage::default(),
            round: 0,
        }))
    }
}

/// 回放窗口 → `input` items。
///
/// assistant 那一轮**优先用存下来的原始 output items**：reasoning 项、它的
/// `encrypted_content`、函数调用的 `call_id`，只有原样送回去才继续成立；重新拼一个
/// "等价"的消息就是把它们丢了。
fn replay(messages: &[ReplayMessage]) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        match message.role {
            Role::User => input.push(wire::user_message(
                message.text.as_deref().unwrap_or_default(),
            )),
            Role::Assistant => match &message.provider_blocks {
                // 一轮的全部 output items。
                Some(Value::Array(items)) => input.extend(items.iter().cloned()),
                // Chat Completions 的原生 assistant message 不能送进 Responses；跨协议
                // 切模型时从规范化的正文和调用记录重建。
                Some(_) | None => {
                    if let Some(text) = &message.text
                        && !text.is_empty()
                    {
                        input.push(wire::assistant_message(text));
                    }
                    for call in &message.tool_calls {
                        input.push(wire::function_call(
                            &call.provider_call_id,
                            &call.name,
                            &arguments_text(&call.arguments),
                        ));
                    }
                }
            },
            // 工具结果是自己的 item，按 `call_id` 配对。
            Role::Tool => input.extend(message.tool_results.iter().map(tool_output)),
        }
    }
    input
}

fn tool_output(result: &ToolResultForModel) -> Value {
    wire::function_call_output(&result.provider_call_id, &result.content)
}

fn arguments_text(arguments: &Value) -> String {
    match arguments {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// 一次 Run 的一个执行段。
struct ResponsesDriver {
    endpoint: String,
    credential: Credential,
    transport: Arc<dyn HttpTransport>,
    role: ModelRole,
    model: ModelConfig,
    session: SessionId,
    tools: Vec<ToolDefinition>,
    instructions: String,
    /// 到目前为止的全部 items——**回放靠它全量携带**（`store: false`）。
    input: Vec<Value>,
    max_output_tokens: Option<u32>,
    usage: TokenUsage,
    round: u32,
}

#[async_trait]
impl TurnDriver for ResponsesDriver {
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError> {
        if let RoundInput::ToolResults { results } = &input {
            // 紧跟在上一轮的 output items 之后。
            self.input.extend(results.iter().map(tool_output));
        }
        self.round += 1;

        let effort = EffortSetting::from_option(self.model.effort.clone());
        let body = wire::responses_request(
            &self.model,
            &self.instructions,
            &self.input,
            &self.tools,
            effort.as_option(),
            self.max_output_tokens,
        );
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
        // 下一轮要把这一轮的 items 原样送回去。
        if let Some(Value::Array(items)) = &round.provider_blocks {
            self.input.extend(items.iter().cloned());
        }
        accumulate(&mut self.usage, &round.usage);
        Ok(round)
    }

    fn usage(&self) -> TokenUsage {
        self.usage
    }
}

impl ResponsesDriver {
    /// 取这次要用的凭证与请求头（§13.3：每次请求前取，`ChatGpt` 那条路快过期就刷新）。
    async fn credential(&self) -> Result<(String, Vec<(String, String)>), LlmError> {
        match &self.credential {
            Credential::Env(Some(key)) => Ok((key.clone(), Vec::new())),
            Credential::Env(None) => Err(LlmError::Rejected {
                status: 401,
                message: format!(
                    "没有凭证：环境里找不到 `{}`（config.toml 里写的是变量名）",
                    self.model.api_key_env
                ),
            }),
            Credential::ChatGpt(source) => {
                let creds = source.token().await.map_err(codex_auth_error)?;
                Ok((
                    creds.access_token,
                    vec![
                        ("ChatGPT-Account-ID".to_string(), creds.account_id),
                        ("originator".to_string(), "komo".to_string()),
                        ("session_id".to_string(), self.session.to_string()),
                    ],
                ))
            }
        }
    }

    async fn round_trip(&self, body: Value) -> Result<Round, LlmError> {
        let (api_key, extra_headers) = self.credential().await?;
        let mut request = HttpRequest::new(&self.endpoint, body)
            .with_key(Some(api_key))
            .with_timeout(Duration::from_secs(self.model.timeout_secs.max(1)));
        for (name, value) in extra_headers {
            request = request.with_header(name, value);
        }
        let response = self
            .transport
            .post(request)
            .await
            .map_err(transport_error)?;

        let status = response.status;
        if !(200..300).contains(&status) {
            let text = response.text().await.unwrap_or_default();
            // §13.3：模型 400 **不触发**静默删参重试——这里只把拒绝原样报上去，
            // 没有任何一条路径会去掉 `reasoning` 再发一次。
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
                "流结束时没有收到 response.completed"
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
    reasoning_summary: String,
    /// 按 `output_index` 收的函数调用（`output_item.added` 给身份，`arguments.done`
    /// 给完整参数）。
    calls: BTreeMap<usize, PartialCall>,
    /// 按 `output_index` 收的 `output_item.done`。
    items: BTreeMap<usize, Value>,
    /// `response.completed` 里那份权威的 output 数组。
    final_output: Option<Vec<Value>>,
    usage: TokenUsage,
    completed: bool,
    /// `response.incomplete` 的原因。
    incomplete: Option<String>,
    failure: Option<LlmError>,
}

#[derive(Debug, Default)]
struct PartialCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl Assembled {
    /// 这一轮有结论了吗（收齐、截断、失败，三者之一）。
    fn settled(&self) -> bool {
        self.completed || self.failure.is_some()
    }

    fn absorb(&mut self, name: Option<&str>, data: &str) {
        // 一帧解析不了就丢掉这一帧：各家会插心跳和自定义字段，为它们中断整轮是过头的。
        // 真正的"没收齐"由终止事件判定。
        let Ok(payload) = serde_json::from_str::<EventPayload>(data) else {
            tracing::debug!(bytes = data.len(), "跳过一帧解析不了的 SSE 负载");
            return;
        };
        // `event:` 行缺了就用负载自己的 `type`。
        let Some(name) = name.or(payload.kind.as_deref()) else {
            return;
        };
        let index = payload.output_index.unwrap_or(0);

        match name {
            wire::RESPONSE_CREATED => {
                if let Some(id) = payload.response.as_ref().and_then(|r| r.id.as_deref()) {
                    tracing::debug!(response = %id, "response.created");
                }
            }
            wire::OUTPUT_ITEM_ADDED => {
                if let Some(item) = &payload.item
                    && let Some((call_id, call_name)) = wire::function_call_of(item)
                {
                    self.calls.insert(
                        index,
                        PartialCall {
                            call_id,
                            name: call_name,
                            arguments: String::new(),
                        },
                    );
                }
            }
            wire::OUTPUT_TEXT_DELTA => {
                if let Some(delta) = payload.delta {
                    self.text.push_str(&delta);
                }
            }
            wire::OUTPUT_TEXT_DONE => {
                // 增量丢了一段时，这条是权威的全文。
                if self.text.is_empty()
                    && let Some(text) = payload.text
                {
                    self.text = text;
                }
            }
            // 增量只用于进度，不参与拼装——完整参数由 `arguments.done` 给。
            wire::FUNCTION_ARGS_DELTA => {}
            wire::FUNCTION_ARGS_DONE => {
                if let Some(arguments) = payload.arguments {
                    self.calls.entry(index).or_default().arguments = arguments;
                }
            }
            wire::REASONING_SUMMARY_DELTA => {
                if let Some(delta) = payload.delta {
                    self.reasoning_summary.push_str(&delta);
                }
            }
            wire::OUTPUT_ITEM_DONE => {
                if let Some(item) = payload.item {
                    self.items.insert(index, item);
                }
            }
            wire::RESPONSE_COMPLETED => {
                self.completed = true;
                self.take_response(payload);
            }
            wire::RESPONSE_INCOMPLETE => {
                self.completed = true;
                self.incomplete = payload
                    .response
                    .as_ref()
                    .and_then(|r| r.incomplete_details.as_ref())
                    .and_then(|d| d.reason.clone())
                    .or(Some("unknown".into()));
                self.take_response(payload);
            }
            wire::RESPONSE_FAILED => {
                let detail = payload
                    .response
                    .as_ref()
                    .and_then(|r| r.error.clone())
                    .unwrap_or_default();
                self.failure = Some(LlmError::Rejected {
                    status: 502,
                    message: format!("response.failed：{}", detail.to_line()),
                });
            }
            wire::ERROR => {
                let message = payload.message.unwrap_or_else(|| data.to_string());
                let code = payload
                    .code
                    .map(|c| format!("（code={c}）"))
                    .unwrap_or_default();
                self.failure = Some(LlmError::Rejected {
                    status: 500,
                    message: format!("{message}{code}"),
                });
            }
            _ => {}
        }
    }

    fn take_response(&mut self, payload: EventPayload) {
        let Some(response) = payload.response else {
            return;
        };
        tracing::debug!(
            status = response.status.as_deref().unwrap_or("?"),
            items = response.output.as_ref().map(Vec::len).unwrap_or(0),
            "response 终态"
        );
        if let Some(usage) = response.usage {
            self.usage = usage.to_kernel();
        }
        if let Some(output) = response.output {
            self.final_output = Some(output);
        }
    }

    fn finish(self, round: u32) -> Result<Round, LlmError> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }

        // 这一轮的 items：`response.completed` 给的那份最权威，其次是逐条收到的
        // `output_item.done`。
        let mut items: Vec<Value> = match self.final_output {
            Some(output) if !output.is_empty() => output,
            _ => self.items.into_values().collect(),
        };

        // §6：被截断的一轮**不构造任何工具调用**——参数未收齐时不能开始执行。
        if let Some(reason) = &self.incomplete {
            // `max_output_tokens` 是"写满了"，别的原因（内容过滤一类）是"被拦了"——
            // 对 loop 都是同一件事：这一轮不完整，不能按半轮调用执行。
            let hit_budget = reason == wire::REASON_MAX_OUTPUT_TOKENS;
            tracing::warn!(
                reason = %reason,
                hit_budget,
                "response.incomplete：这一轮被截断"
            );
            items.retain(|item| wire::function_call_of(item).is_none());
            return Ok(Round {
                round,
                text: (!self.text.is_empty()).then_some(self.text),
                tool_calls: Vec::new(),
                provider_blocks: Some(Value::Array(items)),
                usage: self.usage,
                truncated: true,
            });
        }

        // 调用从 items 里取（它带着服务端最终确认的 call_id / name / arguments），
        // items 里没有时退回逐帧收到的那份。
        let mut tool_calls = Vec::new();
        let from_items: Vec<(String, String, String)> = items
            .iter()
            .filter_map(|item| {
                let (call_id, name) = wire::function_call_of(item)?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Some((call_id, name, arguments))
            })
            .collect();
        let streamed: Vec<(String, String, String)> = self
            .calls
            .into_values()
            .map(|call| (call.call_id, call.name, call.arguments))
            .collect();
        let sourced = if from_items.is_empty() {
            // 没有 items 就自己拼一份，回放才不会缺了这几条。
            for (call_id, name, arguments) in &streamed {
                items.push(wire::function_call(call_id, name, arguments));
            }
            streamed
        } else {
            from_items
        };

        for (call_id, name, arguments) in sourced {
            // 参数是逐段到的 JSON 文本；拼不成 JSON 就是没收齐——不能按半轮调用执行。
            let parsed: Value = if arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&arguments).map_err(|_| LlmError::Incomplete)?
            };
            tool_calls.push(ProviderToolCall {
                provider_call_id: call_id,
                name,
                arguments: parsed,
            });
        }

        // 没有任何 item 的一轮（纯文本且服务端没给 output）也要留下点回放得了的东西。
        if items.is_empty() && !self.text.is_empty() {
            items.push(wire::assistant_message(&self.text));
        }

        Ok(Round {
            round,
            text: (!self.text.is_empty()).then_some(self.text),
            tool_calls,
            // §13.2：协议回放所需的消息块，**原样**——reasoning 项和它的
            // encrypted_content 就在里面。
            provider_blocks: Some(Value::Array(items)),
            usage: self.usage,
            truncated: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::{Seq, ToolCallId};
    use komo_kernel::types::turn::ToolCallRequest;

    fn assemble(frames: &[(&str, &str)]) -> Result<Round, LlmError> {
        let mut assembled = Assembled::default();
        for (name, data) in frames {
            assembled.absorb(Some(name), data);
        }
        if !assembled.settled() {
            return Err(LlmError::Incomplete);
        }
        assembled.finish(1)
    }

    const COMPLETED_WITH_CALL: &str = r#"{"response":{"status":"completed","output":[
        {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"ENC"},
        {"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.txt\"}"}
    ],"usage":{"input_tokens":5,"output_tokens":2,"output_tokens_details":{"reasoning_tokens":1}}}}"#;

    #[test]
    fn a_function_call_is_assembled_with_its_call_id_and_parsed_arguments() {
        let round = assemble(&[
            (
                wire::OUTPUT_ITEM_ADDED,
                r#"{"output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"read","arguments":""}}"#,
            ),
            (
                wire::FUNCTION_ARGS_DELTA,
                r#"{"output_index":0,"delta":"{\"path\":"}"#,
            ),
            (
                wire::FUNCTION_ARGS_DONE,
                r#"{"output_index":0,"arguments":"{\"path\":\"a.txt\"}"}"#,
            ),
            (wire::RESPONSE_COMPLETED, COMPLETED_WITH_CALL),
        ])
        .unwrap();

        assert_eq!(round.tool_calls.len(), 1);
        assert_eq!(round.tool_calls[0].provider_call_id, "call_1");
        assert_eq!(round.tool_calls[0].name, "read");
        assert_eq!(round.tool_calls[0].arguments["path"], json!("a.txt"));
        assert_eq!(round.usage.input, Some(5));
        assert_eq!(round.usage.reasoning, Some(1));
        assert!(!round.truncated);
    }

    /// §13.2：reasoning 项（含 `encrypted_content`）原样进 `provider_blocks`。
    #[test]
    fn the_reasoning_item_is_carried_back_verbatim() {
        let round = assemble(&[(wire::RESPONSE_COMPLETED, COMPLETED_WITH_CALL)]).unwrap();
        let Some(Value::Array(items)) = &round.provider_blocks else {
            panic!("{:?}", round.provider_blocks)
        };
        assert_eq!(items[0]["type"], json!("reasoning"));
        assert_eq!(items[0]["encrypted_content"], json!("ENC"));
        assert_eq!(items[1]["call_id"], json!("call_1"));
    }

    #[test]
    fn text_deltas_become_the_rounds_text() {
        let round = assemble(&[
            (wire::OUTPUT_TEXT_DELTA, r#"{"output_index":0,"delta":"好"}"#),
            (wire::OUTPUT_TEXT_DELTA, r#"{"output_index":0,"delta":"的"}"#),
            (
                wire::RESPONSE_COMPLETED,
                r#"{"response":{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"好的"}]}]}}"#,
            ),
        ])
        .unwrap();
        assert_eq!(round.text.as_deref(), Some("好的"));
        assert!(round.tool_calls.is_empty());
    }

    #[test]
    fn a_lost_delta_is_recovered_from_output_text_done() {
        let round = assemble(&[
            (
                wire::OUTPUT_TEXT_DONE,
                r#"{"output_index":0,"text":"全文"}"#,
            ),
            (wire::RESPONSE_COMPLETED, r#"{"response":{"output":[]}}"#),
        ])
        .unwrap();
        assert_eq!(round.text.as_deref(), Some("全文"));
    }

    #[test]
    fn half_an_argument_object_is_incomplete_not_an_empty_call() {
        let error = assemble(&[
            (
                wire::OUTPUT_ITEM_ADDED,
                r#"{"output_index":0,"item":{"type":"function_call","call_id":"c","name":"read"}}"#,
            ),
            (
                wire::FUNCTION_ARGS_DONE,
                r#"{"output_index":0,"arguments":"{\"path\":"}"#,
            ),
            (wire::RESPONSE_COMPLETED, r#"{"response":{"output":[]}}"#),
        ])
        .unwrap_err();
        assert_eq!(error, LlmError::Incomplete);
    }

    /// 没有 `response.completed` 就是没收齐。
    #[test]
    fn a_stream_without_its_terminal_event_is_incomplete() {
        let error = assemble(&[(wire::OUTPUT_TEXT_DELTA, r#"{"delta":"半句"}"#)]).unwrap_err();
        assert_eq!(error, LlmError::Incomplete);
    }

    /// `response.incomplete` → 截断，**不构造任何工具调用**。
    #[test]
    fn an_incomplete_response_truncates_and_builds_no_calls() {
        let round = assemble(&[
            (
                wire::OUTPUT_ITEM_ADDED,
                r#"{"output_index":0,"item":{"type":"function_call","call_id":"c","name":"read"}}"#,
            ),
            (wire::OUTPUT_TEXT_DELTA, r#"{"delta":"写到一半"}"#),
            (
                wire::RESPONSE_INCOMPLETE,
                r#"{"response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"function_call","call_id":"c","name":"read","arguments":"{\"pa"}],"usage":{"input_tokens":9,"output_tokens":100}}}"#,
            ),
        ])
        .unwrap();

        assert!(round.truncated, "provider 说这次回复被截断了（§6）");
        assert!(round.tool_calls.is_empty(), "半轮调用不能执行");
        assert_eq!(round.text.as_deref(), Some("写到一半"));
        assert_eq!(round.usage.output, Some(100), "用量照记");
        let Some(Value::Array(items)) = &round.provider_blocks else {
            panic!()
        };
        assert!(
            items
                .iter()
                .all(|item| wire::function_call_of(item).is_none()),
            "没收齐的调用不进回放：{items:?}"
        );
    }

    #[test]
    fn a_failed_response_is_an_error_not_an_empty_round() {
        let error = assemble(&[(
            wire::RESPONSE_FAILED,
            r#"{"response":{"status":"failed","error":{"code":"server_error","message":"上游炸了"}}}"#,
        )])
        .unwrap_err();
        let LlmError::Rejected { status, message } = &error else {
            panic!("{error:?}")
        };
        assert_eq!(*status, 502);
        assert!(message.contains("上游炸了"), "{message}");
    }

    #[test]
    fn a_top_level_error_event_ends_the_round() {
        let error = assemble(&[(
            wire::ERROR,
            r#"{"type":"error","code":"rate_limit_exceeded","message":"慢一点"}"#,
        )])
        .unwrap_err();
        assert!(
            matches!(&error, LlmError::Rejected { message, .. } if message.contains("慢一点")),
            "{error:?}"
        );
    }

    #[test]
    fn an_unparseable_frame_is_skipped_rather_than_failing_the_round() {
        let round = assemble(&[
            ("response.output_text.delta", "{not json}"),
            (wire::OUTPUT_TEXT_DELTA, r#"{"delta":"ok"}"#),
            (wire::RESPONSE_COMPLETED, r#"{"response":{"output":[]}}"#),
        ])
        .unwrap();
        assert_eq!(round.text.as_deref(), Some("ok"));
    }

    #[test]
    fn items_are_rebuilt_from_the_stream_when_the_server_sends_no_output_array() {
        let round = assemble(&[
            (
                wire::OUTPUT_ITEM_ADDED,
                r#"{"output_index":0,"item":{"type":"function_call","call_id":"call_9","name":"shell"}}"#,
            ),
            (
                wire::FUNCTION_ARGS_DONE,
                r#"{"output_index":0,"arguments":"{}"}"#,
            ),
            (wire::RESPONSE_COMPLETED, r#"{"response":{}}"#),
        ])
        .unwrap();
        assert_eq!(round.tool_calls[0].provider_call_id, "call_9");
        let Some(Value::Array(items)) = &round.provider_blocks else {
            panic!()
        };
        assert_eq!(items[0]["type"], json!("function_call"), "回放里要有这一条");
    }

    // ---- 回放 ----

    #[test]
    fn a_stored_output_item_array_is_replayed_verbatim() {
        let items = json!([
            { "type": "reasoning", "id": "rs_1", "encrypted_content": "ENC" },
            { "type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{}" }
        ]);
        let replayed = replay(&[ReplayMessage {
            role: Role::Assistant,
            seq: Seq(1),
            text: Some("被忽略的重拼版本".into()),
            tool_calls: vec![],
            tool_results: vec![],
            provider_blocks: Some(items.clone()),
        }]);
        assert_eq!(replayed, items.as_array().unwrap().clone());
    }

    #[test]
    fn an_assistant_without_stored_items_is_rebuilt_as_message_plus_function_call() {
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
        assert_eq!(replayed[0]["type"], json!("message"));
        assert_eq!(replayed[0]["role"], json!("assistant"));
        assert_eq!(replayed[1]["type"], json!("function_call"));
        assert_eq!(replayed[1]["call_id"], json!("call_1"));
        assert_eq!(replayed[1]["arguments"], json!("{\"path\":\"a.txt\"}"));
    }

    #[test]
    fn a_user_message_and_tool_results_become_their_own_items() {
        let replayed = replay(&[
            ReplayMessage {
                role: Role::User,
                seq: Seq(1),
                text: Some("看看这个文件".into()),
                tool_calls: vec![],
                tool_results: vec![],
                provider_blocks: None,
            },
            ReplayMessage {
                role: Role::Tool,
                seq: Seq(2),
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
            },
        ]);
        assert_eq!(replayed[0]["content"][0]["type"], json!("input_text"));
        assert_eq!(replayed[1]["type"], json!("function_call_output"));
        assert_eq!(replayed[1]["call_id"], json!("call_1"));
        assert_eq!(replayed[2]["output"], json!("boom"));
    }
}
