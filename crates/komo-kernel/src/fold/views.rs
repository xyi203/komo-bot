//! `Surface` 上的各个视图类型。
//!
//! 它们是 [`super::fold`] 的**输出形状**，和折叠本身分开——加一个字段不用读折叠的
//! 状态机，改折叠也不用翻一百行结构体定义。

use serde::{Deserialize, Serialize};

use crate::types::ids::{ApprovalId, AttemptId, EventId, RunId, Seq, ShortId, ToolCallId};
use crate::types::plan::PlanHash;
use crate::types::refs::{OutputRef, PayloadRef, ToolResultStatus};
use crate::types::status::{RunState, ToolCallState, WaitReason};
use crate::types::turn::{Role, ToolCallRequest};

use super::Surface;

/// 会话消息面上的一条消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceMessage {
    pub seq: Seq,
    pub event_id: EventId,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// 正文外置时的引用；读取时按引用加载并校验哈希（§8.3）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_ref: Option<PayloadRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRequest>,
    /// 角色为 [`Role::Tool`] 的节点上，本轮回传的调用结果。**正文不在这里**：
    /// 只有引用与预览，完整输出在 `output.json`（§8.3）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<SurfaceToolResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_blocks: Option<serde_json::Value>,
}

/// 回放面上的一条工具结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceToolResult {
    pub call: ToolCallId,
    pub attempt: AttemptId,
    pub status: ToolResultStatus,
    pub output: OutputRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// 一个 Run 的派生状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunView {
    pub run: RunId,
    pub status: RunState,
    /// 停在什么上（只在 `status == Waiting` 时有意义，§8.4）。
    ///
    /// 它与状态是**两个维度**：状态说"能不能跑"，这一格说"在等谁、等到什么时候"。
    /// 少了它，"排队二十分钟"就只是一句状态，答不出为什么。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitReason>,
    /// 承载输入的事件（`run.accepted`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_event: Option<EventId>,
    /// 承载终态的事件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_event: Option<EventId>,
    /// 最终回复正文（用于"结果已保存但客户端没收到"的补读，§8.4）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    /// 领取代次，来自最后一条 `run.started`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// 这个 Run 里到目前为止跑过的模型轮次。
    #[serde(default)]
    pub rounds: u32,
    /// 这是一条子 Run：谁派的、父侧哪次调用、结果要长什么样（§4）。
    ///
    /// 父视图只靠这一条边认得出"哪些 Run 是我的子代理"；子 Run 自己靠它拿到结果契约，
    /// 重启之后也一样（它来自 `run.accepted`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate: Option<crate::types::delegate::DelegateSpec>,
    /// 属于它的调用，按出现顺序。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ToolCallId>,
    pub first_seq: Seq,
    pub last_seq: Seq,
}

impl RunView {
    /// 还有没有未完成的调用——§8.4 的"从未完成的 ToolCall 继续"。
    pub fn unfinished_calls<'a>(
        &'a self,
        surface: &'a Surface,
    ) -> impl Iterator<Item = &'a ToolCallView> {
        self.calls
            .iter()
            .filter_map(move |id| surface.calls.get(id))
            .filter(|call| !call.state.is_terminal())
    }
}

/// 一个 ToolCall 的最新状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallView {
    pub call: ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    pub state: ToolCallState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_hash: Option<PlanHash>,
    /// 承载计划的事件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_event: Option<EventId>,
    /// 最后一次尝试。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<AttemptId>,
    /// 已发布的输出引用（有它才谈得上"复用原输出"）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputRef>,
    /// 一共尝试了几次。
    #[serde(default)]
    pub attempts: u32,
    pub last_seq: Seq,
}

/// 一条还没有决定的审批。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalView {
    pub approval: ApprovalId,
    pub short_id: ShortId,
    pub plan_hash: PlanHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<ToolCallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    pub reason: String,
    pub requested_seq: Seq,
}

/// 一条这个版本读不懂的事件。保留下来，不解释（§8.3）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnknownEvent {
    pub seq: Seq,
    pub event_id: EventId,
    pub event_type: String,
    pub raw: serde_json::Value,
}

/// fold 读出来的不一致。**记录，不修复。**
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FoldViolation {
    /// 连续两条同角色消息。provider 回放会拒绝。
    ConsecutiveRole { seq: Seq, role: Role },
    /// seq 不连续：中间少了行。已提交范围缺失要停止受影响会话（§8.3）。
    SeqGap { expected: Seq, found: Seq },
    /// seq 没有严格递增。
    SeqOutOfOrder { previous: Seq, found: Seq },
    /// 结果事件指向的调用没有出现过。
    ResultWithoutCall { seq: Seq, call: ToolCallId },
}
