//! JSONL 事件行的 serde 结构（§8.3）。
//!
//! 一行一条事件，UTF-8，内容里的换行由 JSON 转义；禁止多行美化输出。信封是
//!
//! ```jsonl
//! {"v":1,"seq":41,"event_id":"evt-41","session_id":"sess-1","run_id":"run-1","at":"2026-09-15T08:00:00Z","type":"assistant.message","data":{…}}
//! ```
//!
//! 两条读取规则，它们不是同一条：
//!
//! - **未知 `type`** 解成 [`EventPayload::Unknown`]，**保留原始内容与 seq**。降级运行
//!   的进程、或某个已退役机制写下的行，不能让它后面的每个事件都重新编号，也不能让
//!   整个会话读不出来。
//! - **已知 `type` 但 payload 解不出**是错误，不是 Unknown。那是损坏，不是词汇缺失。
//! - **未知 `v`** 也是错误：§8.3 要求「影响执行、权限或调用配对的未知版本必须暂停恢复
//!   并要求兼容版本」，把它降格成 Unknown 会让恢复接着往下跑。

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;

mod tree_codec;

use crate::compaction::{CompactionDebt, CompactionDecision};
use crate::types::chat::PeerId;
use crate::types::digest::ContentHash;
use crate::types::ids::{AttemptId, EventId, RunId, Seq, SessionId, ToolCallId};
use crate::types::model::{EffortSetting, TokenUsage};
use crate::types::plan::{ExecutionPlan, PlanHash, PlanSource};
use crate::types::refs::{ContentRef, OutputRef, PayloadRef, ToolResultStatus};
use crate::types::status::{AttemptState, WaitReason};
use crate::types::turn::{MemoryUse, SeqRange, ToolCallRequest};

/// 当前的事件格式版本。
pub const EVENT_FORMAT_VERSION: u32 = 1;

/// 一条 JSONL 事件。
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// 格式版本。
    pub v: u32,
    /// Session 内按追加顺序严格递增。
    pub seq: Seq,
    /// 一次逻辑事件的 ID；重复提交同一 ID 必须幂等。
    pub event_id: EventId,
    pub session: SessionId,
    pub run: Option<RunId>,
    pub ts: OffsetDateTime,
    pub payload: EventPayload,
    /// Canonical pi entry. Legacy events have no entry until projected for migration.
    pub entry: Option<Box<crate::session::SessionEntry>>,
}

impl Event {
    /// 解析一行。
    pub fn from_line(line: &str) -> Result<Event, EventDecodeError> {
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(EventDecodeError::Envelope)?;
        if value.get("komo").is_some() {
            serde_json::from_value(value).map_err(EventDecodeError::Envelope)
        } else {
            Event::from_raw(serde_json::from_value(value).map_err(EventDecodeError::Envelope)?)
        }
    }

    /// 序列化成一行（**不**带结尾换行；写入器负责补上并要求每条完整记录以换行结束）。
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// 这条事件的 `type` 字符串。
    pub fn type_name(&self) -> &str {
        self.payload.type_name()
    }

    fn from_raw(raw: RawEvent) -> Result<Event, EventDecodeError> {
        if raw.v != EVENT_FORMAT_VERSION {
            return Err(EventDecodeError::UnsupportedVersion {
                v: raw.v,
                seq: Some(raw.seq),
            });
        }
        let payload = EventPayload::from_parts(&raw.event_type, raw.data)?;
        Ok(Event {
            entry: None,
            v: raw.v,
            seq: Seq(raw.seq),
            event_id: raw.event_id,
            session: raw.session,
            run: raw.run,
            ts: raw.at,
            payload,
        })
    }

    fn to_raw(&self) -> Result<RawEvent, serde_json::Error> {
        Ok(RawEvent {
            v: self.v,
            seq: self.seq.0,
            event_id: self.event_id.clone(),
            session: self.session.clone(),
            run: self.run.clone(),
            at: self.ts,
            event_type: self.payload.type_name().to_string(),
            data: self.payload.to_data()?,
        })
    }
}

impl Serialize for Event {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.entry {
            Some(entry) => tree_codec::encode(self, entry)
                .map_err(serde::ser::Error::custom)?
                .serialize(serializer),
            None => self
                .to_raw()
                .map_err(serde::ser::Error::custom)?
                .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("komo").is_some() {
            tree_codec::decode(value).map_err(D::Error::custom)
        } else {
            let raw: RawEvent = serde_json::from_value(value).map_err(D::Error::custom)?;
            Event::from_raw(raw).map_err(D::Error::custom)
        }
    }
}

