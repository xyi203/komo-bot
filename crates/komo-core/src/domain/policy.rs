//! Configurable permission policy (roadmap §3): a pure rule engine that decides
//! whether a side-effecting action is auto-allowed, hard-denied, or escalated to
//! interactive approval — consulted *before* the interactive [`Approver`].
//!
//! Pure: no I/O and no config parsing. `config.rs` parses the `[policy]` table
//! from config.toml into these types (via the `parse` helpers here), and
//! `agent::policy_approver::PolicyApprover` wraps the interactive approver and
//! consults a [`Policy`] on every non-`Safe` request.
//!
//! Layering: the policy sits *above* each tool's own hardline floor (shell's
//! refused patterns, HA's blocked domains). Those short-circuit inside the tool
//! before any approver is consulted, so no policy `Allow` rule can unlock them —
//! the policy can only make the gate stricter than a tool's floor, never looser.
//!
//! [`Approver`]: crate::domain::approval::Approver

use crate::domain::approval::{ActionRef, ApprovalRequest, Risk};

/// The class of action a rule applies to (mirrors [`ActionRef`]'s variants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Shell,
    File,
    Network,
    HomeAssistant,
    /// Tool calls on external MCP servers, targeted as `server.tool`.
    Mcp,
    /// Note-vault index maintenance (`wiki_index`), targeted by action name.
    Wiki,
    /// Tools a local plugin registered (`~/.komo/plugins/*.py`), targeted by
    /// the plugin's own tool name.
    ///
    /// Its own category rather than folded into [`Mcp`](Self::Mcp): both are
    /// code komo did not write, but a plugin is *local* code the operator (or
    /// the agent, under approval) authored on this machine, while an MCP server
    /// is a remote party. An operator who trusts their own plugins should not
    /// have to trust every remote to say so.
    Plugin,
}

impl Category {
    /// Parse a config string (`shell` / `file` / `network` / `homeassistant` /
    /// `mcp` / `wiki`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "shell" => Some(Self::Shell),
            "file" => Some(Self::File),
            "network" | "net" => Some(Self::Network),
            "homeassistant" | "ha" => Some(Self::HomeAssistant),
            "mcp" => Some(Self::Mcp),
            "wiki" => Some(Self::Wiki),
            "plugin" | "plugins" => Some(Self::Plugin),
            _ => None,
        }
    }
}

/// How a rule's `value` is compared against the action's target string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matcher {
    Prefix,
    Suffix,
    Exact,
    Contains,
    /// Every target in the rule's category, whatever its value — what a rule
    /// with no `match`/`value` means. This is the only matcher a tool can be
    /// dropped from the catalog for (see [`Policy::wholly_denied`]): any of the
    /// others leaves *some* action in the category permitted, so the tool has to
    /// stay advertised.
    Any,
}

impl Matcher {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "prefix" => Some(Self::Prefix),
            "suffix" => Some(Self::Suffix),
            "exact" => Some(Self::Exact),
            "contains" => Some(Self::Contains),
            "any" | "*" | "all" => Some(Self::Any),
            _ => None,
        }
    }

    fn matches(&self, value: &str, target: &str) -> bool {
        match self {
            Matcher::Prefix => target.starts_with(value),
            Matcher::Suffix => target.ends_with(value),
            Matcher::Exact => target == value,
            Matcher::Contains => target.contains(value),
            Matcher::Any => true,
        }
    }
}

/// Filesystem access kind a `file` rule scopes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

impl Access {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            _ => None,
        }
    }
}

/// What a matching rule does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Deny,
}

impl Effect {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// The policy's decision for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Auto-allow without prompting.
    Allow,
    /// Hard-deny without prompting.
    Deny,
    /// Escalate to the interactive approver (the current behavior).
    Ask,
}

impl Verdict {
    /// Parse the `default_normal` config value (`ask` / `deny` / `allow`).
    pub fn parse_default(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "ask" => Some(Self::Ask),
            "deny" => Some(Self::Deny),
            "allow" => Some(Self::Allow),
            _ => None,
        }
    }
}

/// How a [`Verdict::Ask`] is handled — the `[policy] mode` setting.
///
/// Deliberately *not* a field on [`Policy`]: the engine is a pure rule
/// evaluator, and this changes nothing about which verdict a request gets. It
/// decides who answers an `Ask` — the human alone, or an aux-model reviewer
/// first (`agent::auto_reviewer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PolicyMode {
    /// Every `Ask` goes straight to the human. The default, and what komo did
    /// before the mode existed.
    #[default]
    Ask,
    /// An `Ask` is reviewed by the aux model first, which may auto-allow it or
    /// hand it to the human. It can never deny — refusal stays the operator's.
    Auto,
}

