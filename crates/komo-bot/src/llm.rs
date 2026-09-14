use komo_infra::claude_code::{self, CLAUDE_BASE_URL, ClaudeCodeAuth};
use komo_infra::codex::{CODEX_BASE_URL, CodexAuth, codex_static_headers};
use komo_services::artifact_store::ArtifactStore;
use komo_services::memory_enrichment::MemoryEnricher;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use serde_json::{Value, json};
use tracing::warn;

use komo_config::{ModelConfig, Provider, split_model_id};
use komo_core::domain::{
    catalog::ToolCatalog,
    llm::{DeltaSink, LlmClient, Step, TokenUsage, ToolCallReq, ToolOutcome, TurnDriver},
    message::{Message, Role},
    run::RecalledMemories,
    session::Session,
    session_event::{
        AssistantRoundEvent, HeaderReason, MessageSource, RequestHeaderEvent, SessionEvent,
        SessionEventKind, SurfacePlacement, TurnRecorder, UserMessageEvent, fold_request_header,
    },
};
use komo_provider::{
    AssistantBlock, Auth, Completion, Delta, Endpoint, LlmError, LlmErrorKind, ProviderClient,
    ToolSchema, Turn, UserBlock, Wire,
};

/// Produces the system prompt (preamble) on demand, for a turn on a session
/// with these roots. Called once per user turn so the prompt is rebuilt per
/// session rather than baked once at startup — the gateway is a long-lived
/// process, so a baked prompt would freeze the volatile tier (date) at boot.
/// The factory's output is day-precision, so it stays byte-identical across
/// turns within a day (upstream prompt cache stays warm) and self-heals across
/// midnight.
///
/// The roots are what make the context tier the *task's* rather than the
/// gateway process's; they are fixed for a task session's whole life and empty
/// for every other kind, so the prompt stays as cacheable as it was.
pub type PreambleFn = Arc<dyn Fn(&[String]) -> String + Send + Sync>;

/// What a runtime may add to the tail of a turn's user message.
///
/// Both vary per turn or per session, which is precisely why they live here
/// rather than in the system prompt: the provider cache prefix runs tools →
/// system → messages, so anything that moves that often must land where the new
/// bytes already are. Both are granted per runtime — an aux or delegate
/// sub-agent gets neither.
#[derive(Clone, Default)]
pub struct TurnInjections {
    /// Per-turn memory enrichment. `Some` only for the main agent — aux/delegate
    /// sub-agents must not be fed the user's memory library. The enricher owns
    /// the whole memory policy (selection, screening, rendering, usage tracking);
    /// this adapter only appends the finished prefix.
    pub enricher: Option<Arc<MemoryEnricher>>,
    /// komo's artifacts directory, for a runtime whose tools can write. The note
    /// names this session's own subdirectory; the workspace is what makes it
    /// writable (docs/bot-runtime.md §5.16).
    pub artifacts: Option<Arc<ArtifactStore>>,
}

/// The line that tells the model where its own output belongs. Deliberately
/// short — it rides on every turn — and it states the *distinction* rather than
/// only the path, because "put files here" without "those files are the user's"
/// is the half that gets ignored.
fn artifacts_note(artifacts: &ArtifactStore, session_id: &str) -> String {
    format!(
        "[artifacts] This conversation's own output directory is {}. Put anything \
         meant to last there — reports, scripts you wrote, files you downloaded — \
         and say where you put it. It is writable and is never cleaned up. Files \
         in the working directory are the user's: change them only when asked to.",
        artifacts.session_dir(session_id).display()
    )
}

/// Stand-in for a provider whose API key is missing (see [`build_llm`]):
/// construction always succeeds so a fresh install boots, and every call —
/// `begin_turn` inherits the default one-shot driver over `complete` — fails
/// with the fix. The error text reaches the user as the turn's reply.
struct UnconfiguredLlm {
    message: String,
}

#[async_trait]
impl LlmClient for UnconfiguredLlm {
    async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
        anyhow::bail!("{}", self.message)
    }
}

/// A [`LlmClient`] over one provider, via komo's own provider layer
/// (`infra::provider`).
///
/// komo owns the tool loop (`run_agent_loop`), so what this needs from a
/// provider is exactly one completion per call. Everything a client library used
/// to hold for us — the model handle, the preamble, the tool schemas — is a plain
/// field here, and switching model within a provider is a `String` swap rather
/// than minting a new typed handle.
pub struct ProviderLlm {
    client: Arc<ProviderClient>,
    /// The catalog whose schemas are advertised to the provider. Only the
    /// *declaration* goes over the wire: komo dispatches every requested call
    /// itself in `ToolExecutor::execute_round`.
    ///
    /// Read per turn rather than copied once at wiring, so a tool mounted while
    /// the process runs actually reaches the model. Rendering is name-sorted
    /// and therefore byte-stable for an unchanged set — mounting something is
    /// what costs the provider's cached prefix, not re-reading the catalog.
    /// `None` for a tool-less backend (aux, delegate, reviewer).
    tools: Option<Arc<ToolCatalog>>,
    /// The configured model: what a session with no override runs on.
    default_model: String,
    /// Which provider this is, for mapping a session's reasoning-effort level
    /// onto request params (see [`reasoning_params`]).
    provider: Provider,
    /// The effort a session with no override runs at (config `effort`; `None` =
    /// the provider's own default). Every aux path builds a synthetic session
    /// with empty overrides, so this is the only way an aux backend's effort is
    /// ever set — see [`ModelConfig::aux_variant`].
    default_effort: Option<String>,
    /// Prompt-cache family this backend's turns belong to, when it is not the
    /// session (see [`ProviderLlm::model_for`]). `None` — the main agent — keys
    /// the cache by session id; a backend whose sessions are one-shot but whose
    /// prompt prefix is always the same names its family here so those turns
    /// share one warm prefix instead of each cold-starting.
    cache_family: Option<String>,
    /// Rebuilds the system prompt each turn (see [`PreambleFn`]).
    preamble: PreambleFn,
    /// Max prior messages replayed as history per turn (config
    /// `max_history_messages`; `0` = unlimited). The backstop against a
    /// long-lived chat session sending its entire transcript every turn — see
    /// [`ProviderLlm::assemble`].
    max_history_messages: usize,
    /// Byte budget for the replayed history (`0` = unlimited). The message-count
    /// window alone can't bound context: a handful of pasted logs or diffs blows
    /// past any token limit while sitting well inside the count. Applied after the
    /// count window, trimming from the oldest end.
    max_history_bytes: usize,
    /// What this backend appends to a turn's user message, granted per runtime
    /// (see [`TurnInjections`]).
    injections: TurnInjections,
    /// Per-completion timeout, bounding all attempts of one round together.
    /// `None` = no timeout (config `llm_timeout_secs = 0`).
    timeout: Option<Duration>,
}

/// Total attempts for one model round-trip whose failure classifies as transient
/// (1 initial + retries). A constant rather than config, for the same reason the
/// tool executor's is: transient retry is an internal robustness backstop.
const LLM_RETRY_MAX_ATTEMPTS: usize = 4;
/// Local backoff before each retry, indexed by retry number (last entry reused).
///
/// A *fallback*: when the provider tells us how long to wait
/// ([`LlmError::retry_after`]) that always wins, because a server reporting when
/// its limit clears is more accurate than any table. This covers the failures
/// that carry no such hint — connection resets, 5xx, a stalled stream — and is
/// sized for the one that does not: a rate limit with no `Retry-After` takes
/// seconds to tens of seconds to clear, so a quarter-second-then-two-seconds
/// table would run out its attempts while the limit was still in force.
///
/// Total backoff (21s) stays well inside the round's `llm_timeout_secs` budget,
/// which bounds all attempts together (see [`with_retry`]).
const LLM_RETRY_BACKOFF_MS: [u64; 3] = [1_000, 5_000, 15_000];

/// Re-run `attempt` while its failure is retryable, bounded by
/// [`LLM_RETRY_MAX_ATTEMPTS`].
///
/// Retryability is the error's own answer ([`LlmError::is_retryable`]) rather
/// than a guess from its text, and the delay is the server's when it gave one.
/// A completion has no side effect that could double-apply, so re-sending is
/// safe by construction.
///
/// Deliberately nested *inside* [`with_timeout`] by every caller: the configured
/// timeout is then a budget for the whole round (attempts included), so retrying
/// can't multiply a turn's worst-case latency.
async fn with_retry<F, Fut, T>(mut attempt: F) -> Result<T, LlmError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, LlmError>>,
{
    let mut retries = 0usize;
    loop {
        let error = match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if retries + 1 >= LLM_RETRY_MAX_ATTEMPTS || !error.is_retryable() {
            return Err(error);
        }
        // The provider's own answer beats the table. This is the whole point of
        // carrying `retry_after` on the error: under a real rate limit the
        // server knows when it clears and we do not.
        let delay = error.retry_after.unwrap_or_else(|| {
            Duration::from_millis(LLM_RETRY_BACKOFF_MS[retries.min(LLM_RETRY_BACKOFF_MS.len() - 1)])
        });
        tracing::warn!(
            attempt = retries + 1,
            delay_ms = delay.as_millis(),
            kind = ?error.kind,
            server_paced = error.retry_after.is_some(),
            error = %error,
            "retryable LLM failure; retrying the completion"
        );
        tokio::time::sleep(delay).await;
        retries += 1;
    }
}

