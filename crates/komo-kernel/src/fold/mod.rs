//! `fold(events) -> Surface`：把一串 JSONL 事件折成"现在是什么样"。
//!
//! **日志记录发生了什么，fold 决定它意味着什么。**正常修改只追加新事件；纠正、取消和
//! 覆盖关系通过新记录表达，不重写旧行（§8.3）——于是"这个 Session 现在长什么样"就必须
//! 是一个纯函数，而不是散在若干写入点上的规矩。
//!
//! 两条它必须守住的性质：
//!
//! 1. **可切分**：`fold(prefix).extend(rest) == fold(all)`，切在任何一点都成立。检查点
//!    读法（读一个快照 + 尾部增量）与全量重扫必须给出同一个结果，否则检查点就是另一
//!    份真相。
//! 2. **交替**：回放给模型的消息里用户与助手交替——多个 provider 拒绝连续两条用户
//!    消息。fold **只记录**违例（[`FoldViolation`]），不改写日志：改写会让人读到的
//!    历史和模型读到的历史不是同一份。

mod views;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use views::{
    ApprovalView, FoldViolation, RunView, SurfaceMessage, SurfaceToolResult, ToolCallView,
    UnknownEvent,
};

use crate::events::{Event, EventPayload};
use crate::types::ids::{ApprovalId, RunId, Seq, SessionId, ToolCallId};
use crate::types::status::{RunState, ToolCallState};
use crate::types::turn::Role;

/// 一串事件折出来的全部派生状态。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Surface {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// 连续、已校验的事件前缀推进到哪里（§8.5）。遇到断档就停在断档前。
    #[serde(default)]
    pub applied_seq: Seq,
    /// 见过的最大 seq，无论连不连续。
    #[serde(default)]
    pub max_seq: Seq,
    /// 整段会话的消息面。
    #[serde(default)]
    pub messages: Vec<SurfaceMessage>,
    /// 最新一个 `conversation.boundary` 在 `messages` 里的位置：回放窗口从这里开始。
    #[serde(default)]
    replay_from: usize,
    #[serde(default)]
    pub runs: BTreeMap<RunId, RunView>,
    #[serde(default)]
    pub calls: BTreeMap<ToolCallId, ToolCallView>,
    /// 还没有决定的审批。
    #[serde(default)]
    pub pending_approvals: BTreeMap<ApprovalId, ApprovalView>,
    #[serde(default)]
    pub unknown: Vec<UnknownEvent>,
    #[serde(default)]
    pub violations: Vec<FoldViolation>,
    /// 最近一条消息的角色，交替判定靠它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_role: Option<Role>,
    /// 最近一条事件的时间。
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_at: Option<OffsetDateTime>,
}

/// 把一串事件折成 [`Surface`]。
pub fn fold<'a, I: IntoIterator<Item = &'a Event>>(events: I) -> Surface {
    let mut surface = Surface::default();
    surface.extend(events);
    surface
}

impl Surface {
    /// 在已有状态上继续折。`fold(prefix).extend(rest)` 与 `fold(all)` 相等。
    pub fn extend<'a, I: IntoIterator<Item = &'a Event>>(&mut self, events: I) {
        for event in events {
            self.apply(event);
        }
    }

    /// 交给模型回放的窗口：最新一个 `conversation.boundary` 之后的消息（§13.1）。
    /// 最新一个 `conversation.boundary` 在 `messages` 里的位置。
    ///
    /// 记忆注入按它分段（§9.4）：**同一段里逐字复用**，只有换了段（`/new`）才重新召回。
    /// 注入段在 system 消息里，它一变，服务端前缀缓存里**整条前缀**（连对话历史）都失效。
    pub fn boundary(&self) -> usize {
        self.replay_from
    }

    pub fn replay(&self) -> &[SurfaceMessage] {
        &self.messages[self.replay_from.min(self.messages.len())..]
    }

