use super::*;

#[test]
fn parse_kind_accepts_legacy_and_new() {
    assert_eq!(parse_memory_kind("user"), MemoryKind::Profile);
    assert_eq!(parse_memory_kind("preference"), MemoryKind::Preference);
    assert_eq!(parse_memory_kind("decision"), MemoryKind::Decision);
    assert_eq!(parse_memory_kind("nonsense"), MemoryKind::Fact);
}

#[test]
fn scope_roundtrips_through_parts() {
    let scopes = [
        MemoryScope::Global,
        MemoryScope::Project("komo".into()),
        MemoryScope::Channel {
            platform: "telegram".into(),
            chat_id: "42".into(),
        },
        MemoryScope::Session("feishu:oc_x".into()),
    ];
    for scope in scopes {
        let rebuilt = MemoryScope::from_parts(scope.type_str(), &scope.key());
        assert_eq!(rebuilt, scope);
    }
}

#[test]
fn channel_scope_with_malformed_key_degrades_to_global() {
    assert_eq!(
        MemoryScope::from_parts("channel", "no-colon"),
        MemoryScope::Global
    );
}

#[test]
fn context_from_chat_session_allows_global_channel_session() {
    let ctx = MemoryContext::new("s1", Some(&ChannelPeer::new("telegram", "42")));
    assert!(ctx.allows(&MemoryScope::Global));
    assert!(ctx.allows(&MemoryScope::Channel {
        platform: "telegram".into(),
        chat_id: "42".into()
    }));
    assert!(ctx.allows(&MemoryScope::Session("s1".into())));
    // A different channel is not allowed.
    assert!(!ctx.allows(&MemoryScope::Channel {
        platform: "feishu".into(),
        chat_id: "oc_x".into()
    }));
    assert_eq!(
        ctx.write_scope(),
        MemoryScope::Channel {
            platform: "telegram".into(),
            chat_id: "42".into()
        }
    );
}

#[test]
fn cli_session_context_writes_global() {
    let ctx = MemoryContext::local("0192-uuid");
    assert_eq!(ctx.write_scope(), MemoryScope::Global);
}

#[test]
fn recall_terms_splits_ascii_words_and_cjk_bigrams() {
    let terms = recall_terms("Uses Rust 项目");
    assert!(terms.contains("uses"));
    assert!(terms.contains("rust"));
    assert!(terms.contains("项目")); // CJK bigram
}

#[test]
fn recall_score_requires_term_overlap() {
    let now = 1_000;
    let m = Memory::new(MemoryKind::Project, "the project is written in Rust");
    // Overlapping term "rust" → scored.
    let hit = RecallQuery::lexical("what language is the rust project in");
    assert!(recall_score(&m, &hit, now).is_some());
    // No overlap → excluded.
    let miss = RecallQuery::lexical("当前天气如何");
    assert!(recall_score(&m, &miss, now).is_none());
    // Empty query → excluded.
    assert!(recall_score(&m, &RecallQuery::lexical(""), now).is_none());
}

/// The defect this whole layer exists for: a Chinese question and an
/// English memory tokenize into disjoint sets, so lexical recall can never
/// admit one for the other.
#[test]
fn lexical_terms_never_cross_the_script_boundary() {
    let zh = recall_terms("我平时用什么语言跟你说话");
    let en = recall_terms("User communicates in Chinese.");
    assert!(
        zh.intersection(&en).next().is_none(),
        "CJK bigrams and ASCII words are structurally incapable of overlapping"
    );
}

/// …and the fix: with a query vector close to the memory's, the same pair
/// is admitted and scored, purely on the semantic arm.
#[test]
fn semantic_similarity_recalls_across_languages() {
    let now = 1_000;
    let mut memory = Memory::new(MemoryKind::Profile, "User communicates in Chinese.");
    memory.embedding = vec![0.6, 0.8]; // unit length
    memory.embedding_model = "test-model".into();

    let zh = "我平时用什么语言跟你说话";
    assert!(
        recall_score(&memory, &RecallQuery::lexical(zh), now).is_none(),
        "lexically this pair cannot match"
    );

    // A near-parallel query vector (cosine ≈ 0.999) — well past the floor.
    let query = RecallQuery::semantic(zh, vec![0.62, 0.78], "test-model");
    assert!(
        recall_score(&memory, &query, now).is_some(),
        "the semantic arm admits what the lexical arm cannot"
    );
}