impl PolicyMode {
    /// Parse the `[policy] mode` value (`ask` / `auto`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "ask" => Some(Self::Ask),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// One policy rule. Built by `config.rs` from a `[[policy.rule]]` table.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Channel scope (`feishu` / `telegram` / `cli` / …); `None` = all channels.
    pub channels: Option<Vec<String>>,
    pub category: Category,
    pub matcher: Matcher,
    pub value: String,
    /// `file`-only: restrict to reads or writes; `None` = either.
    pub access: Option<Access>,
    pub effect: Effect,
    /// Allow rules don't grant `Risk::Dangerous` actions unless this is set.
    pub include_dangerous: bool,
    /// Allow rules apply only to an attended turn unless this is set: an
    /// `unattended = true` allow also grants where nobody is watching (the
    /// cron sweep's agent turns). Deny rules ignore this — they are
    /// unconditional everywhere. The narrow channel of roadmap §3.
    ///
    /// Those turns reach the engine with `channel = None`; what makes them
    /// channel-less is `SessionOrigin`, not a missing session — see
    /// `agent::policy_approver`.
    pub unattended: bool,
}

impl Rule {
    /// The **narrowest** allow rule that would cover `action` again — what an
    /// "always allow" answer saves. `None` when the request carries no
    /// [`ActionRef`] to generalize from, in which case there is nothing to
    /// remember and the prompt must not offer to.
    ///
    /// Narrow by construction, because the operator is answering one prompt and
    /// should not be granting a category:
    ///
    /// - `shell` → the command's **first token** (`cargo build` → prefix
    ///   `cargo `), not the whole command line (which would never match twice)
    ///   and not the category;
    /// - `file` → the target's **parent directory** as a prefix, plus the
    ///   read/write access kind, so approving a write under `src/` doesn't also
    ///   grant reads elsewhere;
    /// - `network` → the **host** as a dot-boundary suffix;
    /// - `homeassistant` → the exact `domain.service`;
    /// - `wiki` → the exact action name (`refresh` never implies `rebuild`).
    ///
    /// Always scoped to `channel`: an approval given at the CLI must not silently
    /// grant the same action to a chat channel where someone else is typing.
    pub fn narrowest_for(action: &ActionRef, channel: &str) -> Option<Rule> {
        let (category, matcher, value, access) = match action {
            ActionRef::Shell { command } => {
                let first = command.split_whitespace().next()?.to_string();
                // Keep the trailing space when the command had arguments: it is
                // strictly narrower (`cargo ` can't match `cargonaut`).
                let value = if command.trim() == first {
                    first
                } else {
                    format!("{first} ")
                };
                (Category::Shell, Matcher::Prefix, value, None)
            }
            ActionRef::File { path, write } => {
                let dir = path.parent().filter(|p| !p.as_os_str().is_empty())?;
                let access = Some(if *write { Access::Write } else { Access::Read });
                (
                    Category::File,
                    Matcher::Prefix,
                    // Trailing separator so `/src` can't also cover `/src-old`.
                    format!("{}/", dir.display().to_string().trim_end_matches('/')),
                    access,
                )
            }
            ActionRef::Network { url } => {
                let host = host_of(url);
                if host.is_empty() {
                    return None;
                }
                (Category::Network, Matcher::Suffix, host, None)
            }
            ActionRef::Service { domain, service } => (
                Category::HomeAssistant,
                Matcher::Exact,
                format!("{domain}.{service}"),
                None,
            ),
            ActionRef::Mcp { server, tool } => (
                Category::Mcp,
                Matcher::Exact,
                format!("{server}.{tool}"),
                None,
            ),
            ActionRef::Wiki { action } => (Category::Wiki, Matcher::Exact, action.clone(), None),
            ActionRef::Plugin { tool } => (Category::Plugin, Matcher::Exact, tool.clone(), None),
        };
        Some(Rule {
            channels: Some(vec![channel.to_string()]),
            category,
            matcher,
            value,
            access,
            effect: Effect::Allow,
            // Both deliberately off for a saved entry: the engine refuses to read
            // it for a dangerous or unattended action anyway, and storing `true`
            // here would misrepresent what it can do.
            include_dangerous: false,
            unattended: false,
        })
    }

