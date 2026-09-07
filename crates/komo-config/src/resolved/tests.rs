use super::super::ConfigSnapshot;
use super::super::sources::{
    ApiFileConfig, ChannelsFileConfig, FileConfig, McpFileConfig, McpServerFileConfig, Secrets,
    TelegramFileConfig,
};
use super::*;
use std::path::PathBuf;

fn sources() -> ConfigSources {
    ConfigSources {
        home: PathBuf::from("/tmp/komo-test-home"),
        file: FileConfig::default(),
        env: KomoEnv::default(),
        secrets: Secrets::default(),
        env_error: None,
    }
}

fn with_deepseek_key(mut s: ConfigSources) -> ConfigSources {
    s.secrets.deepseek_api_key = Some("sk-test".into());
    s
}

/// The python plugin host is on unless it is switched off, and `KOMO_*`
/// wins over the file like every other setting.
#[test]
fn the_python_host_is_enabled_unless_switched_off() {
    let rt = ConfigSnapshot::from_sources(with_deepseek_key(sources())).runtime;
    assert!(rt.pyhost_enabled, "on by default");

    let mut s = with_deepseek_key(sources());
    s.file.pyhost_enabled = Some(false);
    assert!(!ConfigSnapshot::from_sources(s).runtime.pyhost_enabled);

    let mut s = with_deepseek_key(sources());
    s.file.pyhost_enabled = Some(true);
    s.env.pyhost_enabled = Some(false);
    assert!(
        !ConfigSnapshot::from_sources(s).runtime.pyhost_enabled,
        "the env overrides the file"
    );
}

#[test]
fn defaults_resolve_without_file_or_env() {
    let snap = ConfigSnapshot::from_sources(with_deepseek_key(sources()));
    let rt = &snap.runtime;
    assert_eq!(rt.model.provider, Provider::DeepSeek);
    assert_eq!(rt.model.model, "deepseek-v4-flash");
    assert_eq!(rt.model.max_turns, DEFAULT_MAX_TURNS);
    assert_eq!(rt.maintenance_schedule, DEFAULT_MAINTENANCE_SCHEDULE);
    assert_eq!(
        rt.dream_schedule.as_deref(),
        Some(DEFAULT_DREAM_SCHEDULE),
        "dreaming is on by default"
    );
    assert_eq!(rt.review_interval, DEFAULT_REVIEW_INTERVAL);
    assert_eq!(snap.report.provider_origin, Origin::Default);
    assert_eq!(snap.report.model_origin, Origin::Default);
    assert!(snap.report.fatal().is_none());
    assert!(snap.validate_gateway().is_ok());
}

#[test]
fn readable_roots_are_canonicalized_and_invalid_entries_are_reported() {
    let valid = std::env::temp_dir();
    let missing = valid.join(format!("komo-missing-{}", uuid::Uuid::new_v4()));
    let mut s = with_deepseek_key(sources());
    s.file.readable_roots = Some(vec![valid.clone(), missing]);

    let snap = ConfigSnapshot::from_sources(s);
    assert_eq!(
        snap.runtime.readable_roots,
        vec![valid.canonicalize().unwrap()]
    );
    assert!(snap.report.issues.iter().any(|issue| {
        issue.path == "readable_roots" && issue.severity == IssueSeverity::Warning
    }));
}

#[test]
fn precedence_is_default_then_file_then_env() {
    let mut s = sources();
    s.secrets.openai_api_key = Some("sk-env".into());
    s.file.provider = Some("deepseek".into());
    s.file.model = Some("file-model".into());
    s.file.max_turns = Some(7);
    s.env.provider = Some("openai".into());
    s.env.model = Some("env-model".into());
    let snap = ConfigSnapshot::from_sources(s);
    assert_eq!(snap.runtime.model.provider, Provider::OpenAi);
    assert_eq!(snap.runtime.model.model, "env-model");
    assert_eq!(snap.runtime.model.max_turns, 7, "file wins over default");
    assert_eq!(snap.report.provider_origin, Origin::Env);
    assert_eq!(snap.report.model_origin, Origin::Env);
}