/// A vector from another model is not comparable, so it must read as
/// "not embedded" rather than scoring against an unrelated space.
#[test]
fn embedding_from_another_model_is_ignored() {
    let now = 1_000;
    let mut memory = Memory::new(MemoryKind::Fact, "User communicates in Chinese.");
    memory.embedding = vec![1.0, 0.0];
    memory.embedding_model = "old-model".into();

    let query = RecallQuery::semantic("我说什么语言", vec![1.0, 0.0], "new-model");
    assert!(recall_score(&memory, &query, now).is_none());
    assert!(memory.embedding_for("new-model").is_none());
    assert!(memory.embedding_for("old-model").is_some());
}

/// An unrelated question must stay below the floor even with embeddings on
/// — the semantic arm widens recall, it does not disable it.
#[test]
fn weak_similarity_stays_below_the_floor() {
    let now = 1_000;
    let mut memory = Memory::new(MemoryKind::Fact, "User communicates in Chinese.");
    memory.embedding = vec![1.0, 0.0];
    memory.embedding_model = "test-model".into();

    // cosine = 0.3, under RECALL_SEMANTIC_FLOOR.
    let mut weak = vec![0.3, (1.0f32 - 0.09).sqrt()];
    super::super::embedding::normalize(&mut weak);
    let query = RecallQuery::semantic("今天午饭吃什么", weak, "test-model");
    assert!(recall_score(&memory, &query, now).is_none());
}

/// Lexical evidence must keep working with embeddings configured — a
/// memory the query overlaps is admitted even with no vector at all.
#[test]
fn lexical_hits_survive_when_the_memory_has_no_vector() {
    let now = 1_000;
    let memory = Memory::new(MemoryKind::Fact, "the rust toolchain is pinned");
    let query = RecallQuery::semantic("rust toolchain", vec![1.0, 0.0], "test-model");
    assert!(recall_score(&memory, &query, now).is_some());
}

/// A query with no lexical terms is still a real query when it carries a
/// vector — otherwise `select_recall` would bail before scoring anything.
#[test]
fn a_query_is_empty_only_without_terms_and_vector() {
    assert!(RecallQuery::lexical("").is_empty());
    assert!(RecallQuery::lexical("!!!").is_empty());
    assert!(!RecallQuery::semantic("!!!", vec![1.0], "m").is_empty());
    assert!(!RecallQuery::lexical("rust").terms().is_empty());
}

/// A local surface has no correspondent, so an automated write there is
/// global — it must be recallable from the next conversation. A real chat
/// channel keeps its scope, which is a privacy boundary.
#[test]
fn a_turn_without_a_correspondent_writes_global_but_a_chat_keeps_its_channel() {
    assert_eq!(
        MemoryContext::local("019fb0ce-9f7a-7c23-a87d-dab9df9216d8").write_scope(),
        MemoryScope::Global,
        "a local conversation names no partner to scope to"
    );
    assert_eq!(
        MemoryContext::new("s1", Some(&ChannelPeer::new("feishu", "ou_445299e2"))).write_scope(),
        MemoryScope::Channel {
            platform: "feishu".into(),
            chat_id: "ou_445299e2".into(),
        },
        "a real chat channel's scope is a privacy boundary and must survive"
    );
}

/// A global memory written from one api conversation must be recallable
/// from the next — the end-to-end shape of the scope fix.
#[test]
fn a_memory_written_in_one_local_conversation_is_recallable_in_the_next() {
    let now = 1_000;
    let write_ctx = MemoryContext::local("conversation-one");
    let mut memory = Memory::new(MemoryKind::Fact, "the rust toolchain is pinned");
    memory.scope = write_ctx.write_scope();

    let read_ctx = MemoryContext::local("conversation-two");
    let query = RecallQuery::lexical("rust toolchain");
    assert_eq!(select_recall(&[memory], &read_ctx, &query, 5, now).len(), 1,);
}

