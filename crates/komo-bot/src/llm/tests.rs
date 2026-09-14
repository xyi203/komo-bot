use super::*;

/// The log's account of a round is counts and a flag — and a round that
/// called nothing has to be visibly distinct from one that called something.
#[test]
fn round_shape_counts_calls_and_text_without_reading_content() {
    let answered_alone = round_shape(&[AssistantBlock::Text("热水器已打开".into())]);
    assert_eq!(
        answered_alone,
        RoundShape {
            tool_calls: 0,
            text_chars: 6,
            reasoning: false,
        }
    );
    let acted = round_shape(&[
        AssistantBlock::Reasoning(Default::default()),
        AssistantBlock::Text("ok".into()),
        AssistantBlock::ToolCall {
            id: "1".into(),
            call_id: None,
            name: "homeassistant".into(),
            args: "{}".into(),
        },
        AssistantBlock::ToolCall {
            id: "2".into(),
            call_id: None,
            name: "read".into(),
            args: "{}".into(),
        },
    ]);
    assert_eq!(
        acted,
        RoundShape {
            tool_calls: 2,
            text_chars: 2,
            reasoning: true,
        }
    );
}

#[test]
fn openai_style_providers_send_reasoning_effort() {
    for provider in [Provider::OpenAi, Provider::OpenRouter, Provider::Codex] {
        assert_eq!(
            reasoning_params(provider, "high"),
            Some(json!({ "reasoning": { "effort": "high" } })),
            "{provider:?} should carry reasoning.effort"
        );
    }
}

#[test]
fn anthropic_maps_effort_onto_a_thinking_budget() {
    let low = reasoning_params(Provider::Anthropic, "low").unwrap();
    let high = reasoning_params(Provider::Anthropic, "high").unwrap();
    let budget = |v: &Value| v["thinking"]["budget_tokens"].as_u64().unwrap();
    assert_eq!(low["thinking"]["type"], "enabled");
    assert!(
        budget(&low) < budget(&high),
        "a higher effort must buy more thinking"
    );
}

/// A Claude Code login only reaches Claude 4.6+, where the budget above is a
/// 400 — the level rides on `output_config.effort` instead.
#[test]
fn claude_code_maps_effort_onto_adaptive_thinking() {
    for level in ["low", "medium", "high", "xhigh", "max"] {
        let params = reasoning_params(Provider::ClaudeCode, level).unwrap();
        assert_eq!(params["thinking"]["type"], "adaptive", "{level:?}");
        assert_eq!(params["output_config"]["effort"], level);
        assert!(
            params["thinking"]["budget_tokens"].is_null(),
            "the manual thinking budget is rejected on adaptive models"
        );
        // The default is `omitted`, which would drop the reasoning komo's
        // clients render.
        assert_eq!(params["thinking"]["display"], "summarized");
    }
}

/// An adaptive model thinks unless told not to, so the aux default has to send
/// a real disable rather than simply omit the parameter.
#[test]
fn claude_code_disables_thinking_for_the_aux_default() {
    assert_eq!(Provider::ClaudeCode.aux_default_effort(), Some("none"));
    assert_eq!(
        reasoning_params(Provider::ClaudeCode, "none"),
        Some(json!({ "thinking": { "type": "disabled" } }))
    );
    // …and it is a level a session can pick too, like DeepSeek's: on an
    // adaptive model "no thinking" is a real answer shape, not just a default.
    assert!(Provider::ClaudeCode.efforts().contains(&"none"));
}

#[test]
fn deepseek_maps_its_own_scale_and_nothing_else() {
    // `none` is thinking off, and a level of its own on this scale.
    for level in ["none", "low", "high", "max"] {
        assert_eq!(
            reasoning_params(Provider::DeepSeek, level),
            Some(json!({ "reasoning": { "effort": level } })),
            "{level:?}"
        );
    }
    // `medium` is not on DeepSeek's scale — the server would alias it onto
    // `high`, so komo declines it rather than sending a level it did not offer.
    for level in ["", "  ", "medium", "auto", "HIGH"] {
        assert_eq!(
            reasoning_params(Provider::DeepSeek, level),
            None,
            "{level:?}"
        );
    }
    // Nobody else has a "thinking off" rung, so `none` means nothing there.
    assert_eq!(reasoning_params(Provider::OpenAi, "none"), None);
    assert_eq!(reasoning_params(Provider::Anthropic, "none"), None);
    for level in ["", "  ", "auto", "xhigh", "HIGH"] {
        assert_eq!(reasoning_params(Provider::OpenAi, level), None, "{level:?}");
    }
}

#[test]
fn every_advertised_effort_level_actually_maps() {
    // The menu a client is shown (`Provider::efforts`) and what reaches the
    // wire must agree — otherwise the UI offers a switch that does nothing.
    for provider in Provider::ALL {
        for level in provider.efforts() {
            assert!(
                reasoning_params(provider, level).is_some(),
                "{provider:?} advertises `{level}` but sends nothing"
            );
        }
    }
}

/// Four providers on one codec is the reason this layer is small; the two
/// Anthropic backends are the exception, because Anthropic serves no Responses
/// endpoint — and they differ from each other only in where the credential
/// comes from, not in what it speaks.
#[test]
fn only_the_anthropic_backends_speak_messages() {
    for provider in Provider::ALL {
        let expected = match provider {
            Provider::Anthropic | Provider::ClaudeCode => Wire::Messages,
            _ => Wire::Responses,
        };
        assert_eq!(wire_for(provider), expected, "{provider:?}");
    }
}

