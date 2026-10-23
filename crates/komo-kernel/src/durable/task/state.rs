//! 任务记录上的状态转换（spec §5.1）：谁能变成谁、整条替换时哪些字段不许动。

use std::collections::BTreeMap;

use serde_json::Value;

use crate::durable::ids::TaskId;
use crate::durable::records::{
    AbortReason, JoinPolicy, TaskOutcome, TaskRecord, TaskState, TaskStatus,
};

/// 任务给自己提交的下一个状态：新的 checkpoint、一次等待，或结局。`pending` 永远不由任务
/// 自己返回（只有 reconciliation 与 handover 写它），所以类型里没有。
#[derive(Debug, Clone, PartialEq)]
pub enum NextTaskState {
    Running {
        checkpoint: Value,
    },
    Waiting {
        checkpoint: Value,
        on: Vec<TaskId>,
        policy: JoinPolicy,
    },
    Terminal {
        outcome: TaskOutcome,
    },
}

impl NextTaskState {
    pub fn into_state(self) -> TaskState {
        match self {
            NextTaskState::Running { checkpoint } => TaskState::Running { checkpoint },
            NextTaskState::Waiting {
                checkpoint,
                on,
                policy,
            } => TaskState::Waiting {
                checkpoint,
                on,
                policy,
            },
            NextTaskState::Terminal { outcome } => TaskState::Terminal { outcome },
        }
    }
}

/// 一条 `running` 的任务，交给 phase handler 的那一份拷贝。
#[derive(Debug, Clone, PartialEq)]
pub struct RunningTask {
    pub record: TaskRecord,
    pub checkpoint: Value,
}

impl RunningTask {
    /// 记录必须是 `running`；否则返回 `None`。
    pub fn from_record(record: &TaskRecord) -> Option<Self> {
        match &record.state {
            TaskState::Running { checkpoint } => Some(Self {
                checkpoint: checkpoint.clone(),
                record: record.clone(),
            }),
            _ => None,
        }
    }

    pub fn id(&self) -> TaskId {
        self.record.id
    }

    pub fn input(&self) -> &Value {
        &self.record.input
    }