#[test]
fn select_recall_ranks_in_scope_matches_and_caps() {
    let ctx = MemoryContext::local("s1");
    let now = 1_000;
    let mut hit = Memory::new(MemoryKind::Fact, "the rust toolchain is pinned");
    hit.updated_at = now;
    let miss = Memory::new(MemoryKind::Fact, "unrelated weather note");
    let query = RecallQuery::lexical("rust toolchain");
    let scored = select_recall(&[hit.clone(), miss], &ctx, &query, 5, now);
    assert_eq!(
        scored.len(),
        1,
        "only the lexically overlapping memory scores"
    );
    assert_eq!(scored[0].memory.id, hit.id);
    // limit is honoured.
    assert!(select_recall(&[hit], &ctx, &query, 0, now).len() <= 1);
}

#[test]
fn recall_score_orders_by_overlap_then_signals() {
    let now = 1_000;
    let mut more = Memory::new(MemoryKind::Fact, "rust async tokio runtime");
    more.updated_at = now;
    let mut fewer = Memory::new(MemoryKind::Fact, "rust crate");
    fewer.updated_at = now;
    let q = RecallQuery::lexical("rust async tokio");
    let s_more = recall_score(&more, &q, now).unwrap();
    let s_fewer = recall_score(&fewer, &q, now).unwrap();
    assert!(s_more > s_fewer, "more overlapping terms must score higher");
}

fn candidate(recall_count: i64, age_days: i64, now: i64) -> Memory {
    let mut m = Memory::new(MemoryKind::Fact, "the rust toolchain is pinned");
    m.status = MemoryStatus::Candidate;
    m.confidence = MemoryConfidence::Extracted;
    m.created_at = now - age_days * 86_400;
    m.recall_count = recall_count;
    if recall_count > 0 {
        m.last_used_at = Some(now - 86_400); // used yesterday
    }
    m
}

/// Support from independent occasions is what earns promotion.
#[test]
fn dream_promotes_a_candidate_with_independent_support() {
    let now = 10_000 * 86_400;
    let mut m = candidate(0, 5, now);
    m.record_evidence("s-1", "s-1", EvidenceRelation::Supports, "said it", now);
    assert_eq!(
        dream_verdict(&m, now),
        DreamVerdict::Keep,
        "one occasion is not a pattern"
    );
    m.record_evidence(
        "s-2",
        "s-2",
        EvidenceRelation::Supports,
        "said it again",
        now,
    );
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Promote);
}

/// Occasions corroborate what the *user* keeps saying. A claim komo read
/// out of a fetched page can be re-read on any number of occasions without
/// anyone ever having said it — so support alone must not promote it into
/// the prompt of every later turn. Only the user ruling on it can.
#[test]
fn dream_will_not_promote_a_tool_derived_claim_on_support_alone() {
    let now = 10_000 * 86_400;
    let mut m = candidate(0, 5, now);
    m.provenance = MemoryProvenance::Tool;
    m.record_evidence(
        "s-1",
        "s-1",
        EvidenceRelation::Supports,
        "the page said it",
        now,
    );
    m.record_evidence("s-2", "s-2", EvidenceRelation::Supports, "and again", now);
    assert_eq!(
        dream_verdict(&m, now),
        DreamVerdict::Keep,
        "two occasions of reading the same page is not the user saying so"
    );
    assert!(
        !m.is_supported(),
        "and the injected line must not claim it is corroborated either"
    );

    // The user confirming it is a different fact, and it is enough.
    m.last_confirmed_at = Some(now - 86_400);
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Promote);
    assert!(m.is_supported());
}

/// An explicit confirmation is enough on its own — no waiting for a second
/// occasion when the user has already said so outright.
#[test]
fn dream_promotes_an_explicitly_confirmed_candidate() {
    let now = 10_000 * 86_400;
    let mut m = candidate(0, 5, now);
    m.last_confirmed_at = Some(now - 86_400);
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Promote);
}

