//! 一条 Run 的在线压缩状态：从事件日志折出来，**不单独存**。
//!
//! 它要回答的都是"到现在为止"的问题——发过几次请求、每完成一步平均花几次请求、上下文
//! 每次涨多少、压过几次、还欠多少缓存债——这些在日志里都有原始事实：每条
//! `message.assistant` 是一次 provider 请求（带它的提示 token 数），每个完成了的
//! `update_plan` 调用是一次计划更新。所以状态是 fold 出来的，重启之后重新折一遍就回
//! 来了，没有第二份真相会漂。
//!
//! 状态按 Run 划分：新的一条 Run 从零开始。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::economics::CompactionDebt;
use super::plan::{PlanProgress, PlanStatus, PlanStep, PlanTransition, PlanUpdate};
use crate::events::{Event, EventPayload};
use crate::types::ids::{RunId, ToolCallId};
use crate::types::plan::Operation;
use crate::types::refs::ToolResultStatus;

/// 完成一步时留下的进度，等下一次压缩写进摘要请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSummary {
    pub step_id: String,
    pub goal: String,
    pub files_changed: Vec<String>,
    pub verification: Vec<String>,
    pub decisions: Vec<String>,
    /// 当时还没完成的步骤目标。
    pub next_work: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OnlineState {
    /// 最近一次生效的整份计划。
    pub plan: Vec<PlanStep>,
    /// 上次压缩以来完成的步骤留下的进度。
    pub pending_progress: Vec<ProgressSummary>,
    /// 这条 Run 发过几次 provider 请求。
    pub request_count: u64,
    /// 上次压缩时的 `request_count`；没压过是 `None`。
    pub last_compaction_request_count: Option<u64>,
    /// 上一个计划边界时的 `request_count`。
    pub last_boundary_request_count: u64,
    /// 每完成一步各花了几次请求。
    pub completed_boundary_request_counts: Vec<u64>,
    /// 最近一次请求的提示 token 数；不知道（用量缺失，或刚压缩过）是 `None`。
    pub last_context_tokens: Option<u64>,
    pub positive_context_delta_total: u64,
    pub positive_context_delta_count: u64,
    pub compaction_count: u64,
    /// 还没还清的缓存债（token）。
    pub cache_debt_tokens: f64,
    /// 每次请求还多少债。
    pub cache_debt_repayment_tokens: u64,
    /// 刚过了一个计划边界：此后既没有新的请求，也没有记下压缩的结果。
    /// 这一刻是做压缩决定的时机。
    pub pending_boundary: bool,
}

impl OnlineState {
    /// 一次 provider 请求。`context_tokens` 是它整个提示的 token 数（含缓存）。
    ///
    /// 用量缺失时照样记一次请求、照样还一次债，但不知道上下文多大：涨幅不计，下一次
    /// 也不跟这一次比——跨过一次未知去比，会把两次的涨幅算成一次的。
    pub fn record_provider_request(&mut self, context_tokens: Option<u64>) {
        if let (Some(now), Some(last)) = (context_tokens, self.last_context_tokens)
            && now > last
        {
            self.positive_context_delta_total += now - last;
            self.positive_context_delta_count += 1;
        }
        self.request_count += 1;
        self.last_context_tokens = context_tokens;
        self.cache_debt_tokens =
            (self.cache_debt_tokens - self.cache_debt_repayment_tokens as f64).max(0.0);
        if self.cache_debt_tokens == 0.0 {
            self.cache_debt_repayment_tokens = 0;
        }
        self.pending_boundary = false;
    }

    /// 一次完成了的 `update_plan`。完成了一个先前登记过的步骤就是一个计划边界。
    pub fn record_plan_update(&mut self, update: &PlanUpdate) -> PlanTransition {
        let transition = super::plan::analyze_transition(&self.plan, &update.steps);
        match transition.completed.first() {
            Some(step) => {
                let progress = update
                    .progress
                    .as_ref()
                    .map(|progress| summary(step, progress, &update.steps));
                self.record_boundary(&update.steps, progress);
            }
            None => self.plan = update.steps.clone(),
        }
        transition
    }

    /// 一个计划边界：记下这一步花了几次请求。
    pub fn record_boundary(&mut self, plan: &[PlanStep], progress: Option<ProgressSummary>) {
        let interval = self
            .request_count
            .saturating_sub(self.last_boundary_request_count);
        self.plan = plan.to_vec();
        self.pending_progress.extend(progress);
        self.last_boundary_request_count = self.request_count;
        self.completed_boundary_request_counts.push(interval);
        self.pending_boundary = true;
    }

