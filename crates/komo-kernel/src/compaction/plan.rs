//! 工作计划：`update_plan` 的参数、校验与前后两版之间的转移。
//!
//! 计划是模型自己报的进度。它只有一个用处：**"完成了一个先前登记过的步骤"是一个安全
//! 点**——上一步的细节可以收成摘要，而不打断正在做的事。所以这里关心的不是计划写得好
//! 不好，而是哪一次更新算"完成了一步"。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// 一份计划最多几步。
pub const MAX_PLAN_STEPS: usize = 16;
/// 步骤 id 的字节上限。
pub const MAX_STEP_ID_BYTES: usize = 64;
/// 步骤目标的字节上限（约 80 个汉字）。
pub const MAX_STEP_GOAL_BYTES: usize = 240;
/// `progress` 里每张清单最多几条。
pub const MAX_PROGRESS_ITEMS: usize = 8;
/// `progress` 里每一条的字节上限。
pub const MAX_PROGRESS_ITEM_BYTES: usize = 240;
/// 整份参数序列化后的字节上限。
///
/// 上面几条单项上限各自都满了会超过 4 KiB，真正兜底的是这一条：参数要连同计划的其余
/// 字段（id、来源、cwd……约几百字节）一起放进 `tool.planned` 的内联计划
/// （[`INLINE_ARGUMENT_LIMIT_BYTES`]）。外置了，fold 就读不到它——在线状态是从日志
/// 折出来的，读不到等于这次更新没发生。
///
/// [`INLINE_ARGUMENT_LIMIT_BYTES`]: crate::types::refs::INLINE_ARGUMENT_LIMIT_BYTES
pub const MAX_PLAN_UPDATE_BYTES: usize = 3072;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub goal: String,
    pub status: PlanStatus,
}

/// 完成一步时附带的证据。它进摘要，不进判断。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanProgress {
    pub files_changed: Vec<String>,
    pub verification: Vec<String>,
    pub decisions: Vec<String>,
}

/// `update_plan` 的参数：**每次都是整份计划**，不是增量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanUpdate {
    pub steps: Vec<PlanStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<PlanProgress>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("计划至少要有一步")]
    Empty,
    #[error("计划有 {count} 步，最多 {max} 步")]
    TooManySteps { count: usize, max: usize },
    #[error("步骤 id {0:?} 重复了：同一份计划里 id 不能重复")]
    DuplicateId(String),
    #[error("{field} 不能是空的")]
    EmptyField { field: &'static str },
    #[error("{field} 有 {bytes} 字节，最多 {max} 字节")]
    FieldTooLong {
        field: &'static str,
        bytes: usize,
        max: usize,
    },
    #[error("progress.{field} 有 {count} 条，最多 {max} 条")]
    TooManyItems {
        field: &'static str,
        count: usize,
        max: usize,
    },
    #[error("整份计划有 {bytes} 字节，最多 {max} 字节：把步骤目标写短一点")]
    TooLarge { bytes: usize, max: usize },
}

impl PlanUpdate {
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.steps.is_empty() {
            return Err(PlanError::Empty);
        }
        if self.steps.len() > MAX_PLAN_STEPS {
            return Err(PlanError::TooManySteps {
                count: self.steps.len(),
                max: MAX_PLAN_STEPS,
            });
        }
        let mut ids = BTreeSet::new();
        for step in &self.steps {
            bounded("id", &step.id, MAX_STEP_ID_BYTES)?;
            bounded("goal", &step.goal, MAX_STEP_GOAL_BYTES)?;
            if !ids.insert(step.id.as_str()) {
                return Err(PlanError::DuplicateId(step.id.clone()));
            }
        }
        if let Some(progress) = &self.progress {
            for (field, items) in [
                ("files_changed", &progress.files_changed),
                ("verification", &progress.verification),
                ("decisions", &progress.decisions),
            ] {
                if items.len() > MAX_PROGRESS_ITEMS {
                    return Err(PlanError::TooManyItems {
                        field,
                        count: items.len(),
                        max: MAX_PROGRESS_ITEMS,
                    });
                }
                for item in items {
                    if item.len() > MAX_PROGRESS_ITEM_BYTES {
                        return Err(PlanError::FieldTooLong {
                            field,
                            bytes: item.len(),
                            max: MAX_PROGRESS_ITEM_BYTES,
                        });
                    }
                }
            }
        }
        let bytes = serde_json::to_vec(self).map_or(usize::MAX, |encoded| encoded.len());
        if bytes > MAX_PLAN_UPDATE_BYTES {
            return Err(PlanError::TooLarge {
                bytes,
                max: MAX_PLAN_UPDATE_BYTES,
            });
        }
        Ok(())
    }
}