    /// 这个 Run **还没有结果的调用**，按它们被要到的次序。
    ///
    /// 与 `runs[..].calls` 里那些非终态的不是一回事：`calls` 要等到 `tool.planned` 才有
    /// 那一行（计划是执行到它才 `prepare` 的），而**一轮里前一个调用停下时，后面的调用
    /// 连计划都还没有**（§8.3 把一轮的回复与全部调用计划当一个逻辑事件，但计划落盘是后
    /// 来的事）。只看 `calls`，续跑就只跑完前一个，模型下一轮拿到"要了两次、只回了一次
    /// 输出"的转写——provider 直接 400（`No tool output found for tool call …`）。
    pub fn open_calls(&self, run: &RunId) -> Vec<ToolCallId> {
        let mut open: Vec<ToolCallId> = Vec::new();
        for message in self.replay() {
            if message.run.as_ref() == Some(run) {
                for call in &message.tool_calls {
                    open.push(call.call_id.clone());
                }
            }
            // 结果按调用号销账，不按它落在哪条消息上。
            for result in &message.tool_results {
                open.retain(|call| call != &result.call);
            }
        }
        open
    }

    /// 回放窗口里用户侧与助手侧是否交替。
    pub fn replay_alternates(&self) -> bool {
        let mut previous: Option<Role> = None;
        for message in self.replay() {
            if previous.is_some_and(|last| last.is_user_side() == message.role.is_user_side()) {
                return false;
            }
            previous = Some(message.role);
        }
        true
    }

    /// 还没有终态的 Run。
    pub fn unfinished_runs(&self) -> impl Iterator<Item = &RunView> {
        self.runs.values().filter(|run| run.status.is_unfinished())
    }