/// **The defect this rework exists for.** A candidate retrieved many times by
/// many different questions used to promote itself on that alone — but recall
/// frequency measures the retriever, not the truth of what it retrieved.
#[test]
fn recall_frequency_alone_can_never_promote_a_candidate() {
    let now = 10_000 * 86_400;
    let m = candidate(50, 5, now);
    assert_eq!(
        dream_verdict(&m, now),
        DreamVerdict::Keep,
        "heavily recalled, never corroborated — it stays a candidate"
    );
    // …and it is not archived either: it is plainly still useful.
    let mut cold_but_hot = m.clone();
    cold_but_hot.created_at = now - (DREAM_FORGET_AGE_DAYS + 10) * 86_400;
    assert_eq!(dream_verdict(&cold_but_hot, now), DreamVerdict::Keep);
}

/// An unresolved conflict blocks promotion however well-supported the claim
/// is: promoting into a contest asserts one side of an open question.
#[test]
fn dream_never_promotes_a_contested_or_contradicted_candidate() {
    let now = 10_000 * 86_400;
    let mut m = candidate(0, 5, now);
    m.record_evidence("s-1", "s-1", EvidenceRelation::Supports, "a", now);
    m.record_evidence("s-2", "s-2", EvidenceRelation::Supports, "b", now);
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Promote);

    // A contradiction from a third occasion stops it, even before anything
    // marks the belief contested.
    let mut contradicted = m.clone();
    contradicted.record_evidence("s-3", "s-3", EvidenceRelation::Contradicts, "no", now);
    assert_eq!(dream_verdict(&contradicted, now), DreamVerdict::Keep);

    let mut contested = m.clone();
    contested.contest(now);
    assert_eq!(dream_verdict(&contested, now), DreamVerdict::Keep);

    let mut superseded = m.clone();
    superseded.supersede("mem-new", now);
    assert_eq!(dream_verdict(&superseded, now), DreamVerdict::Keep);
}

/// The asymmetry: a refutation nobody rules on retires the candidate well
/// before the thirty-day cold rule would, and *regardless* of how warm
/// retrieval keeps it — a claim the user has spoken against cannot earn its
/// recall slot back by being relevant.
#[test]
fn dream_archives_a_candidate_left_refuted() {
    let now = 10_000 * 86_400;
    let stale = now - (DREAM_REFUTED_FORGET_AGE_DAYS + 1) * 86_400;

    // Warm (recalled yesterday) and young — neither saves it.
    let mut contradicted = candidate(20, 5, now);
    contradicted.record_evidence("s-1", "s-1", EvidenceRelation::Contradicts, "no", stale);
    assert_eq!(dream_verdict(&contradicted, now), DreamVerdict::Archive);

    // `contest`/`supersede` write no evidence entry; the edit clock stands in.
    let mut superseded = candidate(20, 5, now);
    superseded.supersede("mem-new", stale);
    assert_eq!(dream_verdict(&superseded, now), DreamVerdict::Archive);
}

/// A refutation is a question for the operator first. It only becomes a
/// retirement once a week has passed with nobody answering it.
#[test]
fn dream_leaves_a_fresh_refutation_for_the_operator() {
    let now = 10_000 * 86_400;
    let mut m = candidate(20, 5, now);
    m.record_evidence(
        "s-1",
        "s-1",
        EvidenceRelation::Contradicts,
        "no",
        now - (DREAM_REFUTED_FORGET_AGE_DAYS - 1) * 86_400,
    );
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Keep);
}

/// A confirmation *after* the conflict is the ruling the window was waiting
/// for — the candidate goes back to being judged on support alone.
#[test]
fn dream_does_not_retire_a_refutation_a_confirmation_has_settled() {
    let now = 10_000 * 86_400;
    let mut m = candidate(0, 5, now);
    m.record_evidence(
        "s-1",
        "s-1",
        EvidenceRelation::Contradicts,
        "no",
        now - (DREAM_REFUTED_FORGET_AGE_DAYS + 1) * 86_400,
    );
    m.last_confirmed_at = Some(now - 86_400);
    assert_eq!(m.unresolved_refutation_at(), None);
    // Still not promoted — an outstanding contradiction count blocks that —
    // but no longer on the refuted clock either.
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Keep);
}

