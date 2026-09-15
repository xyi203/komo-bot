use super::*;
use async_trait::async_trait;

/// Real wall clock. Fixtures are built with `Memory::new`, which stamps
/// `created_at` now, so a fake clock would make every one of them read as
/// decades stale.
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

use komo_core::domain::llm::{DeltaSink, Step, ToolOutcome, TurnDriver};
use komo_core::domain::memory::{
    EvidenceRelation, MEMORY_STALE_AFTER_DAYS, MemoryConfidence, MemoryKind, MemoryStatus,
};
use std::sync::Mutex;

// ---- fakes over the existing repository/LLM seams ----

/// Id batches recorded by `mark_used`.
type UsedCalls = Vec<Vec<String>>;

struct FakeStore {
    memories: Vec<Memory>,
    fail_list: bool,
    used: Arc<Mutex<UsedCalls>>,
}

impl FakeStore {
    fn new(memories: Vec<Memory>) -> Self {
        Self {
            memories,
            fail_list: false,
            used: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl MemoryRepository for FakeStore {
    async fn save(&self, _memory: &Memory) -> anyhow::Result<()> {
        Ok(())
    }
    async fn list(&self) -> anyhow::Result<Vec<Memory>> {
        if self.fail_list {
            anyhow::bail!("store offline");
        }
        Ok(self.memories.clone())
    }
    async fn mark_used(&self, ids: &[String], _now: i64) -> anyhow::Result<()> {
        self.used.lock().unwrap().push(ids.to_vec());
        Ok(())
    }
}

/// An aux agent with a fixed reply (or failure).
struct FakeAux {
    reply: anyhow::Result<String>,
}

#[async_trait]
impl LlmClient for FakeAux {
    async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
        match &self.reply {
            Ok(r) => Ok(r.clone()),
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
                _results: Vec<ToolOutcome>,
                _interjected: Option<String>,
            ) -> anyhow::Result<Step> {
                anyhow::bail!("unused")
            }
        }
        Ok(Box::new(Dead))
    }
}

fn pinned_memory(content: &str) -> Memory {
    let mut m = Memory::new(MemoryKind::Preference, content);
    m.pinned = true;
    m.status = MemoryStatus::Active;
    m.confidence = MemoryConfidence::UserWritten;
    m
}

fn active_fact(id: &str, content: &str) -> Memory {
    let mut m = Memory::new(MemoryKind::Fact, content);
    m.id = id.to_string();
    m.status = MemoryStatus::Active;
    m
}

/// A lexical-only enricher: no embedding backend, which is also the
/// degraded-but-working shape production falls back to.
fn enricher(store: FakeStore, aux: Option<Arc<dyn LlmClient>>) -> MemoryEnricher {
    let store = Arc::new(store);
    let query = Arc::new(MemoryQueryService::new(store.clone()));
    MemoryEnricher::new(store, aux, query)
}

/// An embedding backend returning one fixed vector. The failure modes are
/// exercised where they are handled — `memory_query`'s own tests.
struct FakeEmbedder(Vec<f32>);

#[async_trait]
impl komo_core::domain::embedding::EmbeddingClient for FakeEmbedder {
    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| self.0.clone()).collect())
    }
    fn model_id(&self) -> &str {
        "fake-model"
    }
}

/// A backend that is configured and does not answer — the shape that makes
/// recall degraded rather than lexical-by-design.
struct DeadEmbedder;

#[async_trait]
impl komo_core::domain::embedding::EmbeddingClient for DeadEmbedder {
    async fn embed(&self, _texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("backend down")
    }
    fn model_id(&self) -> &str {
        "fake-model"
    }
}

fn dead_enricher(store: FakeStore) -> MemoryEnricher {
    let store = Arc::new(store);
    let query =
        Arc::new(MemoryQueryService::new(store.clone()).with_embedder(Arc::new(DeadEmbedder)));
    MemoryEnricher::new(store, None, query)
}

