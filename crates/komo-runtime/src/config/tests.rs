//! 加载路径的端到端测试：真文件、真解析、真校验。

use super::testing::{Fixture, SECRET_VALUE, write};
use super::*;

#[test]
fn a_valid_directory_loads_into_a_snapshot() {
    let fixture = Fixture::valid();
    let loaded = load_config(&fixture.options()).unwrap();

    assert_eq!(loaded.snapshot.model.model, "chat-a");
    assert_eq!(loaded.snapshot.memory.model.model, "memory-a");
    assert_eq!(
        loaded
            .snapshot
            .memory
            .embedding
            .as_ref()
            .unwrap()
            .dimensions,
        Some(1024)
    );
    assert_eq!(
        loaded.snapshot.channels.feishu.allow_from,
        vec![komo_kernel::types::chat::PeerId::new("ou_operator")]
    );
    assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
    let chat = loaded.snapshot.model_catalog.get("chat").unwrap();
    assert_eq!(chat.model_provider(), Some("openrouter"));
    assert_eq!(
        chat.completion().unwrap().base_url,
        "https://llm.example.com/v1"
    );
}

#[test]
fn a_model_overrides_its_providers_connection_defaults_as_one_unit() {
    let fixture = Fixture::valid();
    let text = Fixture::config_text("chat-a", "medium").replacen(
        "model_provider = \"openrouter\"\neffort = \"medium\"",
        "model_provider = \"openrouter\"\n\
         base_url = \"https://special.example.com/v1/\"\n\
         api_backend = \"chat_completions\"\n\
         env_key = \"SPECIAL_API_KEY\"\n\
         effort = \"medium\"",
        1,
    );
    write(&fixture.sources().config, &text);
    write(
        &fixture.sources().env,
        &format!("{}SPECIAL_API_KEY=special\n", Fixture::env_text()),
    );

    let loaded = load_config(&fixture.options()).unwrap();
    let chat = loaded.snapshot.model_catalog.completion("chat").unwrap();
    assert_eq!(chat.base_url, "https://special.example.com/v1");
    assert_eq!(chat.provider, "chat_completions");
    assert_eq!(chat.api_key_env, "SPECIAL_API_KEY");
    assert_eq!(loaded.snapshot.model, *chat);
}

#[test]
fn role_aliases_must_point_at_the_right_model_type() {
    let fixture = Fixture::valid();
    let text = Fixture::config_text("chat-a", "medium")
        .replace("default = \"chat\"", "default = \"embedding\"");
    write(&fixture.sources().config, &text);
    let error = load_config(&fixture.options()).unwrap_err();
    let ConfigError::Missing { key, .. } = error else {
        panic!("{error:?}")
    };
    assert_eq!(key.as_str(), "models.default");

    let text = Fixture::config_text("chat-a", "medium").replace(
        "model = \"memory\"\nembedding",
        "model = \"embedding\"\nembedding",
    );
    write(&fixture.sources().config, &text);
    let error = load_config(&fixture.options()).unwrap_err();
    let ConfigError::Missing { key, .. } = error else {
        panic!("{error:?}")
    };
    assert_eq!(key.as_str(), "memory.model");
}

#[test]
fn paths_default_under_the_data_directory_and_resolve_relative_to_the_config_file() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("\n[paths]\nlogs_dir = \"logs-elsewhere\"\n");
    write(&fixture.sources().config, &text);

    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(
        loaded.snapshot.paths.logs_dir,
        fixture.path().join("logs-elsewhere"),
        "相对路径按配置文件所在目录解析（§12）"
    );
    assert_eq!(
        loaded.snapshot.start_only.db_path,
        fixture.path().join("state.db")
    );
    assert_eq!(
        loaded.snapshot.paths.sessions_dir,
        fixture.path().join("sessions")
    );
}

#[test]
fn an_omitted_memory_model_inherits_the_whole_main_model_including_effort() {
    let fixture = Fixture::valid();
    let text = Fixture::config_text("chat-a", "medium");
    // 去掉 memory.model alias，便继承 [models].default。
    let trimmed = text.replace("model = \"memory\"\n", "");
    write(&fixture.sources().config, &trimmed);

    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(loaded.snapshot.memory.model, loaded.snapshot.model);
}

