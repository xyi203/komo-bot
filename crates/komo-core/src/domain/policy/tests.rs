use super::*;
use std::path::PathBuf;

fn shell(cmd: &str, risk: Risk) -> ApprovalRequest {
    let mut req = ApprovalRequest::normal(format!("run: {cmd}"));
    req.risk = risk;
    req.with_action(ActionRef::Shell {
        command: cmd.to_string(),
    })
}

fn file_write(path: &str) -> ApprovalRequest {
    ApprovalRequest::normal("write").with_action(ActionRef::File {
        path: PathBuf::from(path),
        write: true,
    })
}

fn rule(category: Category, matcher: Matcher, value: &str, effect: Effect) -> Rule {
    Rule {
        channels: None,
        category,
        matcher,
        value: value.to_string(),
        access: None,
        effect,
        include_dangerous: false,
        unattended: false,
    }
}

#[test]
fn unattended_grants_only_through_an_explicit_unattended_rule() {
    let mut r = rule(Category::Shell, Matcher::Prefix, "curl ", Effect::Allow);
    // Plain allow: grants in a session, not unattended.
    let p = Policy::new(vec![r.clone()], Verdict::Ask);
    assert_eq!(
        p.decide(&shell("curl http://x", Risk::Normal), Some("cli"))
            .verdict,
        Verdict::Allow
    );
    assert_eq!(
        p.decide(&shell("curl http://x", Risk::Normal), None)
            .verdict,
        Verdict::Ask,
        "non-unattended allow must not grant without a session"
    );
    // Opt-in: grants unattended too.
    r.unattended = true;
    let p = Policy::new(vec![r], Verdict::Ask);
    let d = p.decide(&shell("curl http://x", Risk::Normal), None);
    assert_eq!(d.verdict, Verdict::Allow);
    assert_eq!(d.rule, Some(0));
}

#[test]
fn default_allow_never_grants_unattended() {
    let p = Policy::new(Vec::new(), Verdict::Allow);
    assert_eq!(
        p.decide(&shell("ls", Risk::Normal), Some("cli")).verdict,
        Verdict::Allow
    );
    assert_eq!(
        p.decide(&shell("ls", Risk::Normal), None).verdict,
        Verdict::Ask,
        "a default can never be an unattended grant"
    );
}

#[test]
fn safe_actions_get_deny_only_evaluation() {
    let net = |url: &str| {
        ApprovalRequest::safe("fetch").with_action(ActionRef::Network {
            url: url.to_string(),
        })
    };
    let p = Policy::new(
        vec![
            // An allow rule must be irrelevant to safe actions…
            rule(
                Category::Network,
                Matcher::Suffix,
                "github.com",
                Effect::Allow,
            ),
            rule(
                Category::Network,
                Matcher::Suffix,
                "internal.corp",
                Effect::Deny,
            ),
        ],
        // …and so must default_normal: even Deny leaves unmatched safe alone.
        Verdict::Deny,
    );
    let denied = p.decide(&net("https://api.internal.corp/x"), Some("cli"));
    assert_eq!(denied.verdict, Verdict::Deny);
    assert_eq!(denied.rule, Some(1));

    let unmatched = p.decide(&net("https://example.com"), Some("cli"));
    assert_eq!(unmatched.verdict, Verdict::Allow);
    assert_eq!(unmatched.rule, None);
}

#[test]
fn decide_reports_the_matching_rule_index() {
    let p = Policy::new(
        vec![
            rule(Category::Shell, Matcher::Prefix, "cargo ", Effect::Allow),
            rule(Category::Shell, Matcher::Prefix, "git ", Effect::Allow),
        ],
        Verdict::Ask,
    );
    let d = p.decide(&shell("git status", Risk::Normal), Some("cli"));
    assert_eq!(d.verdict, Verdict::Allow);
    assert_eq!(d.rule, Some(1));
}

#[test]
fn empty_policy_asks_for_normal_and_dangerous() {
    let p = Policy::default();
    assert_eq!(
        p.decide(&shell("ls", Risk::Normal), Some("cli")).verdict,
        Verdict::Ask
    );
    assert_eq!(
        p.decide(&shell("rm -rf x", Risk::Dangerous), Some("cli"))
            .verdict,
        Verdict::Ask
    );
}

