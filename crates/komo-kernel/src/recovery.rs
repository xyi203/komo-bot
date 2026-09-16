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
use crate::types::status::RunStatus;

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

/// 临时故障重试的观察。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryObservation {
    /// 已经用掉的次数。**重启不重置预算**（§8.5）。
    pub attempts: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub next_retry_at: OffsetDateTime,
    /// 预算已经用完了。
    #[serde(default)]
    pub exhausted: bool,
}

/// 一个 Run 在重启后被观察到的样子。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryInput {
    /// state.db 里的状态。
    pub db_status: RunStatus,
    /// JSONL 尾部。
    pub log_tail: LogTail,
    /// 输出文件校验。
    pub output_check: OutputCheck,
    #[serde(default = "no_approval")]
    pub approval: ApprovalObservation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryObservation>,
    /// 最终结果已经送达客户端了吗。
    #[serde(default)]
    pub result_delivered: bool,
    /// 上一个执行实例确认已经停止了吗。**无法确认时阻止重复启动**（§8.7）。
    #[serde(default = "yes")]
    pub previous_executor_stopped: bool,
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
    /// 保留原审批请求，继续等。
    KeepWaitingApproval { approval: ApprovalId },
    /// 已答复且仍有效：按原决定继续，**不因重启再问一次**。
    ResumeWithDecision {
        approval: ApprovalId,
        approved: bool,
    },
    /// 沿用已保存的次数与 `next_retry_at`，到期再尝试。
    WaitUntilRetry {
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
        attempts: u32,
    },
    /// 补发或补读原结果，**不重新执行任务**。
    RedeliverResult,
    /// 保持终态，不因重启自动开启新一轮。
    KeepTerminal { status: RunStatus },
    /// 停止受影响任务并报告损坏。**不重跑来掩盖数据损坏**（§8.5）。
    HaltCorrupt { reason: String },
    /// 需要操作者判断。
    NeedsAttention { reason: String },
}