#[test]
fn policy_toml_becomes_the_rule_table_and_its_absence_is_the_initial_one() {
    let fixture = Fixture::valid();
    let loaded = load_config(&fixture.options()).unwrap();
    let ids: Vec<&str> = loaded
        .snapshot
        .policy
        .rules
        .iter()
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(ids, vec!["deny-policy-change", "allow-read-in-roots"]);

    let bare = Fixture::without_policy();
    let loaded = load_config(&bare.options()).unwrap();
    assert_eq!(
        loaded.snapshot.policy,
        komo_kernel::policy::RuleTable::initial()
    );
}

/// `mode = "auto"` 选的是 auto 基表，文件里的规则**追加在它之后**（§7.1）。
#[test]
fn a_policy_mode_picks_a_base_table_and_appends_the_files_own_rules() {
    let fixture = Fixture::valid();
    write(
        &fixture.sources().policy,
        r#"
mode = "auto"

[[rules]]
id = "my-own-allow"
effect = "allow"
reason = "我自己放的"
scopes = ["once"]
requires_isolation = false

[rules.matcher]
operations = ["read_file"]
"#,
    );
    let loaded = load_config(&fixture.options()).unwrap();
    let table = &loaded.snapshot.policy;

    // 基表在：auto 的那几条（含"只问危险形状"），而且默认结论跟着基表走。
    let ids: Vec<&str> = table.rules.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"dangerous-shapes"), "{ids:?}");
    assert!(ids.contains(&"policy-change"), "{ids:?}");
    assert_eq!(ids.last(), Some(&"my-own-allow"), "追加在基表之后：{ids:?}");
    assert_eq!(table.default, komo_kernel::policy::Effect::Allow);
    assert_eq!(table.rules.len(), RuleTable::auto().rules.len() + 1);
}

/// `mode = "strict"` 是 §7.1 的初始建议，一字不差。
#[test]
fn a_strict_mode_is_exactly_the_initial_suggestion() {
    let fixture = Fixture::valid();
    write(&fixture.sources().policy, "mode = \"strict\"\n");
    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(loaded.snapshot.policy, RuleTable::initial());
}

/// `mode` 与 `default` 同时写是**两个互相打架的答案**，宁可起不来也不猜。
#[test]
fn a_policy_mode_with_its_own_default_is_refused() {
    let fixture = Fixture::valid();
    write(
        &fixture.sources().policy,
        "mode = \"auto\"\ndefault = \"ask\"\n",
    );
    let error = load_config(&fixture.options()).unwrap_err().to_string();
    assert!(
        error.contains("mode") && error.contains("default"),
        "{error}"
    );
}

/// 认不出的 `mode` 值（打错字）不是"没有 mode"。
#[test]
fn an_unknown_policy_mode_is_refused_instead_of_falling_back() {
    let fixture = Fixture::valid();
    write(&fixture.sources().policy, "mode = \"aut\"\n");
    let error = load_config(&fixture.options()).unwrap_err().to_string();
    assert!(error.contains("aut"), "{error}");
}

#[test]
fn a_bad_effort_refuses_the_whole_load_and_points_at_the_key() {
    let fixture = Fixture::valid();
    write(
        &fixture.sources().config,
        &Fixture::config_text("chat-a", "ultra"),
    );
    let error = load_config(&fixture.options()).unwrap_err();
    let issues = error.issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].key.as_str(), "model.chat.effort");
    assert!(issues[0].message.contains("ultra"), "{:?}", issues[0]);
}

#[test]
fn a_missing_credential_variable_refuses_the_load() {
    let fixture = Fixture::valid();
    write(
        &fixture.sources().env,
        "FEISHU_APP_ID=cli_x\nFEISHU_APP_SECRET=y\n",
    );
    let error = load_config(&fixture.options()).unwrap_err();
    let keys: Vec<&str> = error.issues().iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "model.chat.api_key_env",
            "model.embedding.api_key_env",
            "model.memory.api_key_env"
        ]
    );
}

