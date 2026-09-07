//! The run ledger, read out of the session event log.
//!
//! [`Run`] and [`RunStep`] are rows in a disposable database, and every fact
//! they hold is already a durable event: a turn opens with `turn/started`, its
//! calls bracket as `tool/call-started` / `tool/call-settled`, and it closes
//! with one of the three terminal events. Keeping the rows as a *second*
//! authoritative write meant two records of the same turn that could disagree —
//! and after a crash they routinely did, because the row's update is not part of
//! the append that made the event durable.
//!
//! So the rows become a query index over this fold. Nothing here reads the
//! database, and the fold is total: an event log alone reproduces the ledger.
//!
//! Two fields are **not** produced here, and each for its own reason:
//!
//! - [`Run::outcome`] is revisable by what the user says in a *later* turn, so
//!   it is row-held state merged over the projection, never derived from it.
//! - [`Run::recoverable`] folds as *interrupted and unclaimed*, which is not
//!   the same as the reconciled row: a turn with no terminal event is either
//!   running right now or died, and only the process can tell those apart.
//!   Translating one into `Failed` with an interrupted error is the
//!   reconciler's job at startup, not a fact the log states.

use async_trait::async_trait;

use super::cancel::CANCELLED_ERROR;
use super::run::{RUN_FIELD_CAP, Run, RunStatus, RunStep, truncate};
use super::session_event::{MessageSource, SessionEvent, SessionEventKind, ToolOutcome};

/// Where a folded ledger lands: the query tables `komo run` reads.
///
/// The write half of this module. A commit is **idempotent** — it is replayed
/// after every turn and again by a full rebuild, over rows that may already
/// hold most of what it is committing.
#[async_trait]
pub trait RunProjectionStore: Send + Sync {
    /// Commit one session's folded runs, which the log holds `through` this seq.
    ///
    /// `through` is the projection's watermark, and a commit that would not
    /// advance it is skipped: a session gains events constantly and the fold is
    /// over the whole log, so re-committing an unchanged one is pure cost.
    ///
    /// Row-held fields are **merged, never overwritten**: [`Run::outcome`] is
    /// revised by a later turn and the log does not carry it, and
    /// [`Run::learned`] only ever advances — a rebuild must not un-retire a
    /// turn whose watermark event predates the log it can still read.
    async fn commit(
        &self,
        session_id: &str,
        runs: &[ProjectedRun],
        through: u64,
    ) -> anyhow::Result<()>;
}

/// One turn, as the log records it.
#[derive(Debug, Clone)]
pub struct ProjectedRun {
    pub run: Run,
    pub steps: Vec<ProjectedStep>,
    /// Where this turn begins in the log — the seq of its `turn/started`.
    ///
    /// Retention deletes whole segments, so the question it has to answer is
    /// "from which seq on must the log survive intact", and a run's id says
    /// nothing about that.
    pub start_seq: u64,
}

/// One call, and whether the log ever saw it finish.
///
/// The ledger cannot express this: a step row is written *at settle*, so a call
/// the process died in the middle of leaves no row at all — the record says the
/// turn never made the call. The log brackets every call, so the projection can
/// tell "never dispatched" from "dispatched and we lost the answer", which is
/// the question recovery has to answer and the reason the two halves are
/// separate events.
#[derive(Debug, Clone)]
pub struct ProjectedStep {
    pub step: RunStep,
    pub settled: bool,
}

/// The seq a turn's log has to survive from: its own start, and the start of
/// every earlier attempt at it.
///
/// A continuation is rebuilt from its whole `resumed_from` chain, so cutting an
/// ancestor's rounds away would leave a turn that still reads as resumable but
/// silently re-runs the work those rounds already paid for. Retention asks this
/// rather than `start_seq` for exactly that reason.
///
/// Bounded by the number of runs, so a `resumed_from` cycle cannot loop.
pub fn replay_floor(runs: &[ProjectedRun], run: &ProjectedRun) -> u64 {
    let mut floor = run.start_seq;
    let mut current = run.run.resumed_from.clone();
    for _ in 0..runs.len() {
        let Some(id) = current else { break };
        let Some(parent) = runs.iter().find(|other| other.run.id == id) else {
            break;
        };
        floor = floor.min(parent.start_seq);
        current = parent.run.resumed_from.clone();
    }
    floor
}

/// The answer an **earlier attempt** at this turn already got for this call.
///
/// A call that stopped for an approval is re-dispatched by the continuation,
/// and the answer was written against the turn that *asked* — so the call whose
/// entire story is its approval would otherwise be the one call the ledger
/// cannot say who allowed, or how long the person took (docs/bot-runtime.md §8,
/// criterion 2: `waited_ms ≈ 5h` on the step that finally ran).
///
/// Bounded by the number of runs, so a `resumed_from` cycle cannot loop.
fn inherited_approval(
    runs: &[ProjectedRun],
    at: usize,
    call_id: &str,
    approvals: &[(String, String, String, i64)],
) -> Option<(String, i64)> {
    let mut current = runs[at].run.resumed_from.clone();
    for _ in 0..runs.len() {
        let id = current?;
        if let Some((.., decided_by, waited)) = approvals
            .iter()
            .rev()
            .find(|(turn, call, ..)| *turn == id && call == call_id)
        {
            return Some((decided_by.clone(), *waited));
        }
        current = runs
            .iter()
            .find(|other| other.run.id == id)?
            .run
            .resumed_from
            .clone();
    }
    None
}