fn bounded(field: &'static str, value: &str, max: usize) -> Result<(), PlanError> {
    if value.is_empty() {
        return Err(PlanError::EmptyField { field });
    }
    if value.len() > max {
        return Err(PlanError::FieldTooLong {
            field,
            bytes: value.len(),
            max,
        });
    }
    Ok(())
}

/// 前后两版计划之间发生了什么。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanTransition {
    /// 这次**从未完成变成完成**的步骤，按新计划里的顺序。
    pub completed: Vec<PlanStep>,
    /// 给模型的提醒，随工具结果一起回去。
    pub advice: Vec<String>,
}

/// 比较前后两版计划。
///
/// 只有**先前登记过、当时还没完成**的步骤这次标成 completed 才算一次转移：一上来就
/// 标成 completed 的新步骤是在补记历史，不是刚做完一件事——把它当安全点，模型只要
/// 一次性报一串"已完成"就能随时触发压缩。
pub fn analyze_transition(previous: &[PlanStep], next: &[PlanStep]) -> PlanTransition {
    let previous: BTreeMap<&str, &PlanStep> = previous
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect();
    let mut transition = PlanTransition::default();

    for step in next {
        let Some(prior) = previous.get(step.id.as_str()) else {
            continue;
        };
        if prior.status != PlanStatus::Completed && step.status == PlanStatus::Completed {
            transition.completed.push(step.clone());
        }
        if prior.goal != step.goal {
            transition.advice.push(format!(
                "步骤 {:?} 的目标变了：同一个 id 只用于同一个目标，换了目标就换个 id。",
                step.id
            ));
        }
    }

    let in_progress = next
        .iter()
        .filter(|step| step.status == PlanStatus::InProgress)
        .count();
    if in_progress > 1 {
        transition
            .advice
            .push("同一时间最多一个步骤处于 in_progress。".into());
    }
    if in_progress == 0 && next.iter().any(|step| step.status == PlanStatus::Pending) {
        transition
            .advice
            .push("开始做一个 pending 的步骤之前，先把它标成 in_progress。".into());
    }

    transition
}

