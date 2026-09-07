use super::*;
use komo_core::domain::policy::{Access, Category, Matcher, PolicyMode};

fn mode_of(raw: Option<&str>) -> (PolicyMode, usize) {
    let mut issues = Vec::new();
    let report = build_policy(
        PolicyFileConfig {
            mode: raw.map(str::to_string),
            ..Default::default()
        },
        &mut issues,
    );
    (report.mode, issues.len())
}

/// A typo must never turn the reviewer on: an unreadable mode falls back to
/// `ask` **and** says so, rather than silently widening the gate.
#[test]
fn policy_mode_defaults_to_ask_and_warns_on_a_bad_value() {
    assert_eq!(mode_of(None), (PolicyMode::Ask, 0));
    assert_eq!(mode_of(Some("ask")), (PolicyMode::Ask, 0));
    assert_eq!(mode_of(Some(" AUTO ")), (PolicyMode::Auto, 0));
    assert_eq!(mode_of(Some("yolo")), (PolicyMode::Ask, 1));
}

fn rule(category: &str, matcher: &str, value: &str) -> PolicyRuleFileConfig {
    PolicyRuleFileConfig {
        category: category.to_string(),
        matcher: matcher.to_string(),
        value: value.to_string(),
        effect: "deny".to_string(),
        ..Default::default()
    }
}

/// `category = "shell", effect = "deny"` with no `match`/`value` is the
/// whole-category form — it must survive parsing, since it's what takes a
/// tool out of the model's catalog.
#[test]
fn a_rule_without_match_or_value_is_a_wildcard() {
    let parsed = build_rule(rule("shell", "", "")).expect("wildcard rule is valid");
    assert_eq!(parsed.matcher, Matcher::Any);
    assert_eq!(parsed.category, Category::Shell);

    // Still scopable by access — deny every write, leave reads alone.
    let mut r = rule("file", "", "");
    r.access = Some("write".to_string());
    let parsed = build_rule(r).unwrap();
    assert_eq!(parsed.matcher, Matcher::Any);
    assert_eq!(parsed.access, Some(Access::Write));
}

/// A matcher with nothing to compare is a config mistake, not a wildcard:
/// reading `prefix ""` as "everything" would be the worst possible way for
/// the operator to discover the typo.
#[test]
fn a_matcher_without_a_value_stays_invalid() {
    assert!(build_rule(rule("shell", "prefix", "")).is_none());
    assert!(build_rule(rule("shell", "nonsense", "x")).is_none());
    assert!(build_rule(rule("nonsense", "prefix", "x")).is_none());
}
