//! Run / ToolCall / Attempt 的状态机（§6、§8.1、§8.4）。

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::ids::{ApprovalId, AttemptId, EventId, RunId, ToolCallId};

/// Run 的状态。§8.4 的状态图逐个列出，没有第十一个。
///
/// ```text
/// ingesting → queued → running → completed / failed / cancelled
///             ├─ waiting_approval → queued
///             ├─ waiting_retry    → queued
///             ├─ interrupted      → 核对后 queued / needs_attention
///             └─ needs_attention  → 操作者处理后 queued / cancelled
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// 已用请求键预留 ID，正文可能还没写完（§8.5）。
    Ingesting,
    /// 输入已持久保存，等待领取。
    Queued,
    /// 已被某个执行者领取。
    Running,
    /// 停在一条审批上，已释放执行名额（§7.4）。
    WaitingApproval,
    /// 停在一次有界退避上，次数与 `next_retry_at` 已持久化（§8.5）。
    WaitingRetry,
    /// 上一个执行实例没有正常收尾——停机或崩溃，不等于用户取消（§8.4）。
    Interrupted,
    /// 需要操作者判断：结果不明、无可靠恢复方式、引用损坏（§8.6）。
    NeedsAttention,
    /// 本轮有明确终态。
    Completed,
    Failed,
    /// 用户明确取消，不自动复活（§8.4）。
    Cancelled,
}

impl RunStatus {
    /// 终态：重启不会让它再开一轮。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        )
    }

    /// 未完成状态。§8.4：`waiting_approval` / `waiting_retry` / `interrupted` /
    /// `needs_attention` 都是未完成，同一 Session 的后续 Run 不越过它。
    pub fn is_unfinished(self) -> bool {
        !self.is_terminal()
    }

    /// 还在排队等领取：输入**已经落盘、但还没进会话**（§8.5 的接收顺序 + §8.4 的次序）。
    ///
    /// 回放窗口要跳过这一种的用户消息。照搬日志位置发出去，provider 看到的是"助手要了
    /// 一次调用、紧接着另一个 Run 的用户消息、最后才是那次调用的输出"，直接 400
    /// （`No tool output found for tool call …`）。
    pub fn awaits_claim(self) -> bool {
        matches!(self, RunStatus::Ingesting | RunStatus::Queued)
    }

    /// 正在等人或等时钟，已经让出执行名额。
    pub fn is_waiting(self) -> bool {
        matches!(
            self,
            RunStatus::WaitingApproval | RunStatus::WaitingRetry | RunStatus::NeedsAttention
        )
    }

    /// 可以被调度器领取。
    pub fn is_claimable(self) -> bool {
        matches!(self, RunStatus::Queued)
    }
}

/// ToolCall 的状态（§8.4）。`uncertain` 是这里的状态，不是 Run 的——副作用可能已经
/// 发生而完整输出没有落盘，这段窗口必须保留，不能用"重试成功"盖掉（§8.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallState {
    /// 计划已持久保存，确定尚未执行。
    Planned,
    /// `tool.started` 已提交；此后才允许产生真实副作用。
    Started,
    Completed,
    Failed,
    /// 结果不明：先核对目标状态，再决定是否重试（§8.6）。
    Uncertain,
}

impl ToolCallState {
    pub fn is_terminal(self) -> bool {
        matches!(self, ToolCallState::Completed | ToolCallState::Failed)
    }

    /// 需要走 §8.6 的核对流程。
    pub fn needs_verification(self) -> bool {
        matches!(self, ToolCallState::Started | ToolCallState::Uncertain)
    }
}

/// 一次实际执行尝试的状态（`tool_attempts`）。一次重试沿用 ToolCall ID，新增一条
/// attempt（§8.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Started,
    Completed,
    Failed,
    /// 执行实例没有收尾就消失了（停机 / 崩溃）。
    Interrupted,
}

impl AttemptState {
    pub fn is_terminal(self) -> bool {
        matches!(self, AttemptState::Completed | AttemptState::Failed)
    }
}

/// `Ledger::suspend` 的参数：Run 为什么让出执行名额。每个变体对应一个
/// [`RunStatus`]（§13.5 的方法注释）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Wait {
    /// 等审批。`approval` 是 `approval_requests` 里的那一条。
    Approval {
        approval: ApprovalId,
        /// 停在哪个**逻辑调用**上；没有工具调用的审批（例如 Memory 内部变更）为 None。
        /// 它与 `run.waiting_approval` 事件里的 `call` 是同一个东西，所以是同一个类型。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<ToolCallId>,
        /// 停在哪次**尝试**上。**通常是 None**：审批发生在 `tool.started` 之前，那时候
        /// 一次尝试都还没有；只有"执行到一半才发现要再批一次"这类情形才填得出来。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<AttemptId>,
    },
    /// 等一次有界退避到期。
    Retry {
        attempts: u32,
        #[serde(with = "time::serde::rfc3339")]
        next_retry_at: OffsetDateTime,
        reason: String,
    },
    /// 需要操作者判断。
    Attention { reason: String },
}

