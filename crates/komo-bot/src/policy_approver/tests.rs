use super::*;
use komo_core::domain::approval::ActionRef;
use komo_core::domain::policy::{Category, Effect, Matcher, Rule};
use komo_services::tool_execution::{SessionContext, SessionOrigin, with_job_grants, with_session};
use std::sync::Mutex;

/// A cron job's turn as the sweep really builds it: it **has** a session
/// (id, ledger, session-scoped tools) and is nonetheless unattended.
fn cron_ctx() -> SessionContext {
    SessionContext::detached("cron:ac-temp:1700000000").with_origin(SessionOrigin::Cron)
}

struct Recording {
    asked: Mutex<bool>,
    answer: bool,
}
#[async_trait]
impl Approver for Recording {
    async fn decide(&self, _request: &ApprovalRequest) -> Decision {
        *self.asked.lock().unwrap() = true;
        self.answer.into()
    }
}

fn allow_rule(value: &str) -> Rule {
    Rule {
        channels: None,
        category: Category::Shell,
        matcher: Matcher::Prefix,
        value: value.to_string(),
        access: None,
        effect: Effect::Allow,
        include_dangerous: false,
        unattended: false,
    }
}

fn shell_req() -> ApprovalRequest {
    ApprovalRequest::normal("run: cargo build").with_action(ActionRef::Shell {
        command: "cargo build".to_string(),
    })
}

#[tokio::test]
async fn auto_allow_skips_inner_within_a_session() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: false,
    });
    let approver = PolicyApprover::wrap(
        Policy::new(vec![allow_rule("cargo ")], Verdict::Ask),
        inner.clone(),
    );
    let ctx = SessionContext::detached("cli-session");
    let allowed = with_session(ctx, approver.approve(&shell_req())).await;
    assert!(allowed);
    assert!(!*inner.asked.lock().unwrap(), "inner must not be consulted");
}

/// "Why did this go through?" is asked long after the fact, and `allowed`
/// alone cannot answer it — every rung produces the same `true`. Each one
/// names itself for the audit record.
#[tokio::test]
async fn every_rung_of_the_ladder_says_which_one_decided() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: true,
    });

    // A config allow rule, matched.
    let approver = PolicyApprover::wrap(
        Policy::new(vec![allow_rule("cargo ")], Verdict::Ask),
        inner.clone(),
    );
    let ctx = SessionContext::detached("cli-session");
    let (decision, by) = with_session(
        ctx,
        async move { approver.decide_reported(&shell_req()).await },
    )
    .await;
    assert!(decision.is_allowed());
    assert_eq!(by, DECIDED_BY_CONFIG_ALLOW);

    // A deny rule.
    let mut deny = allow_rule("cargo ");
    deny.effect = Effect::Deny;
    let approver = PolicyApprover::wrap(Policy::new(vec![deny], Verdict::Ask), inner.clone());
    let ctx = SessionContext::detached("cli-session");
    let (decision, by) = with_session(
        ctx,
        async move { approver.decide_reported(&shell_req()).await },
    )
    .await;
    assert!(!decision.is_allowed());
    assert_eq!(by, DECIDED_BY_CONFIG_DENY);

    // The job's own grants, which is the only way a cron turn gets one.
    let approver = PolicyApprover::wrap(Policy::new(Vec::new(), Verdict::Ask), inner.clone());
    let grant = {
        let mut rule = allow_rule("cargo ");
        rule.unattended = true;
        rule
    };
    let (decision, by) = with_session(
        cron_ctx(),
        with_job_grants(vec![grant], async move {
            approver.decide_reported(&shell_req()).await
        }),
    )
    .await;
    assert!(decision.is_allowed());
    assert_eq!(by, DECIDED_BY_JOB_GRANT);

    // Escalated: whatever the inner approver is, the report is its own.
    // `Recording` does not override the method, so it answers with the
    // trait's default rather than claiming a rung it does not know.
    let approver = PolicyApprover::wrap(Policy::new(Vec::new(), Verdict::Ask), inner.clone());
    let ctx = SessionContext::detached("cli-session");
    let (decision, by) = with_session(
        ctx,
        async move { approver.decide_reported(&shell_req()).await },
    )
    .await;
    assert!(decision.is_allowed());
    assert_eq!(by, komo_core::domain::approval::DECIDED_BY_APPROVER);
    assert!(*inner.asked.lock().unwrap());
}

#[tokio::test]
async fn allow_without_session_falls_through_to_inner() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: false,
    });
    let approver = PolicyApprover::wrap(
        Policy::new(vec![allow_rule("cargo ")], Verdict::Ask),
        inner.clone(),
    );
    // No `with_session`: a sweep-like context. Allow must not auto-grant.
    let allowed = approver.approve(&shell_req()).await;
    assert!(!allowed);
    assert!(*inner.asked.lock().unwrap(), "inner should be consulted");
}

