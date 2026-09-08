use std::collections::HashSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::session::ChannelPeer;

/// A long-term memory: a durable fact, preference, or note about the user, a
/// project, a person, or a decision. Memories are governed (status/confidence)
/// and scoped (where they may surface) so the agent can be injected with a
/// conservative profile (L1), recall relevant facts (L3), and let the user
/// curate the full library (L2). See `docs/personal-agent-roadmap.md`.
///
/// Not `Eq`: `embedding` is `Vec<f32>`, and float equality is not an
/// equivalence relation. Nothing keys a collection on a whole `Memory` — `id`
/// is the identity — so `PartialEq` (assertions, dedup checks) is enough.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub kind: MemoryKind,
    pub content: String,

    /// Lifecycle state. Automated extraction lands as `Candidate`; only
    /// user-confirmed/written memories become high-confidence `Active`.
    pub status: MemoryStatus,
    /// Whether komo believes this *right now* — a different axis from `status`.
    /// See [`BeliefState`].
    pub belief: BeliefState,
    /// How much the memory can be trusted, by origin.
    pub confidence: MemoryConfidence,
    /// **Who** the claim came from — the user, or something a tool returned.
    /// See [`MemoryProvenance`]: this is the one axis that decides whether a
    /// claim may promote on its own accumulated support.
    pub provenance: MemoryProvenance,
    /// 0–100 ranking weight; ties broken by recency. Default 50.
    pub importance: i32,
    /// Retired storage field. L1 is sourced exclusively from MEMORY.md.
    pub pinned: bool,

    /// Where this memory may surface. Scope is enforced at the query layer, not
    /// the render layer, so a channel-scoped memory never leaks into another
    /// chat. See [`MemoryContext`].
    pub scope: MemoryScope,

    /// Session this memory was distilled from (`telegram:{chat_id}`, a cli
    /// session uuid, …). Empty = written outside any session.
    pub source: String,
    /// Content-derived dedup key set on automated extraction (FNV-1a over the
    /// normalized content), so re-reviewing a session never duplicates it.
    pub source_message_id: String,

    pub created_at: i64,
    pub updated_at: i64,
    /// Optional governance TTL: a unix timestamp past which the memory is
    /// treated as stale and hidden from recall. `None` = never expires.
    pub expires_at: Option<i64>,
    /// Last time this memory surfaced in recall, for usage-based
    /// retention signals. `None` = never used.
    pub last_used_at: Option<i64>,

    // ── truth signals ────────────────────────────────────────────────────────
    //
    // Deliberately separate from the usage signals below. `recall_count` proves
    // a memory keeps being *relevant*; only these prove it is *true*. Promotion
    // reads these, retention reads those — conflating them is what let a wrong
    // memory promote itself by being repeatedly retrieved.
    /// Independent occasions on which the user said something supporting this
    /// claim. Independence is per learning occasion — see
    /// [`Memory::record_evidence`].
    pub support_count: i64,
    /// Independent occasions on which the user said something conflicting with
    /// it. Any unresolved contradiction is what `Contested` expresses.
    pub contradiction_count: i64,
    /// When the user last explicitly confirmed this (triage promote, or an
    /// unambiguous restatement). The strongest truth signal there is, and the
    /// freshness that matters for a memory about to drive an action.
    pub last_confirmed_at: Option<i64>,
    /// Id of the memory that replaced this one, when `belief` is `Superseded`.
    /// Empty otherwise.
    pub superseded_by: String,
    /// Capped, summarized provenance: *why* komo believes this. See [`Evidence`].
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    /// How many times this memory has surfaced in L3 recall — a **utility**
    /// signal, not a truth signal. It says the retriever keeps finding this
    /// relevant, which is exactly what retention should be decided on, and is
    /// exactly what promotion must *not* be decided on: a wrong memory that
    /// happens to be relevant to a recurring question would otherwise promote
    /// itself. See [`dream_verdict`].
    pub recall_count: i64,
    /// L2-normalized semantic vector of [`content`](Self::content), or empty
    /// when none has been computed yet (no embedding backend configured, or the
    /// backfill has not reached this memory). This is what lets a Chinese
    /// question recall an English memory — see [`super::embedding`].
    ///
    /// Filled in the background, never on the write path: a memory is always
    /// usable lexically the moment it is saved.
    #[serde(default)]
    pub embedding: Vec<f32>,
    /// Which model produced [`embedding`](Self::embedding). Vectors from
    /// different models live in incomparable spaces, so recall only uses an
    /// embedding whose model matches the *current* backend; a mismatch is
    /// treated as "not embedded yet" and re-embedded by the backfill.
    #[serde(default)]
    pub embedding_model: String,
}

/// Default ranking weight for a new memory.
pub const DEFAULT_IMPORTANCE: i32 = 50;

/// How long a memory can go unvouched-for before an injected line flags it.
///
/// Six months: long enough that a settled preference is not nagged about every
/// turn, short enough that a stale one is questioned before it drives an action.
pub const MEMORY_STALE_AFTER_DAYS: i64 = 180;

impl Memory {
    /// A new memory with conservative defaults: `Active` status, `Inferred`
    /// confidence, global scope, not pinned. Callers (the `memory` tool, the
    /// reviewer) override status/confidence/scope to match their trust level.
    pub fn new(kind: MemoryKind, content: impl Into<String>) -> Self {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        Self {
            id: format!("mem-{}", uuid::Uuid::now_v7()),
            kind,
            content: content.into(),
            status: MemoryStatus::Active,
            belief: BeliefState::Current,
            confidence: MemoryConfidence::Inferred,
            // The permissive default is the honest one for everything komo
            // wrote before provenance existed: it was all extracted from
            // conversations. A tool-derived claim is marked by whoever creates
            // it, and that is the only writer that knows.
            provenance: MemoryProvenance::User,
            importance: DEFAULT_IMPORTANCE,
            pinned: false,
            scope: MemoryScope::Global,
            source: String::new(),
            source_message_id: String::new(),
            created_at: now,
            updated_at: now,
            expires_at: None,
            last_used_at: None,
            // Zero, not one: the count means "recorded evidence", so the
            // observation that created a memory is registered by whoever creates
            // it (`record_evidence`), never assumed here.
            support_count: 0,
            contradiction_count: 0,
            last_confirmed_at: None,
            superseded_by: String::new(),
            evidence: Vec::new(),
            recall_count: 0,
            embedding: Vec::new(),
            embedding_model: String::new(),
        }
    }

