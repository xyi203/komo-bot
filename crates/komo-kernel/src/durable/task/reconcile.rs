//! reconciliation 的决定（spec §5.1、§5.4、§5.5）：开库时 `running` 怎么回 `pending`、
//! `waiting` 何时可恢复、failFast 与取消意图级联打哪些 abort 标记、`completing` 何时转
//! terminal。输入是已提交的记录与所有权树，输出是要写什么；写由调度器做。

use std::collections::{BTreeMap, HashSet};

use crate::durable::ids::TaskId;
use crate::durable::records::{AbortReason, JoinPolicy, TaskRecord, TaskState};

use super::graph::{Node, Overlay, TaskGraph};
use super::state::{cancellation_intent, failed_outcome, with_state};

/// 开库：幸存的 `running` 一律回 `pending`，checkpoint 与 abort 标记原样保留；其余状态
/// 不动（`waiting` 与 `completing` 另行重新评估）。返回要写的新记录。
pub fn reconcile_on_open(record: &TaskRecord) -> Option<TaskRecord> {
    match &record.state {
        TaskState::Running { checkpoint } => Some(with_state(
            record,
            TaskState::Pending {
                checkpoint: checkpoint.clone(),
            },
        )),
        _ => None,
    }
}

/// 一条任务下次 invocation 之前还在等的活任务：带 abort 标记时是它活的普通所有的工作
/// （abort 自底向上），否则是 `waiting` 的 `on` 里还活着的那些。`first` 只要第一个。
pub fn waiting_on(graph: &TaskGraph, record: &TaskRecord, first: bool) -> Vec<TaskId> {
    if record.abort_requested {
        return graph.owned_live(record.id, None, first, &HashSet::new());
    }
    let TaskState::Waiting { on, .. } = &record.state else {
        return Vec::new();
    };
    let mut live = on.iter().copied().filter(|id| graph.is_live(*id));
    if first {
        live.next().into_iter().collect()
    } else {
        live.collect()
    }
}

/// failFast：`on` 里已有任务握着或终结于非 `completed` 结局时，其余还活着（且自己没有失败
/// 结局）的成员要打标记。`failed` 是调用方已判明失败的任务集合（活的与已终结的都在内）。
/// 等待者自己不被标记。
pub fn fail_fast_targets(
    graph: &TaskGraph,
    waiter: &TaskRecord,
    failed: &HashSet<TaskId>,
) -> Vec<TaskId> {
    let TaskState::Waiting {
        on,
        policy: JoinPolicy::FailFast,
        ..
    } = &waiter.state
    else {
        return Vec::new();
    };
    if !on.iter().any(|id| failed.contains(id)) {
        return Vec::new();
    }
    on.iter()
        .copied()
        .filter(|id| {
            graph
                .live_record(*id)
                .is_some_and(|record| !failed_outcome(record))
        })
        .collect()
}

/// 要打的一个 abort 标记；`reason` 为 `None` 是请求，`Some(Restart)` 是重启放弃。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbortMark {
    pub id: TaskId,
    pub reason: Option<AbortReason>,
}

/// 这一轮 reconcile 要打的全部标记（spec §5.4、§5.5）：
///
/// - 每个取消意图的活所有者之下的活的非后台任务（沿最近的意图所有者往下，不进入另一个意图
///   所有者，那一份由它自己的遍历覆盖）；只有因重启被放弃的所有者（且不是失败结局）才把
///   `restart` 原因传下去，别的意图按请求打，并会替换下面已有的 `restart` 标记；
/// - failFast 等待者里，已失败成员之外还活着的成员（总是请求）。
///
/// 同一轮里请求胜过 `restart`；已有的请求标记不重复打。
pub fn abort_marks(graph: &TaskGraph, failed: &HashSet<TaskId>) -> Vec<AbortMark> {
    let mut marks: BTreeMap<TaskId, Option<AbortReason>> = BTreeMap::new();
    let mut mark = |record: &TaskRecord, reason: Option<AbortReason>| {
        if let Some(staged) = marks.get(&record.id)
            && staged.is_none()
        {
            return;
        }
        if record.abort_requested && (record.abort_reason.is_none() || reason.is_some()) {
            return;
        }
        marks.insert(record.id, reason);
    };
    let owners: Vec<&TaskRecord> = graph
        .live()
        .filter(|record| cancellation_intent(record))
        .collect();
    let intent: HashSet<TaskId> = owners.iter().map(|record| record.id).collect();
    for owner in &owners {
        let restart = owner.abort_reason == Some(AbortReason::Restart) && !failed_outcome(owner);
        let reason = restart.then_some(AbortReason::Restart);
        let enter = |below: TaskId| !intent.contains(&below);
        for walked in graph.walk_down(Node::Task(owner.id), None, false, &enter) {
            if let Some(live) = &walked.live
                && !walked.node.background
            {
                mark(live, reason);
            }
        }
    }
    for waiter in graph.live() {
        for target in fail_fast_targets(graph, waiter, failed) {
            if let Some(record) = graph.live_record(target) {
                mark(record, None);
            }
        }
    }
    marks
        .into_iter()
        .map(|(id, reason)| AbortMark { id, reason })
        .collect()
}

