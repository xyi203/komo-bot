//! §8.4 的恢复决策表，一个纯函数。
//!
//! 「**Gateway 自动接续能够确定恢复位置的任务。用户只处理审批、时效或结果不明等确实
//! 需要判断的情况。**」
//!
//! 输入是三样观察的并置：JSONL 尾部最后看到什么、state.db 最后看到什么、以及被引用
//! 的输出文件校验成不成立。输出是那张表右列的动作。这里**不做任何 I/O**——观察由
//! runtime 采集，决定在这里，于是"重启后会怎样"可以被穷举成单元测试，而不是靠在真
//! 机器上拔电源来验证。
//!
//! 一条贯穿全表的规则：**回放只补索引与派生执行状态**，不调用工具、不发送外部请求、
//! 不消费授权，也不能覆盖 state.db 已记录的用户取消或权限撤销（§8.5）。

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::types::ids::{ApprovalId, ToolCallId};
use crate::types::status::{RunState, SessionState, WaitReason};

/// JSONL 尾部最后看到的东西。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogTail {
    /// 只用请求键预留了 Run ID，正文尚未完整写入。
    InputIncomplete,
    /// `run.accepted` 已同步，还没有 `run.queued` / `run.started`。
    InputPersisted,
    /// 已经在跑，但这一轮的完整 assistant 回复尚未保存。
    AwaitingModelReply,
    /// 完整 assistant 回复与该轮全部调用计划已保存。
    RoundPersisted { pending: PendingCall },
    /// Run 的终态事件已经写进 JSONL。
    Final,
}

/// 这一轮里最靠前的那个未完成调用停在哪。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingCall {
    /// 这一轮的调用都有结果了。
    None,
    /// `tool.planned` 已写，`tool.started` 没有——**确定尚未执行**。
    Planned { call: ToolCallId },
    /// `tool.started` 已写，没有结果。**started 本身不证明副作用已发生**（§8.5）。
    Started { call: ToolCallId },
    /// JSONL 已有 `tool.result`，state.db 可能落后。
    ResultPersisted { call: ToolCallId },
}

/// 被引用的输出文件校验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputCheck {
    /// 这一步没有输出引用要校验。
    NotApplicable,
    /// 引用存在，正文与哈希都对得上。
    Verified,
    /// 引用存在，正文缺失。
    Missing,
    /// 引用存在，正文在，哈希对不上。
    HashMismatch,
}

/// 审批的观察。**权威是 state.db**——JSONL 里的审计副本不能自行创建授权（§7.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalObservation {
    /// 这个 Run 没有停在审批上。
    None,
    /// 请求还在，没人答。
    Pending { approval: ApprovalId },
    /// 有答复了。
    Decided {
        approval: ApprovalId,
        approved: bool,
        /// 范围、计划、版本与有效期都还成立。
        still_valid: bool,
    },
    /// JSONL 说在等审批，state.db 里却没有这一行——拒绝执行，要求操作者重答（§8.2）。
    MissingFromDatabase,
}

/// Session 侧的观察（§8.9）。**这一维是 reconcile 存在的理由**：Run 自己的状态说得再
/// 清楚，也答不出"它所属的会话还在不在服务范围里、内容还读不读得出来"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionObservation {
    /// `sessions.state`（§8.10）。
    pub state: SessionState,
    /// 目录、JSONL 与被引用的输出此刻读得出来吗。`purged` 的会话本来就该是 `false`。
    pub content_available: bool,
}

impl Default for SessionObservation {
    fn default() -> Self {
        // 没给这一维的老调用方（测试替身、旧线格式）按"正常活跃会话"读——**不是**按
        // "内容缺失"读：默认成不可用会把每一条 Run 都停成 blocked。
        SessionObservation {
            state: SessionState::Active,
            content_available: true,
        }
    }
}

impl SessionObservation {
    /// 这个会话还服务吗——**领取一条 Run 之前问的就是它**（§8.9）。
    pub fn serves(&self) -> bool {
        self.state.serves() && self.content_available
    }