#[tokio::test]
async fn unattended_rule_auto_allows_without_a_session() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: false,
    });
    let mut rule = allow_rule("cargo ");
    rule.unattended = true;
    let approver = PolicyApprover::wrap(Policy::new(vec![rule], Verdict::Ask), inner.clone());
    // No `with_session`: the sweep context. The explicit opt-in grants.
    let allowed = approver.approve(&shell_req()).await;
    assert!(allowed);
    assert!(!*inner.asked.lock().unwrap(), "inner must not be consulted");
}

/// The regression this whole change exists for: a cron turn carries a
/// session id (`cron:<job>:<unix>`), so reading a channel off it made the
/// engine's unattended branch unreachable. A plain allow rule — no
/// `unattended` opt-in — must not grant there.
#[tokio::test]
async fn a_plain_allow_rule_does_not_grant_in_a_cron_turn() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: false,
    });
    let approver = PolicyApprover::wrap(
        Policy::new(vec![allow_rule("cargo ")], Verdict::Ask),
        inner.clone(),
    );
    let allowed = with_session(cron_ctx(), approver.approve(&shell_req())).await;
    assert!(!allowed);
    assert!(
        *inner.asked.lock().unwrap(),
        "a non-unattended allow must fall through, not auto-grant"
    );
}

/// The other half of the same hole: `default_normal = allow` is a *default*,
/// and a default may never be an unattended grant.
#[tokio::test]
async fn default_normal_allow_does_not_grant_in_a_cron_turn() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: false,
    });
    let approver = PolicyApprover::wrap(Policy::new(Vec::new(), Verdict::Allow), inner.clone());
    assert!(!with_session(cron_ctx(), approver.approve(&shell_req())).await);
    assert!(*inner.asked.lock().unwrap());
}

/// …while the explicit opt-in still works, which is what keeps every
/// `unattended = true` rule people already configured doing its job.
#[tokio::test]
async fn an_unattended_rule_grants_in_a_cron_turn() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: false,
    });
    let mut rule = allow_rule("cargo ");
    rule.unattended = true;
    let approver = PolicyApprover::wrap(Policy::new(vec![rule], Verdict::Ask), inner.clone());
    assert!(with_session(cron_ctx(), approver.approve(&shell_req())).await);
    assert!(!*inner.asked.lock().unwrap(), "inner must not be consulted");
}

/// A channel-scoped rule must not match an unattended turn — the session id
/// prefix (`cron`) is not a channel, and treating it as one would let a rule
/// written for a chat channel leak into a scheduled job.
#[tokio::test]
async fn a_channel_scoped_rule_does_not_match_a_cron_turn() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: false,
    });
    let mut rule = allow_rule("cargo ");
    rule.unattended = true;
    rule.channels = Some(vec!["cron".to_string()]);
    let approver = PolicyApprover::wrap(Policy::new(vec![rule], Verdict::Ask), inner.clone());
    assert!(!with_session(cron_ctx(), approver.approve(&shell_req())).await);
    assert!(*inner.asked.lock().unwrap());
}

/// A job's grant reaches the approver through the ambient scope, and
/// grants an action no config rule covers.
#[tokio::test]
async fn a_job_grant_allows_within_its_turn() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: false,
    });
    let approver = PolicyApprover::wrap(Policy::default(), inner.clone());
    let mut grant = allow_rule("cargo ");
    grant.unattended = true;
    let allowed = with_job_grants(
        vec![grant],
        with_session(cron_ctx(), approver.approve(&shell_req())),
    )
    .await;
    assert!(allowed);
    assert!(!*inner.asked.lock().unwrap());
}

/// **The containment guarantee.** One job's grant must not reach anything
/// outside that job's turn — not another job, and not a conversation.
/// Without this the feature is just a global rule with extra steps.
#[tokio::test]
async fn a_job_grant_does_not_escape_its_turn() {
    let approver = PolicyApprover::wrap(
        Policy::default(),
        Arc::new(Recording {
            asked: Mutex::new(false),
            answer: false,
        }),
    );
    let mut grant = allow_rule("cargo ");
    grant.unattended = true;

    // Granted inside…
    assert!(
        with_job_grants(
            vec![grant],
            with_session(cron_ctx(), approver.approve(&shell_req()))
        )
        .await
    );

    // …and gone the moment the scope ends: a later cron turn (a different
    // job) and an ordinary conversation both see nothing.
    assert!(!with_session(cron_ctx(), approver.approve(&shell_req())).await);
    let chat = SessionContext::detached("feishu:oc_abc");
    assert!(!with_session(chat, approver.approve(&shell_req())).await);
}