/// The case the note exists for. With the semantic half down, a question in
/// one language retrieves nothing from a library written in another — and
/// nothing is also what an empty library returns. Silence here is what makes
/// the model tell the user it has no record of something it holds, so a
/// degraded turn must produce a block even with zero hits.
#[tokio::test]
async fn a_degraded_arm_speaks_up_even_with_nothing_to_show() {
    let e = dead_enricher(FakeStore::new(Vec::new()));
    let injection = e
        .enrich(&Session::new("s"), "我平时用什么语言", &[])
        .await
        .expect("a degraded turn must say so rather than return nothing");
    let recall = injection.recall.expect("a block carrying the note");
    assert!(recall.contains("[recall degraded]"), "got: {recall}");
    assert!(
        recall.contains("do NOT conclude"),
        "the note has to forbid the inference, not just mention the fault: {recall}"
    );
    assert!(
        injection.used.recall.is_empty(),
        "nothing was recalled, so nothing may be recorded as having shaped the turn"
    );
}

/// And when it does have something, the note rides in front of it rather than
/// replacing it — the hits are still valid, the set is just incomplete.
#[tokio::test]
async fn a_degraded_arm_keeps_the_hits_it_did_find() {
    let e = dead_enricher(FakeStore::new(vec![active_fact(
        "m1",
        "kanban tasks live in kanban.db",
    )]));
    let recall = e
        .enrich(&Session::new("s"), "where do kanban tasks live", &[])
        .await
        .expect("hits")
        .recall
        .expect("a block");
    assert!(recall.contains("[recall degraded]"));
    assert!(recall.contains("kanban.db"), "got: {recall}");
}

/// No backend configured is a deployment choice, not a fault. Announcing a
/// degradation every turn would train the model to ignore the note.
#[tokio::test]
async fn a_lexical_only_deployment_never_announces_a_degradation() {
    let e = enricher(FakeStore::new(Vec::new()), None);
    assert!(
        e.enrich(&Session::new("s"), "hello", &[]).await.is_none(),
        "no backend, nothing found: there is nothing to report"
    );
}

/// A memory carrying a vector for the fake backend's model.
fn embedded_fact(id: &str, content: &str, vector: Vec<f32>) -> Memory {
    let mut m = active_fact(id, content);
    m.embedding = vector;
    m.embedding_model = "fake-model".into();
    m
}

/// The end-to-end shape of cross-language recall: a message sharing no
/// lexical term with the memory still reaches the rendered injection block.
#[tokio::test]
async fn semantic_recall_injects_a_memory_with_no_shared_terms() {
    let store = Arc::new(FakeStore::new(vec![embedded_fact(
        "m-zh",
        "User communicates in Chinese.",
        vec![1.0, 0.0],
    )]));
    let query = Arc::new(
        MemoryQueryService::new(store.clone())
            .with_embedder(Arc::new(FakeEmbedder(vec![1.0, 0.0]))),
    );
    let e = MemoryEnricher::new(store, None, query);
    let injection = e
        .enrich(&Session::new("s"), "我平时用什么语言跟你说话", &[])
        .await
        .expect("the semantic arm recalls it");
    assert!(
        injection
            .recall
            .as_deref()
            .unwrap()
            .contains("communicates in Chinese")
    );
}

/// An unresolved conflict must not reach the prompt: handing the model both
/// sides and letting it choose is the failure `BeliefState` exists to stop.
#[tokio::test]
async fn a_contested_memory_is_never_injected() {
    let mut contested = active_fact("m-old", "durable kanban tasks live in kanban.db");
    contested.contest(1_000);
    let e = enricher(FakeStore::new(vec![contested]), None);
    assert!(
        e.enrich(&Session::new("s"), "where do kanban tasks live?", &[])
            .await
            .is_none(),
        "the only match was contested, so nothing is injected"
    );
}

#[tokio::test]
async fn a_superseded_memory_is_never_injected() {
    let mut old = active_fact("m-old", "durable kanban tasks live in kanban.db");
    old.supersede("m-new", 1_000);
    let e = enricher(FakeStore::new(vec![old]), None);
    assert!(
        e.enrich(&Session::new("s"), "where do kanban tasks live?", &[])
            .await
            .is_none()
    );
}

#[tokio::test]
async fn empty_store_yields_no_prefix() {
    let e = enricher(FakeStore::new(Vec::new()), None);
    assert!(e.enrich(&Session::new("s"), "hello", &[]).await.is_none());
}

#[tokio::test]
async fn store_failure_is_swallowed_not_propagated() {
    let mut store = FakeStore::new(Vec::new());
    store.fail_list = true;
    let e = enricher(store, None);
    assert!(e.enrich(&Session::new("s"), "hello", &[]).await.is_none());
}