/// Run `fut` under `timeout` (if set), turning a stall into a clean error rather
/// than an indefinite await. Wraps [`with_retry`], so the budget covers every
/// attempt of one round rather than each attempt separately.
async fn with_timeout<F, T>(timeout: Option<Duration>, fut: F) -> Result<T, LlmError>
where
    F: Future<Output = Result<T, LlmError>>,
{
    match timeout {
        Some(d) => match tokio::time::timeout(d, fut).await {
            Ok(result) => result,
            Err(_) => Err(LlmError::new(
                LlmErrorKind::Timeout,
                format!(
                    "LLM completion timed out after {}s (provider unresponsive; \
                     failing the turn instead of leaving it running — raise \
                     `llm_timeout_secs` / `KOMO_LLM_TIMEOUT_SECS` if this is too tight)",
                    d.as_secs()
                ),
            )),
        },
        None => fut.await,
    }
}

/// Cross-provider dispatcher: one backend per provider, selected by the
/// session's model id.
///
/// A qualified id (`deepseek:deepseek-chat`) picks the backend here; the bare
/// remainder picks the model inside it.
///
/// A session naming no model of its own — every aux caller — runs the config's
/// own model, so a qualified *config* model decides its backend too
/// ([`RoutingLlm::own_provider`]).
///
/// An unqualified id — or one naming a provider this gateway has no client for —
/// falls through to the default backend rather than failing the turn: the api
/// channel already validates a client's choice against the advertised menu, so
/// reaching here with something unroutable means config changed under a stored
/// session, and running on the default is the recoverable answer.
struct RoutingLlm {
    by_provider: Vec<(Provider, Arc<dyn LlmClient>)>,
    /// The configured provider: where a session's **unqualified** id belongs
    /// (see `ModelConfig::menu`), and the last resort for one that names a
    /// backend this gateway has no client for.
    default_provider: Provider,
    /// The provider the **config's own model** names — where a session that
    /// names no model runs. The two differ exactly when that model is
    /// provider-qualified, which is what an aux or memory variant is
    /// (`[memory] model = "codex:gpt-5.6-sol"` on a DeepSeek conversation).
    /// Every aux caller builds a synthetic session with empty overrides, so
    /// without this the memory pipeline would quietly stay on the
    /// conversation's backend.
    own_provider: Provider,
}

impl RoutingLlm {
    fn route(&self, session: &Session) -> &Arc<dyn LlmClient> {
        let wanted = match session.model_override() {
            Some(id) => split_model_id(id).0.unwrap_or(self.default_provider),
            None => self.own_provider,
        };
        self.backend(wanted)
            .or_else(|| self.backend(self.default_provider))
            .expect("routing llm always holds its default provider's backend")
    }

    fn backend(&self, provider: Provider) -> Option<&Arc<dyn LlmClient>> {
        self.by_provider
            .iter()
            .find(|(p, _)| *p == provider)
            .map(|(_, backend)| backend)
    }
}

#[async_trait]
impl LlmClient for RoutingLlm {
    async fn complete(&self, session: &Session) -> anyhow::Result<String> {
        self.route(session).complete(session).await
    }

    async fn begin_turn(
        &self,
        session: &Session,
        deltas: Option<Arc<dyn DeltaSink>>,
        recorder: Option<Arc<dyn TurnRecorder>>,
    ) -> anyhow::Result<Box<dyn TurnDriver>> {
        self.route(session)
            .begin_turn(session, deltas, recorder)
            .await
    }

    async fn resume_turn(
        &self,
        session: &Session,
        events: &[SessionEvent],
        turn_id: &str,
        deltas: Option<Arc<dyn DeltaSink>>,
        recorder: Option<Arc<dyn TurnRecorder>>,
    ) -> anyhow::Result<Box<dyn TurnDriver>> {
        // Route on the provider the *interrupted turn* recorded, not the
        // session's current model override: continuing on a different backend
        // would replay one provider's opaque state (reasoning blobs, item ids)
        // into another. No backend for that provider anymore ⇒ error out, and
        // the caller falls back to the digest-primed fresh turn.
        let last_seq = events.last().map(|e| e.seq).unwrap_or(0);
        let header = fold_request_header(events, last_seq)
            .context("the interrupted turn recorded no request header")?;
        let provider = Provider::parse(&header.provider)?;
        let backend = self
            .backend(provider)
            .with_context(|| format!("no configured backend for provider `{}`", header.provider))?;
        backend
            .resume_turn(session, events, turn_id, deltas, recorder)
            .await
    }
}

/// Extra answer budget granted on top of an Anthropic thinking budget, so the
/// model has room to write a reply after it finishes reasoning.
const THINKING_ANSWER_HEADROOM: u64 = 8_192;

/// `max_tokens` for a Claude Code turn. Adaptive thinking names no budget of
/// its own and is charged against this cap, so it has to leave room for a
/// reasoning pass *and* the answer after it.
const CLAUDE_CODE_MAX_TOKENS: u64 = 32_000;

/// Map a reasoning-effort level onto the provider's request params, or `None`
/// when this provider/level pair has no effect.
///
/// Which levels a provider offers is [`Provider::efforts`]; this is the other
/// half — how a level is actually spelled on the wire.
fn reasoning_params(provider: Provider, effort: &str) -> Option<Value> {
    // The scale differs per provider (DeepSeek has `max` and no `medium`), so
    // the accepted set is the one that provider advertises — plus its aux
    // default (`none` on DeepSeek: thinking off, a real wire value kept off the
    // menu because it is not a level anyone picks per turn).
    let level = effort.trim();
    if !provider.accepts_effort(level) {
        return None;
    }
    match provider {
        // Every Responses-API provider takes `reasoning.effort` verbatim.
        Provider::OpenAi | Provider::OpenRouter | Provider::Codex | Provider::DeepSeek => {
            Some(json!({ "reasoning": { "effort": level } }))
        }
        // Anthropic has no effort scale — it budgets thinking in tokens, so the
        // levels map onto budgets. The caller must also raise `max_tokens` above
        // the budget (thinking is charged against it): see `model_for`.
        Provider::Anthropic => {
            let budget = match level {
                "low" => 4_096,
                "medium" => 10_240,
                _ => 24_576,
            };
            Some(json!({ "thinking": { "type": "enabled", "budget_tokens": budget } }))
        }
        // A Claude Code login only reaches Claude 4.6+, where extended thinking
        // is *adaptive*: the manual `thinking` block above is rejected outright
        // (4.7+) and the level goes to `output_config.effort` instead. `display`
        // defaults to `omitted` there, which would drop the reasoning komo shows
        // in its clients, so it asks for summaries.
        //
        // `none` sends the explicit disable rather than omitting the parameter,
        // because an adaptive model thinks unless it is told not to.
        Provider::ClaudeCode => Some(if level == "none" {
            json!({ "thinking": { "type": "disabled" } })
        } else {
            json!({
                "thinking": { "type": "adaptive", "display": "summarized" },
                "output_config": { "effort": level },
            })
        }),
    }
}

/// Shallow-merge `extra`'s top-level keys into `base` (extra wins).
fn merge_params(base: Option<Value>, extra: Value) -> Value {
    match (base, extra) {
        (Some(Value::Object(mut base)), Value::Object(extra)) => {
            base.extend(extra);
            Value::Object(base)
        }
        (_, extra) => extra,
    }
}

impl ProviderLlm {
    /// This turn's tool declarations, rendered from the shared catalog.
    ///
    /// Name-sorted (the catalog is), so the block is byte-identical between
    /// turns whose tool set did not change — which is what keeps the provider's
    /// cached prefix valid across a conversation. Read per turn rather than
    /// copied at wiring, so a tool mounted while the process runs is one the
    /// model can actually see.
    fn tool_schemas(&self) -> Vec<ToolSchema> {
        let Some(catalog) = &self.tools else {
            return Vec::new();
        };
        catalog
            .snapshot()
            .advertised()
            .map(|tool| ToolSchema {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
            })
            .collect()
    }