/// 计划的快照：工具结果里就是这一段，模型下一轮照着它接着更新。
pub fn format_snapshot(steps: &[PlanStep]) -> String {
    #[derive(Serialize)]
    struct Snapshot<'a> {
        steps: &'a [PlanStep],
    }
    let body = serde_json::to_string(&Snapshot { steps }).expect("计划的每个字段都可序列化");
    format!(r#"<komo-plan task_status="active">{body}</komo-plan>"#)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::{OperationId, RunId, SessionId, ToolCallId};
    use crate::types::plan::{ExecutionPlan, Operation, PlanSource, PlanVersions, RecoveryMode};
    use crate::types::refs::INLINE_ARGUMENT_LIMIT_BYTES;

    fn step(id: &str, goal: &str, status: PlanStatus) -> PlanStep {
        PlanStep {
            id: id.into(),
            goal: goal.into(),
            status,
        }
    }

    fn update(steps: Vec<PlanStep>) -> PlanUpdate {
        PlanUpdate {
            steps,
            progress: None,
        }
    }

    #[test]
    fn completing_a_registered_step_is_a_transition() {
        let before = [
            step("a", "读代码", PlanStatus::InProgress),
            step("b", "改代码", PlanStatus::Pending),
        ];
        let after = [
            step("a", "读代码", PlanStatus::Completed),
            step("b", "改代码", PlanStatus::InProgress),
        ];
        let transition = analyze_transition(&before, &after);
        assert_eq!(transition.completed, vec![after[0].clone()]);
        assert!(transition.advice.is_empty(), "{:?}", transition.advice);
    }

    #[test]
    fn a_newly_introduced_completed_step_is_history_not_a_transition() {
        let before = [step("a", "读代码", PlanStatus::InProgress)];
        let after = [
            step("a", "读代码", PlanStatus::InProgress),
            step("z", "早就做完的事", PlanStatus::Completed),
        ];
        assert!(analyze_transition(&before, &after).completed.is_empty());
        assert!(analyze_transition(&[], &after).completed.is_empty());
    }

    #[test]
    fn a_step_already_completed_before_is_not_completed_again() {
        let before = [
            step("a", "读代码", PlanStatus::Completed),
            step("b", "改代码", PlanStatus::InProgress),
        ];
        assert!(analyze_transition(&before, &before).completed.is_empty());
    }

    #[test]
    fn several_steps_completed_at_once_are_all_reported_in_new_order() {
        let before = [
            step("a", "一", PlanStatus::InProgress),
            step("b", "二", PlanStatus::Pending),
            step("c", "三", PlanStatus::Pending),
        ];
        let after = [
            step("b", "二", PlanStatus::Completed),
            step("a", "一", PlanStatus::Completed),
            step("c", "三", PlanStatus::InProgress),
        ];
        let ids: Vec<_> = analyze_transition(&before, &after)
            .completed
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["b", "a"]);
    }

    #[test]
    fn changing_a_goal_under_the_same_id_is_advised_against() {
        let before = [step("a", "读代码", PlanStatus::InProgress)];
        let after = [step("a", "写文档", PlanStatus::InProgress)];
        let transition = analyze_transition(&before, &after);
        assert_eq!(transition.advice.len(), 1);
        assert!(
            transition.advice[0].contains("\"a\""),
            "{:?}",
            transition.advice
        );
        assert!(transition.advice[0].contains("目标变了"));
    }

    #[test]
    fn more_than_one_in_progress_is_advised_against() {
        let after = [
            step("a", "一", PlanStatus::InProgress),
            step("b", "二", PlanStatus::InProgress),
        ];
        let advice = analyze_transition(&[], &after).advice;
        assert_eq!(advice, vec!["同一时间最多一个步骤处于 in_progress。"]);
    }

    #[test]
    fn pending_work_with_nothing_in_progress_is_advised() {
        let after = [
            step("a", "一", PlanStatus::Completed),
            step("b", "二", PlanStatus::Pending),
        ];
        let advice = analyze_transition(&[], &after).advice;
        assert_eq!(
            advice,
            vec!["开始做一个 pending 的步骤之前，先把它标成 in_progress。"]
        );

        let all_done = [step("a", "一", PlanStatus::Completed)];
        assert!(analyze_transition(&[], &all_done).advice.is_empty());
    }

    #[test]
    fn a_plan_parses_from_snake_case_arguments() {
        let parsed: PlanUpdate = serde_json::from_value(serde_json::json!({
            "steps": [{"id": "a", "goal": "读代码", "status": "in_progress"}],
            "progress": {"files_changed": ["a.rs"], "verification": [], "decisions": []},
        }))
        .unwrap();
        assert_eq!(parsed.steps[0].status, PlanStatus::InProgress);
        assert_eq!(parsed.progress.unwrap().files_changed, ["a.rs"]);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let extra = serde_json::json!({
            "steps": [{"id": "a", "goal": "g", "status": "pending", "note": "x"}],
        });
        assert!(serde_json::from_value::<PlanUpdate>(extra).is_err());
        let bad_status = serde_json::json!({
            "steps": [{"id": "a", "goal": "g", "status": "done"}],
        });
        assert!(serde_json::from_value::<PlanUpdate>(bad_status).is_err());
    }

    #[test]
    fn validation_rejects_empty_duplicate_and_oversized_plans() {
        assert_eq!(update(vec![]).validate(), Err(PlanError::Empty));

        let duplicate = update(vec![
            step("a", "一", PlanStatus::Pending),
            step("a", "二", PlanStatus::Pending),
        ]);
        assert_eq!(
            duplicate.validate(),
            Err(PlanError::DuplicateId("a".into()))
        );

        let too_many = update(
            (0..=MAX_PLAN_STEPS)
                .map(|i| step(&i.to_string(), "g", PlanStatus::Pending))
                .collect(),
        );
        assert!(matches!(
            too_many.validate(),
            Err(PlanError::TooManySteps { .. })
        ));

        assert_eq!(
            update(vec![step("", "g", PlanStatus::Pending)]).validate(),
            Err(PlanError::EmptyField { field: "id" })
        );
        assert_eq!(
            update(vec![step("a", "", PlanStatus::Pending)]).validate(),
            Err(PlanError::EmptyField { field: "goal" })
        );
        let long_goal = "汉".repeat(MAX_STEP_GOAL_BYTES / 3 + 1);
        assert!(matches!(
            update(vec![step("a", &long_goal, PlanStatus::Pending)]).validate(),
            Err(PlanError::FieldTooLong { field: "goal", .. })
        ));
        let long_id = "x".repeat(MAX_STEP_ID_BYTES + 1);
        assert!(matches!(
            update(vec![step(&long_id, "g", PlanStatus::Pending)]).validate(),
            Err(PlanError::FieldTooLong { field: "id", .. })
        ));

        let mut with_progress = update(vec![step("a", "g", PlanStatus::Completed)]);
        with_progress.progress = Some(PlanProgress {
            decisions: vec!["d".into(); MAX_PROGRESS_ITEMS + 1],
            ..Default::default()
        });
        assert!(matches!(
            with_progress.validate(),
            Err(PlanError::TooManyItems {
                field: "decisions",
                ..
            })
        ));
    }

    #[test]
    fn every_step_at_its_limit_still_trips_the_total_budget() {
        let goal = "汉".repeat(MAX_STEP_GOAL_BYTES / 3);
        let full = update(
            (0..MAX_PLAN_STEPS)
                .map(|i| step(&format!("{i:0>64}"), &goal, PlanStatus::InProgress))
                .collect(),
        );
        assert!(matches!(full.validate(), Err(PlanError::TooLarge { .. })));
    }

    /// 上限是为了让整份计划连同 `ExecutionPlan` 的其余字段仍然内联进 `tool.planned`。
    #[test]
    fn a_plan_at_the_total_budget_fits_inline_in_the_execution_plan() {
        let goal = "汉".repeat(MAX_STEP_GOAL_BYTES / 3);
        let mut steps = Vec::new();
        let mut candidate = update(steps.clone());
        for i in 0..MAX_PLAN_STEPS {
            steps.push(step(&format!("step-{i}"), &goal, PlanStatus::Pending));
            let next = update(steps.clone());
            if next.validate().is_err() {
                break;
            }
            candidate = next;
        }
        let args_len = serde_json::to_vec(&candidate).unwrap().len();
        assert!(args_len > MAX_PLAN_UPDATE_BYTES - 400, "{args_len}");

        let plan = ExecutionPlan {
            operation_id: OperationId::from_raw("0199a0b1-2c3d-7e4f-8a9b-0c1d2e3f4a5b"),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("0199a0b1-2c3d-7e4f-8a9b-0c1d2e3f4a5c"),
            },
            tool: "update_plan".into(),
            operation: Operation::UpdatePlan,
            run: Some(RunId::from_raw("0199a0b1-2c3d-7e4f-8a9b-0c1d2e3f4a5d")),
            tool_call: Some(ToolCallId::from_raw("toolu_01ABCDEFGHIJKLMNOPQRSTUVWXYZ")),
            args: serde_json::to_value(&candidate).unwrap(),
            cwd: Some(format!("/Users/someone/.komo/workspaces/{}", "w".repeat(120)).into()),
            targets: vec![],
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: RecoveryMode::SafeReread,
        };
        let encoded = serde_json::to_vec(&plan).unwrap().len();
        assert!(encoded <= INLINE_ARGUMENT_LIMIT_BYTES, "{encoded}");
    }

    #[test]
    fn the_snapshot_carries_the_whole_plan() {
        let steps = [step("a", "读代码", PlanStatus::InProgress)];
        assert_eq!(
            format_snapshot(&steps),
            r#"<komo-plan task_status="active">{"steps":[{"id":"a","goal":"读代码","status":"in_progress"}]}</komo-plan>"#
        );
    }
}