#[test]
fn endpoint_urls_append_the_wires_path_to_the_root() {
    assert_eq!(
        endpoint_url(Provider::DeepSeek, None),
        "https://api.deepseek.com/v1/responses"
    );
    assert_eq!(
        endpoint_url(Provider::Anthropic, None),
        "https://api.anthropic.com/v1/messages"
    );
    assert_eq!(
        endpoint_url(Provider::Codex, None),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    // A configured root points the same wire at a proxy, trailing slash or
    // not.
    assert_eq!(
        endpoint_url(Provider::OpenAi, Some("http://localhost:8080/v1/")),
        "http://localhost:8080/v1/responses"
    );
}

/// A backend that reports which provider it was routed to.
struct Tagged(&'static str);

#[async_trait]
impl LlmClient for Tagged {
    async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
        Ok(self.0.to_string())
    }

    async fn resume_turn(
        &self,
        _session: &Session,
        _events: &[SessionEvent],
        _turn_id: &str,
        _deltas: Option<Arc<dyn DeltaSink>>,
        _recorder: Option<Arc<dyn TurnRecorder>>,
    ) -> anyhow::Result<Box<dyn TurnDriver>> {
        // Reports which backend the router picked, via the error text.
        anyhow::bail!("resumed-on:{}", self.0)
    }
}

use komo_core::domain::session_event::{
    ToolCallSettledEvent, ToolCallStartedEvent, ToolOutcome as SettledOutcome,
};

// ── rebuilding an interrupted turn from its events ───────────────────────

fn at(text: &str) -> time::OffsetDateTime {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).unwrap()
}

fn ev(seq: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent::new(seq, at("2026-09-01T10:00:00Z"), kind)
}

fn header_event(seq: u64) -> SessionEvent {
    ev(
        seq,
        SessionEventKind::RequestHeader(RequestHeaderEvent {
            reason: HeaderReason::Initial,
            provider: "openai".into(),
            model: "gpt-test".into(),
            effort: String::new(),
            system: "SYSTEM".into(),
            tools: vec![],
            extra: Some(json!({ "prompt_cache_key": "komo:s" })),
        }),
    )
}

fn round_event(seq: u64, text: &str, calls: &[&str]) -> SessionEvent {
    let mut blocks = vec![AssistantBlock::Reasoning(komo_provider::types::Reasoning {
        id: Some("rs".into()),
        summary: vec![],
        encrypted: Some("blob".into()),
        text: vec![],
    })];
    if !text.is_empty() {
        blocks.push(AssistantBlock::Text(text.into()));
    }
    for name in calls {
        blocks.push(AssistantBlock::ToolCall {
            id: format!("item-{name}"),
            call_id: Some(format!("call-{name}")),
            name: (*name).into(),
            args: "{}".into(),
        });
    }
    ev(
        seq,
        SessionEventKind::AssistantRound(AssistantRoundEvent {
            turn_id: "t1".into(),
            round: 0,
            response_id: "msg".into(),
            blocks: serde_json::to_value(&blocks).unwrap(),
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
        }),
    )
}

fn started_event(seq: u64, name: &str, index: u32) -> SessionEvent {
    ev(
        seq,
        SessionEventKind::ToolCallStarted(ToolCallStartedEvent {
            turn_id: "t1".into(),
            call_id: format!("call-{name}"),
            call_index: index,
            tool: name.into(),
            args: "{}".into(),
        }),
    )
}

fn settled_event(seq: u64, name: &str, index: u32, result: &str) -> SessionEvent {
    ev(
        seq,
        SessionEventKind::ToolCallSettled(ToolCallSettledEvent {
            turn_id: "t1".into(),
            call_id: format!("call-{name}"),
            call_index: index,
            outcome: SettledOutcome::Succeeded,
            result: result.into(),
            error: String::new(),
            elapsed_ms: 1,
            structured: Value::Null,
            output_paths: vec![],
        }),
    )
}

fn asked(text: &str) -> Session {
    let mut session = Session::new("s");
    session.messages.push(Message::user(text));
    session
}

/// A backend with nothing but the injections under test — enough to call
/// [`ProviderLlm::assemble`], which touches no network.
fn llm_with(injections: TurnInjections) -> ProviderLlm {
    ProviderLlm {
        client: Arc::new(ProviderClient {
            endpoint: Endpoint {
                url: "http://127.0.0.1:1/v1/responses".to_string(),
                auth: Auth::Bearer(String::new()),
                headers: Vec::new(),
                client: reqwest::Client::new(),
            },
            wire: Wire::Responses,
        }),
        tools: None,
        default_model: "m".to_string(),
        provider: Provider::OpenAi,
        default_effort: None,
        cache_family: None,
        preamble: Arc::new(|_| "you are komo".to_string()),
        max_history_messages: 0,
        max_history_bytes: 0,
        injections,
        timeout: None,
    }
}

/// A backend's default effort is what an aux turn runs at: every aux caller
/// builds a synthetic session whose overrides are empty.
#[test]
fn a_backend_default_effort_fills_in_for_a_session_that_named_none() {
    let sent = |provider: Provider, default: Option<&str>, chosen: &str| {
        let mut llm = llm_with(TurnInjections::default());
        llm.provider = provider;
        llm.default_effort = default.map(str::to_string);
        let mut session = Session::new("s");
        session.effort = chosen.to_string();
        llm.model_for("p".into(), &session)
            .extra
            .and_then(|extra| extra["reasoning"]["effort"].as_str().map(str::to_string))
    };

    assert_eq!(
        sent(Provider::DeepSeek, Some("none"), "").as_deref(),
        Some("none"),
        "the aux backend runs with thinking off"
    );
    assert_eq!(
        sent(Provider::OpenAi, Some("low"), "high").as_deref(),
        Some("high"),
        "a session's own choice wins over the backend default"
    );
    assert_eq!(
        sent(Provider::OpenAi, None, ""),
        None,
        "no default and no override sends no reasoning field at all"
    );
}

/// The artifacts directory is per session, so it rides at the **tail** of the
/// user message and never in the system prompt: the cache prefix is
/// tools → system → messages, and a per-session system tier would give every
/// conversation its own cold prefix.
#[tokio::test]
async fn the_artifacts_directory_reaches_the_model_after_the_user_message() {
    let store = Arc::new(ArtifactStore::new(std::path::PathBuf::from(
        "/komo/artifacts",
    )));
    let llm = llm_with(TurnInjections {
        enricher: None,
        artifacts: Some(store.clone()),
    });
    let session = asked("写个报告");
    let dir = store.session_dir(&session.id).display().to_string();

    let (preamble, prompt, _, _) = llm.assemble(&session).await.unwrap();
    assert!(prompt.contains(&dir), "{prompt}");
    assert!(
        prompt.starts_with("写个报告"),
        "the user's own words come first: {prompt}"
    );
    assert!(
        !preamble.contains(&dir),
        "a per-session path must stay out of the cached prefix"
    );
}