impl Wait {
    pub fn status(&self) -> RunStatus {
        match self {
            Wait::Approval { .. } => RunStatus::WaitingApproval,
            Wait::Retry { .. } => RunStatus::WaitingRetry,
            Wait::Attention { .. } => RunStatus::NeedsAttention,
        }
    }
}

/// `Ledger::complete` 的参数：Run 怎么结束的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEnd {
    /// 正常结束，带最终回复正文（大正文由调用方先外置）。
    Completed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_message: Option<String>,
        /// 这个 Run 一共跑了几轮模型。`run inspect` 与预算核对读它——`RunCompleted.rounds`
        /// 原先恒为 0，因为账本接口上根本没地方把这个数交进来。
        #[serde(default)]
        rounds: u32,
    },
    Failed {
        reason: String,
    },
    /// 用户明确取消。
    Cancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
}

impl RunEnd {
    pub fn status(&self) -> RunStatus {
        match self {
            RunEnd::Completed { .. } => RunStatus::Completed,
            RunEnd::Failed { .. } => RunStatus::Failed,
            RunEnd::Cancelled { .. } => RunStatus::Cancelled,
        }
    }
}

/// 调度器领取到的一个 Run（§8.7：条件更新 + 递增代次）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claimed {
    pub run: RunId,
    /// 领取代次。JSONL 追加与状态提交都校验它；旧代次不能继续提交新状态。
    pub generation: u64,
}

/// Run 的最终事件引用，用于"结果已保存但客户端没收到"的补读（§8.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalEventRef {
    pub event: EventId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_three_run_statuses_are_terminal() {
        let all = [
            RunStatus::Ingesting,
            RunStatus::Queued,
            RunStatus::Running,
            RunStatus::WaitingApproval,
            RunStatus::WaitingRetry,
            RunStatus::Interrupted,
            RunStatus::NeedsAttention,
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ];
        let terminal: Vec<_> = all.into_iter().filter(|s| s.is_terminal()).collect();
        assert_eq!(
            terminal,
            vec![
                RunStatus::Completed,
                RunStatus::Failed,
                RunStatus::Cancelled
            ]
        );
        assert!(all.iter().all(|s| s.is_terminal() != s.is_unfinished()));
    }

    #[test]
    fn interrupted_is_unfinished_but_not_waiting() {
        assert!(RunStatus::Interrupted.is_unfinished());
        assert!(!RunStatus::Interrupted.is_waiting());
    }

    #[test]
    fn started_and_uncertain_calls_both_need_verification() {
        assert!(ToolCallState::Started.needs_verification());
        assert!(ToolCallState::Uncertain.needs_verification());
        assert!(!ToolCallState::Completed.needs_verification());
        assert!(!ToolCallState::Planned.needs_verification());
    }

    #[test]
    fn a_wait_names_the_status_it_puts_the_run_in() {
        let approval = Wait::Approval {
            approval: ApprovalId::from_raw("a-1"),
            call: Some(ToolCallId::from_raw("call-7")),
            attempt: None,
        };
        assert_eq!(approval.status(), RunStatus::WaitingApproval);
        let text = serde_json::to_string(&approval).unwrap();
        assert!(
            !text.contains("attempt"),
            "停在审批上的调用通常还没有尝试：{text}"
        );
        assert_eq!(serde_json::from_str::<Wait>(&text).unwrap(), approval);

        let end = RunEnd::Completed {
            final_message: Some("等于 2".into()),
            rounds: 2,
        };
        assert_eq!(end.status(), RunStatus::Completed);
        let old = r#"{"kind":"completed","final_message":"好了"}"#;
        assert_eq!(
            serde_json::from_str::<RunEnd>(old).unwrap(),
            RunEnd::Completed {
                final_message: Some("好了".into()),
                rounds: 0
            },
            "老行没有 rounds，读为默认"
        );
        assert_eq!(
            Wait::Attention {
                reason: "结果不明".into()
            }
            .status(),
            RunStatus::NeedsAttention
        );
    }

    #[test]
    fn statuses_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&RunStatus::WaitingApproval).unwrap(),
            "\"waiting_approval\""
        );
        assert_eq!(
            serde_json::to_string(&ToolCallState::Uncertain).unwrap(),
            "\"uncertain\""
        );
    }
}