#[test]
fn allow_rule_matches_command_prefix() {
    let p = Policy::new(
        vec![rule(
            Category::Shell,
            Matcher::Prefix,
            "cargo ",
            Effect::Allow,
        )],
        Verdict::Ask,
    );
    assert_eq!(
        p.decide(&shell("cargo build", Risk::Normal), Some("cli"))
            .verdict,
        Verdict::Allow
    );
    assert_eq!(
        p.decide(&shell("npm install", Risk::Normal), Some("cli"))
            .verdict,
        Verdict::Ask
    );
}

#[test]
fn deny_rule_beats_allow_regardless_of_order() {
    let p = Policy::new(
        vec![
            rule(Category::Shell, Matcher::Prefix, "git ", Effect::Allow),
            rule(Category::Shell, Matcher::Contains, "push", Effect::Deny),
        ],
        Verdict::Ask,
    );
    assert_eq!(
        p.decide(&shell("git push origin", Risk::Normal), Some("cli"))
            .verdict,
        Verdict::Deny
    );
}

#[test]
fn allow_rule_does_not_grant_dangerous_without_opt_in() {
    let p = Policy::new(
        vec![rule(Category::Shell, Matcher::Prefix, "rm ", Effect::Allow)],
        Verdict::Ask,
    );
    assert_eq!(
        p.decide(&shell("rm file", Risk::Dangerous), Some("cli"))
            .verdict,
        Verdict::Ask
    );

    let mut allow_dangerous = rule(Category::Shell, Matcher::Prefix, "rm ", Effect::Allow);
    allow_dangerous.include_dangerous = true;
    let p = Policy::new(vec![allow_dangerous], Verdict::Ask);
    assert_eq!(
        p.decide(&shell("rm file", Risk::Dangerous), Some("cli"))
            .verdict,
        Verdict::Allow
    );
}

#[test]
fn file_write_prefix_and_access_scope() {
    let mut r = rule(
        Category::File,
        Matcher::Prefix,
        "/home/me/proj",
        Effect::Allow,
    );
    r.access = Some(Access::Write);
    let p = Policy::new(vec![r], Verdict::Ask);
    assert_eq!(
        p.decide(&file_write("/home/me/proj/src/x.rs"), Some("cli"))
            .verdict,
        Verdict::Allow
    );
    assert_eq!(
        p.decide(&file_write("/etc/passwd"), Some("cli")).verdict,
        Verdict::Ask
    );
}

#[test]
fn channel_scope_limits_a_rule() {
    let mut r = rule(Category::Shell, Matcher::Prefix, "cargo ", Effect::Allow);
    r.channels = Some(vec!["cli".to_string()]);
    let p = Policy::new(vec![r], Verdict::Ask);
    assert_eq!(
        p.decide(&shell("cargo build", Risk::Normal), Some("cli"))
            .verdict,
        Verdict::Allow
    );
    assert_eq!(
        p.decide(&shell("cargo build", Risk::Normal), Some("feishu"))
            .verdict,
        Verdict::Ask
    );
    // No session in scope → a channel-scoped rule never matches.
    assert_eq!(
        p.decide(&shell("cargo build", Risk::Normal), None).verdict,
        Verdict::Ask
    );
}

#[test]
fn network_suffix_matches_on_dot_boundary() {
    let net = |url: &str| {
        ApprovalRequest::normal("fetch").with_action(ActionRef::Network {
            url: url.to_string(),
        })
    };
    let p = Policy::new(
        vec![rule(
            Category::Network,
            Matcher::Suffix,
            "github.com",
            Effect::Allow,
        )],
        Verdict::Ask,
    );
    assert_eq!(
        p.decide(&net("https://api.github.com/repos"), Some("cli"))
            .verdict,
        Verdict::Allow
    );
    assert_eq!(
        p.decide(&net("https://github.com"), Some("cli")).verdict,
        Verdict::Allow
    );
    // Not a real subdomain — must not match.
    assert_eq!(
        p.decide(&net("https://evilgithub.com"), Some("cli"))
            .verdict,
        Verdict::Ask
    );
}