    pub fn phase(&self) -> &str {
        checkpoint_phase(&self.checkpoint).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("checkpoint must be an object with a string `phase`")]
    BadCheckpoint,
    #[error("task {task} cannot change from {from:?} to {to:?}")]
    IllegalTransition {
        task: TaskId,
        from: TaskStatus,
        to: TaskStatus,
    },
    #[error("task {0} cannot change its {1}")]
    ImmutableField(TaskId, &'static str),
}

pub fn checkpoint_phase(checkpoint: &Value) -> Option<&str> {
    checkpoint.get("phase").and_then(Value::as_str)
}

/// checkpoint 是带字符串 `phase` 的对象（phase 分派的前提）。
pub fn check_checkpoint(checkpoint: &Value) -> Result<(), StateError> {
    if checkpoint.is_object() && checkpoint_phase(checkpoint).is_some() {
        Ok(())
    } else {
        Err(StateError::BadCheckpoint)
    }
}

/// 任务经自己的 runtime 提交的状态：running / waiting 的 checkpoint 要合法。
pub fn check_next_state(next: &NextTaskState) -> Result<(), StateError> {
    match next {
        NextTaskState::Running { checkpoint } | NextTaskState::Waiting { checkpoint, .. } => {
            check_checkpoint(checkpoint)
        }
        NextTaskState::Terminal { .. } => Ok(()),
    }
}

/// 状态的合法边（调度器自己写的和任务提交的合在一起）：
///
/// - `pending → running`（reservation）、`pending → completing | terminal`（孤儿 / 故障）
/// - `running → pending`（reconciliation / handover）、`→ waiting | completing | terminal`
/// - `waiting → running`（reservation）、`→ completing | terminal`（孤儿，经 abort）
/// - `completing → terminal`（收尾）
/// - 同一状态自己到自己（abort 标记、memo、checkpoint 替换）
/// - `terminal` 之后不再变
pub fn legal_transition(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    if from == Terminal {
        return false;
    }
    if from == to {
        return true;
    }
    matches!(
        (from, to),
        (Pending, Running | Completing | Terminal)
            | (Running, Pending | Waiting | Completing | Terminal)
            | (Waiting, Running | Completing | Terminal)
            | (Completing, Terminal)
    )
}

/// 整条替换要守的规矩：状态边合法；归属字段创建后不可变；`completing` 的结局不能换；
/// 已是 terminal 不能再改。
pub fn check_replacement(old: &TaskRecord, new: &TaskRecord) -> Result<(), StateError> {
    if old.id != new.id {
        return Err(StateError::ImmutableField(old.id, "id"));
    }
    if old.conversation != new.conversation {
        return Err(StateError::ImmutableField(old.id, "conversation"));
    }
    if old.owner != new.owner {
        return Err(StateError::ImmutableField(old.id, "owner"));
    }
    if old.background != new.background {
        return Err(StateError::ImmutableField(old.id, "background"));
    }
    if old.abandon_on_restart != new.abandon_on_restart {
        return Err(StateError::ImmutableField(old.id, "abandon_on_restart"));
    }
    let (from, to) = (old.state.status(), new.state.status());
    if !legal_transition(from, to) {
        return Err(StateError::IllegalTransition {
            task: old.id,
            from,
            to,
        });
    }
    if let (TaskState::Completing { outcome: a }, TaskState::Terminal { outcome: b }) =
        (&old.state, &new.state)
        && a != b
    {
        return Err(StateError::ImmutableField(old.id, "held outcome"));
    }
    Ok(())
}

/// 还没终结的任务：pending / running / waiting / completing。
pub fn is_live(record: &TaskRecord) -> bool {
    record.state.status() != TaskStatus::Terminal
}

/// 记录握着或终结于 `completed` 之外的结局。
pub fn failed_outcome(record: &TaskRecord) -> bool {
    match &record.state {
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => {
            !matches!(outcome, TaskOutcome::Completed { .. })
        }
        _ => false,
    }
}

/// 活着的所有者的"取消意图"：abort 标记，或握着的非 `completed` 结局（spec §5.4）。
pub fn cancellation_intent(record: &TaskRecord) -> bool {
    is_live(record) && (record.abort_requested || failed_outcome(record))
}

/// 换状态；结局已定（completing / terminal）时 memos 消失。
pub fn with_state(record: &TaskRecord, state: TaskState) -> TaskRecord {
    let mut next = record.clone();
    if matches!(
        state,
        TaskState::Completing { .. } | TaskState::Terminal { .. }
    ) {
        next.memos = None;
    }
    next.state = state;
    next
}

/// 打 abort 标记：`restart` 是重启放弃，不带原因是请求，后者会替换前者。
pub fn with_abort_mark(record: &TaskRecord, reason: Option<AbortReason>) -> TaskRecord {
    let mut next = record.clone();
    next.abort_requested = true;
    next.abort_reason = reason;
    next
}

/// 只认 memo 自己的键。
pub fn memo_of<'a>(record: &'a TaskRecord, name: &str) -> Option<&'a Value> {
    record.memos.as_ref().and_then(|memos| memos.get(name))
}

