use super::*;

#[test]
fn stateful_input_takes_last_user_message() {
    let messages = vec![
        ChatMessage {
            role: "user".into(),
            content: "first".into(),
        },
        ChatMessage {
            role: "assistant".into(),
            content: "reply".into(),
        },
        ChatMessage {
            role: "user".into(),
            content: "second".into(),
        },
    ];
    assert_eq!(build_input(&messages, true), "second");
}

#[test]
fn stateless_input_flattens_conversation() {
    let messages = vec![
        ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        },
        ChatMessage {
            role: "assistant".into(),
            content: "hello".into(),
        },
    ];
    assert_eq!(
        build_input(&messages, false),
        "user: hi\n\nassistant: hello"
    );
}

#[test]
fn resolve_session_is_ephemeral_without_header() {
    let headers = axum::http::HeaderMap::new();
    let (id, stateful) = resolve_session(&headers).expect("no header is not an error");
    assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id}");
    assert!(!stateful);
}

#[test]
fn resolve_session_uses_a_uuid_header_verbatim() {
    // One form, everywhere: what the client sends is the id komo stores and
    // the id `komo resume` takes. Nothing is added and nothing is stripped.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "x-komo-session-id",
        "019fad15-8199-7461-9d48-0a6c779f1c8d".parse().unwrap(),
    );
    let (id, stateful) = resolve_session(&headers).expect("a uuid is accepted");
    assert_eq!(id, "019fad15-8199-7461-9d48-0a6c779f1c8d");
    assert!(stateful);
}

/// The `api:` prefix used to wrap whatever a client sent, which quietly kept
/// it inside its own namespace. Without the wrapper the header *is* the
/// session id, so a caller that could name anything could address another
/// ingress's conversation — and be evaluated against that channel's
/// permission scope and write into its memory scope. Requiring a UUID is
/// what replaces the wrapper.
#[test]
fn resolve_session_refuses_an_id_that_is_not_a_uuid() {
    for forged in [
        "feishu:oc_abc",
        "panel-1",
        "api:019fad15-8199-7461-9d48-0a6c779f1c8d",
        "../../etc/passwd",
    ] {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-komo-session-id", forged.parse().unwrap());
        let rejected = resolve_session(&headers);
        assert!(rejected.is_err(), "{forged} must be refused");
        assert_eq!(
            rejected.err().unwrap().status,
            StatusCode::BAD_REQUEST,
            "{forged} is the caller's mistake, not a server fault"
        );
    }
}

#[test]
fn local_origins_are_allowed_for_cors() {
    for origin in [
        "http://127.0.0.1:5273", // Electron renderer, vite dev
        "http://localhost:5274", // web build, vite dev
        "http://[::1]:5273",     // IPv6 loopback
        "http://127.0.0.2:8080", // any loopback address
        "null",                  // packaged Electron (file://)
    ] {
        assert!(
            is_local_origin(&HeaderValue::from_static(origin)),
            "{origin} should be allowed"
        );
    }
}

#[test]
fn remote_origins_are_refused_for_cors() {
    for origin in [
        "https://evil.example",
        "http://127.0.0.1.evil.example", // loopback as a subdomain label
        "http://192.168.1.20:5273",      // LAN, not loopback
        "file://",                       // not an origin we grant
        "ws://127.0.0.1:5273",           // only http(s) is granted
        "http://[::1",                   // malformed
    ] {
        assert!(
            !is_local_origin(&HeaderValue::from_static(origin)),
            "{origin} should be refused"
        );
    }
}

/// A router shaped like the real one: a protected route whose auth layer
/// rejects everything, with the CORS layer outermost.
fn cors_test_router() -> Router {
    Router::new()
        .route("/api/status", get(|| async { "ok" }))
        .route_layer(middleware::from_fn(
            |_req: Request, _next: Next| async move { StatusCode::UNAUTHORIZED },
        ))
        .layer(cors_layer())
}

fn preflight(origin: &str) -> Request {
    Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/status")
        .header("origin", origin)
        .header("access-control-request-method", "GET")
        .header("access-control-request-headers", "authorization")
        .body(axum::body::Body::empty())
        .unwrap()
}

