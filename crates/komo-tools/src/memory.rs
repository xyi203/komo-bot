use std::sync::Arc;

use async_trait::async_trait;
use komo_services::memory_query::MemoryQueryService;
use komo_services::tool_execution::SessionContext;
use serde::Deserialize;
use serde_json::{Value, json};

use komo_core::domain::{
    context::ToolContext,
    memory::{
        EvidenceRelation, Memory, MemoryConfidence, MemoryContext, MemoryKind, MemoryRepository,
        ScoredMemory, parse_memory_kind,
    },
    tool::{Tool, ToolError, ToolOutput, parse_args},
};

/// Default cap on search results.
const SEARCH_LIMIT: usize = 10;

/// How many possibly-related existing memories a `save` reports back. Enough to
/// surface a contradiction, small enough not to bloat the tool result.
const RELATED_LIMIT: usize = 3;

#[derive(Deserialize)]
struct MemoryArgs {
    action: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    query: Option<String>,
    /// Optional TTL in days (action=save).
    #[serde(default)]
    expiry_days: Option<i64>,
    /// Ids of existing memories the new fact replaces (action=save); superseded
    /// in the same call so an outdated fact never coexists with its successor.
    #[serde(default)]
    supersedes: Option<Vec<String>>,
}

impl MemoryArgs {
    /// Some models fill every optional schema field with a placeholder instead
    /// of omitting it (`"kind": ""`, `"query": ""`). An empty string is never a
    /// meaningful value for any of these, so normalize it to absent — otherwise
    /// `parse_memory_kind("")` silently picks a category the model did not mean.
    fn normalized(mut self) -> Self {
        for field in [&mut self.text, &mut self.kind, &mut self.query] {
            if field.as_deref().is_some_and(|s| s.trim().is_empty()) {
                *field = None;
            }
        }
        // Same placeholder problem in list form: `"supersedes": []` or `[""]`.
        if let Some(ids) = &mut self.supersedes {
            ids.retain(|id| !id.trim().is_empty());
            if ids.is_empty() {
                self.supersedes = None;
            }
        }
        self
    }
}

/// Long-term, cross-session memory. The model `save`s facts and `search`es
/// them (scoped to the current chat/session); it does not curate the library.
/// Governance — promote, reject, archive, edit — belongs to the operator
/// (`komo memory`) and to Dream, which rule on a claim by the evidence for it.
/// A model that could promote its own candidate would be corroborating itself,
/// which is the one thing the truth/utility split exists to prevent.
/// L1 lives in the operator-edited MEMORY.md file. Storage lives behind
/// [`MemoryRepository`] — the same store the reviewer writes to.
///
/// Searching goes through the same [`MemoryQueryService`] as automatic recall, so
/// what the model can find by asking is exactly what it can be handed
/// unprompted — including candidates and cross-language matches.
pub struct MemoryTool {
    memories: Arc<dyn MemoryRepository>,
    query: Arc<MemoryQueryService>,
}

impl MemoryTool {
    pub fn new(memories: Arc<dyn MemoryRepository>, query: Arc<MemoryQueryService>) -> Self {
        Self { memories, query }
    }