    /// Assemble this turn's `(preamble, prompt, history)`: split the session
    /// into the latest user prompt + prior history, rebuild the system prompt,
    /// and inject recalled context (main agent only) in front of the user prompt.
    /// Run once per turn — never per tool-loop round (recall is keyed on the
    /// user message, and re-running it each round would churn the prompt).
    ///
    /// # Invariant: a stored message renders the same bytes forever
    ///
    /// komo does not store provider-shaped history (that is what lets a session
    /// switch models, even across providers, mid-conversation) — it re-renders
    /// the transcript every turn. The price of that freedom is this rule:
    /// **rendering a message must be a pure function of that message's stored
    /// fields.** Nothing may depend on how far it sits from the end of the
    /// window, or on anything else that moves as the conversation grows.
    ///
    /// Break it and the provider prefix cache dies quietly: a message whose
    /// bytes change is a divergence point, and everything after it is recomputed
    /// on every request for the rest of the turn (see [`to_turn`], which used to
    /// break exactly this way). The two places that legitimately vary per turn —
    /// where the window *starts*, and the memory blocks — are handled so that
    /// they don't rewrite anything: the cut snaps to a content-derived anchor
    /// ([`window_history`]), and recall is appended at the tail rather than
    /// folded into the prefix (below).
    async fn assemble(
        &self,
        session: &Session,
    ) -> anyhow::Result<(String, String, Vec<Turn>, RecalledMemories)> {
        // The current prompt is the most recent user message; everything before
        // it forms the conversation history sent to the model.
        let last_user_idx = session
            .messages
            .iter()
            .rposition(|m| m.role == Role::User)
            .context("no user message to respond to")?;
        let prompt = session.messages[last_user_idx].content.clone();

        // Window the replayed history to the most recent `max_history_messages`
        // (0 = keep everything). Without this a long-lived chat session
        // (telegram/feishu/wechat are keyed by chat id and only rotate on an
        // explicit context boundary) would resend its entire transcript every
        // turn —
        // unbounded token cost and latency, eventually overflowing the context
        // window. The stable system-prompt + memory prefix is untouched, so the
        // upstream prompt cache is unaffected by trimming the tail.
        let window = window_history(
            &session.messages[..last_user_idx],
            self.max_history_messages,
            self.max_history_bytes,
        );
        let history: Vec<Turn> = window.iter().flat_map(to_turns).collect();

        // Rebuild the system prompt for this turn. It rides on the per-turn
        // request rather than on shared state, so concurrent sessions in the
        // gateway stay independent. A task session's roots decide which
        // project's instructions the context tier carries.
        let preamble = (self.preamble)(&session.roots);

        // L1 is already in the file-backed preamble. L3 varies per turn and
        // rides on the user message to preserve the stable prompt prefix.
        let mut prompt = prompt;
        let mut memories = RecalledMemories::default();
        if let Some(enricher) = &self.injections.enricher
            && let Some(injection) = enricher
                .enrich(session, &prompt, &session.messages[..last_user_idx])
                .await
        {
            if let Some(recall) = injection.recall {
                prompt = format!("{recall}\n\n{prompt}");
            }
            memories = injection.used;
        }

        // Where this session's own output belongs. It names a per-session
        // directory, so it rides at the tail of the user message for the same
        // reason recall does: in the system prompt it would give every session a
        // different cached prefix.
        if let Some(artifacts) = &self.injections.artifacts {
            prompt = format!("{prompt}\n\n{}", artifacts_note(artifacts, &session.id));
        }

        Ok((preamble, prompt, history, memories))
    }

    /// Resolve this turn's model settings: the assembled preamble, then the
    /// session's own model / reasoning-effort choices.
    ///
    /// Only the *main* agent is ever handed a stored session: every aux path
    /// (reviewer, delegate, recall screening, sweeps) builds a synthetic
    /// `Session`, whose overrides are empty. That is what keeps a conversation's
    /// model choice from leaking onto the aux model.
    fn model_for(&self, preamble: String, session: &Session) -> TurnModel {
        // A session's model may be provider-qualified (`deepseek:deepseek-chat`).
        // Routing on the prefix is `RoutingLlm`'s job — by the time we get here
        // the provider is already decided, so only the bare id matters.
        let model = session
            .model_override()
            .map(|id| split_model_id(id).1.to_string())
            .unwrap_or_else(|| self.default_model.clone());
        let mut turn = TurnModel {
            model,
            preamble,
            extra: None,
        };

        if self.provider == Provider::ClaudeCode {
            // Claude Code's own opening line, ahead of komo's prompt. Anthropic
            // routes OAuth traffic by client identity and the headers are only
            // half of it: requests that do not announce themselves this way
            // intermittently answer 500. It leads the stable tier, so the
            // cached prefix is unaffected.
            turn.preamble = format!("{}\n\n{}", claude_code::SYSTEM_PREFIX, turn.preamble);
            // Anthropic requires `max_tokens` and charges thinking against it,
            // and adaptive thinking has no budget for the codec's default to be
            // sized against — an 8K cap would let a long reasoning pass eat the
            // whole answer. Well under the model's 128K ceiling, so a long
            // conversation stays clear of the prompt-relative limit.
            turn.extra = Some(merge_params(
                turn.extra.take(),
                json!({ "max_tokens": CLAUDE_CODE_MAX_TOKENS }),
            ));
        }

        // The Responses API caches by prefix automatically, but shard routing is
        // best-effort; `prompt_cache_key` pins related requests to the same
        // cache shard — the Codex CLI itself sends its session id here for the
        // same reason.
        //
        // What the key must identify is the *prefix family*, not the
        // conversation: two requests share a cache only if their
        // system-prompt + tool-definition bytes match. For the main agent the
        // two coincide (one conversation = one prefix), so the session id is
        // right. For a backend whose sessions are one-shot — delegate
        // (`delegate:<uuid>`), cron (`cron:<name>:<ts>`) — the
        // session id is a *different* key every time even though every one of
        // those turns opens with identical bytes, so each would cold-start.
        // Those backends declare a family at wiring instead. Anchoring them on
        // the *parent's* key would be the other mistake: a frequent side query
        // would evict the conversation's own prefix.
        //
        // Anthropic has no such parameter (it caches from explicit
        // `cache_control` breakpoints, which `provider::messages` marks) and
        // rejects unknown request fields, so it is excluded.
        if self.client.wire == Wire::Responses {
            turn.extra = Some(merge_params(
                turn.extra.take(),
                json!({
                    "prompt_cache_key": cache_key(self.cache_family.as_deref(), &session.id)
                }),
            ));
        }

        if let Some(params) = session
            .effort_override()
            .or(self.default_effort.as_deref())
            .and_then(|effort| reasoning_params(self.provider, effort))
        {
            // Anthropic charges thinking against `max_tokens`, so a budget above
            // the cap is rejected outright — raise the cap to clear it.
            if let Some(budget) = params
                .get("thinking")
                .and_then(|thinking| thinking.get("budget_tokens"))
                .and_then(Value::as_u64)
            {
                turn.extra = Some(merge_params(
                    turn.extra.take(),
                    json!({ "max_tokens": budget + THINKING_ANSWER_HEADROOM }),
                ));
            }
            turn.extra = Some(merge_params(turn.extra.take(), params));
        }
        turn
    }
}

/// One turn's model settings: which model runs it, the system prompt assembled
/// for it, and the request knobs the session's reasoning-effort choice implies.
///
/// Requests are built off this round by round, so a round is exactly one
/// provider completion and komo's loop stays in charge of what happens between
/// rounds.
struct TurnModel {
    model: String,
    preamble: String,
    /// Extra top-level request fields, merged over the codec's defaults.
    extra: Option<Value>,
}

#[async_trait]
impl LlmClient for ProviderLlm {
    async fn complete(&self, session: &Session) -> anyhow::Result<String> {
        // Tool-less by contract: this is the single-shot path for aux callers
        // (reviewer / recall screening), and it advertises no
        // tools at all — nothing here would dispatch a call the model made, so it
        // must not be able to ask for one. One completion is the whole answer.
        let (preamble, prompt, history, _) = self.assemble(session).await?;
        let turn = self.model_for(preamble, session);
        let mut history = history;
        history.push(Turn::user(prompt));
        let completion = with_timeout(
            self.timeout,
            with_retry(|| {
                self.client.complete(
                    &turn.model,
                    &turn.preamble,
                    &history,
                    &[],
                    turn.extra.as_ref(),
                    // An aux completion has no watcher by construction — it is a
                    // side query on a synthetic session, not the conversation.
                    None,
                )
            }),
        )
        .await?;
        Ok(completion.text())
    }

    async fn begin_turn(
        &self,
        session: &Session,
        deltas: Option<Arc<dyn DeltaSink>>,
        recorder: Option<Arc<dyn TurnRecorder>>,
    ) -> anyhow::Result<Box<dyn TurnDriver>> {
        let (preamble, prompt, history, memories) = self.assemble(session).await?;
        let turn_loop = TurnLoop {
            client: self.client.clone(),
            turn: self.model_for(preamble, session),
            // Taken once here, then re-sent unchanged every round: a turn
            // declares one set of tools from its first round to its last.
            tools: self.tool_schemas(),
            history,
            start: TurnStart::Prompt(Turn::user(prompt)),
            recorder,
            provider_name: self.provider.name(),
            timeout: self.timeout,
            usage: TokenUsage::default(),
            memories,
            degraded: false,
            replayed: Vec::new(),
            deltas,
            rounds: 0,
        };
        turn_loop.record_header(HeaderReason::Initial).await;
        Ok(Box::new(turn_loop))
    }

    async fn resume_turn(
        &self,
        session: &Session,
        events: &[SessionEvent],
        turn_id: &str,
        deltas: Option<Arc<dyn DeltaSink>>,
        recorder: Option<Arc<dyn TurnRecorder>>,
    ) -> anyhow::Result<Box<dyn TurnDriver>> {
        // Which tools recovery may simply run again. Read off the same catalog
        // the round was dispatched from, so a tool that stops being idempotent
        // stops being replayed with it.
        let catalog = self.tools.as_ref().map(|tools| tools.snapshot());
        let idempotent = |name: &str| {
            catalog
                .as_ref()
                .and_then(|catalog| catalog.get(name))
                .is_some_and(|tool| tool.idempotent())
        };
        let rebuilt = rebuild_from_events(session, events, turn_id, &idempotent)?;
        let turn_loop = TurnLoop {
            client: self.client.clone(),
            turn: TurnModel {
                model: rebuilt.model,
                preamble: rebuilt.preamble,
                extra: rebuilt.extra,
            },
            tools: self.tool_schemas(),
            history: rebuilt.history,
            start: rebuilt.start,
            recorder,
            provider_name: self.provider.name(),
            timeout: self.timeout,
            usage: TokenUsage::default(),
            // A resumed turn reopens the recorded prompt rather than
            // assembling one, so there is no fresh enrichment to report; the
            // interrupted run's row already holds what was injected.
            memories: RecalledMemories::default(),
            degraded: false,
            replayed: Vec::new(),
            deltas,
            rounds: 0,
        };
        // The continuation journals itself from its rebuilt state, so a second
        // interruption resumes from here rather than replaying this rebuild.
        turn_loop.record_header(HeaderReason::Resume).await;
        Ok(Box::new(turn_loop))
    }
}