    /// 压缩完成。计划和每步的样本都留着：压缩换的是上下文的写法，不是哪些步骤做完了。
    pub fn record_compaction(&mut self, debt: CompactionDebt) {
        self.last_compaction_request_count = Some(self.request_count);
        self.pending_progress.clear();
        // 摘要换掉了前面的内容，上下文大小不能跨过它比较。
        self.last_context_tokens = None;
        self.compaction_count += 1;
        self.cache_debt_tokens += debt.debt_tokens.max(0.0);
        self.cache_debt_repayment_tokens += debt.repayment_tokens;
        self.pending_boundary = false;
    }

    /// 在边界上决定了不压（或者压不成）。上下文原样还在，只是这个边界用掉了。
    pub fn record_skipped(&mut self) {
        self.pending_boundary = false;
    }

    /// 每次请求上下文平均涨多少（只算涨的那些次）。
    pub fn average_context_token_increment(&self) -> Option<f64> {
        (self.positive_context_delta_count > 0).then(|| {
            self.positive_context_delta_total as f64 / self.positive_context_delta_count as f64
        })
    }

    /// 计划里还没完成的步数。
    pub fn remaining_boundaries(&self) -> u64 {
        self.plan
            .iter()
            .filter(|step| step.status != PlanStatus::Completed)
            .count() as u64
    }

    /// 上次压缩之后过了几次请求；没压过是 `None`。
    pub fn requests_since_last_compaction(&self) -> Option<u64> {
        self.last_compaction_request_count
            .map(|at| self.request_count.saturating_sub(at))
    }
}

fn summary(step: &PlanStep, progress: &PlanProgress, steps: &[PlanStep]) -> ProgressSummary {
    ProgressSummary {
        step_id: step.id.clone(),
        goal: step.goal.clone(),
        files_changed: progress.files_changed.clone(),
        verification: progress.verification.clone(),
        decisions: progress.decisions.clone(),
        next_work: steps
            .iter()
            .filter(|step| step.status != PlanStatus::Completed)
            .map(|step| step.goal.clone())
            .collect(),
    }
}

/// 把一条 Run 的事件折成 [`OnlineState`]。可切分：`fold(prefix).extend(rest)` 与
/// `fold(all)` 相等——一个调用的计划和结果可能落在切点两边，所以还没收到结果的
/// `update_plan` 也是折叠状态的一部分。
#[derive(Debug, Clone, PartialEq)]
pub struct OnlineFold {
    run: RunId,
    state: OnlineState,
    /// 已经有计划、还没有完成结果的 `update_plan` 调用。
    planned: BTreeMap<ToolCallId, PlanUpdate>,
}

impl OnlineFold {
    pub fn new(run: RunId) -> Self {
        Self {
            run,
            state: OnlineState::default(),
            planned: BTreeMap::new(),
        }
    }

    pub fn state(&self) -> &OnlineState {
        &self.state
    }

    pub fn into_state(self) -> OnlineState {
        self.state
    }

    pub fn extend<'a, I: IntoIterator<Item = &'a Event>>(&mut self, events: I) {
        for event in events {
            self.apply(event);
        }
    }

    fn apply(&mut self, event: &Event) {
        if event.run.as_ref() != Some(&self.run) {
            return;
        }
        match &event.payload {
            EventPayload::MessageAssistant(body) => {
                self.state.record_provider_request(body.input_tokens);
            }
            // 计划超过内联上限会外置（`plan_ref`），这里就读不到；`update_plan` 的参数
            // 上限保证了合法的计划总是内联的。解不出或不合法的参数，工具那边本来就会拒绝。
            EventPayload::ToolPlanned(body) => {
                if let Some(plan) = &body.plan
                    && plan.operation == Operation::UpdatePlan
                    && let Ok(update) = serde_json::from_value::<PlanUpdate>(plan.args.clone())
                    && update.validate().is_ok()
                {
                    self.planned.insert(body.call_id.clone(), update);
                }
            }
            // 只认完成的结果；同一个调用的后一条结果（先 uncertain、介入后补一条
            // completed）照样生效一次，之后的重复不再生效。
            EventPayload::ToolResult(body) => {
                if body.status == ToolResultStatus::Completed
                    && let Some(update) = self.planned.remove(&body.call_id)
                {
                    self.state.record_plan_update(&update);
                }
            }
            // 压缩事件（第 8 步的 `context.compacted`）在这里补一条：
            //   压了 → `self.state.record_compaction(debt)`；
            //   没压 / 压不成 → `self.state.record_skipped()`。
            // 其余事件与未知词汇对在线状态没有意义（§8.3）。
            _ => {}
        }
    }
}