/// …and an ordinary conversation is untouched: `default_normal = allow`
/// still grants where a human is behind the session.
#[tokio::test]
async fn a_user_turn_still_honors_default_normal_allow() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: false,
    });
    let approver = PolicyApprover::wrap(Policy::new(Vec::new(), Verdict::Allow), inner.clone());
    let ctx = SessionContext::detached("feishu:oc_abc");
    assert!(with_session(ctx, approver.approve(&shell_req())).await);
    assert!(!*inner.asked.lock().unwrap());
}

#[tokio::test]
async fn safe_action_is_blocked_by_a_deny_rule_without_asking() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: true,
    });
    let mut deny = allow_rule("");
    deny.category = Category::Network;
    deny.matcher = Matcher::Suffix;
    deny.value = "internal.corp".to_string();
    deny.effect = Effect::Deny;
    let approver = PolicyApprover::wrap(Policy::new(vec![deny], Verdict::Ask), inner.clone());

    let req = ApprovalRequest::safe("fetch").with_action(ActionRef::Network {
        url: "https://api.internal.corp/secrets".to_string(),
    });
    let ctx = SessionContext::detached("cli-session");
    assert!(!with_session(ctx, approver.approve(&req)).await);
    assert!(!*inner.asked.lock().unwrap(), "safe deny must not prompt");
}

#[tokio::test]
async fn unmatched_safe_action_passes_without_consulting_inner() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: false,
    });
    let approver = PolicyApprover::wrap(Policy::default(), inner.clone());
    let req = ApprovalRequest::safe("fetch").with_action(ActionRef::Network {
        url: "https://example.com".to_string(),
    });
    // Even with no session in scope (sweep/aux), safe stays allowed.
    assert!(approver.approve(&req).await);
    assert!(!*inner.asked.lock().unwrap());
}

/// The `a` answer's whole point: the grant outlives the process. The
/// approver is the only writer, so this is where that is pinned.
#[tokio::test]
async fn always_persists_a_narrow_grant_and_stops_asking() {
    struct Always;
    #[async_trait]
    impl Approver for Always {
        async fn decide(&self, _request: &ApprovalRequest) -> Decision {
            Decision::AllowAlways
        }
    }

    let home = std::env::temp_dir().join("komo_policy_always");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let store = Arc::new(PermissionsStore::load(&home));
    let approver = PolicyApprover::wrap_with_store(
        Policy::default().with_saved(store.rules()),
        Arc::new(Always),
        store.clone(),
    );

    let ctx = SessionContext::detached("cli-session");
    assert!(with_session(ctx.clone(), approver.approve(&shell_req())).await);

    // Saved, narrowed to the command's first token, scoped to the channel.
    let saved = store.list();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].value, "cargo ");
    assert_eq!(saved[0].channels, Some(vec!["cli".to_string()]));

    // …and a fresh process would honor it without asking: build a *deny-all*
    // inner over the reloaded store and confirm the policy short-circuits.
    let reloaded = Arc::new(PermissionsStore::load(&home));
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: false,
    });
    let next = PolicyApprover::wrap_with_store(
        Policy::default().with_saved(reloaded.rules()),
        inner.clone(),
        reloaded,
    );
    assert!(with_session(ctx, next.approve(&shell_req())).await);
    assert!(
        !*inner.asked.lock().unwrap(),
        "a saved grant must not reach the interactive approver"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// No session in scope ⇒ nothing to scope a rule to, so the grant stays
/// session-local rather than being written channel-less (which would apply
/// everywhere — the opposite of narrow).
#[tokio::test]
async fn always_without_a_session_grants_once_and_saves_nothing() {
    struct Always;
    #[async_trait]
    impl Approver for Always {
        async fn decide(&self, _request: &ApprovalRequest) -> Decision {
            Decision::AllowAlways
        }
    }

    let home = std::env::temp_dir().join("komo_policy_always_nosession");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let store = Arc::new(PermissionsStore::load(&home));
    let approver = PolicyApprover::wrap_with_store(
        Policy::default().with_saved(store.rules()),
        Arc::new(Always),
        store.clone(),
    );

    assert!(approver.approve(&shell_req()).await);
    assert!(store.is_empty());

    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn ask_delegates_to_inner() {
    let inner = Arc::new(Recording {
        asked: Mutex::new(false),
        answer: true,
    });
    let approver = PolicyApprover::wrap(Policy::default(), inner.clone());
    let ctx = SessionContext::detached("cli-session");
    let allowed = with_session(ctx, approver.approve(&shell_req())).await;
    assert!(allowed);
    assert!(*inner.asked.lock().unwrap());
}