/// An interrupted turn, rebuilt from its journal: the model settings and
/// history to reopen with, and how to pick up (see [`TurnStart`]).
struct RebuiltTurn {
    model: String,
    preamble: String,
    extra: Option<Value>,
    history: Vec<Turn>,
    start: TurnStart,
}

/// Rebuild the state an interrupted turn died with, from its session's events.
///
/// The history is **derived**, not stored: `session.messages` is the
/// conversation projected out of the same log, and resume's own precondition is
/// that it still ends on the interrupted turn's user message. Only what a later
/// request cannot re-derive — the model settings and the rendered prompt — was
/// snapshotted, in `request/header`.
///
/// Rounds are replayed in `seq` order, and each round's **results are ordered by
/// its own recorded blocks**, never by the seq their settle landed on: a round
/// runs concurrently, so settle order is completion order, and rebuilding in it
/// would hand the provider a different request than the live turn sent.
///
/// `idempotent` answers whether a tool may simply be run again — the newest
/// round's unsettled calls are re-dispatched through it rather than reported as
/// lost (see [`TurnStart::Replay`]).
///
/// A turn resumed **twice** is rebuilt from every attempt at it, not just the
/// last one: each continuation is its own turn in the log, so the rounds a
/// second crash has to replay are spread across the chain, and reading only the
/// newest id would drop the work the first attempt already paid for and answer
/// the question from scratch. The chain is walked through
/// `turn/started{resumed_from}`.
fn rebuild_from_events(
    session: &Session,
    events: &[SessionEvent],
    turn_id: &str,
    idempotent: &dyn Fn(&str) -> bool,
) -> anyhow::Result<RebuiltTurn> {
    let last_seq = events.last().map(|e| e.seq).unwrap_or(0);
    let header = fold_request_header(events, last_seq)
        .context("the interrupted turn recorded no request header")?
        .clone();

    let mut history: Vec<Turn> = session.messages.iter().flat_map(to_turns).collect();

    let attempts = komo_core::domain::session_event::attempt_chain(events, turn_id);

    // One round's calls: what was dispatched, and what came back.
    struct Round {
        id: String,
        blocks: Vec<AssistantBlock>,
        settled: std::collections::HashMap<String, String>,
        /// The runtime's nudge, if this round drew one. It belongs to the round
        /// it answered — a nudge only ever follows a round that made no call,
        /// and that round contributes no results message, so this is the only
        /// thing that keeps the rebuilt history alternating there.
        nudge: Option<String>,
    }
    let mut rounds: Vec<Round> = Vec::new();
    // Calls that reached the approval gate. A durable `approval/requested` with
    // no settle beside it **proves the tool body never ran** — that is the whole
    // point of writing it before the wait — so such a call is re-dispatched on
    // the way back regardless of whether it is idempotent. Without this, a turn
    // that stopped for approval on a `shell` would come back and tell the model
    // the command may or may not have landed, which is the one thing the barrier
    // exists to rule out.
    let mut gated: std::collections::HashSet<String> = std::collections::HashSet::new();
    for event in events
        .iter()
        .filter(|e| e.turn_id_of_work().is_some_and(|id| attempts.contains(id)))
    {
        match &event.kind {
            SessionEventKind::ApprovalRequested(approval) => {
                gated.insert(approval.call_id.clone());
            }
            // A call that stopped to wait did not happen either — the tool
            // asked to be woken instead of running (`wait`, `ask_user`). Same
            // reading, same unconditional re-dispatch: idempotency has nothing
            // to say about a body that never ran.
            SessionEventKind::TurnSuspended(suspended) if !suspended.call_id.is_empty() => {
                gated.insert(suspended.call_id.clone());
            }
            SessionEventKind::AssistantRound(round) => rounds.push(Round {
                id: round.response_id.clone(),
                blocks: serde_json::from_value(round.blocks.clone())
                    .context("parsing a recorded assistant round")?,
                settled: std::collections::HashMap::new(),
                nudge: None,
            }),
            SessionEventKind::ToolCallSettled(call) => {
                if let Some(round) = rounds.last_mut() {
                    let text = if call.error.is_empty() {
                        call.result.clone()
                    } else {
                        call.error.clone()
                    };
                    round.settled.insert(call.call_id.clone(), text);
                }
            }
            // The runtime's own mid-turn message, recorded before the round it
            // asked for. It answers the round it followed, so it rides on that
            // round rather than on `history` — which at this point still ends
            // wherever the surface left it.
            SessionEventKind::UserMessage(m) if m.source == MessageSource::Runtime => {
                if let Some(round) = rounds.last_mut() {
                    round.nudge = Some(m.content.clone());
                }
            }
            SessionEventKind::UserMessage(m) if m.source == MessageSource::Injected => {
                // Recorded when it entered history mid-turn; it belongs after
                // the results of the round it interrupted.
                if let Some(Turn::User(blocks)) = history.last_mut() {
                    blocks.push(UserBlock::Text(format!(
                        "{INTERJECTION_PREFIX}{}",
                        m.content
                    )));
                }
            }
            _ => {}
        }
    }

    let mut lost_calls = false;
    let mut replay: Option<(Vec<ToolCallReq>, Vec<ReplaySlot>)> = None;
    let last = rounds.len().saturating_sub(1);
    for (n, round) in rounds.iter().enumerate() {
        history.push(Turn::Assistant {
            id: (!round.id.is_empty()).then(|| round.id.clone()),
            blocks: round.blocks.clone(),
        });
        // The round's own blocks are the call list, not its `tool/call-started`
        // events: the blocks are what the model sent — verbatim, in provider
        // order, with unredacted arguments — so a call rebuilt from them is the
        // call the live turn made. The started events are the ledger's redacted
        // copy, which is the wrong thing to re-issue.
        let Step::ToolCalls { calls, .. } = blocks_to_step(&round.blocks) else {
            // A round that called nothing sends no results message back, so the
            // only user turn that can follow it is a nudge. Without it the
            // rebuilt history would put two assistant turns back to back.
            if let Some(text) = &round.nudge {
                history.push(Turn::User(vec![UserBlock::Text(text.clone())]));
            }
            continue;
        };
        let mut slots: Vec<ReplaySlot> = Vec::new();
        let mut rerun = Vec::new();
        for call in calls {
            let key = call.call_id.clone().unwrap_or_else(|| call.id.clone());
            let known = match round.settled.get(&key) {
                Some(text) => Some(text.clone()),
                // Unsettled. Only the newest round is still in flight — an
                // earlier round's results must have been sent or the round
                // after it would not exist — and only an idempotent tool may
                // simply be run again. Anything else is the model's call to
                // make, so it gets told.
                // Never started: either the gate holds it (see `gated`), or it
                // is the newest round's call and the tool may simply be run
                // again.
                None if n == last && (gated.contains(&key) || idempotent(&call.name)) => {
                    rerun.push(call);
                    None
                }
                None => {
                    lost_calls = true;
                    Some(INTERRUPTED_RESULT_NOTE.clone())
                }
            };
            slots.push((
                key.clone(),
                known.map(|text| UserBlock::ToolResult {
                    id: key.clone(),
                    call_id: Some(key),
                    text,
                }),
            ));
        }
        if rerun.is_empty() {
            history.push(Turn::User(
                slots.into_iter().filter_map(|(_, block)| block).collect(),
            ));
        } else {
            // Held back: this round's results message is only complete once the
            // replayed calls have answered, and it is `step` that assembles it.
            replay = Some((rerun, slots));
        }
    }

    // Where did the interruption land? A round whose calls all settled and that
    // produced no further round is a turn that had answered and only failed to
    // land it; anything else continues.
    let start = if let Some((calls, slots)) = replay {
        TurnStart::Replay { calls, slots }
    } else {
        match history.last() {
            None => anyhow::bail!("nothing to resume: the turn recorded no history"),
            Some(Turn::Assistant { blocks, .. }) if !lost_calls => {
                let text = blocks
                    .iter()
                    .filter_map(|block| match block {
                        AssistantBlock::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                anyhow::ensure!(
                    !text.trim().is_empty(),
                    "interrupted turn ended on an empty assistant round"
                );
                TurnStart::Final(text)
            }
            _ => TurnStart::Continue,
        }
    };

    Ok(RebuiltTurn {
        model: header.model,
        preamble: header.system,
        extra: header.extra,
        history,
        start,
    })
}

/// Marks a mid-turn user message inside the tool-results message, so the model
/// can tell "the human just told me something" from tool output. Without it the
/// text sits next to a pile of results and reads as more data.
const INTERJECTION_PREFIX: &str = "The user sent this while you were working — \
     take it into account before your next step:\n";

/// What a tool call whose result was lost to the interruption gets fed back as
/// on resume. This is the answer for a tool that is *not* idempotent — one that
/// is gets re-dispatched instead ([`TurnStart::Replay`]). A mutation cannot be
/// assumed repeatable, so whether to re-issue it stays the model's decision.
static INTERRUPTED_RESULT_NOTE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "[This call's result was lost when the process was interrupted before it finished. {}]",
        komo_core::domain::tool::UNCERTAIN_OUTCOME_ADVICE
    )
});

