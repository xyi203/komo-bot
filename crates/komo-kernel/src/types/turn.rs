//! 一轮模型往返所需的值类型（§6、§8.5、§13.5）。

use serde::{Deserialize, Serialize};

use super::ids::{MemoryId, RunId, Seq, SessionId, ToolCallId};
use super::model::{ModelConfig, TokenUsage};
use super::refs::PayloadRef;
use super::tool::{ToolDefinition, ToolImage};

/// 回放面上一个节点的角色。
///
/// 交替不变量说的是**两边**交替，不是三个角色轮流：工具结果在 provider 那边是用户侧
/// 的消息（OpenAI 的 `role="tool"`、Anthropic 的 user + tool_result 块），所以
/// `assistant → tool → assistant` 是合法的，`assistant → assistant` 不是。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// 一轮调用的结果。回放时并进用户侧的一条消息。
    Tool,
}

impl Role {
    /// 在 provider 的回放里算不算用户侧。
    pub fn is_user_side(self) -> bool {
        !matches!(self, Role::Assistant)
    }
}

/// 模型回复里的一次工具调用。工具名与参数来自 provider 原生字段，不从自然语言或
/// 代码块推断（§6）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    /// Runtime 分配的内部 ID。
    pub call_id: ToolCallId,
    /// provider 自己的 call_id，回放时按它配对。
    pub provider_call_id: String,
    pub name: String,
    /// 内联参数；超过 4 KiB 时外置，见 `arguments_ref`（§8.3）。
    #[serde(default)]
    pub arguments: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_ref: Option<PayloadRef>,
}

/// 一次模型往返的返回。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Round {
    pub round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// 空 = 本轮没有调用，可以正常结束（§6）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ProviderToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_blocks: Option<serde_json::Value>,
    #[serde(default)]
    pub usage: TokenUsage,
    /// provider 说这次回复被截断了。参数未收齐时不能开始执行（§6）。
    #[serde(default)]
    pub truncated: bool,
}

/// provider 给出的一次调用，还没有分配 Runtime 内部 ID。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderToolCall {
    pub provider_call_id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// `TurnDriver::next` 的输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoundInput {
    /// 首轮。
    First,
    /// 按 call_id 回传上一轮的结果。
    ToolResults {
        results: Vec<ToolResultForModel>,
        /// 更早结果的新视图：driver 按 `provider_call_id` 换掉历史里那条结果的正文，
        /// 配对（id、位置、`is_error`）不动，此后一直用新正文。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        revised: Vec<ToolResultForModel>,
    },
}

/// 回传给模型的一条工具结果。完整输出在 `output.json` 里，这里只给模型看得下的部分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultForModel {
    pub provider_call_id: String,
    pub call_id: ToolCallId,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
    /// 随这条结果交给模型的图片（按引用从输出目录读回来的原始字节）。不序列化：事实是
    /// `output.json` 里的 [`ImageRef`](super::refs::ImageRef)，这里只是这一次请求要发的字节。
    /// 修订（`revised`）只换正文，不动图片。
    #[serde(skip)]
    pub images: Vec<ToolImage>,
}

/// `LlmClient::begin_turn` 的输入：一次 Run 的一个执行段。工具 Schema、系统提示、
/// 记忆注入在这里装配一次（§13.5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRequest {
    pub session: SessionId,
    pub run: RunId,
    /// 本次 Run 固定的模型配置快照（§6）。配置热重载不在半路换模型（§3）。
    pub model: ModelConfig,
    pub system_prompt: String,
    /// 回放窗口：`conversation.boundary` 之后的消息（§13.1 `/new`）。
    pub messages: Vec<ReplayMessage>,
    pub tools: Vec<ToolDefinition>,
    /// 本次注入了哪些记忆条目——审计证据，resume 时重新核对（§9.7）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemoryUse>,
    /// 本次请求覆盖的事件范围，写进检查点（§8.3）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covers: Option<SeqRange>,
    /// 提示缓存按哪个会话的键路由（Responses 的 `prompt_cache_key`）。`None` = 按
    /// `session`；fork 任务给派它的会话，前缀与父的请求相同才命中同一份缓存。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_session: Option<SessionId>,
}

/// 交给模型回放的一条消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayMessage {
    pub role: Role,
    pub seq: Seq,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResultForModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_blocks: Option<serde_json::Value>,
}

/// 一次请求用到的某条记忆的具体版本（§9.7）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryUse {
    pub memory: MemoryId,
    pub revision: u32,
}

/// 一段 seq 区间，闭区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqRange {
    pub from: Seq,
    pub to: Seq,
}

/// LLM 调用失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    #[error("请求被拒绝（{status}）：{message}")]
    Rejected { status: u16, message: String },
    #[error("超时")]
    Timeout,
    #[error("回复未收齐——不能按半轮调用执行")]
    Incomplete,
    /// 模型不支持配置里的 effort。不能删掉 effort 自动重试（§13.3）。
    #[error("模型 {model} 不支持 effort={effort}")]
    UnsupportedEffort { model: String, effort: String },
    #[error("传输错误：{0}")]
    Transport(String),
    /// 结果与用量都未知：保留预留额度和未知标记，不能把这次消耗当成零（§8.5）。
    #[error("结果未知：{0}")]
    Unknown(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_round_input_round_trips_by_kind() {
        let input = RoundInput::ToolResults {
            results: vec![ToolResultForModel {
                provider_call_id: "pc-7".into(),
                call_id: ToolCallId::from_raw("call-7"),
                content: "result = 2".into(),
                is_error: false,
                images: vec![],
            }],
            revised: vec![ToolResultForModel {
                provider_call_id: "pc-3".into(),
                call_id: ToolCallId::from_raw("call-3"),
                content: "短视图".into(),
                is_error: true,
                images: vec![],
            }],
        };
        let text = serde_json::to_string(&input).unwrap();
        assert_eq!(serde_json::from_str::<RoundInput>(&text).unwrap(), input);
    }

    #[test]
    fn a_round_input_without_revised_reads_as_none_and_writes_without_the_key() {
        let old = r#"{"kind":"tool_results","results":[{"provider_call_id":"pc-7","call_id":"call-7","content":"result = 2"}]}"#;
        let input = serde_json::from_str::<RoundInput>(old).unwrap();
        let RoundInput::ToolResults { results, revised } = &input else {
            panic!("应读成 ToolResults：{input:?}");
        };
        assert_eq!(results.len(), 1);
        assert!(revised.is_empty());
        let text = serde_json::to_string(&input).unwrap();
        assert!(!text.contains("revised"), "{text}");
    }
}