#[test]
fn the_aux_backend_turns_deepseek_thinking_off_by_default() {
    // Every aux caller builds a session with empty overrides, so the
    // backend default is the only effort those turns ever carry.
    let snap = ConfigSnapshot::from_sources(with_deepseek_key(sources()));
    assert_eq!(
        snap.runtime.model.effort, None,
        "a conversation picks its own"
    );
    assert_eq!(
        snap.runtime.model.aux_variant().effort.as_deref(),
        Some("none")
    );
}

#[test]
fn a_configured_aux_effort_wins_over_the_providers_default() {
    let mut s = sources();
    s.secrets.openai_api_key = Some("sk-test".into());
    s.file.provider = Some("openai".into());
    s.env.aux_effort = Some("low".into());
    let snap = ConfigSnapshot::from_sources(s);
    assert_eq!(
        snap.runtime.model.aux_variant().effort.as_deref(),
        Some("low")
    );
}

#[test]
fn an_aux_effort_the_provider_rejects_warns_and_reads_as_unset() {
    // DeepSeek has no effort scale, so `medium` is not a level it accepts.
    let mut s = with_deepseek_key(sources());
    s.file.aux_effort = Some("medium".into());
    let snap = ConfigSnapshot::from_sources(s);
    let issue = snap
        .report
        .issues
        .iter()
        .find(|i| i.path == "model.aux_effort")
        .expect("an unusable aux_effort is reported");
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert!(
        snap.report.fatal().is_none(),
        "a typo never aborts resolution"
    );
    assert_eq!(
        snap.runtime.model.aux_variant().effort.as_deref(),
        Some("none"),
        "it falls back to the provider's own aux default"
    );
}

#[test]
fn a_provider_with_no_aux_default_leaves_effort_alone() {
    let mut s = sources();
    s.secrets.openai_api_key = Some("sk-test".into());
    s.file.provider = Some("openai".into());
    let snap = ConfigSnapshot::from_sources(s);
    assert_eq!(snap.runtime.model.aux_variant().effort, None);
}

#[test]
fn file_model_reports_file_origin() {
    let mut s = with_deepseek_key(sources());
    s.file.model = Some("deepseek-reasoner".into());
    let snap = ConfigSnapshot::from_sources(s);
    assert_eq!(snap.report.model_origin, Origin::File);
    assert_eq!(snap.report.provider_origin, Origin::Default);
}

#[test]
fn missing_api_key_warns_but_does_not_block_startup() {
    let snap = ConfigSnapshot::from_sources(sources());
    assert_eq!(snap.runtime.model.api_key, "");
    // Degraded, not dead: a fresh install must boot (build_llm degrades to
    // an every-call-errors client), so the issue is a warning.
    assert!(snap.report.fatal().is_none());
    let issue = snap
        .report
        .issues
        .iter()
        .find(|i| i.path == "model.api_key")
        .expect("missing key is reported");
    assert_eq!(issue.severity, IssueSeverity::Warning);
    assert!(issue.message.contains("DEEPSEEK_API_KEY"));
    assert!(snap.validate_gateway().is_ok());
    assert!(snap.validate_agent().is_ok());
}

#[test]
fn codex_needs_no_api_key() {
    let mut s = sources();
    s.file.provider = Some("codex".into());
    let snap = ConfigSnapshot::from_sources(s);
    assert_eq!(snap.runtime.model.provider, Provider::Codex);
    assert_eq!(snap.runtime.model.model, "gpt-5.5");
    assert!(
        snap.report.fatal().is_none(),
        "codex auth is OAuth, not an env key"
    );
    assert!(!snap.report.key_present(Provider::Codex));
}

#[test]
fn invalid_provider_is_fatal_and_falls_back() {
    let mut s = sources();
    s.file.provider = Some("nonsense".into());
    let snap = ConfigSnapshot::from_sources(s);
    let fatal = snap.report.fatal().expect("bad provider is fatal");
    assert_eq!(fatal.path, "model.provider");
    assert_eq!(
        snap.runtime.model.provider,
        Provider::DeepSeek,
        "resolution continues on the default provider"
    );
    assert!(snap.validate_agent().is_err());
}

