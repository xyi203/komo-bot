//! 一个阶段返回之后的 step 判定（spec §5.1 规则 1–6）。
//!
//! 调度器在线上同步地读已提交的任务，把"刚返回的阶段"的结果交给 [`decide_step`]，按返回
//! 的决定写 fault / handover 或继续。本函数不做 I/O、不看时钟。

use serde_json::Value;

use crate::durable::records::{TaskRecord, TaskState};

use super::state::checkpoint_phase;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationMode {
    Run,
    Abort,
}

/// 刚返回的阶段：它开始时的 checkpoint，以及它是否出错（panic 也算）。
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseReturn {
    pub checkpoint: Value,
    pub failure: Option<String>,
}

/// 注册表现在给这个 kind 的定义，相对 invocation 起跑时那一个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replacement {
    /// 同一个定义（或没有变）。
    Unchanged,
    /// 换了定义，且能接手这条任务（同版本，或更新且有 migrate）。
    Takeover,
    /// 换了定义（或没了），但不能接手：继续用旧的，报告一次。
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct StepInput<'a> {
    pub mode: InvocationMode,
    /// 线上读到的已提交记录；任务已不在或不是 `running` 时规则 1 生效。
    pub task: Option<&'a TaskRecord>,
    /// 规则 2：Session 正在关闭。
    pub closing: bool,
    /// 所有者握着取消意图但标记还没级联下来（规则 3 的补充）。
    pub owner_cancelled: bool,
    /// 刚返回的阶段；首个阶段之前为 `None`（只有规则 1–3）。
    pub previous: Option<&'a PhaseReturn>,
    pub replacement: Replacement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepDecision {
    /// 规则 1 / 2 / 3：invocation 到此为止，不写任何东西。
    Stop,
    /// 同上，并且要补一次级联（所有者的取消意图还没落成标记）。
    StopAndCascade,
    /// 规则 4 / 6（及 abort 阶段没给结局）：写 terminal `faulted`。
    Fault { message: String },
    /// 规则 5，换定义：把任务写回 `pending`（保留 checkpoint、memos、标记），invocation 结束。
    Handover,
    /// 规则 5 / 首阶段：在同一个 invocation 里跑下一个阶段。`report_unavailable` 表示换了
    /// 定义却接不了手，调度器按 invocation 报告一次。
    Continue { report_unavailable: bool },
}