    /// 不服务时，说清是哪一种：给操作者看的理由（§7.5 的 `blocked` 条目正文）。
    pub fn blocked_reason(&self) -> Option<String> {
        match self.state {
            SessionState::Purged => {
                Some("这个会话的内容已经回收（`purged`），这条 Run 不会再被领走".into())
            }
            SessionState::Deleted => {
                Some("这个会话已被逻辑删除（`deleted`），这条 Run 不会再被领走".into())
            }
            SessionState::Active | SessionState::Closing => (!self.content_available).then(|| {
                format!(
                    "会话内容读不出来（数据库说它是 `{}`），不能按空上下文继续这条 Run",
                    self.state.as_str()
                )
            }),
        }
    }
}

/// 一个 Run 在重启后被观察到的样子。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryInput {
    /// state.db 里的状态。
    pub state: RunState,
    /// 停在什么上（`state == Waiting` 时有值）。**决策表按它分派等待中的那几行**——
    /// 这正是把状态与理由拆开之后，恢复侧要做的那一点改动：以前靠
    /// `waiting_approval` / `waiting_retry` / `needs_attention` 三个状态名分派，
    /// 现在靠一个 `WaitReason`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitReason>,
    /// JSONL 尾部。
    pub log_tail: LogTail,
    /// 输出文件校验。
    pub output_check: OutputCheck,
    #[serde(default = "no_approval")]
    pub approval: ApprovalObservation,
    /// 最终结果已经送达客户端了吗。
    #[serde(default)]
    pub result_delivered: bool,
    /// 上一个执行实例确认已经停止了吗。**无法确认时阻止重复启动**（§8.7）。
    #[serde(default = "yes")]
    pub previous_executor_stopped: bool,
    /// 会话还在不在服务范围里（§8.9）。
    #[serde(default)]
    pub session: SessionObservation,
}

fn no_approval() -> ApprovalObservation {
    ApprovalObservation::None
}

fn yes() -> bool {
    true
}

/// 恢复要做的事。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RecoveryAction {
    /// 补齐 state.db 索引后自动入队。
    BackfillIndexAndQueue,
    /// 保持 ingesting，等待同请求键重传。**不能凭输入哈希补造用户指令**（§8.5）。
    AwaitResend,
    /// 丢弃未完成输出，以已保存上下文重新请求模型。可能再次产生模型费用。
    DiscardPartialAndRerequestModel,
    /// 沿用原计划继续这一轮——这一轮的调用都有结果了，接着请求下一轮模型。
    ResumeFromPlan,
    /// 校验原计划与当前权限后自动执行这个调用。
    ExecutePlannedCall { call: ToolCallId },
    /// 先核对外部效果，按 §8.6 决定是否安全继续。
    VerifyEffect { call: ToolCallId },
    /// 补齐结果索引和状态，**复用原输出，不重放动作**。
    BackfillResultAndContinue { call: ToolCallId },
    /// **原样停着，这一趟什么都不做**：它在等审批答复、等操作者在清单上答、或者等前
    /// 一条 Run 进终态。判断"什么时候能再跑"是 reconcile 的事（审批回答 / 到点 / 前一条
    /// 终态），不是恢复决策的事。
    KeepWaiting { reason: WaitReason },
    /// 已答复且仍有效：按原决定继续，**不因重启再问一次**。
    ResumeWithDecision {
        approval: ApprovalId,
        approved: bool,
    },
    /// 沿用已保存的次数与到点时刻，到期再尝试。
    WaitUntilRetry {
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
        attempts: u32,
    },
    /// 补发或补读原结果，**不重新执行任务**。
    RedeliverResult,
    /// 保持终态，不因重启自动开启新一轮。
    KeepTerminal { state: RunState },
    /// 停止受影响任务并报告损坏。**不重跑来掩盖数据损坏**（§8.5）。
    HaltCorrupt { reason: String },
    /// 需要操作者判断：把这条 Run 停成 `waiting + intervention`，它会出现在 §7.5 的
    /// 清单里等人答复。**不是"卡住"**——是这条 Run 确实有一个只有人能回答的问题。
    WaitingForOperator { reason: String },
}

