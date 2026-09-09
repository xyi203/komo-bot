//! Construction of a fully-wired `AgentRuntime`.
//!
//! The gateway is the only caller: it is the only process that opens komo's
//! state, so it is the only process that has an agent. `komo chat` and the CLI
//! talk to it over the api channel.
//!
//! Every tool komo mounts is built here, once, and registered into one
//! executor per runtime — so a runtime's tool set cannot drift from the list
//! this module holds.

use komo_bot::compaction::Compactor;
use komo_bot::delegate::DelegateTool;
use komo_bot::interaction::{ApprovalState, ChatApprover};
use komo_bot::learning_coordinator::LearningCoordinator;
use komo_bot::llm::{PreambleFn, TurnInjections, build_llm};
use komo_bot::reviewer::ReflectiveReviewer;
use komo_bot::runtime::AgentRuntime;
use komo_bot::system_prompt::SystemPromptBuilder;
use komo_bot::unattended::UnattendedSuspend;
use komo_core::domain::embedding::EmbeddingClient;
use komo_core::domain::skill::SkillOffer;
use komo_infra::embedding::{GatedEmbedder, OllamaEmbedder};
use komo_infra::permissions_store::PermissionsStore;
use komo_infra::persistence::db::Db;
use komo_infra::skills::FsSkillStore;
use komo_services::artifact_store::ArtifactStore;
use komo_services::memory_enrichment::MemoryEnricher;
use komo_services::skill_registry::SkillRegistry;
use komo_services::tool_execution::{ToolExecutionConfig, ToolExecutor};
use komo_services::tool_output_store::ToolOutputStore;
use std::sync::Arc;

use komo_config::ConfigSnapshot;
use komo_core::domain::catalog::ToolCatalog;
use komo_core::domain::tool::Tool;
use komo_core::domain::{
    approval::Approver, cron::CronJobRepository, llm::LlmClient, memory::MemoryRepository,
    reviewer::Reviewer, workspace::Workspace,
};
use komo_tools::apply_patch::ApplyPatchTool;
use komo_tools::ask_user::AskUserTool;
use komo_tools::cron::CronTool;
use komo_tools::edit::EditTool;
use komo_tools::glob::GlobTool;
use komo_tools::grep::GrepTool;
use komo_tools::homeassistant::HomeAssistantTool;
use komo_tools::logs::LogsTool;
use komo_tools::memory::MemoryTool;
use komo_tools::read::ReadTool;
use komo_tools::session::SessionTool;
use komo_tools::shell::ShellTool;
use komo_tools::skill::SkillTool;
use komo_tools::time::TimeTool;
use komo_tools::todo::TodoTool;
use komo_tools::web_fetch::WebFetchTool;
use komo_tools::web_search::WebSearchTool;
use komo_tools::write::WriteTool;

/// Which of the three tool-wielding runtimes an executor is for: it picks that
/// runtime's tool catalog, and `Main` is the only one whose calls are recorded
/// in a transcript.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Runtime {
    /// The user-facing conversation.
    Main,
    /// The `delegate` tool's sub-agent.
    Subagent,
    /// The unattended routine runtime.
    Cron,
}

/// One tool catalog per runtime.
///
/// Separate rather than shared because the runtimes deliberately differ in what
/// they mount — `delegate` is the conversation's alone — and because a catalog
/// is what the plugin host mounts into, so each runtime's view of the plugins
/// dies with its own executor.
struct Catalogs([Arc<ToolCatalog>; 3]);

impl Catalogs {
    fn new() -> Self {
        Self(std::array::from_fn(|_| Arc::new(ToolCatalog::new())))
    }

    fn of(&self, runtime: Runtime) -> &Arc<ToolCatalog> {
        match runtime {
            Runtime::Main => &self.0[0],
            Runtime::Subagent => &self.0[1],
            Runtime::Cron => &self.0[2],
        }
    }
}

/// A wired agent plus the handles background work needs (sessions for sweeping,
/// the reviewer the sweep invokes).
pub struct Wiring {
    pub runtime: AgentRuntime,
    /// The shared review coordinator (post-turn + scheduled), for the
    /// gateway's `ReviewSweep`.
    pub review: Arc<LearningCoordinator>,
    /// The markdown memory store.
    pub memories: Arc<dyn MemoryRepository>,
    /// The hybrid query service, so the operator surface can drive an embedding
    /// backfill through the same one recall uses.
    pub memory_query: Arc<komo_services::memory_query::MemoryQueryService>,
    /// The cron sweep's agent for `CronAction::Agent` jobs: the full tool set
    /// with unattended policy gating. Main model, no memory enricher.
    pub cron_runtime: Arc<AgentRuntime>,
    /// Where over-limit tool results are stored in full. Exposed so the gateway
    /// can run the retention sweep once at startup — the store re-sweeps at most
    /// hourly on its own, and this is deliberately not a cron schedule: expiring
    /// a scratch file does not need to happen on the minute.
    pub output_store: Arc<ToolOutputStore>,
    /// Note-vault handles, shared with the operator surface so `komo wiki` works
    /// while the gateway holds the index open.
    pub wiki: Option<crate::services::operator_control::actions::WikiOps>,
    /// The pending-approval registry the chat approver writes into, shared with
    /// the gateway's dispatcher and api channel so `/approve` — typed in a chat,
    /// clicked in the desktop app, or pressed in the TUI's modal — resolves the
    /// wait the turn is parked on.
    pub approvals: Arc<ApprovalState>,
}