    /// This memory's vector, but only if it was produced by `model` — vectors
    /// from another model are not comparable, so they read as absent. `None`
    /// also when nothing has been embedded yet.
    pub fn embedding_for<'a>(&'a self, model: &str) -> Option<&'a [f32]> {
        (!self.embedding.is_empty() && self.embedding_model == model)
            .then_some(self.embedding.as_slice())
    }

    /// Whether this memory has expired as of `now` (a unix timestamp).
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|e| e <= now)
    }

    // Governance transitions (the triage ladder). Shared by the CLI, the api
    // channel, and the `memory` tool so the semantics can never drift between
    // surfaces.

    /// Promote a candidate to an active, **confirmed** memory (a human vouched
    /// for it — unlike dreaming's evidence-driven promote, which caps at
    /// inferred).
    ///
    /// A human vouching *is* an explicit confirmation, so this also stamps
    /// `last_confirmed_at` and clears any contest: whatever conflict was
    /// outstanding, the operator has now ruled on it.
    pub fn promote(&mut self, now: i64) {
        self.status = MemoryStatus::Active;
        self.confidence = MemoryConfidence::Confirmed;
        self.last_confirmed_at = Some(now);
        self.belief = BeliefState::Current;
        self.superseded_by.clear();
        self.updated_at = now;
    }

    /// Record an observation bearing on this memory. Returns whether it counted.
    ///
    /// **Independence is per learning occasion**, not per session. One extraction
    /// pass over one batch of runs is one occasion, named here by its canonical
    /// id ([`Occasion::key`]); a retry or a re-extraction of the same batch names
    /// the same occasion and is dropped, which is what makes
    /// `support_count` mean "N separate occasions" instead of "N sentences" — a
    /// user restating a preference three times in one conversation must not look
    /// like three independent confirmations. Session alone cannot be that unit
    /// any more: the operator's private conversations are all one permanent home
    /// session, so keying on it would mean support never accumulates there at all.
    ///
    /// This dedupes on the canonical name alone. A caller holding the whole
    /// [`Occasion`] — one that must also recognize evidence founded under
    /// another run of its own batch — asks [`Memory::witnessed_on`] first.
    ///
    /// Legacy evidence stored before this field existed carries an empty
    /// occasion and is keyed by its session instead ([`Evidence::occasion_key`]).
    /// A session id never collides with a run id, so old evidence counts as one
    /// occasion and every later pass counts separately.
    ///
    /// The evidence *list* is capped at [`EVIDENCE_CAP`] (most recent kept) while
    /// the counts keep rising, so a long-lived memory cannot grow its row without
    /// bound. Consequence, deliberately accepted: once the cap is reached, an
    /// occasion evicted from the list can be counted again. Every threshold that
    /// reads these counts sits far below the cap, so this cannot change a verdict.
    pub fn record_evidence(
        &mut self,
        session: &str,
        occasion: &str,
        relation: EvidenceRelation,
        excerpt: &str,
        now: i64,
    ) -> bool {
        if self.evidence.iter().any(|e| e.occasion_key() == occasion) {
            return false;
        }
        match relation {
            EvidenceRelation::Supports => self.support_count += 1,
            EvidenceRelation::Contradicts => self.contradiction_count += 1,
        }
        self.evidence.push(Evidence {
            session: session.to_string(),
            occasion: occasion.to_string(),
            observed_at: now,
            relation,
            excerpt: truncate_excerpt(excerpt),
        });
        if self.evidence.len() > EVIDENCE_CAP {
            let drop = self.evidence.len() - EVIDENCE_CAP;
            self.evidence.drain(..drop);
        }
        self.updated_at = now;
        true
    }

    /// Whether this memory already carries evidence from `occasion` — under its
    /// canonical name or under any other run of the same pass.
    ///
    /// [`Occasion`] says why the second half matters: the `memory` tool and the
    /// review of the turn it ran in name the same pass differently, and only the
    /// set knows they are one.
    pub fn witnessed_on(&self, occasion: &Occasion) -> bool {
        self.evidence
            .iter()
            .any(|e| occasion.covers(e.occasion_key()))
    }

    /// Something now conflicts with this memory and nothing has resolved which
    /// wins. It stops being injected until a confirmation or triage rules on it.
    pub fn contest(&mut self, now: i64) {
        self.belief = BeliefState::Contested;
        self.updated_at = now;
    }

    /// This memory was true and has been replaced by `by`. Kept as history
    /// rather than deleted — "what did I use to prefer" is a real question, and
    /// the replacement's provenance points back here.
    pub fn supersede(&mut self, by: &str, now: i64) {
        self.belief = BeliefState::Superseded;
        self.superseded_by = by.to_string();
        self.updated_at = now;
    }

    /// When this memory's content was last *vouched for*: an explicit
    /// confirmation, else the most recent recorded observation, else the day it
    /// was created.
    ///
    /// Deliberately not `updated_at`, which is an edit clock — it also moves when
    /// importance is retuned or the belief is contested, neither of which says
    /// anything about whether the claim still holds. This is the clock that
    /// answers "how long since anyone actually backed this up", which is what a
    /// memory about to drive an action should be judged on.
    pub fn vouched_at(&self) -> i64 {
        let newest_evidence = self.evidence.iter().map(|e| e.observed_at).max();
        [self.last_confirmed_at, newest_evidence]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(self.created_at)
    }

    /// When something last conflicted with this memory and nothing has ruled on
    /// it since — `None` when nothing conflicts, or when a confirmation came
    /// after the conflict (the operator has since decided the question).
    ///
    /// Read off the contradicting evidence, falling back to `updated_at` for a
    /// conflict that left no entry: `contest`/`supersede` write none, and the
    /// oldest entries drain out of the capped list. The fallback is an edit
    /// clock, so it can only ever read *later* than the real refutation — which
    /// delays retiring a refuted claim, never hastens it.
    pub fn unresolved_refutation_at(&self) -> Option<i64> {
        if self.contradiction_count == 0 && self.belief == BeliefState::Current {
            return None;
        }
        let refuted_at = self
            .evidence
            .iter()
            .filter(|e| e.relation == EvidenceRelation::Contradicts)
            .map(|e| e.observed_at)
            .max()
            .unwrap_or(self.updated_at);
        if self.last_confirmed_at.is_some_and(|c| c >= refuted_at) {
            return None;
        }
        Some(refuted_at)
    }

    /// Whether nothing has vouched for this memory in [`MEMORY_STALE_AFTER_DAYS`].
    /// Injected memories say so, because acting on a long-unconfirmed preference
    /// is how an assistant gets corrected.
    pub fn is_stale(&self, now: i64) -> bool {
        (now - self.vouched_at()).max(0) / 86_400 >= MEMORY_STALE_AFTER_DAYS
    }

    /// Whether independent occasions corroborate this memory — the same bar
    /// promotion uses, so what an injected line claims and what dreaming acts on
    /// cannot drift apart.
    pub fn is_supported(&self) -> bool {
        self.last_confirmed_at.is_some()
            || (self.support_count >= DREAM_MIN_SUPPORT && self.provenance.promotable_on_support())
    }

    /// Whether this memory may enter a prompt **unasked** (L1 pinned or L3
    /// recall).
    ///
    /// Only a `Current` belief qualifies. Injecting a contested memory would hand
    /// the model both sides of an unresolved conflict and let it pick one, which
    /// is the specific failure the state exists to prevent; injecting a
    /// superseded one would assert something known to be out of date.
    ///
    /// This gates *injection*, not retrieval. An explicit `memory search` still
    /// returns these — the model cannot help resolve a conflict it is forbidden
    /// to see, and the rendered line names the state.
    pub fn is_injectable(&self) -> bool {
        self.belief == BeliefState::Current
    }

    /// Reject a memory so it never surfaces in recall or injection.
    pub fn reject(&mut self, now: i64) {
        self.status = MemoryStatus::Rejected;
        self.updated_at = now;
    }
}