#[tokio::test]
async fn database_pins_are_only_recalled_when_relevant() {
    let e = enricher(
        FakeStore::new(vec![pinned_memory("prefers concise answers about kanban")]),
        None,
    );
    assert!(
        e.enrich(&Session::new("s"), "unrelated", &[])
            .await
            .is_none()
    );
    let injection = e.enrich(&Session::new("s"), "kanban", &[]).await.unwrap();
    assert!(
        injection
            .recall
            .unwrap()
            .contains("prefers concise answers")
    );
    assert!(injection.used.pinned.is_empty());
}

#[tokio::test]
async fn only_injected_ids_are_marked_used() {
    let store = FakeStore::new(vec![active_fact("m-1", "kanban tasks live in kanban.db")]);
    let used = store.used.clone();
    let e = enricher(store, None);
    e.enrich(&Session::new("s"), "kanban tasks?", &[])
        .await
        .expect("injects");
    // mark_used is spawned off the reply path; give it a beat.
    tokio::task::yield_now().await;
    for _ in 0..50 {
        if !used.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let calls = used.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], vec!["m-1".to_string()]);
}

#[tokio::test]
async fn few_candidates_skip_the_aux_screen() {
    // An aux whose reply would keep nothing: if it were consulted, recall
    // would fall back — but with ≤ limit candidates it must not be called,
    // so the hit injects directly.
    let store = FakeStore::new(vec![active_fact("m-1", "kanban tasks live in kanban.db")]);
    let aux: Arc<dyn LlmClient> = Arc::new(FakeAux {
        reply: Err(anyhow::anyhow!("aux must not be consulted")),
    });
    let e = enricher(store, Some(aux));
    let injection = e
        .enrich(&Session::new("s"), "kanban tasks?", &[])
        .await
        .expect("injects");
    assert!(injection.joined().contains("kanban.db"));
}

fn crowded_store() -> FakeStore {
    // More matching candidates than the limit, so the aux screen engages.
    let memories: Vec<Memory> = (0..8)
        .map(|i| {
            active_fact(
                &format!("m-{i}"),
                &format!("kanban fact number {i} about kanban tasks"),
            )
        })
        .collect();
    FakeStore::new(memories)
}

#[tokio::test]
async fn aux_selection_narrows_recall() {
    let aux: Arc<dyn LlmClient> = Arc::new(FakeAux {
        reply: Ok(r#"{"keep":[{"id":"m-3","line":"the third kanban fact"}]}"#.into()),
    });
    let e = enricher(crowded_store(), Some(aux));
    let injection = e
        .enrich(&Session::new("s"), "kanban tasks?", &[])
        .await
        .expect("injects");
    let s = injection.joined();
    assert!(s.contains("the third kanban fact"), "condensation applied");
    assert_eq!(
        s.matches("kanban fact number").count(),
        0,
        "unselected candidates dropped"
    );
}

#[tokio::test]
async fn aux_failure_falls_back_to_lexical_top() {
    let aux: Arc<dyn LlmClient> = Arc::new(FakeAux {
        reply: Err(anyhow::anyhow!("aux down")),
    });
    let e = enricher(crowded_store(), Some(aux));
    let injection = e
        .enrich(&Session::new("s"), "kanban tasks?", &[])
        .await
        .expect("injects");
    let bullets = injection
        .joined()
        .lines()
        .filter(|l| l.starts_with("- ["))
        .count();
    assert_eq!(bullets, 5, "lexical top recall_limit inject");
}

#[tokio::test]
async fn aux_invalid_json_falls_back() {
    let aux: Arc<dyn LlmClient> = Arc::new(FakeAux {
        reply: Ok("sorry, I can't help with that".into()),
    });
    let e = enricher(crowded_store(), Some(aux));
    let injection = e
        .enrich(&Session::new("s"), "kanban tasks?", &[])
        .await
        .expect("injects");
    let bullets = injection
        .joined()
        .lines()
        .filter(|l| l.starts_with("- ["))
        .count();
    assert_eq!(bullets, 5);
}

// ---- aux reply validation ----

fn hit(id: &str, content: &str) -> ScoredMemory {
    let mut memory = Memory::new(MemoryKind::Fact, content);
    memory.id = id.to_string();
    ScoredMemory { memory, score: 1.0 }
}

const LIMIT: usize = 5;

#[test]
fn aux_selection_keeps_valid_ids_and_drops_fabrications() {
    let hits = vec![hit("mem-a", "fact a"), hit("mem-b", "fact b")];
    let reply = r#"{"keep":[{"id":"mem-b"},{"id":"mem-forged"},{"id":"mem-b"}]}"#;
    let kept = apply_aux_selection(&hits, reply, LIMIT).unwrap();
    // Fabricated id dropped, duplicate deduped, order = aux's ranking.
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].memory.id, "mem-b");
    assert_eq!(kept[0].memory.content, "fact b", "no line → verbatim");
}