/// 按规则 1–6 的次序判定。
pub fn decide_step(input: &StepInput<'_>) -> StepDecision {
    // 1. terminal / completing / waiting（或不存在）：停。
    let Some(task) = input.task else {
        return StepDecision::Stop;
    };
    let TaskState::Running {
        checkpoint: current,
    } = &task.state
    else {
        return StepDecision::Stop;
    };
    // 2. Session 关闭：停，checkpoint 与标记留给重开。
    if input.closing {
        return StepDecision::Stop;
    }
    match input.mode {
        InvocationMode::Abort => {
            // abort 阶段只适用规则 1、2、4：handler 返回后任务还是 running，就是没给结局。
            let message = match input.previous.and_then(|p| p.failure.as_deref()) {
                Some(error) => error.to_owned(),
                None => format!(
                    "Abort handler of task {} returned without a terminal outcome",
                    task.id
                ),
            };
            StepDecision::Fault { message }
        }
        InvocationMode::Run => {
            // 3. 有 abort 标记：结束并 join run invocation；之后由 abort invocation 接手。
            if task.abort_requested {
                return StepDecision::Stop;
            }
            // 所有者的取消意图也让它在下个阶段前结束，保留给级联去标记。
            if !task.background && input.owner_cancelled {
                return StepDecision::StopAndCascade;
            }
            let Some(previous) = input.previous else {
                return StepDecision::Continue {
                    report_unavailable: false,
                };
            };
            // 4. 未捕获的错误。
            if let Some(error) = &previous.failure {
                return StepDecision::Fault {
                    message: error.clone(),
                };
            }
            // 6. checkpoint 没变：没有持久进展。
            if *current == previous.checkpoint {
                return StepDecision::Fault {
                    message: format!(
                        "Task {} phase {} returned without durable progress",
                        task.kind,
                        checkpoint_phase(&previous.checkpoint).unwrap_or("?")
                    ),
                };
            }
            // 5. 有进展：换定义就 handover，否则继续。
            match input.replacement {
                Replacement::Takeover => StepDecision::Handover,
                Replacement::Unavailable => StepDecision::Continue {
                    report_unavailable: true,
                },
                Replacement::Unchanged => StepDecision::Continue {
                    report_unavailable: false,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::durable::ids::{ConversationId, TaskId};
    use crate::durable::records::TaskOutcome;

    fn running(checkpoint: Value) -> TaskRecord {
        let mut task = TaskRecord::pending(
            TaskId::new(2),
            ConversationId::new(1),
            "k",
            json!({}),
            checkpoint.clone(),
        );
        task.state = TaskState::Running { checkpoint };
        task
    }

    fn input<'a>(task: &'a TaskRecord, previous: Option<&'a PhaseReturn>) -> StepInput<'a> {
        StepInput {
            mode: InvocationMode::Run,
            task: Some(task),
            closing: false,
            owner_cancelled: false,
            previous,
            replacement: Replacement::Unchanged,
        }
    }

    fn returned(checkpoint: Value, failure: Option<&str>) -> PhaseReturn {
        PhaseReturn {
            checkpoint,
            failure: failure.map(str::to_owned),
        }
    }

    const CONTINUE: StepDecision = StepDecision::Continue {
        report_unavailable: false,
    };

    #[test]
    fn rule_1_stops_when_the_task_is_not_running() {
        let mut task = running(json!({"phase": "a"}));
        task.state = TaskState::Terminal {
            outcome: TaskOutcome::Completed { result: json!(1) },
        };
        let previous = returned(json!({"phase": "a"}), Some("late error"));
        assert_eq!(
            decide_step(&input(&task, Some(&previous))),
            StepDecision::Stop,
            "a committed outcome beats an error thrown afterwards"
        );
        let mut waiting = running(json!({"phase": "a"}));
        waiting.state = TaskState::Waiting {
            checkpoint: json!({"phase": "b"}),
            on: vec![],
            policy: crate::durable::records::JoinPolicy::AllSettled,
        };
        assert_eq!(decide_step(&input(&waiting, None)), StepDecision::Stop);
        assert_eq!(
            decide_step(&StepInput {
                task: None,
                ..input(&waiting, None)
            }),
            StepDecision::Stop
        );
    }

    #[test]
    fn rule_2_stops_when_closing_even_after_a_failure() {
        let task = running(json!({"phase": "a"}));
        let previous = returned(json!({"phase": "a"}), Some("boom"));
        let step = StepInput {
            closing: true,
            ..input(&task, Some(&previous))
        };
        assert_eq!(decide_step(&step), StepDecision::Stop);
    }

    #[test]
    fn rule_3_an_abort_mark_ends_the_run_before_faults_are_considered() {
        let mut task = running(json!({"phase": "a"}));
        task.abort_requested = true;
        let previous = returned(json!({"phase": "a"}), Some("boom"));
        assert_eq!(
            decide_step(&input(&task, Some(&previous))),
            StepDecision::Stop
        );
        assert_eq!(decide_step(&input(&task, None)), StepDecision::Stop);
    }

    #[test]
    fn rule_3_an_owner_with_cancellation_intent_stops_and_asks_for_the_cascade() {
        let task = running(json!({"phase": "a"}));
        let step = StepInput {
            owner_cancelled: true,
            ..input(&task, None)
        };
        assert_eq!(decide_step(&step), StepDecision::StopAndCascade);
        let mut background = task.clone();
        background.background = true;
        let step = StepInput {
            owner_cancelled: true,
            ..input(&background, None)
        };
        assert_eq!(decide_step(&step), CONTINUE, "background is a boundary");
    }

    #[test]
    fn the_first_phase_only_applies_rules_1_to_3() {
        let task = running(json!({"phase": "a"}));
        assert_eq!(decide_step(&input(&task, None)), CONTINUE);
    }

    #[test]
    fn rule_4_a_failed_phase_faults_with_its_error() {
        let task = running(json!({"phase": "b"}));
        let previous = returned(json!({"phase": "a"}), Some("boom"));
        assert_eq!(
            decide_step(&input(&task, Some(&previous))),
            StepDecision::Fault {
                message: "boom".into()
            }
        );
    }

    #[test]
    fn rule_5_progress_continues_or_hands_over() {
        let task = running(json!({"phase": "b"}));
        let previous = returned(json!({"phase": "a"}), None);
        assert_eq!(decide_step(&input(&task, Some(&previous))), CONTINUE);
        let handover = StepInput {
            replacement: Replacement::Takeover,
            ..input(&task, Some(&previous))
        };
        assert_eq!(decide_step(&handover), StepDecision::Handover);
        let stuck = StepInput {
            replacement: Replacement::Unavailable,
            ..input(&task, Some(&previous))
        };
        assert_eq!(
            decide_step(&stuck),
            StepDecision::Continue {
                report_unavailable: true
            }
        );
    }

    #[test]
    fn rule_5_progress_within_the_same_phase_counts() {
        let task = running(json!({"phase": "a", "n": 2}));
        let previous = returned(json!({"phase": "a", "n": 1}), None);
        assert_eq!(decide_step(&input(&task, Some(&previous))), CONTINUE);
    }

    #[test]
    fn rule_6_an_unchanged_checkpoint_faults_whatever_the_key_order() {
        let task = running(json!({"phase": "a", "x": [1, {"b": 2, "c": 3}]}));
        let previous = returned(json!({"x": [1, {"c": 3, "b": 2}], "phase": "a"}), None);
        let StepDecision::Fault { message } = decide_step(&input(&task, Some(&previous))) else {
            panic!("expected a fault");
        };
        assert!(message.contains("without durable progress"), "{message}");
    }

    #[test]
    fn an_abort_handler_that_settles_nothing_faults_unless_it_failed_with_a_reason() {
        let task = running(json!({"phase": "a"}));
        let silent = StepInput {
            mode: InvocationMode::Abort,
            ..input(&task, None)
        };
        let StepDecision::Fault { message } = decide_step(&silent) else {
            panic!("expected a fault");
        };
        assert!(message.contains("without a terminal outcome"));
        let previous = returned(json!({"phase": "a"}), Some("handler threw"));
        let failed = StepInput {
            mode: InvocationMode::Abort,
            ..input(&task, Some(&previous))
        };
        assert_eq!(
            decide_step(&failed),
            StepDecision::Fault {
                message: "handler threw".into()
            }
        );
        let closing = StepInput {
            closing: true,
            ..failed.clone()
        };
        assert_eq!(decide_step(&closing), StepDecision::Stop);
    }
}
