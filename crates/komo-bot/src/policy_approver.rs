//! [`PolicyApprover`] — the configurable permission layer (roadmap §3).
//!
//! A decorator over the interactive approver (`CliApprover` / `ChatApprover`):
//! it consults the resolved [`Policy`] first, and only escalates to the wrapped
//! approver when the policy returns [`Verdict::Ask`]. This keeps the per-action
//! decision logic in one configurable place instead of scattered `if/else` in
//! each tool, while leaving each tool's own hardline floor untouched below it.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::info;

use komo_core::domain::{
    approval::{
        ApprovalRequest, Approver, DECIDED_BY_CONFIG_ALLOW, DECIDED_BY_CONFIG_DENY,
        DECIDED_BY_DEFAULT, DECIDED_BY_JOB_GRANT, DECIDED_BY_SAVED_GRANT, Decision, Risk,
    },
    policy::{Policy, Rule, RuleSource, Verdict},
};
use komo_infra::permissions_store::PermissionsStore;
use komo_services::tool_execution::{current_job_grants, current_session};

/// Wraps an [`Approver`], applying a [`Policy`] before falling back to it.
pub struct PolicyApprover {
    policy: Policy,
    inner: Arc<dyn Approver>,
    /// Where an "always allow" answer is persisted. `None` for the unattended
    /// approver (cron), which can never receive that answer anyway — there is
    /// nobody at the prompt.
    saved: Option<Arc<PermissionsStore>>,
}

impl PolicyApprover {
    /// Wrap `inner` with `policy`. Returns the trait object the tools depend on.
    pub fn wrap(policy: Policy, inner: Arc<dyn Approver>) -> Arc<dyn Approver> {
        Arc::new(Self {
            policy,
            inner,
            saved: None,
        })
    }

    /// [`wrap`](Self::wrap), plus the store that makes an `a` answer durable.
    /// This is the **only** place a grant is written: the interactive approvers
    /// just report which answer came back, so the three of them (CLI, chat, TUI)
    /// can't drift on what "always" means.
    pub fn wrap_with_store(
        policy: Policy,
        inner: Arc<dyn Approver>,
        saved: Arc<PermissionsStore>,
    ) -> Arc<dyn Approver> {
        Arc::new(Self {
            policy,
            inner,
            saved: Some(saved),
        })
    }

    /// Persist the narrowest rule covering `request`, scoped to the channel the
    /// answer came from. Best-effort and never fails the call: the user said yes,
    /// and the only thing at stake is whether they get asked again.
    fn remember(&self, request: &ApprovalRequest, channel: Option<&str>) {
        let (Some(store), Some(action), Some(channel)) =
            (self.saved.as_ref(), request.action.as_ref(), channel)
        else {
            // No store, no structured action to generalize, or no session to
            // scope to — nothing safe to write, so the grant stays session-local.
            info!(summary = %request.summary, "always-allow could not be saved; granted once");
            return;
        };
        let Some(rule) = Rule::narrowest_for(action, channel) else {
            return;
        };
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let described = rule.describe();
        if store.remember(rule, &now) {
            info!(rule = %described, "saved an always-allow grant");
        }
    }
}

#[async_trait]
impl Approver for PolicyApprover {
    async fn decide(&self, request: &ApprovalRequest) -> Decision {
        self.decide_reported(request).await.0
    }

    /// The ladder, and which rung of it answered. One implementation for both
    /// entry points: a rung that decides differently depending on who asked is
    /// a rung the audit record cannot describe.
    async fn decide_reported(&self, request: &ApprovalRequest) -> (Decision, &'static str) {
        // An unattended turn (cron) is evaluated channel-lessly even though
        // it *has* a session: `SessionOrigin` is what says nobody is
        // watching, and only that makes the engine's unattended branch run —
        // reading `cron:<job>:<unix>` as a channel would let `default_normal =
        // allow` and plain (non-`unattended`) allow rules grant there.
        let channel = current_session()
            .filter(|c| !c.is_unattended())
            .map(|c| c.channel_name().to_string());
        // The running job's own grants, if this is a scheduled job's turn.
        let grants = current_job_grants();

        // Read-only actions get deny-only evaluation: a deny rule can block a
        // network fetch / file read, but nothing escalates one to a prompt — an
        // unmatched safe action stays allowed without consulting the inner
        // approver (which would auto-pass it anyway).
        if request.risk == Risk::Safe {
            let decision = self.policy.decide(request, channel.as_deref());
            if decision.verdict == Verdict::Deny {
                info!(summary = %request.summary, channel = ?channel, rule = ?decision.rule,
                      "policy: denied (safe action)");
                return (policy_denial(decision), DECIDED_BY_CONFIG_DENY);
            }
            return (Decision::Allow, DECIDED_BY_DEFAULT);
        }

        let decision = self
            .policy
            .decide_with_grants(request, channel.as_deref(), &grants);
        match decision.verdict {
            Verdict::Deny => {
                info!(summary = %request.summary, channel = ?channel, rule = ?decision.rule,
                      "policy: denied");
                (policy_denial(decision), DECIDED_BY_CONFIG_DENY)
            }
            // The engine already gates unattended grants: with `channel = None`
            // only an explicitly `unattended` allow rule or one of this job's
            // own grants (never a default) produces `Allow`, so an Allow here is
            // safe to honor as-is. The source is logged because it is the only
            // way to answer "why did this go through" after the fact — and
            // recorded on the event for the same reason.
            Verdict::Allow => {
                info!(summary = %request.summary, channel = ?channel, rule = ?decision.rule,
                      source = ?decision.source, "policy: auto-allowed");
                (Decision::Allow, granted_by(&decision))
            }
            // Escalated: whoever the prompt reaches — the auto-reviewer, then a
            // person — is the rung that decided, and says so itself.
            Verdict::Ask => match self.inner.decide_reported(request).await {
                // "Allow, and remember it": persist here, then hand the caller a
                // plain Allow — no tool needs to know the difference.
                (Decision::AllowAlways, by) => {
                    self.remember(request, channel.as_deref());
                    (Decision::Allow, by)
                }
                other => other,
            },
        }
    }
}

/// Which list an auto-allow came from. A saved grant and a config rule are the
/// same `Allow` to the caller and completely different answers to "who let this
/// happen".
fn granted_by(decision: &komo_core::domain::policy::Decision) -> &'static str {
    match decision.source {
        RuleSource::Saved => DECIDED_BY_SAVED_GRANT,
        RuleSource::JobGrant => DECIDED_BY_JOB_GRANT,
        RuleSource::Config if decision.rule.is_some() => DECIDED_BY_CONFIG_ALLOW,
        // No rule matched: `default_normal`, or a safe action's floor.
        RuleSource::Config => DECIDED_BY_DEFAULT,
    }
}

/// A policy denial, explained to the model: naming the rule that blocked it is
/// what stops the model from retrying the same call in a loop, and tells it
/// whether to look for another route or give up and report the block.
fn policy_denial(decision: komo_core::domain::policy::Decision) -> Decision {
    match decision.rule {
        // The index is the one `komo policy list` prints, so the operator can
        // find the exact line if the user asks why.
        Some(i) => Decision::deny_because(format!(
            "被权限策略拒绝（命中规则 #{i}，见 `komo policy list`）。\
             这是 operator 在 config.toml 里设定的，重试同样的调用不会成功。"
        )),
        None => Decision::deny_because(
            "被权限策略的默认规则拒绝。重试同样的调用不会成功；\
             需要 operator 在 config.toml 的 [policy] 里放行。",
        ),
    }
}

#[cfg(test)]
mod tests;
