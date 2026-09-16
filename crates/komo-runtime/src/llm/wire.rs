//! OpenAI **Responses API** 的线格式（§13.2）。
//!
//! 一个协议，一处线格式：请求是 `POST {base_url}/responses`，回复是一串带 `event:` 名
//! 字的 SSE 帧。这里只有这个协议自己的字段——新协议是新模块，不是这里的新分支。
//!
//! 请求体手写而不是 `Serialize` 一个结构，理由和之前一样：`reasoning` 在**未配置
//! effort 时必须整个字段不出现**（§13.3），而这件事和别的可选字段混在一起，迟早会被谁
//! 加一个默认值填上。

use komo_kernel::types::model::{Effort, ModelConfig, TokenUsage};
use komo_kernel::types::tool::ToolDefinition;
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// `store: false` 下也要能回放 reasoning，就得让服务端把它加密后给我们。
pub const INCLUDE_ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";

/// 一次请求体。
pub fn responses_request(
    config: &ModelConfig,
    instructions: &str,
    input: &[Value],
    tools: &[ToolDefinition],
    effort: Option<&Effort>,
    max_output_tokens: Option<u32>,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(config.model));
    body.insert("instructions".into(), json!(instructions));
    body.insert("input".into(), Value::Array(input.to_vec()));
    body.insert("stream".into(), json!(true));
    // **不依赖服务端保存**：回放靠 `input` 全量携带，所以也不用 `previous_response_id`。
    body.insert("store".into(), json!(false));
    // 于是 reasoning 项要连 `encrypted_content` 一起拿回来，下一轮才回放得了。
    body.insert("include".into(), json!([INCLUDE_ENCRYPTED_REASONING]));
    if !tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(tools.iter().map(tool_schema).collect()),
        );
        body.insert("tool_choice".into(), json!("auto"));
    }
    // §13.3：未配置 = **不发送这个字段**，由服务端采用默认行为。
    if let Some(effort) = effort {
        body.insert("reasoning".into(), json!({ "effort": effort.as_str() }));
    }
    if let Some(max_output_tokens) = max_output_tokens {
        body.insert("max_output_tokens".into(), json!(max_output_tokens));
    }
    Value::Object(body)
}

/// Responses 的函数工具是**扁的**：`name` / `description` / `parameters` 直接挂在
/// item 上，不像 Chat Completions 那样再包一层 `function`。
fn tool_schema(tool: &ToolDefinition) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    })
}

// ---------------------------------------------------------------- input items

/// 用户消息。
pub fn user_message(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": text }],
    })
}

/// assistant 文本消息（只在没有原始 output items 可回放时才自己拼）。
pub fn assistant_message(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": text }],
    })
}

/// 一次函数调用（回放时用）。
pub fn function_call(call_id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    })
}

/// 一次函数调用的结果。按 `call_id` 配对，紧跟在那一轮的 output items 之后。
pub fn function_call_output(call_id: &str, output: &str) -> Value {
    json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output,
    })
}

// ---------------------------------------------------------------- SSE 事件

/// 事件名。终止帧是 [`RESPONSE_COMPLETED`]——**没收到它就是没收齐**。
pub const RESPONSE_CREATED: &str = "response.created";
pub const OUTPUT_ITEM_ADDED: &str = "response.output_item.added";
pub const OUTPUT_ITEM_DONE: &str = "response.output_item.done";
pub const OUTPUT_TEXT_DELTA: &str = "response.output_text.delta";
pub const OUTPUT_TEXT_DONE: &str = "response.output_text.done";
pub const FUNCTION_ARGS_DELTA: &str = "response.function_call_arguments.delta";
pub const FUNCTION_ARGS_DONE: &str = "response.function_call_arguments.done";
pub const REASONING_SUMMARY_DELTA: &str = "response.reasoning_summary_text.delta";
pub const RESPONSE_COMPLETED: &str = "response.completed";
pub const RESPONSE_FAILED: &str = "response.failed";
pub const RESPONSE_INCOMPLETE: &str = "response.incomplete";
pub const ERROR: &str = "error";

/// 一帧的负载。字段全是可选：各家给的子集不同，缺哪个都不该让整轮失败。
#[derive(Debug, Default, Deserialize)]
pub struct EventPayload {
    /// 帧自己也带 `type`，与 `event:` 行同名；`event:` 行缺了就用它。
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub output_index: Option<usize>,
    /// `output_item.added` / `output_item.done` 的那个 item。
    #[serde(default)]
    pub item: Option<Value>,
    /// 文本 / 参数 / 摘要的增量。
    #[serde(default)]
    pub delta: Option<String>,
    /// `output_text.done` 的全文。
    #[serde(default)]
    pub text: Option<String>,
    /// `function_call_arguments.done` 的完整参数串。
    #[serde(default)]
    pub arguments: Option<String>,
    /// `response.created` / `completed` / `failed` / `incomplete` 里的那个 response。
    #[serde(default)]
    pub response: Option<ResponseBody>,
    /// 顶层 `error` 事件。
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResponseBody {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// 本轮全部 output items——**这是回放要原样带回去的那一份**。
    #[serde(default)]
    pub output: Option<Vec<Value>>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub error: Option<ErrorDetail>,
    #[serde(default)]
    pub incomplete_details: Option<IncompleteDetails>,
}

#[derive(Debug, Default, Deserialize)]
pub struct IncompleteDetails {
    #[serde(default)]
    pub reason: Option<String>,
}

/// §6：「provider 说这次回复被截断了」。
pub const REASON_MAX_OUTPUT_TOKENS: &str = "max_output_tokens";

#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens_details: Option<OutputTokenDetails>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub struct OutputTokenDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    pub fn to_kernel(self) -> TokenUsage {
        TokenUsage {
            input: self.input_tokens,
            output: self.output_tokens,
            reasoning: self
                .output_tokens_details
                .and_then(|details| details.reasoning_tokens),
        }
    }
}