// ── kind ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    Profile,
    Preference,
    Feedback,
    Project,
    Person,
    Fact,
    Decision,
    Reference,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Preference => "preference",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Person => "person",
            Self::Fact => "fact",
            Self::Decision => "decision",
            Self::Reference => "reference",
        }
    }
}

/// Parse a kind string, accepting both the current vocabulary and the legacy
/// markdown values (`user` → `Profile`). Unknown → `Fact` (the most neutral
/// bucket).
pub fn parse_memory_kind(value: &str) -> MemoryKind {
    match value.trim() {
        "profile" | "user" => MemoryKind::Profile,
        "preference" => MemoryKind::Preference,
        "feedback" => MemoryKind::Feedback,
        "project" => MemoryKind::Project,
        "person" => MemoryKind::Person,
        "decision" => MemoryKind::Decision,
        "reference" => MemoryKind::Reference,
        _ => MemoryKind::Fact,
    }
}

// ── status ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryStatus {
    Candidate,
    Active,
    Archived,
    Rejected,
}

impl MemoryStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Rejected => "rejected",
        }
    }
}

pub fn parse_memory_status(value: &str) -> MemoryStatus {
    match value.trim() {
        "candidate" => MemoryStatus::Candidate,
        "archived" => MemoryStatus::Archived,
        "rejected" => MemoryStatus::Rejected,
        _ => MemoryStatus::Active,
    }
}

// ── belief ────────────────────────────────────────────────────────────────────

/// Whether komo believes a memory right now.
///
/// A **separate axis from [`MemoryStatus`]**, on purpose. Status answers "where is
/// this in the triage pipeline" (candidate → active → archived/rejected) and is
/// what the CLI, the operator surfaces and recall eligibility are built on. Belief
/// answers "is this true at the moment", which no consumer of `status` has an
/// opinion about: an `Active` memory the user just contradicted is still active —
/// somebody curated it — but must stop being asserted until the conflict resolves.
///
/// Folding the two into one column was the tempting shortcut. It would have made
/// every reader of `status` handle a truth value, and every truth transition
/// clobber a governance decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BeliefState {
    /// Believed, and safe to inject.
    Current,
    /// A later observation conflicts with it, and nothing has resolved which
    /// wins. Never injected unasked — see [`Memory::is_injectable`].
    Contested,
    /// Was true, has been replaced by a newer fact (`Memory::superseded_by`).
    Superseded,
}

impl BeliefState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Contested => "contested",
            Self::Superseded => "superseded",
        }
    }
}

/// Parse a belief state. Unknown → `Current`, which is what every row written
/// before this column existed means.
pub fn parse_belief_state(value: &str) -> BeliefState {
    match value.trim() {
        "contested" => BeliefState::Contested,
        "superseded" => BeliefState::Superseded,
        _ => BeliefState::Current,
    }
}

// ── evidence ──────────────────────────────────────────────────────────────────

