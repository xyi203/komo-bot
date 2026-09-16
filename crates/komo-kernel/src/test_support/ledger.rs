//! 内存里的 [`Ledger`]。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::traits::*;
use crate::types::ids::*;

use crate::events::{
    ConversationBoundary, EVENT_FORMAT_VERSION, Event, EventPayload, MessageAssistant, RunAccepted,
    RunCancelled, RunCompleted, RunFailed, RunNeedsAttention, RunQueued, RunWaitingApproval,
    RunWaitingRetry, ToolPlanned, ToolResult, ToolStarted,
};
use crate::fold::{Surface, fold};
use crate::types::plan::{ExecutionPlan, PlanHash};
use crate::types::refs::{PublishedOutput, ToolResultStatus};
use crate::types::status::{AttemptState, RunEnd, Wait};
use crate::types::turn::{AcceptInput, Accepted, AssistantRound, EventBatch, GrantUse};

use super::TestClock;

#[derive(Debug, Default)]
struct LedgerState {
    events: Vec<Event>,
    next_seq: u64,
    /// request_key → (run, 输入哈希)
    accepted: BTreeMap<String, (RunId, crate::types::digest::ContentHash)>,
    /// call → 它所属的 run，以及承载计划的事件。
    calls: BTreeMap<ToolCallId, CallState>,
    /// attempt → call
    attempts: BTreeMap<AttemptId, ToolCallId>,
    sessions: BTreeMap<RunId, SessionId>,
}

#[derive(Debug, Clone)]
struct CallState {
    run: RunId,
    session: SessionId,
    plan_event: Option<EventId>,
    plan_hash: Option<PlanHash>,
}

/// 内存里的 [`Ledger`]：真的按 seq 追加事件，真的能 fold。
#[derive(Debug, Clone)]
pub struct MemLedger {
    state: Arc<Mutex<LedgerState>>,
    clock: TestClock,
}

impl MemLedger {
    pub fn new(clock: TestClock) -> Self {
        Self {
            state: Arc::new(Mutex::new(LedgerState::default())),
            clock,
        }
    }

    /// 到目前为止写下的全部事件。
    pub fn events(&self) -> Vec<Event> {
        self.state.lock().expect("账本").events.clone()
    }

    /// 折出来的现状——替身"够真"的那部分就在这：它折得出来。
    pub fn surface(&self) -> Surface {
        fold(&self.events())
    }

