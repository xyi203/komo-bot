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
fn a_completion_alias_resolves_with_its_context_window() {
    let fixture = Fixture::valid();
    let text = Fixture::config_text("chat-a", "medium").replacen(
        "model_provider = \"openrouter\"\neffort = \"medium\"",
        "model_provider = \"openrouter\"\neffort = \"medium\"\ncontext_window = 200000",
        1,
    );
    write(&fixture.sources().config, &text);

    let loaded = load_config(&fixture.options()).unwrap();
    let chat = loaded.snapshot.model_catalog.completion("chat").unwrap();
    assert_eq!(chat.context_window, Some(200_000));
    assert_eq!(
        loaded.snapshot.model.context_window,
        Some(200_000),
        "主模型就是解析出来的那份配置，窗口跟着走"
    );
    assert_eq!(loaded.snapshot.memory.model.context_window, None);
}

/// `[model_providers.codex]` 的例子（docs/komo_bot.md §13.3）：`auth = "chatgpt"`，
/// 没有 `env_key`——那条路的凭证是 ChatGPT 账号 OAuth，不是 `.env` 变量。
fn chatgpt_config_text() -> &'static str {
    r#"
default_agent = "assistant"

[agents.assistant]

[model_providers.codex]
base_url = "https://chatgpt.com/backend-api/codex"
api_backend = "responses"
auth = "chatgpt"

[model.chat]
type = "completion"
model = "gpt-5-codex"
model_provider = "codex"

[models]
default = "chat"

[memory]
enabled = false
"#
}

/// `auth = "chatgpt"` 需要凭证文件——没有就在装载阶段整体拒绝并指名该跑的命令。
#[test]
fn auth_chatgpt_without_its_credential_file_refuses_the_whole_load() {
    let fixture = Fixture::valid();
    write(&fixture.sources().config, chatgpt_config_text());

    let error = load_config(&fixture.options()).unwrap_err();
    let ConfigError::Invalid { issues } = &error else {
        panic!("{error:?}")
    };
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].key.as_str(), "model.chat.auth");
    assert!(
        issues[0].message.contains("komo auth codex login"),
        "{:?}",
        issues[0]
    );
}

/// 凭证文件在，就不需要 `.env` 里的任何变量；`api_key_env` 解成空串。
#[test]
fn auth_chatgpt_with_its_credential_file_present_needs_no_env_variable() {
    let fixture = Fixture::valid();
    write(&fixture.sources().config, chatgpt_config_text());
    write(
        &fixture.path().join("codex").join("auth.json"),
        r#"{"access_token":"a","refresh_token":"r","account_id":"acct","expires_at":"2099-01-01T00:00:00Z"}"#,
    );

    let loaded = load_config(&fixture.options()).unwrap();
    assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
    let chat = loaded.snapshot.model_catalog.completion("chat").unwrap();
    assert_eq!(chat.auth.as_deref(), Some("chatgpt"));
    assert_eq!(chat.api_key_env, "");
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

/// §9.10：`[memory]` 的提取节奏有默认值，也读得出 `extract_idle` / `max_observations`；
/// 写成 0 或看不懂的单位，要么校验报键名，要么加载失败。
#[test]
fn the_memory_extraction_pacing_has_defaults_and_is_read_from_config() {
    let fixture = Fixture::valid();
    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(loaded.snapshot.memory.extract_idle_secs, 600);
    assert_eq!(loaded.snapshot.memory.max_observations, 8);

    let text = Fixture::config_text("chat-a", "medium");
    write(
        &fixture.sources().config,
        &text.replace(
            "model = \"memory\"\n",
            "model = \"memory\"\nextract_idle = \"30m\"\nmax_observations = 3\n",
        ),
    );
    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(loaded.snapshot.memory.extract_idle_secs, 1800);
    assert_eq!(loaded.snapshot.memory.max_observations, 3);

    write(
        &fixture.sources().config,
        &text.replace(
            "model = \"memory\"\n",
            "model = \"memory\"\nextract_idle = \"soon\"\n",
        ),
    );
    assert!(matches!(
        load_config(&fixture.options()),
        Err(ConfigError::Parse { .. })
    ));
}

/// `policy.toml` 已经退役（§7）：还在就报一句警告，配置照常装上，规则一条都不读——
/// 连写坏了的旧文件也不让加载失败。
#[test]
fn a_leftover_policy_toml_is_reported_not_read() {
    let fixture = Fixture::valid();
    let quiet = load_config(&fixture.options()).unwrap();
    assert!(
        quiet
            .issues
            .iter()
            .all(|issue| issue.key.as_str() != "policy.toml"),
        "{:?}",
        quiet.issues
    );

    write(
        &fixture.path().join("policy.toml"),
        "mode = \"aut\"\n[[rules\n",
    );
    let loaded = load_config(&fixture.options()).unwrap();
    let issue = loaded
        .issues
        .iter()
        .find(|issue| issue.key.as_str() == "policy.toml")
        .expect("报出来了");
    assert_eq!(
        issue.severity,
        komo_kernel::protocol::config::IssueSeverity::Warning
    );
    assert!(issue.message.contains("已不再使用"), "{}", issue.message);
    assert!(issue.message.contains("可以删掉"), "{}", issue.message);

    // 热重载的报告带着同一句，而且这个文件的改动不算配置变化。
    let holder = fixture.holder();
    assert!(!holder.changed_since());
    write(&fixture.path().join("policy.toml"), "default = \"allow\"\n");
    assert!(!holder.changed_since(), "热重载不再看 policy.toml");
    let report = holder.reload().unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|issue| issue.key.as_str() == "policy.toml"),
        "{:?}",
        report.warnings
    );
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