/// §8.4 的决策表。
pub fn decide(observed: &RecoveryInput) -> RecoveryAction {
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
    if !observed.previous_executor_stopped && !observed.db_status.is_terminal() {
        return RecoveryAction::NeedsAttention {
            reason: "无法确认上一个执行实例已经结束，暂不重复启动".into(),
        };
    }

    match observed.db_status {
        // 第 11 行 / 第 10 行。
        RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled => {
            if observed.result_delivered {
                RecoveryAction::KeepTerminal {
                    status: observed.db_status,
                }
            } else {
                RecoveryAction::RedeliverResult
            }
        }
        // 第 8 行。
        RunStatus::WaitingApproval => match &observed.approval {
            ApprovalObservation::Pending { approval } => RecoveryAction::KeepWaitingApproval {
                approval: approval.clone(),
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
            } => RecoveryAction::NeedsAttention {
                reason: "授权的范围、计划、版本或有效期已经变化，需要重新审核".into(),
            },
            ApprovalObservation::MissingFromDatabase => RecoveryAction::NeedsAttention {
                reason: "数据库里没有这条审批记录；JSONL 的审计副本不能创建授权，请重答".into(),
            },
            ApprovalObservation::None => RecoveryAction::NeedsAttention {
                reason: "状态是等待审批，却找不到对应的审批请求".into(),
            },
        },
        // 第 9 行。
        RunStatus::WaitingRetry => match &observed.retry {
            Some(retry) if retry.exhausted => RecoveryAction::NeedsAttention {
                reason: "重试预算已用完".into(),
            },
            Some(retry) => RecoveryAction::WaitUntilRetry {
                at: retry.next_retry_at,
                attempts: retry.attempts,
            },
            None => RecoveryAction::NeedsAttention {
                reason: "状态是等待重试，却没有已保存的次数与下次时间".into(),
            },
        },
        RunStatus::NeedsAttention => RecoveryAction::NeedsAttention {
            reason: "上一次已经停在这里，等操作者处理".into(),
        },
        // ingesting / queued / running / interrupted：位置由 JSONL 尾部决定。
        RunStatus::Ingesting | RunStatus::Queued | RunStatus::Running | RunStatus::Interrupted => {
            decide_by_log_tail(observed)
        }
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
    use time::macros::datetime;

    fn call() -> ToolCallId {
        ToolCallId::from_raw("call-7")
    }

    fn input(db_status: RunStatus, log_tail: LogTail) -> RecoveryInput {
        RecoveryInput {
            db_status,
            log_tail,
            output_check: OutputCheck::NotApplicable,
            approval: ApprovalObservation::None,
            retry: None,
            result_delivered: false,
            previous_executor_stopped: true,
        }
    }

    // ---- §8.4「重启前停在哪里 → Gateway 启动后怎么处理」逐行 ----

    /// 第 1 行：输入已在 JSONL 持久保存，Run 尚未开始 → 补齐 state.db 索引后自动入队。
    #[test]
    fn row_1_input_persisted_but_the_run_never_started() {
        let observed = input(RunStatus::Ingesting, LogTail::InputPersisted);
        assert_eq!(decide(&observed), RecoveryAction::BackfillIndexAndQueue);
    }

    /// 第 2 行：输入请求只预留了 ID，正文尚未完整写入 → 保持 ingesting；等待同请求键
    /// 重传，不能执行缺失输入。
    #[test]
    fn row_2_only_the_id_was_reserved_and_the_body_never_landed() {
        let observed = input(RunStatus::Ingesting, LogTail::InputIncomplete);
        assert_eq!(decide(&observed), RecoveryAction::AwaitResend);
    }

    /// 第 3 行：正在请求 LLM，完整回复尚未保存 → 丢弃未完成输出，以已保存上下文重新
    /// 请求；可能再次产生模型费用。
    #[test]
    fn row_3_the_model_reply_never_arrived_in_full() {
        let observed = input(RunStatus::Running, LogTail::AwaitingModelReply);
        assert_eq!(
            decide(&observed),
            RecoveryAction::DiscardPartialAndRerequestModel
        );
    }

    /// 第 4 行：assistant 回复和调用计划已保存 → 沿用原计划，从未完成的 ToolCall 继续。
    #[test]
    fn row_4_the_round_is_persisted_and_nothing_is_left_unfinished() {
        let observed = input(
            RunStatus::Interrupted,
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
            RunStatus::Running,
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
            RunStatus::Interrupted,
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
            RunStatus::Interrupted,
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

        let mut pending = input(RunStatus::WaitingApproval, LogTail::AwaitingModelReply);
        pending.approval = ApprovalObservation::Pending {
            approval: approval.clone(),
        };
        assert_eq!(
            decide(&pending),
            RecoveryAction::KeepWaitingApproval {
                approval: approval.clone()
            }
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
            matches!(decide(&stale), RecoveryAction::NeedsAttention { .. }),
            "范围、计划、版本或有效期变化就重新审核"
        );
    }

    /// 第 9 行：等待临时故障重试 → 沿用已保存的次数与 next_retry_at，到期再尝试。
    #[test]
    fn row_9_waiting_for_a_backoff_to_expire() {
        let at = datetime!(2026-09-15 08:05:00 UTC);
        let mut observed = input(RunStatus::WaitingRetry, LogTail::AwaitingModelReply);
        observed.retry = Some(RetryObservation {
            attempts: 2,
            next_retry_at: at,
            exhausted: false,
        });
        assert_eq!(
            decide(&observed),
            RecoveryAction::WaitUntilRetry { at, attempts: 2 },
            "重启不重置预算"
        );
    }

    /// 第 10 行：已保存最终结果，但客户端没有收到 → 补发或补读原结果，不重新执行任务。
    #[test]
    fn row_10_the_result_is_final_but_the_client_never_saw_it() {
        let mut observed = input(RunStatus::Completed, LogTail::Final);
        observed.result_delivered = false;
        assert_eq!(decide(&observed), RecoveryAction::RedeliverResult);
    }

    /// 第 11 行：completed / failed / cancelled → 保持终态，不因重启自动开启新一轮。
    #[test]
    fn row_11_a_terminal_run_stays_terminal() {
        for status in [
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            let mut observed = input(status, LogTail::Final);
            observed.result_delivered = true;
            assert_eq!(decide(&observed), RecoveryAction::KeepTerminal { status });
        }
    }

    // ---- 表外但同一段落规定的几条 ----

    #[test]
    fn a_missing_or_altered_output_body_stops_the_task_instead_of_rerunning_it() {
        for check in [OutputCheck::Missing, OutputCheck::HashMismatch] {
            let mut observed = input(
                RunStatus::Running,
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
            RunStatus::Cancelled,
            LogTail::RoundPersisted {
                pending: PendingCall::Planned { call: call() },
            },
        );
        observed.result_delivered = true;
        assert_eq!(
            decide(&observed),
            RecoveryAction::KeepTerminal {
                status: RunStatus::Cancelled
            },
            "用户明确取消的 Run 不自动复活"
        );
    }

    #[test]
    fn a_surviving_previous_executor_blocks_a_second_start() {
        let mut observed = input(
            RunStatus::Interrupted,
            LogTail::RoundPersisted {
                pending: PendingCall::Planned { call: call() },
            },
        );
        observed.previous_executor_stopped = false;
        assert!(matches!(
            decide(&observed),
            RecoveryAction::NeedsAttention { .. }
        ));
    }

    #[test]
    fn an_approval_the_database_lost_asks_the_operator_again() {
        let mut observed = input(RunStatus::WaitingApproval, LogTail::AwaitingModelReply);
        observed.approval = ApprovalObservation::MissingFromDatabase;
        let action = decide(&observed);
        let RecoveryAction::NeedsAttention { reason } = &action else {
            panic!("{action:?}")
        };
        assert!(reason.contains("审计副本"), "{reason}");
    }

    #[test]
    fn an_exhausted_retry_budget_ends_in_needs_attention_not_a_loop() {
        let mut observed = input(RunStatus::WaitingRetry, LogTail::AwaitingModelReply);
        observed.retry = Some(RetryObservation {
            attempts: 5,
            next_retry_at: datetime!(2026-09-15 08:05:00 UTC),
            exhausted: true,
        });
        assert!(matches!(
            decide(&observed),
            RecoveryAction::NeedsAttention { .. }
        ));
    }

    #[test]
    fn a_recovery_input_round_trips_through_json() {
        let observed = input(
            RunStatus::Running,
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
