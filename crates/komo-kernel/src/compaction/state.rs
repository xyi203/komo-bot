//! 一条 Run 的在线压缩状态：从事件日志折出来，**不单独存**。
//!
//! 它要回答的都是"到现在为止"的问题——发过几次请求、上下文每次涨多少、压过几次、还欠
//! 多少缓存债——这些在日志里都有原始事实：每条 `message.assistant` 是一次 provider 请求
//! （带它的提示 token 数），每条 `context.compacted` 是一次改写或一次没改成。所以状态是
//! fold 出来的，重启之后重新折一遍就回来了，没有第二份真相会漂。
//!
//! 状态按 Run 划分：新的一条 Run 从零开始。

use serde::{Deserialize, Serialize};

use super::economics::CompactionDebt;
use crate::events::{ContextCompacted, Event, EventPayload};
use crate::types::ids::RunId;
use crate::types::refs::ToolResultStatus;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OnlineState {
    /// 这条 Run 发过几次 provider 请求。
    pub request_count: u64,
    /// 上次压缩时的 `request_count`；没压过是 `None`。
    pub last_compaction_request_count: Option<u64>,
    /// 最近一次请求的提示 token 数；不知道（用量缺失，或刚压缩过）是 `None`。
    pub last_context_tokens: Option<u64>,
    pub positive_context_delta_total: u64,
    pub positive_context_delta_count: u64,
    pub compaction_count: u64,
    /// 还没还清的缓存债（token）。
    pub cache_debt_tokens: f64,
    /// 每次请求还多少债。
    pub cache_debt_repayment_tokens: u64,
    /// 上一次决定了压、摘要请求却没成（`skipped` 带着 `compact: true` 的决策）。此后
    /// 窗口压力不再触发重试，直到出现真正的工具工作（一次完成了的调用）或一次成功的
    /// 压缩——同一段上下文再请求一次摘要，多半还是同样的结果。
    pub compaction_refused: bool,
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
    }

    /// 压缩完成。
    pub fn record_compaction(&mut self, debt: CompactionDebt) {
        self.last_compaction_request_count = Some(self.request_count);
        // 摘要换掉了前面的内容，上下文大小不能跨过它比较。
        self.last_context_tokens = None;
        self.compaction_count += 1;
        self.cache_debt_tokens += debt.debt_tokens.max(0.0);
        self.cache_debt_repayment_tokens += debt.repayment_tokens;
        self.compaction_refused = false;
    }

    /// 一批结果换成了短视图：欠下的缓存债与压缩记在同一本账上，冷却也从这里算——两次
    /// 改写挨着做，第二次又让刚写好的缓存失效。
    pub fn record_decay(&mut self, debt: CompactionDebt) {
        self.last_compaction_request_count = Some(self.request_count);
        self.last_context_tokens = None;
        self.cache_debt_tokens += debt.debt_tokens.max(0.0);
        self.cache_debt_repayment_tokens += debt.repayment_tokens;
    }

    /// 一条 `skipped`。`refused` = 决定了压、摘要请求却没成；旧日志里计划边界上"决定
    /// 不压"的那些不是拒绝，什么也不改。
    pub fn record_skipped(&mut self, refused: bool) {
        if refused {
            self.compaction_refused = true;
        }
    }

    /// 一次调用完成了：上下文里有了新的工作，可以再试着压。
    pub fn record_tool_work(&mut self) {
        self.compaction_refused = false;
    }

    /// 每次请求上下文平均涨多少（只算涨的那些次）。
    pub fn average_context_token_increment(&self) -> Option<f64> {
        (self.positive_context_delta_count > 0).then(|| {
            self.positive_context_delta_total as f64 / self.positive_context_delta_count as f64
        })
    }

    /// 上次压缩之后过了几次请求；没压过是 `None`。
    pub fn requests_since_last_compaction(&self) -> Option<u64> {
        self.last_compaction_request_count
            .map(|at| self.request_count.saturating_sub(at))
    }
}

/// 把一条 Run 的事件折成 [`OnlineState`]。可切分：`fold(prefix).extend(rest)` 与
/// `fold(all)` 相等。
#[derive(Debug, Clone, PartialEq)]
pub struct OnlineFold {
    run: RunId,
    state: OnlineState,
}