/// 先写者赢地放一个 memo，返回（新记录，赢家）。
pub fn put_memo(record: &TaskRecord, name: &str, candidate: Value) -> (TaskRecord, Value) {
    if let Some(winner) = memo_of(record, name) {
        return (record.clone(), winner.clone());
    }
    let mut next = record.clone();
    next.memos
        .get_or_insert_with(BTreeMap::new)
        .insert(name.to_owned(), candidate.clone());
    (next, candidate)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::durable::ids::ConversationId;
    use crate::durable::records::TaskOutcomeError;

    fn record(state: TaskState) -> TaskRecord {
        let mut record = TaskRecord::pending(
            TaskId::new(5),
            ConversationId::new(1),
            "k",
            json!({}),
            json!({"phase": "a"}),
        );
        record.state = state;
        record
    }

    fn completed() -> TaskOutcome {
        TaskOutcome::Completed { result: json!(1) }
    }

    #[test]
    fn terminal_never_changes_and_completing_only_finishes() {
        use TaskStatus::*;
        for to in [Pending, Running, Waiting, Completing, Terminal] {
            assert!(!legal_transition(Terminal, to));
        }
        assert!(legal_transition(Completing, Terminal));
        assert!(!legal_transition(Completing, Running));
        assert!(!legal_transition(Completing, Waiting));
        assert!(legal_transition(Pending, Running));
        assert!(!legal_transition(Pending, Waiting));
        assert!(legal_transition(Running, Pending));
        assert!(legal_transition(Waiting, Running));
    }

    #[test]
    fn a_held_outcome_cannot_be_swapped_at_the_final_commit() {
        let held = record(TaskState::Completing {
            outcome: completed(),
        });
        let same = with_state(
            &held,
            TaskState::Terminal {
                outcome: completed(),
            },
        );
        assert!(check_replacement(&held, &same).is_ok());
        let other = with_state(
            &held,
            TaskState::Terminal {
                outcome: TaskOutcome::Orphaned { reason: "x".into() },
            },
        );
        assert_eq!(
            check_replacement(&held, &other),
            Err(StateError::ImmutableField(held.id, "held outcome"))
        );
    }

    #[test]
    fn ownership_fields_are_immutable() {
        let old = record(TaskState::Running {
            checkpoint: json!({"phase": "a"}),
        });
        let mut moved = old.clone();
        moved.conversation = ConversationId::new(2);
        assert!(check_replacement(&old, &moved).is_err());
        let mut reowned = old.clone();
        reowned.owner = Some(TaskId::new(1));
        assert!(check_replacement(&old, &reowned).is_err());
    }

    #[test]
    fn outcome_states_drop_memos_and_checkpoints_need_a_phase() {
        let old = record(TaskState::Running {
            checkpoint: json!({"phase": "a"}),
        });
        let (old, winner) = put_memo(&old, "k", json!(1));
        assert_eq!(winner, json!(1));
        let (same, winner) = put_memo(&old, "k", json!(2));
        assert_eq!(winner, json!(1), "first writer wins");
        assert_eq!(same, old);
        let done = with_state(
            &old,
            TaskState::Terminal {
                outcome: completed(),
            },
        );
        assert!(done.memos.is_none());
        assert!(check_checkpoint(&json!({"phase": "a"})).is_ok());
        assert!(check_checkpoint(&json!({"phase": 1})).is_err());
        assert!(check_checkpoint(&json!("a")).is_err());
    }

    #[test]
    fn cancellation_intent_is_a_mark_or_a_held_failure_but_never_completed() {
        let failed = TaskOutcome::Failed {
            error: TaskOutcomeError {
                message: "m".into(),
                detail: None,
            },
            result: None,
        };
        let held_failed = record(TaskState::Completing { outcome: failed });
        let held_ok = record(TaskState::Completing {
            outcome: completed(),
        });
        let marked = with_abort_mark(
            &record(TaskState::Pending {
                checkpoint: json!({"phase": "a"}),
            }),
            None,
        );
        assert!(cancellation_intent(&held_failed));
        assert!(!cancellation_intent(&held_ok));
        assert!(cancellation_intent(&marked));
        let ended = with_state(
            &held_failed,
            TaskState::Terminal {
                outcome: TaskOutcome::Aborted {
                    reason: None,
                    result: None,
                },
            },
        );
        assert!(
            !cancellation_intent(&ended),
            "a terminal owner never cascades"
        );
    }
}