/// 一条 Run 现在的在线状态。
pub fn online_state(events: &[Event], run: &RunId) -> OnlineState {
    let mut fold = OnlineFold::new(run.clone());
    fold.extend(events);
    fold.into_state()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        EVENT_FORMAT_VERSION, MessageAssistant, RunCompleted, ToolPlanned, ToolResult,
    };
    use crate::types::digest::ContentHash;
    use crate::types::ids::{AttemptId, EventId, OperationId, Seq, SessionId};
    use crate::types::plan::{ExecutionPlan, PlanSource, PlanVersions, RecoveryMode};
    use crate::types::refs::{ContentRef, OutputRef};
    use time::macros::datetime;

    const RUN: &str = "run-1";

    fn event(seq: u64, run: &str, payload: EventPayload) -> Event {
        Event {
            v: EVENT_FORMAT_VERSION,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("evt-{seq}")),
            session: SessionId::from_raw("sess-1"),
            run: Some(RunId::from_raw(run)),
            ts: datetime!(2026-09-15 08:00:00 UTC),
            payload,
        }
    }

    fn assistant(seq: u64, run: &str, input_tokens: Option<u64>) -> Event {
        event(
            seq,
            run,
            EventPayload::MessageAssistant(MessageAssistant {
                round: seq as u32,
                text: None,
                text_ref: None,
                tool_calls: vec![],
                provider_blocks: None,
                input_tokens,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
            }),
        )
    }

    fn step(id: &str, status: PlanStatus) -> serde_json::Value {
        let status = serde_json::to_value(status).unwrap();
        serde_json::json!({ "id": id, "goal": format!("做 {id}"), "status": status })
    }

    fn planned(
        seq: u64,
        run: &str,
        call: &str,
        operation: Operation,
        args: serde_json::Value,
    ) -> Event {
        let plan = ExecutionPlan {
            operation_id: OperationId::from_raw(format!("op-{seq}")),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            tool: "update_plan".into(),
            operation,
            run: Some(RunId::from_raw(run)),
            tool_call: Some(ToolCallId::from_raw(call)),
            args,
            cwd: None,
            targets: vec![],
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: RecoveryMode::SafeReread,
        };
        event(
            seq,
            run,
            EventPayload::ToolPlanned(ToolPlanned {
                call_id: ToolCallId::from_raw(call),
                plan_hash: plan.plan_hash(),
                plan: Some(Box::new(plan)),
                plan_ref: None,
            }),
        )
    }

    fn update_plan(seq: u64, call: &str, steps: Vec<serde_json::Value>) -> Event {
        planned(
            seq,
            RUN,
            call,
            Operation::UpdatePlan,
            serde_json::json!({ "steps": steps }),
        )
    }

    fn result(seq: u64, run: &str, call: &str, status: ToolResultStatus) -> Event {
        event(
            seq,
            run,
            EventPayload::ToolResult(ToolResult {
                call_id: ToolCallId::from_raw(call),
                attempt_id: AttemptId::from_raw(format!("attempt-{seq}")),
                status,
                output_ref: OutputRef(ContentRef {
                    path: format!("tool-output/{run}/{call}/output.json"),
                    size: 2,
                    hash: ContentHash::of_str("ok"),
                    pointer: None,
                }),
                elapsed_ms: 1,
                preview: None,
                stdout: None,
                stderr: None,
                attempt_state: None,
            }),
        )
    }

    fn done(seq: u64, call: &str) -> Event {
        result(seq, RUN, call, ToolResultStatus::Completed)
    }

    /// 两步计划：登记 → 三次请求后完成第一步 → 再两次请求后完成第二步。
    fn log() -> Vec<Event> {
        use PlanStatus::*;
        vec![
            assistant(1, RUN, Some(10_000)),
            update_plan(2, "c1", vec![step("a", InProgress), step("b", Pending)]),
            done(3, "c1"),
            assistant(4, RUN, Some(12_000)),
            assistant(5, RUN, Some(11_000)),
            update_plan(6, "c2", vec![step("a", Completed), step("b", InProgress)]),
            done(7, "c2"),
            assistant(8, RUN, None),
            assistant(9, RUN, Some(15_000)),
            assistant(10, RUN, Some(18_000)),
            update_plan(11, "c3", vec![step("a", Completed), step("b", Completed)]),
            done(12, "c3"),
        ]
    }

    #[test]
    fn a_plan_log_folds_to_the_expected_state() {
        let state = online_state(&log(), &RunId::from_raw(RUN));
        assert_eq!(state.request_count, 6);
        assert_eq!(
            state.completed_boundary_request_counts,
            vec![3, 3],
            "第一步花了请求 1、4、5；第二步花了 8、9、10"
        );
        assert_eq!(state.last_boundary_request_count, 6);
        // 涨幅：10000→12000 记 2000；12000→11000 不记；11000→未知 不比；
        // 未知→15000 不比；15000→18000 记 3000。
        assert_eq!(state.positive_context_delta_total, 5_000);
        assert_eq!(state.positive_context_delta_count, 2);
        assert_eq!(state.average_context_token_increment(), Some(2_500.0));
        assert_eq!(state.last_context_tokens, Some(18_000));
        assert_eq!(state.remaining_boundaries(), 0);
        assert!(state.pending_boundary);
        assert_eq!(state.compaction_count, 0);
        assert_eq!(state.requests_since_last_compaction(), None);
    }

    #[test]
    fn the_boundary_stays_pending_only_until_the_next_request() {
        let mut events = log();
        events.truncate(7);
        let run = RunId::from_raw(RUN);
        assert!(online_state(&events, &run).pending_boundary);
        events.push(assistant(8, RUN, Some(13_000)));
        assert!(!online_state(&events, &run).pending_boundary);
    }

    #[test]
    fn folding_a_prefix_then_the_rest_equals_folding_everything() {
        let events = log();
        let run = RunId::from_raw(RUN);
        let mut whole = OnlineFold::new(run.clone());
        whole.extend(&events);
        for cut in 0..=events.len() {
            let mut split = OnlineFold::new(run.clone());
            split.extend(&events[..cut]);
            split.extend(&events[cut..]);
            assert_eq!(split, whole, "切在 {cut}");
        }
    }

    #[test]
    fn a_new_completed_step_is_history_and_not_a_boundary() {
        use PlanStatus::*;
        let events = vec![
            assistant(1, RUN, Some(1_000)),
            update_plan(2, "c1", vec![step("old", Completed), step("a", InProgress)]),
            done(3, "c1"),
        ];
        let state = online_state(&events, &RunId::from_raw(RUN));
        assert!(state.completed_boundary_request_counts.is_empty());
        assert!(!state.pending_boundary);
        assert_eq!(state.plan.len(), 2);
    }

    #[test]
    fn only_completed_results_apply_and_each_call_applies_once() {
        use PlanStatus::*;
        let run = RunId::from_raw(RUN);
        let register = vec![
            update_plan(1, "c1", vec![step("a", InProgress)]),
            done(2, "c1"),
        ];
        let complete = || update_plan(3, "c2", vec![step("a", Completed)]);

        // 失败的结果不生效。
        let mut failed = register.clone();
        failed.push(complete());
        failed.push(result(4, RUN, "c2", ToolResultStatus::Failed));
        assert!(
            online_state(&failed, &run)
                .completed_boundary_request_counts
                .is_empty()
        );

        // 先 uncertain，介入后补一条 completed：生效一次。
        let mut resolved = register.clone();
        resolved.push(complete());
        resolved.push(result(4, RUN, "c2", ToolResultStatus::Uncertain));
        resolved.push(result(5, RUN, "c2", ToolResultStatus::Completed));
        resolved.push(result(6, RUN, "c2", ToolResultStatus::Completed));
        assert_eq!(
            online_state(&resolved, &run).completed_boundary_request_counts,
            vec![0]
        );
    }

    #[test]
    fn other_runs_other_tools_and_unknown_events_are_ignored() {
        use PlanStatus::*;
        let mut events = log();
        let run = RunId::from_raw(RUN);
        let expected = online_state(&events, &run);

        let mut seq = 100;
        let mut next = || {
            seq += 1;
            seq
        };
        events.push(assistant(next(), "child", Some(99_999)));
        let s = next();
        events.push(planned(
            s,
            "child",
            "k1",
            Operation::UpdatePlan,
            serde_json::json!({ "steps": [step("b", InProgress)] }),
        ));
        events.push(result(next(), "child", "k1", ToolResultStatus::Completed));
        let s = next();
        events.push(planned(
            s,
            RUN,
            "w1",
            Operation::WriteFile,
            serde_json::json!({ "steps": [step("a", Pending)] }),
        ));
        events.push(done(next(), "w1"));
        let s = next();
        events.push(event(
            s,
            RUN,
            EventPayload::Unknown {
                event_type: "context.someday".into(),
                raw: serde_json::json!({ "anything": true }),
            },
        ));
        let s = next();
        events.push(event(
            s,
            RUN,
            EventPayload::RunCompleted(RunCompleted {
                final_message: None,
                final_message_ref: None,
                rounds: 6,
            }),
        ));
        assert_eq!(online_state(&events, &run), expected);
    }

    #[test]
    fn invalid_plan_arguments_are_ignored() {
        let events = vec![
            update_plan(1, "c1", vec![]),
            done(2, "c1"),
            planned(
                3,
                RUN,
                "c2",
                Operation::UpdatePlan,
                serde_json::json!({ "steps": "nope" }),
            ),
            done(4, "c2"),
        ];
        assert_eq!(
            online_state(&events, &RunId::from_raw(RUN)),
            OnlineState::default()
        );
    }

    #[test]
    fn progress_is_kept_for_the_first_completed_step() {
        use PlanStatus::*;
        let mut events = vec![
            update_plan(1, "c1", vec![step("a", InProgress), step("b", Pending)]),
            done(2, "c1"),
        ];
        events.push(planned(
            3,
            RUN,
            "c2",
            Operation::UpdatePlan,
            serde_json::json!({
                "steps": [step("a", Completed), step("b", InProgress)],
                "progress": {
                    "files_changed": ["src/a.rs"],
                    "verification": ["cargo test 通过"],
                    "decisions": ["用 BTreeMap"],
                },
            }),
        ));
        events.push(done(4, "c2"));
        let state = online_state(&events, &RunId::from_raw(RUN));
        assert_eq!(
            state.pending_progress,
            vec![ProgressSummary {
                step_id: "a".into(),
                goal: "做 a".into(),
                files_changed: vec!["src/a.rs".into()],
                verification: vec!["cargo test 通过".into()],
                decisions: vec!["用 BTreeMap".into()],
                next_work: vec!["做 b".into()],
            }]
        );
    }

    #[test]
    fn a_compaction_resets_the_context_baseline_and_takes_on_debt() {
        let mut state = online_state(&log(), &RunId::from_raw(RUN));
        state.pending_progress.push(ProgressSummary {
            step_id: "a".into(),
            goal: "g".into(),
            files_changed: vec![],
            verification: vec![],
            decisions: vec![],
            next_work: vec![],
        });
        let before = state.clone();
        state.record_compaction(CompactionDebt {
            debt_tokens: 1_500.0,
            repayment_tokens: 1_000,
        });

        assert_eq!(state.compaction_count, 1);
        assert_eq!(state.last_compaction_request_count, Some(6));
        assert_eq!(state.requests_since_last_compaction(), Some(0));
        assert_eq!(state.last_context_tokens, None);
        assert!(state.pending_progress.is_empty());
        assert!(!state.pending_boundary);
        assert_eq!(state.plan, before.plan, "计划不随压缩丢");
        assert_eq!(
            state.completed_boundary_request_counts,
            before.completed_boundary_request_counts
        );
        assert_eq!(state.cache_debt_tokens, 1_500.0);
        assert_eq!(state.cache_debt_repayment_tokens, 1_000);

        // 压缩之后第一次请求不和压缩前比；每次请求还一次债，还清了就不再还。
        state.record_provider_request(Some(5_000));
        assert_eq!(
            state.positive_context_delta_count,
            before.positive_context_delta_count
        );
        assert_eq!(state.cache_debt_tokens, 500.0);
        assert_eq!(state.cache_debt_repayment_tokens, 1_000);
        state.record_provider_request(Some(6_000));
        assert_eq!(state.cache_debt_tokens, 0.0);
        assert_eq!(state.cache_debt_repayment_tokens, 0);
        assert_eq!(state.requests_since_last_compaction(), Some(2));
    }

    #[test]
    fn a_skipped_compaction_only_consumes_the_boundary() {
        let mut state = online_state(&log(), &RunId::from_raw(RUN));
        assert!(state.pending_boundary);
        let before = state.clone();
        state.record_skipped();
        assert!(!state.pending_boundary);
        assert_eq!(
            OnlineState {
                pending_boundary: true,
                ..state
            },
            before
        );
    }
}
