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

use crate::types::chat::{ApprovalScope, PeerId};
use crate::types::digest::ContentHash;
use crate::types::ids::{
    ApprovalId, AttemptId, EventId, GrantId, RunId, Seq, SessionId, ShortId, ToolCallId,
};
use crate::types::model::EffortSetting;
use crate::types::plan::{ExecutionPlan, PlanHash, PlanSource};
use crate::types::refs::{ContentRef, OutputRef, PayloadRef, ToolResultStatus};
use crate::types::status::AttemptState;
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
}

impl Event {
    /// 解析一行。
    pub fn from_line(line: &str) -> Result<Event, EventDecodeError> {
        let raw: RawEvent = serde_json::from_str(line).map_err(EventDecodeError::Envelope)?;
        Event::from_raw(raw)
    }

    /// 序列化成一行（**不**带结尾换行；写入器负责补上并要求每条完整记录以换行结束）。
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.to_raw()?)
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
        self.to_raw()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawEvent::deserialize(deserializer)?;
        Event::from_raw(raw).map_err(D::Error::custom)
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
    /// 停在一条审批上，已释放执行名额（§7.4）。
    RunWaitingApproval(RunWaitingApproval) => "run.waiting_approval",
    /// 停在一次有界退避上。
    RunWaitingRetry(RunWaitingRetry) => "run.waiting_retry",
    /// 上一个执行实例没有收尾。停机或崩溃，不等于用户取消。
    RunInterrupted(RunInterrupted) => "run.interrupted",
    /// 需要操作者判断。
    RunNeedsAttention(RunNeedsAttention) => "run.needs_attention",
    /// 正常结束，带最终回复。
    RunCompleted(RunCompleted) => "run.completed",
    RunFailed(RunFailed) => "run.failed",
    RunCancelled(RunCancelled) => "run.cancelled",
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
    /// 审批请求的**审计副本**。它不能自行创建授权——权威在 state.db（§7.4）。
    ApprovalRequested(ApprovalRequested) => "approval.requested",
    /// 审批决定的审计补写，保留原始发生时间（§8.5 的反向 outbox）。
    ApprovalDecided(ApprovalDecided) => "approval.decided",
    /// 已覆盖的 seq、上下文与记忆版本引用、执行游标（§8.2）。
    Checkpoint(Checkpoint) => "checkpoint",
    /// resume 时操作者明确切换了模型 / effort（§13.3）。
    ConfigChanged(ConfigChanged) => "config.changed",
}

impl EventPayload {
    /// 未知词汇。恢复读到它只保留、不解释。
    pub fn is_unknown(&self) -> bool {
        matches!(self, EventPayload::Unknown { .. })
    }
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunWaitingApproval {
    pub approval: ApprovalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<ToolCallId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunWaitingRetry {
    pub attempts: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub next_retry_at: OffsetDateTime,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunInterrupted {
    /// 哪个执行实例没有收尾。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<crate::types::ids::ExecutorId>,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunNeedsAttention {
    pub reason: String,
    /// 停在哪个调用上（如果有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<ToolCallId>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
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
    /// 这次执行消费了哪条授权（如果有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<GrantId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
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
pub struct ApprovalRequested {
    pub approval: ApprovalId,
    pub short_id: ShortId,
    pub plan_hash: PlanHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<ToolCallId>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<ApprovalScope>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalDecided {
    pub approval: ApprovalId,
    pub approved: bool,
    pub scope: ApprovalScope,
    /// 决定人。**审计副本**，权威是 state.db 里经过操作者认证的那一行（§7.4）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<PeerId>,
    /// 原始决定时间。补写保留它，不凭日志行相邻推断审批关系（§8.3）。
    #[serde(with = "time::serde::rfc3339")]
    pub decided_at: OffsetDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<GrantId>,
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
    fn an_event_serializes_onto_one_physical_line() {
        let event = Event {
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
            v: EVENT_FORMAT_VERSION,
            seq: Seq(2),
            event_id: EventId::from_raw("evt-2"),
            session: SessionId::from_raw("sess-1"),
            run: Some(RunId::from_raw("run-1")),
            ts: datetime!(2026-09-15 08:00:00 UTC),
            payload: EventPayload::MessageUser(MessageUser {
                text: Some("第一行\n第二行".into()),
                text_ref: None,
            }),
        };
        let line = event.to_line().unwrap();
        assert!(!line.contains('\n'));
        assert!(line.contains("\\n"));
        assert_eq!(Event::from_line(&line).unwrap(), event);
    }
}