/// The other half of that split: the project instruction file belongs to the
/// task's workspace, which is fixed for the session's life — so it is built into
/// the system prompt, and the session's roots are what the builder renders it
/// from.
#[tokio::test]
async fn the_system_prompt_is_built_from_the_sessions_roots() {
    let mut llm = llm_with(TurnInjections::default());
    llm.preamble = Arc::new(|roots: &[String]| format!("roots: {roots:?}"));
    let mut session = asked("改一下这个函数");
    session.roots = vec!["/work/proj".to_string(), "/work/lib".to_string()];

    let (preamble, _, _, _) = llm.assemble(&session).await.unwrap();
    assert_eq!(preamble, r#"roots: ["/work/proj", "/work/lib"]"#);

    let (unbound, _, _, _) = llm.assemble(&asked("你好")).await.unwrap();
    assert_eq!(unbound, "roots: []", "an unbound session passes none");
}

/// A runtime that was not granted one is told nothing — an aux or delegate
/// sub-agent has no files of its own to leave behind.
#[tokio::test]
async fn a_runtime_without_an_artifacts_grant_says_nothing_about_it() {
    let llm = llm_with(TurnInjections::default());
    let (_, prompt, _, _) = llm.assemble(&asked("写个报告")).await.unwrap();
    assert_eq!(prompt, "写个报告");
}

/// A turn resumed **twice**: A died after a round, B (resumed from A) died
/// after another, and C picks up B. C has to replay both rounds — reading
/// only B's would send the model back to the question, so one crash would
/// be recoverable and two would not.
#[test]
fn a_second_resume_replays_every_attempt_before_it() {
    let session = asked("go");
    let opened = |seq: u64, turn: &str, from: Option<&str>| {
        ev(
            seq,
            SessionEventKind::TurnStarted {
                turn_id: turn.into(),
                resumed_from: from.map(str::to_string),
            },
        )
    };
    let round_of = |seq: u64, turn: &str, id: &str| {
        ev(
            seq,
            SessionEventKind::AssistantRound(AssistantRoundEvent {
                turn_id: turn.into(),
                round: 0,
                response_id: id.into(),
                blocks: serde_json::to_value(vec![AssistantBlock::Text("working".into())]).unwrap(),
                tokens_in: 0,
                tokens_out: 0,
                tokens_cached: 0,
            }),
        )
    };
    let events = vec![
        opened(0, "A", None),
        header_event(1),
        round_of(2, "A", "msg-a"),
        opened(3, "B", Some("A")),
        round_of(4, "B", "msg-b"),
        opened(5, "C", Some("B")),
    ];

    let rebuilt = rebuild_from_events(&session, &events, "C", &|_| false).unwrap();

    let replayed: Vec<String> = rebuilt
        .history
        .iter()
        .filter_map(|turn| match turn {
            Turn::Assistant { id, .. } => id.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(
        replayed,
        vec!["msg-a".to_string(), "msg-b".to_string()],
        "both attempts' rounds, in the order they happened"
    );
    assert_eq!(
        rebuilt.history.len(),
        3,
        "the question, then the two rounds"
    );
}

#[test]
fn a_rebuild_replays_the_rounds_and_their_results() {
    let session = asked("go");
    let events = vec![
        header_event(0),
        round_event(1, "", &["read"]),
        started_event(2, "read", 0),
        settled_event(3, "read", 0, "file contents"),
    ];
    let rebuilt = rebuild_from_events(&session, &events, "t1", &|_| false).unwrap();
    // The envelope came from the header, not from a stored copy of history.
    assert_eq!(rebuilt.model, "gpt-test");
    assert_eq!(rebuilt.preamble, "SYSTEM");
    assert!(rebuilt.extra.is_some());
    // user → assistant round → its results.
    assert_eq!(rebuilt.history.len(), 3);
    assert!(matches!(rebuilt.history[1], Turn::Assistant { .. }));
    assert!(matches!(rebuilt.start, TurnStart::Continue));
}

#[test]
fn results_rebuild_in_call_order_not_in_the_order_they_settled() {
    // A round runs concurrently, so settle order is completion order.
    // Rebuilding in it would hand the provider a different request than the
    // live turn sent — and `rebuild == live` is the whole point.
    let session = asked("go");
    let events = vec![
        header_event(0),
        round_event(1, "", &["a", "b", "c"]),
        started_event(2, "a", 0),
        started_event(3, "b", 1),
        started_event(4, "c", 2),
        settled_event(5, "c", 2, "third"),
        settled_event(6, "a", 0, "first"),
        settled_event(7, "b", 1, "second"),
    ];
    let rebuilt = rebuild_from_events(&session, &events, "t1", &|_| false).unwrap();
    let Some(Turn::User(blocks)) = rebuilt.history.last() else {
        panic!("the round's results close the history");
    };
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            UserBlock::ToolResult { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["first", "second", "third"]);
}

#[test]
fn a_call_that_never_settled_comes_back_as_an_uncertain_outcome() {
    // The tool is *not* re-run: a mutation cannot be assumed idempotent, so
    // whether to re-issue it is the model's decision — told plainly.
    let session = asked("go");
    let events = vec![
        header_event(0),
        round_event(1, "", &["a", "b"]),
        started_event(2, "a", 0),
        started_event(3, "b", 1),
        settled_event(4, "a", 0, "landed"),
    ];
    let rebuilt = rebuild_from_events(&session, &events, "t1", &|_| false).unwrap();
    let Some(Turn::User(blocks)) = rebuilt.history.last() else {
        panic!("results close the history");
    };
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            UserBlock::ToolResult { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts[0], "landed", "a settled call keeps its real result");
    assert!(texts[1].contains("interrupted"));
    assert!(matches!(rebuilt.start, TurnStart::Continue));
}

#[test]
fn an_unsettled_idempotent_call_is_re_dispatched_instead_of_reported_lost() {
    // The interrupted turn's own answer to "did this run?" — for a tool
    // that can simply be run again, re-running it is cheaper and more
    // accurate than spending a model round to say the result was lost.
    let session = asked("go");
    let events = vec![
        header_event(0),
        round_event(1, "", &["a", "b"]),
        started_event(2, "a", 0),
        started_event(3, "b", 1),
        settled_event(4, "a", 0, "landed"),
    ];
    let rebuilt = rebuild_from_events(&session, &events, "t1", &|name| name == "b").unwrap();
    let TurnStart::Replay { calls, slots } = rebuilt.start else {
        panic!("the unsettled idempotent call should be re-dispatched");
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "b");
    // The round's results message is held back whole: it is one message and
    // it cannot be sent until every call in it has answered.
    assert!(matches!(
        rebuilt.history.last(),
        Some(Turn::Assistant { .. })
    ));
    assert_eq!(slots.len(), 2);
    assert!(slots[0].1.is_some(), "the settled call keeps its result");
    assert!(slots[1].1.is_none(), "the replayed call leaves a hole");
}

#[test]
fn a_non_idempotent_call_in_the_same_round_still_gets_the_note() {
    // Replay is per tool, not per round: one call being safe to repeat says
    // nothing about the one beside it.
    let session = asked("go");
    let events = vec![
        header_event(0),
        round_event(1, "", &["read", "shell"]),
        started_event(2, "read", 0),
        started_event(3, "shell", 1),
    ];
    let rebuilt = rebuild_from_events(&session, &events, "t1", &|name| name == "read").unwrap();
    let TurnStart::Replay { calls, slots } = rebuilt.start else {
        panic!("expected a replay");
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "read");
    let Some(UserBlock::ToolResult { text, .. }) = &slots[1].1 else {
        panic!("the non-idempotent call keeps a recorded result");
    };
    assert!(text.contains("interrupted"));
}

#[test]
fn only_the_newest_round_is_replayed() {
    // An earlier round's results must have been sent, or the round after it
    // would not exist. An unsettled call there is a lost event, not work in
    // flight, and re-running it would change a request already answered.
    let session = asked("go");
    let events = vec![
        header_event(0),
        round_event(1, "", &["a"]),
        started_event(2, "a", 0),
        round_event(3, "", &["b"]),
        started_event(4, "b", 1),
    ];
    let rebuilt = rebuild_from_events(&session, &events, "t1", &|_| true).unwrap();
    let TurnStart::Replay { calls, .. } = rebuilt.start else {
        panic!("expected a replay of the newest round");
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "b");
    // The stale round closed with the note rather than being re-run.
    let stale = rebuilt
        .history
        .iter()
        .filter_map(|turn| match turn {
            Turn::User(blocks) => Some(blocks),
            _ => None,
        })
        .nth(1)
        .expect("the first round's results are in history");
    assert!(matches!(
        stale.first(),
        Some(UserBlock::ToolResult { text, .. }) if text.contains("interrupted")
    ));
}

#[test]
fn a_replayed_round_closes_in_the_order_the_model_issued_the_calls() {
    // Not in the order the results arrived: the message has to match the
    // one the interrupted turn was assembling.
    let block = |id: &str, text: &str| UserBlock::ToolResult {
        id: id.into(),
        call_id: Some(id.into()),
        text: text.into(),
    };
    let slots: Vec<ReplaySlot> = vec![
        ("a".into(), Some(block("a", "first"))),
        ("b".into(), None),
        ("c".into(), Some(block("c", "third"))),
        ("d".into(), None),
    ];
    // The replay answered d before b.
    let fresh = vec![block("d", "fourth"), block("b", "second")];
    let texts: Vec<String> = fill_replay_slots(slots, fresh)
        .into_iter()
        .map(|b| match b {
            UserBlock::ToolResult { text, .. } => text,
            _ => panic!("results only"),
        })
        .collect();
    assert_eq!(texts, vec!["first", "second", "third", "fourth"]);
}

#[test]
fn a_replayed_result_is_matched_by_call_id_not_item_id() {
    // On the Responses wire the executor answers with the `fc_…` item id
    // and the `call_…` call id; the slot is keyed by the latter.
    let slots: Vec<ReplaySlot> = vec![("call_1".into(), None)];
    let fresh = vec![UserBlock::ToolResult {
        id: "fc_1".into(),
        call_id: Some("call_1".into()),
        text: "ok".into(),
    }];
    let blocks = fill_replay_slots(slots, fresh);
    assert_eq!(blocks.len(), 1);
    assert!(matches!(
        &blocks[0],
        UserBlock::ToolResult { text, .. } if text == "ok"
    ));
    assert!(!blocks.iter().any(|b| matches!(
        b,
        UserBlock::ToolResult { text, .. } if text.contains("interrupted")
    )));
}

#[test]
fn a_replayed_call_that_came_back_with_nothing_still_fills_its_slot() {
    // Unreachable in practice — the executor answers every call it is
    // handed — but an open hole is the one shape a provider rejects.
    let slots: Vec<ReplaySlot> = vec![("a".into(), None)];
    let blocks = fill_replay_slots(slots, vec![]);
    assert_eq!(blocks.len(), 1);
    assert!(matches!(
        &blocks[0],
        UserBlock::ToolResult { text, .. } if text.contains("interrupted")
    ));
}

#[test]
fn a_turn_that_had_already_answered_hands_the_answer_back() {
    // The reply was produced and only the ledger close was lost. Re-issuing
    // the request would pay for an answer that already exists.
    let session = asked("go");
    let events = vec![header_event(0), round_event(1, "all done", &[])];
    let rebuilt = rebuild_from_events(&session, &events, "t1", &|_| false).unwrap();
    match rebuilt.start {
        TurnStart::Final(text) => assert_eq!(text, "all done"),
        TurnStart::Continue => panic!("expected the answer back, not a continuation"),
        _ => panic!("expected the answer back"),
    }
}

/// A turn the runtime nudged and that then stopped for an approval must
/// rebuild to the history the live turn had. The nudged round called
/// nothing, so it contributes no results message — without the recorded
/// nudge the rebuild would hand the provider two assistant turns in a row.
#[test]
fn a_nudged_round_replays_the_runtime_message_that_followed_it() {
    let session = asked("打开热水器");
    let events = vec![
        header_event(0),
        round_event(1, "热水器已打开 ✅", &[]),
        ev(
            2,
            SessionEventKind::UserMessage(UserMessageEvent {
                turn_id: "t1".into(),
                content: "Runtime check: this turn issued no tool call.".into(),
                source: MessageSource::Runtime,
                surface: SurfacePlacement::append(),
            }),
        ),
        round_event(3, "", &["homeassistant"]),
        ev(
            4,
            SessionEventKind::ApprovalRequested(
                komo_core::domain::session_event::ApprovalRequestedEvent {
                    turn_id: "t1".into(),
                    call_id: "call-homeassistant".into(),
                    call_index: 0,
                    scope_key: String::new(),
                },
            ),
        ),
    ];

    let rebuilt = rebuild_from_events(&session, &events, "t1", &|_| false).unwrap();

    let shape: Vec<&str> = rebuilt
        .history
        .iter()
        .map(|turn| match turn {
            Turn::User(_) => "user",
            Turn::Assistant { .. } => "assistant",
        })
        .collect();
    assert_eq!(shape, vec!["user", "assistant", "user", "assistant"]);
    let Some(Turn::User(blocks)) = rebuilt.history.get(2) else {
        panic!("the nudge stands between the two rounds");
    };
    assert!(matches!(
        &blocks[0],
        UserBlock::Text(text) if text.starts_with("Runtime check:")
    ));
    // The gated call never ran, so the last round is re-dispatched.
    let TurnStart::Replay { calls, .. } = rebuilt.start else {
        panic!("expected the gated call to be replayed");
    };
    assert_eq!(calls[0].name, "homeassistant");
}

#[test]
fn a_turn_with_no_recorded_header_cannot_be_rebuilt() {
    // Without the envelope the continuation would be a *different* request:
    // a re-assembled prompt whose memory recall and clock have moved on.
    let session = asked("go");
    assert!(rebuild_from_events(&session, &[round_event(0, "hi", &[])], "t1", &|_| false).is_err());
}

#[test]
fn another_turns_events_are_not_replayed_into_this_one() {
    let session = asked("go");
    let mut other = round_event(1, "", &["read"]);
    if let SessionEventKind::AssistantRound(round) = &mut other.kind {
        round.turn_id = "t0".into();
    }
    let events = vec![header_event(0), other];
    let rebuilt = rebuild_from_events(&session, &events, "t1", &|_| false).unwrap();
    assert_eq!(rebuilt.history.len(), 1, "only the user message survives");
}

#[tokio::test]
async fn resume_routes_on_the_recorded_provider_not_the_session() {
    let router = router();
    // The session says codex (the default), but the interrupted turn ran on
    // deepseek — continuing it anywhere else would replay one provider's
    // opaque state into another.
    let events = vec![ev(
        0,
        SessionEventKind::RequestHeader(RequestHeaderEvent {
            reason: HeaderReason::Initial,
            provider: "deepseek".into(),
            model: "deepseek-v4".into(),
            effort: String::new(),
            system: String::new(),
            tools: vec![],
            extra: None,
        }),
    )];
    let error = router
        .resume_turn(
            &session_on("codex:gpt-5.3-codex"),
            &events,
            "t1",
            None,
            None,
        )
        .await
        .err()
        .expect("the tagged stub always errors");
    assert!(error.to_string().contains("resumed-on:deepseek"));
}

fn router() -> RoutingLlm {
    router_on(Provider::Codex, Provider::Codex)
}

/// A router whose configured default and whose config's own model sit on
/// different providers — what an aux/memory variant looks like on a DeepSeek
/// conversation, and the case `own_provider` exists for.
fn router_on(default_provider: Provider, own_provider: Provider) -> RoutingLlm {
    RoutingLlm {
        by_provider: vec![
            (
                Provider::Codex,
                Arc::new(Tagged("codex")) as Arc<dyn LlmClient>,
            ),
            (
                Provider::DeepSeek,
                Arc::new(Tagged("deepseek")) as Arc<dyn LlmClient>,
            ),
        ],
        default_provider,
        own_provider,
    }
}

fn session_on(model: &str) -> Session {
    let mut session = Session::new("s");
    session.model = model.to_string();
    session
}

#[tokio::test]
async fn a_qualified_id_routes_to_that_provider() {
    let router = router();
    assert_eq!(
        router
            .complete(&session_on("deepseek:deepseek-chat"))
            .await
            .unwrap(),
        "deepseek"
    );
}

#[tokio::test]
async fn an_unqualified_id_stays_on_the_default_provider() {
    let router = router();
    for model in ["gpt-5.5", "codex:gpt-5.6-sol"] {
        assert_eq!(
            router.complete(&session_on(model)).await.unwrap(),
            "codex",
            "model {model:?}"
        );
    }
}

#[tokio::test]
async fn a_session_with_no_model_runs_the_configs_own_model() {
    // Every aux caller — the memory pipeline's reviewer, the consolidator, the
    // verdict, recall screening — builds a session with empty overrides, so this
    // is the *only* routing decision those turns ever make. Sending them to the
    // configured provider regardless would run the memory pipeline on the
    // conversation's backend while `komo model memory` reported Codex.
    let router = router_on(Provider::DeepSeek, Provider::Codex);
    assert_eq!(router.complete(&session_on("")).await.unwrap(), "codex");
    assert_eq!(
        router.complete(&Session::new("s")).await.unwrap(),
        "codex",
        "a session that never picked a model is the same case"
    );
    // A bare id is *not* that case: it belongs to the configured provider.
    assert_eq!(
        router
            .complete(&session_on("deepseek-v4-flash"))
            .await
            .unwrap(),
        "deepseek"
    );
}

#[tokio::test]
async fn a_provider_with_no_backend_falls_back_to_the_default() {
    // Config can change under a stored session (a key removed, an entry
    // dropped), and running on the default beats failing the turn.
    let router = router();
    assert_eq!(
        router
            .complete(&session_on("anthropic:claude-sonnet-4-5"))
            .await
            .unwrap(),
        "codex"
    );
}

fn turn(user: &str, assistant: &str, note: &str) -> Vec<Message> {
    vec![
        Message::user(user),
        Message::assistant(assistant).with_tool_note(note),
    ]
}

fn retryable(message: &str) -> LlmError {
    LlmError::new(LlmErrorKind::Overloaded, message)
}

/// A rate limit takes seconds to clear, so the local fallback table has to
/// outlast one: four attempts spanning 21s, versus the three-in-2.5s that
/// used to run out while the limit was still in force.
#[tokio::test(start_paused = true)]
async fn the_retry_budget_outlasts_a_rate_limit() {
    assert_eq!(LLM_RETRY_MAX_ATTEMPTS, LLM_RETRY_BACKOFF_MS.len() + 1);
    assert!(
        LLM_RETRY_BACKOFF_MS.iter().sum::<u64>() >= 20_000,
        "the table has to cover a rate limit that lasts tens of seconds"
    );

    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let result = with_retry(|| async {
        let n = attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 3 {
            return Err(retryable("429 Too Many Requests"));
        }
        Ok("answered")
    })
    .await
    .unwrap();
    assert_eq!(result, "answered");
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::Relaxed),
        LLM_RETRY_MAX_ATTEMPTS,
        "a fourth attempt has to exist for the last backoff to be worth having"
    );
}

/// The payoff of carrying `retry_after` on the error: when the server says
/// when its limit clears, we wait exactly that long instead of guessing.
#[tokio::test(start_paused = true)]
async fn a_servers_own_delay_beats_the_local_table() {
    let started = tokio::time::Instant::now();
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let result = with_retry(|| async {
        let n = attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n == 0 {
            return Err(LlmError::new(LlmErrorKind::RateLimited, "slow down")
                .with_retry_after(Some(Duration::from_millis(200))));
        }
        Ok(())
    })
    .await;
    assert!(result.is_ok());
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_millis(900),
        "waited {waited:?} — the server said 200ms, the table's first entry is 1s"
    );
}