/// 非 2xx 时服务端给的错误正文。
#[derive(Debug, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ErrorDetail {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

impl ErrorDetail {
    /// 压成一行。`code` 与 `type` 都是服务端给的定位信息，有哪个用哪个。
    pub fn to_line(&self) -> String {
        match (&self.code, &self.kind) {
            (Some(code), _) => format!("{}（code={code}）", self.message),
            (None, Some(kind)) => format!("{}（type={kind}）", self.message),
            (None, None) => self.message.clone(),
        }
    }
}

/// 把服务端的错误正文压成一行。
pub fn error_message(status: u16, body: &str) -> String {
    match serde_json::from_str::<ErrorBody>(body) {
        Ok(parsed) => parsed.error.to_line(),
        Err(_) => {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                format!("HTTP {status}")
            } else {
                trimmed.chars().take(400).collect()
            }
        }
    }
}

/// 一个 item 是不是函数调用；是就给出 `call_id` 与 `name`。
pub fn function_call_of(item: &Value) -> Option<(String, String)> {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }
    let call_id = item.get("call_id").and_then(Value::as_str)?.to_string();
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((call_id, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(effort: Option<&str>) -> ModelConfig {
        ModelConfig {
            provider: "openai_responses".into(),
            base_url: "https://x/v1".into(),
            model: "m".into(),
            api_key_env: "K".into(),
            effort: effort.map(Effort::new),
            efforts: None,
            timeout_secs: 30,
        }
    }

    #[test]
    fn an_unset_effort_leaves_the_reasoning_field_out_entirely() {
        let body = responses_request(&config(None), "sys", &[], &[], None, None);
        assert!(
            body.get("reasoning").is_none(),
            "未配置就不该出现这个键：{body}"
        );
    }

    #[test]
    fn an_explicit_effort_is_sent_as_reasoning_effort() {
        let effort = Effort::new("high");
        let body = responses_request(&config(Some("high")), "sys", &[], &[], Some(&effort), None);
        assert_eq!(body["reasoning"], json!({ "effort": "high" }));
    }

    #[test]
    fn every_request_asks_for_encrypted_reasoning_and_keeps_nothing_on_the_server() {
        let body = responses_request(&config(None), "sys", &[], &[], None, None);
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["instructions"], json!("sys"));
        assert!(body.get("previous_response_id").is_none());
    }

    #[test]
    fn tools_are_flat_and_omitted_when_there_are_none() {
        let body = responses_request(&config(None), "sys", &[], &[], None, None);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());

        let tool = ToolDefinition {
            name: "read".into(),
            description: "读文件".into(),
            parameters: json!({ "type": "object" }),
        };
        let body = responses_request(&config(None), "sys", &[], &[tool], None, None);
        assert_eq!(body["tools"][0]["type"], json!("function"));
        assert_eq!(
            body["tools"][0]["name"],
            json!("read"),
            "名字在 item 上，不再包一层 function"
        );
        assert_eq!(body["tool_choice"], json!("auto"));
    }

    #[test]
    fn max_output_tokens_is_optional() {
        let body = responses_request(&config(None), "sys", &[], &[], None, Some(2048));
        assert_eq!(body["max_output_tokens"], json!(2048));
        assert!(
            responses_request(&config(None), "sys", &[], &[], None, None)
                .get("max_output_tokens")
                .is_none()
        );
    }

    #[test]
    fn usage_maps_reasoning_tokens_and_keeps_unknown_unknown() {
        let usage: Usage = serde_json::from_value(json!({
            "input_tokens": 10,
            "output_tokens": 3,
            "output_tokens_details": { "reasoning_tokens": 7 }
        }))
        .unwrap();
        assert_eq!(usage.to_kernel().reasoning, Some(7));
        assert!(Usage::default().to_kernel().is_unknown());
    }

    #[test]
    fn a_server_error_body_becomes_one_line() {
        let text = r#"{"error":{"message":"unsupported reasoning.effort","code":"invalid_request_error"}}"#;
        let line = error_message(400, text);
        assert!(line.contains("unsupported reasoning.effort"), "{line}");
        assert!(line.contains("invalid_request_error"), "{line}");
        assert_eq!(error_message(500, ""), "HTTP 500");
    }

    #[test]
    fn a_function_call_item_gives_up_its_call_id_and_name() {
        let item = json!({
            "type": "function_call",
            "call_id": "fc_1",
            "name": "read",
            "arguments": "{}"
        });
        assert_eq!(
            function_call_of(&item),
            Some(("fc_1".to_string(), "read".to_string()))
        );
        assert_eq!(function_call_of(&json!({ "type": "reasoning" })), None);
    }
}