/// 行信封。字段名是线格式，Rust 侧的名字见 [`Event`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawEvent {
    v: u32,
    seq: u64,
    event_id: EventId,
    #[serde(rename = "session_id")]
    session: SessionId,
    #[serde(rename = "run_id", default, skip_serializing_if = "Option::is_none")]
    run: Option<RunId>,
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    data: serde_json::Value,
}

/// 解析一行事件失败。
#[derive(Debug, thiserror::Error)]
pub enum EventDecodeError {
    /// 信封本身不是合法 JSON，或缺字段。
    #[error("事件信封解析失败：{0}")]
    Envelope(#[source] serde_json::Error),
    /// 未知格式版本——暂停恢复，要求兼容版本（§8.3）。
    #[error("不支持的事件格式版本 v={v}（seq={seq:?}）")]
    UnsupportedVersion { v: u32, seq: Option<u64> },
    /// 已知 type，payload 解不出：损坏，不是词汇缺失。
    #[error("事件 {event_type} 的 payload 解析失败：{source}")]
    Payload {
        event_type: String,
        #[source]
        source: serde_json::Error,
    },
}

macro_rules! event_payload {
    (
        $(
            $(#[$meta:meta])*
            $variant:ident($body:ty) => $name:literal,
        )*
    ) => {
        /// 事件族。加变体时记得同时加 `type` 字符串——两者由这个宏绑在一起。
        #[derive(Debug, Clone, PartialEq)]
        pub enum EventPayload {
            $(
                $(#[$meta])*
                $variant($body),
            )*
            /// 这个版本没有词汇的 `type`。保留原始 `type` 与 `data`，seq 在
            /// [`Event`] 上，因此不会被重新编号。
            Unknown {
                event_type: String,
                raw: serde_json::Value,
            },
        }

        impl EventPayload {
            pub fn type_name(&self) -> &str {
                match self {
                    $(EventPayload::$variant(_) => $name,)*
                    EventPayload::Unknown { event_type, .. } => event_type,
                }
            }

            fn from_parts(
                event_type: &str,
                data: serde_json::Value,
            ) -> Result<EventPayload, EventDecodeError> {
                match event_type {
                    $(
                        $name => serde_json::from_value(data)
                            .map(EventPayload::$variant)
                            .map_err(|source| EventDecodeError::Payload {
                                event_type: event_type.to_string(),
                                source,
                            }),
                    )*
                    other => Ok(EventPayload::Unknown {
                        event_type: other.to_string(),
                        raw: data,
                    }),
                }
            }

            fn to_data(&self) -> Result<serde_json::Value, serde_json::Error> {
                match self {
                    $(EventPayload::$variant(body) => serde_json::to_value(body),)*
                    EventPayload::Unknown { raw, .. } => Ok(raw.clone()),
                }
            }
        }
    };
}

event_payload! {
    /// 输入已持久保存并绑定 Run ID。**这一条就是会话里的那句用户消息**（§8.5）。
    RunAccepted(RunAccepted) => "run.accepted",
    /// 事件引用与 `queued` 已提交，可以被领取。
    RunQueued(RunQueued) => "run.queued",
    /// 某个执行实例领取了它。
    RunStarted(RunStarted) => "run.started",
    /// 停在某个外部条件上，已释放执行名额（§8.4）：退避到点、干预答复、前一条 Run
    /// 进终态。`reason` 说清是哪一种——**状态只有一个 `waiting`**，
    /// "为什么停着"在这一条事件里。
    RunWaiting(RunWaiting) => "run.waiting",
    /// 上一次执行没有收尾，领取权已交还（§8.9）。**它不是状态**：那条 Run 由 reconcile
    /// 当场判成 `queued`（安全）或 `waiting + intervention`（副作用不明），所以这里只
    /// 记"发生过什么"，不写状态。
    RunReclaimed(RunReclaimed) => "run.reclaimed",
    /// 操作者在 Intervention 清单上放弃（§7.5）。**与取消分得开**：不是用户不想跑了，
    /// 而是这件事不会再有下文。
    RunAbandoned(RunAbandoned) => "run.abandoned",
    /// 正常结束，带最终回复。
    RunCompleted(RunCompleted) => "run.completed",
    RunFailed(RunFailed) => "run.failed",
    RunCancelled(RunCancelled) => "run.cancelled",
    /// 插话（§6）：这条排着队的输入被同 Session 正在跑的那条 Run 在两轮之间并了进去。
    /// 写在**被并的那条**上（`run_id` 是它）；fold 把那句话挪到宿主 Run 这一刻的位置。
    RunAbsorbed(RunAbsorbed) => "run.absorbed",
    /// 一条不开启 Run 的用户消息。
    MessageUser(MessageUser) => "message.user",
    /// 一次完整的模型回复与该轮全部调用计划，一个逻辑事件（§8.3）。
    MessageAssistant(MessageAssistant) => "message.assistant",
    /// `/new`：在当前 Session 追加一个回放边界，不切 Session（§13.1）。
    ConversationBoundary(ConversationBoundary) => "conversation.boundary",
    /// 一次调用的准备计划已持久保存，确定尚未执行。
    ToolPlanned(ToolPlanned) => "tool.planned",
    /// 此后才允许产生真实副作用（§8.5）。**它本身不证明副作用已发生**。
    ToolStarted(ToolStarted) => "tool.started",
    /// 只有状态、耗时等元信息、`output_ref` 和最多 1 KiB 预览（§8.3）。
    ToolResult(ToolResult) => "tool.result",
    /// Python 子调用 journal；不是顶层工具消息，不参与调度状态折叠。
    CodemodeJournal(CodemodeJournal) => "codemode.journal",
    // 2026-10-11 之前还有 `approval.requested` / `approval.decided`（审批的审计副本）。
    // 审批删掉之后它们不再有词汇：旧行读成 `Unknown`，保留 seq、什么都不表示（§8.3）。
    /// 已覆盖的 seq、上下文与记忆版本引用、执行游标（§8.2）。
    Checkpoint(Checkpoint) => "checkpoint",
    /// resume 时操作者明确切换了模型 / effort（§13.3）。
    ConfigChanged(ConfigChanged) => "config.changed",
    /// 在线压缩的结论：压了（摘要 + 从哪一轮起原样保留），或没压。
    ContextCompacted(ContextCompacted) => "context.compacted",
    /// 这一段对话钉住的记忆注入段（§9.4）：渲染好的正文与选中的条目，重启之后照它逐字复用。
    MemoryInjected(MemoryInjected) => "memory.injected",
}

impl EventPayload {
    /// 未知词汇。恢复读到它只保留、不解释。
    pub fn is_unknown(&self) -> bool {
        matches!(self, EventPayload::Unknown { .. })
    }
}

/// 子调用 journal 的信封；阶段协议由 runtime 校验。正文通过 output 引用外置，
/// 这里仅含父身份、阶段与引用，不能投影成额外的模型工具结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodemodeJournal {
    pub run: RunId,
    pub parent: ToolCallId,
    pub record: serde_json::Value,
}

/// §8.5：`run.accepted` 带完整输入与绑定的 Run ID。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunAccepted {
    pub request_key: crate::types::ids::RequestKey,
    pub input_hash: ContentHash,
    /// 输入正文；大正文外置到 `payloads/`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_ref: Option<PayloadRef>,
    pub source: PlanSource,
    /// 来源会话的平台 id（如果有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// 本次 Run 固定的模型快照，只记身份不记凭证。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortSetting>,
    /// 这是一条**子 Run**：谁派的、结果要长什么样、它有几步预算（[`DelegateSpec`]）。
    ///
    /// 契约落在这里而不是 state.db，是因为重启之后它是子代理唯一的"结果要长什么样"的
    /// 依据，而账本里的这一条正好活得过重启（§8.3：内容权威）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate: Option<crate::types::delegate::DelegateSpec>,
    /// 受理时冻结的身份与能力（§4.3，[`crate::types::agent::RunSnapshot`]）。
    ///
    /// 契约与 `delegate` 同理落在**事件**里：它是这一段执行唯一的身份依据，而账本里的这
    /// 一条正好活得过重启。`None` = 旧行 / 没有归属的入口——装配时按当前配置的默认 Agent
    /// 兜底，**不重写旧日志**（§八）。
    /// 装箱是因为它带着整份 [`crate::types::model::ModelConfig`]：不装的话
    /// [`EventPayload`] 这个枚举本身就有 700 多字节，而账本里绝大多数事件都用不上它。
    /// serde 对 `Box` 透明，落盘的 JSONL 一个字节都不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Box<crate::types::agent::RunSnapshot>>,
    /// 发送方要求**插话**（§6，只有 TUI 发）：同 Session 有 Run 正在跑时，这句话不排到它
    /// 后面，而是由它在下一次请求模型之前并进去（[`RunAbsorbed`]）。没轮上就照常排队。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interject: bool,
}

/// 插话（§6）：被并进了哪条 Run。日志里这一刻的位置就是那句话在宿主 Run 里的位置——
/// 它前面是宿主上一轮的调用结果，后面是宿主的下一轮回复。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunAbsorbed {
    pub into: RunId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunQueued {
    /// 引用 `run.accepted` 的事件 ID。
    pub input_ref: EventId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunStarted {
    pub executor: crate::types::ids::ExecutorId,
    /// 领取代次。旧代次不能继续提交新状态（§8.7）。
    pub generation: u64,
}

/// 停在某个外部条件上（§8.4）。
///
/// **只记"在等什么"，不记"停在哪个调用上"**：后者是派生的——结果不明时它在
/// `tool_calls.state = 'uncertain'` 那一条上（§7.5）。事件里再存一份就是同一个事实的第三个落点，而它会漂。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunWaiting {
    pub reason: WaitReason,
}

/// 上一次执行没有收尾（停机、崩溃、handler 任务死了），领取权已交还（§8.9）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunReclaimed {
    /// 哪个执行实例没有收尾。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<crate::types::ids::ExecutorId>,
    #[serde(default)]
    pub reason: String,
}