/// What distinguishes one runtime from another.
///
/// Four runtimes exist — interactive, delegate, cron, and the reviewer's aux
/// calls — and they were near-identical struct literals whose *differences*
/// were three fields buried among nine identical ones.
/// Everything shared (the stores, the history window, the learning
/// coordinator) is supplied by [`RuntimeParts`]; a profile states only what is
/// its own.
///
struct CapabilityProfile {
    llm: Arc<dyn LlmClient>,
    tools: ToolExecutor,
    max_turns: usize,
    /// Learns from its own finished turns. **False for every aux runtime**: a
    /// sub-agent's transcript is scratch work, and a sweep restates what komo
    /// already knows, so extracting from either feeds the memory pipeline its
    /// own output. (`LearningCoordinator` also refuses sweep sessions by id —
    /// this is the half that stops them being offered at all.)
    learns: bool,
    /// Summarises its oldest messages once the window starts dropping them.
    /// True only for conversations, which are the only sessions that outlive
    /// their window: a sweep or a delegation opens, answers and is done.
    compacts: bool,
}

/// What every runtime shares. Held once so [`CapabilityProfile`] can be read as
/// a list of differences.
struct RuntimeParts {
    db: Arc<Db>,
    /// Shared by every runtime that compacts — the aux model summarising, over
    /// the same window the history read uses.
    compactor: Arc<Compactor>,
    /// Mirrors the LLM's own history window, so a turn loads exactly what the
    /// model will replay and no long transcript is read in full.
    history_window: usize,
    learning: Arc<LearningCoordinator>,
}

impl RuntimeParts {
    fn build(&self, profile: CapabilityProfile) -> AgentRuntime {
        AgentRuntime {
            llm: profile.llm,
            sessions: self.db.clone(),
            messages: self.db.clone(),
            events: self.db.clone(),
            // Every runtime shares the run ledger, which is what makes a
            // delegation and a cron job each auditable through `komo run list`
            // alongside ordinary turns.
            runs: self.db.clone(),
            projection: self.db.clone(),
            tool_executor: profile.tools,
            max_turns: profile.max_turns,
            history_window: self.history_window,
            learning: profile.learns.then(|| self.learning.clone()),
            compaction: profile.compacts.then(|| self.compactor.clone()),
            // Every runtime that can be gated can suspend, and a suspended
            // turn's wait has to outlive the process — so all of them get the
            // store, conversations and routines alike.
            wakeups: Some(self.db.clone()),
        }
    }
}

/// A warning, never a fatal — the same call komo makes for a missing model key
/// or a token-less HA channel. Recall keeps working without it.
///
/// The probe runs in the background, not here: awaiting it held the first TUI
/// frame hostage to Ollama's cold model load (seconds), and its verdict is a
/// diagnostic plus a kill switch, never a precondition — every embed caller
/// already degrades to lexical on failure. A failed probe closes the
/// [`GatedEmbedder`]'s gate so later calls fail instantly instead of re-paying
/// a network timeout per turn; a successful one doubles as a model warm-up, so
/// the first recall of the session usually hits a resident model.
pub(crate) fn build_embedder(
    config: Option<&komo_config::EmbeddingConfig>,
) -> Option<Arc<dyn EmbeddingClient>> {
    let config = config?;
    let embedder = match OllamaEmbedder::new(&config.url, &config.model) {
        Ok(embedder) => embedder,
        Err(error) => {
            tracing::warn!(%error, "memory embedding backend unusable — recall stays lexical");
            return None;
        }
    };
    let gated = Arc::new(GatedEmbedder::new(embedder));
    let probe = gated.clone();
    let (url, model) = (config.url.clone(), config.model.clone());
    tokio::spawn(async move {
        match probe.probe().await {
            Ok(()) => tracing::info!(model = %model, "memory embedding backend ready"),
            Err(error) => tracing::warn!(
                %error,
                url = %url,
                model = %model,
                "memory embedding backend unreachable — recall stays lexical"
            ),
        }
    });
    Some(gated)
}

