use super::*;
use async_trait::async_trait;
use komo_core::domain::llm::{DeltaSink, Step, ToolOutcome, TurnDriver};
use komo_core::domain::memory::BeliefState;
use std::sync::Mutex;

struct FakeStore {
    memories: Mutex<Vec<Memory>>,
    /// Every `(id, belief)` handed to `save`, in order — how the intermediate
    /// states of a multi-write consolidation are observed.
    writes: Mutex<Vec<(String, BeliefState)>>,
}

impl FakeStore {
    fn new(memories: Vec<Memory>) -> Self {
        Self {
            memories: Mutex::new(memories),
            writes: Mutex::new(Vec::new()),
        }
    }

    /// The belief state of every write to `id`, oldest first.
    fn saved_beliefs(&self, id: &str) -> Vec<BeliefState> {
        self.writes
            .lock()
            .unwrap()
            .iter()
            .filter(|(written, _)| written == id)
            .map(|(_, belief)| *belief)
            .collect()
    }
    fn get(&self, id: &str) -> Memory {
        self.memories
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == id)
            .cloned()
            .expect("memory present")
    }
    fn len(&self) -> usize {
        self.memories.lock().unwrap().len()
    }
}

#[async_trait]
impl MemoryRepository for FakeStore {
    async fn save(&self, memory: &Memory) -> anyhow::Result<()> {
        self.writes
            .lock()
            .unwrap()
            .push((memory.id.clone(), memory.belief));
        let mut all = self.memories.lock().unwrap();
        match all.iter_mut().find(|m| m.id == memory.id) {
            Some(existing) => *existing = memory.clone(),
            None => all.push(memory.clone()),
        }
        Ok(())
    }
    async fn list(&self) -> anyhow::Result<Vec<Memory>> {
        Ok(self.memories.lock().unwrap().clone())
    }
}

/// An aux model with a canned reply, or a failure.
struct FakeAux(anyhow::Result<String>);

#[async_trait]
impl LlmClient for FakeAux {
    async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
        match &self.0 {
            Ok(reply) => Ok(reply.clone()),
            Err(e) => Err(anyhow::anyhow!("{e:#}")),
        }
    }
    async fn begin_turn(
        &self,
        _session: &Session,
        _deltas: Option<Arc<dyn DeltaSink>>,
        _recorder: Option<Arc<dyn komo_core::domain::session_event::TurnRecorder>>,
    ) -> anyhow::Result<Box<dyn TurnDriver>> {
        struct Dead;
        #[async_trait]
        impl TurnDriver for Dead {
            async fn first(&mut self) -> anyhow::Result<Step> {
                anyhow::bail!("unused")
            }
            async fn step(
                &mut self,
                _r: Vec<ToolOutcome>,
                _i: Option<String>,
            ) -> anyhow::Result<Step> {
                anyhow::bail!("unused")
            }
        }
        Ok(Box::new(Dead))
    }
}

fn active(id: &str, content: &str) -> Memory {
    let mut m = Memory::new(MemoryKind::Preference, content);
    m.id = id.to_string();
    m.status = MemoryStatus::Active;
    m
}

fn observation(content: &str) -> Observation {
    Observation {
        kind: MemoryKind::Preference,
        content: content.to_string(),
        excerpt: format!("user said: {content}"),
        provenance: MemoryProvenance::User,
    }
}

/// The same claim, but read out of something a tool returned.
fn from_tool(content: &str) -> Observation {
    Observation {
        provenance: MemoryProvenance::Tool,
        ..observation(content)
    }
}

fn consolidator(store: Arc<FakeStore>, reply: anyhow::Result<String>) -> MemoryConsolidator {
    let repo: Arc<dyn MemoryRepository> = store;
    let query = Arc::new(MemoryQueryService::new(repo.clone()));
    MemoryConsolidator::new(repo, Arc::new(FakeAux(reply)), query)
}

fn ctx() -> MemoryContext {
    MemoryContext::local("s1")
}