/// The preview ordering has to put what is closest to promotion on top, or
/// `komo dream` stops being a useful triage queue.
/// Golden case: being retrieved a lot must never make a memory *true*.
///
/// This is the self-confirming loop the truth/utility split exists to break.
/// Promotion once read the recall counters, so a wrong memory that happened
/// to be relevant to a recurring question would keep being injected, keep
/// scoring, and promote itself on the strength of nothing but its own
/// retrieval. The thing retrieved is not the thing tested.
///
/// Ranking is checked below; this checks the *gate*, which is what actually
/// decides promotion.
#[test]
fn retrieval_alone_never_makes_a_memory_supported() {
    let now = 1_800_000_000;

    // Recalled relentlessly, corroborated by nobody.
    let mut popular = candidate(500, 10, now);
    popular.last_used_at = Some(now);
    assert!(
        !popular.is_supported(),
        "500 recalls and no evidence must not clear the promotion bar"
    );

    // One independent occasion is still not enough — the bar is two.
    let mut once = candidate(0, 10, now);
    once.record_evidence(
        "session-a",
        "session-a",
        EvidenceRelation::Supports,
        "said it once",
        now,
    );
    assert!(!once.is_supported(), "one occasion is not corroboration");

    // A second, independent occasion clears it.
    let mut twice = once.clone();
    twice.record_evidence(
        "session-b",
        "session-b",
        EvidenceRelation::Supports,
        "said it again",
        now,
    );
    assert!(
        twice.is_supported(),
        "two independent occasions corroborate"
    );

    // ...but the same session saying it twice is one occasion, however
    // talkative it is. Otherwise one conversation corroborates itself.
    let mut echo = once.clone();
    echo.record_evidence(
        "session-a",
        "session-a",
        EvidenceRelation::Supports,
        "and again",
        now,
    );
    assert!(
        !echo.is_supported(),
        "one session cannot be two independent occasions"
    );
}

#[test]
fn dream_score_ranks_supported_candidates_above_merely_recalled_ones() {
    let now = 10_000 * 86_400;
    let mut supported = candidate(0, 5, now);
    supported.record_evidence("s-1", "s-1", EvidenceRelation::Supports, "a", now);
    let recalled = candidate(5, 5, now);
    assert!(
        dream_score(&supported, now) > dream_score(&recalled, now),
        "one real corroboration outranks five retrievals"
    );

    let mut contradicted = supported.clone();
    contradicted.record_evidence("s-2", "s-2", EvidenceRelation::Contradicts, "no", now);
    assert!(
        dream_score(&contradicted, now) < dream_score(&supported, now),
        "a contradiction pushes a candidate down the queue"
    );
}

#[test]
fn governance_transitions_set_status_confidence_and_updated_at() {
    let now = 9_000;
    let mut m = candidate(0, 1, 8_000);
    m.promote(now);
    assert_eq!(m.status, MemoryStatus::Active);
    assert_eq!(m.confidence, MemoryConfidence::Confirmed);
    assert_eq!(m.updated_at, now);

    let mut m = candidate(0, 1, 8_000);
    m.reject(now);
    assert_eq!(m.status, MemoryStatus::Rejected);
}

#[test]
fn dream_keeps_under_recalled_candidate() {
    let now = 10_000 * 86_400;
    // Two recalls — below the threshold of three — and still young.
    let m = candidate(2, 5, now);
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Keep);
}

#[test]
fn dream_archives_old_never_recalled_candidate() {
    let now = 10_000 * 86_400;
    let m = candidate(0, DREAM_FORGET_AGE_DAYS + 1, now);
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Archive);
}

#[test]
fn dream_keeps_young_never_recalled_candidate() {
    let now = 10_000 * 86_400;
    // Never recalled but still within the forget window — give it time.
    let m = candidate(0, DREAM_FORGET_AGE_DAYS - 1, now);
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Keep);
}

#[test]
fn dream_archives_weakly_recalled_gone_cold() {
    let now = 10_000 * 86_400;
    // Two recalls long ago, then silence: below the promote bar, and cold
    // (last used outside the forget window) — this is the leak the old
    // `recall_count == 0` archive check let linger forever. Now retired.
    let mut m = candidate(2, DREAM_FORGET_AGE_DAYS + 10, now);
    m.last_used_at = Some(now - (DREAM_FORGET_AGE_DAYS + 5) * 86_400);
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Archive);
}