/// One observation bearing on a memory, kept so governance is auditable rather
/// than merely asserted.
///
/// **Not an episode store.** The conversation this came from already lives in the
/// run ledger and transcript; this is a capped, summarized pointer at *why* komo
/// believes something, so a `support_count` of 3 can be inspected instead of
/// trusted. `state.db` is disposable, so the excerpt is the part that survives
/// its deletion — which is also why it is bounded rather than verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// Session the observation came from. Provenance and display only — the
    /// operator's private conversations are all one permanent home session, so
    /// this cannot be the unit of independence.
    pub session: String,
    /// The learning occasion that produced it, by its canonical name — see
    /// [`Occasion`], which is the whole batch of runs one pass read and not
    /// necessarily the run these words were said in. The unit of independence:
    /// two statements gathered by one pass are one observation. Empty on
    /// evidence stored before this field existed; see
    /// [`Evidence::occasion_key`].
    #[serde(default)]
    pub occasion: String,
    pub observed_at: i64,
    pub relation: EvidenceRelation,
    /// Short quote of what was actually said, capped at
    /// [`EVIDENCE_EXCERPT_MAX`] characters.
    pub excerpt: String,
}

impl Evidence {
    /// What this observation is deduplicated on: its occasion, falling back to
    /// its session for rows written before occasions existed.
    pub fn occasion_key(&self) -> &str {
        match self.occasion.is_empty() {
            true => &self.session,
            false => &self.occasion,
        }
    }
}

/// One learning occasion, as the set of runs it read.
///
/// An occasion is *a pass*, not a turn: a sweep batches up to `LEARN_BATCH_CAP`
/// runs into one extraction, and everything that pass gathered is one
/// observation. Its identity therefore has to be the whole batch. New evidence
/// is stamped with a single canonical name — the oldest run in it, ids being
/// UUIDv7 and so sorting by time — and the rest of the set is what recognizes
/// evidence *another* writer founded inside this same pass.
///
/// That second half is the whole reason this is a set. The `memory` tool founds
/// evidence with the turn's own run id, and that turn is usually somewhere in
/// the middle of the batch whose review reads it later. Compare canonical names
/// alone and the review of a turn would "support" the claim the model saved
/// during it — one occasion counted twice, which is exactly the
/// self-corroboration [`Memory::record_evidence`] exists to prevent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Occasion {
    /// Sorted and deduplicated, so `runs[0]` is the canonical key.
    runs: Vec<String>,
}

impl Occasion {
    /// The occasion a pass over these runs is. An empty set names nothing: it
    /// keys as the empty string and covers no evidence.
    pub fn over(runs: impl IntoIterator<Item = String>) -> Self {
        let mut runs: Vec<String> = runs.into_iter().collect();
        runs.sort();
        runs.dedup();
        Self { runs }
    }

    /// An occasion of exactly one id — an explicit `memory` save, whose occasion
    /// is the turn it was made in.
    pub fn single(id: impl Into<String>) -> Self {
        Self::over([id.into()])
    }

    /// The name evidence recorded on this occasion carries.
    pub fn key(&self) -> &str {
        self.runs.first().map(String::as_str).unwrap_or_default()
    }

    /// Whether an existing [`Evidence::occasion_key`] belongs to this occasion.
    pub fn covers(&self, key: &str) -> bool {
        self.runs.iter().any(|run| run == key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceRelation {
    Supports,
    Contradicts,
}

/// How many evidence entries one memory retains. Five is well past every
/// threshold that reads the counts, so the cap bounds the row without ever
/// bounding a decision.
pub const EVIDENCE_CAP: usize = 5;

/// Character cap on a stored excerpt. Long enough to recognize what was said,
/// short enough that five of them are not a transcript.
pub const EVIDENCE_EXCERPT_MAX: usize = 200;

/// Trim an excerpt to [`EVIDENCE_EXCERPT_MAX`] characters (not bytes — the vault
/// and these memories are largely CJK).
fn truncate_excerpt(excerpt: &str) -> String {
    let excerpt = excerpt.trim();
    if excerpt.chars().count() <= EVIDENCE_EXCERPT_MAX {
        return excerpt.to_string();
    }
    excerpt.chars().take(EVIDENCE_EXCERPT_MAX).collect()
}

// ── confidence ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryConfidence {
    Extracted,
    Inferred,
    Confirmed,
    UserWritten,
}

impl MemoryConfidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Extracted => "extracted",
            Self::Inferred => "inferred",
            Self::Confirmed => "confirmed",
            Self::UserWritten => "user_written",
        }
    }
}

/// Where a claim came from, which is not the same question as how much it is
/// believed ([`MemoryConfidence`]) or whether it is believed now
/// ([`BeliefState`]).
///
/// A turn reads web pages, files and MCP servers, and every one of those is
/// content **nobody in the conversation wrote**. A page that says "the user
/// prefers X" is a page saying so, not the user saying so — and an extractor
/// reading the turn afterwards cannot tell the difference from the claim alone.
/// Left unmarked, such a claim accumulates support like any other and promotes
/// itself into the prompt of every later turn: a durable instruction planted by
/// whatever the agent happened to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemoryProvenance {
    /// The user said it, or the operator wrote it.
    #[default]
    User,
    /// It came out of tool output. Recordable, retrievable, and **never
    /// promotable by accumulation** — only the user confirming it can make it a
    /// memory komo asserts on its own (`dream_verdict`).
    Tool,
}

impl MemoryProvenance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Tool => "tool",
        }
    }

    /// Whether a claim from here may promote on accumulated support alone.
    pub fn promotable_on_support(&self) -> bool {
        matches!(self, Self::User)
    }
}

/// Parse a provenance. Unknown → `User`, which is what every row written before
/// the column existed means — they were all extracted from conversations.
pub fn parse_memory_provenance(value: &str) -> MemoryProvenance {
    match value.trim() {
        "tool" => MemoryProvenance::Tool,
        _ => MemoryProvenance::User,
    }
}

pub fn parse_memory_confidence(value: &str) -> MemoryConfidence {
    match value.trim() {
        "inferred" => MemoryConfidence::Inferred,
        "confirmed" => MemoryConfidence::Confirmed,
        "user_written" => MemoryConfidence::UserWritten,
        _ => MemoryConfidence::Extracted,
    }
}