    /// One line describing this rule the way config would write it — shown at the
    /// approval prompt (so the operator sees exactly how wide the grant is before
    /// answering) and by `komo policy list` / `saved list`.
    pub fn describe(&self) -> String {
        let mut parts = vec![
            match self.effect {
                Effect::Allow => "allow".to_string(),
                Effect::Deny => "deny ".to_string(),
            },
            format!("{:<14}", category_str(self.category)),
            match self.matcher {
                // A wildcard rule has no value to show — printing `any ""` would
                // read like an empty pattern rather than "the whole category".
                Matcher::Any => "any (whole category)".to_string(),
                other => format!("{} \"{}\"", matcher_str(other), self.value),
            },
        ];
        if let Some(a) = self.access {
            parts.push(format!(
                "access={}",
                match a {
                    Access::Read => "read",
                    Access::Write => "write",
                }
            ));
        }
        if let Some(c) = &self.channels {
            parts.push(format!("channels={}", c.join(",")));
        }
        if self.include_dangerous {
            parts.push("include_dangerous".to_string());
        }
        if self.unattended {
            parts.push("unattended".to_string());
        }
        parts.join("  ")
    }

    /// Whether this rule is in scope for `action` on `channel` (ignores `value`).
    fn applies(&self, action: &ActionRef, channel: Option<&str>) -> bool {
        if self.category != category_of(action) {
            return false;
        }
        if let Some(allowed) = &self.channels {
            match channel {
                Some(c) if allowed.iter().any(|x| x == c) => {}
                _ => return false,
            }
        }
        // File access scope: a `write` rule applies only to writes, etc.
        if let (Some(want), ActionRef::File { write, .. }) = (self.access, action)
            && (want == Access::Write) != *write
        {
            return false;
        }
        true
    }

    /// Whether this rule's `value`/`matcher` matches the action's target.
    fn matches(&self, action: &ActionRef) -> bool {
        match action {
            // Network matches on the host, with dotted-boundary suffix matching
            // so `suffix github.com` does not also match `evilgithub.com`.
            ActionRef::Network { url } => {
                let host = host_of(url);
                match self.matcher {
                    Matcher::Suffix => {
                        let want = self.value.trim_start_matches('.');
                        host == want || host.ends_with(&format!(".{want}"))
                    }
                    other => other.matches(&self.value, &host),
                }
            }
            ActionRef::Shell { command } => self.matcher.matches(&self.value, command),
            ActionRef::File { path, .. } => {
                self.matcher.matches(&self.value, &path.to_string_lossy())
            }
            ActionRef::Service { domain, service } => self
                .matcher
                .matches(&self.value, &format!("{domain}.{service}")),
            ActionRef::Mcp { server, tool } => self
                .matcher
                .matches(&self.value, &format!("{server}.{tool}")),
            ActionRef::Wiki { action } => self.matcher.matches(&self.value, action),
            ActionRef::Plugin { tool } => self.matcher.matches(&self.value, tool),
        }
    }
}

/// The serde form of a [`Rule`], field-for-field with a `[[policy.rule]]` table.
///
/// A [`Rule`] holds parsed enums, so it cannot round-trip through JSON on its
/// own; this is the wire/at-rest shape for the stores that persist rules —
/// today a cron job's grants (`cron.db`). Field names deliberately match the
/// config table, so what an operator reads in a store is what they would write
/// in `config.toml`.
///
/// `permissions.json` keeps its own narrower `Entry` shape (single `channel`,
/// plus `created_at` / `source` provenance): a saved grant is always
/// single-channel and carries metadata a rule does not. That is a *narrowing*
/// of this shape, not a competing one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuleSpec {
    pub category: String,
    #[serde(rename = "match")]
    pub matcher: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<String>>,
    pub effect: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_dangerous: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unattended: bool,
}

impl RuleSpec {
    /// Parse into a [`Rule`]. `None` when a field does not name a known variant —
    /// the caller decides whether that is "skip this entry" (a store reading
    /// something an older/newer komo wrote) or an error.
    pub fn to_rule(&self) -> Option<Rule> {
        Some(Rule {
            channels: self.channels.clone().filter(|c| !c.is_empty()),
            category: Category::parse(&self.category)?,
            matcher: Matcher::parse(&self.matcher)?,
            value: self.value.clone(),
            access: match &self.access {
                Some(a) => Some(Access::parse(a)?),
                None => None,
            },
            effect: Effect::parse(&self.effect)?,
            include_dangerous: self.include_dangerous,
            unattended: self.unattended,
        })
    }