#[test]
fn env_error_is_fatal_for_startup_not_diagnostics() {
    let mut s = with_deepseek_key(sources());
    s.env_error = Some("invalid KOMO_* environment variable: bad".into());
    let snap = ConfigSnapshot::from_sources(s);
    let fatal = snap.report.fatal().expect("env error is fatal");
    assert_eq!(fatal.path, "env");
    assert!(snap.validate_agent().is_err());
    // Diagnostics still get a fully-resolved snapshot.
    assert_eq!(snap.runtime.model.provider, Provider::DeepSeek);
}

#[test]
fn disabled_channel_missing_secret_is_not_an_issue() {
    let mut s = with_deepseek_key(sources());
    s.file.channels = Some(ChannelsFileConfig {
        telegram: Some(TelegramFileConfig {
            enabled: false,
            ..Default::default()
        }),
        ..Default::default()
    });
    let snap = ConfigSnapshot::from_sources(s);
    assert!(matches!(snap.runtime.telegram, ChannelState::Disabled));
    assert!(snap.report.fatal().is_none());
}

#[test]
fn enabled_channel_missing_secret_is_one_fatal_issue() {
    let mut s = with_deepseek_key(sources());
    s.file.channels = Some(ChannelsFileConfig {
        telegram: Some(TelegramFileConfig {
            enabled: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let snap = ConfigSnapshot::from_sources(s);
    let ChannelState::Misconfigured(msg) = &snap.runtime.telegram else {
        panic!("enabled without token must be misconfigured");
    };
    assert!(msg.contains("TELEGRAM_BOT_TOKEN"));
    assert_eq!(
        snap.report
            .issues
            .iter()
            .filter(|i| i.path == "channels.telegram")
            .count(),
        1
    );
    // The gateway fails fast; a chat turn doesn't need the channel.
    assert!(snap.validate_gateway().is_err());
    assert!(snap.validate_agent().is_ok());
}

#[test]
fn api_defaults_to_loopback_ephemeral_with_auto_key() {
    let snap = ConfigSnapshot::from_sources(with_deepseek_key(sources()));
    let api = snap.runtime.api.ready().expect("api is always on");
    assert_eq!(api.bind, "127.0.0.1");
    assert_eq!(api.port, 0, "ephemeral port by default");
    assert!(!api.server_key.is_empty(), "auto-generated key");
}

#[test]
fn external_api_requires_a_key() {
    let mut s = with_deepseek_key(sources());
    s.file.channels = Some(ChannelsFileConfig {
        api: Some(ApiFileConfig {
            enabled: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let snap = ConfigSnapshot::from_sources(s);
    assert!(matches!(snap.runtime.api, ChannelState::Misconfigured(_)));
    assert!(snap.validate_gateway().is_err());

    let mut s = with_deepseek_key(sources());
    s.secrets.api_server_key = Some("k".into());
    s.file.channels = Some(ChannelsFileConfig {
        api: Some(ApiFileConfig {
            enabled: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let snap = ConfigSnapshot::from_sources(s);
    let api = snap
        .runtime
        .api
        .ready()
        .expect("keyed external api is ready");
    assert_eq!(api.port, 8765, "stable default port when external");
    assert_eq!(api.server_key, "k");
}

#[test]
fn report_never_contains_secret_values() {
    let mut s = sources();
    s.secrets.deepseek_api_key = Some("sk-super-secret-value".into());
    s.secrets.telegram_bot_token = Some("123:telegram-secret".into());
    s.file.channels = Some(ChannelsFileConfig {
        telegram: Some(TelegramFileConfig {
            enabled: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let snap = ConfigSnapshot::from_sources(s);
    let dump = format!("{:?}", snap.report);
    assert!(!dump.contains("sk-super-secret-value"));
    assert!(!dump.contains("telegram-secret"));
    assert!(snap.report.key_present(Provider::DeepSeek));
}

#[test]
fn dream_schedule_defaults_on_and_can_be_disabled() {
    // Unset → on by default at the nightly slot.
    assert_eq!(
        resolve_dream_schedule(None).as_deref(),
        Some(DEFAULT_DREAM_SCHEDULE)
    );
    // A custom cron is taken verbatim.
    assert_eq!(
        resolve_dream_schedule(Some("0 4 * * *".into())).as_deref(),
        Some("0 4 * * *")
    );
    // Empty or off-like values disable it.
    for off in ["", "  ", "off", "OFF", "none", "disabled"] {
        assert_eq!(
            resolve_dream_schedule(Some(off.into())),
            None,
            "`{off}` should disable dreaming"
        );
    }
}

#[test]
fn enabled_switches_override_a_configured_schedule() {
    let mut s = with_deepseek_key(sources());
    s.file.dream_schedule = Some("0 3 * * *".into());
    s.env.dream_schedule_enabled = Some(false);

    let rt = ConfigSnapshot::from_sources(s).runtime;
    assert_eq!(rt.dream_schedule, None);
}

#[test]
fn enabled_switches_default_on_and_env_beats_file() {
    let mut s = with_deepseek_key(sources());
    s.file.dream_schedule = Some("0 3 * * *".into());
    s.file.dream_schedule_enabled = Some(false);
    s.env.dream_schedule_enabled = Some(true);

    let rt = ConfigSnapshot::from_sources(s).runtime;
    assert_eq!(rt.dream_schedule.as_deref(), Some("0 3 * * *"));
}

#[test]
fn skills_path_splits_on_colons() {
    let mut s = with_deepseek_key(sources());
    s.env.skills_path = Some("/a/skills:/b/skills:".into());
    let snap = ConfigSnapshot::from_sources(s);
    assert_eq!(
        snap.runtime.skills_path,
        vec![PathBuf::from("/a/skills"), PathBuf::from("/b/skills")]
    );
}

#[test]
fn the_db_url_derives_from_home() {
    let snap = ConfigSnapshot::from_sources(with_deepseek_key(sources()));
    assert_eq!(snap.runtime.db_url, "turso:/tmp/komo-test-home/komo.db");
}

#[test]
fn model_menu_defaults_to_the_model_plus_aux() {
    assert_eq!(
        resolve_model_menu("deepseek-chat", Some("deepseek-chat-lite"), None, None),
        vec!["deepseek-chat", "deepseek-chat-lite"]
    );
    // No aux model configured: the menu is just the one model.
    assert_eq!(
        resolve_model_menu("deepseek-chat", None, None, None),
        vec!["deepseek-chat"]
    );
    // An aux model equal to the main one must not appear twice.
    assert_eq!(
        resolve_model_menu("deepseek-chat", Some("deepseek-chat"), None, None),
        vec!["deepseek-chat"]
    );
}

#[test]
fn model_menu_always_offers_the_running_model_first() {
    // A declared menu that forgot the configured model would otherwise leave
    // the model the gateway is actually running unselectable.
    let file = vec!["b".to_string(), "c".to_string()];
    assert_eq!(
        resolve_model_menu("a", None, None, Some(&file)),
        vec!["a", "b", "c"]
    );
}

#[test]
fn env_model_menu_wins_over_the_file_and_tolerates_sloppy_csv() {
    let file = vec!["ignored".to_string()];
    assert_eq!(
        resolve_model_menu("a", Some("aux"), Some(" b , a ,, c "), Some(&file)),
        vec!["a", "b", "c"],
        "env wins; blanks and the duplicate default are dropped, aux is not appended"
    );
}

/// A codex-default config whose menu also names a deepseek model.
fn cross_provider_config(with_deepseek_key: bool) -> ModelConfig {
    let mut keys = HashMap::new();
    if with_deepseek_key {
        keys.insert(Provider::DeepSeek, "sk-ds".to_string());
    }
    ModelConfig {
        provider: Provider::Codex,
        model: "gpt-5.6-terra".into(),
        models: vec![
            "gpt-5.6-terra".into(),
            "deepseek:deepseek-chat".into(),
            "gpt-5.4-mini".into(),
        ],
        keys,
        api_key: String::new(),
        base_url: Some("https://proxy.example".into()),
        aux_model: None,
        aux_effort: None,
        effort: None,
        max_turns: DEFAULT_MAX_TURNS,
        max_tool_result_bytes: DEFAULT_MAX_TOOL_RESULT_BYTES,
        max_turn_result_bytes: DEFAULT_MAX_TURN_RESULT_BYTES,
        tool_timeout_secs: DEFAULT_TOOL_TIMEOUT_SECS,
        max_history_messages: DEFAULT_MAX_HISTORY_MESSAGES,
        max_history_bytes: DEFAULT_MAX_HISTORY_BYTES,
        llm_timeout_secs: DEFAULT_LLM_TIMEOUT_SECS,
    }
}

#[test]
fn menu_resolves_qualified_ids_to_their_own_provider_and_efforts() {
    let menu = cross_provider_config(true).menu();
    let ids: Vec<_> = menu.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        ["gpt-5.6-terra", "deepseek:deepseek-chat", "gpt-5.4-mini"]
    );

    let deepseek = &menu[1];
    assert_eq!(deepseek.provider, Provider::DeepSeek);
    assert_eq!(deepseek.model, "deepseek-chat", "the prefix is stripped");
    assert_eq!(
        deepseek.efforts,
        ["low", "high", "max"],
        "deepseek has its own scale, not the codex entries'"
    );
    // An unqualified entry inherits the configured provider.
    assert_eq!(menu[2].provider, Provider::Codex);
    assert_eq!(menu[0].efforts, ["low", "medium", "high"]);
}

#[test]
fn menu_drops_models_whose_provider_has_no_credential() {
    // Offering one would mean a model that errors on every single turn.
    let menu = cross_provider_config(false).menu();
    let ids: Vec<_> = menu.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["gpt-5.6-terra", "gpt-5.4-mini"]);
}

#[test]
fn the_running_model_survives_even_without_its_credential() {
    // Hiding it would misreport what the gateway is actually running; the
    // missing key is already a startup warning.
    let mut config = cross_provider_config(false);
    config.provider = Provider::OpenAi;
    config.model = "gpt-4.1".into();
    config.models = vec!["gpt-4.1".into(), "deepseek:deepseek-chat".into()];
    let ids: Vec<_> = config.menu().iter().map(|e| e.id.clone()).collect();
    assert_eq!(ids, ["gpt-4.1"]);
}

#[test]
fn menu_providers_covers_every_backend_plus_the_default() {
    let config = cross_provider_config(true);
    assert_eq!(
        config.menu_providers(),
        vec![Provider::Codex, Provider::DeepSeek]
    );
    // With no credential the deepseek entry is gone, so no client is built.
    assert_eq!(
        cross_provider_config(false).menu_providers(),
        vec![Provider::Codex]
    );
}

#[test]
fn for_provider_carries_base_url_only_to_the_default_provider() {
    let config = cross_provider_config(true);
    // base_url overrides one specific endpoint; applying it to deepseek would
    // silently point it at an unrelated OpenAI-compatible proxy.
    let other = config.for_provider(Provider::DeepSeek, "deepseek-chat".into());
    assert_eq!(other.base_url, None);
    assert_eq!(other.api_key, "sk-ds", "each backend gets its own key");

    let same = config.for_provider(Provider::Codex, "gpt-5.6-terra".into());
    assert_eq!(same.base_url.as_deref(), Some("https://proxy.example"));
}

#[test]
fn debug_output_masks_api_key() {
    let cfg = ModelConfig {
        provider: Provider::DeepSeek,
        model: "deepseek-chat".into(),
        models: vec!["deepseek-chat".into()],
        keys: Default::default(),
        api_key: "sk-abcdefghijklmnopqr".into(),
        base_url: None,
        aux_model: None,
        aux_effort: None,
        effort: None,
        max_turns: DEFAULT_MAX_TURNS,
        max_tool_result_bytes: DEFAULT_MAX_TOOL_RESULT_BYTES,
        max_turn_result_bytes: DEFAULT_MAX_TURN_RESULT_BYTES,
        tool_timeout_secs: DEFAULT_TOOL_TIMEOUT_SECS,
        max_history_messages: DEFAULT_MAX_HISTORY_MESSAGES,
        max_history_bytes: DEFAULT_MAX_HISTORY_BYTES,
        llm_timeout_secs: DEFAULT_LLM_TIMEOUT_SECS,
    };
    let s = format!("{cfg:?}");
    assert!(
        !s.contains("sk-abcdefghijklmnopqr"),
        "full key must not appear in Debug output"
    );
    assert!(s.contains("sk-"), "prefix should be visible");
}

/// One `[mcp.servers.<name>]` table plus the resolved snapshot it produces.
///
/// Deliberately never sets an env var: `resolve_mcp_servers` reads the
/// process environment, which is shared by every test running in parallel.
fn with_mcp(name: &str, server: McpServerFileConfig) -> ConfigSnapshot {
    let mut s = with_deepseek_key(sources());
    s.file.mcp = Some(McpFileConfig {
        servers: [(name.to_string(), server)].into_iter().collect(),
    });
    ConfigSnapshot::from_sources(s)
}

fn mcp_issues(snap: &ConfigSnapshot) -> Vec<&str> {
    snap.report
        .issues
        .iter()
        .filter(|i| i.path == "mcp.servers")
        .map(|i| i.message.as_str())
        .collect()
}

#[test]
fn mcp_server_with_an_explicit_tool_list_resolves() {
    let snap = with_mcp(
        "memos",
        McpServerFileConfig {
            url: "https://memos.example.com/mcp".into(),
            tools: vec!["create_memo".into(), "list_memos".into()],
            ..Default::default()
        },
    );
    assert_eq!(snap.runtime.mcp_servers.len(), 1);
    let server = &snap.runtime.mcp_servers[0];
    assert_eq!(server.name, "memos");
    assert_eq!(server.tools, ["create_memo", "list_memos"]);
    assert!(server.token.is_none());
    assert!(mcp_issues(&snap).is_empty());
}

#[test]
fn mcp_server_without_a_tool_list_mounts_nothing() {
    // Closed by default, like the HA channel's event filters: a server can
    // advertise dozens of tools and each one costs a schema every round.
    let snap = with_mcp(
        "memos",
        McpServerFileConfig {
            url: "https://memos.example.com/mcp".into(),
            ..Default::default()
        },
    );
    assert!(snap.runtime.mcp_servers.is_empty());
    let issues = mcp_issues(&snap);
    assert_eq!(issues.len(), 1);
    assert!(issues[0].contains("all_tools"), "{}", issues[0]);
    // A warning, never fatal — an optional integration must not block boot.
    assert!(snap.validate_gateway().is_ok());
    assert!(snap.validate_agent().is_ok());
}

#[test]
fn mcp_all_tools_opts_out_of_the_allowlist() {
    let snap = with_mcp(
        "memos",
        McpServerFileConfig {
            url: "https://memos.example.com/mcp".into(),
            all_tools: true,
            ..Default::default()
        },
    );
    assert_eq!(snap.runtime.mcp_servers.len(), 1);
    assert!(
        snap.runtime.mcp_servers[0].tools.is_empty(),
        "an empty allowlist is how wiring spells `mount everything`"
    );
    assert!(mcp_issues(&snap).is_empty());
}

#[test]
fn mcp_server_naming_an_unset_token_var_is_skipped_with_a_warning() {
    // Naming token_env states the server needs auth; connecting anyway
    // would turn a clear config warning into a 401 at call time.
    let snap = with_mcp(
        "memos",
        McpServerFileConfig {
            url: "https://memos.example.com/mcp".into(),
            token_env: Some("KOMO_TEST_DEFINITELY_UNSET_TOKEN".into()),
            tools: vec!["create_memo".into()],
            ..Default::default()
        },
    );
    assert!(snap.runtime.mcp_servers.is_empty());
    let issues = mcp_issues(&snap);
    assert_eq!(issues.len(), 1);
    assert!(
        issues[0].contains("KOMO_TEST_DEFINITELY_UNSET_TOKEN"),
        "{}",
        issues[0]
    );
    assert!(snap.validate_gateway().is_ok());
}

#[test]
fn disabled_mcp_server_is_skipped_silently() {
    let snap = with_mcp(
        "memos",
        McpServerFileConfig {
            enabled: Some(false),
            url: "https://memos.example.com/mcp".into(),
            tools: vec!["create_memo".into()],
            ..Default::default()
        },
    );
    assert!(snap.runtime.mcp_servers.is_empty());
    assert!(mcp_issues(&snap).is_empty());
}

#[test]
fn no_mcp_table_means_no_servers_and_no_issues() {
    let snap = ConfigSnapshot::from_sources(with_deepseek_key(sources()));
    assert!(snap.runtime.mcp_servers.is_empty());
    assert!(mcp_issues(&snap).is_empty());
}