#[test]
fn aux_selection_applies_bounded_condensations_only() {
    let hits = vec![hit("mem-a", "a very long original fact")];
    let reply = r#"{"keep":[{"id":"mem-a","line":"short version"}]}"#;
    let kept = apply_aux_selection(&hits, reply, LIMIT).unwrap();
    assert_eq!(kept[0].memory.content, "short version");

    // A runaway condensation falls back to the verbatim memory.
    let long = "x".repeat(AUX_RECALL_LINE_MAX + 1);
    let reply = format!(r#"{{"keep":[{{"id":"mem-a","line":"{long}"}}]}}"#);
    let kept = apply_aux_selection(&hits, &reply, LIMIT).unwrap();
    assert_eq!(kept[0].memory.content, "a very long original fact");
}

#[test]
fn aux_selection_tolerates_fenced_reply_and_caps_at_limit() {
    let hits: Vec<ScoredMemory> = (0..10).map(|i| hit(&format!("m{i}"), "f")).collect();
    let ids: Vec<String> = (0..10).map(|i| format!(r#"{{"id":"m{i}"}}"#)).collect();
    let reply = format!("```json\n{{\"keep\":[{}]}}\n```", ids.join(","));
    let kept = apply_aux_selection(&hits, &reply, LIMIT).unwrap();
    assert_eq!(kept.len(), LIMIT);
}

#[test]
fn aux_selection_unusable_replies_return_none() {
    let hits = vec![hit("mem-a", "fact a")];
    // Empty keep is indistinguishable from a lazy reply → fall back.
    assert!(apply_aux_selection(&hits, r#"{"keep":[]}"#, LIMIT).is_none());
    assert!(apply_aux_selection(&hits, "no json here", LIMIT).is_none());
    assert!(apply_aux_selection(&hits, "} {", LIMIT).is_none());
    assert!(apply_aux_selection(&hits, r#"{"keep":[{"id":"other"}]}"#, LIMIT).is_none());
}

// ---- block rendering ----

fn scored(content: &str, score: f64) -> ScoredMemory {
    ScoredMemory {
        memory: Memory::new(MemoryKind::Fact, content),
        score,
    }
}

#[test]
fn empty_recall_renders_nothing() {
    assert!(render_recalled_memory_block(&[], now()).is_none());
}

#[test]
fn recall_block_has_markers_caveat_and_tagged_lines() {
    let block =
        render_recalled_memory_block(&[scored("komo uses a DDD layout", 3.0)], now()).unwrap();
    assert!(block.starts_with(RECALL_OPEN));
    assert!(block.trim_end().ends_with(RECALL_CLOSE));
    assert!(block.contains("untrusted background facts"));
    assert!(block.contains("- [fact/inferred/global] komo uses a DDD layout"));
}

/// A claim that came out of a fetched page reads exactly like one the user
/// made — so the line has to say which it is.
#[test]
fn recall_block_marks_a_tool_derived_memory_as_one() {
    let now = 10_000 * 86_400;
    let mut hit = scored("the docs say komo prefers tabs", 2.0);
    hit.memory.provenance = MemoryProvenance::Tool;
    let block = render_recalled_memory_block(&[hit], now).unwrap();
    assert!(block.contains("/from-tool"), "{block}");
}

/// A fact and how much to trust it are different things, and an injected
/// line has to carry both.
#[test]
fn recall_block_marks_supported_and_stale_memories() {
    let now = now();
    // Corroborated on two independent occasions.
    let mut supported = scored("user prefers rebase", 2.0);
    supported
        .memory
        .record_evidence("s-1", "s-1", EvidenceRelation::Supports, "a", now);
    supported
        .memory
        .record_evidence("s-2", "s-2", EvidenceRelation::Supports, "b", now);
    let block = render_recalled_memory_block(&[supported], now).unwrap();
    assert!(block.contains("/supported]"), "{block}");
    // Checked on the bullet, not the block: the header mentions `stale` by
    // design, to say what the marker means.
    let bullet = block.lines().find(|l| l.starts_with("- [")).unwrap();
    assert!(!bullet.contains("stale"), "{bullet}");

    // Nothing has vouched for this one in a very long time.
    let mut stale = scored("user prefers tabs", 2.0);
    stale.memory.created_at = now - (MEMORY_STALE_AFTER_DAYS + 33) * 86_400;
    let block = render_recalled_memory_block(&[stale], now).unwrap();
    assert!(block.contains("/stale:213d]"), "{block}");
    // …and the header has to say what to do about it, or the marker is noise.
    assert!(block.contains("check with the user"), "{block}");
}

/// An ordinary memory earns no markers: they exist to flag the exceptions,
/// and tagging everything would cost bytes on every turn for no signal.
#[test]
fn an_ordinary_memory_gets_no_markers() {
    let now = now();
    let block =
        render_recalled_memory_block(&[scored("komo is written in Rust", 1.0)], now).unwrap();
    assert!(block.contains("- [fact/inferred/global]"), "{block}");
}

/// Screening decides what changes the turn, so it has to see what the turn is
/// about — not just the last sentence of it.
#[test]
fn the_aux_screen_prompt_carries_the_conversation_goal() {
    let history = vec![
        Message::user("I'm migrating the billing service off PHP".to_string()),
        Message::assistant("Which parts are moving first?".to_string()),
    ];
    let hits = vec![hit("mem-a", "user is rewriting billing in Go")];
    let prompt = aux_recall_prompt("start with the invoice endpoint", &history, &hits, 5);
    assert!(prompt.contains("migrating the billing service"), "{prompt}");
    assert!(prompt.contains("Which parts are moving first?"), "{prompt}");
    assert!(
        prompt.contains("start with the invoice endpoint"),
        "{prompt}"
    );
    // The criterion is usefulness, not topical relatedness.
    assert!(
        prompt.contains("would change what the assistant does next"),
        "{prompt}"
    );
}

/// Only the tail is shown, and each line is clipped: screening must not turn
/// into re-reading the transcript every turn.
#[test]
fn the_aux_screen_prompt_bounds_the_history_it_shows() {
    let history: Vec<Message> = (0..20)
        .map(|i| Message::user(format!("message number {i}")))
        .collect();
    let rendered = render_recent(&history);
    assert_eq!(rendered.lines().count(), AUX_HISTORY_MESSAGES);
    assert!(rendered.contains("message number 19"), "the newest is kept");
    assert!(
        !rendered.contains("message number 13"),
        "older ones are dropped"
    );

    let long = vec![Message::user("x".repeat(AUX_HISTORY_LINE_MAX + 500))];
    let rendered = render_recent(&long);
    assert!(rendered.chars().count() < AUX_HISTORY_LINE_MAX + 20);
}

#[test]
fn recall_block_tags_source_when_present() {
    let mut s = scored("durable tasks live in kanban.db", 2.0);
    s.memory.source = "cli-session-1".into();
    let block = render_recalled_memory_block(&[s], now()).unwrap();
    assert!(block.contains("/source:cli-session-1]"));
}

#[test]
fn recall_block_respects_budget_whole_lines_only() {
    let big: Vec<ScoredMemory> = (0..200)
        .map(|i| {
            scored(
                &format!("recalled fact number {i} stated in a full sentence"),
                1.0,
            )
        })
        .collect();
    let block = render_recalled_memory_block(&big, now()).unwrap();
    let bullets: Vec<&str> = block.lines().filter(|l| l.starts_with("- [")).collect();
    let bullet_bytes: usize = bullets.iter().map(|l| l.len() + 1).sum();
    assert!(bullet_bytes <= RECALLED_MEMORY_BUDGET);
    assert!(!bullets.is_empty() && bullets.len() < 200);
    for line in &bullets {
        assert!(line.contains("recalled fact number"));
    }
}