// ── scope ─────────────────────────────────────────────────────────────────────

/// Where a memory may surface. Serialized to the DB as a `(scope_type,
/// scope_key)` pair so it can be filtered in queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryScope {
    /// Visible everywhere.
    Global,
    /// Tied to a project (CLI workspace key).
    Project(String),
    /// Tied to a chat channel (`{platform}:{chat_id}`).
    Channel { platform: String, chat_id: String },
    /// Tied to a single session id.
    Session(String),
}

impl MemoryScope {
    pub fn type_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project(_) => "project",
            Self::Channel { .. } => "channel",
            Self::Session(_) => "session",
        }
    }

    /// The opaque key stored alongside `type_str`. Empty for `Global`.
    pub fn key(&self) -> String {
        match self {
            Self::Global => String::new(),
            Self::Project(p) => p.clone(),
            Self::Channel { platform, chat_id } => format!("{platform}:{chat_id}"),
            Self::Session(id) => id.clone(),
        }
    }

    /// Rebuild a scope from its serialized `(type, key)` pair. Unknown type or a
    /// malformed channel key degrades to `Global` (fail safe — never widen).
    pub fn from_parts(scope_type: &str, scope_key: &str) -> Self {
        match scope_type.trim() {
            "project" if !scope_key.is_empty() => Self::Project(scope_key.to_string()),
            "channel" => match scope_key.split_once(':') {
                Some((platform, chat_id)) => Self::Channel {
                    platform: platform.to_string(),
                    chat_id: chat_id.to_string(),
                },
                None => Self::Global,
            },
            "session" if !scope_key.is_empty() => Self::Session(scope_key.to_string()),
            _ => Self::Global,
        }
    }
}

/// The scopes a memory may be drawn from for the current turn. `Global` is
/// always allowed; a turn with a correspondent adds that `Channel` and its own
/// `Session`. Scope is decided here, before any query, so a query can never
/// widen beyond what the context permits.
///
/// `Channel` scope is what keeps a fact disclosed in one person's DM from
/// surfacing in someone else's chat while still following that person across
/// sessions — which only means anything when the address identifies a **durable
/// correspondent** rather than one conversation. That used to need a
/// `is_durable_channel` exception, because the local surfaces were modelled as a
/// channel (`api:<uuid>`) whose "correspondent" was a fresh uuid per
/// conversation, so channel-scoping there wrote memories unrecallable from the
/// next turn. A local turn now simply has no correspondent, and the exception is
/// gone with the prefix that forced it.
#[derive(Debug, Clone)]
pub struct MemoryContext {
    pub allowed_scopes: Vec<MemoryScope>,
}

impl MemoryContext {
    /// Derive the allowed scopes for a turn: its session, its correspondent's
    /// channel when it has one, and always `Global`. (Project scope is wired
    /// separately once the workspace key is known.)
    pub fn new(session_id: &str, channel: Option<&ChannelPeer>) -> Self {
        let mut allowed_scopes = vec![MemoryScope::Global];
        if let Some(peer) = channel {
            allowed_scopes.push(MemoryScope::Channel {
                platform: peer.platform.clone(),
                chat_id: peer.peer_id.clone(),
            });
        }
        allowed_scopes.push(MemoryScope::Session(session_id.to_string()));
        Self { allowed_scopes }
    }

    /// A turn with no correspondent — every local surface, and komo's own
    /// sweeps.
    pub fn local(session_id: &str) -> Self {
        Self::new(session_id, None)
    }

    /// The scope an automated write from this context should carry: the
    /// correspondent's channel when there is one, else global. (Never
    /// `Session`, which would make a memory unrecallable outside the exact
    /// session.)
    pub fn write_scope(&self) -> MemoryScope {
        self.allowed_scopes
            .iter()
            .find(|s| matches!(s, MemoryScope::Channel { .. }))
            .cloned()
            .unwrap_or(MemoryScope::Global)
    }

    /// Whether a memory's scope is permitted in this context.
    pub fn allows(&self, scope: &MemoryScope) -> bool {
        self.allowed_scopes.contains(scope)
    }
}

// ── query / scored result ─────────────────────────────────────────────────────

/// A memory plus its relevance score for a given query.
#[derive(Debug, Clone)]
pub struct ScoredMemory {
    pub memory: Memory,
    pub score: f64,
}

/// The importance + confidence + recency component of [`recall_score`]:
/// importance in `0..~1`, a confidence step, and a 30-day half-life decay on the
/// last update. Separate from the query-match component so the two can be read
/// (and tuned) independently.
fn signal_bonus(memory: &Memory, now: i64) -> f64 {
    let mut bonus = memory.importance as f64 / 100.0; // 0..~1
    bonus += match memory.confidence {
        MemoryConfidence::UserWritten => 0.4,
        MemoryConfidence::Confirmed => 0.3,
        MemoryConfidence::Inferred => 0.1,
        MemoryConfidence::Extracted => 0.0,
    };
    let age_days = (now - memory.updated_at).max(0) as f64 / 86_400.0;
    bonus += 0.5 * (-age_days / 30.0).exp();
    bonus
}

// ── recall (L3) ───────────────────────────────────────────────────────────────