/// Build the agent against `db` — sessions, tasks, memories, cron jobs, the
/// ledger, all of it (docs/adr/0004). Every setting comes from the caller's one
/// resolved `config` snapshot — wiring never re-reads config.toml, the env, or
/// `.env`.
///
/// The store is passed in rather than opened here because Turso takes an
/// exclusive lock per file: the gateway already holds it open and must hand its
/// own handle over.
pub async fn build(config: &ConfigSnapshot, db: Arc<Db>) -> anyhow::Result<Wiring> {
    // Tool actions that need approval are gated on the conversation: the agent
    // sends a prompt and the turn suspends until `/approve` (or the desktop
    // app's modal, or the TUI's) answers it. The registry is returned so the
    // dispatcher and the api channel share this one.
    let approvals = Arc::new(ApprovalState::new());
    let approver: Arc<dyn Approver> = Arc::new(ChatApprover::new(approvals.clone()));
    // One file, one handle, several repository traits over it. Named separately
    // because the things they mean are still separate — durable jobs, durable
    // memories — even though they are now tables in the same database.
    let cron_jobs: Arc<dyn CronJobRepository> = db.clone();
    // An unusable model selection (bad KOMO_* value, unknown provider,
    // missing API key) can't produce a working agent — fail here like the old
    // strict resolver did.
    config.validate_agent()?;
    let model_config = &config.runtime.model;

    // Approvals the operator chose to make durable (`a` at the prompt →
    // ~/.komo/permissions.json). The store's list is *shared* with the policy, so
    // a grant applies to the next decision without a restart.
    let permissions = Arc::new(PermissionsStore::load(&config.runtime.home));
    let interactive_policy = config
        .runtime
        .policy
        .policy
        .clone()
        .with_saved(permissions.rules());

    // Over-limit tool output is kept in full under ~/.komo/tool-output; the model
    // gets a head+tail preview naming the file (roadmap item 10).
    let output_store = Arc::new(ToolOutputStore::new(
        config.runtime.home.join("tool-output"),
    ));

    // Where a turn puts what it *made*, as opposed to what it changed
    // (docs/bot-runtime.md §5.16). A writable root outside the workspace, one
    // subdirectory per session, created on first write and never swept.
    let artifacts = Arc::new(ArtifactStore::new(config.runtime.home.join("artifacts")));

    // Mutations and shell workdirs remain confined to the current working
    // directory plus komo's own two writable roots. Local files are readable
    // from any directory (subject to the file-read permission policy); every
    // managed root is retained for session-derived workspaces as well.
    let mut readonly_roots = config.runtime.readable_roots.clone();
    readonly_roots.push(output_store.root().to_path_buf());
    let workspace = Arc::new(
        Workspace::current_dir()?
            .with_readonly(readonly_roots)
            .with_artifacts(artifacts.root().to_path_buf())
            // The other writable root, and the only one whose contents komo
            // itself runs: a `.py` file here becomes a tool. Writes to it are
            // `Risk::Dangerous` (see `fs_common::write_request`), so authoring
            // a plugin is a thing the operator approves, one file at a time.
            .with_plugins(config.runtime.home.join("plugins"))
            .with_unrestricted_reads(),
    );

    // ── Shared dependencies (built once, used by every tool set) ─────────────
    // Memories are `memory_records` in `komo.db`, shared by the `memory` tool,
    // the reflective reviewer and L3 recall.
    let memory_repo: Arc<dyn MemoryRepository> = db.clone();

    // The delegate tool runs a separate, tool-less sub-agent on the (optionally
    // cheaper) aux model. It gets a minimal identity-only preamble — no tools,
    // skills, or project context — rebuilt per turn like the main agent.
    let aux_config = model_config.aux_variant();
    let aux_builder = Arc::new(SystemPromptBuilder::new(&aux_config));
    let aux_preamble: PreambleFn = Arc::new(move |roots| aux_builder.build(roots));
    // Aux/delegate sub-agents must not be fed the user's memory library — and
    // the aux agent never gets an aux of its own (no recursion).
    let aux_llm = build_llm(
        &aux_config,
        None,
        aux_preamble,
        TurnInjections::default(),
        Some("aux"),
    )?;

    // ── The attended approval chain ──────────────────────────────────────────
    // Built here rather than at the top of `build` because its middle rung needs
    // the aux model above.
    //
    // `[policy] mode = "auto"` inserts a second-opinion reviewer between the
    // policy's `Ask` and the human: it may auto-allow an action the operator's
    // own request plainly covers, or hand it over — it can never deny. In `ask`
    // mode (the default) the decorator is absent, so this is byte-identical to
    // what the chain was before the mode existed.
    //
    // Attended runtimes only. Cron builds its own `PolicyApprover` over an
    // unattended inner further down and deliberately skips this: an unattended
    // turn grants through rules approved in advance, never a live judgement
    // call (ADR 0002 / 0003).
    let approver = match config.runtime.policy.mode {
        komo_core::domain::policy::PolicyMode::Auto => {
            tracing::info!("permission policy: auto mode (aux reviewer may auto-allow prompts)");
            komo_bot::auto_reviewer::AutoReviewApprover::wrap(aux_llm.clone(), db.clone(), approver)
        }
        komo_core::domain::policy::PolicyMode::Ask => approver,
    };

    // Wrap that in the configurable permission policy (roadmap §3): the policy
    // auto-allows / hard-denies per `[policy]` rules and only escalates when it
    // says "ask". With no `[policy]` table this is the empty policy — identical
    // to the bare interactive approver.
    let approver = komo_bot::policy_approver::PolicyApprover::wrap_with_store(
        interactive_policy,
        approver,
        permissions.clone(),
    );

    // The governed skill store: `~/.komo/skills` is the komo-owned home for
    // durable skills (files, not db — roadmap §9), written by a human and
    // installed by a human.
    let skill_store = Arc::new(FsSkillStore::new(FsSkillStore::default_root()));

    // Skills load from, in priority order (first to define a name wins):
    //   KOMO_SKILLS_PATH (colon-separated), <workspace>/skills,
    //   <workspace>/.claude/skills, the governed ~/.komo/skills store, then the
    //   user-global ~/.agents/skills and ~/.claude/skills shared by other agents.
    let root = workspace.roots().first().cloned().unwrap_or_default();
    let skill_dirs = komo_infra::skills::runtime_skill_dirs(
        &config.runtime.skills_path,
        &root,
        skill_store.root(),
        dirs::home_dir().as_deref(),
    );
    let skills = Arc::new(SkillRegistry::load_from_dirs(&skill_dirs));

    // One memory query service, shared by the `memory` tool's explicit search and
    // the enricher's automatic recall — that sharing is the point: a model handed
    // a memory unprompted must be able to find the same memory by asking. Built
    // before the tool set because the tool holds it.
    // Built once and shared: the same backend serves memory recall and the
    // episodic index over transcripts, and its background probe (see
    // `build_embedder`) then covers both.
    let embedder = build_embedder(config.runtime.embedding.as_ref());
    let mut memory_query =
        komo_services::memory_query::MemoryQueryService::new(memory_repo.clone());
    if let Some(embedder) = &embedder {
        memory_query = memory_query.with_embedder(embedder.clone());
    }
    let memory_query = Arc::new(memory_query);

    // Episodic search over komo's own transcripts. Its own collection, beside
    // the wiki's and independent of `[wiki]`: this corpus is komo's, exists
    // whether or not the operator keeps a note vault, and needs only the
    // embedding backend memory recall already asked for.
    //
    // Absent when there is no embedder, or when the store cannot be opened —
    // `session` search then falls back to the substring scan it had before,
    // which is worse but never silent.
    let episodic = match &embedder {
        Some(embedder) => match db.chunk_index(komo_infra::chunk_index::SESSIONS).await {
            Ok(index) => Some(Arc::new(
                komo_services::session_indexing::SessionSearch::new(
                    Arc::new(index),
                    embedder.clone(),
                ),
            )),
            Err(error) => {
                tracing::warn!(%error,
                    "session index unusable — `session` search stays lexical");
                None
            }
        },
        None => None,
    };

    // ── Every tool komo mounts, built once ───────────────────────────────────
    // Built here and shared by the three executors below: an `McpTool` leaks
    // its name and description to satisfy `Tool`'s `&'static str`, so building
    // one per runtime would leak the same strings three times.
    let mut all_tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(TimeTool),
        Arc::new(SkillTool::new(skills.clone(), skill_store.clone())),
        Arc::new(WebFetchTool::new()),
        Arc::new(WebSearchTool::new()),
        Arc::new(ReadTool::new(workspace.clone())),
        Arc::new(WriteTool::new(workspace.clone())),
        Arc::new(EditTool::new(workspace.clone())),
        Arc::new(ApplyPatchTool::new(workspace.clone())),
        Arc::new(GrepTool::new(workspace.clone())),
        Arc::new(GlobTool::new(workspace.clone())),
        Arc::new(ShellTool::new(workspace.clone())),
        // komo's own tracing log, so a failed tool call can be diagnosed from
        // the `tool` span in the same conversation that hit it.
        Arc::new(LogsTool),
        Arc::new({
            let tool = SessionTool::new(db.clone(), db.clone());
            match &episodic {
                Some(search) => tool.with_episodic_search(search.clone()),
                None => tool,
            }
        }),
        // Scheduled jobs from inside a conversation. Every mutation is gated
        // through the executor's approver — a chat-authored job is
        // model-authored, unlike one added with `komo cron add`.
        Arc::new(CronTool::new(cron_jobs.clone())),
        Arc::new(TodoTool::new(db.clone())),
        Arc::new(AskUserTool::new()),
        Arc::new(MemoryTool::new(memory_repo.clone(), memory_query.clone())),
    ];
    // Mounted only when its credentials are configured (`HASS_TOKEN`/`HASS_URL`).
    if let Some(ha) = &config.runtime.homeassistant_tool {
        all_tools.push(Arc::new(HomeAssistantTool::new(
            ha.base_url.clone(),
            ha.token.clone(),
        )));
    }
    // The note vault (`[wiki]`) and external MCP servers are optional
    // integrations: a vault whose index will not open, or a server that is
    // down, costs its own tools and never the boot.
    let mut wiki_ops = None;
    if let Some(wiki) = &config.runtime.wiki {
        let (tools, ops) = wiki_tools(wiki, &db).await;
        all_tools.extend(tools);
        wiki_ops = ops;
    }
    all_tools.extend(mcp_tools(&config.runtime.mcp_servers).await);

    // One catalog per runtime, created before the executors so the python
    // plugin host — which can gain a tool the moment a file is written — has
    // somewhere to mount into while the process runs.
    let catalogs = Catalogs::new();
    // Whether a plugin host can run here at all. Asked now because it decides
    // whether `run_code` is registered below; the host itself is started once
    // the executors exist, since a plugin tool dispatches its own tool calls
    // through the executor of the runtime it was mounted into.
    let plugins_dir = crate::pyhost::available(
        &config.runtime.home,
        config.runtime.pyhost_enabled,
        &config.runtime.policy.policy,
    );
    // The slot `run_code` holds, filled by the supervisor once a host has
    // answered. `None` = no host, which costs `run_code` and the `py__` tools
    // and nothing else.
    let pyhost = plugins_dir.is_some().then(komo_pyhost::SharedHost::default);
    // Where the host's tools go, collected as each runtime's executor is built.
    let mut mounts: Vec<crate::pyhost::PluginMount> = Vec::new();

    // Keep the always-on preamble small: list a bounded catalog, the rest is
    // discoverable on demand via the `skill` tool.
    //
    // Built per runtime rather than once, because the catalog is gated on what
    // *that* runtime offers: a skill restricted to another OS, or one requiring
    // a tool this runtime never registered (config-absent, or dropped by a
    // policy deny), is not worth a prompt line every turn. Offer-time only —
    // `skill` view/list and every `komo skills` command ignore the gating, so a
    // skill left out of the preamble still loads the moment it's named.
    const SKILL_CATALOG_CAP: usize = 30;
    let skills_note_for = |tool_names: &[String]| -> Option<String> {
        let catalog = skills.catalog_capped(
            SKILL_CATALOG_CAP,
            &SkillOffer::here(tool_names.iter().cloned()),
        );
        (!catalog.is_empty()).then(|| {
            format!(
                "You have skills (instruction playbooks) available. To use one, call the \
                 `skill` tool with action=view and the skill name to load its instructions, \
                 then follow them. Available skills:\n{catalog}"
            )
        })
    };

    // One executor per runtime over that one tool list. `delegate` is passed in
    // rather than held in the list because the sub-agent it runs needs an
    // executor of its own — built by this same closure with `delegate: None`,
    // which is the structural guard against recursion.
    let executor_for = |runtime: Runtime,
                        approver: Arc<dyn Approver>,
                        delegate: Option<Arc<DelegateTool>>|
     -> ToolExecutor {
        let mut tools = ToolExecutor::with_catalog(
            catalogs.of(runtime).clone(),
            ToolExecutionConfig::with_result_cap(model_config.max_tool_result_bytes)
                .with_turn_budget(model_config.max_turn_result_bytes)
                .with_call_timeout_secs(model_config.tool_timeout_secs),
        )
        .with_approver(approver)
        .with_output_store(output_store.clone());
        // Only the main runtime records its tool calls in a transcript: every
        // other one runs on a synthetic session (delegate, cron), where a file
        // per one-shot turn is litter rather than history. Set here, not on the
        // returned executor — registering a tool shares the core, and the
        // setters take `Arc::get_mut`.
        if runtime == Runtime::Main {
            tools = tools.with_events(db.clone());
        }
        for tool in &all_tools {
            tools.register(tool.clone());
        }
        if let Some(delegate) = delegate {
            tools.register(delegate);
        }
        // Code mode: one tool that runs a program, in place of the model
        // calling three tools in three rounds. Registered last because a
        // program's calls go back through this executor — see `run_code`.
        if let Some(host) = &pyhost {
            let run_code = Arc::new(komo_tools::run_code::RunCodeTool::new(
                host.clone(),
                tools.downgrade(),
            ));
            tools.register(run_code);
        }
        // A tool the policy denies outright never gets advertised: it would
        // otherwise cost a schema, a prompt entry, and a whole round-trip per
        // attempt, all to be refused. Runs before the catalog is read, so the
        // prompt's tool list and the model's schemas agree by construction.
        let dropped = tools.drop_policy_denied(&config.runtime.policy.policy);
        if !dropped.is_empty() {
            tracing::info!(tools = %dropped.join(", "), "tools withheld by a policy deny rule");
        }
        tools
    };
    // The `run_code` API listing, when that runtime loaded the tool. Rendered
    // from the same catalog the schemas come from, so the two can never
    // disagree about what a program may call.
    let code_note_for = |tools: &ToolExecutor| -> Option<String> {
        let snapshot = tools.snapshot();
        snapshot
            .get("run_code")
            .is_some()
            .then(|| komo_tools::run_code::sdk_note(&snapshot))
            .flatten()
    };

    let tool_names_of = |tools: &ToolExecutor| -> Vec<String> {
        tools
            .definitions()
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    };

    // ── Sub-agent runtime (the `delegate` tool's worker) ─────────────────────
    // A real agent turn, not a bare completion: the full tool set, so a delegated
    // subtask can actually search/read/edit — and `delegate`'s `model` argument
    // picks which model does it (plan on one, apply on another).
    //
    // Safety comes from three places, none of them a new mechanism:
    //   - it is built WITHOUT `delegate`, so a sub-agent cannot spawn another;
    //   - it shares the **main approver**, and the parent's ambient session
    //     context is inherited (`AgentRuntime::handle_input` never overrides one),
    //     so every side effect still prompts the human in the real conversation
    //     and still resolves against the parent's workspace root;
    //   - it shares the run ledger, so each delegation is auditable on its own.
    // No memory enricher: a sub-agent is a worker, not the user's assistant.
    let subagent_tools = executor_for(Runtime::Subagent, approver.clone(), None);
    mounts.push(crate::pyhost::PluginMount::of(&subagent_tools));
    let subagent_tool_names = tool_names_of(&subagent_tools);
    let subagent_note = skills_note_for(&subagent_tool_names);
    let subagent_builder = Arc::new(
        SystemPromptBuilder::new(model_config)
            .tools(subagent_tool_names)
            .skills_note(subagent_note)
            .code_note(code_note_for(&subagent_tools))
            .plugins_dir(plugins_dir.clone())
            .workspace_root(Some(root.clone())),
    );
    let subagent_preamble: PreambleFn = Arc::new(move |roots| subagent_builder.build(roots));
    let subagent_llm = build_llm(
        model_config,
        Some(&subagent_tools),
        subagent_preamble,
        TurnInjections::default(),
        Some("delegate"),
    )?;
    // The seam every extracted observation goes through. It shares the query
    // service with recall, so "which existing claims might this be about" is
    // answered by the same hybrid matching that decides what gets injected.
    let consolidator = Arc::new(
        komo_services::memory_consolidation::MemoryConsolidator::new(
            memory_repo.clone(),
            aux_llm.clone(),
            memory_query.clone(),
        ),
    );
    let reviewer: Arc<dyn Reviewer> =
        Arc::new(ReflectiveReviewer::new(aux_llm.clone(), consolidator));
    // One coordinator instance shared by the runtime's post-run trigger and
    // the gateway's scheduled sweep — that sharing is what makes its
    // per-session in-flight guard effective across the two paths.
    let review = Arc::new(
        LearningCoordinator::new(
            db.clone(),
            db.clone(),
            db.clone(),
            reviewer,
            config.runtime.review_interval,
        )
        // Reads the user's next message as a verdict on the previous turn.
        // Without it every outcome stays `Unknown`, since nothing observable
        // when a turn ends tells success from silence.
        .with_feedback(aux_llm.clone()),
    );

    // Built before the runtimes because every one of them is assembled from it.
    let parts = RuntimeParts {
        db: db.clone(),
        history_window: model_config.max_history_messages,
        learning: review.clone(),
        // The window is the trigger: what a conversation loses to it is exactly
        // what a summary is for.
        compactor: Arc::new(Compactor::new(
            aux_llm.clone(),
            db.clone(),
            model_config.max_history_messages,
        )),
    };

    let subagent_runtime = Arc::new(parts.build(CapabilityProfile {
        llm: subagent_llm,
        tools: subagent_tools,
        max_turns: model_config.max_turns,
        learns: false,
        compacts: false,
    }));
    let delegate = Arc::new(DelegateTool::new(
        subagent_runtime,
        db.clone(),
        model_config.menu(),
        model_config.model.clone(),
    ));

    // Only the main runtime records its tool calls in a transcript. Every other
    // scope runs on a synthetic session (delegate, cron), and a transcript file
    // per one-shot turn is litter, not history.
    let tools = executor_for(Runtime::Main, approver.clone(), Some(delegate));
    mounts.push(crate::pyhost::PluginMount::of(&tools));

    // Assemble the tiered system prompt: stable identity + tool-aware guidance
    // (gated on the tools actually loaded) + skills catalog, then the project
    // instruction file of the turn's own working directory, then the
    // day-precision volatile footer. Wrapped in a factory so `complete` rebuilds
    // it per turn (per session) rather than freezing the date at process start —
    // important for the long-lived gateway. `workspace_root` is only the
    // fallback: a task session renders that tier from its own bound root, which
    // reaches the factory as the turn's `Session.roots`.
    let tool_names = tool_names_of(&tools);
    let main_note = skills_note_for(&tool_names);
    let prompt_builder = Arc::new(
        SystemPromptBuilder::new(model_config)
            .tools(tool_names)
            .skills_note(main_note)
            .code_note(code_note_for(&tools))
            .plugins_dir(plugins_dir.clone())
            .workspace_root(Some(root.clone()))
            // The main agent fields "how do I configure Komo" questions, so it
            // gets the built-in platform manual (wechat login, pairing, …).
            // Aux/delegate builders deliberately don't.
            .operations_manual()
            // …and the operator-authored user profile (~/.komo/USER.md), for the
            // same reason the aux/reviewer builders don't get it.
            .user_profile()
            .memory()
            // …and their machine-wide agent instructions (~/.agents/AGENTS.md),
            // shared with whatever other agents read that directory.
            .global_instructions(),
    );
    let preamble: PreambleFn = Arc::new(move |roots| prompt_builder.build(roots));

    // Hand the same tool instances to the LLM so the model can call them, plus
    // the memory enricher (main agent only): the memory store for recall
    // selection and the aux agent for recall screening, behind one interface.
    let enricher = Arc::new(MemoryEnricher::new(
        memory_repo.clone(),
        Some(aux_llm.clone()),
        memory_query.clone(),
    ));
    let llm = build_llm(
        model_config,
        Some(&tools),
        preamble,
        TurnInjections {
            enricher: Some(enricher),
            artifacts: Some(artifacts.clone()),
        },
        None,
    )?;

    // The conversation: the only runtime that learns from what it did, and the
    // only one whose turns are worth resuming.
    let runtime = parts.build(CapabilityProfile {
        llm,
        // The in-house agent loop hands each round to this executor; the LLM
        // above was handed the same catalog's schemas, declaration only.
        tools,
        max_turns: model_config.max_turns,
        learns: true,
        compacts: true,
    });

    // ── Cron agent runtime (general cron, agent mode) ────────────────────────
    // Runs `CronAction::Agent` jobs: the SAME full tool set as the main agent
    // (so a scheduled job can act — shell, git, skills), under the unattended
    // safety model — a `PolicyApprover` whose inner **suspends**, so a
    // `Risk::Normal` action either passes through an `unattended = true` policy
    // rule or this job's own grants, or stops the turn until the operator
    // answers in the home chat (docs/bot-runtime.md §5.4). Main model (jobs can
    // be arbitrarily complex), no memory enricher (sweeps aren't fed the user's
    // memory library), and the run ledger is shared so every job execution is
    // auditable via `komo run list`.
    // Deliberately `wrap`, not `wrap_with_store`: saved grants were accumulated
    // interactively and must not leak into an unattended context, where only an
    // explicit `unattended = true` config rule may grant. (The engine enforces
    // this again for a channel-less decision — two floors, on purpose.)
    let cron_approver = komo_bot::policy_approver::PolicyApprover::wrap(
        config.runtime.policy.policy.clone(),
        Arc::new(UnattendedSuspend),
    );
    // No `delegate`: the sub-agent runtime carries the *interactive* approver, and
    // handing that to an unattended job mixes trust models — a cron turn has no
    // ambient session, so the sub-agent's Risk::Normal actions would be auto-denied
    // anyway, just less legibly. A cron job that needs a sub-agent should say so
    // explicitly (its own runtime with the unattended approver), not inherit one.
    let cron_tools = executor_for(Runtime::Cron, cron_approver, None);
    mounts.push(crate::pyhost::PluginMount::of(&cron_tools));
    let cron_tool_names = tool_names_of(&cron_tools);
    // No operations_manual / user_profile: the cron agent is a background task
    // executor, not the user-facing assistant.
    let cron_note = skills_note_for(&cron_tool_names);
    let cron_builder = Arc::new(
        SystemPromptBuilder::new(model_config)
            .tools(cron_tool_names)
            .skills_note(cron_note)
            .code_note(code_note_for(&cron_tools))
            .plugins_dir(plugins_dir.clone())
            .workspace_root(Some(root.clone())),
    );
    let cron_preamble: PreambleFn = Arc::new(move |roots| cron_builder.build(roots));
    // An unattended routine writes files too — a nightly report is the case
    // §5.16 is for — so it is told where they belong. No enricher: a sweep must
    // not be fed the user's memory library.
    let cron_llm = build_llm(
        model_config,
        Some(&cron_tools),
        cron_preamble,
        TurnInjections {
            enricher: None,
            artifacts: Some(artifacts.clone()),
        },
        Some("cron"),
    )?;
    let cron_runtime = Arc::new(parts.build(CapabilityProfile {
        llm: cron_llm,
        tools: cron_tools,
        max_turns: model_config.max_turns,
        learns: false,
        compacts: false,
    }));

    // Now that every runtime has its executor, start the plugin host: a `py__`
    // tool has to be mounted with a handle back to the executor it will
    // dispatch its own tool calls through.
    if let (Some(plugins_dir), Some(host)) = (plugins_dir, pyhost) {
        crate::pyhost::start(&config.runtime.home, plugins_dir, host, mounts);
    }

    Ok(Wiring {
        runtime,
        review,
        memories: memory_repo,
        memory_query: memory_query.clone(),
        cron_runtime,
        output_store,
        wiki: wiki_ops,
        approvals,
    })
}