#[test]
fn a_malformed_channel_id_refuses_the_load() {
    let fixture = Fixture::valid();
    let text = Fixture::config_text("chat-a", "medium").replace("ou_operator", "operator");
    write(&fixture.sources().config, &text);
    let error = load_config(&fixture.options()).unwrap_err();
    let keys: Vec<&str> = error.issues().iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, vec!["channels.feishu.allow_from"]);
}

#[test]
fn an_unknown_key_is_a_parse_error_naming_the_file() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("\n[model_typo]\nprovider = \"x\"\n");
    write(&fixture.sources().config, &text);

    let error = load_config(&fixture.options()).unwrap_err();
    let ConfigError::Parse { file, message } = &error else {
        panic!("{error:?}")
    };
    assert_eq!(file, &fixture.sources().config);
    assert!(message.contains("model_typo"), "{message}");
}

#[test]
fn a_config_without_a_model_section_says_which_key_is_missing() {
    let fixture = Fixture::valid();
    write(&fixture.sources().config, "[memory]\nenabled = false\n");
    let error = load_config(&fixture.options()).unwrap_err();
    let ConfigError::Missing { key, .. } = &error else {
        panic!("{error:?}")
    };
    assert_eq!(key.as_str(), "models");
}

/// §3：重载日志「只出键名，凭证更不带」。这条断言把它钉在**所有**对外结构上。
#[test]
fn an_env_value_never_reaches_a_snapshot_a_diff_or_a_report() {
    let fixture = Fixture::valid();
    let holder = fixture.holder();
    let before = holder.current();

    // 换一个凭证值 + 换一个模型名，然后重载。
    write(
        &fixture.sources().env,
        &Fixture::env_text().replace(SECRET_VALUE, "sk-rotated-9999"),
    );
    write(
        &fixture.sources().config,
        &Fixture::config_text("chat-b", "medium"),
    );
    let report = holder.reload().unwrap();
    let after = holder.current();

    // 凭证换了是看得见的——以**键名**的形式。
    assert!(
        report
            .changed
            .iter()
            .any(|k| k.as_str() == "credentials.KOMO_LLM_API_KEY"),
        "{report:?}"
    );

    let surfaces = [
        format!("{before:?}"),
        format!("{after:?}"),
        format!("{report:?}"),
        report.summary(),
        format!("{:?}", before.diff(&after)),
        format!("{:?}", holder.secrets()),
        format!("{holder:?}"),
        serde_json::to_string(&*after).unwrap(),
    ];
    for surface in surfaces {
        assert!(!surface.contains(SECRET_VALUE), "泄漏了旧凭证：{surface}");
        assert!(
            !surface.contains("sk-rotated-9999"),
            "泄漏了新凭证：{surface}"
        );
    }
    // 但值本身当然还拿得到——它只住在 Secrets 里。
    assert_eq!(
        holder.secrets().get("KOMO_LLM_API_KEY"),
        Some("sk-rotated-9999")
    );
}

#[test]
fn the_snapshot_records_the_source_files_it_was_built_from() {
    let fixture = Fixture::valid();
    let loaded = load_config(&fixture.options()).unwrap();
    let paths: Vec<_> = loaded
        .snapshot
        .sources
        .iter()
        .map(|s| s.path.clone())
        .collect();
    assert!(paths.contains(&fixture.sources().config));
    assert!(paths.contains(&fixture.sources().env));
    assert!(paths.contains(&fixture.sources().policy));
}

#[test]
fn a_warning_rides_along_with_a_config_that_still_loads() {
    let fixture = Fixture::valid();
    let text = Fixture::config_text("chat-a", "medium")
        .replace("allow_from = [\"ou_operator\"]", "allow_from = []");
    write(&fixture.sources().config, &text);

    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(loaded.issues.len(), 1, "{:?}", loaded.issues);
    assert_eq!(
        loaded.issues[0].severity,
        komo_kernel::protocol::config::IssueSeverity::Warning
    );
}