/// Extract lexical terms from text for recall matching, language-agnostically:
/// runs of alphanumeric characters of length ≥ 2 become word terms, and adjacent
/// CJK characters become bigrams (a cheap stand-in for word segmentation, since
/// CJK has no whitespace boundaries). Everything lowercased.
///
/// Token overlap rather than substring containment, because the input is as
/// often a whole user message as a focused keyword query — a substring match
/// would find nothing in the former case.
pub(crate) fn recall_terms(text: &str) -> HashSet<String> {
    let mut terms = HashSet::new();
    let mut word = String::new();
    let mut prev_cjk: Option<char> = None;
    fn flush(word: &mut String, terms: &mut HashSet<String>) {
        if word.chars().count() >= 2 && !is_stopword(word) {
            terms.insert(word.clone());
        }
        word.clear();
    }
    for ch in text.chars() {
        let lc = ch.to_lowercase().next().unwrap_or(ch);
        if is_cjk(ch) {
            if let Some(p) = prev_cjk {
                terms.insert(format!("{p}{lc}"));
            }
            prev_cjk = Some(lc);
            flush(&mut word, &mut terms);
        } else if ch.is_alphanumeric() {
            word.push(lc);
            prev_cjk = None;
        } else {
            flush(&mut word, &mut terms);
            prev_cjk = None;
        }
    }
    flush(&mut word, &mut terms);
    terms
}

/// High-frequency English function words that carry no recall signal — dropping
/// them keeps a memory like "the user likes coffee" from matching any query that
/// merely contains "the". Not exhaustive; just the worst offenders.
fn is_stopword(word: &str) -> bool {
    matches!(
        word,
        "the"
            | "and"
            | "are"
            | "for"
            | "you"
            | "your"
            | "with"
            | "was"
            | "were"
            | "this"
            | "that"
            | "what"
            | "how"
            | "why"
            | "when"
            | "where"
            | "who"
            | "does"
            | "did"
            | "can"
            | "will"
            | "would"
            | "should"
            | "has"
            | "have"
            | "had"
            | "not"
            | "but"
            | "from"
            | "into"
            | "out"
            | "off"
            | "all"
            | "any"
            | "some"
            | "than"
            | "then"
            | "them"
            | "they"
            | "其中"
            | "可以"
            | "如何"
    )
}

/// CJK ranges where per-character (bigram) matching beats whitespace tokens:
/// CJK ideographs (+ Ext A), Hiragana/Katakana, Hangul syllables.
fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
    )
}

/// Minimum cosine similarity for a memory to be *admitted* to recall on
/// semantic grounds alone.
///
/// Calibrated against a real memory library with the multilingual embedding
/// model komo ships against: cross-language paraphrases of a stored fact score
/// 0.54–0.83, while the best match for an unrelated question tops out around
/// 0.43. The floor sits below the true positives and above that noise.
///
/// Deliberately permissive rather than precise. This gate decides *candidate
/// generation*, not what reaches the prompt — candidates past `recall_limit`
/// are screened by the aux relevance pass and capped before injection, so a
/// false positive costs one screening slot while a false negative is the
/// cross-language miss this whole layer exists to fix.
pub const RECALL_SEMANTIC_FLOOR: f32 = 0.45;

/// What a semantic match at full similarity contributes to the recall score,
/// in units of "shared lexical terms". A 0.78-similarity hit scores about two
/// shared terms; a hit just past the floor scores near zero, so lexical
/// evidence still leads when both are present.
const RECALL_SEMANTIC_WEIGHT: f64 = 3.0;

/// One turn's recall query: the lexical terms, plus optionally the embedding
/// that lets it match memories written in another language.
///
/// The embedding is optional at every step — no backend configured, a failed or
/// slow call, or a memory embedded by a different model all degrade to the
/// lexical behavior that predates this struct, never to worse.
#[derive(Debug, Clone)]
pub struct RecallQuery {
    terms: HashSet<String>,
    embedding: Vec<f32>,
    /// Model that produced `embedding`; only memories carrying the same model's
    /// vector are comparable to it.
    model: String,
}

impl RecallQuery {
    /// Terms only — the lexical-only path.
    pub fn lexical(text: &str) -> Self {
        Self {
            terms: recall_terms(text),
            embedding: Vec::new(),
            model: String::new(),
        }
    }

    /// Terms plus an L2-normalized query vector from `model`.
    pub fn semantic(text: &str, embedding: Vec<f32>, model: impl Into<String>) -> Self {
        Self {
            terms: recall_terms(text),
            embedding,
            model: model.into(),
        }
    }

    /// Nothing to match on: no terms *and* no vector. (Terms alone being empty
    /// is not enough — a query of pure punctuation can still match
    /// semantically.)
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.embedding.is_empty()
    }

    /// Whether the semantic arm is actually armed for this query — a lexical
    /// fallback (no backend, or one that errored) is indistinguishable from a
    /// semantic query from the outside, and that is exactly what recall's log
    /// line has to be able to say.
    pub fn has_embedding(&self) -> bool {
        !self.embedding.is_empty()
    }

    /// Cosine similarity to `memory`, or 0.0 when either side lacks a
    /// comparable vector.
    fn similarity(&self, memory: &Memory) -> f32 {
        if self.embedding.is_empty() {
            return 0.0;
        }
        match memory.embedding_for(&self.model) {
            Some(vector) => super::embedding::cosine(&self.embedding, vector),
            None => 0.0,
        }
    }

    #[cfg(test)]
    fn terms(&self) -> &HashSet<String> {
        &self.terms
    }
}

/// Score a memory for L3 recall. Returns `None` when the memory matches
/// neither lexically nor semantically (it is excluded); otherwise a positive
/// score combining shared-term count, the semantic bonus, and the same
/// importance/confidence/recency signals as [`rerank_score`]. Scope/status are
/// filtered before this is called.
pub fn recall_score(memory: &Memory, query: &RecallQuery, now: i64) -> Option<f64> {
    if query.is_empty() {
        return None;
    }
    let mem_terms = recall_terms(&memory.content);
    let overlap = query
        .terms
        .iter()
        .filter(|t| mem_terms.contains(*t))
        .count();
    let similarity = query.similarity(memory);

    // Admission: either kind of evidence is enough. This union is the fix for
    // cross-language recall — a Chinese question has zero term overlap with an
    // English memory by construction (CJK bigrams and ASCII words can never be
    // equal), so without the semantic arm it could never be admitted at all.
    if overlap == 0 && similarity < RECALL_SEMANTIC_FLOOR {
        return None;
    }

    let mut score = overlap as f64; // each shared term = 1.0
    if similarity >= RECALL_SEMANTIC_FLOOR {
        let above_floor = (similarity - RECALL_SEMANTIC_FLOOR) / (1.0 - RECALL_SEMANTIC_FLOOR);
        score += RECALL_SEMANTIC_WEIGHT * above_floor as f64;
    }
    score += signal_bonus(memory, now);
    Some(score)
}