/// One call's place in a replayed round's results message: `None` is the hole a
/// re-dispatched call fills, `Some` a result the interrupted process already had.
type ReplaySlot = (String, Option<UserBlock>);

/// A result block's slot key: the `call_id` when there is one, else the item
/// id — the same derivation [`rebuild_from_events`] keys its slots by. On the
/// Responses wire the two differ (`fc_…` item id, `call_…` call id), and the
/// call id is the one the provider matches outputs on: keying a slot one way
/// and its result the other leaves the hole filled with a placeholder and the
/// real result appended beside it, which the provider rejects as a duplicate
/// output for that call.
fn result_key(block: &UserBlock) -> Option<&str> {
    match block {
        UserBlock::ToolResult { id, call_id, .. } => Some(call_id.as_deref().unwrap_or(id)),
        _ => None,
    }
}

/// Close a replayed round: drop what just ran into the holes the interrupted
/// process left, in the order the model issued the calls.
///
/// Order is the point. The results could be concatenated in any order and still
/// answer every call, but the round's message would then differ from the one the
/// live turn was assembling — and `rebuild == live` is what lets a second
/// interruption resume from here, and what keeps the provider's cached prefix.
fn fill_replay_slots(slots: Vec<ReplaySlot>, mut fresh: Vec<UserBlock>) -> Vec<UserBlock> {
    let mut blocks = Vec::with_capacity(slots.len());
    for (id, known) in slots {
        if let Some(block) = known {
            blocks.push(block);
            continue;
        }
        let at = fresh
            .iter()
            .position(|block| result_key(block) == Some(id.as_str()));
        match at {
            Some(at) => blocks.push(fresh.remove(at)),
            // The executor answers every call it is handed, so this is
            // unreachable — but an open hole is the one shape a provider
            // rejects outright, so it is filled rather than left.
            None => blocks.push(UserBlock::ToolResult {
                id: id.clone(),
                call_id: Some(id),
                text: INTERRUPTED_RESULT_NOTE.clone(),
            }),
        }
    }
    // A result that matched no slot would otherwise be dropped, and a dropped
    // tool result is the other shape a provider rejects.
    blocks.append(&mut fresh);
    blocks
}

/// How a [`TurnLoop`] opens: a fresh turn sends its prompt; a resumed one
/// picks up from whatever state the journal ended in.
enum TurnStart {
    /// Fresh turn — push the opening prompt, then complete.
    Prompt(Turn),
    /// Resumed with the history already ending on a user turn (the journal's
    /// last row was tool results, or the envelope's own prompt) — complete
    /// over it as it stands.
    Continue,
    /// Resumed past the finish line: the interrupted turn had already produced
    /// its final answer, it just never reached the transcript. No request at
    /// all — hand the answer back.
    Final(String),
    /// Resumed into a round that was still running: `calls` never settled and
    /// their tools are idempotent, so they are simply re-dispatched instead of
    /// costing the model a round to be told their results were lost.
    ///
    /// `slots` is the round's results message with a hole where each of those
    /// calls goes — held back because a round sends *one* results message, and
    /// `step` can only assemble it once the re-dispatched calls have answered.
    /// Keeping the holes in place is what makes the message it finally sends
    /// byte-identical to the one the interrupted turn was building.
    Replay {
        calls: Vec<ToolCallReq>,
        slots: Vec<ReplaySlot>,
    },
    /// `first()` already ran.
    Started,
}

/// A [`TurnDriver`] over a per-turn [`TurnModel`]. Holds the growing conversation
/// history so each round is a single provider completion — one round-trip per
/// round, komo owns the loop.
struct TurnLoop {
    client: Arc<ProviderClient>,
    turn: TurnModel,
    /// Tool schemas re-sent every round (see [`ProviderLlm::tools`]).
    tools: Vec<ToolSchema>,
    history: Vec<Turn>,
    /// The opening move; consumed by `first()`, then [`TurnStart::Started`].
    start: TurnStart,
    /// Journal for this turn's provider-level state, written in lockstep with
    /// `history` (envelope at construction, one row per round-trip and one per
    /// results feed-back). `None` — every aux path — journals nothing.
    recorder: Option<Arc<dyn TurnRecorder>>,
    /// This backend's provider, as recorded in the journal envelope.
    provider_name: &'static str,
    /// Per-round completion timeout (see [`ProviderLlm::timeout`]).
    timeout: Option<Duration>,
    /// Tokens spent so far this turn, summed over rounds; read by the runtime for
    /// the ledger once the turn ends.
    usage: TokenUsage,
    /// The memories prompt assembly injected, carried to the ledger at turn end
    /// (see `TurnDriver::memories`). Set once when the turn starts and never
    /// touched again — a resumed turn assembles nothing, so it reports none.
    memories: RecalledMemories,
    /// Whether this turn already spent its one context-overflow degrade (see
    /// [`TurnLoop::degrade_for_overflow`]). Once used, a second overflow
    /// fails the turn: the first degrade is a real reclaim, so if the request
    /// is *still* too large the shortfall is structural and retrying only burns
    /// another round-trip on a request that cannot fit.
    degraded: bool,
    /// A resumed round's results message under construction — see
    /// [`TurnStart::Replay`]. Empty for every turn but one resumed into a round
    /// that was still running.
    replayed: Vec<ReplaySlot>,
    /// Where to stream this turn's output as it is produced. `None` when nothing
    /// is watching, which is most turns — and then no per-chunk work happens at
    /// all.
    deltas: Option<Arc<dyn DeltaSink>>,
    /// Model round-trips this turn has made, for the per-round token log.
    rounds: usize,
}

/// Bytes of a tool result kept when a turn is degraded for overflow — the head
/// and tail of that size each, on the reasoning that a result's beginning
/// (what it is) and end (how it concluded) carry most of the signal. The full
/// text is already on disk in the tool-output store when it mattered, so this
/// discards a copy, not the only copy.
const OVERFLOW_TOOL_RESULT_KEEP: usize = 4 * 1024;

impl TurnLoop {
    /// Record this turn's events. Best-effort by contract: recording buys
    /// resumability, and a broken store must cost exactly that.
    async fn record(&self, kinds: Vec<SessionEventKind>) {
        if let Some(recorder) = &self.recorder {
            recorder.record(kinds).await;
        }
    }

    fn turn_id(&self) -> String {
        self.recorder
            .as_ref()
            .map(|r| r.turn_id().to_string())
            .unwrap_or_default()
    }

    /// Record the request envelope — model settings, the rendered system
    /// prompt, the assembled tool schemas.
    ///
    /// Deliberately **not** the history: that is derived from the session's own
    /// events, so copying it here would put a rendered prompt and every tool
    /// schema into the log once per round. Only the parts a later request
    /// cannot re-derive are stored, and only when they change — an unchanged
    /// envelope is inherited (`header_snapshot_reason`).
    ///
    /// A resumed turn always writes one, identical or not: the boundary between
    /// the interrupted loop and the one that picked it up has to be visible in
    /// the log rather than inferred from a gap.
    async fn record_header(&self, reason: HeaderReason) {
        self.record(vec![SessionEventKind::RequestHeader(RequestHeaderEvent {
            reason,
            provider: self.provider_name.to_string(),
            model: self.turn.model.clone(),
            effort: String::new(),
            system: self.turn.preamble.clone(),
            tools: self.tools.iter().map(|t| t.name.clone()).collect(),
            extra: self.turn.extra.clone(),
        })])
        .await;
    }

    /// Send one round-trip: complete over `history`, then commit the assistant
    /// turn (verbatim — text + tool calls + reasoning together) to history so the
    /// next round sees a provider-correct transcript.
    ///
    /// Committing reasoning verbatim is what carries a reasoning model's chain of
    /// thought across the tool loop: the provider hands back an opaque blob, and
    /// echoing it into the next request is the only way the model picks up where
    /// it left off instead of re-deriving its plan every round.
    async fn run(&mut self, prompt: Turn) -> anyhow::Result<Step> {
        self.history.push(prompt);
        self.complete_committed().await
    }

