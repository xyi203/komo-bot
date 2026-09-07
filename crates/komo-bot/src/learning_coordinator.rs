//! Learning orchestration (docs/episode-learning-framework.md §5.1): one owner
//! for *when* komo learns from what it did, which episodes the extractor sees,
//! and how the watermark and concurrency behave.
//!
//! The unit is an **episode** — one finished [`Run`] and the tool steps it
//! produced — not a session transcript. A transcript holds user and assistant
//! text only: tool results are never persisted as messages, so an extractor
//! reading one cannot tell whether a command ran, what it returned, or whether
//! the turn ended by delivering or by failing. It learns from the agent's
//! account of itself, which is the one source that cannot corroborate it.
//!
//! Two triggers share this one instance (wiring creates it once): the runtime
//! reports each finished run, and the maintenance sweep picks up whatever the
//! interval left behind. The shared instance is what makes the per-session
//! in-flight guard effective across both.
//!
//! **Ordering matters here.** Learning is dispatched *after* `runs.finish`, not
//! from inside the turn: an episode assembled while its run is still open sees a
//! status that has not been decided and steps that are still arriving.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tracing::warn;

use komo_core::domain::{
    context::SessionOrigin,
    episode::{AssessedEpisode, OutcomeAssessment},
    llm::LlmClient,
    repository::{SessionEventRepository, SessionRepository},
    reviewer::{ReviewOutcome, Reviewer},
    run::{Run, RunRepository},
    session::Session,
    session_event::SessionEventKind,
};
use komo_services::episode::assemble;

/// Most episodes one learning pass will read. A session that accumulated a
/// backlog (the sweep was off, or the gateway was down for a week) is learned in
/// batches this size rather than in one prompt that would be elided anyway.
const LEARN_BATCH_CAP: usize = 50;

/// How many unlearned runs the sweep pulls per cycle before grouping them by
/// session. Larger than the batch cap so one busy session cannot starve the
/// others of a turn.
const SWEEP_SCAN_CAP: usize = 200;

/// Why this session's turns are not lessons, or `None` if they are.
///
/// Two kinds are exempt, for one reason: both would hand the memory
/// consolidator the same occasion twice, and a second occasion is what it reads
/// as corroboration. An unattended **sweep** restates facts the agent already
/// knows on a timer; a **delegation** is the parent turn's own work, already
/// being learned from where it was asked for.
///
/// Read off the session record's `origin`, which the turn stamped when it
/// opened the session. It used to be a prefix test on the id — a second
/// representation of the same fact that could disagree with the first, and one
/// a session could acquire by being named unluckily.
///
/// A session that cannot be read is **not** exempt: silently skipping learning
/// is the failure nobody would notice, while learning from a sweep is one the
/// dream sweep's evidence counts would eventually show.
async fn learning_exemption(
    sessions: &Arc<dyn SessionRepository>,
    session_id: &str,
) -> Option<&'static str> {
    match sessions.find_windowed(session_id, 1).await {
        Ok(Some(session)) if !session.origin.is_learnable() => Some(match session.origin {
            SessionOrigin::Delegate => DELEGATED_TURN,
            _ => SWEEP_SESSION,
        }),
        Ok(_) => None,
        Err(error) => {
            warn!(%error, session = %session_id, "could not read a session's origin; not exempting");
            None
        }
    }
}

/// Why a learning pass is being requested.
pub enum LearningTrigger {
    /// A run just finished. Learn if its session has accumulated a full
    /// interval of unlearned turns, else leave them for the sweep.
    AfterRun { run_id: String },
    /// The maintenance sweep: learn from every session holding unlearned turns,
    /// whatever the interval.
    Scheduled,
}

/// What one coordinator run accomplished, aggregated across sessions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LearningReport {
    pub sessions_learned: usize,
    pub episodes_learned: usize,
    pub memories_written: usize,
}

impl LearningReport {
    pub fn is_empty(&self) -> bool {
        self.sessions_learned == 0 && self.episodes_learned == 0 && self.memories_written == 0
    }

    fn absorb(&mut self, outcome: &ReviewOutcome, episodes: usize) {
        self.sessions_learned += 1;
        self.episodes_learned += episodes;
        self.memories_written += outcome.memories_written.len();
    }
}