/// Select in-scope active and candidate memories for L3 recall.
pub fn select_recall(
    memories: &[Memory],
    ctx: &MemoryContext,
    query: &RecallQuery,
    limit: usize,
    now: i64,
) -> Vec<ScoredMemory> {
    select_matching(memories, ctx, query, limit, now, |status| {
        matches!(status, MemoryStatus::Active | MemoryStatus::Candidate)
    })
}

/// [`select_recall`], plus the claims the user has already **rejected**.
///
/// The memory consolidator's view, and only its: an observation that matches a
/// rejected claim is the user's own "no" being re-observed, and writing it back
/// as a fresh candidate is how a rejection is forgotten — the same claim would
/// come back on the next occasion, and the one after that.
///
/// A rejected memory still never reaches a prompt: injection goes through
/// [`select_recall`], and `dream_verdict` promotes nothing that is not a
/// candidate, so the evidence this lets a rejection accumulate cannot revive
/// it. Only a human `memory promote` can.
pub fn select_related(
    memories: &[Memory],
    ctx: &MemoryContext,
    query: &RecallQuery,
    limit: usize,
    now: i64,
) -> Vec<ScoredMemory> {
    select_matching(memories, ctx, query, limit, now, |status| {
        matches!(
            status,
            MemoryStatus::Active | MemoryStatus::Candidate | MemoryStatus::Rejected
        )
    })
}

fn select_matching(
    memories: &[Memory],
    ctx: &MemoryContext,
    query: &RecallQuery,
    limit: usize,
    now: i64,
    admits: impl Fn(MemoryStatus) -> bool,
) -> Vec<ScoredMemory> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<ScoredMemory> = memories
        .iter()
        .filter(|m| admits(m.status))
        .filter(|m| ctx.allows(&m.scope))
        .filter_map(|m| {
            recall_score(m, query, now).map(|score| ScoredMemory {
                memory: m.clone(),
                score,
            })
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if limit > 0 {
        scored.truncate(limit);
    }
    scored
}

// ── repository ────────────────────────────────────────────────────────────────

#[async_trait]
pub trait MemoryRepository: Send + Sync {
    /// Persist a memory (create or overwrite by id).
    async fn save(&self, memory: &Memory) -> anyhow::Result<()>;

    /// All non-expired memories, any status. Callers filter further. (Kept
    /// no-arg for the `memory` tool; richer scope/status queries go through
    /// `recall` / `search`.)
    async fn list(&self) -> anyhow::Result<Vec<Memory>>;

    /// Fetch a single memory by id. Default scans [`list`](MemoryRepository::list)
    /// (so it does not see expired memories); a store may override to fetch
    /// directly. Used by governance actions (promote/reject/archive/update).
    async fn get(&self, id: &str) -> anyhow::Result<Option<Memory>> {
        Ok(self.list().await?.into_iter().find(|m| m.id == id))
    }

    /// L3 active recall: the in-scope memories most relevant to `text` (the
    /// current user message), ranked by [`recall_score`], top `limit`.
    /// Scope/status are enforced here (design principle 3: never widen in the
    /// render layer). Default runs over [`list`](MemoryRepository::list); a
    /// store may override the candidate fetch later without changing scoring.
    ///
    /// **Candidates are recallable.** Both `Active` and `Candidate` memories
    /// surface (only `Archived`/`Rejected` are excluded). This is what makes the
    /// OpenClaw-style dreaming loop possible: a reviewer-extracted candidate must
    /// be visible to recall to *earn* its usage signal (`recall_count`), which
    /// the `DreamSweep` then uses to auto-promote it. Candidates score lower
    /// (their `Extracted` confidence adds nothing) and the rendered block tags
    /// each line with confidence, so the model treats them cautiously.
    ///
    /// The per-turn hot path in `infra/llm.rs` uses [`select_recall`] over a
    /// single shared `list()` instead; this method is
    /// the standalone entry point retained for the memory store's query surface
    /// and its integration tests. Lexical-only: a store holds no embedding
    /// backend, so the semantic arm belongs to the turn path, which does.
    #[allow(dead_code)]
    async fn recall(
        &self,
        ctx: &MemoryContext,
        text: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<ScoredMemory>> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let query = RecallQuery::lexical(text);
        Ok(select_recall(&self.list().await?, ctx, &query, limit, now))
    }

    /// Record that memories surfaced in recall: bump `recall_count` and stamp
    /// `last_used_at`, never touching `updated_at` so the recency-decay signal
    /// stays tied to real edits.
    ///
    /// A **utility** signal only. It says the retriever keeps finding these
    /// relevant, which is what retention is decided on; what makes a memory *true*
    /// is recorded by `record_evidence` instead. Best-effort: ids that no longer
    /// resolve are skipped.
    async fn mark_used(&self, ids: &[String], now: i64) -> anyhow::Result<()> {
        for id in ids {
            if let Some(mut memory) = self.get(id).await? {
                memory.recall_count += 1;
                memory.last_used_at = Some(now);
                self.save(&memory).await?;
            }
        }
        Ok(())
    }
}

// ── dreaming (evidence-driven consolidation) ─────────────────────────────────

/// What the nightly `DreamSweep` should do with a candidate memory.
///
/// **Truth and utility are decided by different signals**, and mixing them was
/// the defect this split fixes. Promotion used to be earned by `recall_count` and
/// query diversity — which prove only that the retriever keeps finding a memory
/// relevant. A wrong candidate that happened to be relevant to a question the
/// user asks often would therefore promote itself: injected, counted, promoted,
/// on the strength of nothing but its own retrieval. Recall could never be
/// evidence, because the thing being retrieved is not the thing being tested.
///
/// So promotion now reads the evidence signals — an explicit confirmation, or
/// support from independent occasions, with no unresolved conflict — and
/// `recall_count` decides only *retention*: whether a candidate nobody ever needed
/// should be retired. Which is the one question retrieval frequency genuinely
/// answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamVerdict {
    /// Recalled often enough, recently enough → promote to active.
    Promote,
    /// Old and never recalled → archive (reversible; not rejected).
    Archive,
    /// Leave as-is this cycle.
    Keep,
}