    fn apply(&mut self, event: &Event) {
        self.note_seq(event.seq);
        self.last_at = Some(event.ts);
        if self.session.is_none() {
            self.session = Some(event.session.clone());
        }

        match &event.payload {
            EventPayload::RunAccepted(body) => {
                let run = self.run_mut(event);
                run.status = RunState::Accepted;
                run.input_event = Some(event.event_id.clone());
                run.delegate = body.delegate.clone();
                self.push_message(SurfaceMessage {
                    seq: event.seq,
                    event_id: event.event_id.clone(),
                    role: Role::User,
                    run: event.run.clone(),
                    text: body.text.clone(),
                    text_ref: body.text_ref.clone(),
                    tool_calls: Vec::new(),
                    tool_results: Vec::new(),
                    provider_blocks: None,
                });
            }
            EventPayload::RunQueued(_) => {
                let run = self.run_mut(event);
                run.status = RunState::Queued;
                // 回到"缺 worker"这一刻，"在等什么"就没了——它不再等任何外部条件。
                run.wait = None;
            }
            EventPayload::RunStarted(body) => {
                let run = self.run_mut(event);
                run.status = RunState::Running;
                run.wait = None;
                run.generation = Some(body.generation);
            }
            // 「停着」只有一个状态，**为什么停着**记在 `wait` 里（§8.4）。这正是
            // "排队二十分钟不知道为什么"的解药：界面读这一格就能说出在等谁、等到什么时候。
            EventPayload::RunWaiting(body) => {
                let run = self.run_mut(event);
                run.status = RunState::Waiting;
                run.wait = Some(body.reason.clone());
            }
            // 回收只说明"上一次执行没收尾、领取权交还了"，**它不是一个状态**（§8.9）：
            // 那条 Run 由 reconcile 按 §8.4 当场判成 `queued` 或 `waiting + intervention`，
            // 这里不替它猜。
            EventPayload::RunReclaimed(_) => {}
            EventPayload::RunCompleted(body) => {
                let event_id = event.event_id.clone();
                let run = self.run_mut(event);
                run.status = RunState::Completed;
                run.final_event = Some(event_id);
                run.final_message = body.final_message.clone();
            }
            EventPayload::RunFailed(_) => {
                let event_id = event.event_id.clone();
                let run = self.run_mut(event);
                run.status = RunState::Failed;
                run.final_event = Some(event_id);
            }
            EventPayload::RunCancelled(_) => {
                let event_id = event.event_id.clone();
                let run = self.run_mut(event);
                run.status = RunState::Cancelled;
                run.final_event = Some(event_id);
            }
            EventPayload::RunAbandoned(_) => {
                let event_id = event.event_id.clone();
                let run = self.run_mut(event);
                run.status = RunState::Abandoned;
                run.final_event = Some(event_id);
            }
            EventPayload::MessageUser(body) => self.push_message(SurfaceMessage {
                seq: event.seq,
                event_id: event.event_id.clone(),
                role: Role::User,
                run: event.run.clone(),
                text: body.text.clone(),
                text_ref: body.text_ref.clone(),
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
                provider_blocks: None,
            }),
            EventPayload::MessageAssistant(body) => {
                if event.run.is_some() {
                    let rounds = self.run_mut(event).rounds.max(body.round);
                    self.run_mut(event).rounds = rounds;
                }
                self.push_message(SurfaceMessage {
                    seq: event.seq,
                    event_id: event.event_id.clone(),
                    role: Role::Assistant,
                    run: event.run.clone(),
                    text: body.text.clone(),
                    text_ref: body.text_ref.clone(),
                    tool_calls: body.tool_calls.clone(),
                    tool_results: Vec::new(),
                    provider_blocks: body.provider_blocks.clone(),
                });
            }
            EventPayload::ConversationBoundary(_) => {
                // 回放从这一刀之后开始。日志本身一个字都没删。
                self.replay_from = self.messages.len();
                self.last_role = None;
            }
            EventPayload::ToolPlanned(body) => {
                let call = self.call_mut(&body.call_id, event);
                call.state = ToolCallState::Planned;
                call.plan_hash = Some(body.plan_hash.clone());
                call.plan_event = Some(event.event_id.clone());
                self.link_call(event, &body.call_id);
            }
            EventPayload::ToolStarted(body) => {
                let call = self.call_mut(&body.call_id, event);
                call.state = ToolCallState::Started;
                call.attempt = Some(body.attempt_id.clone());
                call.attempts += 1;
                if call.plan_hash.is_none() {
                    call.plan_hash = Some(body.plan_hash.clone());
                }
                self.link_call(event, &body.call_id);
            }
            EventPayload::ToolResult(body) => {
                if !self.calls.contains_key(&body.call_id) {
                    self.violations.push(FoldViolation::ResultWithoutCall {
                        seq: event.seq,
                        call: body.call_id.clone(),
                    });
                }
                let call = self.call_mut(&body.call_id, event);
                call.state = match body.status {
                    crate::types::refs::ToolResultStatus::Completed => ToolCallState::Completed,
                    crate::types::refs::ToolResultStatus::Failed => ToolCallState::Failed,
                    crate::types::refs::ToolResultStatus::Uncertain => ToolCallState::Uncertain,
                };
                call.attempt = Some(body.attempt_id.clone());
                call.output = Some(body.output_ref.clone());
                self.link_call(event, &body.call_id);
                self.push_tool_result(
                    event,
                    SurfaceToolResult {
                        call: body.call_id.clone(),
                        attempt: body.attempt_id.clone(),
                        status: body.status,
                        output: body.output_ref.clone(),
                        elapsed_ms: body.elapsed_ms,
                        // 投影要的就是这几格：有了它们，回放那一侧才渲染得出与刚跑完时
                        // **逐字节相同**的正文（大小、尾部提示都从引用里来）。
                        stdout: body.stdout.clone(),
                        stderr: body.stderr.clone(),
                        preview: body.preview.clone(),
                    },
                );
            }
            EventPayload::ApprovalRequested(body) => {
                self.pending_approvals.insert(
                    body.approval.clone(),
                    ApprovalView {
                        approval: body.approval.clone(),
                        short_id: body.short_id.clone(),
                        plan_hash: body.plan_hash.clone(),
                        call: body.call_id.clone(),
                        run: event.run.clone(),
                        reason: body.reason.clone(),
                        requested_seq: event.seq,
                    },
                );
            }
            EventPayload::ApprovalDecided(body) => {
                // 审计副本：它只说明这条不再待处理，**不创建授权**（§7.4）。
                self.pending_approvals.remove(&body.approval);
            }
            EventPayload::Checkpoint(_) | EventPayload::ConfigChanged(_) => {}
            EventPayload::Unknown { event_type, raw } => self.unknown.push(UnknownEvent {
                seq: event.seq,
                event_id: event.event_id.clone(),
                event_type: event_type.clone(),
                raw: raw.clone(),
            }),
        }

        if let Some(run) = event.run.as_ref().and_then(|id| self.runs.get_mut(id)) {
            run.last_seq = event.seq;
        }
    }

