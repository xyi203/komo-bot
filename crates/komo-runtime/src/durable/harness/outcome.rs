//! 任务状态与 submission 结局的简写。

use komo_kernel::durable::records::{JoinPolicy, TaskOutcome, TaskOutcomeError};
use komo_kernel::durable::task::NextTaskState;
use komo_kernel::durable::{SubmissionSettlement, TaskId};
use serde_json::{Value, json};

pub fn running(checkpoint: Value) -> NextTaskState {
    NextTaskState::Running { checkpoint }
}

/// 等 `on` 里的任务都终结（`allSettled`）。
pub fn waiting(checkpoint: Value, on: Vec<TaskId>) -> NextTaskState {
    NextTaskState::Waiting {
        checkpoint,
        on,
        policy: JoinPolicy::AllSettled,
    }
}

pub fn completed(result: Value) -> NextTaskState {
    NextTaskState::Terminal {
        outcome: TaskOutcome::Completed { result },
    }
}

/// 任务失败；`reason` 进 `detail.reason`（`model_error` / `no_model` / `budget`）。
pub fn failed(message: impl Into<String>, reason: &str, result: Option<Value>) -> NextTaskState {
    NextTaskState::Terminal {
        outcome: TaskOutcome::Failed {
            error: TaskOutcomeError {
                message: message.into(),
                detail: Some(json!({ "reason": reason })),
            },
            result,
        },
    }
}

pub fn aborted(result: Option<Value>) -> NextTaskState {
    NextTaskState::Terminal {
        outcome: TaskOutcome::Aborted {
            reason: None,
            result,
        },
    }
}

pub fn done(answer: komo_kernel::durable::EntryId) -> SubmissionSettlement {
    SubmissionSettlement::Done { answer }
}

/// 输入以 `unanswered` 收场的原因：`no_model`、`model_error`（detail 是 provider 的错误文字）、
/// `aborted`、`faulted`（detail 是错误信息）、`reset`、`budget`，或孤儿任务的阻塞原因。
pub fn unanswered(reason: &str, detail: Option<&str>) -> SubmissionSettlement {
    SubmissionSettlement::Unanswered {
        reason: reason.to_owned(),
        detail: detail.map(|text| json!(text)),
    }
}
