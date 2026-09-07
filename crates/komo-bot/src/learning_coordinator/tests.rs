use super::*;
use async_trait::async_trait;
use komo_core::domain::context::SessionOrigin;
use komo_core::domain::{
    cancel::CANCELLED_ERROR,
    message::Message,
    run::{RunStatus, RunStep},
};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A ledger of hand-built runs, recording which ids were retired.
struct FakeRuns {
    runs: Mutex<Vec<Run>>,
    steps: Mutex<Vec<RunStep>>,
    marked: Mutex<Vec<String>>,
    fail_mark: bool,
}

impl FakeRuns {
    fn new(runs: Vec<Run>) -> Arc<Self> {
        Arc::new(Self {
            runs: Mutex::new(runs),
            steps: Mutex::new(Vec::new()),
            marked: Mutex::new(Vec::new()),
            fail_mark: false,
        })
    }
}

#[async_trait]
impl RunRepository for FakeRuns {
    async fn get(&self, id: &str) -> anyhow::Result<Option<Run>> {
        Ok(self
            .runs
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .cloned())
    }
    async fn steps(&self, run_id: &str) -> anyhow::Result<Vec<RunStep>> {
        Ok(self
            .steps
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.run_id == run_id)
            .cloned()
            .collect())
    }
    async fn unlearned(&self, session_id: Option<&str>, limit: usize) -> anyhow::Result<Vec<Run>> {
        Ok(self
            .runs
            .lock()
            .unwrap()
            .iter()
            .filter(|r| !r.learned && r.status.is_terminal())
            .filter(|r| session_id.is_none_or(|s| r.session_id == s))
            .take(limit)
            .cloned()
            .collect())
    }
    async fn set_outcome(&self, run_id: &str, outcome: &str) -> anyhow::Result<()> {
        let mut runs = self.runs.lock().unwrap();
        if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
            run.outcome = outcome.to_string();
        }
        Ok(())
    }
    async fn previous_in_session(&self, run_id: &str) -> anyhow::Result<Option<Run>> {
        let runs = self.runs.lock().unwrap();
        let Some(current) = runs.iter().find(|r| r.id == run_id) else {
            return Ok(None);
        };
        Ok(runs
            .iter()
            .filter(|r| r.session_id == current.session_id && r.started_at < current.started_at)
            .max_by_key(|r| r.started_at)
            .cloned())
    }
    async fn mark_learned(&self, run_ids: &[String]) -> anyhow::Result<()> {
        if self.fail_mark {
            anyhow::bail!("ledger offline");
        }
        let mut runs = self.runs.lock().unwrap();
        for id in run_ids {
            if let Some(run) = runs.iter_mut().find(|r| r.id == *id) {
                run.learned = true;
            }
        }
        self.marked.lock().unwrap().extend(run_ids.iter().cloned());
        Ok(())
    }
    async fn list(&self, _limit: usize) -> anyhow::Result<Vec<Run>> {
        Ok(Vec::new())
    }
    async fn prune(&self, _cutoff: i64) -> anyhow::Result<usize> {
        Ok(0)
    }
    async fn reconcile_interrupted(&self, _now: i64) -> anyhow::Result<usize> {
        Ok(0)
    }
}

struct FakeSessions {
    loads: AtomicUsize,
    /// Ids the store reports with a non-`User` origin. Named explicitly
    /// rather than derived from the id: what is driving a session is a
    /// stored fact now, and a test that spelled it into the id would still
    /// pass if the code went back to parsing one.
    origins: Vec<(String, SessionOrigin)>,
}

impl FakeSessions {
    fn new() -> Arc<Self> {
        Self::with_origins(Vec::new())
    }

    fn with_sweeps(sweeps: Vec<&str>) -> Arc<Self> {
        Self::with_origins(
            sweeps
                .into_iter()
                .map(|id| (id, SessionOrigin::Cron))
                .collect(),
        )
    }

