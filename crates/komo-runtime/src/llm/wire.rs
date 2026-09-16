//! Chat Completions 的线格式（§13.2「首版生成模型接入一种明确的协议」）。
//!
//! 只有**一种**协议：OpenAI 兼容的 `/chat/completions`，流式 + tool calling。它不是
//! "所有字段都兼容"的保证（§13.2 明说不这么当），所以这里写的是这个协议自己的字段，
//! 新协议是新模块而不是新分支。
//!
//! 请求侧手写而不是 `Serialize` 一个结构：`reasoning_effort` 在**未配置时必须整个字段
//! 不出现**（§13.3），`skip_serializing_if` 表达得了，但把"不发"这件事和别的可选字段
//! 混在一起容易在后来被谁加一个默认值填上。

use komo_kernel::types::model::{Effort, ModelConfig, TokenUsage};
use komo_kernel::types::tool::ToolDefinition;
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// 一次请求体。
pub fn chat_request(
    config: &ModelConfig,
    messages: &[Value],
    tools: &[ToolDefinition],
    effort: Option<&Effort>,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(config.model));
    body.insert("messages".into(), Value::Array(messages.to_vec()));
    body.insert("stream".into(), json!(true));
    // 用量只在最后一帧里给；不要它就只能把这次消耗当成零，而"未知不是零"（§8.5）。
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    if !tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(tools.iter().map(tool_schema).collect()),
        );
    }
    // §13.3：未配置 = **不发送这个字段**，由服务端采用默认行为。
    if let Some(effort) = effort {
        body.insert("reasoning_effort".into(), json!(effort.as_str()));
    }
    Value::Object(body)
}

fn tool_schema(tool: &ToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        }
    })
}

// ---------------------------------------------------------------- 流式响应

/// 一帧 `data:` 的负载。字段全是可选的：各家实现给的子集不同，缺哪个都不该让整轮失败。
#[derive(Debug, Default, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    /// DeepSeek 一类把思维链单独放这里；**原样带回**才能让回放继续成立（§13.2）。
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Default, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    /// 逐段到达的参数 JSON 文本。
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionDetails>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub struct CompletionDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    pub fn to_kernel(self) -> TokenUsage {
        TokenUsage {
            input: self.prompt_tokens,
            output: self.completion_tokens,
            reasoning: self
                .completion_tokens_details
                .and_then(|details| details.reasoning_tokens),
        }
    }
}

/// 非 2xx 时服务端给的错误正文——能解析就把 `message` 带给操作者，解析不了就用原文。
#[derive(Debug, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Deserialize)]
pub struct ErrorDetail {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
}

/// 把服务端的错误正文压成一行。
pub fn error_message(status: u16, body: &str) -> String {
    match serde_json::from_str::<ErrorBody>(body) {
        Ok(parsed) => match parsed.error.code {
            Some(code) => format!("{}（code={code}）", parsed.error.message),
            None => parsed.error.message,
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(effort: Option<&str>) -> ModelConfig {
        ModelConfig {
            provider: "openai_compatible".into(),
            base_url: "https://x/v1".into(),
            model: "m".into(),
            api_key_env: "K".into(),
            effort: effort.map(Effort::new),
            timeout_secs: 30,
        }
    }

    #[test]
    fn an_unset_effort_leaves_the_field_out_entirely() {
        let body = chat_request(&config(None), &[], &[], None);
        assert!(
            body.get("reasoning_effort").is_none(),
            "未配置就不该出现这个键：{body}"
        );
    }

    #[test]
    fn an_explicit_effort_is_sent_as_reasoning_effort() {
        let effort = Effort::new("high");
        let body = chat_request(&config(Some("high")), &[], &[], Some(&effort));
        assert_eq!(body["reasoning_effort"], json!("high"));
    }

    #[test]
    fn tools_are_omitted_when_there_are_none() {
        let body = chat_request(&config(None), &[], &[], None);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn usage_maps_reasoning_tokens_and_keeps_unknown_unknown() {
        let usage = Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(3),
            completion_tokens_details: Some(CompletionDetails {
                reasoning_tokens: Some(7),
            }),
        };
        assert_eq!(usage.to_kernel().reasoning, Some(7));
        assert!(Usage::default().to_kernel().is_unknown());
    }

    #[test]
    fn a_server_error_body_becomes_one_line() {
        let text =
            r#"{"error":{"message":"unsupported reasoning_effort","code":"invalid_request"}}"#;
        let line = error_message(400, text);
        assert!(line.contains("unsupported reasoning_effort"), "{line}");
        assert!(line.contains("invalid_request"), "{line}");
        assert_eq!(error_message(500, ""), "HTTP 500");
    }
}
