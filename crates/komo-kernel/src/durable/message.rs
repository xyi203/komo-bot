//! 模型消息（pi-ai 的 `Message` 形状），entry 的 `model` 与 `edits.replace` 都存它。
//!
//! 旧的 `session::entry` 把消息当 `serde_json::Value` 存，没有可供推导上下文用的类型，
//! 所以这里新定义。推导只需要认得 role、内容块的类型、tool call / result 的配对键、
//! `stopReason`；其余字段（api、provider、usage、签名、provider 原生块……）一律
//! 落进 `extra` 原样来回，不改任何字节。图片块在 komo 里是 `{mime, path, hash}` 引用
//! （§3.5），也只是一个带 `extra` 的 `image` 块。

use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// 类型之外的字段，原样保留。
pub type Extra = Map<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    System(SystemMessage),
}

impl Message {
    pub fn is_system(&self) -> bool {
        matches!(self, Message::System(_))
    }

    pub fn is_user(&self) -> bool {
        matches!(self, Message::User(_))
    }

    pub fn user(text: impl Into<String>, timestamp: i64) -> Self {
        Message::User(UserMessage {
            content: Content::Text(text.into()),
            timestamp,
            extra: Extra::new(),
        })
    }

    pub fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason, timestamp: i64) -> Self {
        Message::Assistant(AssistantMessage {
            content,
            stop_reason,
            timestamp,
            extra: Extra::new(),
        })
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        text: impl Into<String>,
        timestamp: i64,
    ) -> Self {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::text(text)],
            is_error: false,
            details: None,
            timestamp,
            extra: Extra::new(),
        })
    }

    /// 带 sections 补丁的 system 消息（`content` 恒为空串，§2）。
    pub fn system(sections: Sections, timestamp: i64) -> Self {
        Message::System(SystemMessage {
            content: Content::Text(String::new()),
            sections: Some(sections),
            tools_added: None,
            tools_removed: None,
            timestamp,
            extra: Extra::new(),
        })
    }
}

/// user / system 的内容：一段文本，或一串内容块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
        extra: Extra,
    },
    /// 推理块；签名等 provider 原生字段在 `extra`。
    Thinking {
        thinking: String,
        #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
        extra: Extra,
    },
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        arguments: Value,
        #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
        extra: Extra,
    },
    /// 图片引用 `{mime, path, hash}`，字节不进记录。
    Image {
        #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
        extra: Extra,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text {
            text: text.into(),
            extra: Extra::new(),
        }
    }

    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        ContentBlock::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            extra: Extra::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: Content,
    #[serde(default)]
    pub timestamp: i64,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

/// user 消息 `extra` 里标"这不是用户说的，是宿主附上的"的那一格（值是来源，如 `memory`）。
/// 发给模型时它照样是 user 角色（Anthropic 不收对话中间的 system 消息）；代码找"最近的用户输入"、
/// 压缩挑切点、序列化摘要材料时靠它跳过这条（§9.4）。
pub const HOST_ORIGIN_KEY: &str = "komoOrigin";

impl UserMessage {
    /// 宿主附上的来源；用户自己的输入是 `None`。
    pub fn host_origin(&self) -> Option<&str> {
        self.extra.get(HOST_ORIGIN_KEY).and_then(Value::as_str)
    }
}