#[test]
fn dream_keeps_weakly_recalled_still_warm() {
    let now = 10_000 * 86_400;
    // Old, only two recalls — but recalled recently, so it is still earning
    // its keep and might yet reach the promote bar. Not archived.
    let mut m = candidate(2, DREAM_FORGET_AGE_DAYS + 10, now);
    m.last_used_at = Some(now - 5 * 86_400);
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Keep);
}

#[test]
fn dream_never_touches_active_memories() {
    let now = 10_000 * 86_400;
    // An active memory recalled a lot is still left alone (no auto-archive of
    // user-kept memories), and an old unused active is not archived either.
    let mut hot = candidate(99, 1, now);
    hot.status = MemoryStatus::Active;
    assert_eq!(dream_verdict(&hot, now), DreamVerdict::Keep);
    let mut cold = candidate(0, DREAM_FORGET_AGE_DAYS + 100, now);
    cold.status = MemoryStatus::Active;
    assert_eq!(dream_verdict(&cold, now), DreamVerdict::Keep);
}

// ---- belief state and evidence ----

/// Independence is per occasion: everything one extraction pass gathered is
/// one observation, however many sentences it read.
#[test]
fn evidence_from_the_same_occasion_counts_once() {
    let now = 1_000;
    let mut m = Memory::new(MemoryKind::Preference, "user prefers rebase");
    assert!(m.record_evidence("s-1", "run-1", EvidenceRelation::Supports, "I rebase", now));
    assert!(!m.record_evidence(
        "s-1",
        "run-1",
        EvidenceRelation::Supports,
        "always rebase",
        now + 5
    ));
    assert_eq!(m.support_count, 1);
    assert_eq!(m.evidence.len(), 1);

    // A different occasion is a genuinely independent observation.
    assert!(m.record_evidence(
        "s-1",
        "run-2",
        EvidenceRelation::Supports,
        "rebase again",
        now + 10
    ));
    assert_eq!(m.support_count, 2);
}

/// The case the home session made unreachable: every private conversation is
/// one permanent session, so support has to accumulate across passes on it or
/// nothing extracted there can ever promote.
#[test]
fn two_occasions_on_one_session_promote_a_candidate() {
    let now = 1_000;
    let mut m = Memory::new(MemoryKind::Preference, "user prefers rebase");
    m.status = MemoryStatus::Candidate;
    m.provenance = MemoryProvenance::User;
    m.record_evidence("home", "run-1", EvidenceRelation::Supports, "I rebase", now);
    assert_eq!(dream_verdict(&m, now), DreamVerdict::Keep);

    m.record_evidence(
        "home",
        "run-2",
        EvidenceRelation::Supports,
        "rebased again",
        now + 86_400,
    );
    assert_eq!(m.support_count, DREAM_MIN_SUPPORT);
    assert_eq!(dream_verdict(&m, now + 86_400), DreamVerdict::Promote);
}

/// Evidence stored before occasions existed is keyed by its session, and a
/// session id never collides with a run id — so the old row counts as one
/// occasion and the next pass counts separately.
#[test]
fn legacy_evidence_is_keyed_by_its_session() {
    let now = 1_000;
    let mut m = Memory::new(MemoryKind::Preference, "user prefers rebase");
    m.evidence.push(Evidence {
        session: "home".into(),
        occasion: String::new(),
        observed_at: now,
        relation: EvidenceRelation::Supports,
        excerpt: "I rebase".into(),
    });
    m.support_count = 1;

    assert!(
        !m.record_evidence("home", "home", EvidenceRelation::Supports, "again", now + 5),
        "the legacy row falls back to its session as the key"
    );
    assert!(m.record_evidence(
        "home",
        "run-2",
        EvidenceRelation::Supports,
        "again",
        now + 10
    ));
    assert_eq!(m.support_count, 2);
}