    /// Look up a memory `supersedes` names. A missing / unknown id is the
    /// model's mistake to fix, so both map to [`ToolError::InvalidInput`]
    /// rather than a retryable failure.
    async fn require(&self, id: &Option<String>) -> Result<Memory, ToolError> {
        let id = id.as_deref().ok_or_else(|| {
            ToolError::InvalidInput("`id` is required for this action".to_string())
        })?;
        self.memories
            .get(id)
            .await?
            .ok_or_else(|| ToolError::InvalidInput(format!("no memory with id `{id}`")))
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    /// Recall already injects the relevant memories into every turn's prompt
    /// (L3, `MemoryEnricher`), and the reviewer extracts new ones after the
    /// turn ends. What is left for the tool is an explicit "remember this" and
    /// an explicit lookup — rare enough to pay a discovery round for.
    fn advertised(&self) -> bool {
        false
    }

    fn description(&self) -> &'static str {
        "Long-term memory across sessions: `save` stores a fact, `search` \
         retrieves. Never store what goes stale within a week: task progress, \
         PR numbers, commit SHAs."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["save", "search"],
                    "description": "The memory operation to perform."
                },
                "text": { "type": "string", "description": "Fact to store (action=save)." },
                "kind": {
                    "type": "string",
                    "enum": ["profile", "preference", "feedback", "project", "person", "fact", "decision", "reference"],
                    "description": "Category (action=save, default profile)."
                },
                "query": { "type": "string", "description": "Search term (action=search); matched by meaning as well as by wording, so retrying different wording helps." },
                "expiry_days": { "type": "integer", "description": "Optional TTL in days (action=save); omit for permanent." },
                "supersedes": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Ids of stored memories this fact replaces (action=save) — pass them when it contradicts one; they retire as history."
                }
            },
            "required": ["action"]
        })
    }

    async fn call(&self, input: Value, tool_ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: MemoryArgs = parse_args::<MemoryArgs>(&input)?.normalized();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        // Scope comes from the *explicit* per-call context (tool trait v2), not
        // the ambient task-local: `memory` was the last tool reading that seam.
        let scope = memory_context(&tool_ctx.session);

        match args.action.as_str() {
            "save" => {
                let text = args.text.ok_or_else(|| {
                    ToolError::InvalidInput("`text` is required for action=save".to_string())
                })?;
                let kind = args
                    .kind
                    .as_deref()
                    .map(parse_memory_kind)
                    .unwrap_or(MemoryKind::Profile);
                // Validate every superseded id *before* any write, so a typo'd
                // id can never leave the save half-applied.
                let mut superseded: Vec<Memory> = Vec::new();
                for id in args.supersedes.as_deref().unwrap_or_default() {
                    superseded.push(self.require(&Some(id.clone())).await?);
                }
                // Look for possibly-conflicting memories *before* the save, so the
                // new memory cannot be reported against itself. Through the shared
                // query service, so this catches a cross-language near-duplicate —
                // exactly the case a lexical scan misses and the user then has to
                // correct by hand.
                let related = self
                    .query
                    .lookup(&scope, &text, RELATED_LIMIT + superseded.len())
                    .await
                    .map_err(ToolError::Failed)?;

                let mut memory = Memory::new(kind, text);
                // An explicit user save is the highest trust tier.
                memory.confidence = MemoryConfidence::UserWritten;
                // …and it is a confirmation, not merely an origin: the user asked
                // for this to be remembered. Stamped so the freshness clock starts
                // now rather than at "never vouched for", and recorded as evidence
                // so provenance is uniform across every write path.
                memory.last_confirmed_at = Some(now);
                memory.record_evidence(
                    &tool_ctx.session.session_id,
                    // An explicit save is its own occasion; the turn it was made
                    // in is the narrowest id available for one.
                    tool_ctx
                        .run
                        .as_ref()
                        .map(|r| r.run_id.as_str())
                        .unwrap_or(&tool_ctx.session.session_id),
                    EvidenceRelation::Supports,
                    &memory.content.clone(),
                    now,
                );
                // Scope to the current chat so a channel fact does not leak elsewhere.
                memory.scope = scope.write_scope();
                if let Some(days) = args.expiry_days.filter(|d| *d > 0) {
                    memory.expires_at = Some(now + days * 86_400);
                }
                self.memories.save(&memory).await?;
                // `memory` is the one state-changing tool that never consults
                // the approver — saving a fact the user just stated is not an
                // action anybody should have to approve — so it marks the turn
                // itself. Without this, a turn whose only work was a save is
                // read-only as far as the runtime can tell, and its honest
                // "已保存" reads as a claim about something it never did.
                if let Some(run) = &tool_ctx.run {
                    run.note_effectful();
                }
                let mut out = format!("Saved memory {}.", memory.id);

                let superseded_ids: Vec<String> = superseded.iter().map(|m| m.id.clone()).collect();
                if !superseded.is_empty() {
                    for mut old in superseded.drain(..) {
                        // `supersede`, not `Archived`: the two express different
                        // things, and conflating them loses both. Archived means
                        // "retired, nobody needed it"; superseded means "was true,
                        // this replaced it" — it carries a forward link, stays
                        // queryable as history ("what did I use to prefer"), and is
                        // already barred from injection by `is_injectable`. It is
                        // also what the reviewer-side consolidation seam writes, so
                        // the explicit and the automated path now say the same
                        // thing about the same event.
                        old.supersede(&memory.id, now);
                        self.memories.save(&old).await?;
                    }
                    out.push_str(&format!("\nSuperseded: {}.", superseded_ids.join(", ")));
                }

                // Surface possibly related existing memories so a contradiction is
                // caught while the model is still in context. The consolidation seam
                // only sees a conversation *after* it ends, so on this path the
                // model is the conflict detector.
                let related: Vec<&ScoredMemory> = related
                    .iter()
                    .filter(|h| !superseded_ids.contains(&h.memory.id))
                    .take(RELATED_LIMIT)
                    .collect();
                if !related.is_empty() {
                    out.push_str(
                        "\nPossibly related existing memories — if the new fact replaces one, \
                         save again with `supersedes: [id]`:",
                    );
                    for hit in &related {
                        out.push('\n');
                        out.push_str(&render_one(&hit.memory));
                    }
                }

                Ok(ToolOutput::text(out).with_structured(json!({ "id": memory.id })))
            }
            "search" => {
                let text = args.query.ok_or_else(|| {
                    ToolError::InvalidInput("`query` is required for action=search".to_string())
                })?;
                let (hits, arm) = self
                    .query
                    .lookup_reported(&scope, &text, SEARCH_LIMIT)
                    .await
                    .map_err(ToolError::Failed)?;
                // "(no matches)" is an answer about the library. With the
                // semantic half down it is an answer about the backend, and the
                // model cannot tell the two apart unless told — so say it here,
                // where it is reading the result.
                let mut out = render_scored(&hits);
                if arm.is_degraded() {
                    out.push_str(SEARCH_DEGRADED_NOTE);
                }
                Ok(ToolOutput::text(out).with_title(format!("{} matches", hits.len())))
            }
            other => Err(ToolError::InvalidInput(format!(
                "unknown action `{other}` (expected save/search)"
            ))),
        }
    }
}