/// The note-vault tools (`[wiki]`), plus the operator handle `komo wiki`
/// borrows while the gateway holds the index open.
///
/// `wiki_read` is mounted even when the search backend will not open: a vault
/// whose index is unusable costs search, not the ability to read a note whose
/// path the user or a memory already names.
async fn wiki_tools(
    wiki: &komo_config::WikiConfig,
    db: &Arc<Db>,
) -> (
    Vec<Arc<dyn Tool>>,
    Option<crate::services::operator_control::actions::WikiOps>,
) {
    let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(komo_tools::wiki_read::WikiReadTool::new(
        wiki.vault.clone(),
    ))];

    // The index is a table in a database this process already has open, so
    // there is nothing left to be unreachable — the only failure is an
    // embedding url that is not a url.
    let handles = async {
        let index = db.chunk_index(komo_infra::chunk_index::WIKI).await?;
        let embedder = komo_infra::embedding::OllamaEmbedder::new(
            wiki.embedding.url.clone(),
            wiki.embedding.model.clone(),
        )?;
        Ok::<_, anyhow::Error>((
            Arc::new(index),
            Arc::new(embedder) as Arc<dyn komo_core::domain::embedding::EmbeddingClient>,
        ))
    }
    .await;
    let (index, embedder) = match handles {
        Ok(handles) => handles,
        Err(error) => {
            tracing::warn!(error = format!("{error:#}"), "wiki_search unavailable");
            return (tools, None);
        }
    };
    tracing::info!(vault = %wiki.vault.display(), "wiki_search ready");
    // One runner shared by every indexing caller: this process's `wiki_index`
    // tool, `komo wiki index` over the operator channel, and any cron job. Two
    // concurrent runs over one store is not merely wasteful — a rebuild resets
    // it.
    let runner = Arc::new(komo_services::wiki_indexing::WikiIndexRunner::new(
        index.clone(),
        embedder.clone(),
        wiki.vault.clone(),
        wiki.embedding.model.clone(),
    ));
    tools.push(Arc::new(komo_tools::wiki_search::WikiSearchTool::new(
        index, embedder,
    )));
    tools.push(Arc::new(komo_tools::wiki_index::WikiIndexTool::new(
        runner.clone(),
    )));
    (
        tools,
        Some(crate::services::operator_control::actions::WikiOps { runner }),
    )
}