    fn note_seq(&mut self, seq: Seq) {
        if seq <= self.max_seq && self.max_seq != Seq::ZERO {
            self.violations.push(FoldViolation::SeqOutOfOrder {
                previous: self.max_seq,
                found: seq,
            });
        }
        if seq == self.applied_seq.next() {
            self.applied_seq = seq;
        } else if seq > self.applied_seq.next() {
            self.violations.push(FoldViolation::SeqGap {
                expected: self.applied_seq.next(),
                found: seq,
            });
        }
        self.max_seq = self.max_seq.max(seq);
    }

    fn push_message(&mut self, message: SurfaceMessage) {
        if self
            .last_role
            .is_some_and(|last| last.is_user_side() == message.role.is_user_side())
        {
            self.violations.push(FoldViolation::ConsecutiveRole {
                seq: message.seq,
                role: message.role,
            });
        }
        self.last_role = Some(message.role);
        self.messages.push(message);
    }

    /// 一轮里的多个结果并进**同一个**用户侧节点：provider 收到的是一条消息带几个
    /// tool_result 块，不是几条连续的用户消息。
    fn push_tool_result(&mut self, event: &Event, result: SurfaceToolResult) {
        if self.last_role == Some(Role::Tool)
            && let Some(last) = self.messages.last_mut()
        {
            last.tool_results.push(result);
            return;
        }
        self.push_message(SurfaceMessage {
            seq: event.seq,
            event_id: event.event_id.clone(),
            role: Role::Tool,
            run: event.run.clone(),
            text: None,
            text_ref: None,
            tool_calls: Vec::new(),
            tool_results: vec![result],
            provider_blocks: None,
        });
    }

    fn run_mut(&mut self, event: &Event) -> &mut RunView {
        let id = event
            .run
            .clone()
            .unwrap_or_else(|| RunId::from_raw(format!("orphan:{}", event.seq)));
        self.runs.entry(id.clone()).or_insert_with(|| RunView {
            run: id,
            status: RunState::Accepted,
            wait: None,
            input_event: None,
            final_event: None,
            final_message: None,
            generation: None,
            rounds: 0,
            delegate: None,
            calls: Vec::new(),
            first_seq: event.seq,
            last_seq: event.seq,
        })
    }

    fn call_mut(&mut self, call: &ToolCallId, event: &Event) -> &mut ToolCallView {
        self.calls
            .entry(call.clone())
            .or_insert_with(|| ToolCallView {
                call: call.clone(),
                run: event.run.clone(),
                state: ToolCallState::Planned,
                plan_hash: None,
                plan_event: None,
                attempt: None,
                output: None,
                attempts: 0,
                last_seq: event.seq,
            })
            .tap_seq(event.seq)
    }

    fn link_call(&mut self, event: &Event, call: &ToolCallId) {
        if event.run.is_some() {
            let run = self.run_mut(event);
            if !run.calls.contains(call) {
                run.calls.push(call.clone());
            }
        }
    }
}

impl ToolCallView {
    fn tap_seq(&mut self, seq: Seq) -> &mut Self {
        self.last_seq = seq;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        ConversationBoundary, EVENT_FORMAT_VERSION, MessageAssistant, MessageUser, RunAccepted,
        RunCompleted, RunQueued, RunStarted, ToolPlanned, ToolResult, ToolStarted,
    };
    use crate::types::digest::ContentHash;
    use crate::types::ids::{AttemptId, EventId, ExecutorId, RequestKey, ShortId};
    use crate::types::plan::PlanHash;
    use crate::types::plan::PlanSource;
    use crate::types::refs::{ContentRef, OutputRef, ToolResultStatus};
    use time::macros::datetime;