/// The memory context for this call: the turn's own session, plus its
/// correspondent's channel when the turn has one (a chat turn does, a local one
/// does not).
fn memory_context(session: &SessionContext) -> MemoryContext {
    MemoryContext::new(&session.session_id, session.channel.as_ref())
}

fn render_one(m: &Memory) -> String {
    // Belief is shown only when it is *not* `current`, so the common line stays
    // short — but a contested or superseded memory can never be read as an
    // ordinary fact, which is the whole point of it being searchable at all.
    let belief = if m.is_injectable() {
        String::new()
    } else {
        format!("/{}", m.belief.as_str())
    };
    let mut line = format!(
        "[{}/{}/{}{}] {}: {}",
        m.kind.as_str(),
        m.status.as_str(),
        m.scope.type_str(),
        belief,
        m.id,
        m.content
    );
    if !m.superseded_by.is_empty() {
        line.push_str(&format!(" (replaced by {})", m.superseded_by));
    }
    if !m.source.is_empty() {
        line.push_str(&format!(" (from {})", m.source));
    }
    line
}

/// Appended to a `search` result whose semantic half did not run. Same fact as
/// the recall block's note and for the same reason: lexical matching compares
/// CJK bigrams against ASCII words and can never equate them, so a degraded
/// search across languages returns nothing — which is indistinguishable from
/// there being nothing to return.
const SEARCH_DEGRADED_NOTE: &str = "\n\n(Semantic search was unavailable for this query — \
    these are literal word-overlap matches only. A memory phrased in another language would \
    not appear. Do not report an empty result as proof that nothing is stored.)";