/// 不认识的键**不让加载失败**，但每一个都作为警告列出来（§3）：升级前后多一个、少一个键
/// 是常态，为此整份配置装不上代价太大；可拼错的键也不能悄悄消失。
#[test]
fn an_unknown_key_is_a_warning_naming_the_key() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("\n[model_typo]\nprovider = \"x\"\n");
    write(&fixture.sources().config, &text);

    let loaded = load_config(&fixture.options()).expect("未知键不挡加载");
    let unknown: Vec<&ConfigIssue> = loaded
        .issues
        .iter()
        .filter(|issue| issue.key.as_str() == "model_typo")
        .collect();
    assert_eq!(unknown.len(), 1, "{:?}", loaded.issues);
    assert_eq!(unknown[0].severity, IssueSeverity::Warning);
}

/// 表里面的未知键报**完整路径**：`default_agent` 写在 `[agents.x]` 之后会被 TOML 收进那张
/// 表，警告要能指出它落在了哪里。
#[test]
fn an_unknown_key_inside_a_table_is_reported_with_its_full_path() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("\n[gateway]\nlisten_typo = \"127.0.0.1:1\"\n");
    write(&fixture.sources().config, &text);

    let loaded = load_config(&fixture.options()).expect("未知键不挡加载");
    assert!(
        loaded
            .issues
            .iter()
            .any(|issue| issue.key.as_str() == "gateway.listen_typo"
                && issue.severity == IssueSeverity::Warning),
        "{:?}",
        loaded.issues
    );
}

/// 退役的 `groups` 名单：旧配置里留着它照常加载，只是一条警告（群里 @ 就响应，没有名单了）。
#[test]
fn a_retired_groups_list_still_loads_with_a_warning() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("groups = [\"oc_x\"]\n");
    write(&fixture.sources().config, &text);

    let loaded = load_config(&fixture.options()).expect("退役的键不挡加载");
    assert!(
        loaded
            .issues
            .iter()
            .any(|issue| issue.key.as_str() == "channels.feishu.groups"
                && issue.severity == IssueSeverity::Warning),
        "{:?}",
        loaded.issues
    );
}

/// 键认识、值的类型不对仍然是解析错误：那种情况没有"忽略"这个安全的解释。
#[test]
fn a_known_key_with_the_wrong_type_still_refuses_the_load() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("\n[execution]\nmodel_result_bytes = \"很多\"\n");
    write(&fixture.sources().config, &text);

    let error = load_config(&fixture.options()).unwrap_err();
    assert!(matches!(error, ConfigError::Parse { .. }), "{error:?}");
}

#[test]
fn the_call_timeout_is_read_from_the_execution_section() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("\n[execution]\ncall_timeout_secs = 600\n");
    write(&fixture.sources().config, &text);

    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(loaded.snapshot.execution.call_timeout_secs, 600);
    assert!(
        loaded
            .issues
            .iter()
            .all(|issue| !issue.key.as_str().starts_with("execution")),
        "{:?}",
        loaded.issues
    );
}