    /// The serde form of `rule` — the inverse of [`to_rule`](Self::to_rule).
    pub fn from_rule(rule: &Rule) -> Self {
        Self {
            category: category_str(rule.category).to_string(),
            matcher: matcher_str(rule.matcher).to_string(),
            value: rule.value.clone(),
            access: rule.access.map(|a| {
                match a {
                    Access::Read => "read",
                    Access::Write => "write",
                }
                .to_string()
            }),
            channels: rule.channels.clone(),
            effect: match rule.effect {
                Effect::Allow => "allow",
                Effect::Deny => "deny",
            }
            .to_string(),
            include_dangerous: rule.include_dangerous,
            unattended: rule.unattended,
        }
    }
}

/// Which rule list [`Decision::rule`] indexes into. The three lists are numbered
/// independently, so the index alone is ambiguous without this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSource {
    /// The `[[policy.rule]]` list as configured — `komo policy list` shows the
    /// same numbering, so a `check` result points at a real line.
    Config,
    /// The runtime-accumulated allow list (`komo policy saved list`).
    Saved,
    /// The running job's own grants, approved when the job was created.
    JobGrant,
}

/// A verdict plus which rule produced it (`None` = fell through to a default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    pub rule: Option<usize>,
    /// Which list [`rule`](Self::rule) indexes. Only an `Allow` ever comes from
    /// `Saved` or `JobGrant` — both hold allows only.
    pub source: RuleSource,
}

impl Decision {
    fn fallback(verdict: Verdict) -> Self {
        Self {
            verdict,
            rule: None,
            source: RuleSource::Config,
        }
    }

    fn from_config(verdict: Verdict, rule: usize) -> Self {
        Self {
            verdict,
            rule: Some(rule),
            source: RuleSource::Config,
        }
    }
}

/// Allow rules accumulated at runtime — the operator answering "always" at an
/// approval prompt (`~/.komo/permissions.json`, persisted by
/// `infra::permissions_store`).
///
/// Shared rather than copied on purpose: the store and every [`Policy`] clone
/// hold the same list, so an entry saved at a prompt applies to the *next*
/// decision without a restart. Held here as plain data — the domain never does
/// the file I/O.
pub type SavedRules = std::sync::Arc<std::sync::RwLock<Vec<Rule>>>;

/// A resolved permission policy: an ordered rule list plus the fallback verdict
/// for a `Risk::Normal` action that no rule matches, and optionally the saved
/// allow list accumulated at runtime.
#[derive(Debug, Clone)]
pub struct Policy {
    rules: Vec<Rule>,
    default_normal: Verdict,
    saved: Option<SavedRules>,
}

impl Policy {
    pub fn new(rules: Vec<Rule>, default_normal: Verdict) -> Self {
        Self {
            rules,
            default_normal,
            saved: None,
        }
    }

    /// Attach the runtime-accumulated allow list. Without this a policy behaves
    /// exactly as before — saved approvals are opt-in wiring, and every
    /// unattended construction deliberately leaves them out.
    pub fn with_saved(mut self, saved: SavedRules) -> Self {
        self.saved = Some(saved);
        self
    }