/// Fold one session's events into the runs they record, oldest first.
///
/// Events for a turn that never opened with `turn/started` are ignored rather
/// than synthesizing a run: the log is contiguous and checked on read, so their
/// absence is not a gap to paper over.
pub fn project_runs(session_id: &str, events: &[SessionEvent]) -> Vec<ProjectedRun> {
    let mut runs: Vec<ProjectedRun> = Vec::new();
    // A call's own `tool/call-started` and `tool/call-settled` are separated by
    // however long the tool took, and the settle lands in completion order, so
    // the two halves are matched by id rather than by adjacency.
    let mut open: Vec<(String, String, usize, usize)> = Vec::new();
    // The turn's reply, staged until the turn *completes*. A cancelled or failed
    // turn also leaves an `assistant/message` — a placeholder that keeps the
    // transcript alternating — but that is what the conversation shows, not an
    // answer the turn produced, and the ledger has always left `final_output`
    // empty for both.
    let mut replies: Vec<String> = Vec::new();
    // Every answer to an approval, as `(turn, call, rung, waited_ms)`. Kept
    // beyond the turn it was recorded against because a call that stopped to
    // wait is re-dispatched by a *later* attempt (see [`inherited_approval`]).
    let mut approvals: Vec<(String, String, String, i64)> = Vec::new();

    for event in events {
        // The learning watermark. Decided by the sweep after the turn is over,
        // so it arrives past the run's terminal event and is not part of its
        // work — but it is the same fact the row's `learned` flag held, and
        // *skipped* has to advance it too, or a turn the sweep considered and
        // declined is offered again forever.
        if let SessionEventKind::LearningCompleted { turn_id }
        | SessionEventKind::LearningSkipped { turn_id, .. } = &event.kind
        {
            if let Some(projected) = runs.iter_mut().find(|p| p.run.id == *turn_id) {
                projected.run.learned = true;
            }
            continue;
        }

        let Some(turn_id) = event.turn_id_of_work() else {
            continue;
        };
        let at = event.at.unix_timestamp();

        if let SessionEventKind::TurnStarted {
            turn_id,
            resumed_from,
        } = &event.kind
        {
            let mut run = Run::start(session_id, "");
            run.id = turn_id.clone();
            run.started_at = at;
            run.resumed_from = resumed_from.clone();
            // Every turn opens interrupted and is cleared by its own terminal
            // event. A run left this way is the residue of a process that died
            // mid-turn — which is exactly what recovery is looking for.
            run.recoverable = true;
            runs.push(ProjectedRun {
                run,
                steps: Vec::new(),
                start_seq: event.seq,
            });
            replies.push(String::new());
            continue;
        }

        let Some(at_index) = runs.iter().position(|p| p.run.id == turn_id) else {
            continue;
        };
        // An answer an earlier attempt at this turn already collected — read
        // before the mutable borrow below, and only for the one event that can
        // use it.
        let inherited = match &event.kind {
            SessionEventKind::ToolCallStarted(call) => {
                inherited_approval(&runs, at_index, &call.call_id, &approvals)
            }
            _ => None,
        };
        let projected = &mut runs[at_index];

        match &event.kind {
            SessionEventKind::UserMessage(message) if message.source == MessageSource::User => {
                projected.run.input = truncate(&message.content, RUN_FIELD_CAP);
            }
            SessionEventKind::TurnMemories { memories, .. } => {
                projected.run.memories = memories.clone();
            }
            SessionEventKind::AssistantRound(round) => {
                projected.run.tokens_in += round.tokens_in;
                projected.run.tokens_out += round.tokens_out;
                projected.run.tokens_cached += round.tokens_cached;
            }
            SessionEventKind::AssistantMessage(message) => {
                replies[at_index] = truncate(&message.content, RUN_FIELD_CAP);
            }
            SessionEventKind::ToolCallStarted(call) => {
                let seq = projected.steps.len() as i64;
                projected.steps.push(ProjectedStep {
                    settled: false,
                    step: RunStep {
                        run_id: turn_id.to_string(),
                        seq,
                        tool_name: call.tool.clone(),
                        args: call.args.clone(),
                        result: String::new(),
                        error: String::new(),
                        ok: false,
                        uncertain: false,
                        started_at: at,
                        ended_at: at,
                        elapsed_ms: 0,
                        structured: serde_json::Value::Null,
                        output_paths: Vec::new(),
                        approved_by: inherited
                            .as_ref()
                            .map(|(by, _)| by.clone())
                            .unwrap_or_default(),
                        approval_waited_ms: inherited.map(|(_, waited)| waited).unwrap_or_default(),
                    },
                });
                open.push((
                    turn_id.to_string(),
                    call.call_id.clone(),
                    at_index,
                    projected.steps.len() - 1,
                ));
            }
            // The audit half of an approval-gated call: which rung let it
            // happen and how long the answer took. Matched by `call_id` like a
            // settle — the approval resolves while the call is still open, so
            // adjacency says nothing.
            SessionEventKind::ApprovalResolved(approval) => {
                let found = open
                    .iter()
                    .find(|(run_id, call_id, ..)| run_id == turn_id && *call_id == approval.call_id)
                    .map(|(.., step_index)| *step_index);
                if let Some(step_index) = found
                    && let Some(projected_step) = projected.steps.get_mut(step_index)
                {
                    projected_step.step.approved_by = approval.decided_by.clone();
                    projected_step.step.approval_waited_ms = approval.waited_ms;
                }
                approvals.push((
                    approval.turn_id.clone(),
                    approval.call_id.clone(),
                    approval.decided_by.clone(),
                    approval.waited_ms,
                ));
            }
            SessionEventKind::ToolCallSettled(call) => {
                let found = open.iter().position(|(run_id, call_id, ..)| {
                    run_id == turn_id && *call_id == call.call_id
                });
                let Some(found) = found else {
                    continue;
                };
                let (.., step_index) = open.remove(found);
                projected.steps[step_index].settled = true;
                let step = &mut projected.steps[step_index].step;
                step.result = call.result.clone();
                step.error = call.error.clone();
                step.ok = call.outcome == ToolOutcome::Succeeded;
                step.uncertain = call.outcome == ToolOutcome::Uncertain;
                step.ended_at = at;
                step.elapsed_ms = call.elapsed_ms;
                step.structured = call.structured.clone();
                step.output_paths = call.output_paths.clone();
            }
            // Waiting, not working and not dead. `recoverable` is what
            // `komo run resume` offers and what the startup reconciler rules
            // on; neither applies to a turn whose return is already scheduled —
            // resuming it by hand would run it twice, and reconciling it would
            // report a crash that did not happen.
            SessionEventKind::TurnSuspended(_) => {
                projected.run.status = RunStatus::Suspended;
                projected.run.recoverable = false;
            }
            // The wait is over: whatever comes next, this turn is no longer
            // parked. A continuation picks it up under its own `turn/started`,
            // so the status only has to stop claiming it is still waiting.
            SessionEventKind::WakeupFired(_) => {
                if projected.run.status == RunStatus::Suspended {
                    projected.run.status = RunStatus::Running;
                    projected.run.recoverable = true;
                }
            }
            SessionEventKind::TurnCompleted { .. } => {
                projected.run.final_output = std::mem::take(&mut replies[at_index]);
                projected.run.status = RunStatus::Done;
                projected.run.ended_at = Some(at);
                projected.run.recoverable = false;
            }
            SessionEventKind::TurnFailed { error, .. } => {
                projected.run.status = RunStatus::Failed;
                projected.run.error = truncate(error, RUN_FIELD_CAP);
                projected.run.ended_at = Some(at);
                projected.run.recoverable = false;
            }
            SessionEventKind::TurnCancelled { .. } => {
                // Cancelled, not broken — and deliberately not recoverable:
                // there is nothing to resume, the user asked it to stop.
                projected.run.status = RunStatus::Failed;
                projected.run.error = CANCELLED_ERROR.to_string();
                projected.run.ended_at = Some(at);
                projected.run.recoverable = false;
            }
            _ => {}
        }
    }

    // A continuation's own `turn/started` **is** the claim on the turn it picked
    // up: seq assignment is what serializes two would-be resumers, so the log
    // decides which of them owns the recovery rather than a row update racing
    // another reader. Once claimed, a turn is not offered again — resuming a
    // turn twice re-runs work the first continuation already did.
    let claimed: Vec<String> = runs
        .iter()
        .filter_map(|projected| projected.run.resumed_from.clone())
        .collect();

    // A continuation appends no user message of its own — the question is the
    // one the interrupted turn was already answering — so it inherits it. The
    // row is what an operator reads in `run list`, and a turn there with no
    // input reads as a turn about nothing.
    let inherited: Vec<(String, String)> = runs
        .iter()
        .filter(|projected| projected.run.input.is_empty())
        .filter_map(|projected| {
            let from = projected.run.resumed_from.as_deref()?;
            let parent = runs.iter().find(|other| other.run.id == from)?;
            Some((projected.run.id.clone(), parent.run.input.clone()))
        })
        .collect();

    for projected in &mut runs {
        if claimed.contains(&projected.run.id) {
            projected.run.recoverable = false;
        }
        if let Some((_, input)) = inherited.iter().find(|(id, _)| *id == projected.run.id) {
            projected.run.input = input.clone();
        }
        // The LLM owns tool dispatch, so the plan is a description of what the
        // turn turned out to do, not a decision made before it ran.
        projected.run.plan = match projected.steps.len() {
            0 => "respond".to_string(),
            n => format!("{n} tool call(s)"),
        };
    }
    runs
}

#[cfg(test)]
mod tests;