/// §8.4 的决策表。
pub fn decide(observed: &RecoveryInput) -> RecoveryAction {
    // 会话这一维最靠前（§8.9）：一条 Run 自己的状态说得再清楚，也答不出"它所属的会话还
    // 在不在服务范围里、内容还读不读得出来"。**按空上下文继续一轮比停下来更糟**——那是
    // 把一个已经不存在的对话接下去，而模型会照着它编。
    if !observed.state.is_terminal()
        && let Some(reason) = observed.session.blocked_reason()
    {
        return RecoveryAction::WaitingForOperator { reason };
    }

    // 引用损坏先于一切：「若结果引用存在但对应输出缺失或哈希不符，停止受影响任务，
    // 不能重跑来掩盖数据损坏」（§8.5）。
    match observed.output_check {
        OutputCheck::Missing => {
            return RecoveryAction::HaltCorrupt {
                reason: "结果引用存在，但引用的输出正文缺失".into(),
            };
        }
        OutputCheck::HashMismatch => {
            return RecoveryAction::HaltCorrupt {
                reason: "结果引用存在，但引用的输出内容哈希不符".into(),
            };
        }
        OutputCheck::NotApplicable | OutputCheck::Verified => {}
    }

    // 「无法确认旧执行已结束时，阻止该任务重复启动并显示原因」（§8.7）。
    if !observed.previous_executor_stopped && !observed.state.is_terminal() {
        return RecoveryAction::WaitingForOperator {
            reason: "无法确认上一个执行实例已经结束，暂不重复启动".into(),
        };
    }

    match observed.state {
        // 终态：只差投递的话补投，别的一律不动。
        RunState::Completed | RunState::Failed | RunState::Cancelled | RunState::Abandoned => {
            if observed.result_delivered {
                RecoveryAction::KeepTerminal {
                    state: observed.state,
                }
            } else {
                RecoveryAction::RedeliverResult
            }
        }
        // 停在外部条件上：按"在等什么"分派。
        RunState::Waiting => decide_while_waiting(observed),
        // accepted / queued / running：位置由 JSONL 尾部决定。`running` 且没有主人的那些
        // 是 reconcile 的孤儿（§8.9），走到这里说明领取权已经交还，同样按尾部判。
        RunState::Accepted | RunState::Queued | RunState::Running => decide_by_log_tail(observed),
    }
}

fn decide_while_waiting(observed: &RecoveryInput) -> RecoveryAction {
    match &observed.wait {
        // 第 8 行：等审批 → 保留原请求；已答复且仍有效的批准自动接续。
        Some(WaitReason::Approval { approval }) => match &observed.approval {
            ApprovalObservation::Pending { .. } => RecoveryAction::KeepWaiting {
                reason: observed.wait.clone().expect("刚刚匹配过"),
            },
            ApprovalObservation::Decided {
                approval,
                approved,
                still_valid: true,
            } => RecoveryAction::ResumeWithDecision {
                approval: approval.clone(),
                approved: *approved,
            },
            ApprovalObservation::Decided {
                still_valid: false, ..
            } => RecoveryAction::WaitingForOperator {
                reason: "授权的范围、计划、版本或有效期已经变化，需要重新审核".into(),
            },
            ApprovalObservation::MissingFromDatabase => RecoveryAction::WaitingForOperator {
                reason: "数据库里没有这条审批记录；JSONL 的审计副本不能创建授权，请重答".into(),
            },
            ApprovalObservation::None => RecoveryAction::WaitingForOperator {
                reason: format!("状态说在等审批 {approval}，却找不到对应的请求"),
            },
        },
        // 第 9 行：等退避 → 沿用已保存的次数与到点时刻，到期再尝试（重启不重置预算）。
        Some(WaitReason::Retry {
            attempts,
            not_before,
            ..
        }) => RecoveryAction::WaitUntilRetry {
            at: *not_before,
            attempts: *attempts,
        },
        // 停在需要人判断上：**原样保留**。它不是"卡住"，是清单里的一条，等操作者答复
        // （§7.5）。恢复扫描不替人答，也不偷偷往下跑。
        Some(WaitReason::Intervention { .. }) => RecoveryAction::KeepWaiting {
            reason: observed.wait.clone().expect("刚刚匹配过"),
        },
        // 等前一条 Run：谁放它出来由 reconcile 判（前一条进终态 → 回 `queued`）。
        Some(WaitReason::Dependency { .. }) => RecoveryAction::KeepWaiting {
            reason: observed.wait.clone().expect("刚刚匹配过"),
        },
        // 状态说在等，却说不清在等什么——这是损坏，不是"没关系"。
        None => RecoveryAction::WaitingForOperator {
            reason: "状态是等待，却没有记录在等什么".into(),
        },
    }
}