impl Message {
    /// 宿主附上的 user 消息（[`HOST_ORIGIN_KEY`]）。
    pub fn is_host_injected(&self) -> bool {
        matches!(self, Message::User(user) if user.host_origin().is_some())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    #[serde(default)]
    pub timestamp: i64,
    /// api / provider / model / usage / errorMessage …
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

impl AssistantMessage {
    /// 这条消息发起的工具调用，按调用顺序。
    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str)> {
        self.content.iter().filter_map(|block| match block {
            ContentBlock::ToolCall { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default)]
    pub timestamp: i64,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

/// 位置化的 system 消息：按 transcript 次序重放所有 system 消息得到当前提示与工具集。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    pub content: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Sections>,
    /// 工具定义（provider 无关的 JSON），先删后加。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_added: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_removed: Option<Vec<Value>>,
    #[serde(default)]
    pub timestamp: i64,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

impl StopReason {
    /// `aborted` / `error` / `deferred` 的 assistant 消息不再进后续请求（§2.1 规则 9）。
    pub fn excluded_from_context(self) -> bool {
        matches!(
            self,
            StopReason::Aborted | StopReason::Error | StopReason::Deferred
        )
    }
}

/// 有序的具名段补丁：字符串 = 新增或替换，`None`（JSON `null`）= 删除。
///
/// 次序有意义，而 `serde_json::Map` 在没开 `preserve_order` 时按键排序，所以自己存一个
/// `Vec`，序列化成 JSON 对象、反序列化时按文档次序读。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sections(pub Vec<(String, Option<String>)>);

impl Sections {
    pub fn set(mut self, name: impl Into<String>, text: impl Into<String>) -> Self {
        self.0.push((name.into(), Some(text.into())));
        self
    }

    pub fn remove(mut self, name: impl Into<String>) -> Self {
        self.0.push((name.into(), None));
        self
    }
}

impl Serialize for Sections {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, text) in &self.0 {
            map.serialize_entry(name, text)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Sections {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SectionsVisitor;

        impl<'de> Visitor<'de> for SectionsVisitor {
            type Value = Sections;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of section name to string or null")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Sections, A::Error> {
                let mut sections = Vec::new();
                while let Some(entry) = access.next_entry::<String, Option<String>>()? {
                    sections.push(entry);
                }
                Ok(Sections(sections))
            }
        }

        deserializer.deserialize_map(SectionsVisitor)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn provider_native_fields_survive_a_round_trip_byte_for_byte() {
        let raw = json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "hm", "thinkingSignature": "sig"},
                {"type": "text", "text": "hi", "textSignature": "t"},
                {"type": "toolCall", "id": "c1", "name": "read", "arguments": {"path": "a"}, "thoughtSignature": "x"},
            ],
            "stopReason": "toolUse",
            "timestamp": 5,
            "api": "anthropic-messages",
            "provider": "anthropic",
            "usage": {"input": 1},
        });
        let message: Message = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&message).unwrap(), raw);
        let Message::Assistant(assistant) = &message else {
            panic!("assistant expected");
        };
        assert_eq!(assistant.tool_calls().collect::<Vec<_>>(), [("c1", "read")]);
    }

    #[test]
    fn user_content_is_a_string_or_blocks_and_images_are_references() {
        let text: Message =
            serde_json::from_value(json!({"role": "user", "content": "yo", "timestamp": 1}))
                .unwrap();
        let blocks = json!({
            "role": "user",
            "content": [{"type": "image", "mime": "image/png", "path": "images/a.png", "hash": "ab"}],
            "timestamp": 2,
        });
        let image: Message = serde_json::from_value(blocks.clone()).unwrap();
        assert!(text.is_user());
        assert_eq!(serde_json::to_value(&image).unwrap(), blocks);
    }

    #[test]
    fn system_sections_keep_their_order_and_null_means_remove() {
        let raw = r#"{"role":"system","content":"","sections":{"z":"last-alpha","a":null,"m":"mid"},"toolsRemoved":[{"name":"x"}],"timestamp":3}"#;
        let message: Message = serde_json::from_str(raw).unwrap();
        let Message::System(system) = &message else {
            panic!("system expected");
        };
        let sections = system.sections.as_ref().unwrap();
        assert_eq!(
            sections,
            &Sections::default()
                .set("z", "last-alpha")
                .remove("a")
                .set("m", "mid")
        );
        assert_eq!(serde_json::to_string(&message).unwrap(), raw);
    }

    #[test]
    fn tool_result_decodes_with_pi_field_names() {
        let raw = json!({
            "role": "toolResult", "toolCallId": "c1", "toolName": "bash",
            "content": [{"type": "text", "text": "ok"}], "isError": false, "timestamp": 4,
        });
        let message: Message = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&message).unwrap(), raw);
    }
}