    /// 每一行都能序列化成一条合法 JSONL，且读回来一模一样。
    pub fn to_jsonl(&self) -> String {
        self.events()
            .iter()
            .map(|e| e.to_line().expect("事件可序列化"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn append(
        &self,
        state: &mut LedgerState,
        session: &SessionId,
        run: Option<RunId>,
        payload: EventPayload,
    ) -> (EventId, Seq) {
        state.next_seq += 1;
        let seq = Seq(state.next_seq);
        let event_id = EventId::new_at(self.clock.now());
        state.events.push(Event {
            v: EVENT_FORMAT_VERSION,
            seq,
            event_id: event_id.clone(),
            session: session.clone(),
            run,
            ts: self.clock.now(),
            payload,
        });
        (event_id, seq)
    }
}

#[async_trait]
impl Ledger for MemLedger {
    async fn accept_input(&self, input: AcceptInput) -> Result<Accepted, LedgerError> {
        let mut state = self.state.lock().expect("账本");
        let hash = input.input_hash();
        if let Some((run, previous)) = state.accepted.get(input.request_key.as_str()).cloned() {
            if previous != hash {
                return Err(LedgerError::RequestKeyConflict {
                    key: input.request_key.to_string(),
                });
            }
            let seq = Seq(state.next_seq);
            return Ok(Accepted {
                run,
                session: input.session,
                event: EventId::from_raw("deduplicated"),
                seq,
                deduplicated: true,
            });
        }

        let run = RunId::new_at(self.clock.now());
        let (event, seq) = self.append(
            &mut state,
            &input.session,
            Some(run.clone()),
            EventPayload::RunAccepted(RunAccepted {
                request_key: input.request_key.clone(),
                input_hash: hash.clone(),
                text: Some(input.text.clone()),
                text_ref: None,
                source: input.source.clone(),
                peer: input.peer.as_ref().map(|p| p.to_string()),
                model: Some(input.model.model.clone()),
                effort: None,
            }),
        );
        self.append(
            &mut state,
            &input.session,
            Some(run.clone()),
            EventPayload::RunQueued(RunQueued {
                input_ref: event.clone(),
            }),
        );
        state
            .accepted
            .insert(input.request_key.to_string(), (run.clone(), hash));
        state.sessions.insert(run.clone(), input.session.clone());

        Ok(Accepted {
            run,
            session: input.session,
            event,
            seq,
            deduplicated: false,
        })
    }

    async fn record_round(
        &self,
        run: &RunId,
        round: AssistantRound,
    ) -> Result<Vec<ToolCallId>, LedgerError> {
        let mut state = self.state.lock().expect("账本");
        let session = state
            .sessions
            .get(run)
            .cloned()
            .ok_or_else(|| LedgerError::NotFound {
                what: format!("run {run}"),
            })?;
        let calls: Vec<ToolCallId> = round.tool_calls.iter().map(|c| c.call_id.clone()).collect();
        for call in &calls {
            state.calls.insert(
                call.clone(),
                CallState {
                    run: run.clone(),
                    session: session.clone(),
                    plan_event: None,
                    plan_hash: None,
                },
            );
        }
        self.append(
            &mut state,
            &session,
            Some(run.clone()),
            EventPayload::MessageAssistant(MessageAssistant {
                round: round.round,
                text: round.text,
                text_ref: round.text_ref,
                tool_calls: round.tool_calls,
                provider_blocks: round.provider_blocks,
                input_tokens: round.usage.input,
                output_tokens: round.usage.output,
            }),
        );
        Ok(calls)
    }

    async fn plan_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
    ) -> Result<EventId, LedgerError> {
        let mut state = self.state.lock().expect("账本");
        let entry = state
            .calls
            .get(call)
            .cloned()
            .ok_or_else(|| LedgerError::NotFound {
                what: format!("call {call}"),
            })?;
        let (event, _) = self.append(
            &mut state,
            &entry.session,
            Some(entry.run.clone()),
            EventPayload::ToolPlanned(ToolPlanned {
                call_id: call.clone(),
                plan_hash: plan.plan_hash(),
                plan: Some(Box::new(plan.clone())),
                plan_ref: None,
            }),
        );
        if let Some(slot) = state.calls.get_mut(call) {
            slot.plan_event = Some(event.clone());
            slot.plan_hash = Some(plan.plan_hash());
        }
        Ok(event)
    }

    async fn start_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
        grant: Option<GrantUse>,
    ) -> Result<AttemptId, LedgerError> {
        let mut state = self.state.lock().expect("账本");
        let entry = state
            .calls
            .get(call)
            .cloned()
            .ok_or_else(|| LedgerError::NotFound {
                what: format!("call {call}"),
            })?;
        let plan_event = entry
            .plan_event
            .clone()
            .ok_or_else(|| LedgerError::Conflict("必须先 plan_call 才能 start_call".into()))?;
        let attempt = AttemptId::new_at(self.clock.now());
        self.append(
            &mut state,
            &entry.session,
            Some(entry.run.clone()),
            EventPayload::ToolStarted(ToolStarted {
                call_id: call.clone(),
                attempt_id: attempt.clone(),
                plan_ref: plan_event,
                plan_hash: plan.plan_hash(),
                grant: grant.and_then(|g| g.grant),
            }),
        );
        state.attempts.insert(attempt.clone(), call.clone());
        Ok(attempt)
    }

    async fn finish_call(
        &self,
        attempt: &AttemptId,
        published: PublishedOutput,
    ) -> Result<(), LedgerError> {
        let mut state = self.state.lock().expect("账本");
        let call = state
            .attempts
            .get(attempt)
            .cloned()
            .ok_or_else(|| LedgerError::NotFound {
                what: format!("attempt {attempt}"),
            })?;
        let entry = state.calls.get(&call).cloned().expect("attempt 必有 call");
        self.append(
            &mut state,
            &entry.session,
            Some(entry.run),
            EventPayload::ToolResult(ToolResult {
                call_id: call,
                attempt_id: attempt.clone(),
                status: published.status,
                output_ref: published.output,
                elapsed_ms: published.elapsed_ms,
                preview: published.preview,
                stdout: published.stdout,
                stderr: published.stderr,
                attempt_state: Some(match published.status {
                    ToolResultStatus::Completed => AttemptState::Completed,
                    ToolResultStatus::Failed => AttemptState::Failed,
                    ToolResultStatus::Uncertain => AttemptState::Started,
                }),
            }),
        );
        Ok(())
    }

    async fn suspend(&self, run: &RunId, wait: Wait) -> Result<(), LedgerError> {
        let mut state = self.state.lock().expect("账本");
        let session = state
            .sessions
            .get(run)
            .cloned()
            .ok_or_else(|| LedgerError::NotFound {
                what: format!("run {run}"),
            })?;
        let payload = match wait {
            Wait::Approval { approval, .. } => {
                EventPayload::RunWaitingApproval(RunWaitingApproval {
                    approval,
                    call: None,
                })
            }
            Wait::Retry {
                attempts,
                next_retry_at,
                reason,
            } => EventPayload::RunWaitingRetry(RunWaitingRetry {
                attempts,
                next_retry_at,
                reason,
            }),
            Wait::Attention { reason } => {
                EventPayload::RunNeedsAttention(RunNeedsAttention { reason, call: None })
            }
        };
        self.append(&mut state, &session, Some(run.clone()), payload);
        Ok(())
    }

    async fn complete(&self, run: &RunId, end: RunEnd) -> Result<(), LedgerError> {
        let mut state = self.state.lock().expect("账本");
        let session = state
            .sessions
            .get(run)
            .cloned()
            .ok_or_else(|| LedgerError::NotFound {
                what: format!("run {run}"),
            })?;
        let payload = match end {
            RunEnd::Completed { final_message } => EventPayload::RunCompleted(RunCompleted {
                final_message,
                final_message_ref: None,
                rounds: 0,
            }),
            RunEnd::Failed { reason } => EventPayload::RunFailed(RunFailed { reason }),
            RunEnd::Cancelled { by } => EventPayload::RunCancelled(RunCancelled {
                by: by.map(crate::types::chat::PeerId::new),
            }),
        };
        self.append(&mut state, &session, Some(run.clone()), payload);
        Ok(())
    }

    async fn read(
        &self,
        session: &SessionId,
        from: Seq,
        limit: u32,
    ) -> Result<EventBatch, LedgerError> {
        /// `limit == 0` 交给实现决定；替身取一个小到能在测试里翻好几页的数。
        const DEFAULT_PAGE: u32 = 64;

        let state = self.state.lock().expect("账本");
        let matching: Vec<Event> = state
            .events
            .iter()
            .filter(|e| &e.session == session && e.seq > from)
            .cloned()
            .collect();
        let page = if limit == 0 { DEFAULT_PAGE } else { limit } as usize;
        let more = matching.len() > page;
        let events: Vec<Event> = matching.into_iter().take(page).collect();
        let next = more.then(|| events.last().map(|e| e.seq)).flatten();
        Ok(EventBatch {
            session: session.clone(),
            events,
            next,
        })
    }

    async fn boundary(&self, session: &SessionId) -> Result<Seq, LedgerError> {
        let mut state = self.state.lock().expect("账本");
        let (_, seq) = self.append(
            &mut state,
            session,
            None,
            EventPayload::ConversationBoundary(ConversationBoundary { by: None }),
        );
        Ok(seq)
    }

    async fn append_audit(
        &self,
        session: &SessionId,
        event_id: &EventId,
        payload: EventPayload,
        occurred_at: OffsetDateTime,
    ) -> Result<Seq, LedgerError> {
        let mut state = self.state.lock().expect("账本");
        // 按 event_id 幂等：已写入就复用原事件位置（§8.5）。
        if let Some(existing) = state.events.iter().find(|e| &e.event_id == event_id) {
            return Ok(existing.seq);
        }
        state.next_seq += 1;
        let seq = Seq(state.next_seq);
        state.events.push(Event {
            v: EVENT_FORMAT_VERSION,
            seq,
            event_id: event_id.clone(),
            session: session.clone(),
            run: None,
            // 补写保留**原始发生时间**，不是补写的时间。
            ts: occurred_at,
            payload,
        });
        Ok(seq)
    }
}