#[tokio::test(start_paused = true)]
async fn a_terminal_failure_is_not_retried() {
    // An auth or schema error will fail identically forever; retrying it just
    // delays the message the user needs to see.
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let error = with_retry(|| async {
        attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err::<(), _>(LlmError::new(LlmErrorKind::Auth, "invalid api key"))
    })
    .await
    .expect_err("terminal errors surface");
    assert!(error.message.contains("invalid api key"));
    assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 1);
}

/// An overflow is recoverable, but not by re-sending: the driver has to
/// shrink the history first, so the retry layer must pass it straight
/// through to the degrade path.
#[tokio::test(start_paused = true)]
async fn an_overflow_is_not_retried_but_reaches_the_driver() {
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let error = with_retry(|| async {
        attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err::<(), _>(LlmError::new(LlmErrorKind::ContextOverflow, "too long"))
    })
    .await
    .expect_err("an overflow surfaces");
    assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(error.is_context_overflow());
}

#[tokio::test(start_paused = true)]
async fn completion_retries_are_bounded() {
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let _ = with_retry(|| async {
        attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err::<(), _>(LlmError::transport("connection refused"))
    })
    .await
    .expect_err("a permanently down provider still fails");
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::Relaxed),
        LLM_RETRY_MAX_ATTEMPTS
    );
}