/// The catalog filter: only an unscoped wildcard deny takes a tool away.
#[test]
fn wholly_denied_only_for_an_unscoped_wildcard_deny() {
    let wildcard = |category, effect| rule(category, Matcher::Any, "", effect);

    let p = Policy::new(vec![wildcard(Category::Shell, Effect::Deny)], Verdict::Ask);
    assert!(p.wholly_denied(Category::Shell, None));
    assert!(!p.wholly_denied(Category::Network, None));

    // A value-scoped deny still permits other commands ⇒ keep the tool.
    let p = Policy::new(
        vec![rule(Category::Shell, Matcher::Contains, "rm", Effect::Deny)],
        Verdict::Ask,
    );
    assert!(!p.wholly_denied(Category::Shell, None));

    // A channel-scoped deny leaves the tool usable elsewhere ⇒ keep it.
    let mut scoped = wildcard(Category::Shell, Effect::Deny);
    scoped.channels = Some(vec!["feishu".to_string()]);
    assert!(!Policy::new(vec![scoped], Verdict::Ask).wholly_denied(Category::Shell, None));

    // An allow rule never removes anything, and neither does default_normal.
    let p = Policy::new(
        vec![wildcard(Category::Shell, Effect::Allow)],
        Verdict::Deny,
    );
    assert!(!p.wholly_denied(Category::Shell, None));
}

/// `file` splits by access: banning writes must not take the readers away.
#[test]
fn wholly_denied_respects_file_access_scope() {
    let mut write_ban = rule(Category::File, Matcher::Any, "", Effect::Deny);
    write_ban.access = Some(Access::Write);
    let p = Policy::new(vec![write_ban], Verdict::Ask);
    assert!(p.wholly_denied(Category::File, Some(Access::Write)));
    assert!(!p.wholly_denied(Category::File, Some(Access::Read)));
    assert!(!p.wholly_denied(Category::File, None));

    // Unscoped by access ⇒ both halves go.
    let p = Policy::new(
        vec![rule(Category::File, Matcher::Any, "", Effect::Deny)],
        Verdict::Ask,
    );
    assert!(p.wholly_denied(Category::File, Some(Access::Read)));
    assert!(p.wholly_denied(Category::File, Some(Access::Write)));
}

#[test]
fn any_matcher_matches_every_target() {
    let p = Policy::new(
        vec![rule(Category::Shell, Matcher::Any, "", Effect::Deny)],
        Verdict::Ask,
    );
    assert_eq!(
        p.decide(&shell("anything at all", Risk::Normal), Some("cli"))
            .verdict,
        Verdict::Deny
    );
}

fn saved(rules: Vec<Rule>) -> SavedRules {
    std::sync::Arc::new(std::sync::RwLock::new(rules))
}

/// A saved grant shortcuts the prompt — but only inside a session, and only
/// for an action the operator could have been asked about.
#[test]
fn a_saved_grant_allows_where_config_would_ask() {
    let grant = Rule::narrowest_for(
        &ActionRef::Shell {
            command: "cargo build".into(),
        },
        "cli",
    )
    .unwrap();
    let p = Policy::new(Vec::new(), Verdict::Ask).with_saved(saved(vec![grant]));

    let d = p.decide(&shell("cargo test", Risk::Normal), Some("cli"));
    assert_eq!(d.verdict, Verdict::Allow);
    assert_eq!(
        d.source,
        RuleSource::Saved,
        "the decision must name the saved list"
    );
    assert_eq!(d.rule, Some(0));
    // Scoped to the channel it was granted on.
    assert_eq!(
        p.decide(&shell("cargo test", Risk::Normal), Some("feishu"))
            .verdict,
        Verdict::Ask
    );
    // And narrow: a different command still asks.
    assert_eq!(
        p.decide(&shell("npm install", Risk::Normal), Some("cli"))
            .verdict,
        Verdict::Ask
    );
}