#[test]
fn the_decay_keys_default_when_the_execution_section_omits_them() {
    let fixture = Fixture::valid();
    let loaded = load_config(&fixture.options()).unwrap();
    let execution = &loaded.snapshot.execution;
    assert_eq!(
        (
            execution.decay_threshold_bytes,
            execution.decay_full_sends,
            execution.decay_head_bytes,
            execution.decay_tail_bytes,
        ),
        (4096, 2, 2048, 1536)
    );
    let decay = execution.projection().decay.expect("默认开着衰减");
    assert_eq!(decay.threshold_bytes, 4096);
}

#[test]
fn a_zero_decay_threshold_turns_decay_off() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("\n[execution]\ndecay_threshold_bytes = 0\ndecay_head_bytes = 9000\n");
    write(&fixture.sources().config, &text);

    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(loaded.snapshot.execution.projection().decay, None);
}

#[test]
fn decay_excerpts_that_do_not_fit_under_the_threshold_refuse_the_load() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("\n[execution]\ndecay_head_bytes = 3000\ndecay_tail_bytes = 1096\n");
    write(&fixture.sources().config, &text);

    let error = load_config(&fixture.options()).unwrap_err();
    let keys: Vec<&str> = error.issues().iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, vec!["execution.decay_head_bytes"]);
}

#[test]
fn zero_full_sends_refuses_the_load() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("\n[execution]\ndecay_full_sends = 0\n");
    write(&fixture.sources().config, &text);

    let error = load_config(&fixture.options()).unwrap_err();
    let keys: Vec<&str> = error.issues().iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, vec!["execution.decay_full_sends"]);
}

#[test]
fn compaction_is_off_by_default_and_its_keys_map_onto_the_economics() {
    let fixture = Fixture::valid();
    let loaded = load_config(&fixture.options()).unwrap();
    let compaction = &loaded.snapshot.compaction;
    assert!(!compaction.enabled, "默认关");
    assert_eq!(compaction.cache_write_read_ratio, 12.5);
    assert_eq!(compaction.keep_recent_tokens, 20_000);
    assert_eq!(compaction.memo_token_estimate, 1_000);
    assert_eq!(compaction.max_context_tokens, 150_000);
    assert_eq!(
        compaction.economics(),
        komo_kernel::compaction::CompactionEconomics::default()
    );

    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str(
        "\n[compaction]\nenabled = true\nwindow_reserve_tokens = 8000\n\
         first_compaction_scale = 3.0\nsubsequent_margin = 2.0\ncooldown_requests = 4\n\
         max_context_tokens = 200000\n",
    );
    write(&fixture.sources().config, &text);
    let loaded = load_config(&fixture.options()).unwrap();
    let economics = loaded.snapshot.compaction.economics();
    assert!(loaded.snapshot.compaction.enabled);
    assert_eq!(economics.window_reserve_tokens, 8_000);
    assert_eq!(economics.first_compaction_request_scale, 3.0);
    assert_eq!(economics.subsequent_compaction_margin, 2.0);
    assert_eq!(economics.minimum_requests_since_compaction, 4);
    assert_eq!(loaded.snapshot.compaction.max_context_tokens, 200_000);
    assert_eq!(loaded.snapshot.compaction.window(Some(1_000_000)), 208_000);
}

#[test]
fn nonsensical_compaction_numbers_refuse_the_load() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str(
        "\n[compaction]\ncache_write_read_ratio = -1.0\nsubsequent_margin = 0.0\n\
         keep_recent_tokens = 0\nmax_context_tokens = 0\n",
    );
    write(&fixture.sources().config, &text);

    let error = load_config(&fixture.options()).unwrap_err();
    let keys: Vec<&str> = error.issues().iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "compaction.cache_write_read_ratio",
            "compaction.keep_recent_tokens",
            "compaction.max_context_tokens",
            "compaction.subsequent_margin",
        ]
    );
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

// ---------------------------------------------------------------- [mcp]（docs/mcp.md）