/// The retry budget lives *inside* the timeout, so a flapping provider can't
/// multiply a turn's worst-case latency by the attempt count.
#[tokio::test(start_paused = true)]
async fn the_timeout_bounds_every_retry_together() {
    let started = tokio::time::Instant::now();
    let error = with_timeout(
        Some(Duration::from_secs(1)),
        with_retry(|| async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Err::<(), _>(LlmError::transport("connection refused"))
        }),
    )
    .await
    .expect_err("the round times out");
    assert_eq!(error.kind, LlmErrorKind::Timeout);
    assert!(
        !error.is_retryable(),
        "the budget it exceeded covers every attempt, so there is nothing left to retry"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

fn tool_result_turn(text: &str) -> Turn {
    Turn::User(vec![UserBlock::ToolResult {
        id: "call-1".into(),
        call_id: Some("call-1".into()),
        text: text.into(),
    }])
}

/// The usual overflow is one turn that read several large things. Shrinking
/// those reclaims the context without touching the conversation, and the
/// full text is still on disk in the tool-output store.
#[test]
fn reclaiming_shrinks_bulky_tool_results_and_keeps_the_conversation() {
    let mut history = vec![
        Turn::user("find the bug"),
        tool_result_turn(&"x".repeat(200_000)),
        Turn::assistant("reading further"),
    ];
    let before = history.len();

    assert!(reclaim_context(&mut history));
    assert_eq!(history.len(), before, "no message is dropped");
    let rendered = format!("{history:?}");
    assert!(rendered.contains("elided"), "the big result was shrunk");
    assert!(rendered.contains("find the bug"), "the ask is still there");
}

/// When there is nothing bulky to shrink the weight is the conversation
/// itself, so the oldest half goes — and what is left still opens on a user
/// message, which several providers require.
#[test]
fn reclaiming_falls_back_to_dropping_the_oldest_half() {
    let mut history: Vec<Turn> = (0..8)
        .flat_map(|i| {
            [
                Turn::user(format!("q{i}")),
                Turn::assistant(format!("a{i}")),
            ]
        })
        .collect();

    assert!(reclaim_context(&mut history));
    assert!(history.len() <= 8);
    assert!(
        !matches!(history.first(), Some(Turn::Assistant { .. })),
        "history must still open on a user message"
    );
    let rendered = format!("{history:?}");
    assert!(!rendered.contains("q0"), "the oldest exchange is gone");
    assert!(rendered.contains("q7"), "the newest is kept");
}

/// The cut can land inside a tool round; the kept half must not open on
/// tool results whose function_calls went with the dropped half — strict
/// providers (DeepSeek) reject such orphans with a 400.
#[test]
fn reclaiming_never_leaves_orphaned_tool_results_at_the_window_head() {
    let call_turn = Turn::Assistant {
        id: None,
        blocks: vec![AssistantBlock::ToolCall {
            id: "fc_1".into(),
            call_id: Some("call_1".into()),
            name: "read".into(),
            args: "{}".into(),
        }],
    };
    let mut history = vec![
        Turn::user("q0"),
        Turn::assistant("a0"),
        Turn::user("q1"),
        call_turn,
        // Small on purpose: big results are shrunk by the first step and
        // never reach the half-drop this test is about.
        tool_result_turn("ok"),
        Turn::assistant("a1"),
        Turn::user("q2"),
        Turn::assistant("a2"),
    ];

    assert!(reclaim_context(&mut history));
    // The cut (len 8 → 4) lands right between the call and its result.
    let rendered = format!("{history:?}");
    assert!(
        !rendered.contains("ToolResult"),
        "no orphaned tool result may survive: {rendered}"
    );
    assert!(
        matches!(history.first(), Some(Turn::User(blocks))
                if matches!(blocks.first(), Some(UserBlock::Text(t)) if t == "q2")),
        "the window must open on real user text: {rendered}"
    );
}

/// Nothing left to give: the caller has to surface the failure rather than
/// re-send an empty request.
#[test]
fn reclaiming_reports_failure_when_there_is_nothing_to_reclaim() {
    let mut history = vec![Turn::user("hi")];
    assert!(!reclaim_context(&mut history));
    assert_eq!(history.len(), 1);
}

#[test]
fn head_tail_keeps_both_ends_and_cuts_on_char_boundaries() {
    let text = "前".repeat(4_000); // 12 KB of 3-byte chars
    let cut = head_tail(&text, 1_024);
    assert!(cut.len() < text.len() / 4);
    assert!(cut.starts_with('前') && cut.ends_with('前'));
    assert!(cut.contains("elided"));
}

/// A backend whose sessions are one-shot but whose prompt prefix never
/// changes has to key the cache by that prefix, or every delegation and
/// every cron firing pays a cold start. The main agent keeps keying by
/// session, where one conversation really is one prefix.
#[test]
fn a_declared_family_keys_the_cache_instead_of_the_one_shot_session() {
    let first = cache_key(Some("delegate"), "delegate:0198-aaaa");
    let second = cache_key(Some("delegate"), "delegate:0198-bbbb");
    assert_eq!(first, second, "two delegations share one warm prefix");

    let a = cache_key(None, "telegram:644");
    let b = cache_key(None, "telegram:900");
    assert_ne!(a, b, "separate conversations must not share a key");
    assert_eq!(a, "komo:telegram:644");
}

#[test]
fn the_byte_budget_trims_where_the_count_window_cannot() {
    // Two turns, the first carrying a pasted log. Both fit the count window,
    // so only the byte bound can keep the big one out — the case the count
    // window was blind to.
    let mut prior = turn("here is the log", &"x".repeat(5_000), "");
    prior.extend(turn("and now?", "short answer", ""));

    let counted = window_history(&prior, 50, 0);
    assert_eq!(counted.len(), 4, "the count window keeps everything");

    let bounded = window_history(&prior, 50, 1_000);
    assert_eq!(
        bounded.iter().map(|m| &m.content).collect::<Vec<_>>(),
        vec!["and now?", "short answer"],
        "the oversized turn is trimmed from the oldest end"
    );
}

#[test]
fn a_window_never_opens_on_an_assistant_message() {
    // Whichever bound makes the cut, a leading assistant message must go:
    // Anthropic rejects one outright.
    let mut prior = turn("q1", "a1", "");
    prior.extend(turn("q2", "a2", ""));

    for window in [
        window_history(&prior, 3, 0), // count cut lands on "a1"
        window_history(&prior, 0, 5), // byte cut lands on "a1"
    ] {
        assert_eq!(
            window.first().map(|m| m.role.clone()),
            Some(Role::User),
            "history must start on a user turn, got {:?}",
            window.first().map(|m| &m.content)
        );
    }
}

#[test]
fn the_window_cut_holds_still_between_anchor_jumps() {
    // A long transcript with fixed timestamps (anchor selection hashes
    // stored bytes, so pinning them makes the test deterministic), replayed
    // as a growing session at the count cap — the shape of every long-lived
    // chat session. Without the anchor snap the window start advances every
    // turn and the replayed prefix is never the same twice.
    let mut all = Vec::new();
    for i in 0..120 {
        let mut user = Message::user(format!("question number {i}"));
        user.timestamp = 1_700_000_000 + i;
        let mut assistant = Message::assistant(format!("answer number {i}"));
        assistant.timestamp = 1_700_000_000 + i;
        all.push(user);
        all.push(assistant);
    }

    let max = 50;
    let mut starts = Vec::new();
    for end in (max..=all.len()).step_by(2) {
        // What the DB layer hands the adapter each turn: the last `max`.
        let prior = &all[end - max..end];
        let window = window_history(prior, max, 0);
        assert!(window.len() <= max, "budget respected");
        assert!(!window.is_empty(), "the cut must not starve the model");
        assert_eq!(window.first().unwrap().role, Role::User);
        starts.push(window.first().unwrap().content.clone());
    }

    let turns = starts.len();
    let changes = starts.windows(2).filter(|w| w[0] != w[1]).count();
    assert!(
        changes * 2 < turns,
        "window start moved {changes} times over {turns} turns — \
             the anchor snap should hold it still most turns"
    );
}

#[test]
fn the_byte_budget_is_off_at_zero_and_counts_tool_notes() {
    let prior = turn("q", "a", &"n".repeat(5_000));
    assert_eq!(window_history(&prior, 0, 0).len(), 2, "0 = unlimited");
    // The note is real context sent to the model, so it has to be weighed.
    assert!(
        window_history(&prior, 0, 1_000).is_empty(),
        "a note over the budget must be trimmed like content"
    );
}

/// The prefix-cache invariant `assemble` documents: a stored message renders
/// the same bytes no matter where it sits, so growing the conversation never
/// rewrites an earlier message. This used to fail — only the last three
/// note-bearing turns carried their note, so every tool turn silently
/// changed an older message and cost the cache everything after it.
#[test]
fn a_message_renders_the_same_bytes_wherever_it_sits() {
    let mut prior = Vec::new();
    for i in 0..5 {
        prior.extend(turn(
            &format!("q{i}"),
            &format!("a{i}"),
            &format!("note{i}"),
        ));
    }
    let render = |msgs: &[Message]| -> Vec<String> {
        msgs.iter()
            .flat_map(to_turns)
            .map(|m| format!("{m:?}"))
            .collect()
    };

    // Every note is carried, oldest included — nothing ages out.
    let early = render(&prior);
    for i in 0..5 {
        let note = format!("note{i}");
        assert!(
            early.iter().any(|m| m.contains(&note)),
            "{note} must ride along"
        );
    }

    // Two more turns arrive. The messages that were already there must
    // render byte-identically; only the new ones are added.
    prior.extend(turn("q5", "a5", "note5"));
    prior.extend(turn("q6", "a6", "note6"));
    let later = render(&prior);
    assert_eq!(
        early,
        later[..early.len()],
        "appending a turn must not rewrite an earlier message"
    );
}

#[test]
fn a_tool_note_never_touches_the_user_visible_content() {
    let msg = Message::assistant("the answer").with_tool_note("[tools used] read foo.rs");
    // The model sees the note; `content` stays exactly the reply.
    let rendered: Vec<String> = to_turns(&msg).iter().map(|t| format!("{t:?}")).collect();
    assert!(rendered.iter().any(|t| t.contains("the answer")));
    assert!(rendered.iter().any(|t| t.contains("read foo.rs")));
    // And the stored message itself is untouched — every client renders this.
    assert_eq!(msg.content, "the answer");
    let plain = Message::assistant("just talk");
    let rendered = format!("{:?}", to_turns(&plain)[0]);
    assert!(rendered.contains("just talk"));
}

/// The regression this whole change exists for: a tool digest replayed inside
/// the assistant's own text is a worked example of narrating tool calls in
/// prose, and a model that copies it answers from invented results with no
/// tool steps in the ledger. The digest must reach the model as the *other*
/// speaker's words, and the assistant turn must carry nothing but the reply.
#[test]
fn a_tool_note_reaches_the_model_as_a_user_turn() {
    let msg = Message::assistant("the answer").with_tool_note("[tools used] read foo.rs");
    let turns = to_turns(&msg);
    assert_eq!(turns.len(), 2, "reply plus digest: {turns:?}");
    match &turns[0] {
        Turn::Assistant { .. } => {
            let rendered = format!("{:?}", turns[0]);
            assert!(
                !rendered.contains("read foo.rs"),
                "the digest must not ride in the assistant turn: {rendered}"
            );
        }
        other => panic!("expected the reply first, got {other:?}"),
    }
    match &turns[1] {
        Turn::User(_) => {
            let rendered = format!("{:?}", turns[1]);
            assert!(rendered.contains("read foo.rs"), "{rendered}");
        }
        other => panic!("expected the digest as a user turn, got {other:?}"),
    }
}

#[test]
fn text_alongside_tool_calls_survives_the_step_split() {
    let blocks = vec![
        AssistantBlock::Text("Let me check the config first.".into()),
        AssistantBlock::ToolCall {
            id: "call-1".into(),
            call_id: Some("call-1".into()),
            name: "read".into(),
            args: r#"{"path":"config.toml"}"#.into(),
        },
    ];

    match blocks_to_step(&blocks) {
        Step::ToolCalls { calls, text } => {
            assert_eq!(calls.len(), 1);
            assert_eq!(text, "Let me check the config first.");
        }
        Step::Final(_) => panic!("a tool call must not read as a final answer"),
    }
}

/// Reasoning is echoed back into history verbatim but must not be mistaken
/// for the model's answer — a round that only reasoned is not a final reply.
#[test]
fn reasoning_never_becomes_the_answer() {
    let blocks = vec![
        AssistantBlock::Reasoning(komo_provider::types::Reasoning {
            id: Some("rs_1".into()),
            summary: vec![],
            encrypted: None,
            // DeepSeek's shape: the reasoning itself, no summary, no blob.
            text: vec!["thinking".into()],
        }),
        AssistantBlock::Text("the answer".into()),
    ];
    match blocks_to_step(&blocks) {
        Step::Final(text) => assert_eq!(text, "the answer"),
        Step::ToolCalls { .. } => panic!("no tool was called"),
    }
}

#[test]
fn merging_params_keeps_unrelated_keys_and_overrides_collisions() {
    let merged = merge_params(
        Some(json!({ "store": false, "reasoning": { "effort": "low" } })),
        json!({ "reasoning": { "effort": "high" } }),
    );
    assert_eq!(merged["store"], false);
    assert_eq!(merged["reasoning"]["effort"], "high");
    // No prior params, or a non-object one, is simply replaced.
    assert_eq!(merge_params(None, json!({ "a": 1 })), json!({ "a": 1 }));
    assert_eq!(
        merge_params(Some(Value::Null), json!({ "a": 1 })),
        json!({ "a": 1 })
    );
}