/// Constraint 1: a config deny outranks any saved grant. Otherwise "remember
/// this" would let a prompt answer override what the operator wrote down.
#[test]
fn a_config_deny_beats_a_saved_grant() {
    let grant = Rule::narrowest_for(
        &ActionRef::Shell {
            command: "git push".into(),
        },
        "cli",
    )
    .unwrap();
    let p = Policy::new(
        vec![rule(
            Category::Shell,
            Matcher::Contains,
            "push",
            Effect::Deny,
        )],
        Verdict::Ask,
    )
    .with_saved(saved(vec![grant]));
    assert_eq!(
        p.decide(&shell("git push origin", Risk::Normal), Some("cli"))
            .verdict,
        Verdict::Deny
    );
}

/// Constraint 2: "remember this" must never turn a dangerous action into a
/// silent one — that stays a config-only `include_dangerous` opt-in.
#[test]
fn a_saved_grant_never_covers_a_dangerous_action() {
    let grant = Rule::narrowest_for(
        &ActionRef::Shell {
            command: "rm file".into(),
        },
        "cli",
    )
    .unwrap();
    let p = Policy::new(Vec::new(), Verdict::Ask).with_saved(saved(vec![grant]));
    assert_eq!(
        p.decide(&shell("rm file", Risk::Dangerous), Some("cli"))
            .verdict,
        Verdict::Ask
    );
}

/// Constraint 3: saved grants were accumulated interactively, so an
/// unattended turn (cron / sweep — no channel) must not read them.
#[test]
fn a_saved_grant_is_not_read_in_an_unattended_context() {
    let grant = Rule::narrowest_for(
        &ActionRef::Shell {
            command: "cargo build".into(),
        },
        "cli",
    )
    .unwrap();
    let p = Policy::new(Vec::new(), Verdict::Ask).with_saved(saved(vec![grant]));
    assert_eq!(
        p.decide(&shell("cargo build", Risk::Normal), None).verdict,
        Verdict::Ask
    );
}

// ── job grants ──────────────────────────────────────────────────────────
//
// A scheduled job's own approved actions, granted when a human created it.

fn job_grant(value: &str) -> Rule {
    let mut r = rule(Category::Shell, Matcher::Prefix, value, Effect::Allow);
    r.unattended = true;
    r
}

/// The point of the feature: a job grant is honored in the unattended turn
/// where nothing else would be, without any config rule existing.
#[test]
fn a_job_grant_allows_its_action_unattended() {
    let p = Policy::new(Vec::new(), Verdict::Ask);
    let grants = [job_grant("cargo ")];
    let d = p.decide_with_grants(&shell("cargo build", Risk::Normal), None, &grants);
    assert_eq!(d.verdict, Verdict::Allow);
    assert_eq!(d.source, RuleSource::JobGrant);
    assert_eq!(d.rule, Some(0));
}

/// …and only that action. A grant is a whitelist, not a mode switch.
#[test]
fn a_job_grant_does_not_cover_an_unlisted_action() {
    let p = Policy::new(Vec::new(), Verdict::Ask);
    let grants = [job_grant("cargo ")];
    assert_eq!(
        p.decide_with_grants(&shell("rm -rf /", Risk::Normal), None, &grants)
            .verdict,
        Verdict::Ask
    );
}

/// A config deny outranks a job grant — the operator's config.toml is above
/// anything approved at job-creation time, exactly as it is above a saved
/// grant.
#[test]
fn a_config_deny_beats_a_job_grant() {
    let p = Policy::new(
        vec![rule(
            Category::Shell,
            Matcher::Contains,
            "push",
            Effect::Deny,
        )],
        Verdict::Ask,
    );
    let grants = [job_grant("git ")];
    let d = p.decide_with_grants(&shell("git push origin", Risk::Normal), None, &grants);
    assert_eq!(d.verdict, Verdict::Deny);
    assert_eq!(d.source, RuleSource::Config);
}

/// `include_dangerous` stays a config-only opt-in: approving a job's action
/// list must not silently make a dangerous action unattended.
#[test]
fn a_job_grant_never_covers_a_dangerous_action() {
    let p = Policy::new(Vec::new(), Verdict::Ask);
    let mut grant = job_grant("rm ");
    grant.include_dangerous = true; // even asking for it changes nothing
    assert_eq!(
        p.decide_with_grants(&shell("rm file", Risk::Dangerous), None, &[grant])
            .verdict,
        Verdict::Ask
    );
}