    /// The round-trip itself, over `history` as it stands (which must end on a
    /// user turn). Split from [`run`](Self::run) so a resumed turn — whose
    /// history was rebuilt already ending on a user turn — can complete without
    /// pushing anything.
    async fn complete_committed(&mut self) -> anyhow::Result<Step> {
        let started = std::time::Instant::now();
        let completion = match self.complete_round().await {
            Ok(completion) => completion,
            // Overflowing the context window is not transient — `with_retry`
            // correctly refuses to re-send the same oversized request — but it
            // is recoverable, because most of what fills a long turn is tool
            // output the model has already read once. Reclaim that and try the
            // round again rather than losing every round of work before it.
            Err(error) if error.is_context_overflow() && self.degrade_for_overflow() => {
                tracing::warn!(
                    "context window exceeded; retrying this round on a degraded history"
                );
                self.complete_round().await?
            }
            Err(error) => return Err(error.into()),
        };

        self.usage.add(TokenUsage {
            input: completion.usage.input,
            output: completion.usage.output,
            cached_input: completion.usage.cached_input,
        });
        self.rounds += 1;
        // The per-round record of what the model actually did, at `info` because
        // it is the backbone of every "what happened in that turn?" question.
        // `tool_calls = 0` on round 1 is the one shape worth naming: the model
        // answered without touching anything, so whatever the reply asserts
        // about the world it made up.
        //
        // The token fields are also the only honest way to tune the context
        // knobs: `max_history_bytes` and `max_turn_result_bytes` are *byte*
        // budgets, and bytes are a poor proxy for tokens (CJK spends ~3 bytes
        // per token, code closer to 3.5). `cached` is the payoff of the
        // prefix-cache work in `assemble` — a round where it stays near zero
        // across a tool loop means the prefix is being invalidated and something
        // upstream broke the render invariant.
        let shape = round_shape(&completion.blocks);
        tracing::info!(
            round = self.rounds,
            provider = self.provider_name,
            model = %self.turn.model,
            tool_calls = shape.tool_calls,
            text_chars = shape.text_chars,
            reasoning = shape.reasoning,
            elapsed_ms = started.elapsed().as_millis() as u64,
            input = completion.usage.input,
            output = completion.usage.output,
            cached = completion.usage.cached_input,
            turn_input = self.usage.input,
            turn_output = self.usage.output,
            "model round completed"
        );
        self.record(vec![SessionEventKind::AssistantRound(
            AssistantRoundEvent {
                turn_id: self.turn_id(),
                round: self.rounds as u32,
                response_id: completion.id.clone().unwrap_or_default(),
                blocks: serde_json::to_value(&completion.blocks).unwrap_or(Value::Null),
                tokens_in: completion.usage.input,
                tokens_out: completion.usage.output,
                tokens_cached: completion.usage.cached_input,
            },
        )])
        .await;
        let step = blocks_to_step(&completion.blocks);
        self.history.push(Turn::Assistant {
            id: completion.id,
            blocks: completion.blocks,
        });
        Ok(step)
    }