/// `completing` 的任务在它活的普通所有的工作都没了之后可以写 terminal（在候选 `overlay` 与
/// 已 `done` 的收尾之上判断）。
pub fn finalizable(
    graph: &TaskGraph,
    record: &TaskRecord,
    overlay: Option<&Overlay>,
    done: &HashSet<TaskId>,
) -> bool {
    matches!(record.state, TaskState::Completing { .. })
        && !done.contains(&record.id)
        && !graph.has_owned_live(record.id, overlay, done)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::durable::ids::ConversationId;
    use crate::durable::records::{TaskOutcome, TaskOutcomeError};

    fn task(id: u64, owner: Option<u64>, state: TaskState) -> TaskRecord {
        let mut record = TaskRecord::pending(
            TaskId::new(id),
            ConversationId::new(1),
            "k",
            json!({}),
            json!({"phase": "a"}),
        );
        record.owner = owner.map(TaskId::new);
        record.state = state;
        record
    }

    fn pending() -> TaskState {
        TaskState::Pending {
            checkpoint: json!({"phase": "a"}),
        }
    }

    fn running() -> TaskState {
        TaskState::Running {
            checkpoint: json!({"phase": "a"}),
        }
    }

    fn graph(records: &[TaskRecord]) -> TaskGraph {
        let mut graph = TaskGraph::new();
        graph.set_edge(ConversationId::new(1), None);
        for record in records {
            graph.track(record);
        }
        graph
    }

    fn failed() -> TaskOutcome {
        TaskOutcome::Failed {
            error: TaskOutcomeError {
                message: "x".into(),
                detail: None,
            },
            result: None,
        }
    }

    #[test]
    fn open_turns_running_back_to_pending_and_keeps_marks_and_checkpoint() {
        let mut record = task(1, None, running());
        record.abort_requested = true;
        record.memos = Some(BTreeMap::from([("m".to_owned(), json!(1))]));
        let back = reconcile_on_open(&record).unwrap();
        assert_eq!(back.state, pending());
        assert!(back.abort_requested);
        assert_eq!(back.memos, record.memos, "memos survive a restart");
        assert!(reconcile_on_open(&task(2, None, pending())).is_none());
        let waiting = task(
            3,
            None,
            TaskState::Waiting {
                checkpoint: json!({"phase": "w"}),
                on: vec![],
                policy: JoinPolicy::AllSettled,
            },
        );
        assert!(reconcile_on_open(&waiting).is_none());
    }

    #[test]
    fn a_waiter_resumes_only_when_every_member_of_on_is_terminal() {
        let waiter = task(
            1,
            None,
            TaskState::Waiting {
                checkpoint: json!({"phase": "w"}),
                on: vec![TaskId::new(2), TaskId::new(3)],
                policy: JoinPolicy::AllSettled,
            },
        );
        let both = graph(&[
            waiter.clone(),
            task(2, None, pending()),
            task(3, None, running()),
        ]);
        assert_eq!(
            waiting_on(&both, &waiter, false),
            vec![TaskId::new(2), TaskId::new(3)]
        );
        let mut one = graph(&[
            waiter.clone(),
            task(2, None, pending()),
            task(3, None, running()),
        ]);
        one.track(&task(
            2,
            None,
            TaskState::Terminal {
                outcome: TaskOutcome::Completed { result: json!(1) },
            },
        ));
        assert_eq!(waiting_on(&one, &waiter, true), vec![TaskId::new(3)]);
        one.track(&task(3, None, TaskState::Terminal { outcome: failed() }));
        assert!(waiting_on(&one, &waiter, false).is_empty());
        // 一个没有等待的任务不在等任何东西。
        assert!(waiting_on(&one, &task(9, None, pending()), false).is_empty());
    }

    #[test]
    fn a_marked_waiter_waits_for_its_owned_work_instead_of_its_on() {
        let mut waiter = task(
            1,
            None,
            TaskState::Waiting {
                checkpoint: json!({"phase": "w"}),
                on: vec![TaskId::new(5)],
                policy: JoinPolicy::AllSettled,
            },
        );
        waiter.abort_requested = true;
        let g = graph(&[
            waiter.clone(),
            task(5, None, running()),
            task(2, Some(1), running()),
        ]);
        assert_eq!(waiting_on(&g, &waiter, false), vec![TaskId::new(2)]);
    }

    #[test]
    fn fail_fast_marks_the_other_live_members_and_not_the_failed_one_or_the_waiter() {
        let waiter = task(
            1,
            None,
            TaskState::Waiting {
                checkpoint: json!({"phase": "w"}),
                on: vec![TaskId::new(2), TaskId::new(3), TaskId::new(4)],
                policy: JoinPolicy::FailFast,
            },
        );
        let mut g = graph(&[
            waiter.clone(),
            task(2, Some(1), running()),
            task(3, Some(1), running()),
            task(4, Some(1), running()),
        ]);
        let none = HashSet::new();
        assert!(abort_marks(&g, &none).is_empty());
        // 2 已经终结为失败，3 握着失败结局（completing），4 活着。
        g.track(&task(2, Some(1), TaskState::Terminal { outcome: failed() }));
        g.track(&task(
            3,
            Some(1),
            TaskState::Completing { outcome: failed() },
        ));
        let failed_ids = HashSet::from([TaskId::new(2), TaskId::new(3)]);
        let marks = abort_marks(&g, &failed_ids);
        let marked: Vec<TaskId> = marks.iter().map(|m| m.id).collect();
        assert_eq!(marked, vec![TaskId::new(4)]);
        assert!(marks.iter().all(|m| m.reason.is_none()));
    }

    #[test]
    fn a_cancelled_owner_marks_its_foreground_work_and_passes_restart_down() {
        let mut owner = task(1, None, running());
        owner.abort_requested = true;
        owner.abort_reason = Some(AbortReason::Restart);
        let mut background = task(4, Some(1), running());
        background.background = true;
        let g = graph(&[
            owner,
            task(2, Some(1), running()),
            task(3, Some(2), pending()),
            background,
        ]);
        let marks = abort_marks(&g, &HashSet::new());
        assert_eq!(
            marks,
            vec![
                AbortMark {
                    id: TaskId::new(2),
                    reason: Some(AbortReason::Restart)
                },
                AbortMark {
                    id: TaskId::new(3),
                    reason: Some(AbortReason::Restart)
                },
            ],
            "background is a boundary"
        );
    }

    #[test]
    fn a_request_replaces_a_restart_mark_below_but_an_existing_request_is_not_repeated() {
        let mut owner = task(1, None, running());
        owner.abort_requested = true;
        let mut restarted = task(2, Some(1), running());
        restarted.abort_requested = true;
        restarted.abort_reason = Some(AbortReason::Restart);
        let mut requested = task(3, Some(1), running());
        requested.abort_requested = true;
        let g = graph(&[owner, restarted, requested]);
        let marks = abort_marks(&g, &HashSet::new());
        assert_eq!(
            marks,
            vec![AbortMark {
                id: TaskId::new(2),
                reason: None
            }]
        );
    }

    #[test]
    fn a_held_failed_owner_passes_no_restart_reason_and_a_held_completed_one_cascades_nothing() {
        let mut failed_owner = task(1, None, TaskState::Completing { outcome: failed() });
        failed_owner.abort_reason = Some(AbortReason::Restart);
        failed_owner.abort_requested = true;
        let g = graph(&[failed_owner, task(2, Some(1), running())]);
        let marks = abort_marks(&g, &HashSet::new());
        assert_eq!(marks[0].reason, None);
        let ok_owner = task(
            1,
            None,
            TaskState::Completing {
                outcome: TaskOutcome::Completed { result: json!(1) },
            },
        );
        let g = graph(&[ok_owner, task(2, Some(1), running())]);
        assert!(abort_marks(&g, &HashSet::new()).is_empty());
    }

    #[test]
    fn completing_is_final_once_no_ordinary_owned_work_is_live() {
        let held = task(
            1,
            None,
            TaskState::Completing {
                outcome: TaskOutcome::Completed { result: json!(1) },
            },
        );
        let mut g = graph(&[held.clone(), task(2, Some(1), running())]);
        let none = HashSet::new();
        assert!(!finalizable(&g, &held, None, &none));
        g.track(&task(
            2,
            Some(1),
            TaskState::Terminal {
                outcome: TaskOutcome::Completed { result: json!(0) },
            },
        ));
        assert!(finalizable(&g, &held, None, &none));
        assert!(!finalizable(&g, &task(7, None, running()), None, &none));
    }
}