fn decide_by_log_tail(observed: &RecoveryInput) -> RecoveryAction {
    match &observed.log_tail {
        // 第 2 行。
        LogTail::InputIncomplete => RecoveryAction::AwaitResend,
        // 第 1 行。
        LogTail::InputPersisted => RecoveryAction::BackfillIndexAndQueue,
        // 第 3 行。
        LogTail::AwaitingModelReply => RecoveryAction::DiscardPartialAndRerequestModel,
        // 第 4 行及其三个细分。
        LogTail::RoundPersisted { pending } => match pending {
            PendingCall::None => RecoveryAction::ResumeFromPlan,
            // 第 6 行。
            PendingCall::Planned { call } => {
                RecoveryAction::ExecutePlannedCall { call: call.clone() }
            }
            // 第 7 行。
            PendingCall::Started { call } => RecoveryAction::VerifyEffect { call: call.clone() },
            // 第 5 行。
            PendingCall::ResultPersisted { call } => {
                RecoveryAction::BackfillResultAndContinue { call: call.clone() }
            }
        },
        // JSONL 说结束了而数据库还没提交终态：补索引，不重跑。
        LogTail::Final => RecoveryAction::BackfillIndexAndQueue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::{InterventionId, RunId};
    use crate::types::status::RetryCause;
    use time::macros::datetime;

    fn call() -> ToolCallId {
        ToolCallId::from_raw("call-7")
    }

    fn input(state: RunState, log_tail: LogTail) -> RecoveryInput {
        RecoveryInput {
            state,
            wait: None,
            log_tail,
            output_check: OutputCheck::NotApplicable,
            approval: ApprovalObservation::None,
            result_delivered: false,
            previous_executor_stopped: true,
            session: SessionObservation::default(),
        }
    }

    /// 停在某个外部条件上的 Run（§8.4：状态只有一个 `waiting`，理由在 `WaitReason`）。
    fn waiting(reason: WaitReason, log_tail: LogTail) -> RecoveryInput {
        let mut observed = input(RunState::Waiting, log_tail);
        observed.wait = Some(reason);
        observed
    }

    // ---- §8.4「重启前停在哪里 → Gateway 启动后怎么处理」逐行 ----

    /// 第 1 行：输入已在 JSONL 持久保存，Run 尚未开始 → 补齐 state.db 索引后自动入队。
    #[test]
    fn row_1_input_persisted_but_the_run_never_started() {
        let observed = input(RunState::Accepted, LogTail::InputPersisted);
        assert_eq!(decide(&observed), RecoveryAction::BackfillIndexAndQueue);
    }

    /// 第 2 行：输入请求只预留了 ID，正文尚未完整写入 → 保持 ingesting；等待同请求键
    /// 重传，不能执行缺失输入。
    #[test]
    fn row_2_only_the_id_was_reserved_and_the_body_never_landed() {
        let observed = input(RunState::Accepted, LogTail::InputIncomplete);
        assert_eq!(decide(&observed), RecoveryAction::AwaitResend);
    }

    /// 第 3 行：正在请求 LLM，完整回复尚未保存 → 丢弃未完成输出，以已保存上下文重新
    /// 请求；可能再次产生模型费用。
    #[test]
    fn row_3_the_model_reply_never_arrived_in_full() {
        let observed = input(RunState::Running, LogTail::AwaitingModelReply);
        assert_eq!(
            decide(&observed),
            RecoveryAction::DiscardPartialAndRerequestModel
        );
    }

    /// 第 4 行：assistant 回复和调用计划已保存 → 沿用原计划，从未完成的 ToolCall 继续。
    #[test]
    fn row_4_the_round_is_persisted_and_nothing_is_left_unfinished() {
        let observed = input(
            RunState::Running,
            LogTail::RoundPersisted {
                pending: PendingCall::None,
            },
        );
        assert_eq!(decide(&observed), RecoveryAction::ResumeFromPlan);
    }

    /// 第 5 行：JSONL 已有结果引用且完整输出校验通过，state.db 可能落后 → 先补齐结果
    /// 索引和状态，**复用原输出，不重放动作**。
    #[test]
    fn row_5_the_result_is_on_disk_and_verified_while_the_database_lags() {
        let mut observed = input(
            RunState::Running,
            LogTail::RoundPersisted {
                pending: PendingCall::ResultPersisted { call: call() },
            },
        );
        observed.output_check = OutputCheck::Verified;
        assert_eq!(
            decide(&observed),
            RecoveryAction::BackfillResultAndContinue { call: call() }
        );
    }

    /// 第 6 行：调用 planned，确定尚未执行 → 校验原计划与当前权限后自动执行。
    #[test]
    fn row_6_the_call_is_planned_and_certainly_never_ran() {
        let observed = input(
            RunState::Running,
            LogTail::RoundPersisted {
                pending: PendingCall::Planned { call: call() },
            },
        );
        assert_eq!(
            decide(&observed),
            RecoveryAction::ExecutePlannedCall { call: call() }
        );
    }

    /// 第 7 行：调用 started，没有结果 → 先核对外部效果，按 8.6 决定是否安全继续。
    #[test]
    fn row_7_the_call_started_and_left_no_result() {
        let observed = input(
            RunState::Running,
            LogTail::RoundPersisted {
                pending: PendingCall::Started { call: call() },
            },
        );
        assert_eq!(
            decide(&observed),
            RecoveryAction::VerifyEffect { call: call() },
            "started 本身不证明副作用已发生，也不证明没发生"
        );
    }

    /// 第 8 行：等待审批 → 保留原请求；已答复且仍有效的批准自动接续。
    #[test]
    fn row_8_waiting_on_an_approval() {
        let approval = ApprovalId::from_raw("ap-1");

        let mut pending = waiting(
            WaitReason::Approval {
                approval: approval.clone(),
            },
            LogTail::AwaitingModelReply,
        );
        pending.approval = ApprovalObservation::Pending {
            approval: approval.clone(),
        };
        assert_eq!(
            decide(&pending),
            RecoveryAction::KeepWaiting {
                reason: WaitReason::Approval {
                    approval: approval.clone()
                }
            },
            "保留原请求，等答复"
        );

        let mut answered = pending.clone();
        answered.approval = ApprovalObservation::Decided {
            approval: approval.clone(),
            approved: true,
            still_valid: true,
        };
        assert_eq!(
            decide(&answered),
            RecoveryAction::ResumeWithDecision {
                approval: approval.clone(),
                approved: true
            },
            "审批无需用户因重启再答一次"
        );

        let mut stale = pending.clone();
        stale.approval = ApprovalObservation::Decided {
            approval,
            approved: true,
            still_valid: false,
        };
        assert!(
            matches!(decide(&stale), RecoveryAction::WaitingForOperator { .. }),
            "范围、计划、版本或有效期变化就重新审核"
        );
    }

    /// 第 9 行：等待临时故障重试 → 沿用已保存的次数与 next_retry_at，到期再尝试。
    #[test]
    fn row_9_waiting_for_a_backoff_to_expire() {
        let at = datetime!(2026-09-15 08:05:00 UTC);
        let observed = waiting(
            WaitReason::Retry {
                attempts: 2,
                not_before: at,
                cause: RetryCause::RateLimited,
            },
            LogTail::AwaitingModelReply,
        );
        assert_eq!(
            decide(&observed),
            RecoveryAction::WaitUntilRetry { at, attempts: 2 },
            "重启不重置预算，到点再试"
        );
    }

    /// 第 10 行：已保存最终结果，但客户端没有收到 → 补发或补读原结果，不重新执行任务。
    #[test]
    fn row_10_the_result_is_final_but_the_client_never_saw_it() {
        let mut observed = input(RunState::Completed, LogTail::Final);
        observed.result_delivered = false;
        assert_eq!(decide(&observed), RecoveryAction::RedeliverResult);
    }

    /// 第 11 行：completed / failed / cancelled → 保持终态，不因重启自动开启新一轮。
    #[test]
    fn row_11_a_terminal_run_stays_terminal() {
        for state in [
            RunState::Completed,
            RunState::Failed,
            RunState::Cancelled,
            RunState::Abandoned,
        ] {
            let mut observed = input(state, LogTail::Final);
            observed.result_delivered = true;
            assert_eq!(decide(&observed), RecoveryAction::KeepTerminal { state });
        }
    }

    // ---- 表外但同一段落规定的几条 ----

    /// §8.9 加的三行（§8.4 的同一条要求）：会话不在服务范围里，或者内容读不出来时，
    /// **这条 Run 不许被领走**——按空上下文续一轮比停下来更糟。
    #[test]
    fn a_session_that_cannot_be_served_never_gets_its_run_claimed() {
        // active + 内容读不出来：说"读不出来"，不是"会话没了"。
        let mut observed = input(RunState::Queued, LogTail::InputPersisted);
        observed.session = SessionObservation {
            state: SessionState::Active,
            content_available: false,
        };
        let RecoveryAction::WaitingForOperator { reason } = decide(&observed) else {
            panic!("内容缺失要停在等人处理上")
        };
        assert!(reason.contains("读不出来"), "{reason}");

        // deleted：逻辑删除，内容还在但不再服务。
        observed.session = SessionObservation {
            state: SessionState::Deleted,
            content_available: true,
        };
        let RecoveryAction::WaitingForOperator { reason } = decide(&observed) else {
            panic!("已逻辑删除的会话不该被续跑")
        };
        assert!(reason.contains("逻辑删除"), "{reason}");

        // purged：内容已回收，理由要与"读不出来"分得开。
        observed.session = SessionObservation {
            state: SessionState::Purged,
            content_available: false,
        };
        let RecoveryAction::WaitingForOperator { reason } = decide(&observed) else {
            panic!("已回收的会话不该被续跑")
        };
        assert!(reason.contains("回收"), "{reason}");
    }

    /// `closing` 仍然服务：它只是不再收新活，**手里的活要跑完**（§8.10）。
    #[test]
    fn closing_still_serves_the_work_it_already_has() {
        let mut observed = input(RunState::Queued, LogTail::InputPersisted);
        observed.session = SessionObservation {
            state: SessionState::Closing,
            content_available: true,
        };
        assert_eq!(decide(&observed), RecoveryAction::BackfillIndexAndQueue);
    }

    /// 停在需要人判断上（`WaitReason::Intervention`）：恢复扫描**不替人答**，也不偷偷
    /// 往下跑——它是清单里的一条，等操作者（§7.5）。
    #[test]
    fn a_run_waiting_for_an_operator_is_left_alone() {
        let observed = waiting(
            WaitReason::Intervention {
                intervention: InterventionId::for_run(&RunId::from_raw("run-1")),
            },
            LogTail::AwaitingModelReply,
        );
        assert_eq!(
            decide(&observed),
            RecoveryAction::KeepWaiting {
                reason: observed.wait.clone().unwrap()
            }
        );
    }

    /// 等前一条 Run 也一样：谁放它出来是 reconcile 的事（前一条进终态 → 回 `queued`）。
    #[test]
    fn a_dependent_run_is_left_to_reconcile() {
        let observed = waiting(
            WaitReason::Dependency {
                run: RunId::from_raw("run-earlier"),
            },
            LogTail::InputPersisted,
        );
        assert!(matches!(
            decide(&observed),
            RecoveryAction::KeepWaiting { .. }
        ));
    }

    /// 终态的 Run 不因为会话被删而改判：已经结束的事，回收内容不会把它变成"需要处理"
    /// ——它只是那条投递还能不能补的问题（§8.4 第 10 行）。
    #[test]
    fn a_terminal_run_is_unaffected_by_its_session_being_purged() {
        let mut observed = input(RunState::Completed, LogTail::Final);
        observed.session = SessionObservation {
            state: SessionState::Purged,
            content_available: false,
        };
        assert_eq!(decide(&observed), RecoveryAction::RedeliverResult);
    }

    #[test]
    fn a_missing_or_altered_output_body_stops_the_task_instead_of_rerunning_it() {
        for check in [OutputCheck::Missing, OutputCheck::HashMismatch] {
            let mut observed = input(
                RunState::Running,
                LogTail::RoundPersisted {
                    pending: PendingCall::ResultPersisted { call: call() },
                },
            );
            observed.output_check = check;
            assert!(
                matches!(decide(&observed), RecoveryAction::HaltCorrupt { .. }),
                "{check:?}"
            );
        }
    }

    #[test]
    fn a_cancelled_run_is_not_revived_even_with_work_left_in_the_log() {
        let mut observed = input(
            RunState::Cancelled,
            LogTail::RoundPersisted {
                pending: PendingCall::Planned { call: call() },
            },
        );
        observed.result_delivered = true;
        assert_eq!(
            decide(&observed),
            RecoveryAction::KeepTerminal {
                state: RunState::Cancelled
            },
            "用户明确取消的 Run 不自动复活"
        );
    }

    #[test]
    fn a_surviving_previous_executor_blocks_a_second_start() {
        let mut observed = input(
            RunState::Running,
            LogTail::RoundPersisted {
                pending: PendingCall::Planned { call: call() },
            },
        );
        observed.previous_executor_stopped = false;
        assert!(matches!(
            decide(&observed),
            RecoveryAction::WaitingForOperator { .. }
        ));
    }

    #[test]
    fn an_approval_the_database_lost_asks_the_operator_again() {
        let mut observed = waiting(
            WaitReason::Approval {
                approval: ApprovalId::from_raw("ap-1"),
            },
            LogTail::AwaitingModelReply,
        );
        observed.approval = ApprovalObservation::MissingFromDatabase;
        let action = decide(&observed);
        let RecoveryAction::WaitingForOperator { reason } = &action else {
            panic!("{action:?}")
        };
        assert!(reason.contains("审计副本"), "{reason}");
    }

    /// 状态说在等，却说不清在等什么——这是损坏，不是"没关系"。退避预算用完那种情形
    /// 在**造等待的时候**就该判（预算用完就不该造一个 `Retry`），所以恢复这一层见到
    /// 一个没有理由的 `waiting` 只能是坏行。
    #[test]
    fn a_wait_without_a_reason_is_corruption_not_a_quiet_park() {
        let observed = input(RunState::Waiting, LogTail::AwaitingModelReply);
        let action = decide(&observed);
        let RecoveryAction::WaitingForOperator { reason } = &action else {
            panic!("{action:?}")
        };
        assert!(reason.contains("没有记录在等什么"), "{reason}");
    }

    #[test]
    fn a_recovery_input_round_trips_through_json() {
        let observed = input(
            RunState::Running,
            LogTail::RoundPersisted {
                pending: PendingCall::Started { call: call() },
            },
        );
        let text = serde_json::to_string(&observed).unwrap();
        assert_eq!(
            serde_json::from_str::<RecoveryInput>(&text).unwrap(),
            observed
        );
    }
}