#[test]
fn mcp_servers_load_into_the_start_only_keys() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str(
        r#"
[mcp.servers.gh]
command = "npx"
args = ["-y", "server-github"]
env = ["GITHUB_TOKEN"]
read_only = ["search"]

[mcp.servers.docs]
url = "https://mcp.example.com/mcp"
bearer_token_env = "DOCS_TOKEN"
tools = ["lookup"]
"#,
    );
    write(&fixture.sources().config, &text);

    let loaded = load_config(&fixture.options()).unwrap();
    let mcp = &loaded.snapshot.mcp;
    assert_eq!(
        mcp["gh"].transport,
        komo_kernel::protocol::config::McpTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "server-github".into()],
            env: vec!["GITHUB_TOKEN".into()],
        }
    );
    assert_eq!(mcp["gh"].read_only, vec!["search".to_string()]);
    assert_eq!(mcp["docs"].tools, Some(vec!["lookup".to_string()]));
}

#[test]
fn an_oauth_mcp_server_loads_and_refuses_a_bearer_token_beside_it() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("\n[mcp.servers.devops]\nurl = \"https://mcp.example.com/mcp\"\noauth = true\n");
    write(&fixture.sources().config, &text);
    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(
        loaded.snapshot.mcp["devops"].transport,
        komo_kernel::protocol::config::McpTransport::Http {
            url: "https://mcp.example.com/mcp".into(),
            bearer_token_env: None,
            oauth: true,
        }
    );

    text.push_str("bearer_token_env = \"T\"\n");
    write(&fixture.sources().config, &text);
    let error = load_config(&fixture.options()).unwrap_err();
    assert!(error.to_string().contains("只能写一个"), "{error}");
}

#[test]
fn mcp_exposure_defaults_to_codemode_and_loads_overrides() {
    use komo_kernel::protocol::config::McpExposure;
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str(
        "\n[mcp.servers.devops]\nurl = \"https://mcp.example.com/mcp\"\n\
         description = \"  发布与配置  \"\n\
         tool_exposure = { issue_get = \"direct\", \"delete_*\" = \"hidden\" }\n",
    );
    write(&fixture.sources().config, &text);
    let loaded = load_config(&fixture.options()).unwrap();
    let server = &loaded.snapshot.mcp["devops"];
    assert_eq!(server.exposure, McpExposure::Codemode);
    assert_eq!(server.description.as_deref(), Some("发布与配置"));
    assert_eq!(server.exposure_of("issue_get"), McpExposure::Direct);
    assert_eq!(server.exposure_of("delete_app"), McpExposure::Hidden);
    assert_eq!(server.exposure_of("issue_list"), McpExposure::Codemode);

    text.push_str("exposure = \"deferred\"\n");
    write(&fixture.sources().config, &text);
    assert!(load_config(&fixture.options()).is_err());
}

#[test]
fn an_mcp_server_needs_exactly_one_of_command_and_url() {
    let fixture = Fixture::valid();
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str("\n[mcp.servers.x]\ncommand = \"a\"\nurl = \"http://b\"\n");
    write(&fixture.sources().config, &text);
    let error = load_config(&fixture.options()).unwrap_err();
    assert!(error.to_string().contains("[mcp.servers.x]"), "{error}");
}

fn with_telemetry(fixture: &Fixture, body: &str) {
    let mut text = Fixture::config_text("chat-a", "medium");
    text.push_str(body);
    write(&fixture.sources().config, &text);
}

#[test]
fn telemetry_is_off_unless_an_endpoint_is_written() {
    let fixture = Fixture::valid();
    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(loaded.snapshot.start_only.otlp_endpoint, None);

    with_telemetry(&fixture, "\n[telemetry]\n");
    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(loaded.snapshot.start_only.otlp_endpoint, None);
}

#[test]
fn a_telemetry_endpoint_loads_into_the_start_only_keys() {
    let fixture = Fixture::valid();
    with_telemetry(
        &fixture,
        "\n[telemetry]\notlp_endpoint = \"http://127.0.0.1:4318\"\n",
    );
    let loaded = load_config(&fixture.options()).unwrap();
    assert_eq!(
        loaded.snapshot.start_only.otlp_endpoint.as_deref(),
        Some("http://127.0.0.1:4318")
    );
    assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
}

#[test]
fn a_telemetry_endpoint_that_is_not_an_http_url_refuses_the_load() {
    for bad in ["not a url", "grpc://collector:4317", "127.0.0.1:4318"] {
        let fixture = Fixture::valid();
        with_telemetry(
            &fixture,
            &format!("\n[telemetry]\notlp_endpoint = \"{bad}\"\n"),
        );
        let error = load_config(&fixture.options()).unwrap_err();
        let keys: Vec<&str> = error.issues().iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["start_only.otlp_endpoint"], "{bad}");
    }
}