fn render_scored(hits: &[ScoredMemory]) -> String {
    if hits.is_empty() {
        return "(no matches)".to_string();
    }
    hits.iter()
        .map(|h| render_one(&h.memory))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_core::domain::memory::MemoryStatus;
    use komo_infra::persistence::db::Db;

    /// The real store on an in-memory db, one per test.
    async fn temp_tool() -> MemoryTool {
        let store: Arc<dyn MemoryRepository> =
            Arc::new(Db::connect("turso::memory:").await.expect("in-memory db"));
        // No embedding backend: the lexical arm alone, which is what a machine
        // with no Ollama running gets.
        let query = Arc::new(MemoryQueryService::new(store.clone()));
        MemoryTool::new(store, query)
    }

    /// A CLI-shaped session: global + session scope, no channel scope.
    fn ctx() -> ToolContext {
        crate::test_support::detached_ctx("cli:test")
    }

    #[tokio::test]
    async fn save_and_search_roundtrip() {
        let tool = temp_tool().await;

        tool.call(json!({ "action": "save", "text": "用户喜欢蓝色" }), &ctx())
            .await
            .unwrap();
        tool.call(
            json!({ "action": "save", "text": "项目用 Rust 写", "kind": "project" }),
            &ctx(),
        )
        .await
        .unwrap();

        let hit = tool
            .call(json!({ "action": "search", "query": "rust" }), &ctx())
            .await
            .unwrap()
            .text;
        assert!(hit.contains("Rust"));
        assert!(!hit.contains("蓝色"));
    }

    /// Search reaches candidates, not just active memories — the same set
    /// recall draws from. A candidate the reviewer wrote is exactly what the
    /// model has to be able to find in order to help settle it.
    #[tokio::test]
    async fn search_reaches_candidates() {
        let tool = temp_tool().await;
        let mut cand = Memory::new(MemoryKind::Fact, "user prefers rebase before push");
        cand.status = MemoryStatus::Candidate;
        tool.memories.save(&cand).await.unwrap();

        let out = tool
            .call(json!({ "action": "search", "query": "rebase" }), &ctx())
            .await
            .unwrap()
            .text;
        assert!(out.contains("rebase before push"));
    }

    /// The call shape observed from a model that fills every optional field
    /// with a placeholder (run-019fc562). An empty string is never a value
    /// here, so it must read as absent rather than as a filter or a category.
    #[tokio::test]
    async fn empty_string_args_are_treated_as_absent() {
        let tool = temp_tool().await;
        let mut cand = Memory::new(MemoryKind::Fact, "protoc lives in /opt/homebrew/bin");
        cand.status = MemoryStatus::Candidate;
        tool.memories.save(&cand).await.unwrap();

        let out = tool
            .call(
                json!({ "action": "search", "query": "protoc", "kind": "", "text": "" }),
                &ctx(),
            )
            .await
            .unwrap()
            .text;
        assert!(out.contains("protoc"));
    }

    /// The preference-evolution case: the new fact archives the outdated one in
    /// the same call, so the two never coexist in recall.
    #[tokio::test]
    async fn save_with_supersedes_archives_the_outdated_memory() {
        let tool = temp_tool().await;
        let mut old = Memory::new(MemoryKind::Preference, "User prefers Python for scripting");
        old.status = MemoryStatus::Active;
        tool.memories.save(&old).await.unwrap();

        let out = tool
            .call(
                json!({
                    "action": "save",
                    "text": "User mainly uses Rust for scripting now",
                    "kind": "preference",
                    "supersedes": [old.id]
                }),
                &ctx(),
            )
            .await
            .unwrap()
            .text;
        assert!(out.contains("Superseded"));
        assert!(out.contains(&old.id));
        // The superseded line must not double as a "possibly related" hint.
        assert!(!out.contains("Possibly related"));

        // Retired as *history*, not archived: no longer injectable, still
        // queryable, and pointing at what replaced it. Same mechanism the
        // reviewer-side consolidation seam uses.
        let retired = tool.memories.get(&old.id).await.unwrap().unwrap();
        assert_eq!(
            retired.belief,
            komo_core::domain::memory::BeliefState::Superseded
        );
        assert!(!retired.is_injectable());
        assert!(
            !retired.superseded_by.is_empty(),
            "history has to point at its replacement"
        );
    }

    /// A save that overlaps a stored fact reports it, so the model can catch a
    /// contradiction while still in context.
    #[tokio::test]
    async fn save_reports_possibly_related_existing_memories() {
        let tool = temp_tool().await;
        let mut old = Memory::new(MemoryKind::Preference, "User prefers Python for scripting");
        old.status = MemoryStatus::Active;
        tool.memories.save(&old).await.unwrap();

        let out = tool
            .call(
                json!({ "action": "save", "text": "User mainly uses Rust for scripting now" }),
                &ctx(),
            )
            .await
            .unwrap()
            .text;
        assert!(out.contains("Possibly related existing memories"));
        assert!(out.contains(&old.id));
        assert!(out.contains("Python"));
        // Nothing was archived — surfacing is a hint, not an action.
        assert_eq!(
            tool.memories.get(&old.id).await.unwrap().unwrap().status,
            MemoryStatus::Active
        );
    }

    /// An unrelated save stays quiet — the hint must not fire on every write.
    #[tokio::test]
    async fn save_with_no_overlap_reports_nothing_related() {
        let tool = temp_tool().await;
        let mut old = Memory::new(MemoryKind::Preference, "User prefers Python for scripting");
        old.status = MemoryStatus::Active;
        tool.memories.save(&old).await.unwrap();

        let out = tool
            .call(
                json!({ "action": "save", "text": "团队周会安排在星期二" }),
                &ctx(),
            )
            .await
            .unwrap()
            .text;
        assert!(!out.contains("Possibly related"));
    }

    /// A typo'd supersede id fails the whole call before any write — the new
    /// memory is not saved, nothing is archived.
    #[tokio::test]
    async fn save_with_unknown_supersede_id_writes_nothing() {
        let tool = temp_tool().await;
        let err = tool
            .call(
                json!({
                    "action": "save",
                    "text": "User mainly uses Rust now",
                    "supersedes": ["mem-nope"]
                }),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no memory with id"));
        assert!(tool.memories.list().await.unwrap().is_empty());
    }

    /// Placeholder shapes (`[]`, `[""]`) mean "no supersede", not an error.
    #[tokio::test]
    async fn empty_supersedes_placeholders_are_ignored() {
        let tool = temp_tool().await;
        let out = tool
            .call(
                json!({ "action": "save", "text": "用户喜欢蓝色", "supersedes": [""] }),
                &ctx(),
            )
            .await
            .unwrap()
            .text;
        assert!(out.contains("Saved memory"));
    }

    /// Governance is the operator's and Dream's, not the model's: a model that
    /// could promote its own candidate would corroborate itself, and one that
    /// could archive would settle by fiat what only evidence settles. The
    /// verdicts stay reachable through `komo memory`.
    #[tokio::test]
    async fn governance_actions_are_not_model_facing() {
        let tool = temp_tool().await;
        let m = Memory::new(MemoryKind::Fact, "ephemeral");
        tool.memories.save(&m).await.unwrap();

        for action in ["promote", "reject", "archive", "update", "list"] {
            let err = tool
                .call(json!({ "action": action, "id": m.id }), &ctx())
                .await
                .expect_err(action);
            assert!(
                matches!(err, ToolError::InvalidInput(_)),
                "`{action}` should be an unknown action, got {err:?}"
            );
        }
        assert_eq!(
            tool.memories.get(&m.id).await.unwrap().unwrap().status,
            m.status,
            "a refused governance action must not have changed anything"
        );
    }

    #[tokio::test]
    async fn the_model_facing_text_stays_short() {
        crate::test_support::assert_model_text_budget(&temp_tool().await);
    }
}