    fn event(seq: u64, run: Option<&str>, payload: EventPayload) -> Event {
        Event {
            v: EVENT_FORMAT_VERSION,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("evt-{seq}")),
            session: SessionId::from_raw("sess-1"),
            run: run.map(RunId::from_raw),
            ts: datetime!(2026-09-15 08:00:00 UTC) + time::Duration::seconds(seq as i64),
            payload,
        }
    }

    fn accepted(seq: u64, run: &str, text: &str) -> Event {
        event(
            seq,
            Some(run),
            EventPayload::RunAccepted(RunAccepted {
                request_key: RequestKey::new(format!("api:{seq}")),
                input_hash: ContentHash::of_str(text),
                text: Some(text.to_string()),
                text_ref: None,
                source: PlanSource::Interactive {
                    session: SessionId::from_raw("sess-1"),
                },
                peer: None,
                model: None,
                effort: None,
                delegate: None,
            }),
        )
    }

    fn assistant(seq: u64, run: &str, round: u32, text: &str) -> Event {
        event(
            seq,
            Some(run),
            EventPayload::MessageAssistant(MessageAssistant {
                round,
                text: Some(text.to_string()),
                text_ref: None,
                tool_calls: vec![],
                provider_blocks: None,
                input_tokens: None,
                output_tokens: None,
            }),
        )
    }

    fn output_ref(path: &str) -> OutputRef {
        OutputRef(ContentRef {
            path: path.into(),
            size: 2,
            hash: ContentHash::of_str("ok"),
            pointer: None,
        })
    }

    /// 一次完整的调用：从输入到结果。
    fn conversation() -> Vec<Event> {
        vec![
            accepted(1, "run-1", "把 1 + 1 算出来"),
            event(
                2,
                Some("run-1"),
                EventPayload::RunQueued(RunQueued {
                    input_ref: EventId::from_raw("evt-1"),
                }),
            ),
            event(
                3,
                Some("run-1"),
                EventPayload::RunStarted(RunStarted {
                    executor: ExecutorId::from_raw("ex-1"),
                    generation: 1,
                }),
            ),
            assistant(4, "run-1", 1, "我算一下"),
            event(
                5,
                Some("run-1"),
                EventPayload::ToolPlanned(ToolPlanned {
                    call_id: ToolCallId::from_raw("call-7"),
                    plan_hash: PlanHash::from_raw("h1"),
                    plan: None,
                    plan_ref: None,
                }),
            ),
            event(
                6,
                Some("run-1"),
                EventPayload::ToolStarted(ToolStarted {
                    call_id: ToolCallId::from_raw("call-7"),
                    attempt_id: AttemptId::from_raw("attempt-1"),
                    plan_ref: EventId::from_raw("evt-5"),
                    plan_hash: PlanHash::from_raw("h1"),
                    grant: None,
                }),
            ),
            event(
                7,
                Some("run-1"),
                EventPayload::ToolResult(ToolResult {
                    call_id: ToolCallId::from_raw("call-7"),
                    attempt_id: AttemptId::from_raw("attempt-1"),
                    status: ToolResultStatus::Completed,
                    output_ref: output_ref("tool-output/run-1/call-7/attempt-1/output.json"),
                    elapsed_ms: 12,
                    preview: Some("result = 2".into()),
                    stdout: None,
                    stderr: None,
                    attempt_state: None,
                }),
            ),
            assistant(8, "run-1", 2, "等于 2"),
            event(
                9,
                Some("run-1"),
                EventPayload::RunCompleted(RunCompleted {
                    final_message: Some("等于 2".into()),
                    final_message_ref: None,
                    rounds: 2,
                }),
            ),
        ]
    }

    #[test]
    fn a_finished_conversation_folds_to_its_result() {
        let surface = fold(&conversation());
        assert_eq!(surface.applied_seq, Seq(9));
        assert_eq!(surface.violations, vec![]);
        assert_eq!(surface.messages.len(), 4);
        assert_eq!(surface.messages[0].role, Role::User);
        assert_eq!(surface.messages[1].role, Role::Assistant);
        assert_eq!(
            surface.messages[2].role,
            Role::Tool,
            "结果是用户侧的一个节点"
        );
        assert_eq!(surface.messages[2].tool_results.len(), 1);
        assert_eq!(surface.messages[3].role, Role::Assistant);

        let run = &surface.runs[&RunId::from_raw("run-1")];
        assert_eq!(run.status, RunState::Completed);
        assert_eq!(run.rounds, 2);
        assert_eq!(run.calls, vec![ToolCallId::from_raw("call-7")]);
        assert_eq!(run.final_message.as_deref(), Some("等于 2"));

        let call = &surface.calls[&ToolCallId::from_raw("call-7")];
        assert_eq!(call.state, ToolCallState::Completed);
        assert_eq!(call.attempts, 1);
        assert!(call.output.is_some());
        assert!(surface.replay_alternates());
    }

    #[test]
    fn folding_a_prefix_then_the_rest_equals_folding_everything() {
        let events = conversation();
        let whole = fold(&events);
        for split in 0..=events.len() {
            let mut partial = fold(&events[..split]);
            partial.extend(&events[split..]);
            assert_eq!(partial, whole, "切在 {split}");
        }
    }

    #[test]
    fn a_boundary_moves_the_replay_window_without_touching_the_transcript() {
        let mut events = conversation();
        let next = events.len() as u64 + 1;
        events.push(event(
            next,
            None,
            EventPayload::ConversationBoundary(ConversationBoundary { by: None }),
        ));
        events.push(accepted(next + 1, "run-2", "新话题"));

        let surface = fold(&events);
        assert_eq!(surface.messages.len(), 5, "整段会话还在");
        assert_eq!(surface.replay().len(), 1, "回放只从边界之后开始");
        assert_eq!(surface.replay()[0].text.as_deref(), Some("新话题"));
        assert!(surface.replay_alternates());
    }

    #[test]
    fn a_boundary_splits_the_same_way_wherever_you_cut() {
        let mut events = conversation();
        events.push(event(
            10,
            None,
            EventPayload::ConversationBoundary(ConversationBoundary { by: None }),
        ));
        events.push(accepted(11, "run-2", "新话题"));
        events.push(assistant(12, "run-2", 1, "好"));
        let whole = fold(&events);
        for split in 0..=events.len() {
            let mut partial = fold(&events[..split]);
            partial.extend(&events[split..]);
            assert_eq!(partial, whole, "切在 {split}");
        }
    }

    #[test]
    fn two_user_messages_in_a_row_are_recorded_not_repaired() {
        let events = vec![
            accepted(1, "run-1", "第一句"),
            event(
                2,
                Some("run-1"),
                EventPayload::MessageUser(MessageUser {
                    text: Some("第二句".into()),
                    text_ref: None,
                }),
            ),
        ];
        let surface = fold(&events);
        assert_eq!(surface.messages.len(), 2, "日志一行没少");
        assert_eq!(
            surface.violations,
            vec![FoldViolation::ConsecutiveRole {
                seq: Seq(2),
                role: Role::User
            }]
        );
        assert!(!surface.replay_alternates());
    }

    #[test]
    fn a_gap_in_seq_stops_applied_seq_before_it() {
        let events = vec![accepted(1, "run-1", "a"), assistant(5, "run-1", 1, "b")];
        let surface = fold(&events);
        assert_eq!(surface.applied_seq, Seq(1), "连续前缀只到 1");
        assert_eq!(surface.max_seq, Seq(5));
        assert_eq!(
            surface.violations,
            vec![FoldViolation::SeqGap {
                expected: Seq(2),
                found: Seq(5)
            }]
        );
    }

    #[test]
    fn a_pending_approval_disappears_when_it_is_decided() {
        use crate::events::{ApprovalDecided, ApprovalRequested};
        use crate::types::chat::ApprovalScope;

        let mut events = vec![
            accepted(1, "run-1", "删掉那个目录"),
            event(
                2,
                Some("run-1"),
                EventPayload::ApprovalRequested(ApprovalRequested {
                    approval: ApprovalId::from_raw("ap-1"),
                    short_id: ShortId::from_index(7),
                    plan_hash: PlanHash::from_raw("h1"),
                    call_id: Some(ToolCallId::from_raw("call-1")),
                    reason: "任意 shell".into(),
                    scopes: vec![ApprovalScope::Once],
                }),
            ),
        ];
        let surface = fold(&events);
        assert_eq!(surface.pending_approvals.len(), 1);

        events.push(event(
            3,
            Some("run-1"),
            EventPayload::ApprovalDecided(ApprovalDecided {
                approval: ApprovalId::from_raw("ap-1"),
                approved: true,
                scope: ApprovalScope::Once,
                by: None,
                decided_at: datetime!(2026-09-15 08:05:00 UTC),
                grant: None,
            }),
        ));
        let surface = fold(&events);
        assert!(surface.pending_approvals.is_empty());
    }

    #[test]
    fn an_unknown_event_lands_in_its_own_list_and_changes_nothing_else() {
        let mut events = conversation();
        events.insert(
            4,
            event(
                100,
                Some("run-1"),
                EventPayload::Unknown {
                    event_type: "memory.promoted".into(),
                    raw: serde_json::json!({"memory_id": "m-1"}),
                },
            ),
        );
        let surface = fold(&events);
        assert_eq!(surface.unknown.len(), 1);
        assert_eq!(surface.unknown[0].seq, Seq(100));
        assert_eq!(surface.messages.len(), 4, "消息面不受影响");
    }

    /// 一个确定性的线性同余生成器。属性测试要可复现，而 `test-support` 之外不加
    /// dev 依赖（§13.4），所以自己写一个。
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn folding_splits_the_same_way_on_randomly_generated_logs() {
        for seed in 0..40u64 {
            let mut rng = Lcg(seed.wrapping_mul(2654435761).wrapping_add(1));
            let mut events = Vec::new();
            let mut seq = 0u64;
            let length = 4 + rng.below(20);
            for _ in 0..length {
                seq += 1;
                let run = format!("run-{}", rng.below(3) + 1);
                let payload = match rng.below(8) {
                    0 => return_accepted(seq, &run),
                    1 => EventPayload::RunQueued(RunQueued {
                        input_ref: EventId::from_raw("evt-0"),
                    }),
                    2 => EventPayload::RunStarted(RunStarted {
                        executor: ExecutorId::from_raw("ex-1"),
                        generation: rng.below(4),
                    }),
                    3 => EventPayload::MessageAssistant(MessageAssistant {
                        round: (rng.below(4) + 1) as u32,
                        text: Some("回复".into()),
                        text_ref: None,
                        tool_calls: vec![],
                        provider_blocks: None,
                        input_tokens: None,
                        output_tokens: None,
                    }),
                    4 => EventPayload::ConversationBoundary(ConversationBoundary { by: None }),
                    5 => EventPayload::ToolStarted(ToolStarted {
                        call_id: ToolCallId::from_raw(format!("call-{}", rng.below(3))),
                        attempt_id: AttemptId::from_raw("attempt-1"),
                        plan_ref: EventId::from_raw("evt-0"),
                        plan_hash: PlanHash::from_raw("h"),
                        grant: None,
                    }),
                    6 => EventPayload::ToolResult(ToolResult {
                        call_id: ToolCallId::from_raw(format!("call-{}", rng.below(3))),
                        attempt_id: AttemptId::from_raw("attempt-1"),
                        status: ToolResultStatus::Completed,
                        output_ref: output_ref("tool-output/x/output.json"),
                        elapsed_ms: 1,
                        preview: None,
                        stdout: None,
                        stderr: None,
                        attempt_state: None,
                    }),
                    _ => EventPayload::Unknown {
                        event_type: "future.thing".into(),
                        raw: serde_json::json!({}),
                    },
                };
                let has_run = !matches!(payload, EventPayload::ConversationBoundary(_));
                events.push(event(seq, has_run.then_some(run.as_str()), payload));
            }

            let whole = fold(&events);
            for split in 0..=events.len() {
                let mut partial = fold(&events[..split]);
                partial.extend(&events[split..]);
                assert_eq!(partial, whole, "seed={seed} 切在 {split}");
            }
        }
    }

    fn return_accepted(seq: u64, _run: &str) -> EventPayload {
        EventPayload::RunAccepted(RunAccepted {
            request_key: RequestKey::new(format!("api:{seq}")),
            input_hash: ContentHash::of_str("x"),
            text: Some("输入".into()),
            text_ref: None,
            source: PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            peer: None,
            model: None,
            effort: None,
            delegate: None,
        })
    }
}