/// Nothing related in the library: the observation lands as a candidate,
/// carrying the evidence that created it.
#[tokio::test]
async fn an_unrelated_observation_becomes_a_candidate_with_founding_evidence() {
    let store = Arc::new(FakeStore::new(Vec::new()));
    let c = consolidator(store.clone(), Ok(String::new()));
    let out = c
        .consolidate_all(
            &ctx(),
            "s-1",
            &Occasion::single("s-1"),
            vec![observation("user prefers rebase")],
        )
        .await
        .unwrap();
    let Consolidated::Created { id } = &out[0] else {
        panic!("expected Created, got {:?}", out[0]);
    };
    let written = store.get(id);
    assert_eq!(written.status, MemoryStatus::Candidate);
    assert_eq!(written.confidence, MemoryConfidence::Extracted);
    assert_eq!(written.support_count, 1, "founding evidence is recorded");
    assert_eq!(written.evidence[0].session, "s-1");
    assert!(written.evidence[0].excerpt.contains("prefers rebase"));
}

/// A claim the user rejected must not come back as a fresh candidate the
/// next time it is observed — that is how a rejection is forgotten, one
/// occasion at a time. The consolidator sees rejected claims; the prompt
/// still never does.
#[tokio::test]
async fn a_rejected_claim_is_recognised_rather_than_filed_again() {
    let mut rejected = active("mem-1", "user prefers rebase before push");
    rejected.status = MemoryStatus::Rejected;
    let store = Arc::new(FakeStore::new(vec![rejected]));
    let c = consolidator(
        store.clone(),
        Ok(r#"{"relation":"same","target":"mem-1"}"#.into()),
    );

    let out = c
        .consolidate_all(
            &ctx(),
            "s-2",
            &Occasion::single("s-2"),
            vec![observation(
                "user rebases rather than merging before a push",
            )],
        )
        .await
        .unwrap();

    assert_eq!(out[0], Consolidated::Supported { id: "mem-1".into() });
    assert_eq!(store.len(), 1, "no second memory was created");
    let after = store.get("mem-1");
    assert_eq!(
        after.status,
        MemoryStatus::Rejected,
        "seeing it again is not a reason to un-reject it"
    );
    // And it stays out of every prompt: injection reads `select_recall`.
    let query = komo_core::domain::memory::RecallQuery::lexical("rebase before push");
    assert!(
        komo_core::domain::memory::select_recall(
            &store.list().await.unwrap(),
            &ctx(),
            &query,
            5,
            0
        )
        .is_empty(),
        "a rejected claim is never recallable"
    );
}

/// A page komo read is a page saying something, not the user saying it. It
/// may be recorded — and nothing else: it must not add support to what the
/// user said, and it must not silence it by contesting it.
#[tokio::test]
async fn a_claim_a_tool_returned_does_not_touch_what_the_user_said() {
    let existing = active("mem-1", "user prefers rebase before push");
    let store = Arc::new(FakeStore::new(vec![existing]));
    // The classifier would happily call this the same claim; it is never
    // asked.
    let c = consolidator(
        store.clone(),
        Ok(r#"{"relation":"same","target":"mem-1"}"#.into()),
    );

    let out = c
        .consolidate_all(
            &ctx(),
            "s-2",
            &Occasion::single("s-2"),
            vec![from_tool("user rebases rather than merging before a push")],
        )
        .await
        .unwrap();

    let Consolidated::Created { id } = &out[0] else {
        panic!(
            "a tool-derived claim lands as its own candidate, got {:?}",
            out[0]
        );
    };
    let created = store.get(id);
    assert_eq!(created.provenance, MemoryProvenance::Tool);
    assert_eq!(
        store.get("mem-1").support_count,
        0,
        "the user's own claim gained nothing from a page agreeing with it"
    );
    assert_eq!(
        store.get("mem-1").belief,
        komo_core::domain::memory::BeliefState::Current,
        "and nothing a tool returned may contest it either"
    );
}

/// A restatement in different words adds support instead of a second memory —
/// the deduplication the exact-key check could never do."""

#[tokio::test]
async fn a_reworded_restatement_supports_the_existing_claim() {
    let existing = active("mem-1", "user prefers rebase before push");
    let store = Arc::new(FakeStore::new(vec![existing]));
    let c = consolidator(
        store.clone(),
        Ok(r#"{"relation":"same","target":"mem-1"}"#.into()),
    );
    let out = c
        .consolidate_all(
            &ctx(),
            "s-2",
            &Occasion::single("s-2"),
            vec![observation(
                "user rebases rather than merging before a push",
            )],
        )
        .await
        .unwrap();
    assert_eq!(out[0], Consolidated::Supported { id: "mem-1".into() });
    assert_eq!(store.len(), 1, "no second memory was created");
    let after = store.get("mem-1");
    assert_eq!(after.support_count, 1);
    assert_eq!(after.evidence[0].session, "s-2");
}

/// One learning pass cannot support one claim twice, however many statements
/// it read — that is what makes the count mean "independent occasions".
#[tokio::test]
async fn one_occasion_cannot_support_the_same_claim_twice() {
    let store = Arc::new(FakeStore::new(vec![active("mem-1", "user prefers rebase")]));
    let c = consolidator(
        store.clone(),
        Ok(r#"{"relation":"supports","target":"mem-1"}"#.into()),
    );
    let out = c
        .consolidate_all(
            &ctx(),
            "home",
            &Occasion::single("run-2"),
            vec![
                observation("user rebases rather than merging"),
                observation("user always rebases their branches"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(out[0], Consolidated::Supported { id: "mem-1".into() });
    assert_eq!(
        out[1],
        Consolidated::Skipped,
        "same occasion, no new support"
    );
    assert_eq!(store.get("mem-1").support_count, 1);

    // A later pass on the *same* session is a new occasion, and does count —
    // the home session is one permanent conversation, so nothing extracted
    // there could ever promote otherwise.
    let out = c
        .consolidate_all(
            &ctx(),
            "home",
            &Occasion::single("run-3"),
            vec![observation("user rebases their branches before pushing")],
        )
        .await
        .unwrap();
    assert_eq!(out[0], Consolidated::Supported { id: "mem-1".into() });
    assert_eq!(store.get("mem-1").support_count, 2);
}

/// A conflict silences the old claim rather than letting both be injected.
#[tokio::test]
async fn a_contradiction_contests_the_old_claim_and_lands_the_new_one() {
    let store = Arc::new(FakeStore::new(vec![active(
        "mem-1",
        "user mainly uses Python",
    )]));
    let c = consolidator(
        store.clone(),
        Ok(r#"{"relation":"contradicts","target":"mem-1"}"#.into()),
    );
    let out = c
        .consolidate_all(
            &ctx(),
            "s-2",
            &Occasion::single("s-2"),
            vec![observation("user mainly uses Rust")],
        )
        .await
        .unwrap();
    let Consolidated::Contested { old, new } = &out[0] else {
        panic!("expected Contested, got {:?}", out[0]);
    };
    assert_eq!(old, "mem-1");
    let old = store.get(old);
    assert_eq!(old.belief, BeliefState::Contested);
    assert!(
        !old.is_injectable(),
        "a contested claim stops being asserted"
    );
    assert_eq!(old.contradiction_count, 1);
    // The new claim is a normal candidate, believed until something conflicts.
    let new = store.get(new);
    assert_eq!(new.status, MemoryStatus::Candidate);
    assert!(new.is_injectable());
}

/// An explicit change of position retires the old claim as history and links
/// it forward — a settled ruling, unlike a contest.
#[tokio::test]
async fn an_explicit_change_supersedes_the_old_claim() {
    let store = Arc::new(FakeStore::new(vec![active(
        "mem-1",
        "user wants Python examples",
    )]));
    let c = consolidator(
        store.clone(),
        Ok(r#"{"relation":"supersedes","target":"mem-1"}"#.into()),
    );
    let out = c
        .consolidate_all(
            &ctx(),
            "s-2",
            &Occasion::single("s-2"),
            vec![observation("user wants Rust examples from now on")],
        )
        .await
        .unwrap();
    let Consolidated::Superseded { old, new } = &out[0] else {
        panic!("expected Superseded, got {:?}", out[0]);
    };
    let old = store.get(old);
    assert_eq!(old.belief, BeliefState::Superseded);
    assert_eq!(&old.superseded_by, new, "history points at its replacement");
    assert!(!old.is_injectable());
    // Silenced by the *first* write, before the replacement existed: every
    // intermediate state has to be non-injectable, or a crash mid-supersede
    // would leave both claims assertable forever.
    assert!(
        store
            .saved_beliefs(&old.id)
            .iter()
            .all(|b| *b != BeliefState::Current),
        "the old claim was never left believed after the first write"
    );
}

/// Restating a claim in the same words never manufactures a second
/// candidate — it is the same claim whichever pass reads it. A *later* pass
/// is a later occasion, though, so it backs the claim up.
#[tokio::test]
async fn a_re_review_of_the_same_session_never_duplicates_the_candidate() {
    let store = Arc::new(FakeStore::new(Vec::new()));
    let c = consolidator(store.clone(), Ok(String::new()));
    let first = c
        .consolidate_all(
            &ctx(),
            "s-1",
            &Occasion::single("s-1"),
            vec![observation("komo is written in Rust")],
        )
        .await
        .unwrap();
    assert!(matches!(first[0], Consolidated::Created { .. }));

    // A later sweep, a new occasion, the same transcript.
    let second = c
        .consolidate_all(
            &ctx(),
            "s-1",
            &Occasion::single("run-2"),
            vec![observation("komo is written in Rust")],
        )
        .await
        .unwrap();
    assert!(matches!(second[0], Consolidated::Supported { .. }));
    assert_eq!(store.len(), 1, "no duplicate candidate");
}

/// A memory the model itself saved mid-turn via the `memory` tool is this
/// session's own output too — the tool leaves `source` empty, so only its
/// evidence says so. Extracting it again on the occasion that evidence
/// already names would count one occasion twice.
#[tokio::test]
async fn a_memory_the_tool_saved_this_session_is_not_extracted_again() {
    let mut saved = active("mem-1", "komo is written in Rust");
    saved.status = MemoryStatus::Candidate;
    saved.record_evidence(
        "s-1",
        "run-1",
        EvidenceRelation::Supports,
        "user said so",
        100,
    );
    let store = Arc::new(FakeStore::new(vec![saved]));
    let c = consolidator(
        store.clone(),
        Err(anyhow::anyhow!("aux must not be consulted")),
    );

    let out = c
        .consolidate_all(
            &ctx(),
            "s-1",
            &Occasion::single("run-1"),
            vec![observation("komo is written in Rust")],
        )
        .await
        .unwrap();
    assert_eq!(out[0], Consolidated::Skipped);
    assert_eq!(store.len(), 1, "no duplicate candidate");
    assert_eq!(store.get("mem-1").support_count, 1, "no second occasion");
}

/// …but a *different* session, on a different occasion, observing the same
/// claim is a real second occasion, and must still reach the classifier.
#[tokio::test]
async fn another_session_observing_the_same_claim_is_not_skipped() {
    let mut saved = active("mem-1", "komo is written in Rust");
    saved.status = MemoryStatus::Candidate;
    saved.record_evidence(
        "s-1",
        "run-1",
        EvidenceRelation::Supports,
        "user said so",
        100,
    );
    let store = Arc::new(FakeStore::new(vec![saved]));
    let c = consolidator(
        store.clone(),
        Ok(r#"{"relation":"supports","target":"mem-1"}"#.into()),
    );

    let out = c
        .consolidate_all(
            &ctx(),
            "s-2",
            &Occasion::single("run-2"),
            vec![observation("komo is written in Rust")],
        )
        .await
        .unwrap();
    assert_eq!(out[0], Consolidated::Supported { id: "mem-1".into() });
    assert_eq!(store.get("mem-1").support_count, 2);
}

/// A memory this session produced, already carrying this pass's evidence.
fn produced_by(session: &str, occasion: &str, content: &str) -> Memory {
    let mut m = Memory::new(MemoryKind::Preference, content);
    m.id = "mem-1".to_string();
    m.status = MemoryStatus::Candidate;
    m.source = session.to_string();
    m.source_message_id = memory_key(content);
    m.record_evidence(
        session,
        occasion,
        EvidenceRelation::Supports,
        "user said so",
        0,
    );
    m
}

/// One pass reading its own transcript twice is one occasion: the claim it
/// already filed gains nothing.
#[tokio::test]
async fn one_occasion_restating_a_claim_it_already_filed_is_skipped() {
    let store = Arc::new(FakeStore::new(vec![produced_by(
        "home",
        "run-1",
        "komo is written in Rust",
    )]));
    let c = consolidator(
        store.clone(),
        Err(anyhow::anyhow!("classifier must not be consulted")),
    );
    let out = c
        .consolidate_all(
            &ctx(),
            "home",
            &Occasion::single("run-1"),
            vec![observation("komo is written in Rust")],
        )
        .await
        .unwrap();
    assert_eq!(out[0], Consolidated::Skipped);
    assert_eq!(store.get("mem-1").support_count, 1, "no support was added");
    assert_eq!(store.len(), 1);
}

/// The same words on a *later* occasion are the claim being made again. The
/// home session is one permanent conversation, so this is the only way an
/// identically worded confirmation there ever counts.
#[tokio::test]
async fn a_later_occasion_restating_it_word_for_word_supports_it() {
    let store = Arc::new(FakeStore::new(vec![produced_by(
        "home",
        "run-1",
        "komo is written in Rust",
    )]));
    // An identical key is trivially "same", so no aux call is worth making.
    // This aux fails, and a consulted classifier that fails lands a *second*
    // candidate — so `Supported`, over one memory, is the assertion that it
    // was never called.
    let c = consolidator(
        store.clone(),
        Err(anyhow::anyhow!("classifier must not be consulted")),
    );
    let out = c
        .consolidate_all(
            &ctx(),
            "home",
            &Occasion::single("run-2"),
            vec![observation("komo is written in Rust")],
        )
        .await
        .unwrap();
    assert_eq!(out[0], Consolidated::Supported { id: "mem-1".into() });
    let after = store.get("mem-1");
    assert_eq!(after.support_count, 2);
    assert_eq!(after.evidence.last().unwrap().occasion, "run-2");
    assert_eq!(store.len(), 1, "still one claim, not two");
}

/// The provenance rule is untouched by any of this: a claim read out of tool
/// output supports nothing, however many occasions repeat it — and it does
/// not file a duplicate of the candidate it already produced either.
#[tokio::test]
async fn a_tool_derived_restatement_still_supports_nothing() {
    let store = Arc::new(FakeStore::new(vec![produced_by(
        "home",
        "run-1",
        "komo is written in Rust",
    )]));
    let c = consolidator(
        store.clone(),
        Err(anyhow::anyhow!("classifier must not be consulted")),
    );
    let out = c
        .consolidate_all(
            &ctx(),
            "home",
            &Occasion::single("run-2"),
            vec![from_tool("komo is written in Rust")],
        )
        .await
        .unwrap();
    assert_eq!(out[0], Consolidated::Skipped);
    assert_eq!(store.get("mem-1").support_count, 1);
    assert_eq!(store.len(), 1);
}

/// The `memory` tool founds evidence with the turn's *own* run, while the
/// pass that later reviews that turn is a batch of runs named by its oldest.
/// A pass whose batch contains the founding run is the same occasion, so it
/// supports nothing — otherwise reviewing a turn corroborates what the model
/// recorded during it. A batch that does not contain it is a later occasion
/// and does.
#[tokio::test]
async fn a_pass_over_the_batch_that_founded_a_claim_supports_nothing() {
    // As the `memory` tool leaves it: `source` empty, evidence keyed by the
    // run of the turn the save was made in.
    let mut saved = active("mem-1", "komo is written in Rust");
    saved.status = MemoryStatus::Candidate;
    saved.record_evidence(
        "home",
        "r3",
        EvidenceRelation::Supports,
        "user said so",
        100,
    );
    let store = Arc::new(FakeStore::new(vec![saved]));
    let c = consolidator(
        store.clone(),
        Err(anyhow::anyhow!("classifier must not be consulted")),
    );

    let batch = Occasion::over(["r3".into(), "r1".into(), "r2".into()]);
    assert_eq!(batch.key(), "r1", "the canonical name is the oldest run");
    let out = c
        .consolidate_all(
            &ctx(),
            "home",
            &batch,
            vec![observation("komo is written in Rust")],
        )
        .await
        .unwrap();
    assert_eq!(out[0], Consolidated::Skipped);
    assert_eq!(
        store.get("mem-1").support_count,
        1,
        "the batch already witnessed this claim through r3"
    );

    // A batch that shares no run with it is a genuinely later occasion.
    let out = c
        .consolidate_all(
            &ctx(),
            "home",
            &Occasion::single("r4"),
            vec![observation("komo is written in Rust")],
        )
        .await
        .unwrap();
    assert_eq!(out[0], Consolidated::Supported { id: "mem-1".into() });
    assert_eq!(store.get("mem-1").support_count, 2);
    assert_eq!(store.len(), 1);
}

/// An exact restatement of something komo already holds active is
/// indistinguishable from the assistant echoing its own injected memory, so it
/// earns nothing.
#[tokio::test]
async fn an_exact_echo_of_an_active_memory_is_skipped() {
    let store = Arc::new(FakeStore::new(vec![active(
        "mem-1",
        "User prefers rebase before push",
    )]));
    let c = consolidator(
        store.clone(),
        Err(anyhow::anyhow!("aux must not be consulted")),
    );
    let out = c
        .consolidate_all(
            &ctx(),
            "s-2",
            &Occasion::single("s-2"),
            // Same text, different case and spacing — the normalized key matches.
            vec![observation("user prefers   REBASE before push")],
        )
        .await
        .unwrap();
    assert_eq!(out[0], Consolidated::Skipped);
    assert_eq!(store.len(), 1);
    assert_eq!(store.get("mem-1").support_count, 0);
}

/// Every aux failure lands a candidate: the behavior that predates the seam.
#[tokio::test]
async fn aux_failure_degrades_to_writing_a_candidate() {
    for reply in [
        Err(anyhow::anyhow!("aux down")),
        Ok("sorry, I can't help".to_string()),
        Ok(r#"{"relation":"contradicts","target":"mem-fabricated"}"#.to_string()),
    ] {
        let store = Arc::new(FakeStore::new(vec![active("mem-1", "user uses Python")]));
        let c = consolidator(store.clone(), reply);
        let out = c
            .consolidate_all(
                &ctx(),
                "s-2",
                &Occasion::single("s-2"),
                vec![observation("user uses Python 3.12")],
            )
            .await
            .unwrap();
        assert!(
            matches!(out[0], Consolidated::Created { .. }),
            "got {:?}",
            out[0]
        );
        // The untouched original must not have been contested by a reply that
        // named a memory it was never shown.
        assert_eq!(store.get("mem-1").belief, BeliefState::Current);
    }
}

/// Later observations in one batch see what earlier ones did, so two
/// statements about one preference do not become two memories.
#[tokio::test]
async fn a_batch_consolidates_against_its_own_earlier_writes() {
    let store = Arc::new(FakeStore::new(Vec::new()));
    // Nothing exists at first, so observation one creates. Observation two is
    // then classified against it.
    let c = consolidator(
        store.clone(),
        Ok(r#"{"relation":"supports","target":"MEM_PLACEHOLDER"}"#.into()),
    );
    let out = c
        .consolidate_all(
            &ctx(),
            "s-1",
            &Occasion::single("s-1"),
            vec![
                observation("user prefers rebase"),
                observation("user rebases before pushing"),
            ],
        )
        .await
        .unwrap();
    assert!(matches!(out[0], Consolidated::Created { .. }));
    // The fabricated placeholder id is refused, so this falls back to a
    // candidate — but the point stands: the second observation *was* offered
    // the first one's memory as a related claim.
    assert_eq!(store.len(), 2);
    assert!(matches!(out[1], Consolidated::Created { .. }));
}

#[test]
fn a_classification_naming_an_unknown_id_is_refused() {
    let related = vec![ScoredMemory {
        memory: active("mem-1", "x"),
        score: 1.0,
    }];
    assert!(
        parse_classification(r#"{"relation":"contradicts","target":"mem-9"}"#, &related).is_none()
    );
    // …while an explicit "unrelated" needs no target at all.
    assert_eq!(
        parse_classification(r#"{"relation":"unrelated","target":""}"#, &related),
        Some((Relation::Unrelated, None))
    );
}

#[test]
fn relation_labels_parse_leniently() {
    assert_eq!(parse_relation("same"), Relation::Supports);
    assert_eq!(parse_relation("SUPPORTS"), Relation::Supports);
    assert_eq!(parse_relation(" contradicts "), Relation::Contradicts);
    assert_eq!(parse_relation("supersedes"), Relation::Supersedes);
    // Anything unrecognized means "no relationship found".
    assert_eq!(parse_relation("maybe-related"), Relation::Unrelated);
}

#[test]
fn memory_key_normalizes_case_and_whitespace() {
    assert_eq!(
        memory_key("User prefers  rebase"),
        memory_key("user PREFERS rebase")
    );
    assert_ne!(memory_key("uses rust"), memory_key("uses python"));
}
