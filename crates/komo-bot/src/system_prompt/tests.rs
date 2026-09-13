use super::*;
use komo_config::{DEFAULT_MAX_TURNS, Provider};

fn config() -> ModelConfig {
    ModelConfig {
        provider: Provider::DeepSeek,
        model: "deepseek-chat".into(),
        models: vec!["deepseek-chat".into()],
        keys: Default::default(),
        api_key: "sk-test".into(),
        base_url: None,
        aux_model: None,
        aux_effort: None,
        memory_model: None,
        memory_effort: None,
        effort: None,
        max_turns: DEFAULT_MAX_TURNS,
        max_tool_result_bytes: komo_config::DEFAULT_MAX_TOOL_RESULT_BYTES,
        max_turn_result_bytes: komo_config::DEFAULT_MAX_TURN_RESULT_BYTES,
        tool_timeout_secs: komo_config::DEFAULT_TOOL_TIMEOUT_SECS,
        max_history_messages: komo_config::DEFAULT_MAX_HISTORY_MESSAGES,
        max_history_bytes: komo_config::DEFAULT_MAX_HISTORY_BYTES,
        llm_timeout_secs: komo_config::DEFAULT_LLM_TIMEOUT_SECS,
    }
}

fn tmp(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("komo_sysprompt_test_{suffix}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn minimal_prompt_has_identity_and_volatile_only() {
    let p = SystemPromptBuilder::new(&config())
        .home(tmp("minimal"))
        .build(&[]);
    assert!(p.contains("You are Komo"));
    assert!(p.contains("Model: deepseek-chat"));
    assert!(p.contains("Provider: deepseek"));
    // No tools → no tool-aware guidance.
    assert!(!p.contains("schedule work on a clock"));
    assert!(!p.contains("tmux ls"));
}

#[test]
fn tool_guidance_is_gated_on_loaded_tools() {
    let p = SystemPromptBuilder::new(&config())
        .home(tmp("gated"))
        .tools(vec!["memory".into(), "python".into()])
        .build(&[]);
    assert!(p.contains("tmux ls")); // state guidance, via `memory`
    assert!(p.contains("read a clock")); // code guidance, via `python`
    // `cron` wasn't loaded, so its scheduler-routing guidance stays out.
    assert!(!p.contains("schedule work on a clock"));
}

#[test]
fn todo_guidance_appears_only_with_the_todo_tool() {
    let with = SystemPromptBuilder::new(&config())
        .home(tmp("todo_on"))
        .tools(vec!["todo".into()])
        .build(&[]);
    assert!(with.contains("Skip it entirely for"));
    let without = SystemPromptBuilder::new(&config())
        .home(tmp("todo_off"))
        .tools(vec!["read".into()])
        .build(&[]);
    assert!(!without.contains("Skip it entirely for"));
}

#[test]
fn tool_economy_guidance_requires_at_least_one_tool() {
    let with = SystemPromptBuilder::new(&config())
        .home(tmp("economy_on"))
        .tools(vec!["time".into()])
        .build(&[]);
    assert!(with.contains("run concurrently"));
    // No tools loaded → no round-economy advice to give.
    let without = SystemPromptBuilder::new(&config())
        .home(tmp("economy_off"))
        .build(&[]);
    assert!(!without.contains("run concurrently"));
}

/// The routing rule is what makes `python` reachable at all: the API
/// listing says how to write a program, never when one beats N rounds.
#[test]
fn code_guidance_appears_only_with_python() {
    let with = SystemPromptBuilder::new(&config())
        .home(tmp("code_on"))
        .tools(vec!["python".into(), "read".into()])
        .build(&[]);
    assert!(with.contains("reach for a program"), "{with}");
    let without = SystemPromptBuilder::new(&config())
        .home(tmp("code_off"))
        .tools(vec!["read".into()])
        .build(&[]);
    assert!(!without.contains("reach for a program"));
}

/// Nothing else in the prompt says the agent can author a tool, and the
/// path is not guessable — so it is named, and only where a host is
/// actually running to load what gets written there.
#[test]
fn plugin_guidance_names_the_directory_and_needs_both_python_and_a_host() {
    let with = SystemPromptBuilder::new(&config())
        .home(tmp("plugin_on"))
        .tools(vec!["python".into(), "write".into()])
        .plugins_dir(Some(PathBuf::from("/data/plugins")))
        .build(&[]);
    assert!(with.contains("/data/plugins"), "{with}");
    assert!(with.contains("py__<name>"), "{with}");

    // No host: nowhere to keep a program, so nothing is said about it.
    let hostless = SystemPromptBuilder::new(&config())
        .home(tmp("plugin_nohost"))
        .tools(vec!["python".into()])
        .build(&[]);
    assert!(!hostless.contains("py__<name>"));

    // No `python`: this runtime cannot run a program at all.
    let codeless = SystemPromptBuilder::new(&config())
        .home(tmp("plugin_nocode"))
        .tools(vec!["write".into()])
        .plugins_dir(Some(PathBuf::from("/data/plugins")))
        .build(&[]);
    assert!(!codeless.contains("/data/plugins"));
}

#[test]
fn cron_guidance_appears_only_with_the_cron_tool() {
    let p = SystemPromptBuilder::new(&config())
        .home(tmp("cron"))
        .tools(vec!["cron".into()])
        .build(&[]);
    assert!(p.contains("schedule work on a clock"));
}

#[test]
fn operations_manual_is_opt_in_and_stable_tier() {
    // Absent by default (aux/delegate builders).
    let p = SystemPromptBuilder::new(&config())
        .home(tmp("ops_off"))
        .build(&[]);
    assert!(!p.contains("/wechat login"));
    // Present for the main agent, in the cacheable stable prefix.
    let p = SystemPromptBuilder::new(&config())
        .home(tmp("ops_on"))
        .operations_manual()
        .build(&[]);
    let manual_at = p.find("/wechat login").expect("manual included");
    let date_at = p.find("Today's date is").unwrap();
    assert!(manual_at < date_at, "manual belongs to the stable prefix");
    assert!(p.contains("komo pair approve"));
}

#[test]
fn user_profile_is_opt_in_main_agent_only_and_stable_tier() {
    let home = tmp("user_profile");
    std::fs::write(home.join("USER.md"), "Name: Ada. Prefers terse replies.").unwrap();

    // Off by default (aux/reviewer builders) — profile stays out.
    let off = SystemPromptBuilder::new(&config())
        .home(home.clone())
        .build(&[]);
    assert!(!off.contains("Ada"), "profile must be gated off by default");

    // On for the main agent: injected, labeled, and in the stable prefix.
    let on = SystemPromptBuilder::new(&config())
        .home(home)
        .user_profile()
        .build(&[]);
    assert!(on.contains("Name: Ada. Prefers terse replies."));
    let profile_at = on.find("Ada").unwrap();
    let date_at = on.find("Today's date is").unwrap();
    assert!(profile_at < date_at, "profile belongs to the stable prefix");
    assert!(on.contains("~/.komo/USER.md"), "profile block is labeled");
}

#[test]
fn user_profile_absent_when_file_missing_or_empty() {
    let home = tmp("user_profile_empty");
    // No file at all.
    let p = SystemPromptBuilder::new(&config())
        .home(home.clone())
        .user_profile()
        .build(&[]);
    assert!(!p.contains("~/.komo/USER.md"), "no header when file absent");
    // Present but blank → still nothing injected (filtered on trim).
    std::fs::write(home.join("USER.md"), "\n  \n").unwrap();
    let p = SystemPromptBuilder::new(&config())
        .home(home)
        .user_profile()
        .build(&[]);
    assert!(
        !p.contains("~/.komo/USER.md"),
        "no header for a blank profile"
    );
}

#[test]
fn global_instructions_are_opt_in_main_agent_only_and_stable_tier() {
    let home = tmp("global_home");
    let agents = tmp("global_agents");
    std::fs::write(agents.join("AGENTS.md"), "Always answer in Chinese.").unwrap();

    // Off by default (aux/reviewer builders).
    let off = SystemPromptBuilder::new(&config())
        .home(home.clone())
        .build(&[]);
    assert!(
        !off.contains("Always answer in Chinese."),
        "global instructions must be gated off by default"
    );

    let on = SystemPromptBuilder::new(&config())
        .home(home)
        .global_instructions_in(agents)
        .build(&[]);
    assert!(on.contains("Always answer in Chinese."));
    assert!(on.contains("~/.agents/AGENTS.md"), "the block is labeled");
    let text_at = on.find("Always answer in Chinese.").unwrap();
    let date_at = on.find("Today's date is").unwrap();
    assert!(text_at < date_at, "belongs to the stable prefix");
}

/// Within the machine-wide scope only one file is injected, and komo's own
/// beats the one shared with other agents.
#[test]
fn komo_home_agents_file_outranks_the_shared_one() {
    let home = tmp("global_prec_home");
    let agents = tmp("global_prec_agents");
    std::fs::write(home.join("AGENTS.md"), "komo-specific rule.").unwrap();
    std::fs::write(agents.join("AGENTS.md"), "shared rule.").unwrap();

    let p = SystemPromptBuilder::new(&config())
        .home(home)
        .global_instructions_in(agents)
        .build(&[]);
    assert!(p.contains("komo-specific rule."));
    assert!(!p.contains("shared rule."), "only the winner is injected");
    assert!(p.contains("~/.komo/AGENTS.md"));
    assert!(!p.contains("~/.agents/AGENTS.md"));
}

/// The same rule one scope down: a repo keeping both files (very often
/// `CLAUDE.md` symlinked to `AGENTS.md`) contributes one block, not two.
#[test]
fn workspace_agents_file_outranks_claude_md() {
    let root = tmp("ctx_prec_root");
    std::fs::write(root.join("AGENTS.md"), "canonical project rule.").unwrap();
    std::fs::write(root.join("CLAUDE.md"), "stale copy.").unwrap();

    let p = SystemPromptBuilder::new(&config())
        .home(tmp("ctx_prec_home"))
        .workspace_root(Some(root))
        .build(&[]);
    assert!(p.contains("canonical project rule."));
    assert!(!p.contains("stale copy."));
    assert!(p.contains("project instructions from `AGENTS.md`"));
    assert_eq!(
        p.matches("project instructions from").count(),
        1,
        "exactly one project block"
    );
}

/// Both scopes are in play at once: one machine-wide block and one project
/// block, the project one last so it reads as the more specific override.
#[test]
fn machine_wide_and_project_instructions_both_land() {
    let home = tmp("both_home");
    let agents = tmp("both_agents");
    let root = tmp("both_root");
    std::fs::write(agents.join("AGENTS.md"), "machine-wide rule.").unwrap();
    std::fs::write(root.join("AGENTS.md"), "project rule.").unwrap();

    let p = SystemPromptBuilder::new(&config())
        .home(home)
        .global_instructions_in(agents)
        .workspace_root(Some(root))
        .build(&[]);
    let global_at = p.find("machine-wide rule.").expect("machine-wide block");
    let project_at = p.find("project rule.").expect("project block");
    assert!(global_at < project_at, "project instructions come last");
}

#[test]
fn global_instructions_absent_when_file_missing_or_empty() {
    let home = tmp("global_empty_home");
    let agents = tmp("global_empty_agents");
    let p = SystemPromptBuilder::new(&config())
        .home(home.clone())
        .global_instructions_in(agents.clone())
        .build(&[]);
    assert!(
        !p.contains("global agent instructions"),
        "no header when absent"
    );

    std::fs::write(agents.join("AGENTS.md"), "\n \n").unwrap();
    let p = SystemPromptBuilder::new(&config())
        .home(home)
        .global_instructions_in(agents)
        .build(&[]);
    assert!(
        !p.contains("global agent instructions"),
        "no header when blank"
    );
}

/// A blank `~/.komo/AGENTS.md` must not shadow a real shared one — "present
/// but empty" means the operator keeps nothing there.
#[test]
fn a_blank_higher_priority_file_falls_through() {
    let home = tmp("fallthrough_home");
    let agents = tmp("fallthrough_agents");
    std::fs::write(home.join("AGENTS.md"), "   \n").unwrap();
    std::fs::write(agents.join("AGENTS.md"), "shared rule.").unwrap();

    let p = SystemPromptBuilder::new(&config())
        .home(home)
        .global_instructions_in(agents)
        .build(&[]);
    assert!(p.contains("shared rule."));
    assert!(p.contains("~/.agents/AGENTS.md"));
}

/// What wiring actually reads: the shared file under the **real** home
/// directory, not `KOMO_HOME`; komo's own under `KOMO_HOME`, and it goes first.
#[test]
fn global_instruction_files_resolve_to_the_documented_paths() {
    assert_eq!(default_agents_dir().parent(), dirs::home_dir().as_deref());

    let komo_home = PathBuf::from("/komo-home");
    let files = global_instruction_files(&default_agents_dir(), &komo_home);
    assert_eq!(files[0].0, "~/.komo/AGENTS.md", "komo's own is tried first");
    assert_eq!(files[0].1, komo_home.join("AGENTS.md"));
    assert_eq!(files[1].0, "~/.agents/AGENTS.md");
    assert!(files[1].1.ends_with(".agents/AGENTS.md"));
}

/// The prompt is memoized per builder; editing the shared file has to take
/// effect on the next turn without restarting the gateway.
#[test]
fn editing_global_instructions_busts_the_cache() {
    let agents = tmp("global_cache_agents");
    let path = agents.join("AGENTS.md");
    std::fs::write(&path, "first").unwrap();
    let builder = SystemPromptBuilder::new(&config())
        .home(tmp("global_cache_home"))
        .global_instructions_in(agents);
    assert!(builder.build(&[]).contains("first"));

    std::fs::write(&path, "second").unwrap();
    // mtime is second-precision on some filesystems; move it explicitly so
    // the fingerprint change is not a race.
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(SystemTime::now() + std::time::Duration::from_secs(2))
        .unwrap();
    let rebuilt = builder.build(&[]);
    assert!(rebuilt.contains("second"), "edit must be picked up");
    assert!(!rebuilt.contains("first"));
}

#[test]
fn stable_tier_precedes_volatile_tier() {
    let p = SystemPromptBuilder::new(&config())
        .home(tmp("order"))
        .build(&[]);
    let identity_at = p.find("You are Komo").unwrap();
    let date_at = p.find("Today's date is").unwrap();
    assert!(
        identity_at < date_at,
        "stable identity must precede volatile date"
    );
}

#[test]
fn skills_note_lands_in_stable_tier() {
    let p = SystemPromptBuilder::new(&config())
        .home(tmp("skills"))
        .skills_note(Some("You have skills: foo, bar".into()))
        .build(&[]);
    let note_at = p.find("You have skills").unwrap();
    let date_at = p.find("Today's date is").unwrap();
    assert!(
        note_at < date_at,
        "skills note belongs to the stable prefix"
    );
}

#[test]
fn context_file_is_included_and_labeled() {
    let home = tmp("ctx_home");
    let root = tmp("ctx_root");
    std::fs::write(root.join("AGENTS.md"), "Be terse. Prefer bullet points.").unwrap();
    let p = SystemPromptBuilder::new(&config())
        .home(home)
        .workspace_root(Some(root))
        .build(&[]);
    assert!(p.contains("project instructions from `AGENTS.md`"));
    assert!(p.contains("Prefer bullet points."));
}

/// Named even when no instruction file is there: an answer that found
/// nothing still has to be able to say where it looked.
#[test]
fn workspace_root_is_named_even_without_an_instruction_file() {
    let home = tmp("wsroot_home");
    let root = tmp("wsroot_root");
    let p = SystemPromptBuilder::new(&config())
        .home(home)
        .workspace_root(Some(root.clone()))
        .build(&[]);
    assert!(
        p.contains(&format!("Working directory: {}", root.display())),
        "prompt should name the workspace root: {p}"
    );
}

/// The whole point of the change: a task session carries its own project's
/// instructions, not the gateway process's (under launchd, `~/.komo`).
#[test]
fn a_bound_session_reads_its_own_root_and_an_unbound_one_the_process_root() {
    let process_root = tmp("bound_process");
    let task_root = tmp("bound_task");
    std::fs::write(process_root.join("AGENTS.md"), "gateway home rule.").unwrap();
    std::fs::write(task_root.join("AGENTS.md"), "task project rule.").unwrap();
    let builder = SystemPromptBuilder::new(&config())
        .home(tmp("bound_home"))
        .workspace_root(Some(process_root.clone()));

    let bound = builder.build(&[task_root.display().to_string()]);
    assert!(bound.contains("task project rule."), "{bound}");
    assert!(!bound.contains("gateway home rule."));
    assert!(bound.contains(&format!("Working directory: {}", task_root.display())));

    let unbound = builder.build(&[]);
    assert!(unbound.contains("gateway home rule."), "{unbound}");
    assert!(!unbound.contains("task project rule."));
}

/// `/workspace add` widens what the tools may touch; it does not add a second
/// project's instructions to the prompt.
#[test]
fn only_the_first_root_contributes_project_instructions() {
    let first = tmp("roots_first");
    let second = tmp("roots_second");
    std::fs::write(first.join("AGENTS.md"), "first project rule.").unwrap();
    std::fs::write(second.join("AGENTS.md"), "second project rule.").unwrap();
    let p = SystemPromptBuilder::new(&config())
        .home(tmp("roots_home"))
        .build(&[first.display().to_string(), second.display().to_string()]);
    assert!(p.contains("first project rule."));
    assert!(!p.contains("second project rule."));
    assert_eq!(p.matches("project instructions from").count(), 1);
}

/// One builder serves every session on its runtime, so the memoized render has
/// to be keyed on the root as well as on the mtimes: alternating tasks must not
/// hand each other their project's instructions.
#[test]
fn alternating_tasks_never_inherit_each_others_instructions() {
    let a = tmp("alt_a");
    let b = tmp("alt_b");
    std::fs::write(a.join("AGENTS.md"), "rule of A.").unwrap();
    std::fs::write(b.join("AGENTS.md"), "rule of B.").unwrap();
    let builder = SystemPromptBuilder::new(&config()).home(tmp("alt_home"));
    let for_task = |root: &PathBuf| builder.build(&[root.display().to_string()]);

    assert!(for_task(&a).contains("rule of A."));
    let second = for_task(&b);
    assert!(second.contains("rule of B."), "{second}");
    assert!(!second.contains("rule of A."));
    let back = for_task(&a);
    assert!(back.contains("rule of A."), "{back}");
    assert!(!back.contains("rule of B."));
}

/// Two directories that both keep no instruction file have identical mtime
/// fingerprints, so only the root itself tells the cached renders apart.
#[test]
fn a_task_without_an_instruction_file_still_gets_its_own_directory_named() {
    let a = tmp("noinstr_a");
    let b = tmp("noinstr_b");
    let builder = SystemPromptBuilder::new(&config()).home(tmp("noinstr_home"));
    assert!(
        builder
            .build(&[a.display().to_string()])
            .contains(&format!("Working directory: {}", a.display()))
    );
    let second = builder.build(&[b.display().to_string()]);
    assert!(second.contains(&format!("Working directory: {}", b.display())));
    assert!(!second.contains(&format!("Working directory: {}", a.display())));
}

#[test]
fn persona_override_replaces_builtin_identity() {
    let home = tmp("persona");
    std::fs::write(home.join("SOUL.md"), "You are Nyx, a terse oracle.").unwrap();
    let p = SystemPromptBuilder::new(&config()).home(home).build(&[]);
    assert!(p.contains("You are Nyx, a terse oracle."));
    assert!(!p.contains("You are Komo"));
}

#[test]
fn cached_prompt_picks_up_a_newly_created_context_file() {
    let home = tmp("hot_home");
    let root = tmp("hot_root");
    let builder = SystemPromptBuilder::new(&config())
        .home(home)
        .workspace_root(Some(root.clone()));
    // First build: no context file, so none is mentioned (this seeds cache).
    let first = builder.build(&[]);
    assert!(!first.contains("project instructions"));
    // Create one out-of-band — the mtime fingerprint (None→Some) must bust
    // the cache so the next build reflects it, no restart needed.
    std::fs::write(root.join("AGENTS.md"), "Be terse.").unwrap();
    let second = builder.build(&[]);
    assert!(second.contains("project instructions from `AGENTS.md`"));
    assert!(second.contains("Be terse."));
}

#[test]
fn l1_file_changes_apply_to_next_prompt_and_never_to_aux() {
    let home = tmp("l1_file");
    let path = home.join("MEMORY.md");
    let builder = SystemPromptBuilder::new(&config())
        .home(home.clone())
        .memory();
    assert!(!builder.build(&[]).contains("komo:memory:l1"));
    std::fs::write(&path, "默认用中文回答").unwrap();
    assert!(builder.build(&[]).contains("默认用中文回答"));
    std::fs::write(&path, "使用 Rust 举例，先给结论").unwrap();
    let updated = builder.build(&[]);
    assert!(updated.contains("使用 Rust 举例，先给结论"));
    assert!(!updated.contains("默认用中文回答"));
    let aux = SystemPromptBuilder::new(&config()).home(home.clone());
    assert!(!aux.build(&[]).contains("使用 Rust 举例"));
    std::fs::write(&path, "").unwrap();
    assert!(!builder.build(&[]).contains("komo:memory:l1"));
    std::fs::remove_file(&path).unwrap();
    assert!(!builder.build(&[]).contains("komo:memory:l1"));
}

#[test]
fn l1_file_limit_is_visible_and_unicode_safe() {
    let home = tmp("l1_limit");
    std::fs::write(home.join("MEMORY.md"), "记".repeat(8_001)).unwrap();
    let prompt = SystemPromptBuilder::new(&config())
        .home(home)
        .memory()
        .build(&[]);
    assert!(prompt.contains(&"记".repeat(8_000)));
    assert!(!prompt.contains(&"记".repeat(8_001)));
    assert!(prompt.contains("[... truncated]"));
}

/// The roster of held-back tools, and the two ceilings on it. An MCP server
/// authors its own descriptions and may mount dozens of tools, so an unbounded
/// roster would put back in the prompt what taking the schemas out saved.
mod lazy_roster {
    use super::*;
    use komo_core::domain::{
        catalog::ToolCatalog,
        context::ToolContext,
        tool::{Tool, ToolError, ToolOutput},
    };
    use std::sync::Arc;

    struct Held(&'static str, &'static str);
    #[async_trait::async_trait]
    impl Tool for Held {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            self.1
        }
        fn advertised(&self) -> bool {
            false
        }
        async fn call(
            &self,
            _i: serde_json::Value,
            _c: &ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    struct Shown;
    #[async_trait::async_trait]
    impl Tool for Shown {
        fn name(&self) -> &'static str {
            "read"
        }
        fn description(&self) -> &'static str {
            "already in the schema block"
        }
        async fn call(
            &self,
            _i: serde_json::Value,
            _c: &ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    fn catalog(tools: Vec<Arc<dyn Tool>>) -> Arc<komo_core::domain::catalog::CatalogSnapshot> {
        let catalog = ToolCatalog::new();
        for tool in tools {
            catalog.register(tool);
        }
        catalog.snapshot()
    }

    #[test]
    fn nothing_held_back_means_no_roster() {
        assert!(lazy_tools_note(&catalog(vec![Arc::new(Shown)])).is_none());
    }

    #[test]
    fn the_roster_names_only_the_held_back_tools() {
        let note = lazy_tools_note(&catalog(vec![
            Arc::new(Shown),
            Arc::new(Held("cron", "schedule work")),
        ]))
        .expect("one tool is held back");
        assert!(note.contains("- cron: schedule work"), "{note}");
        assert!(!note.contains("- read:"), "an advertised tool: {note}");
        // The roster is useless without saying how to reach one.
        assert!(note.contains("`tool`"), "{note}");
    }

    /// A server-authored description is not length-checked anywhere else.
    #[test]
    fn a_long_description_is_clipped_on_a_char_boundary() {
        let long: &'static str = Box::leak("字".repeat(1_000).into_boxed_str());
        let note = lazy_tools_note(&catalog(vec![Arc::new(Held("mcp__x__y", long))])).unwrap();
        assert!(note.contains(&"字".repeat(MAX_LAZY_LINE_CHARS)));
        assert!(!note.contains(&"字".repeat(MAX_LAZY_LINE_CHARS + 1)));
        assert!(note.contains('…'));
    }

    /// Past the ceiling the roster says so rather than going on: `tool` with no
    /// arguments is the complete list, and it costs a round only when wanted.
    #[test]
    fn a_long_roster_is_capped_and_says_how_many_it_left_out() {
        let tools: Vec<Arc<dyn Tool>> = (0..MAX_LAZY_LINES + 7)
            .map(|i| {
                let name: &'static str = Box::leak(format!("mcp__s__t{i:03}").into_boxed_str());
                Arc::new(Held(name, "a remote tool")) as Arc<dyn Tool>
            })
            .collect();
        let note = lazy_tools_note(&catalog(tools)).unwrap();
        assert_eq!(
            note.lines().filter(|l| l.starts_with("- ")).count(),
            MAX_LAZY_LINES + 1,
            "the ceiling plus the line saying what it cut"
        );
        assert!(note.contains("…and 7 more"), "{note}");
    }
}