    /// The configured rules, in evaluation-list order (for `komo policy list`).
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// A snapshot of the saved allow entries, in the order they are evaluated
    /// (for `komo policy saved list` and `check`'s explanation).
    pub fn saved_rules(&self) -> Vec<Rule> {
        self.saved
            .as_ref()
            .map(|s| s.read().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_default()
    }

    /// The fallback verdict for an unmatched `Risk::Normal` action.
    pub fn default_normal(&self) -> Verdict {
        self.default_normal
    }

    /// Evaluate `request` for a turn on `channel` (`None` for an **unattended**
    /// turn — a sweep with no session at all, or one whose `SessionOrigin` says
    /// nobody is watching), reporting which rule matched — also the dry-run
    /// surface behind `komo policy check`.
    ///
    /// Deny rules take precedence over allow rules regardless of order; with no
    /// rule matching, `Risk::Normal` falls to `default_normal` and
    /// `Risk::Dangerous` always falls to [`Verdict::Ask`] (only an explicit
    /// `include_dangerous` allow rule grants a dangerous action).
    ///
    /// `Risk::Safe` gets **deny-only** evaluation: deny rules can block a
    /// read-only action (network fetch, file read), but nothing ever escalates
    /// it to a prompt — an unmatched safe action stays allowed. Allow rules are
    /// meaningless for safe actions and are skipped.
    ///
    /// **Unattended contexts** (`channel = None`) only grant through an allow
    /// rule explicitly marked `unattended`; a `default_normal = allow` fallback
    /// degrades to [`Verdict::Ask`] there — an unattended grant is always an
    /// explicit opt-in, never a default.
    pub fn decide(&self, request: &ApprovalRequest, channel: Option<&str>) -> Decision {
        self.decide_with_grants(request, channel, &[])
    }

    /// [`decide`](Self::decide), plus the running job's own `grants` — the
    /// actions a human approved for *this* scheduled job when it was created.
    ///
    /// They sit between config deny and saved grants in the ladder:
    ///
    /// ```text
    /// tool hardline floor > config deny > job grant > saved grant > config allow / default > ask
    /// ```
    ///
    /// - **below config deny**, because a grant given at job-creation time must
    ///   not overrule what the operator wrote in config.toml — same reason a
    ///   saved grant doesn't;
    /// - **above saved grants**, because a job grant was approved against a
    ///   named, visible list, while a saved grant was *generalized* from one
    ///   answer;
    /// - **never `Risk::Dangerous`**, same rule as a saved grant:
    ///   `include_dangerous` stays a config-only opt-in.
    ///
    /// Unlike a saved grant, a job grant *is* read unattended — that is its
    /// entire purpose. Its scope is the job, which is narrower than the
    /// `unattended = true` config rule it replaces, and it dies with the job.
    pub fn decide_with_grants(
        &self,
        request: &ApprovalRequest,
        channel: Option<&str>,
        grants: &[Rule],
    ) -> Decision {
        let Some(action) = request.action.as_ref() else {
            // No structured resource to match on; risk-only fallback.
            return Decision::fallback(self.default_for(request.risk, channel));
        };

        for (i, rule) in self.rules.iter().enumerate() {
            if rule.effect == Effect::Deny && rule.applies(action, channel) && rule.matches(action)
            {
                return Decision::from_config(Verdict::Deny, i);
            }
        }
        if request.risk == Risk::Safe {
            // Deny-only for read-only actions: no allow rules, no escalation.
            return Decision::fallback(Verdict::Allow);
        }
        if request.risk != Risk::Dangerous {
            for (i, rule) in grants.iter().enumerate() {
                // `applies` is called channel-lessly on purpose: a job grant is
                // scoped by the job, not by a channel, and an unattended turn
                // has none to match against anyway.
                if rule.effect == Effect::Allow
                    && rule.applies(action, None)
                    && rule.matches(action)
                {
                    return Decision {
                        verdict: Verdict::Allow,
                        rule: Some(i),
                        source: RuleSource::JobGrant,
                    };
                }
            }
        }
        // Saved allows sit *below* every config deny (checked above) and *above*
        // config allows, so a remembered approval can shortcut a prompt but can
        // never widen past what the operator wrote in config.toml. Two further
        // floors, both enforced here rather than at the call site:
        //
        //   - a saved entry never grants a `Risk::Dangerous` action — "remember
        //     this" must not turn a dangerous action into a silent one, and
        //     `include_dangerous` is a config-only opt-in;
        //   - a saved entry is not read in an **unattended** context (no
        //     channel). It was accumulated interactively; letting it leak into
        //     cron / sweeps would grant there what only `unattended = true`
        //     config rules are allowed to grant.
        if request.risk != Risk::Dangerous && channel.is_some() {
            for (i, rule) in self.saved_rules().iter().enumerate() {
                if rule.applies(action, channel) && rule.matches(action) {
                    return Decision {
                        verdict: Verdict::Allow,
                        rule: Some(i),
                        source: RuleSource::Saved,
                    };
                }
            }
        }
        for (i, rule) in self.rules.iter().enumerate() {
            if rule.effect == Effect::Allow && rule.applies(action, channel) && rule.matches(action)
            {
                if request.risk == Risk::Dangerous && !rule.include_dangerous {
                    continue;
                }
                // No session in scope: only explicitly-unattended allows grant.
                if channel.is_none() && !rule.unattended {
                    continue;
                }
                return Decision::from_config(Verdict::Allow, i);
            }
        }
        Decision::fallback(self.default_for(request.risk, channel))
    }

    /// Whether *every* action in `category` is denied — for every channel and
    /// every target. `access` narrows the question to one kind of file action
    /// (`read` for `read`/`grep`/`glob`, `write` for `write`/`edit`); pass `None`
    /// for the categories that have no access dimension.
    ///
    /// This is the catalog-filtering question (opencode v2's `whollyDisabled`):
    /// a tool that can never do anything is worth dropping from the model's
    /// schema and prompt entirely, rather than burning a round-trip on a call
    /// that is certain to be refused.
    ///
    /// Deliberately conservative — only an unscoped [`Matcher::Any`] deny counts:
    ///
    /// - a **channel-scoped** rule leaves the tool usable elsewhere,
    /// - a **value-scoped** rule (`prefix`, `contains`, …) leaves some target
    ///   permitted.
    ///
    /// Both keep the tool advertised and refuse the individual call instead.
    /// Dropping a tool by mistake hides a capability the model can never learn
    /// it had; keeping one costs a refusal the model is actually told about.
    pub fn wholly_denied(&self, category: Category, access: Option<Access>) -> bool {
        self.rules.iter().any(|rule| {
            rule.effect == Effect::Deny
                && rule.category == category
                && rule.channels.is_none()
                && rule.matcher == Matcher::Any
                && match (rule.access, access) {
                    // Unscoped by access ⇒ covers reads and writes alike.
                    (None, _) => true,
                    // The rule only bans one kind; a tool whose kind we can't
                    // state stays.
                    (Some(_), None) => false,
                    (Some(banned), Some(want)) => banned == want,
                }
        })
    }

    fn default_for(&self, risk: Risk, channel: Option<&str>) -> Verdict {
        match risk {
            Risk::Safe => Verdict::Allow,
            // A default can never grant unattended (channel = None) — only an
            // explicit `unattended` rule does; degrade a would-be Allow to Ask.
            Risk::Normal if channel.is_none() && self.default_normal == Verdict::Allow => {
                Verdict::Ask
            }
            Risk::Normal => self.default_normal,
            Risk::Dangerous => Verdict::Ask,
        }
    }
}

impl Default for Policy {
    /// The empty policy: no rules, `Normal` actions ask. Identical behavior to
    /// having no policy at all — i.e. the current interactive-only flow.
    fn default() -> Self {
        Self::new(Vec::new(), Verdict::Ask)
    }
}

/// The channel name a turn with no correspondent is evaluated against: every
/// local surface — TUI, desktop, web, CLI — is one operator at one machine, so
/// they share one name rather than each inventing its own.
///
/// A turn's channel comes from
/// [`SessionContext::channel_name`](crate::domain::context::SessionContext::channel_name);
/// this is what that answers when the turn has no
/// [`ChannelPeer`](crate::domain::session::ChannelPeer). It used to be derived
/// by splitting the session id on a colon, which meant a client free to name
/// its own session was also free to name its own channel.
pub const LOCAL_CHANNEL: &str = "cli";

pub fn category_str(c: Category) -> &'static str {
    match c {
        Category::Shell => "shell",
        Category::File => "file",
        Category::Network => "network",
        Category::HomeAssistant => "homeassistant",
        Category::Mcp => "mcp",
        Category::Wiki => "wiki",
        Category::Plugin => "plugin",
    }
}

pub fn matcher_str(m: Matcher) -> &'static str {
    match m {
        Matcher::Prefix => "prefix",
        Matcher::Suffix => "suffix",
        Matcher::Exact => "exact",
        Matcher::Contains => "contains",
        Matcher::Any => "any",
    }
}

fn category_of(action: &ActionRef) -> Category {
    match action {
        ActionRef::Shell { .. } => Category::Shell,
        ActionRef::File { .. } => Category::File,
        ActionRef::Network { .. } => Category::Network,
        ActionRef::Service { .. } => Category::HomeAssistant,
        ActionRef::Mcp { .. } => Category::Mcp,
        ActionRef::Wiki { .. } => Category::Wiki,
        ActionRef::Plugin { .. } => Category::Plugin,
    }
}

/// Extract the lowercase host from a URL, dependency-free: strip the scheme, cut
/// at the first `/`, `:`, `?`, or `#`.
fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    after_scheme
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase()
}

#[cfg(test)]
mod tests;