#[test]
fn contradicting_evidence_counts_separately() {
    let now = 1_000;
    let mut m = Memory::new(MemoryKind::Preference, "user prefers rebase");
    m.record_evidence("s-1", "s-1", EvidenceRelation::Supports, "I rebase", now);
    m.record_evidence(
        "s-2",
        "s-2",
        EvidenceRelation::Contradicts,
        "merge now",
        now,
    );
    assert_eq!(m.support_count, 1);
    assert_eq!(m.contradiction_count, 1);
}

/// The list is bounded while the counts keep rising, so a long-lived memory
/// cannot grow its row without limit.
#[test]
fn the_evidence_list_is_capped_but_the_count_is_not() {
    let now = 1_000;
    let mut m = Memory::new(MemoryKind::Fact, "x");
    for i in 0..(EVIDENCE_CAP + 3) {
        m.record_evidence(
            &format!("s-{i}"),
            &format!("occ-{i}"),
            EvidenceRelation::Supports,
            "said so",
            now + i as i64,
        );
    }
    assert_eq!(m.evidence.len(), EVIDENCE_CAP);
    assert_eq!(m.support_count, (EVIDENCE_CAP + 3) as i64);
    // The most recent survive — they are the ones that speak to "still true".
    assert_eq!(m.evidence.last().unwrap().session, "s-7");
}

#[test]
fn an_excerpt_is_truncated_by_characters_not_bytes() {
    let now = 1_000;
    let mut m = Memory::new(MemoryKind::Fact, "x");
    let long = "语".repeat(EVIDENCE_EXCERPT_MAX + 50);
    m.record_evidence("s-1", "s-1", EvidenceRelation::Supports, &long, now);
    assert_eq!(
        m.evidence[0].excerpt.chars().count(),
        EVIDENCE_EXCERPT_MAX,
        "counted in chars, so CJK is not cut to a third"
    );
}

/// The core of the belief axis: a contested memory stays retrievable but
/// stops being assertable.
#[test]
fn contested_and_superseded_memories_are_not_injectable() {
    let now = 1_000;
    let mut m = Memory::new(MemoryKind::Preference, "user writes Python");
    assert!(m.is_injectable());

    m.contest(now);
    assert!(!m.is_injectable());
    assert_eq!(m.belief, BeliefState::Contested);
    // Governance is untouched — contesting is not a triage decision.
    assert_eq!(m.status, MemoryStatus::Active);

    let mut m = Memory::new(MemoryKind::Preference, "user writes Python");
    m.supersede("mem-rust", now);
    assert!(!m.is_injectable());
    assert_eq!(m.superseded_by, "mem-rust");
}

/// Retrieval is deliberately belief-agnostic: the scoring layer must keep
/// returning a contested memory so an explicit search can surface it. The
/// injection filter lives with the injector.
#[test]
fn recall_scoring_still_returns_a_contested_memory() {
    let ctx = MemoryContext::local("s1");
    let now = 1_000;
    let mut m = Memory::new(MemoryKind::Fact, "the rust toolchain is pinned");
    m.contest(now);
    let query = RecallQuery::lexical("rust toolchain");
    assert_eq!(
        select_recall(&[m], &ctx, &query, 5, now).len(),
        1,
        "the query layer finds it; only injection refuses it"
    );
}

/// An operator promote is an explicit ruling: it confirms the memory and
/// clears whatever conflict was outstanding.
#[test]
fn promote_confirms_and_resolves_a_contest() {
    let now = 9_000;
    let mut m = candidate(0, 1, 8_000);
    m.contest(now - 100);
    m.superseded_by = "mem-other".into();
    m.promote(now);
    assert_eq!(m.belief, BeliefState::Current);
    assert!(m.superseded_by.is_empty());
    assert_eq!(m.last_confirmed_at, Some(now));
    assert!(m.is_injectable());
}

#[test]
fn belief_state_round_trips_and_unknown_reads_as_current() {
    for state in [
        BeliefState::Current,
        BeliefState::Contested,
        BeliefState::Superseded,
    ] {
        assert_eq!(parse_belief_state(state.as_str()), state);
    }
    // Every row written before the column existed.
    assert_eq!(parse_belief_state(""), BeliefState::Current);
    assert_eq!(parse_belief_state("nonsense"), BeliefState::Current);
}