/// Independent occasions of support a candidate needs to be promoted without an
/// explicit confirmation.
///
/// Two, not three: independence is already per learning occasion (see
/// [`Memory::record_evidence`]), so two means the user said the same thing on two
/// occasions komo learned from separately — a real pattern rather than one
/// talkative pass. A higher bar would leave genuinely established facts sitting
/// in the candidate pile for weeks.
pub const DREAM_MIN_SUPPORT: i64 = 2;
/// A candidate this old (days) that has gone cold — never recalled, or not
/// recalled within the same window — is archived: the "forget the flotsam" half
/// of dreaming. Coldness is measured on `last_used_at`, not a lifetime
/// `recall_count`, so a *weakly* recalled candidate (one or two hits long ago)
/// is retired too rather than lingering in the pile forever.
pub const DREAM_FORGET_AGE_DAYS: i64 = 30;
/// How long an *unresolved refutation* is left for someone to rule on before the
/// candidate carrying it is archived.
///
/// A week, against the clean candidate's thirty days, and the asymmetry is the
/// point: retiring a refuted claim early costs an archived candidate that triage
/// can restore, while keeping one costs a recall slot in every search about the
/// very thing it is wrong about. Support has to accumulate across independent
/// occasions to promote; a contradiction retires on its own.
pub const DREAM_REFUTED_FORGET_AGE_DAYS: i64 = 7;

/// Dreaming score for a candidate, for **ordering the `komo dream` preview** —
/// never the verdict, which [`dream_verdict`] decides on its own gates.
///
/// Weighted so the preview surfaces what is closest to promotion: support
/// dominates (it is what promotion reads), with recall and recency as tiebreakers
/// among equally-supported candidates. A contradiction pushes a candidate down —
/// it is the furthest thing from ready.
pub fn dream_score(memory: &Memory, now: i64) -> f64 {
    let mut score = 10.0 * memory.support_count as f64;
    score -= 10.0 * memory.contradiction_count as f64;
    score += memory.recall_count as f64; // utility, as a tiebreaker
    score += memory.importance as f64 / 100.0; // 0..~1
    // Recency of last use: a 30-day half-life decay, 0 when never used.
    if let Some(last) = memory.last_used_at {
        let age_days = (now - last).max(0) as f64 / 86_400.0;
        score += 0.5 * (-age_days / 30.0).exp();
    }
    score
}

/// Decide a candidate's fate for this dream cycle. Only `Candidate` memories are
/// ever acted on — active memories (user-saved or already promoted) are left to
/// the operator (`komo memory report` flags long-unused ones), so dreaming can
/// never silently retire something the user deliberately kept.
pub fn dream_verdict(memory: &Memory, now: i64) -> DreamVerdict {
    if memory.status != MemoryStatus::Candidate {
        return DreamVerdict::Keep;
    }
    // Promotion takes precedence, and reads only truth signals: the user said so
    // outright, or said it on enough independent occasions — and nothing is
    // currently in conflict with it. An unresolved conflict blocks promotion
    // however well-supported the claim is: promoting into a contest would assert
    // one side of a question nobody has answered.
    let believed = memory.belief == BeliefState::Current && memory.contradiction_count == 0;
    // Support accumulates from occasions; a confirmation is the user ruling on
    // the claim itself. Only the second can promote something a *tool* said —
    // otherwise a page read on three occasions promotes itself, and komo starts
    // asserting whatever it happened to fetch.
    let proven = memory.last_confirmed_at.is_some()
        || (memory.support_count >= DREAM_MIN_SUPPORT && memory.provenance.promotable_on_support());
    if believed && proven {
        return DreamVerdict::Promote;
    }
    // Forget what has been refuted, on a shorter clock than the merely unused
    // and without waiting for it to go cold. A candidate under an unresolved
    // contradiction can never promote however often it is retrieved, so the only
    // thing left that could change its fate is a human ruling — and once nobody
    // has ruled for a week, retrieval warmth is keeping alive a claim the user
    // has already spoken against.
    let refuted_days = memory
        .unresolved_refutation_at()
        .map(|at| (now - at).max(0) as f64 / 86_400.0);
    if refuted_days.is_some_and(|days| days > DREAM_REFUTED_FORGET_AGE_DAYS as f64) {
        return DreamVerdict::Archive;
    }
    // Forget the flotsam: old enough to have had its chance, and now cold. Cold
    // is measured on `last_used_at` (never used, or last used outside the forget
    // window), not a lifetime `recall_count == 0` — the old check leaked the
    // *weakly* recalled (one or two hits, then silence) into an ever-growing
    // candidate pile that nothing ever retired.
    let age_days = (now - memory.created_at).max(0) as f64 / 86_400.0;
    let cold = match memory.last_used_at {
        Some(last) => (now - last).max(0) as f64 / 86_400.0 > DREAM_FORGET_AGE_DAYS as f64,
        None => true,
    };
    if cold && age_days as i64 > DREAM_FORGET_AGE_DAYS {
        DreamVerdict::Archive
    } else {
        DreamVerdict::Keep
    }
}

#[cfg(test)]
mod tests;