#[tokio::test]
async fn preflight_is_answered_before_auth() {
    use tower::ServiceExt;

    // The whole point of layering CORS outermost: an `Authorization`-bearing
    // request is preflighted, and if the preflight reached the bearer-key
    // middleware it would 401 — killing the real request behind it.
    let res = cors_test_router()
        .oneshot(preflight("http://127.0.0.1:5273"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("access-control-allow-origin").unwrap(),
        "http://127.0.0.1:5273"
    );
    let allowed = res.headers().get("access-control-allow-headers").unwrap();
    assert!(allowed.to_str().unwrap().contains("authorization"));
}

#[tokio::test]
async fn preflight_from_a_remote_origin_gets_no_grant() {
    use tower::ServiceExt;

    let res = cors_test_router()
        .oneshot(preflight("https://evil.example"))
        .await
        .unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
}

#[test]
fn folder_workspace_id_resolves_an_existing_directory() {
    let path = std::env::current_dir().unwrap().canonicalize().unwrap();
    let encoded =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(path.to_string_lossy().as_bytes());
    assert_eq!(
        resolve_folder_workspace(&format!("folder:{encoded}")),
        Some(path)
    );
}

#[test]
fn folder_workspace_id_rejects_non_directories_and_garbage() {
    assert_eq!(resolve_folder_workspace("not-a-folder-id"), None);
    assert_eq!(resolve_folder_workspace("folder:not+base64"), None);

    // A path that doesn't exist: the id decodes, the canonicalize doesn't.
    let missing = std::env::temp_dir().join(format!("komo-missing-{}", uuid::Uuid::now_v7()));
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(missing.to_string_lossy().as_bytes());
    assert_eq!(resolve_folder_workspace(&format!("folder:{encoded}")), None);

    // An existing *file* is not a workspace either.
    let file = std::env::current_dir().unwrap().join("Cargo.toml");
    let encoded =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(file.to_string_lossy().as_bytes());
    assert_eq!(resolve_folder_workspace(&format!("folder:{encoded}")), None);
}

/// A task session is bound on its first turn and honors that binding forever
/// after: continuing it from another directory continues the *task*, it does
/// not move where its tools write.
#[test]
fn a_bound_session_ignores_the_header() {
    let bound = vec!["/home/u/proj".to_string()];
    let plan = plan_workspace(
        Some(&bound),
        Some(PathBuf::from("/home/u/somewhere-else")),
        false,
    );
    assert_eq!(
        plan,
        WorkspacePlan {
            bind: Vec::new(),
            turn: vec![PathBuf::from("/home/u/proj")],
        }
    );
    // Every root travels, not just the anchor — `/workspace add` would be
    // pointless otherwise.
    let widened = vec!["/home/u/proj".to_string(), "/home/u/lib".to_string()];
    assert_eq!(
        plan_workspace(Some(&widened), None, false).turn,
        vec![PathBuf::from("/home/u/proj"), PathBuf::from("/home/u/lib")]
    );
}

#[test]
fn a_first_turn_binds_the_directory_it_was_started_in() {
    let plan = plan_workspace(None, Some(PathBuf::from("/home/u/proj")), false);
    assert_eq!(
        plan,
        WorkspacePlan {
            bind: vec![PathBuf::from("/home/u/proj")],
            turn: vec![PathBuf::from("/home/u/proj")],
        }
    );
}

/// Home is the operator's one ongoing thread, entered from whatever directory
/// they are standing in — binding it to the first one would silently redirect
/// every later turn's file tools (docs/bot-runtime.md §2 D6).
#[test]
fn home_is_never_bound_and_takes_the_header_each_turn() {
    let plan = plan_workspace(None, Some(PathBuf::from("/home/u/proj")), true);
    assert_eq!(
        plan,
        WorkspacePlan {
            bind: Vec::new(),
            turn: vec![PathBuf::from("/home/u/proj")],
        }
    );
    // …and once its row exists, still per turn.
    let unbound: Vec<String> = Vec::new();
    assert_eq!(
        plan_workspace(Some(&unbound), Some(PathBuf::from("/home/u/other")), false).turn,
        vec![PathBuf::from("/home/u/other")]
    );
}

/// A caller with no entitlement to a root (a remote one, or a header that
/// resolves to nothing) creates an unbound session and runs in the process
/// workspace. An old row stays unbound too: it must not be captured by whichever
/// directory happens to send its next message.
#[test]
fn nothing_to_bind_leaves_the_session_unbound() {
    assert_eq!(
        plan_workspace(None, None, false),
        WorkspacePlan {
            bind: Vec::new(),
            turn: Vec::new(),
        }
    );
    let unbound: Vec<String> = Vec::new();
    assert!(
        plan_workspace(Some(&unbound), Some(PathBuf::from("/tmp")), false)
            .bind
            .is_empty()
    );
}

/// A cross-provider menu: the default (codex, three effort levels), another
/// codex model, and a deepseek one — whose scale is its own (`none` instead of
/// `medium`). That asymmetry is what the effort rules below turn on.
fn menu() -> Vec<ModelEntry> {
    vec![
        ModelEntry {
            id: "gpt-5.5".into(),
            provider: komo_config::Provider::Codex,
            model: "gpt-5.5".into(),
            efforts: &["low", "medium", "high"],
        },
        ModelEntry {
            id: "gpt-5.4-mini".into(),
            provider: komo_config::Provider::Codex,
            model: "gpt-5.4-mini".into(),
            efforts: &["low", "medium", "high"],
        },
        ModelEntry {
            id: "deepseek:deepseek-chat".into(),
            provider: komo_config::Provider::DeepSeek,
            model: "deepseek-chat".into(),
            efforts: komo_config::Provider::DeepSeek.efforts(),
        },
    ]
}

const DEFAULT_MODEL: &str = "gpt-5.5";

fn model_headers(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    headers
}

#[test]
fn no_model_headers_leaves_the_stored_selection_alone() {
    // An OpenAI-compatible client that knows nothing about these headers must
    // not silently reset a conversation's model.
    assert!(requested_model(&menu(), DEFAULT_MODEL, &model_headers(&[])).is_none());
}

#[test]
fn advertised_model_and_effort_are_accepted() {
    let selection = requested_model(
        &menu(),
        DEFAULT_MODEL,
        &model_headers(&[("x-komo-model", "gpt-5.4-mini"), ("x-komo-effort", "high")]),
    )
    .expect("headers present");
    assert_eq!(selection.model, "gpt-5.4-mini");
    assert_eq!(selection.effort, "high");
}

#[test]
fn a_qualified_cross_provider_id_is_accepted_verbatim() {
    // The stored value keeps its `provider:` prefix — that is what routes the
    // turn to the other backend (`infra::llm::RoutingLlm`).
    let selection = requested_model(
        &menu(),
        DEFAULT_MODEL,
        &model_headers(&[("x-komo-model", "deepseek:deepseek-chat")]),
    )
    .expect("headers present");
    assert_eq!(selection.model, "deepseek:deepseek-chat");
}

#[test]
fn effort_is_validated_against_the_model_that_will_run() {
    // `medium` is not on deepseek's scale, so a level valid for the codex
    // default must not survive the switch — storing it would do nothing.
    let selection = requested_model(
        &menu(),
        DEFAULT_MODEL,
        &model_headers(&[
            ("x-komo-model", "deepseek:deepseek-chat"),
            ("x-komo-effort", "medium"),
        ]),
    )
    .expect("headers present");
    assert_eq!(selection.model, "deepseek:deepseek-chat");
    assert_eq!(
        selection.effort, "",
        "an effort level the target provider doesn't support must be dropped"
    );
}

#[test]
fn none_is_a_per_turn_effort_on_deepseek_only() {
    let selection = requested_model(
        &menu(),
        DEFAULT_MODEL,
        &model_headers(&[
            ("x-komo-model", "deepseek:deepseek-chat"),
            ("x-komo-effort", "none"),
        ]),
    )
    .expect("headers present");
    assert_eq!(selection.effort, "none", "thinking off is a level to pick");

    // Codex has no "thinking off" rung, so the same header reads as unset.
    let selection = requested_model(
        &menu(),
        DEFAULT_MODEL,
        &model_headers(&[("x-komo-model", "gpt-5.4-mini"), ("x-komo-effort", "none")]),
    )
    .expect("headers present");
    assert_eq!(selection.effort, "");
}

#[test]
fn effort_alone_is_validated_against_the_gateway_default_model() {
    // No model header: the default runs, so *its* levels decide.
    let selection = requested_model(
        &menu(),
        DEFAULT_MODEL,
        &model_headers(&[("x-komo-effort", "medium")]),
    )
    .expect("headers present");
    assert_eq!(selection.model, "");
    assert_eq!(selection.effort, "medium");
}

#[test]
fn unadvertised_values_resolve_to_the_default_not_the_provider() {
    let selection = requested_model(
        &menu(),
        DEFAULT_MODEL,
        &model_headers(&[
            ("x-komo-model", "definitely-not-a-model"),
            ("x-komo-effort", "extreme"),
        ]),
    )
    .expect("headers present");
    assert_eq!(
        selection.model, "",
        "an unknown id must not reach the provider"
    );
    assert_eq!(selection.effort, "");
}

/// The `/model` route's half of the shared check: the header path drops what
/// did not validate, this one has to name it, so the check reports it.
#[test]
fn an_unknown_model_is_named_back_with_the_menu() {
    let check = check_selection(&menu(), DEFAULT_MODEL, "gpt-9", "");
    assert_eq!(check.unknown_model.as_deref(), Some("gpt-9"));
    assert_eq!(
        check.selection.model, "",
        "the header path still gets the default"
    );
    assert_eq!(check.effective_model, DEFAULT_MODEL);
}

#[test]
fn an_unknown_effort_is_named_back_with_the_levels_that_model_takes() {
    let check = check_selection(&menu(), DEFAULT_MODEL, "deepseek:deepseek-chat", "medium");
    assert_eq!(check.unknown_effort.as_deref(), Some("medium"));
    assert_eq!(check.effective_model, "deepseek:deepseek-chat");
    assert_eq!(check.efforts, komo_config::Provider::DeepSeek.efforts());
}

#[test]
fn a_model_switch_drops_an_effort_the_new_scale_lacks() {
    // What the route does with a *stored* level on a model change: `max` is
    // deepseek's, so moving to the codex default clears it rather than failing.
    let check = check_selection(&menu(), DEFAULT_MODEL, "gpt-5.4-mini", "max");
    assert_eq!(check.selection.model, "gpt-5.4-mini");
    assert_eq!(check.selection.effort, "");
    assert_eq!(check.unknown_effort.as_deref(), Some("max"));
}

#[test]
fn an_empty_value_is_a_selection_not_a_mistake() {
    // `/model` with `""` puts the session back on the gateway default; the
    // route must not refuse that, so nothing empty is ever reported unknown.
    let check = check_selection(&menu(), DEFAULT_MODEL, "", "");
    assert_eq!(check.selection.model, "");
    assert_eq!(check.selection.effort, "");
    assert!(check.unknown_model.is_none());
    assert!(check.unknown_effort.is_none());
}

#[test]
fn one_header_alone_clears_the_other() {
    // Sending only a model is a full selection: effort resets to the default.
    let selection = requested_model(
        &menu(),
        DEFAULT_MODEL,
        &model_headers(&[("x-komo-model", "gpt-5.5")]),
    )
    .expect("headers present");
    assert_eq!(selection.model, "gpt-5.5");
    assert_eq!(selection.effort, "");
}

#[test]
fn parse_decision_maps_known_strings_and_rejects_others() {
    assert_eq!(parse_decision("once", None), Some(Answer::Once));
    assert_eq!(parse_decision("session", None), Some(Answer::Session));
    assert_eq!(parse_decision("deny", None), Some(Answer::Deny(None)));
    assert_eq!(parse_decision("approve", None), None);
    assert_eq!(parse_decision("", None), None);
}

#[test]
fn deny_feedback_rides_along_but_blank_is_dropped() {
    assert_eq!(
        parse_decision("deny", Some("用 trash".into())),
        Some(Answer::Deny(Some("用 trash".into())))
    );
    assert_eq!(
        parse_decision("deny", Some("   ".into())),
        Some(Answer::Deny(None))
    );
    // An allow ignores feedback entirely.
    assert_eq!(
        parse_decision("once", Some("ignored".into())),
        Some(Answer::Once)
    );
}

// The interactions state round-trip (register → pending_info/pending_question
// visible → resolve delivers the decision/answer) is covered at the state
// layer in `agent::interaction` and `services::clarify`; the handlers here
// are thin wrappers over those, and `require_loopback` (shared with every
// operator write, `/api/operator` included) gates them by construction.
// The operator request/reply wire shapes are covered where they are defined
// (`operator_control::request`) — this dispatcher only names the arms.