    fn with_origins(origins: Vec<(&str, SessionOrigin)>) -> Arc<Self> {
        Arc::new(Self {
            loads: AtomicUsize::new(0),
            origins: origins
                .into_iter()
                .map(|(id, origin)| (id.to_string(), origin))
                .collect(),
        })
    }

    fn origin_of(&self, id: &str) -> SessionOrigin {
        self.origins
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, origin)| *origin)
            .unwrap_or(SessionOrigin::User)
    }
}

#[async_trait]
impl SessionRepository for FakeSessions {
    async fn find_by_peer(
        &self,
        _channel: &komo_core::domain::session::ChannelPeer,
    ) -> anyhow::Result<Option<Session>> {
        Ok(None)
    }

    async fn find(&self, id: &str) -> anyhow::Result<Option<Session>> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        Ok(Some(Session::new(id).with_origin(self.origin_of(id))))
    }
    async fn find_windowed(&self, id: &str, _limit: usize) -> anyhow::Result<Option<Session>> {
        let mut s = Session::new(id).with_origin(self.origin_of(id));
        s.messages.push(Message::user("hi"));
        Ok(Some(s))
    }
    async fn list(&self) -> anyhow::Result<Vec<Session>> {
        Ok(Vec::new())
    }
    async fn save(&self, _session: &Session) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_empty_sessions(&self) -> anyhow::Result<usize> {
        Ok(0)
    }
}

/// Records the episodes it was handed, per call.
struct RecordingReviewer {
    calls: Mutex<Vec<(String, Vec<String>)>>,
    verdicts: Mutex<Vec<komo_core::domain::episode::OutcomeVerdict>>,
    fail: bool,
    gate: Option<Arc<tokio::sync::Notify>>,
    outcome: ReviewOutcome,
}

impl Default for RecordingReviewer {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            verdicts: Mutex::new(Vec::new()),
            fail: false,
            gate: None,
            outcome: ReviewOutcome::default(),
        }
    }
}

#[async_trait]
impl Reviewer for RecordingReviewer {
    async fn review(
        &self,
        session: &Session,
        episodes: &[AssessedEpisode],
    ) -> anyhow::Result<ReviewOutcome> {
        self.calls.lock().unwrap().push((
            session.id.clone(),
            episodes.iter().map(|e| e.view.id().to_string()).collect(),
        ));
        self.verdicts
            .lock()
            .unwrap()
            .extend(episodes.iter().map(|e| e.outcome.verdict));
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        if self.fail {
            anyhow::bail!("aux model unavailable");
        }
        Ok(self.outcome.clone())
    }
}

fn run(id: &str, session: &str, status: RunStatus, error: &str) -> Run {
    let mut r = Run::start(session, "do it");
    r.id = id.into();
    r.status = status;
    r.error = error.into();
    r
}

fn done(id: &str, session: &str) -> Run {
    run(id, session, RunStatus::Done, "")
}

/// A session log that keeps only what a learning pass writes to it.
struct FakeEvents {
    appended: Mutex<Vec<SessionEventKind>>,
    flushes: AtomicUsize,
    fail_append: bool,
}