    /// One provider round-trip over the history as it currently stands. Split out
    /// of [`run`] so an overflow can re-issue the identical call against a
    /// reclaimed history.
    ///
    /// [`run`]: Self::run
    async fn complete_round(&self) -> Result<Completion, LlmError> {
        // Bridge the domain sink onto the provider layer's callback. Built per
        // round rather than held, so a retry re-streams into the same watcher
        // without the provider layer ever learning what a session is.
        let forward = self.deltas.as_ref().map(|sink| {
            let sink = sink.clone();
            move |delta: Delta<'_>| match delta {
                Delta::Text(text) => sink.text(text),
                Delta::Reasoning(text) => sink.reasoning(text),
            }
        });
        let forward = forward
            .as_ref()
            .map(|f| f as &(dyn Fn(Delta<'_>) + Send + Sync));
        with_timeout(
            self.timeout,
            with_retry(|| {
                self.client.complete(
                    &self.turn.model,
                    &self.turn.preamble,
                    &self.history,
                    &self.tools,
                    self.turn.extra.as_ref(),
                    forward,
                )
            }),
        )
        .await
    }

    /// Reclaim context after an overflow, in place, on this turn's in-memory
    /// history only — the stored transcript is never touched, so a degrade
    /// costs the model's working set for the rest of this turn and nothing
    /// afterwards.
    ///
    /// Two steps, cheapest first: shrink the tool results this turn accumulated
    /// (the usual cause — a turn that read several large files), and only if
    /// that reclaims nothing, drop the oldest half of the replayed history (the
    /// case where the turn was already too big before it made a single call).
    /// Returns whether anything was actually reclaimed; `false` means there is
    /// nothing left to give and the caller must surface the failure.
    ///
    /// At most once per turn — see [`TurnLoop::degraded`].
    fn degrade_for_overflow(&mut self) -> bool {
        if self.degraded {
            return false;
        }
        self.degraded = reclaim_context(&mut self.history);
        self.degraded
    }
}

/// Shrink `history` in place, returning whether anything was reclaimed. Free
/// function rather than a method so the policy can be tested without standing
/// up a provider client — see [`TurnLoop::degrade_for_overflow`] for what it is
/// for.
fn reclaim_context(history: &mut Vec<Turn>) -> bool {
    let mut reclaimed = false;
    for turn in history.iter_mut() {
        let Turn::User(blocks) = turn else {
            continue;
        };
        for block in blocks.iter_mut() {
            if let UserBlock::ToolResult { text, .. } = block
                && text.len() > OVERFLOW_TOOL_RESULT_KEEP * 2
            {
                *text = head_tail(text, OVERFLOW_TOOL_RESULT_KEEP);
                reclaimed = true;
            }
        }
    }
    if reclaimed {
        return true;
    }
    // Nothing bulky to shrink: the weight is in the replayed conversation
    // itself. Drop the older half, keeping the window opening on a user
    // message (a leading assistant message is rejected outright by some
    // providers).
    let cut = history.len() / 2;
    if cut == 0 {
        return false;
    }
    let mut rest = history.split_off(cut);
    // The cut can land inside a tool round, so keep peeling until the window
    // opens on real user text. A leading assistant message is rejected
    // outright by some providers, and a tool result whose function_call went
    // with the dropped half is an orphan strict providers reject the same way
    // (DeepSeek: 400). The two strips alternate because each can create the
    // other's condition: removing an assistant turn with tool calls orphans
    // the results right after it.
    while let Some(first) = rest.first_mut() {
        match first {
            Turn::Assistant { .. } => {
                rest.remove(0);
            }
            Turn::User(blocks) => {
                // Any tool result this far forward pairs with an assistant
                // turn strictly before it — dropped by the cut or by a prior
                // iteration either way. Plain text (the user's ask, or a
                // mid-turn interjection) is kept.
                blocks.retain(|block| !matches!(block, UserBlock::ToolResult { .. }));
                if blocks.is_empty() {
                    rest.remove(0);
                } else {
                    break;
                }
            }
        }
    }
    if rest.is_empty() {
        // Dropping would leave nothing to send; let the turn fail honestly
        // instead of sending an empty request.
        return false;
    }
    *history = rest;
    true
}

/// The provider cache key for a turn: the backend's declared prefix family when
/// it has one, else the session. See [`ProviderLlm::model_for`] for why the two
/// differ.
fn cache_key(family: Option<&str>, session_id: &str) -> String {
    format!("komo:{}", family.unwrap_or(session_id))
}

/// `s` shortened to its first and last `keep` bytes with a marker between,
/// cut on char boundaries. Used only for the overflow degrade.
fn head_tail(s: &str, keep: usize) -> String {
    let head_end = floor_char_boundary(s, keep);
    let tail_start = ceil_char_boundary(s, s.len() - keep);
    format!(
        "{}\n\n…[{} bytes elided to fit the context window; the full result was \
         already delivered earlier this turn]…\n\n{}",
        &s[..head_end],
        tail_start - head_end,
        &s[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut at: usize) -> usize {
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_char_boundary(s: &str, mut at: usize) -> usize {
    while at < s.len() && !s.is_char_boundary(at) {
        at += 1;
    }
    at
}

#[async_trait]
impl TurnDriver for TurnLoop {
    async fn first(&mut self) -> anyhow::Result<Step> {
        match std::mem::replace(&mut self.start, TurnStart::Started) {
            TurnStart::Prompt(prompt) => self.run(prompt).await,
            TurnStart::Continue => self.complete_committed().await,
            TurnStart::Final(text) => Ok(Step::Final(text)),
            TurnStart::Replay { calls, slots } => {
                self.replayed = slots;
                // No narration: the text that went with these calls is already
                // in history on the round that issued them, and repeating it
                // would show the user the same sentence twice.
                Ok(Step::ToolCalls {
                    calls,
                    text: String::new(),
                })
            }
            TurnStart::Started => anyhow::bail!("turn driver already started"),
        }
    }

    async fn step(
        &mut self,
        results: Vec<ToolOutcome>,
        interjected: Option<String>,
    ) -> anyhow::Result<Step> {
        // One user message carrying every tool result. A komo tool's model-facing
        // result is plain text by contract (`domain::tool::ToolOutput::text`), so
        // each goes over as one text payload — no sniffing for an image or
        // multipart envelope.
        let mut blocks: Vec<UserBlock> = results
            .into_iter()
            .map(|r| UserBlock::ToolResult {
                id: r.id,
                call_id: r.call_id,
                text: r.content,
            })
            .collect();
        if !self.replayed.is_empty() {
            blocks = fill_replay_slots(std::mem::take(&mut self.replayed), blocks);
        }
        // What the user said while this round ran, appended to the same user
        // message as a plain text block — after the results, so the model reads
        // the outcome first and the new instruction last (the position it acts
        // on). Labelled, or a bare sentence next to tool output reads as data.
        let interjected_text = interjected.clone();
        if let Some(text) = interjected {
            blocks.push(UserBlock::Text(format!("{INTERJECTION_PREFIX}{text}")));
        }
        if blocks.is_empty() {
            anyhow::bail!("no tool results to send back");
        }
        // The tool results are already in the log — the executor appends one
        // `tool/call-settled` as each call settles. What is *not* yet there is
        // anything the user said mid-turn: record it at the moment it enters
        // history, not at turn end, or a turn that fails after acting on it
        // loses it entirely.
        if let Some(text) = interjected_text {
            self.record(vec![SessionEventKind::UserMessage(UserMessageEvent {
                turn_id: self.turn_id(),
                content: text,
                source: MessageSource::Injected,
                surface: SurfacePlacement::append(),
            })])
            .await;
        }
        self.run(Turn::User(blocks)).await
    }

    async fn nudge(&mut self, text: String) -> anyhow::Result<Option<Step>> {
        // Recorded before the round it asks for, for the same reason an
        // interjection is: a turn that fails after the model has read it must
        // not lose what the model was reacting to — and a resume rebuilds the
        // live history from exactly these events.
        self.record(vec![SessionEventKind::UserMessage(UserMessageEvent {
            turn_id: self.turn_id(),
            content: text.clone(),
            source: MessageSource::Runtime,
            surface: SurfacePlacement::append(),
        })])
        .await;
        Ok(Some(
            self.run(Turn::User(vec![UserBlock::Text(text)])).await?,
        ))
    }

    fn usage(&self) -> TokenUsage {
        self.usage
    }
    fn memories(&self) -> RecalledMemories {
        self.memories.clone()
    }
}

/// What one round's blocks amounted to, for the log: counts and a flag, never
/// content. `tool_calls` is the field an incident is read on — a first round
/// with none means the model answered out of its own head.
#[derive(Debug, Default, PartialEq)]
struct RoundShape {
    tool_calls: usize,
    text_chars: usize,
    reasoning: bool,
}

fn round_shape(blocks: &[AssistantBlock]) -> RoundShape {
    let mut shape = RoundShape::default();
    for block in blocks {
        match block {
            AssistantBlock::ToolCall { .. } => shape.tool_calls += 1,
            AssistantBlock::Text(t) => shape.text_chars += t.chars().count(),
            AssistantBlock::Reasoning(_) => shape.reasoning = true,
        }
    }
    shape
}

/// Split a model's assistant turn into komo's [`Step`]: any tool call makes it
/// a [`Step::ToolCalls`]; otherwise the concatenated text is the final answer.
/// Reasoning blocks are ignored for control flow (the driver still echoes them
/// back into history verbatim).
///
/// Text found *alongside* tool calls travels with them rather than being dropped:
/// it is the model narrating what it is about to do, which is the only account of
/// its reasoning a watcher gets and the honest thing to fall back on if the round
/// budget ends the turn early.
fn blocks_to_step(blocks: &[AssistantBlock]) -> Step {
    let mut calls = Vec::new();
    let mut text = String::new();
    for block in blocks {
        match block {
            AssistantBlock::ToolCall {
                id,
                call_id,
                name,
                args,
            } => calls.push(ToolCallReq {
                id: id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                args: args.clone(),
            }),
            AssistantBlock::Text(t) => text.push_str(t),
            AssistantBlock::Reasoning(_) => {}
        }
    }
    if calls.is_empty() {
        if let Some(marker) = FABRICATED_CALL_MARKERS
            .iter()
            .find(|m| text.contains(**m))
            .filter(|_| !text.is_empty())
        {
            warn!(
                marker,
                "the model wrote tool-call syntax as prose and issued no call — \
                 the answer it is about to give is very likely fabricated"
            );
        }
        Step::Final(text)
    } else {
        Step::ToolCalls { calls, text }
    }
}

/// Tool-call syntax that must never appear in a model's *prose*. Finding it in a
/// round that issued no call means the model narrated a tool call instead of
/// making one, and the answer built on it is invented — the failure that led here
/// had a turn quoting a plausible JSON result for a command it never ran, with an
/// empty ledger and nothing in any log to say so. Detection cannot be a hard error
/// (the strings are model-specific and a legitimate reply could quote one while
/// discussing tooling), so this only leaves a breadcrumb: `komo logs` now names
/// the failure that otherwise has to be reconstructed from an empty run ledger.
///
/// The last entry is komo's own digest fence (`domain::run::tool_digest`): the
/// model emitting it means it is echoing the shape of its replayed history rather
/// than acting.
const FABRICATED_CALL_MARKERS: [&str; 5] = [
    "tool▁calls▁begin",
    "｜DSML｜",
    "<|tool_calls_begin|>",
    "<tool_call>",
    "<previous_turn_tools>",
];

/// Build an LLM client covering every provider the configured `models` menu
/// spans, exposing `tools` via function calling.
///
/// With a single-provider menu this is exactly one backend. With a
/// cross-provider one it is a [`RoutingLlm`] over one backend per provider, and
/// a session's qualified model id (`deepseek:deepseek-chat`) selects among them —
/// so switching provider is the same mechanism as switching model, decided per
/// turn off the session.
///
/// `preamble` is a factory (see [`PreambleFn`]) invoked once per turn to
/// (re)assemble the system prompt — typically wrapping a
/// [`crate::system_prompt::SystemPromptBuilder`]. `injections` is what this
/// runtime may add to a turn's user message (see [`TurnInjections`]).
pub fn build_llm(
    config: &ModelConfig,
    tools: Option<&komo_services::tool_execution::ToolExecutor>,
    preamble: PreambleFn,
    injections: TurnInjections,
    cache_family: Option<&str>,
) -> anyhow::Result<Arc<dyn LlmClient>> {
    let providers = config.menu_providers();
    // The common case: everything on the menu runs on one provider, so there is
    // nothing to route between.
    if providers.len() < 2 {
        return build_provider_llm(config, tools, preamble, injections, cache_family);
    }

    let mut by_provider = Vec::with_capacity(providers.len());
    for provider in providers {
        // Each backend's own default model is the first menu entry naming it —
        // for the configured provider that is `model` itself (the resolver force-
        // includes it first), so the default backend keeps its exact identity.
        let default_model = config
            .menu()
            .into_iter()
            .find(|entry| entry.provider == provider)
            .map(|entry| entry.model)
            .unwrap_or_else(|| provider.default_model().to_string());
        let scoped = config.for_provider(provider, default_model);
        by_provider.push((
            provider,
            build_provider_llm(
                &scoped,
                tools,
                preamble.clone(),
                injections.clone(),
                cache_family,
            )?,
        ));
    }
    Ok(Arc::new(RoutingLlm {
        by_provider,
        default_provider: config.provider,
        // The config's *own* model may be qualified — an aux/memory variant
        // naming `codex:…` on a DeepSeek conversation — and every aux caller
        // hands this router a session with no model of its own, so that id, not
        // `provider`, is where those turns belong.
        own_provider: config.own_provider(),
    }))
}

/// Build the backend for exactly one provider.
fn build_provider_llm(
    config: &ModelConfig,
    tools: Option<&komo_services::tool_execution::ToolExecutor>,
    preamble: PreambleFn,
    injections: TurnInjections,
    cache_family: Option<&str>,
) -> anyhow::Result<Arc<dyn LlmClient>> {
    // A missing API key degrades instead of failing construction: a fresh
    // install (first Docker boot, pre-`komo init`) must still bring the
    // gateway up — channels serve, pairing works — while every LLM call
    // reports the fix. Config resolution records the matching warning.
    if config.provider.uses_api_key() && config.api_key.is_empty() {
        return Ok(Arc::new(UnconfiguredLlm {
            message: format!(
                "{} is not set (required for {:?}). Add it to ~/.komo/.env \
                 (run `komo init` to scaffold one) or the container \
                 environment, then restart the gateway.",
                config.provider.api_key_var(),
                config.provider
            ),
        }));
    }

    // Only the schemas cross to the provider: the executor stays the single
    // dispatcher, so there is exactly one execution semantics (retry/ledger/cap)
    // for every tool call.
    // The catalog, not a copy of its schemas: `ProviderLlm::tool_schemas`
    // renders it per turn, so a tool mounted after wiring is one the model can
    // see without a restart.
    let tool_catalog = tools.map(|executor| executor.catalog().clone());

    let wire = wire_for(config.provider);
    // Auth and the static headers are resolved together because Codex's headers
    // depend on its credentials (the account id rides in one of them).
    let (auth, headers) = match config.provider {
        // Codex authenticates from the Codex CLI's OAuth file, and the token
        // rotates hourly — so it is resolved per request rather than captured
        // here. Missing/broken credentials degrade like a missing API key: the
        // gateway must boot (a fresh box, or a container without ~/.codex
        // mounted) instead of crash-looping, with every LLM call reporting the
        // fix as the turn's reply.
        Provider::Codex => match CodexAuth::load() {
            Ok(auth) => {
                let headers = codex_static_headers(auth.account_id());
                (Auth::Dynamic(auth), headers)
            }
            Err(error) => {
                tracing::warn!(%error, "Codex credentials unavailable; LLM degraded");
                return Ok(Arc::new(UnconfiguredLlm {
                    // The loader's error already names every accepted path and
                    // how to produce the file; only the restart is news here.
                    message: format!(
                        "Codex credentials unavailable: {error:#}. Restart the gateway \
                         once the login is in place."
                    ),
                }));
            }
        },
        // Same story as Codex, one directory over: the login belongs to the
        // Claude Code CLI, the access token rotates within the day, and the
        // request has to present Claude Code's own identity (user agent, betas,
        // `x-app`) or Anthropic answers 500s. An absent login degrades rather
        // than aborting boot, for the same reason a missing API key does.
        Provider::ClaudeCode => match ClaudeCodeAuth::load() {
            Ok(auth) => (Auth::Dynamic(auth), claude_code::static_headers()),
            Err(error) => {
                tracing::warn!(%error, "Claude Code credentials unavailable; LLM degraded");
                return Ok(Arc::new(UnconfiguredLlm {
                    // The loader's error already names every accepted path and
                    // how to produce the login; only the restart is news here.
                    message: format!(
                        "Claude Code credentials unavailable: {error:#}. Restart the gateway \
                         once the login is in place."
                    ),
                }));
            }
        },
        // Anthropic versions its API by header, not by URL.
        Provider::Anthropic => (
            Auth::ApiKey(config.api_key.clone()),
            vec![(
                "anthropic-version".to_string(),
                komo_provider::messages::ANTHROPIC_VERSION.to_string(),
            )],
        ),
        _ => (Auth::Bearer(config.api_key.clone()), Vec::new()),
    };

    let endpoint = Endpoint {
        url: endpoint_url(config.provider, config.base_url.as_deref()),
        auth,
        headers,
        client: reqwest::Client::new(),
    };

    Ok(Arc::new(ProviderLlm {
        client: Arc::new(ProviderClient { endpoint, wire }),
        tools: tool_catalog,
        default_model: config.model.clone(),
        provider: config.provider,
        default_effort: config.effort.clone(),
        cache_family: cache_family.map(str::to_string),
        preamble,
        max_history_messages: config.max_history_messages,
        max_history_bytes: config.max_history_bytes,
        injections,
        // Cap each completion so a hung provider request fails the turn instead
        // of wedging it in `running`. `0` = off.
        timeout: (config.llm_timeout_secs > 0)
            .then(|| Duration::from_secs(config.llm_timeout_secs)),
    }))
}

/// Which wire protocol a provider speaks.
///
/// Four of the five are Responses; Anthropic serves no such endpoint, which is
/// the only reason komo carries a second codec at all.
fn wire_for(provider: Provider) -> Wire {
    match provider {
        Provider::Anthropic | Provider::ClaudeCode => Wire::Messages,
        Provider::DeepSeek | Provider::OpenAi | Provider::OpenRouter | Provider::Codex => {
            Wire::Responses
        }
    }
}

/// The completion endpoint for a provider.
///
/// `base_url` overrides the API root (config `base_url` — an OpenAI-compatible
/// proxy, a self-hosted gateway); the wire's path is appended to it, so callers
/// configure a root and never a full endpoint.
fn endpoint_url(provider: Provider, base_url: Option<&str>) -> String {
    let root = base_url.unwrap_or(match provider {
        Provider::DeepSeek => "https://api.deepseek.com/v1",
        Provider::OpenAi => "https://api.openai.com/v1",
        Provider::Anthropic => "https://api.anthropic.com/v1",
        Provider::OpenRouter => "https://openrouter.ai/api/v1",
        Provider::Codex => CODEX_BASE_URL,
        Provider::ClaudeCode => CLAUDE_BASE_URL,
    });
    let path = match wire_for(provider) {
        Wire::Responses => "responses",
        Wire::Messages => "messages",
    };
    format!("{}/{path}", root.trim_end_matches('/'))
}

/// Trim `prior` (the transcript before this turn's prompt) to the slice replayed
/// as model history, under two independent bounds.
///
/// Without a window, a long-lived chat session — telegram/feishu/wechat are keyed
/// by chat id and only cut on an explicit `/new` — resends its whole transcript
/// every turn. `max_messages` is the count bound (`0` = keep everything);
/// `max_bytes` is the size bound (`0` = no size limit), and it exists because a
/// count says nothing about volume: twenty messages of pasted build output
/// overflow a context that two hundred chat lines sit inside. Both trim from the
/// oldest end, so the stable system prompt and memory prefix are untouched and the
/// upstream prompt cache is unaffected.
fn window_history(prior: &[Message], max_messages: usize, max_bytes: usize) -> &[Message] {
    let mut window = match max_messages {
        0 => prior,
        n => &prior[prior.len().saturating_sub(n)..],
    };
    // Once the transcript is at the count cap, the naive cut advances every
    // turn (each turn pushes the oldest message out), so the replayed history
    // opens with different bytes every turn and the provider prompt cache
    // misses everything after the system prompt. Snap the cut forward to the
    // nearest *anchor* message instead: anchors are a deterministic property
    // of the message itself (a hash of its stored bytes), so consecutive
    // turns keep opening the window on the same message until the cap
    // genuinely passes it — the prefix then stays byte-identical for
    // ~[`WINDOW_ANCHOR_SPACING`] turns at a stretch, at the cost of a
    // slightly shorter window. No anchor in the window ⇒ keep the naive cut
    // (slides, but never under-delivers history).
    if max_messages > 0 && prior.len() >= max_messages {
        if let Some(offset) = window.iter().position(is_window_anchor) {
            window = &window[offset..];
        }
    }
    if max_bytes > 0 {
        let size = |m: &Message| m.content.len() + m.tool_note.len();
        let mut total: usize = window.iter().map(size).sum();
        let mut start = 0;
        // A single message over the whole budget still gets dropped (the loop runs
        // to the end): sending it would blow the context on its own, and the turn's
        // own prompt is never part of this slice, so the model is not left mute.
        while start < window.len() && total > max_bytes {
            total -= size(&window[start]);
            start += 1;
        }
        window = &window[start..];
    }
    // The transcript strictly alternates user/assistant, so either cut can open on
    // an assistant message; drop it so history starts on a user turn (Anthropic
    // rejects a leading assistant message). Applied after both bounds, since
    // either one can be the cut that lands there.
    if window.first().is_some_and(|m| m.role == Role::Assistant) {
        window = &window[1..];
    }
    window
}

/// Average anchor spacing, in *user* messages: a user message is an anchor
/// when its hash lands in `1/WINDOW_ANCHOR_SPACING` of the space, so the
/// window start advances roughly once per this many turns instead of every
/// turn. Larger = warmer cache but a shorter average window (the cut snaps
/// further forward); 6 trades ≤ ~12 messages of tail for a prefix that holds
/// still ~6 turns at a time.
const WINDOW_ANCHOR_SPACING: u64 = 6;

/// Whether `m` is a window-anchor message (see [`window_history`]): a user
/// message whose FNV-1a hash of its stored bytes selects it. Keyed on stored
/// fields only (content + timestamp — never the render-time tool-note
/// decision), so every turn, in every process, agrees on which messages are
/// anchors. User role so a snapped window always opens on a user message,
/// which providers require anyway.
fn is_window_anchor(m: &Message) -> bool {
    if m.role != Role::User {
        return false;
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in m
        .content
        .as_bytes()
        .iter()
        .chain(m.timestamp.to_le_bytes().iter())
    {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    h % WINDOW_ANCHOR_SPACING == 0
}

/// Map a komo message into provider chat-history turns. The system prompt is
/// supplied via the preamble, and tool outputs are folded into the following
/// assistant reply, so both `System` and `Tool` roles are skipped here.
///
/// A note-bearing assistant message renders as **two** turns: the reply itself,
/// then the tool digest as a *user* turn. The digest is what lets the next turn
/// know tools ran at all, but it must not look like assistant output — rendered
/// inside the assistant's own text (as it was until this changed), it reads as a
/// worked example of an assistant narrating tool calls in prose, and a model that
/// copies the shape gets a turn that reports invented commands and invented
/// results with nothing in the ledger. Attributing it to the other side of the
/// conversation, plus `tool_digest`'s fence, is what breaks the pattern. The
/// user-visible `content` is untouched either way — it stays exactly what every
/// client renders.
///
/// Two user turns in a row can result (digest, then the next real user message).
/// Both wires handle it: `messages` merges same-role neighbours because Anthropic
/// demands strict alternation, and `responses` flattens turns into an input list
/// that never required it.
///
/// **The rendering is a pure function of the message.** Nothing here may depend
/// on where the message sits in the window (see [`ProviderLlm::assemble`]) — which
/// is why the digest becomes its own turn rather than a prefix on the *following*
/// user message: `window_history` always cuts so the window opens on a user
/// message, so a prefixed digest would appear or vanish depending on whether its
/// assistant survived the cut. Emitted as a pair, the two turns live and die
/// together. This also used to carry the note only for the last three
/// note-bearing turns, which meant every tool turn silently rewrote an older
/// message's bytes and cost the provider prefix cache everything from ~3 turns
/// back, every turn. Always attaching is both simpler and cheaper: the digest is
/// already capped when it is written (`domain::run::tool_digest`), and
/// [`window_history`]'s byte budget has always counted `tool_note` for every
/// message in the window regardless of whether it was rendered — so the
/// accounting matches what is actually sent.
fn to_turns(msg: &Message) -> Vec<Turn> {
    match msg.role {
        Role::User => vec![Turn::user(msg.content.clone())],
        Role::Assistant if !msg.tool_note.is_empty() => vec![
            Turn::assistant(msg.content.clone()),
            Turn::user(msg.tool_note.clone()),
        ],
        Role::Assistant => vec![Turn::assistant(msg.content.clone())],
        Role::System | Role::Tool => Vec::new(),
    }
}

#[cfg(test)]
mod tests;