/// A job grant sits above a saved grant: it was approved against a named,
/// visible list, where a saved grant was generalized from one prompt.
#[test]
fn a_job_grant_outranks_a_saved_grant() {
    let saved_rule = Rule::narrowest_for(
        &ActionRef::Shell {
            command: "cargo build".into(),
        },
        "cli",
    )
    .unwrap();
    let p = Policy::new(Vec::new(), Verdict::Ask).with_saved(saved(vec![saved_rule]));
    let d = p.decide_with_grants(
        &shell("cargo build", Risk::Normal),
        Some("cli"),
        &[job_grant("cargo ")],
    );
    assert_eq!(d.source, RuleSource::JobGrant);
}

/// `decide` is `decide_with_grants` with no grants — so every existing
/// caller keeps its exact behavior and nothing grants by accident.
#[test]
fn no_grants_decides_exactly_as_before() {
    let p = Policy::new(Vec::new(), Verdict::Ask);
    assert_eq!(
        p.decide_with_grants(&shell("cargo build", Risk::Normal), None, &[])
            .verdict,
        p.decide(&shell("cargo build", Risk::Normal), None).verdict
    );
}

/// A grant saved mid-session applies to the very next decision: the store and
/// the policy share one list, so nothing has to be rebuilt.
#[test]
fn a_grant_added_after_construction_is_seen_immediately() {
    let list = saved(Vec::new());
    let p = Policy::new(Vec::new(), Verdict::Ask).with_saved(list.clone());
    assert_eq!(
        p.decide(&shell("cargo build", Risk::Normal), Some("cli"))
            .verdict,
        Verdict::Ask
    );

    list.write().unwrap().push(
        Rule::narrowest_for(
            &ActionRef::Shell {
                command: "cargo build".into(),
            },
            "cli",
        )
        .unwrap(),
    );
    assert_eq!(
        p.decide(&shell("cargo build", Risk::Normal), Some("cli"))
            .verdict,
        Verdict::Allow
    );
}

/// What "narrowest" means, per action kind — the operator is answering one
/// prompt, not granting a category.
#[test]
fn narrowest_for_generalizes_just_far_enough() {
    let shell_rule = Rule::narrowest_for(
        &ActionRef::Shell {
            command: "cargo build --release".into(),
        },
        "cli",
    )
    .unwrap();
    assert_eq!(shell_rule.matcher, Matcher::Prefix);
    assert_eq!(shell_rule.value, "cargo ");
    assert_eq!(shell_rule.channels, Some(vec!["cli".to_string()]));
    // A bare command has no arguments to separate from.
    assert_eq!(
        Rule::narrowest_for(
            &ActionRef::Shell {
                command: "make".into()
            },
            "cli"
        )
        .unwrap()
        .value,
        "make"
    );

    let file_rule = Rule::narrowest_for(
        &ActionRef::File {
            path: PathBuf::from("/home/me/proj/src/main.rs"),
            write: true,
        },
        "cli",
    )
    .unwrap();
    assert_eq!(file_rule.value, "/home/me/proj/src/");
    assert_eq!(file_rule.access, Some(Access::Write));
    // The write grant must not also cover reads (or vice versa).
    let read_req = ApprovalRequest::normal("read").with_action(ActionRef::File {
        path: PathBuf::from("/home/me/proj/src/main.rs"),
        write: false,
    });
    let p = Policy::new(Vec::new(), Verdict::Ask).with_saved(saved(vec![file_rule]));
    assert_ne!(p.decide(&read_req, Some("cli")).source, RuleSource::Saved);

    let net_rule = Rule::narrowest_for(
        &ActionRef::Network {
            url: "https://api.github.com/repos/x".into(),
        },
        "cli",
    )
    .unwrap();
    assert_eq!(net_rule.matcher, Matcher::Suffix);
    assert_eq!(net_rule.value, "api.github.com");

    let ha_rule = Rule::narrowest_for(
        &ActionRef::Service {
            domain: "light".into(),
            service: "turn_on".into(),
        },
        "cli",
    )
    .unwrap();
    assert_eq!(ha_rule.matcher, Matcher::Exact);
    assert_eq!(ha_rule.value, "light.turn_on");

    // A relative filename has no directory to generalize to.
    assert!(
        Rule::narrowest_for(
            &ActionRef::File {
                path: PathBuf::from("notes.txt"),
                write: true
            },
            "cli"
        )
        .is_none()
    );
}