/// The one learning orchestrator. Both trigger paths must share a single
/// instance — that is what makes the in-flight guard effective when a post-run
/// pass and a sweep reach the same session.
pub struct LearningCoordinator {
    sessions: Arc<dyn SessionRepository>,
    runs: Arc<dyn RunRepository>,
    /// Where the watermark actually lives: one `learning/completed` or
    /// `learning/skipped` per turn this pass finished with.
    events: Arc<dyn SessionEventRepository>,
    reviewer: Arc<dyn Reviewer>,
    /// Learn once a session has this many finished turns waiting.
    interval: usize,
    /// Aux model for reading a turn's reply as a verdict on the previous one.
    /// `None` = outcomes stay deterministic, which means they stay `Unknown`.
    aux: Option<Arc<dyn LlmClient>>,
    /// Session ids currently being learned from (either trigger).
    in_flight: Mutex<HashSet<String>>,
}

impl LearningCoordinator {
    pub fn new(
        sessions: Arc<dyn SessionRepository>,
        runs: Arc<dyn RunRepository>,
        events: Arc<dyn SessionEventRepository>,
        reviewer: Arc<dyn Reviewer>,
        interval: usize,
    ) -> Self {
        Self {
            sessions,
            runs,
            events,
            reviewer,
            interval: interval.max(1),
            aux: None,
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Attach the aux model that reads the user's next message as a verdict on
    /// the previous turn. Without it every outcome stays `Unknown`: nothing
    /// observable when a turn ends distinguishes success from silence.
    pub fn with_feedback(mut self, aux: Arc<dyn LlmClient>) -> Self {
        self.aux = Some(aux);
        self
    }

    /// Run one learning pass for `trigger`. Callers pass no counts or
    /// watermarks — eligibility is this module's knowledge.
    pub async fn run(&self, trigger: LearningTrigger) -> anyhow::Result<LearningReport> {
        let mut report = LearningReport::default();
        match trigger {
            LearningTrigger::AfterRun { run_id } => {
                let Some(run) = self.runs.get(&run_id).await? else {
                    return Ok(report);
                };
                // Two things happen before any learning: this run gets its
                // provisional assessment, and the *previous* run may get a
                // verdict out of what the user just said. The second is the
                // whole reason assessments are stored rather than recomputed.
                self.assess(&run).await;
                self.absorb_feedback(&run).await;
                if let Some(reason) = learning_exemption(&self.sessions, &run.session_id).await {
                    // Retire it from the backlog rather than leaving it to be
                    // re-examined and re-declined by every future sweep.
                    self.retire(&run.session_id, &[Retired::Skipped(run.id.clone(), reason)])
                        .await;
                    return Ok(report);
                }
                let pending = self
                    .runs
                    .unlearned(Some(&run.session_id), LEARN_BATCH_CAP)
                    .await?;
                if pending.len() < self.interval {
                    return Ok(report);
                }
                self.learn_session(&run.session_id, pending, &mut report)
                    .await?;
            }
            LearningTrigger::Scheduled => {
                let pending = self.runs.unlearned(None, SWEEP_SCAN_CAP).await?;
                for (session_id, runs) in group_by_session(pending) {
                    if let Some(reason) = learning_exemption(&self.sessions, &session_id).await {
                        self.retire(&session_id, &skipped(&runs, reason)).await;
                        continue;
                    }
                    // Isolate per-session failures: one bad pass must not abort
                    // the whole sweep.
                    if let Err(error) = self.learn_session(&session_id, runs, &mut report).await {
                        warn!(%error, session = %session_id, "session learning failed (skipped)");
                    }
                }
            }
        }
        Ok(report)
    }

    /// Write the deterministic reading of a just-finished run.
    ///
    /// Best-effort: an unassessed run reads as `Unknown` at learning time,
    /// which is what it would have said anyway.
    async fn assess(&self, run: &Run) {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let Ok(Some(view)) = assemble(&self.runs, &run.id).await else {
            return;
        };
        let assessment = OutcomeAssessment::deterministic(&view, now);
        self.store_outcome(&run.id, &assessment).await;
    }

    /// Read this run's user message as a verdict on the one before it, and
    /// revise that one's assessment if it is.
    ///
    /// Only the immediately preceding turn, and only within the same session:
    /// "还是不行" is about what just happened. Reaching further back would
    /// attach a confident verdict to a turn the user was not talking about,
    /// which is worse than missing one.
    async fn absorb_feedback(&self, run: &Run) {
        let Some(aux) = &self.aux else {
            return;
        };
        if learning_exemption(&self.sessions, &run.session_id)
            .await
            .is_some()
        {
            return;
        }
        let Ok(Some(previous)) = self.runs.previous_in_session(&run.id).await else {
            return;
        };
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let Some(evidence) = crate::feedback::classify(aux, &previous, &run.input, now).await
        else {
            return;
        };

        // Append and re-resolve, never replace: the deterministic evidence
        // still holds — an uncertain step is still uncertain — and the
        // strength ordering is what decides between them.
        //
        // Recomputed when the previous run has no stored assessment, rather
        // than starting from an empty one. A turn whose own trigger never fired
        // (a crash, a restart, a runtime that was not yet learning) would
        // otherwise lose its uncertain steps and failures the moment feedback
        // arrived — and losing evidence is not what appending evidence should do.
        let mut assessment = match serde_json::from_str::<OutcomeAssessment>(&previous.outcome) {
            Ok(stored) => stored,
            Err(_) => match assemble(&self.runs, &previous.id).await {
                Ok(Some(view)) => OutcomeAssessment::deterministic(&view, now),
                _ => OutcomeAssessment::resolve(previous.id.clone(), Vec::new(), now),
            },
        };
        assessment.evidence.push(evidence);
        let revised = OutcomeAssessment::resolve(previous.id.clone(), assessment.evidence, now);
        tracing::info!(
            run_id = %previous.id,
            verdict = revised.verdict.as_str(),
            "outcome revised by the user's next message"
        );
        self.store_outcome(&previous.id, &revised).await;
    }

    async fn store_outcome(&self, run_id: &str, assessment: &OutcomeAssessment) {
        let Ok(json) = serde_json::to_string(assessment) else {
            return;
        };
        if let Err(error) = self.runs.set_outcome(run_id, &json).await {
            warn!(%error, run_id, "failed to store an outcome assessment");
        }
    }

    /// Learn from one session's pending runs, then retire exactly the batch that
    /// was considered — including the runs deliberately skipped, because
    /// "considered and declined" and "not yet considered" have to be different
    /// states or the sweep re-reads them forever.
    ///
    /// A failed pass retires nothing: the watermark stays where it is and the
    /// next sweep tries again.
    async fn learn_session(
        &self,
        session_id: &str,
        pending: Vec<Run>,
        report: &mut LearningReport,
    ) -> anyhow::Result<()> {
        // At most one pass per session at a time, across both triggers. A second
        // concurrent pass would read the same unretired runs and extract them
        // twice — two independent-looking observations of one occasion, which
        // is exactly what the consolidator counts as corroboration.
        let Some(_guard) = InFlightGuard::claim(&self.in_flight, session_id) else {
            return Ok(());
        };
        let batch: Vec<Run> = pending.into_iter().take(LEARN_BATCH_CAP).collect();
        if batch.is_empty() {
            return Ok(());
        }

        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut retired: Vec<Retired> = Vec::new();
        let mut episodes = Vec::new();
        for run in &batch {
            let Some(view) = assemble(&self.runs, &run.id).await? else {
                retired.push(Retired::Skipped(run.id.clone(), NO_EPISODE));
                continue;
            };
            if !view.learning_eligible() {
                retired.push(Retired::Skipped(run.id.clone(), CANCELLED));
                continue;
            }
            retired.push(Retired::Learned(run.id.clone()));
            // The stored assessment, when there is one, may carry the user's
            // own verdict — the strongest evidence there is, and the only kind
            // that arrives after the turn it judges.
            episodes.push(AssessedEpisode::stored_or_deterministic(
                view,
                &run.outcome,
                now,
            ));
        }
        if episodes.is_empty() {
            // Nothing to extract from, but the batch was still examined.
            self.retire(session_id, &retired).await;
            return Ok(());
        }

        // Identity and workspace for the aux call. Windowed to 1 because the
        // extractor reads episodes, not the transcript — loading a long
        // conversation to use two of its metadata fields is what the episode
        // path exists to stop doing.
        let Some(session) = self.sessions.find_windowed(session_id, 1).await? else {
            return Ok(());
        };
        let session = Session {
            messages: Vec::new(),
            ..session
        };

        let count = episodes.len();
        let outcome = self.reviewer.review(&session, &episodes).await?;
        report.absorb(&outcome, count);
        self.retire(session_id, &retired).await;
        Ok(())
    }

    /// Advance the watermark past `retired` — the log first, then the row.
    ///
    /// The event is the watermark and the row is an index over it, so the order
    /// is not a preference: a row that read `learned` over a log that never said
    /// so would come back unlearned the moment the ledger is rebuilt from
    /// events, and every sweep would re-extract the turn.
    ///
    /// Best-effort in both halves: a failure only means those runs are offered
    /// again, and the extractor's dedup guards make a re-read harmless rather
    /// than wrong.
    async fn retire(&self, session_id: &str, retired: &[Retired]) {
        if retired.is_empty() {
            return;
        }
        let kinds = retired
            .iter()
            .map(|entry| match entry {
                Retired::Learned(turn_id) => SessionEventKind::LearningCompleted {
                    turn_id: turn_id.clone(),
                },
                Retired::Skipped(turn_id, reason) => SessionEventKind::LearningSkipped {
                    turn_id: turn_id.clone(),
                    reason: (*reason).to_string(),
                },
            })
            .collect();
        if let Err(error) = self.events.append(session_id, kinds).await {
            warn!(%error, session = %session_id, "failed to record the learning watermark; the batch stays in the backlog");
            return;
        }
        if let Err(error) = self.events.durable_flush(session_id).await {
            warn!(%error, session = %session_id, "the learning watermark is not durable yet (non-fatal)");
        }
        let ids: Vec<String> = retired.iter().map(|e| e.turn_id().to_string()).collect();
        if let Err(error) = self.runs.mark_learned(&ids).await {
            warn!(%error, "failed to advance the learning watermark");
        }
    }
}

/// Why a turn left the learning backlog. Both halves advance the watermark:
/// "considered and declined" and "not yet considered" have to be different
/// states, or every sweep re-examines the same turn forever.
enum Retired {
    Learned(String),
    Skipped(String, &'static str),
}

impl Retired {
    fn turn_id(&self) -> &str {
        match self {
            Retired::Learned(id) | Retired::Skipped(id, _) => id,
        }
    }
}

/// The reasons a turn is retired without being extracted from. Short and
/// stable: they land in a durable log, where they are the only account of why
/// a turn was never learned from.
const SWEEP_SESSION: &str = "sweep session";
const DELEGATED_TURN: &str = "delegated turn";
const CANCELLED: &str = "cancelled turn";
const NO_EPISODE: &str = "episode unavailable";

fn skipped(runs: &[Run], reason: &'static str) -> Vec<Retired> {
    runs.iter()
        .map(|r| Retired::Skipped(r.id.clone(), reason))
        .collect()
}

/// Group runs by session, preserving the oldest-first order within each session
/// and the order in which sessions first appear.
fn group_by_session(runs: Vec<Run>) -> Vec<(String, Vec<Run>)> {
    let mut grouped: Vec<(String, Vec<Run>)> = Vec::new();
    for run in runs {
        match grouped.iter_mut().find(|(id, _)| *id == run.session_id) {
            Some((_, bucket)) => bucket.push(run),
            None => grouped.push((run.session_id.clone(), vec![run])),
        }
    }
    grouped
}

/// RAII claim on a session id in the in-flight set: released on drop, so a
/// panicking or failing pass never wedges the session.
struct InFlightGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    id: String,
}

impl<'a> InFlightGuard<'a> {
    fn claim(set: &'a Mutex<HashSet<String>>, id: &str) -> Option<Self> {
        set.lock()
            .unwrap()
            .insert(id.to_string())
            .then(|| InFlightGuard {
                set,
                id: id.to_string(),
            })
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.id);
    }
}

#[cfg(test)]
mod tests;