/// 操作者放弃（§7.5 的 `abandon`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunAbandoned {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<PeerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunCompleted {
    /// 最终回复正文。**会话消息面由 `message.assistant` 承担**；这里留一份是为了
    /// "结果已保存但客户端没收到"时能直接补读（§8.4）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message_ref: Option<PayloadRef>,
    #[serde(default)]
    pub rounds: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunFailed {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunCancelled {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<PeerId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageUser {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_ref: Option<PayloadRef>,
    /// 受理时正文后面还补了一份会话中改过的项目规范：那一段是给模型的，`text` 里只有用户原话，
    /// 界面只提一句。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project_update: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageAssistant {
    pub round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_ref: Option<PayloadRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_blocks: Option<serde_json::Value>,
    /// 整个提示，含缓存读写的部分（见 [`TokenUsage::input`]）。
    ///
    /// [`TokenUsage::input`]: crate::types::model::TokenUsage::input
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// `output_tokens` 里推理的部分（见 [`TokenUsage::reasoning`]）；没报的留空。
    ///
    /// [`TokenUsage::reasoning`]: crate::types::model::TokenUsage::reasoning
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationBoundary {
    /// 谁划的这一刀（`/new` 来自哪个渠道）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<PeerId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolPlanned {
    pub call_id: ToolCallId,
    pub plan_hash: PlanHash,
    /// 计划内联；大计划外置到 `payloads/`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Box<ExecutionPlan>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_ref: Option<PayloadRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStarted {
    pub call_id: ToolCallId,
    pub attempt_id: AttemptId,
    /// 指向承载计划的事件。
    pub plan_ref: EventId,
    pub plan_hash: PlanHash,
    // 审批删掉之前这里还有 `grant` / `then_run_grant`（这次消费了哪条授权）。旧行带着
    // 它们照样解得出来：serde 忽略多出来的字段。
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// 完整结构化正文；旧事件缺此字段时按 output_ref 读取。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Box<crate::types::refs::ToolResultBody>>,
    pub call_id: ToolCallId,
    pub attempt_id: AttemptId,
    pub status: ToolResultStatus,
    pub output_ref: OutputRef,
    #[serde(default)]
    pub elapsed_ms: u64,
    /// 最多 1 KiB。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_state: Option<AttemptState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub covers: SeqRange,
    /// JSONL 字节位置，只是加速索引；校验不符就重新扫描（§8.3）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_offset: Option<u64>,
    pub format_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemoryUse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval_config_version: Option<String>,
}

/// §9.4：这一段对话（上一个 `conversation.boundary` 之后）注入系统提示的那一块。
///
/// 注入段在 system 消息里，它一变整条前缀缓存就失效，所以同一段里要**逐字**复用。只记在
/// 进程内存里的话，Gateway 一重启就会按另一句用户输入重新召回、换掉整段提示；记成事件，
/// 内容权威就在 Session 目录（§8.2），重启前后折出来是同一份。`text` 为空 = 这一段确实
/// 没有可注入的记忆（也要钉住，不然每一轮都会重新召回一次）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryInjected {
    /// 渲染好的注入段；超过内联上限外置到 `payloads/`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_ref: Option<PayloadRef>,
    /// 注入段里的条目与版本：复用之前逐条核对还算不算数（遗忘立即生效，§9.2）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<MemoryUse>,
    /// 召回诊断；旧行没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall: Option<crate::types::memory::RecallDiagnostics>,
}

/// 截至 `upto`（含）最近的一条 `memory.injected`：一个 Run 系统提示里带的那一块。
///
/// 同一段对话里注入段只在换块时才记一条，后面的 Run 沿用它而不再记，所以要往前找最近
/// 的一条；`run inspect` 与记忆提取（"这一批注入了什么"）共用这一个解法。
pub fn injection_at(events: &[Event], upto: Seq) -> Option<(&Event, &MemoryInjected)> {
    events
        .iter()
        .filter(|event| event.seq <= upto)
        .filter_map(|event| match &event.payload {
            EventPayload::MemoryInjected(body) => Some((event, body)),
            _ => None,
        })
        .next_back()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigChanged {
    /// 变化的键名，**不带值**（§3）。
    pub keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortSetting>,
    #[serde(default)]
    pub reason: String,
}

/// 在线压缩的结论（§8.3）。追加，不改写：被摘要覆盖的那些行一个字节
/// 都没动，回放读到它才把这条 Run 早先的轮次换成摘要——**fold 决定它意味着什么**。
///
/// **摘要只是模型视图，不是恢复依据。**恢复（§8.4、§8.6）从不读它："做没做过"仍然只按
/// `tool.*` 事件与 `output.json` 判。不认识这个 type 的旧二进制把它读成 Unknown 忽略掉，
/// 回放的是完整历史——只是贵，不是错。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ContextCompacted {
    Compacted {
        /// 原样保留的第一条：这条 Run 某一轮 `message.assistant` 的 seq。切点只落在轮次
        /// 开头，调用永远不和它的结果分开。
        first_kept: Seq,
        /// 摘要正文；超过内联上限外置到 `payloads/`（`summary_ref`）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_ref: Option<PayloadRef>,
        decision: CompactionDecision,
        debt: CompactionDebt,
        /// 摘要请求自己的用量。
        #[serde(default)]
        usage: TokenUsage,
    },
    /// 一批大结果换成了短视图（§8.3）。和压缩一样是对上下文前缀的一次改写，所以同一个
    /// 事件、同一本缓存账：缓存已冷或贴着窗口时才换，**一次换一批**
    /// ——逐条到期逐条换，每一条都让它之后的缓存前缀失效一次。fold 把 `calls` 累进这条
    /// Run 的已衰减集合，回放按集合选视图。
    Decayed {
        calls: Vec<ToolCallId>,
        decision: CompactionDecision,
        debt: CompactionDebt,
    },
    /// 这个边界上没压（账算不过来、切不出、摘要请求失败……）。上下文原样还在。
    Skipped {
        reason: String,
        /// 当时的决策，供事后核对。`compact: true` 而落到这里 = 决定了压、摘要请求却没成
        /// （失败、被截断），在线状态据此先不在窗口压力下重试（[`OnlineState`] 的
        /// `compaction_refused`）。
        ///
        /// [`OnlineState`]: crate::compaction::OnlineState
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision: Option<CompactionDecision>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn line(event_type: &str, data: &str) -> String {
        format!(
            r#"{{"v":1,"seq":41,"event_id":"evt-41","session_id":"sess-1","run_id":"run-1","at":"2026-09-15T08:00:00Z","type":"{event_type}","data":{data}}}"#
        )
    }

    #[test]
    fn memory_injected_round_trips_with_and_without_recall_diagnostics() {
        use crate::types::memory::{RecallDiagnostics, RetrievalMode};

        // 旧行：没有 recall。
        let old = Event::from_line(&line(
            "memory.injected",
            r#"{"text":"x","uses":[{"memory":"m-1","revision":2}]}"#,
        ))
        .unwrap();
        let EventPayload::MemoryInjected(body) = &old.payload else {
            panic!("解成了 {:?}", old.payload);
        };
        assert_eq!(body.recall, None);
        assert_eq!(body.uses.len(), 1);
        assert!(!old.to_line().unwrap().contains("recall"), "没有就不写");

        // 新行：带诊断，序列化再解析原样回来。
        let mut event = old.clone();
        let recall = RecallDiagnostics {
            mode: Some(RetrievalMode::Hybrid),
            degraded: true,
            degraded_reason: Some("向量端点不通".into()),
            candidates_before_fusion: Some(12),
            candidates_after_fusion: Some(8),
            dropped_by_budget: Some(3),
            conventions_over_budget: None,
            matters_over_budget: None,
            arm_scores: vec![],
            dropped_by_threshold: None,
            reused_pin: false,
        };
        let EventPayload::MemoryInjected(body) = &mut event.payload else {
            unreachable!()
        };
        body.recall = Some(recall.clone());
        let back = Event::from_line(&event.to_line().unwrap()).unwrap();
        assert_eq!(back, event);
        let EventPayload::MemoryInjected(body) = &back.payload else {
            unreachable!()
        };
        assert_eq!(body.recall, Some(recall));
    }

    #[test]
    fn a_tool_result_line_from_the_design_doc_parses() {
        let raw = r#"{"v":1,"seq":43,"event_id":"evt-43","session_id":"sess-1","run_id":"run-1","at":"2026-09-15T08:00:02Z","type":"tool.result","data":{"call_id":"call-7","attempt_id":"attempt-1","status":"completed","output_ref":{"path":"tool-output/run-1/call-7/attempt-1/output.json","size":12,"hash":"deadbeef"},"preview":"result = 2"}}"#;
        let event = Event::from_line(raw).unwrap();
        assert_eq!(event.seq, Seq(43));
        assert_eq!(event.type_name(), "tool.result");
        let EventPayload::ToolResult(body) = &event.payload else {
            panic!("解成了 {:?}", event.payload);
        };
        assert_eq!(body.status, ToolResultStatus::Completed);
        assert_eq!(body.preview.as_deref(), Some("result = 2"));
        assert_eq!(body.elapsed_ms, 0, "老行没有 elapsed_ms，读为默认");
    }

    #[test]
    fn an_unknown_type_keeps_its_seq_and_its_raw_body() {
        let raw = line(
            "memory.promoted",
            r#"{"memory_id":"m-1","note":"未来的机制"}"#,
        );
        let event = Event::from_line(&raw).unwrap();
        assert_eq!(event.seq, Seq(41), "seq 不能被重新编号");
        assert_eq!(event.type_name(), "memory.promoted");
        assert!(event.payload.is_unknown());
        let EventPayload::Unknown { raw: body, .. } = &event.payload else {
            unreachable!()
        };
        assert_eq!(body["note"], "未来的机制");

        // 原样写回去：未知事件保留原始内容（§8.3）。
        let round_tripped = Event::from_line(&event.to_line().unwrap()).unwrap();
        assert_eq!(round_tripped, event);
    }

    /// 审批删掉之后，旧会话里的 `approval.*` 行没有词汇了：读成 Unknown、保留 seq。
    #[test]
    fn an_old_approval_line_reads_as_unknown() {
        let raw = line(
            "approval.requested",
            r#"{"approval":"ap-1","short_id":"7K2M","plan_hash":"abc","call_id":"call-7","reason":"shell","scopes":["once"]}"#,
        );
        let event = Event::from_line(&raw).unwrap();
        assert_eq!(event.seq, Seq(41));
        assert!(event.payload.is_unknown());
        assert_eq!(event.type_name(), "approval.requested");

        let raw = line(
            "approval.decided",
            r#"{"approval":"ap-1","approved":true,"scope":"once","decided_at":"2026-09-18T08:00:00Z"}"#,
        );
        assert!(Event::from_line(&raw).unwrap().payload.is_unknown());
    }

    /// 审批删掉之前的 `tool.started` 带着 `grant` / `then_run_grant`：照样解得出来。
    #[test]
    fn an_old_tool_started_with_a_grant_still_decodes() {
        let raw = line(
            "tool.started",
            r#"{"call_id":"call-7","attempt_id":"attempt-1","plan_ref":"evt-40","plan_hash":"abc","grant":"g-1","then_run_grant":"g-2"}"#,
        );
        let event = Event::from_line(&raw).unwrap();
        let EventPayload::ToolStarted(body) = &event.payload else {
            panic!("解成了 {:?}", event.payload);
        };
        assert_eq!(body.call_id.as_str(), "call-7");
        assert_eq!(body.plan_hash.as_str(), "abc");
    }

    /// python 工具（2026-10-15 删除）留下的计划：形状取自真实会话里的一行，代码正文换成了占位。
    /// 解得出来，`PythonCode` 与 `versions.env` 都保留，重新序列化与原样一致（计划哈希不变）。
    #[test]
    fn an_old_python_plan_still_decodes_and_keeps_its_shape() {
        let data = r#"{"call_id":"01a0b3aa-c96f-7672-808e-02b5cde75b9b","plan":{"args":{"code":"print(1)","mode":"code"},"cwd":"/home/u/.komo/workspaces","operation":{"kind":"python_code"},"operation_id":"01a0b3aa-c975-75f6-b6fb-82cc56c52c9c","recovery":{"kind":"no_safe_recovery"},"run":"01a0b3aa-922a-71a3-b47f-237eb35db34e","source":{"kind":"interactive","session":"01a0b3aa-7207-792e-98dd-3d450ae2f4a9"},"tool":"python","tool_call":"01a0b3aa-c96f-7672-808e-02b5cde75b9b","versions":{"code":"cf63c33ffe0db31f284ff6a55b456f5d96338d362bad057c5954aaff7b8bec08","env":"py-3.9.6-no-lock"}},"plan_hash":"d26e1738297d7d3aae2389d93a20ba84e1698b57079aca6e3e56e292f2c01661"}"#;
        let raw = line("tool.planned", data);
        let event = Event::from_line(&raw).unwrap();
        let EventPayload::ToolPlanned(body) = &event.payload else {
            panic!("解成了 {:?}", event.payload);
        };
        let plan = body.plan.as_ref().expect("计划内联");
        assert_eq!(plan.tool, "python");
        assert_eq!(plan.operation, crate::types::plan::Operation::PythonCode);
        assert_eq!(
            plan.versions.env.as_ref().map(|env| env.0.as_str()),
            Some("py-3.9.6-no-lock")
        );
        let original: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(serde_json::to_value(plan).unwrap(), original["plan"]);
    }

    /// D2：旧的"停在审批上"那一行仍然解得出来。
    #[test]
    fn an_old_run_waiting_on_an_approval_still_decodes() {
        let raw = line(
            "run.waiting",
            r#"{"reason":{"kind":"approval","approval":"ap-1"}}"#,
        );
        let event = Event::from_line(&raw).unwrap();
        let EventPayload::RunWaiting(body) = &event.payload else {
            panic!("解成了 {:?}", event.payload);
        };
        assert!(matches!(body.reason, WaitReason::Approval { .. }));
        assert!(!body.reason.needs_a_person());
    }

    #[test]
    fn a_known_type_with_a_broken_payload_is_an_error_not_an_unknown() {
        let raw = line("tool.result", r#"{"call_id":"call-7"}"#);
        let err = Event::from_line(&raw).unwrap_err();
        assert!(
            matches!(&err, EventDecodeError::Payload { event_type, .. } if event_type == "tool.result"),
            "{err}"
        );
    }

    #[test]
    fn an_unknown_format_version_stops_the_read() {
        let raw = r#"{"v":2,"seq":7,"event_id":"e","session_id":"s","at":"2026-09-15T08:00:00Z","type":"run.queued","data":{"input_ref":"e0"}}"#;
        let err = Event::from_line(raw).unwrap_err();
        assert!(
            matches!(
                err,
                EventDecodeError::UnsupportedVersion { v: 2, seq: Some(7) }
            ),
            "{err}"
        );
    }

    /// 插话（§6）：`run.absorbed` 认得出来，`run.accepted` 只在要求插话时才写 `interject`。
    #[test]
    fn an_absorbed_line_and_an_interjecting_input_round_trip() {
        let raw = line("run.absorbed", r#"{"into":"run-0"}"#);
        let event = Event::from_line(&raw).unwrap();
        let EventPayload::RunAbsorbed(body) = &event.payload else {
            panic!("解成了 {:?}", event.payload);
        };
        assert_eq!(body.into.as_str(), "run-0");
        assert_eq!(Event::from_line(&event.to_line().unwrap()).unwrap(), event);

        let raw = line(
            "run.accepted",
            r#"{"request_key":"tui:1","input_hash":"abc","text":"顺便","source":{"kind":"interactive","session":"sess-1"},"interject":true}"#,
        );
        let event = Event::from_line(&raw).unwrap();
        let EventPayload::RunAccepted(body) = &event.payload else {
            panic!()
        };
        assert!(body.interject);
        let mut quiet = body.clone();
        quiet.interject = false;
        let line = serde_json::to_string(&quiet).unwrap();
        assert!(!line.contains("interject"), "不插话就不写：{line}");
    }

    #[test]
    fn a_line_written_before_a_field_existed_reads_it_as_the_default() {
        // `run.accepted` 最早只有请求键、哈希和正文。
        let raw = line(
            "run.accepted",
            r#"{"request_key":"telegram:1","input_hash":"abc","text":"你好","source":{"kind":"interactive","session":"sess-1"}}"#,
        );
        let event = Event::from_line(&raw).unwrap();
        let EventPayload::RunAccepted(body) = &event.payload else {
            panic!()
        };
        assert_eq!(body.text.as_deref(), Some("你好"));
        assert!(body.model.is_none(), "后加的 model 读为 None");
        assert!(body.effort.is_none());
        assert!(body.peer.is_none());

        // `message.assistant` 最早没有 provider_blocks / token 计数。
        let raw = line("message.assistant", r#"{"round":3}"#);
        let event = Event::from_line(&raw).unwrap();
        let EventPayload::MessageAssistant(body) = &event.payload else {
            panic!()
        };
        assert_eq!(body.round, 3);
        assert!(body.tool_calls.is_empty());
        assert!(body.provider_blocks.is_none());
        assert!(body.input_tokens.is_none());
    }

    #[test]
    fn assistant_cache_tokens_are_absent_on_old_lines_and_round_trip_on_new_ones() {
        let raw = line(
            "message.assistant",
            r#"{"round":1,"input_tokens":120,"output_tokens":7}"#,
        );
        let event = Event::from_line(&raw).unwrap();
        let EventPayload::MessageAssistant(body) = &event.payload else {
            panic!()
        };
        assert_eq!(body.input_tokens, Some(120));
        assert!(body.cache_read_tokens.is_none());
        assert!(body.cache_write_tokens.is_none());
        let rewritten = event.to_line().unwrap();
        assert!(!rewritten.contains("cache_"), "没有就不写：{rewritten}");

        let raw = line(
            "message.assistant",
            r#"{"round":2,"input_tokens":1200,"output_tokens":7,"cache_read_tokens":1000,"cache_write_tokens":150}"#,
        );
        let event = Event::from_line(&raw).unwrap();
        let EventPayload::MessageAssistant(body) = &event.payload else {
            panic!()
        };
        assert_eq!(body.cache_read_tokens, Some(1000));
        assert_eq!(body.cache_write_tokens, Some(150));
        assert_eq!(Event::from_line(&event.to_line().unwrap()).unwrap(), event);
    }

    #[test]
    fn assistant_reasoning_tokens_are_absent_on_old_lines_and_round_trip_on_new_ones() {
        let raw = line(
            "message.assistant",
            r#"{"round":1,"input_tokens":120,"output_tokens":7}"#,
        );
        let event = Event::from_line(&raw).unwrap();
        let EventPayload::MessageAssistant(body) = &event.payload else {
            panic!()
        };
        assert!(body.reasoning_tokens.is_none());
        assert!(!event.to_line().unwrap().contains("reasoning"));

        let raw = line(
            "message.assistant",
            r#"{"round":2,"input_tokens":120,"output_tokens":70,"reasoning_tokens":55}"#,
        );
        let event = Event::from_line(&raw).unwrap();
        let EventPayload::MessageAssistant(body) = &event.payload else {
            panic!()
        };
        assert_eq!(body.reasoning_tokens, Some(55));
        assert_eq!(Event::from_line(&event.to_line().unwrap()).unwrap(), event);
    }

    #[test]
    fn an_event_serializes_onto_one_physical_line() {
        let event = Event {
            entry: None,
            v: EVENT_FORMAT_VERSION,
            seq: Seq(1),
            event_id: EventId::from_raw("evt-1"),
            session: SessionId::from_raw("sess-1"),
            run: None,
            ts: datetime!(2026-09-15 08:00:00 UTC),
            payload: EventPayload::ConversationBoundary(ConversationBoundary { by: None }),
        };
        let line = event.to_line().unwrap();
        assert!(!line.contains('\n'), "{line}");
        assert!(!line.contains("run_id"), "缺省的 run_id 不写出来：{line}");
        assert!(line.contains(r#""type":"conversation.boundary""#), "{line}");
        assert_eq!(Event::from_line(&line).unwrap(), event);
    }

    #[test]
    fn newlines_inside_content_are_escaped_not_emitted() {
        let event = Event {
            entry: None,
            v: EVENT_FORMAT_VERSION,
            seq: Seq(2),
            event_id: EventId::from_raw("evt-2"),
            session: SessionId::from_raw("sess-1"),
            run: Some(RunId::from_raw("run-1")),
            ts: datetime!(2026-09-15 08:00:00 UTC),
            payload: EventPayload::MessageUser(MessageUser {
                text: Some("第一行\n第二行".into()),
                text_ref: None,
                project_update: false,
            }),
        };
        let line = event.to_line().unwrap();
        assert!(!line.contains('\n'));
        assert!(line.contains("\\n"));
        assert_eq!(Event::from_line(&line).unwrap(), event);
    }
    #[test]
    fn both_compaction_outcomes_round_trip_and_say_which_they_are() {
        let event = |seq: u64, payload: ContextCompacted| Event {
            entry: None,
            v: EVENT_FORMAT_VERSION,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("evt-{seq}")),
            session: SessionId::from_raw("sess-1"),
            run: Some(RunId::from_raw("run-1")),
            ts: datetime!(2026-09-15 08:00:00 UTC),
            payload: EventPayload::ContextCompacted(payload),
        };

        let compacted = event(9, crate::test_support::compacted(Seq(4), "做完了 a"));
        let line = compacted.to_line().unwrap();
        assert!(line.contains(r#""type":"context.compacted""#), "{line}");
        assert!(line.contains(r#""outcome":"compacted""#), "{line}");
        assert!(line.contains(r#""first_kept":4"#), "{line}");
        assert!(!line.contains("summary_ref"), "没外置就不写：{line}");
        assert_eq!(Event::from_line(&line).unwrap(), compacted);

        let skipped = event(
            10,
            ContextCompacted::Skipped {
                reason: "deferred_economic".into(),
                decision: None,
            },
        );
        let line = skipped.to_line().unwrap();
        assert!(line.contains(r#""outcome":"skipped""#), "{line}");
        assert_eq!(Event::from_line(&line).unwrap(), skipped);
    }
}