#[test]
fn a_turns_channel_is_its_correspondent_or_the_local_one() {
    use crate::domain::context::SessionContext;
    use crate::domain::session::ChannelPeer;

    let chat = SessionContext::detached("0192f0aa-1111-7000-8000-000000000000")
        .with_channel(Some(ChannelPeer::new("feishu", "oc_abc")));
    assert_eq!(chat.channel_name(), "feishu");

    // No correspondent — TUI, desktop, web, CLI all evaluate as one
    // operator at one machine. The session id says nothing either way,
    // which is the point: it is a handle, not a schema.
    let local = SessionContext::detached("0192f0aa-1111-7000-8000-000000000000");
    assert_eq!(local.channel_name(), LOCAL_CHANNEL);
    let looks_like_a_channel = SessionContext::detached("feishu:oc_abc");
    assert_eq!(looks_like_a_channel.channel_name(), LOCAL_CHANNEL);
}

#[test]
fn default_normal_can_deny() {
    let p = Policy::new(Vec::new(), Verdict::Deny);
    assert_eq!(
        p.decide(&shell("ls", Risk::Normal), Some("feishu")).verdict,
        Verdict::Deny
    );
    // Dangerous still asks regardless of default_normal.
    assert_eq!(
        p.decide(&shell("rm x", Risk::Dangerous), Some("feishu"))
            .verdict,
        Verdict::Ask
    );
}

fn mcp(server: &str, tool: &str) -> ApprovalRequest {
    ApprovalRequest::normal(format!("call MCP tool `{server}.{tool}`")).with_action(
        ActionRef::Mcp {
            server: server.to_string(),
            tool: tool.to_string(),
        },
    )
}

#[test]
fn mcp_rules_target_one_server_and_tool() {
    let p = Policy::new(
        vec![rule(
            Category::Mcp,
            Matcher::Exact,
            "memos.list_memos",
            Effect::Allow,
        )],
        Verdict::Ask,
    );
    assert_eq!(
        p.decide(&mcp("memos", "list_memos"), Some("cli")).verdict,
        Verdict::Allow
    );
    // A write on the same server is untouched by a read's grant…
    assert_eq!(
        p.decide(&mcp("memos", "create_memo"), Some("cli")).verdict,
        Verdict::Ask
    );
    // …and so is the same tool name on a different server.
    assert_eq!(
        p.decide(&mcp("notes", "list_memos"), Some("cli")).verdict,
        Verdict::Ask
    );
}

#[test]
fn mcp_prefix_rules_can_scope_a_whole_server() {
    let p = Policy::new(
        vec![rule(Category::Mcp, Matcher::Prefix, "memos.", Effect::Deny)],
        Verdict::Allow,
    );
    assert_eq!(
        p.decide(&mcp("memos", "delete_memo"), Some("cli")).verdict,
        Verdict::Deny
    );
    assert_eq!(
        p.decide(&mcp("notes", "delete_memo"), Some("cli")).verdict,
        Verdict::Allow
    );
}

#[test]
fn remembering_an_mcp_approval_grants_only_that_tool() {
    let saved = Rule::narrowest_for(
        &ActionRef::Mcp {
            server: "memos".into(),
            tool: "create_memo".into(),
        },
        "cli",
    )
    .expect("an mcp action can always be generalized");
    assert_eq!(saved.category, Category::Mcp);
    assert_eq!(saved.matcher, Matcher::Exact);
    assert_eq!(saved.value, "memos.create_memo");
    assert_eq!(
        saved.channels.as_deref(),
        Some(["cli".to_string()].as_slice())
    );

    let p = Policy::new(vec![saved], Verdict::Ask);
    assert_eq!(
        p.decide(&mcp("memos", "create_memo"), Some("cli")).verdict,
        Verdict::Allow
    );
    assert_eq!(
        p.decide(&mcp("memos", "delete_memo"), Some("cli")).verdict,
        Verdict::Ask,
        "approving one tool must not grant the server"
    );
}
