//! 一轮模型往返与一次输入接收所需的值类型（§6、§8.5、§13.5）。

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::chat::ChannelPeer;
use super::digest::ContentHash;
use super::ids::{
    ApprovalId, EventId, GrantId, MemoryId, RequestKey, RunId, Seq, SessionId, ToolCallId,
};
use super::model::{ModelConfig, TokenUsage};
use super::plan::{ExecutionPlan, PlanSource};
use super::refs::PayloadRef;
use super::tool::ToolDefinition;

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

/// 一次完整的模型回复 + 该轮全部调用计划。**这是一个逻辑事件**：正文小则内联，大则
/// 引用已经完整持久化的 payload，避免只恢复出半轮调用（§8.3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantRound {
    /// 第几轮。
    pub round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_ref: Option<PayloadRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRequest>,
    /// provider 回放所需的原始块（reasoning、签名等），原样保存。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_blocks: Option<serde_json::Value>,
    #[serde(default)]
    pub usage: TokenUsage,
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
    ToolResults { results: Vec<ToolResultForModel> },
}

/// 回传给模型的一条工具结果。完整输出在 `output.json` 里，这里只给模型看得下的部分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultForModel {
    pub provider_call_id: String,
    pub call_id: ToolCallId,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
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

/// `Ledger::accept_input` 的输入（§8.5 第一段箭头）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptInput {
    pub session: SessionId,
    /// 幂等键。同一键重发返回原 Run，内容哈希不同则拒绝（§8.5）。
    pub request_key: RequestKey,
    pub text: String,
    pub source: PlanSource,
    /// 来源会话；Cron 与本机 CLI 可能没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<ChannelPeer>,
    /// 本次 Run 固定的模型配置快照。
    pub model: ModelConfig,
    /// 这个 Session 的工作目录。Cron Job 的 `workdir`（§10）从这里落到 Session 行上——
    /// 它在**创建 Job 时**就已经核实过存在，这里只是把它带到会话上。
    ///
    /// `None` = 不改（已经在的会话保留它自己的），不是"清空"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<std::path::PathBuf>,
    /// 这是一次**委派**：子 Run 的身份、结果契约与轮次预算（§4）。
    ///
    /// 它在受理那一刻就落进 `run.accepted`，因此**活得过重启**：恢复后的子代理自己读得
    /// 到"结果要长什么样"，父 Run 续跑时也不必去问运行时。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate: Option<crate::types::delegate::DelegateSpec>,
    /// **受理这一刻冻结下来的身份与能力**（§4.3）：哪个 Agent、哪一版 Profile、哪个模型、
    /// 哪个工作目录、这次允许调用的工具、指向身份指令正文的引用、记忆作用域。
    ///
    /// 由受理方算好交下来，随 `run.accepted` 一起落账——**恢复时按它装配**：审批可能一小时
    /// 之后才答复，而那时 Profile、工作目录与磁盘上的文件都可能已经改过，恢复出来的 Run
    /// 不能因此换一副面孔。
    ///
    /// `None` = 这一条没有归属（旧行、Cron、子 Run 由父侧继承）：装配时按**当前**配置的
    /// 默认 Agent 兜底（§八「已有 Session 归入默认 Agent；旧日志不重写」）。
    /// 装箱的理由与 [`crate::events::RunAccepted::snapshot`] 同：它带着整份
    /// [`crate::types::model::ModelConfig`]，而这个结构在受理路径上是按值搬的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Box<crate::types::agent::RunSnapshot>>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

impl AcceptInput {
    /// 输入内容哈希。同一请求键重发、内容不同则拒绝——比的是它。
    pub fn input_hash(&self) -> ContentHash {
        ContentHash::of_str(&self.text)
    }
}

/// `Ledger::accept_input` 的返回。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    pub run: RunId,
    pub session: SessionId,
    pub event: EventId,
    pub seq: Seq,
    /// 这次是不是命中了同一请求键的原 Run（重发）。
    #[serde(default)]
    pub deduplicated: bool,
}

/// `Ledger::start_call` 的授权消费记录：首次授权消费与 `tool.started` 在同一事务里
/// 提交（§7.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantUse {
    pub approval: ApprovalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<GrantId>,
}

/// `Ledger::read` 的返回：**一页**事件。
///
/// `next` 是 `Option` 而不是"一个总是有值的游标"：`None` 就是"读完了"。用一个总有值的
/// 游标表达结尾，调用方只能靠"这一页是不是空的"去猜，而一页空可能只是这一次没读到
/// ——那两件事必须分得开，SSE 的补读循环才不会空转。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventBatch {
    pub session: SessionId,
    pub events: Vec<crate::events::Event>,
    /// 后面还有：下一页从这个 seq **之后**读起。`None` = 读到头了。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Seq>,
}

impl EventBatch {
    /// 后面还有没有。
    pub fn has_more(&self) -> bool {
        self.next.is_some()
    }
}

/// 一次工具调用的计划在账本里的样子——`record_round` 之后、`start_call` 之前。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedCall {
    pub call_id: ToolCallId,
    pub plan: ExecutionPlan,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn an_assistant_round_written_before_usage_existed_reads_with_defaults() {
        let old = r#"{"round":3,"tool_calls":[]}"#;
        let round: AssistantRound = serde_json::from_str(old).unwrap();
        assert_eq!(round.round, 3);
        assert!(round.usage.is_unknown());
        assert!(round.text.is_none());
        assert!(round.provider_blocks.is_none());
    }

    #[test]
    fn the_input_hash_is_over_the_text() {
        let mut input = AcceptInput {
            session: SessionId::from_raw("s"),
            request_key: RequestKey::new("telegram:1"),
            text: "你好".into(),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("s"),
            },
            peer: None,
            model: ModelConfig {
                provider: "openai_compatible".into(),
                base_url: "https://x/v1".into(),
                model: "m".into(),
                api_key_env: "K".into(),
                auth: None,
                effort: None,
                efforts: None,
                timeout_secs: 120,
            },
            workdir: None,
            delegate: None,
            snapshot: None,
            at: datetime!(2026-09-15 08:00:00 UTC),
        };
        let first = input.input_hash();
        input.at = datetime!(2026-09-16 08:00:00 UTC);
        assert_eq!(first, input.input_hash(), "只看正文，不看时间");
        input.text = "你好吗".into();
        assert_ne!(first, input.input_hash());
    }

    #[test]
    fn a_round_input_round_trips_by_kind() {
        let input = RoundInput::ToolResults {
            results: vec![ToolResultForModel {
                provider_call_id: "pc-7".into(),
                call_id: ToolCallId::from_raw("call-7"),
                content: "result = 2".into(),
                is_error: false,
            }],
        };
        let text = serde_json::to_string(&input).unwrap();
        assert_eq!(serde_json::from_str::<RoundInput>(&text).unwrap(), input);
    }
}