/// Connect the configured MCP servers and turn their allowlisted tools into
/// komo tools. An unreachable server is a warning, never a failed boot.
async fn mcp_tools(servers: &[komo_config::McpServerConfig]) -> Vec<Arc<dyn Tool>> {
    if servers.is_empty() {
        return Vec::new();
    }
    let allowlists: std::collections::BTreeMap<String, Vec<String>> = servers
        .iter()
        .map(|s| (s.name.clone(), s.tools.clone()))
        .collect();
    let clients = komo_mcp::connect_all(
        servers
            .iter()
            .map(|s| (s.name.clone(), s.url.clone(), s.token.clone()))
            .collect(),
    )
    .await;

    let mut mounted: Vec<Arc<dyn Tool>> = Vec::new();
    for client in clients {
        let server = client.server().to_string();
        let offered = match client.list_tools().await {
            Ok(tools) => tools,
            Err(error) => {
                tracing::warn!(server = %server, %error, "mcp tools/list failed — no tools mounted");
                continue;
            }
        };
        // Empty allowlist = `all_tools = true`; config resolution rejects the
        // empty-and-not-all case, so this is never an accidental wildcard.
        let allow = allowlists.get(&server).cloned().unwrap_or_default();
        let wanted = |name: &str| allow.is_empty() || allow.iter().any(|t| t == name);

        let offered_names: Vec<String> = offered.iter().map(|t| t.name.clone()).collect();
        // A listed tool the server doesn't have is almost always a typo, and it
        // would otherwise be invisible — the model just never sees the tool.
        for missing in allow.iter().filter(|t| !offered_names.contains(t)) {
            tracing::warn!(
                server = %server,
                tool = %missing,
                available = %offered_names.join(", "),
                "mcp tool listed in config is not offered by the server"
            );
        }

        let mut names = Vec::new();
        for def in offered.into_iter().filter(|d| wanted(&d.name)) {
            let tool = Arc::new(komo_tools::mcp::McpTool::new(client.clone(), def));
            names.push(tool.name().to_string());
            mounted.push(tool);
        }
        tracing::info!(
            server = %server,
            mounted = names.len(),
            offered = offered_names.len(),
            tools = %names.join(", "),
            "mcp tools mounted"
        );
    }
    mounted
}