impl FakeEvents {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            appended: Mutex::new(Vec::new()),
            flushes: AtomicUsize::new(0),
            fail_append: false,
        })
    }

    fn offline() -> Arc<Self> {
        Arc::new(Self {
            appended: Mutex::new(Vec::new()),
            flushes: AtomicUsize::new(0),
            fail_append: true,
        })
    }

    /// The watermark as `(turn id, skip reason)` pairs, in write order.
    fn watermarks(&self) -> Vec<(String, Option<String>)> {
        self.appended
            .lock()
            .unwrap()
            .iter()
            .filter_map(|kind| match kind {
                SessionEventKind::LearningCompleted { turn_id } => Some((turn_id.clone(), None)),
                SessionEventKind::LearningSkipped { turn_id, reason } => {
                    Some((turn_id.clone(), Some(reason.clone())))
                }
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl SessionEventRepository for FakeEvents {
    async fn session_ids(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn surface(
        &self,
        _session_id: &str,
    ) -> anyhow::Result<Option<komo_core::domain::session_event::SurfaceProjection>> {
        Ok(None)
    }

    async fn append(
        &self,
        _session_id: &str,
        kinds: Vec<SessionEventKind>,
    ) -> anyhow::Result<Vec<komo_core::domain::session_event::SessionEvent>> {
        if self.fail_append {
            anyhow::bail!("log offline");
        }
        let mut appended = self.appended.lock().unwrap();
        let first = appended.len() as u64;
        let stamped = kinds
            .iter()
            .enumerate()
            .map(|(i, kind)| {
                komo_core::domain::session_event::SessionEvent::now(first + i as u64, kind.clone())
            })
            .collect();
        appended.extend(kinds);
        Ok(stamped)
    }
    async fn durable_flush(&self, _session_id: &str) -> anyhow::Result<()> {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    async fn events(
        &self,
        _session_id: &str,
    ) -> anyhow::Result<Vec<komo_core::domain::session_event::SessionEvent>> {
        Ok(Vec::new())
    }
    async fn events_from(
        &self,
        _session_id: &str,
        _seq: u64,
    ) -> anyhow::Result<Vec<komo_core::domain::session_event::SessionEvent>> {
        Ok(Vec::new())
    }
    async fn turn_boundary(&self, _session_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn retain(&self, _session_id: &str, _keep_from: u64) -> anyhow::Result<Option<u64>> {
        Ok(None)
    }
}

fn coordinator(
    runs: Arc<FakeRuns>,
    reviewer: Arc<RecordingReviewer>,
    interval: usize,
) -> LearningCoordinator {
    LearningCoordinator::new(
        FakeSessions::new(),
        runs,
        FakeEvents::new(),
        reviewer,
        interval,
    )
}

fn coordinator_logging(
    runs: Arc<FakeRuns>,
    reviewer: Arc<RecordingReviewer>,
    interval: usize,
    events: Arc<FakeEvents>,
    sweeps: Vec<&str>,
) -> LearningCoordinator {
    LearningCoordinator::new(
        FakeSessions::with_sweeps(sweeps),
        runs,
        events,
        reviewer,
        interval,
    )
}

fn coordinator_with_sweeps(
    runs: Arc<FakeRuns>,
    reviewer: Arc<RecordingReviewer>,
    interval: usize,
    sweeps: Vec<&str>,
) -> LearningCoordinator {
    LearningCoordinator::new(
        FakeSessions::with_sweeps(sweeps),
        runs,
        FakeEvents::new(),
        reviewer,
        interval,
    )
}

#[tokio::test]
async fn below_the_interval_nothing_is_learned_and_nothing_is_retired() {
    let runs = FakeRuns::new(vec![done("run-1", "cli:s"), done("run-2", "cli:s")]);
    let reviewer = Arc::new(RecordingReviewer::default());
    let c = coordinator(runs.clone(), reviewer.clone(), 10);

    let report = c
        .run(LearningTrigger::AfterRun {
            run_id: "run-2".into(),
        })
        .await
        .unwrap();

    assert!(report.is_empty());
    assert!(reviewer.calls.lock().unwrap().is_empty());
    assert!(
        runs.marked.lock().unwrap().is_empty(),
        "turns still waiting for a full interval must stay in the backlog"
    );
}

#[tokio::test]
async fn a_full_interval_learns_every_pending_episode_oldest_first() {
    let runs = FakeRuns::new(vec![done("run-1", "cli:s"), done("run-2", "cli:s")]);
    let reviewer = Arc::new(RecordingReviewer::default());
    let c = coordinator(runs.clone(), reviewer.clone(), 2);

    let report = c
        .run(LearningTrigger::AfterRun {
            run_id: "run-2".into(),
        })
        .await
        .unwrap();

    assert_eq!(report.sessions_learned, 1);
    assert_eq!(report.episodes_learned, 2);
    let calls = reviewer.calls.lock().unwrap();
    assert_eq!(calls[0].0, "cli:s");
    assert_eq!(calls[0].1, vec!["run-1".to_string(), "run-2".to_string()]);
    assert_eq!(
        *runs.marked.lock().unwrap(),
        vec!["run-1".to_string(), "run-2".to_string()]
    );
}

/// Golden cases 11 and 12: a cancelled turn is audit, not a lesson — but it
/// still has to leave the backlog, or every sweep re-examines it forever.
#[tokio::test]
async fn cancelled_runs_are_retired_without_being_extracted_from() {
    let runs = FakeRuns::new(vec![
        run("run-1", "cli:s", RunStatus::Failed, CANCELLED_ERROR),
        done("run-2", "cli:s"),
    ]);
    let reviewer = Arc::new(RecordingReviewer::default());
    let c = coordinator(runs.clone(), reviewer.clone(), 2);

    c.run(LearningTrigger::AfterRun {
        run_id: "run-2".into(),
    })
    .await
    .unwrap();

    let calls = reviewer.calls.lock().unwrap();
    assert_eq!(
        calls[0].1,
        vec!["run-2".to_string()],
        "the cancelled turn is not handed to the extractor"
    );
    assert_eq!(
        *runs.marked.lock().unwrap(),
        vec!["run-1".to_string(), "run-2".to_string()],
        "both are retired: the cancelled one was considered and declined"
    );
}

#[tokio::test]
async fn a_batch_of_only_cancelled_runs_is_retired_without_calling_the_model() {
    let runs = FakeRuns::new(vec![
        run("run-1", "cli:s", RunStatus::Failed, CANCELLED_ERROR),
        run("run-2", "cli:s", RunStatus::Failed, CANCELLED_ERROR),
    ]);
    let reviewer = Arc::new(RecordingReviewer::default());
    let c = coordinator(runs.clone(), reviewer.clone(), 2);

    c.run(LearningTrigger::AfterRun {
        run_id: "run-2".into(),
    })
    .await
    .unwrap();

    assert!(reviewer.calls.lock().unwrap().is_empty());
    assert_eq!(runs.marked.lock().unwrap().len(), 2);
}

/// The watermark is a durable event now; the row is the index over it. A
/// pass has to say *per turn* which of the two things happened to it, or a
/// rebuilt ledger cannot tell a turn it learned from apart from one it
/// declined.
#[tokio::test]
async fn each_retired_turn_leaves_its_own_watermark_event() {
    let runs = FakeRuns::new(vec![
        run("run-1", "cli:s", RunStatus::Failed, CANCELLED_ERROR),
        done("run-2", "cli:s"),
    ]);
    let reviewer = Arc::new(RecordingReviewer::default());
    let events = FakeEvents::new();
    let c = coordinator_logging(
        runs.clone(),
        reviewer.clone(),
        2,
        events.clone(),
        Vec::new(),
    );

    c.run(LearningTrigger::AfterRun {
        run_id: "run-2".into(),
    })
    .await
    .unwrap();

    assert_eq!(
        events.watermarks(),
        vec![
            ("run-1".to_string(), Some(CANCELLED.to_string())),
            ("run-2".to_string(), None),
        ],
        "the cancelled turn is skipped with a reason; the extracted one completes"
    );
    assert!(
        events.flushes.load(Ordering::Relaxed) > 0,
        "the watermark has to survive a crash, or the batch is learned twice"
    );
}

/// A delegation is the parent turn's own work, done by a sub-agent on its
/// own session. Learning from both hands the consolidator one occasion
/// twice — and a second independent occasion is exactly what it counts as
/// corroboration, so a claim the parent turn made once could promote itself.
#[tokio::test]
async fn a_delegated_session_is_not_a_second_occasion() {
    let runs = FakeRuns::new(vec![done("run-1", "sub"), done("run-2", "cli:a")]);
    let reviewer = Arc::new(RecordingReviewer::default());
    let events = FakeEvents::new();
    let c = LearningCoordinator::new(
        FakeSessions::with_origins(vec![("sub", SessionOrigin::Delegate)]),
        runs.clone(),
        events.clone(),
        reviewer.clone(),
        1,
    );

    c.run(LearningTrigger::Scheduled).await.unwrap();

    let sessions: Vec<String> = reviewer
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(session, _)| session.clone())
        .collect();
    assert_eq!(
        sessions,
        vec!["cli:a".to_string()],
        "the delegate's session is not handed to the extractor"
    );
    assert_eq!(
        events.watermarks(),
        vec![
            ("run-1".to_string(), Some(DELEGATED_TURN.to_string())),
            ("run-2".to_string(), None),
        ],
        "and it is retired with its reason, not left for every future sweep"
    );
}

/// An exempt session is examined and declined, and the log has to say so —
/// otherwise a rebuilt ledger offers every sweep turn to the extractor.
#[tokio::test]
async fn a_sweep_session_is_skipped_with_its_reason() {
    let runs = FakeRuns::new(vec![done("run-1", "sweep-a")]);
    let reviewer = Arc::new(RecordingReviewer::default());
    let events = FakeEvents::new();
    let c = coordinator_logging(
        runs.clone(),
        reviewer.clone(),
        1,
        events.clone(),
        vec!["sweep-a"],
    );

    c.run(LearningTrigger::AfterRun {
        run_id: "run-1".into(),
    })
    .await
    .unwrap();

    assert_eq!(
        events.watermarks(),
        vec![("run-1".to_string(), Some(SWEEP_SESSION.to_string()))]
    );
}

/// The log leads the row. If the event cannot be written the turn stays in
/// the backlog: a row that reads `learned` over a log that never said so
/// comes back unlearned the moment the ledger is rebuilt from events.
#[tokio::test]
async fn a_log_that_cannot_record_the_watermark_keeps_the_batch_pending() {
    let runs = FakeRuns::new(vec![done("run-1", "cli:s"), done("run-2", "cli:s")]);
    let reviewer = Arc::new(RecordingReviewer::default());
    let c = coordinator_logging(
        runs.clone(),
        reviewer.clone(),
        2,
        FakeEvents::offline(),
        Vec::new(),
    );

    c.run(LearningTrigger::AfterRun {
        run_id: "run-2".into(),
    })
    .await
    .unwrap();

    assert_eq!(
        reviewer.calls.lock().unwrap().len(),
        1,
        "the pass still ran — only the watermark failed"
    );
    assert!(
        runs.marked.lock().unwrap().is_empty(),
        "the row must not advance past a watermark the log does not hold"
    );
}

#[tokio::test]
async fn sweep_sessions_are_never_learned_from_but_are_retired() {
    let runs = FakeRuns::new(vec![
        done("run-1", "sweep-a"),
        done("run-2", "sweep-b"),
        done("run-3", "a-conversation"),
    ]);
    let reviewer = Arc::new(RecordingReviewer::default());
    let c = coordinator_with_sweeps(
        runs.clone(),
        reviewer.clone(),
        1,
        vec!["sweep-a", "sweep-b"],
    );

    let report = c.run(LearningTrigger::Scheduled).await.unwrap();

    assert_eq!(report.sessions_learned, 1);
    let calls = reviewer.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "a-conversation");
    assert_eq!(
        runs.marked.lock().unwrap().len(),
        3,
        "the exempt runs are retired too, or the sweep re-declines them forever"
    );
}

#[tokio::test]
async fn the_after_run_trigger_retires_an_exempt_run_without_scanning() {
    let runs = FakeRuns::new(vec![done("run-1", "sweep-a")]);
    let reviewer = Arc::new(RecordingReviewer::default());
    let c = coordinator_with_sweeps(runs.clone(), reviewer.clone(), 1, vec!["sweep-a"]);

    let report = c
        .run(LearningTrigger::AfterRun {
            run_id: "run-1".into(),
        })
        .await
        .unwrap();

    assert!(report.is_empty());
    assert!(reviewer.calls.lock().unwrap().is_empty());
    assert_eq!(*runs.marked.lock().unwrap(), vec!["run-1".to_string()]);
}

#[tokio::test]
async fn the_sweep_learns_each_session_separately_whatever_the_interval() {
    let runs = FakeRuns::new(vec![
        done("run-1", "cli:a"),
        done("run-2", "cli:b"),
        done("run-3", "cli:a"),
    ]);
    let reviewer = Arc::new(RecordingReviewer::default());
    // Interval 10 — far above what either session holds; the sweep ignores it.
    let c = coordinator(runs.clone(), reviewer.clone(), 10);

    let report = c.run(LearningTrigger::Scheduled).await.unwrap();

    assert_eq!(report.sessions_learned, 2);
    assert_eq!(report.episodes_learned, 3);
    let calls = reviewer.calls.lock().unwrap();
    assert_eq!(calls[0].0, "cli:a");
    assert_eq!(calls[0].1, vec!["run-1".to_string(), "run-3".to_string()]);
    assert_eq!(calls[1].0, "cli:b");
}

#[tokio::test]
async fn a_failed_pass_retires_nothing_so_the_next_sweep_retries_it() {
    let runs = FakeRuns::new(vec![done("run-1", "cli:s")]);
    let reviewer = Arc::new(RecordingReviewer {
        fail: true,
        ..Default::default()
    });
    let c = coordinator(runs.clone(), reviewer.clone(), 1);

    let _ = c.run(LearningTrigger::Scheduled).await;

    assert!(
        runs.marked.lock().unwrap().is_empty(),
        "a failed extraction must not advance the watermark"
    );
    // And the run is still on offer.
    assert_eq!(runs.unlearned(None, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_failing_session_does_not_abort_the_sweep() {
    // `cli:a` fails; the sweep must still reach `cli:b`. One reviewer that
    // fails on every call would stop both, so failure is per-session here:
    // both fail, and the test asserts both were attempted.
    let runs = FakeRuns::new(vec![done("run-1", "cli:a"), done("run-2", "cli:b")]);
    let reviewer = Arc::new(RecordingReviewer {
        fail: true,
        ..Default::default()
    });
    let c = coordinator(runs.clone(), reviewer.clone(), 1);

    let report = c.run(LearningTrigger::Scheduled).await.unwrap();

    assert!(report.is_empty());
    assert_eq!(
        reviewer.calls.lock().unwrap().len(),
        2,
        "the second session is still attempted after the first fails"
    );
}

#[tokio::test]
async fn concurrent_triggers_learn_a_session_once() {
    let runs = FakeRuns::new(vec![done("run-1", "cli:s")]);
    let gate = Arc::new(tokio::sync::Notify::new());
    let reviewer = Arc::new(RecordingReviewer {
        gate: Some(gate.clone()),
        ..Default::default()
    });
    let c = Arc::new(coordinator(runs.clone(), reviewer.clone(), 1));

    let first = tokio::spawn({
        let c = c.clone();
        async move {
            c.run(LearningTrigger::AfterRun {
                run_id: "run-1".into(),
            })
            .await
            .unwrap()
        }
    });
    for _ in 0..100 {
        if !reviewer.calls.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let during = c.run(LearningTrigger::Scheduled).await.unwrap();
    assert!(
        during.is_empty(),
        "an in-flight session is skipped: two passes over the same \
             unretired runs would extract each episode twice"
    );

    gate.notify_one();
    assert_eq!(first.await.unwrap().sessions_learned, 1);
    assert_eq!(reviewer.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_watermark_failure_still_reports_what_was_learned() {
    let mut fake = FakeRuns {
        runs: Mutex::new(vec![done("run-1", "cli:s")]),
        steps: Mutex::new(Vec::new()),
        marked: Mutex::new(Vec::new()),
        fail_mark: false,
    };
    fake.fail_mark = true;
    let runs = Arc::new(fake);
    let reviewer = Arc::new(RecordingReviewer {
        outcome: ReviewOutcome {
            memories_written: vec!["m1".into()],
        },
        ..Default::default()
    });
    let c = LearningCoordinator::new(FakeSessions::new(), runs, FakeEvents::new(), reviewer, 1);

    let report = c.run(LearningTrigger::Scheduled).await.unwrap();

    assert_eq!(report.memories_written, 1);
    // The un-advanced watermark just means a future re-read — allowed.
}

#[tokio::test]
async fn a_run_still_in_flight_is_never_offered() {
    let runs = FakeRuns::new(vec![
        run("run-1", "cli:s", RunStatus::Running, ""),
        done("run-2", "cli:s"),
    ]);
    let reviewer = Arc::new(RecordingReviewer::default());
    let c = coordinator(runs.clone(), reviewer.clone(), 1);

    c.run(LearningTrigger::Scheduled).await.unwrap();

    let calls = reviewer.calls.lock().unwrap();
    assert_eq!(calls[0].1, vec!["run-2".to_string()]);
}

#[tokio::test]
async fn an_unknown_run_id_is_ignored() {
    let runs = FakeRuns::new(Vec::new());
    let reviewer = Arc::new(RecordingReviewer::default());
    let c = coordinator(runs, reviewer.clone(), 1);

    let report = c
        .run(LearningTrigger::AfterRun {
            run_id: "run-gone".into(),
        })
        .await
        .unwrap();

    assert!(report.is_empty());
    assert!(reviewer.calls.lock().unwrap().is_empty());
}

// ── delayed feedback (Phase 2) ─────────────────────────────────────────

struct VerdictLlm(&'static str);

#[async_trait]
impl LlmClient for VerdictLlm {
    async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
        Ok(self.0.to_string())
    }
}

/// Two runs in one session, the second carrying the user's reaction to the
/// first.
fn feedback_pair(reaction: &str) -> Arc<FakeRuns> {
    let mut first = done("run-1", "cli:s");
    first.started_at = 100;
    first.input = "fix the failing test".into();
    first.final_output = "Fixed — the assertion was inverted.".into();
    let mut second = done("run-2", "cli:s");
    second.started_at = 200;
    second.input = reaction.into();
    second.final_output = "ok".into();
    FakeRuns::new(vec![first, second])
}

fn verdict_of(runs: &Arc<FakeRuns>, id: &str) -> OutcomeVerdictName {
    let stored = runs
        .runs
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.id == id)
        .unwrap()
        .outcome
        .clone();
    serde_json::from_str::<OutcomeAssessment>(&stored)
        .map(|a| a.verdict)
        .unwrap_or_default()
}

type OutcomeVerdictName = komo_core::domain::episode::OutcomeVerdict;

fn coordinator_with_feedback(runs: Arc<FakeRuns>, answer: &'static str) -> LearningCoordinator {
    LearningCoordinator::new(
        FakeSessions::new(),
        runs,
        FakeEvents::new(),
        Arc::new(RecordingReviewer::default()),
        10,
    )
    .with_feedback(Arc::new(VerdictLlm(answer)))
}

/// The point of Phase 2: "可以了" is a verdict on the turn *before* it, not
/// on the empty turn that carried it.
#[tokio::test]
async fn a_confirmation_settles_the_previous_run_not_the_current_one() {
    let runs = feedback_pair("可以了");
    let c = coordinator_with_feedback(runs.clone(), "SUCCESS");

    c.run(LearningTrigger::AfterRun {
        run_id: "run-2".into(),
    })
    .await
    .unwrap();

    assert_eq!(verdict_of(&runs, "run-1"), OutcomeVerdictName::Success);
    assert_eq!(
        verdict_of(&runs, "run-2"),
        OutcomeVerdictName::Unknown,
        "the turn carrying the feedback is not the turn it judges"
    );
}

/// The agent said it fixed it. The user says otherwise, and the user wins —
/// that ordering is the whole reason evidence carries a strength.
#[tokio::test]
async fn a_rejection_overturns_the_turns_own_report() {
    let runs = feedback_pair("还是不行");
    let c = coordinator_with_feedback(runs.clone(), "FAILURE");

    c.run(LearningTrigger::AfterRun {
        run_id: "run-2".into(),
    })
    .await
    .unwrap();

    assert_eq!(verdict_of(&runs, "run-1"), OutcomeVerdictName::Failure);
}

#[tokio::test]
async fn an_ordinary_next_request_leaves_the_previous_run_unknown() {
    let runs = feedback_pair("现在改一下 README");
    let c = coordinator_with_feedback(runs.clone(), "NEITHER");

    c.run(LearningTrigger::AfterRun {
        run_id: "run-2".into(),
    })
    .await
    .unwrap();

    assert_eq!(verdict_of(&runs, "run-1"), OutcomeVerdictName::Unknown);
}

/// An uncertain step still means "we do not know", even when the user is
/// happy: their satisfaction says the goal was met, and it is — the
/// strongest evidence decides, and user confirmation outranks it.
#[tokio::test]
async fn a_confirmation_outranks_an_uncertain_step() {
    let runs = feedback_pair("可以了");
    runs.steps.lock().unwrap().push(RunStep {
        run_id: "run-1".into(),
        seq: 1,
        tool_name: "shell".into(),
        args: "{}".into(),
        result: String::new(),
        error: "timed out".into(),
        ok: false,
        uncertain: true,
        started_at: 0,
        ended_at: 0,
        elapsed_ms: 0,
        structured: serde_json::Value::Null,
        output_paths: Vec::new(),
        approved_by: String::new(),
        approval_waited_ms: 0,
    });
    let c = coordinator_with_feedback(runs.clone(), "SUCCESS");

    c.run(LearningTrigger::AfterRun {
        run_id: "run-2".into(),
    })
    .await
    .unwrap();

    let stored = runs
        .runs
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.id == "run-1")
        .unwrap()
        .outcome
        .clone();
    let assessment: OutcomeAssessment = serde_json::from_str(&stored).unwrap();
    assert_eq!(assessment.verdict, OutcomeVerdictName::Success);
    assert_eq!(
        assessment.evidence.len(),
        2,
        "the uncertain step is kept as evidence, just outranked"
    );
}

#[tokio::test]
async fn the_first_run_of_a_session_has_nothing_to_give_feedback_on() {
    let runs = FakeRuns::new(vec![done("run-1", "cli:s")]);
    let c = coordinator_with_feedback(runs.clone(), "SUCCESS");

    c.run(LearningTrigger::AfterRun {
        run_id: "run-1".into(),
    })
    .await
    .unwrap();

    assert_eq!(verdict_of(&runs, "run-1"), OutcomeVerdictName::Unknown);
}

/// Without an aux model there is no verdict to be had, and the turn must
/// still be assessed and still be learnable.
#[tokio::test]
async fn feedback_is_optional() {
    let runs = feedback_pair("可以了");
    let c = coordinator(runs.clone(), Arc::new(RecordingReviewer::default()), 10);

    c.run(LearningTrigger::AfterRun {
        run_id: "run-2".into(),
    })
    .await
    .unwrap();

    assert_eq!(verdict_of(&runs, "run-1"), OutcomeVerdictName::Unknown);
    assert!(
        !runs.runs.lock().unwrap()[1].outcome.is_empty(),
        "the deterministic assessment is still stored"
    );
}

/// A revised outcome has to reach the extractor, or revising it changed
/// nothing about what komo learns.
#[tokio::test]
async fn learning_reads_the_stored_outcome_rather_than_recomputing_it() {
    let runs = feedback_pair("可以了");
    let reviewer = Arc::new(RecordingReviewer::default());
    let c = LearningCoordinator::new(
        FakeSessions::new(),
        runs.clone(),
        FakeEvents::new(),
        reviewer.clone(),
        2,
    )
    .with_feedback(Arc::new(VerdictLlm("SUCCESS")));

    c.run(LearningTrigger::AfterRun {
        run_id: "run-2".into(),
    })
    .await
    .unwrap();

    let verdicts = reviewer.verdicts.lock().unwrap();
    assert_eq!(
        verdicts.first().copied(),
        Some(OutcomeVerdictName::Success),
        "the extractor must see the confirmed outcome, not a fresh Unknown"
    );
}