impl OnlineFold {
    pub fn new(run: RunId) -> Self {
        Self {
            run,
            state: OnlineState::default(),
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
            // 只认完成的结果。旧日志里的 `update_plan`（2026-10-16 退役）也照一次完成的
            // 调用算，计划本身不再有意义。
            EventPayload::ToolResult(body) if body.status == ToolResultStatus::Completed => {
                self.state.record_tool_work();
            }
            EventPayload::ContextCompacted(ContextCompacted::Compacted { debt, .. }) => {
                self.state.record_compaction(*debt);
            }
            EventPayload::ContextCompacted(ContextCompacted::Decayed { debt, .. }) => {
                self.state.record_decay(*debt);
            }
            EventPayload::ContextCompacted(ContextCompacted::Skipped { decision, .. }) => {
                self.state
                    .record_skipped(decision.as_ref().is_some_and(|d| d.compact));
            }
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
    use crate::events::{EVENT_FORMAT_VERSION, MessageAssistant, RunCompleted, ToolResult};
    use crate::types::digest::ContentHash;
    use crate::types::ids::{AttemptId, EventId, Seq, SessionId, ToolCallId};
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
                reasoning_tokens: None,
            }),
        )
    }

    fn result(seq: u64, run: &str, call: &str, status: ToolResultStatus) -> Event {
        event(
            seq,
            run,
            EventPayload::ToolResult(ToolResult {
                body: None,
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

    /// 六次请求，中间穿插几次调用。
    fn log() -> Vec<Event> {
        vec![
            assistant(1, RUN, Some(10_000)),
            done(2, "c1"),
            assistant(3, RUN, Some(12_000)),
            assistant(4, RUN, Some(11_000)),
            done(5, "c2"),
            assistant(6, RUN, None),
            assistant(7, RUN, Some(15_000)),
            assistant(8, RUN, Some(18_000)),
            done(9, "c3"),
        ]
    }

    #[test]
    fn a_log_folds_to_the_expected_state() {
        let state = online_state(&log(), &RunId::from_raw(RUN));
        assert_eq!(state.request_count, 6);
        // 涨幅：10000→12000 记 2000；12000→11000 不记；11000→未知 不比；
        // 未知→15000 不比；15000→18000 记 3000。
        assert_eq!(state.positive_context_delta_total, 5_000);
        assert_eq!(state.positive_context_delta_count, 2);
        assert_eq!(state.average_context_token_increment(), Some(2_500.0));
        assert_eq!(state.last_context_tokens, Some(18_000));
        assert_eq!(state.compaction_count, 0);
        assert_eq!(state.requests_since_last_compaction(), None);
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
    fn other_runs_and_unknown_events_are_ignored() {
        let mut events = log();
        let run = RunId::from_raw(RUN);
        let expected = online_state(&events, &run);

        events.push(assistant(101, "child", Some(99_999)));
        events.push(result(102, "child", "k1", ToolResultStatus::Completed));
        events.push(event(
            103,
            RUN,
            EventPayload::Unknown {
                event_type: "context.someday".into(),
                raw: serde_json::json!({ "anything": true }),
            },
        ));
        events.push(event(
            104,
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
    fn a_compaction_resets_the_context_baseline_and_takes_on_debt() {
        let mut state = online_state(&log(), &RunId::from_raw(RUN));
        let before = state.clone();
        state.record_compaction(CompactionDebt {
            debt_tokens: 1_500.0,
            repayment_tokens: 1_000,
        });

        assert_eq!(state.compaction_count, 1);
        assert_eq!(state.last_compaction_request_count, Some(6));
        assert_eq!(state.requests_since_last_compaction(), Some(0));
        assert_eq!(state.last_context_tokens, None);
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

    fn compaction(seq: u64, run: &str, payload: ContextCompacted) -> Event {
        event(seq, run, EventPayload::ContextCompacted(payload))
    }

    /// 旧日志里计划边界上"决定不压"的那种。
    fn skipped(seq: u64) -> Event {
        compaction(
            seq,
            RUN,
            ContextCompacted::Skipped {
                reason: "deferred_economic".into(),
                decision: None,
            },
        )
    }

    #[test]
    fn compaction_events_fold_into_the_same_state_as_recording_them() {
        let run = RunId::from_raw(RUN);
        let compacted = crate::test_support::compacted(Seq(8), "摘要");
        let ContextCompacted::Compacted { debt, .. } = &compacted else {
            unreachable!()
        };
        let debt = *debt;

        let mut events = log();
        events.push(compaction(13, RUN, compacted.clone()));
        let mut expected = online_state(&log(), &run);
        expected.record_compaction(debt);
        assert_eq!(online_state(&events, &run), expected);
        assert_eq!(expected.compaction_count, 1);

        // 一条不是拒绝的 skipped 什么也不改。
        events.push(skipped(14));
        assert_eq!(online_state(&events, &run), expected);

        // 别的 Run 的压缩不算这条的。
        let mut others = log();
        others.push(compaction(13, "child", compacted));
        assert_eq!(online_state(&others, &run), online_state(&log(), &run));
    }

    /// 决定了压、摘要却没成：先不在窗口压力下重试，直到有真正的工具工作或一次成功的
    /// 压缩。不是拒绝的 `skipped` 不算。
    #[test]
    fn a_refused_summary_holds_until_real_work_or_a_compaction() {
        let run = RunId::from_raw(RUN);
        let refused = |seq| {
            let ContextCompacted::Compacted { decision, .. } =
                crate::test_support::compacted(Seq(8), "摘要")
            else {
                unreachable!()
            };
            compaction(
                seq,
                RUN,
                ContextCompacted::Skipped {
                    reason: "摘要请求失败".into(),
                    decision: Some(decision),
                },
            )
        };
        let mut events = log();
        events.push(skipped(13));
        assert!(!online_state(&events, &run).compaction_refused);
        events.push(refused(14));
        assert!(online_state(&events, &run).compaction_refused);

        // 失败的调用不算工作。
        events.push(result(15, RUN, "w1", ToolResultStatus::Failed));
        assert!(online_state(&events, &run).compaction_refused);
        events.push(done(16, "w2"));
        assert!(!online_state(&events, &run).compaction_refused);

        events.push(refused(17));
        assert!(online_state(&events, &run).compaction_refused);
        events.push(compaction(
            18,
            RUN,
            crate::test_support::compacted(Seq(8), "摘要"),
        ));
        assert!(
            !online_state(&events, &run).compaction_refused,
            "压成了解除"
        );
    }

    #[test]
    fn folding_a_log_with_compactions_splits_the_same_way() {
        let mut events = log();
        events.push(compaction(
            13,
            RUN,
            crate::test_support::compacted(Seq(8), "摘要"),
        ));
        events.push(assistant(14, RUN, Some(9_000)));
        events.push(skipped(15));
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
}
