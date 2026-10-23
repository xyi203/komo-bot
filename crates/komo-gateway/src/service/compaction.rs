//! 改写提示前缀在 Gateway 这一侧（§6）：把决策要的事实读齐，交给 `komo_agent` 的纯函数。
//! 压成摘要与把大结果换成短视图（衰减）共用这个决策点、同一本缓存账。
//!
//! 两个入口，同一套事实、同一个判断：
//!
//! - **装配时**（`GatewaySegments::segment`）：这一段开头就贴着窗口、或者缓存已经冷了——决定压就把摘要请求放进 `Segment.compaction`，loop 先压再跑；决定衰减
//!   就先落账、再重新装配（回放读到新的已衰减集合）；
//! - **一轮收尾之后**（[`GatewayPlanner`]，loop 经 `CompactionPlanner` 来问）：重新读一遍
//!   日志、按同一份模板装配出 provider 正看着的那份视图，再判一次。
//!
//! 配置每次判断时读当前快照，不随 Run 冻结：决策连同全部中间量记进 `context.compacted`，
//! 回放只认那条事件，不会因为配置改了而换一种读法。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use komo_agent::context::compaction::{
    CompactionSettings, Plan, PlanInput, plan, plan_branch, plan_overflow, plan_overflow_branch,
};
use komo_agent::context::history::{self, ReplayScope, ResolvedMessage};
use komo_agent::context::{
    AgentContext, ContextInput, InvocationContext, append_task_context, assemble, tree,
};
use komo_kernel::compaction::{
    CompactionJob, OnlineState, PROMPT_CACHE_TTL, Reshape, online_state,
};
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::fold::fold;
use komo_kernel::projection::ProjectionContext;
use komo_kernel::protocol::config::CompactionConfig;
use komo_kernel::traits::{Clock, LedgerError, ToolOutputStore};
use komo_kernel::types::agent::ProjectFile;
use komo_kernel::types::ids::{RunId, SessionId, ToolCallId};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::tool::{ToolDefinition, ToolNamespace};
use komo_runtime::agent::CompactionPlanner;
use komo_runtime::config::ConfigHolder;
use komo_store::PayloadStore;

use super::context_sources;
use super::ledgers::RoutedLedger;

/// `ContextInput` 里除了回放历史以外的那些：这一段装配时定下来，压缩判断重新装配时
/// 原样再用一次——系统提示因此与 provider 正看着的那份逐字相同。
#[derive(Clone)]
pub(crate) struct ContextTemplate {
    pub instructions: Option<String>,
    pub project_files: Vec<ProjectFile>,
    pub project_update: Option<Vec<ProjectFile>>,
    pub contract: Option<komo_kernel::types::delegate::DelegateContract>,
    pub workspace: PathBuf,
    pub tools: Vec<String>,
    pub mcp_servers: Vec<ToolNamespace>,
    pub memory: Option<String>,
    pub skills: Option<String>,
    pub invocation: InvocationContext,
    pub projection: ProjectionContext,
    /// fork 任务会话冻结的前缀（`docs/background-tasks.md` §2b）。
    pub fork: Option<Arc<komo_kernel::types::turn::ForkPrefix>>,
}

impl ContextTemplate {
    pub fn assemble(
        self,
        history: Vec<ResolvedMessage<'_>>,
        decayed: BTreeSet<ToolCallId>,
    ) -> AgentContext {
        assemble(ContextInput {
            instructions: self.instructions,
            project_files: self.project_files,
            project_update: self.project_update,
            contract: self.contract,
            workspace: self.workspace,
            tools: self.tools,
            mcp_servers: self.mcp_servers,
            history,
            decayed,
            memory: self.memory,
            skills: self.skills,
            invocation: self.invocation,
            projection: self.projection,
            fork: self.fork,
        })
    }
}

/// 当前配置里的 `[compaction]`（没接配置是 `None`）。衰减不受 `enabled` 管，所以关着的
/// 也要给：`enabled` 只决定要不要压成摘要。
pub(crate) fn current(config: Option<&Arc<ConfigHolder>>) -> Option<CompactionConfig> {
    config.map(|config| config.current().compaction.clone())
}

/// 这个 Session 的提示缓存是不是已经冷了：最后一条 `message.assistant`（上一次请求的
/// 完成）离 `now` 超过 [`PROMPT_CACHE_TTL`]。没请求过不算冷——还没有缓存可以失效。
pub(crate) fn cache_cold(events: &[Event], now: time::OffsetDateTime) -> bool {
    events
        .iter()
        .rev()
        .find(|event| matches!(event.payload, EventPayload::MessageAssistant(_)))
        .is_some_and(|event| now - event.ts > PROMPT_CACHE_TTL)
}

/// 按日志装配这条 Run 此刻的上下文：回放按它自己记下的已衰减集合选视图。
pub(crate) async fn assemble_context(
    events: &[Event],
    run: &RunId,
    thread: Option<&[RunId]>,
    template: ContextTemplate,
    payloads: &PayloadStore,
    outputs: Option<&dyn ToolOutputStore>,
) -> Result<AgentContext, LedgerError> {
    assemble_context_with_tools(
        events,
        run,
        thread,
        template,
        payloads,
        outputs,
        &mut Vec::new(),
    )
    .await
}

/// Same assembly for execution and compaction; replay system checkpoints into the
/// model schema without changing the executor's independently frozen capability.
pub(crate) async fn assemble_context_with_tools(
    events: &[Event],
    run: &RunId,
    thread: Option<&[RunId]>,
    template: ContextTemplate,
    payloads: &PayloadStore,
    outputs: Option<&dyn ToolOutputStore>,
    tools: &mut Vec<ToolDefinition>,
) -> Result<AgentContext, LedgerError> {
    if tree::enabled(events) {
        let projection = template.projection;
        let project_update = template.project_update.clone();
        let contract = template.contract.clone();
        let context = template.assemble(Vec::new(), BTreeSet::new());
        let mut context = assemble_tree_context(
            events,
            Some(run),
            thread,
            context,
            projection,
            payloads,
            outputs,
            tools,
        )
        .await?;
        if let Some(task) = events.iter().find(|event| {
            event.run.as_ref() == Some(run) && matches!(event.payload, EventPayload::RunAccepted(_))
        }) {
            append_task_context(
                &mut context,
                task.seq,
                project_update.as_deref(),
                contract.as_ref(),
            );
        }
        return Ok(context);
    }
    let surface = fold(events);
    let scope = match thread {
        Some(chain) => ReplayScope::Thread(chain),
        None => ReplayScope::Conversation(run),
    };
    let resolved =
        context_sources::resolve_history(history::entries(&surface, scope), payloads, outputs)
            .await?;
    let decayed = surface
        .runs
        .get(run)
        .map(|view| view.decayed.clone())
        .unwrap_or_default();
    Ok(template.assemble(resolved, decayed))
}

/// Canonical model context for execution or API/manual summaries. Pass `None`
/// for the UI leaf; a running request must pass its Run. `context` carries only
/// the generated system prompt and optional frozen task prefix, never old history.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn assemble_tree_context(
    events: &[Event],
    run: Option<&RunId>,
    thread: Option<&[RunId]>,
    mut context: AgentContext,
    projection: ProjectionContext,
    payloads: &PayloadStore,
    outputs: Option<&dyn ToolOutputStore>,
    tools: &mut Vec<ToolDefinition>,
) -> Result<AgentContext, LedgerError> {
    let mut selected = tree::select(events, run, thread).map_err(LedgerError::Corrupt)?;
    // The legacy history resolver reads text and outputs; arguments are another
    // externalized payload and must be hydrated for historical protocol too.
    for message in &mut selected.sources {
        for call in &mut message.tool_calls {
            if let Some(reference) = &call.arguments_ref {
                call.arguments = payloads
                    .open_json(reference)
                    .await
                    .map_err(komo_store::db::store_to_ledger)?;
                call.arguments_ref = None;
            }
        }
    }
    let resolved = context_sources::resolve_history(selected.entries(), payloads, outputs).await?;
    selected.apply(&mut context, resolved, &projection, tools);
    if let Some(outputs) = outputs {
        for (seq, references) in &selected.image_refs {
            if let Some(result) = context
                .messages
                .iter_mut()
                .find(|message| message.seq == *seq)
                .and_then(|message| message.tool_results.first_mut())
            {
                for reference in references {
                    if let Ok(bytes) = outputs.open_image(reference).await {
                        let image = komo_kernel::types::tool::ToolImage {
                            mime: reference.mime.clone(),
                            bytes,
                        };
                        if !result.images.contains(&image) {
                            result.images.push(image);
                        }
                    }
                }
            }
        }
    }
    Ok(context)
}

/// 一次判断的事实。
pub(crate) struct Facts<'a> {
    pub context: &'a AgentContext,
    pub tools: &'a [ToolDefinition],
    pub online: &'a OnlineState,
    pub config: &'a CompactionConfig,
    pub model: &'a ModelConfig,
    pub session: &'a SessionId,
    pub run: &'a RunId,
    pub cache_cold: bool,
    /// 上一轮的调用都收尾了。没收尾时不能压（切点会把一轮的调用与结果分开）——续跑那一轮
    /// 收尾之后，loop 按同一份估计再判一次窗口；换短视图只动已完成结果的正文，照常算。
    pub settled: bool,
    pub branch: bool,
}

/// 判一次：要改写前缀就给出整批（摘要请求，或要换短视图的那批结果）；不改什么都不记。
pub(crate) fn decide(facts: Facts<'_>) -> Option<Reshape> {
    let settings = CompactionSettings::from(facts.config);
    let planner = if facts.branch { plan_branch } else { plan };
    let outcome = planner(PlanInput {
        context: facts.context,
        tools: facts.tools,
        online: facts.online,
        settings: &settings,
        model: facts.model,
        session: facts.session,
        run: facts.run,
        cache_cold: facts.cache_cold,
        compaction_enabled: facts.config.enabled,
    });
    match outcome {
        Plan::Compact(job) if facts.settled => Some(Reshape::Compact(job)),
        Plan::Decay(job) => Some(Reshape::Decay(job)),
        Plan::Compact(_) | Plan::Nothing => None,
    }
}

/// 读完一个 Session 的日志。
pub(crate) async fn read_events(
    ledger: &RoutedLedger,
    session: &SessionId,
) -> Result<Vec<Event>, LedgerError> {
    ledger.read_all(session).await
}

/// 一轮收尾之后来问的那一个（`komo_runtime::agent::CompactionPlanner`）。这一段装配时
/// 造好，带着这一段的模板、工具表与冻结的模型。
pub(crate) struct GatewayPlanner {
    pub routed: Arc<RoutedLedger>,
    pub outputs: Option<Arc<dyn ToolOutputStore>>,
    pub config: Option<Arc<ConfigHolder>>,
    pub session: SessionId,
    pub run: RunId,
    /// 子代理这条线（旧→新，含这一条）；主对话是 `None`。
    pub thread: Option<Vec<RunId>>,
    pub template: ContextTemplate,
    pub tools: Vec<ToolDefinition>,
    pub model: ModelConfig,
    pub clock: Arc<dyn Clock>,
}

impl GatewayPlanner {
    async fn context(
        &self,
        events: &[Event],
        tools: &mut Vec<ToolDefinition>,
    ) -> Result<AgentContext, LedgerError> {
        let payloads = PayloadStore::new(self.routed.ledgers().paths_for(&self.session));
        assemble_context_with_tools(
            events,
            &self.run,
            self.thread.as_deref(),
            self.template.clone(),
            &payloads,
            self.outputs.as_deref(),
            tools,
        )
        .await
    }

    /// 读日志、按模板装配：provider 正看着的那份视图和日志本身。读不出就不判。
    async fn read(&self) -> Option<(Vec<Event>, AgentContext, Vec<ToolDefinition>)> {
        let events = match read_events(self.routed.as_ref(), &self.session).await {
            Ok(events) => events,
            Err(error) => {
                tracing::warn!(run = %self.run, %error, "读不出日志，这一轮不判压缩");
                return None;
            }
        };
        let mut tools = self.tools.clone();
        match self.context(&events, &mut tools).await {
            Ok(context) => Some((events, context, tools)),
            Err(error) => {
                tracing::warn!(run = %self.run, %error, "装配不出上下文，这一轮不判压缩");
                None
            }
        }
    }
}

#[async_trait]
impl CompactionPlanner for GatewayPlanner {
    async fn plan(&self) -> Option<Reshape> {
        let config = current(self.config.as_ref())?;
        // 压缩与衰减都关着：planner 只为溢出恢复而在，自动判断不读日志。
        if !config.enabled && self.template.projection.decay.is_none() {
            return None;
        }
        let (events, context, tools) = self.read().await?;
        let online = context_online(&events, &self.run, self.thread.as_deref());
        decide(Facts {
            context: &context,
            tools: &tools,
            online: &online,
            config: &config,
            model: &self.model,
            session: &self.session,
            run: &self.run,
            cache_cold: context_cache_cold(
                &events,
                &self.run,
                self.thread.as_deref(),
                self.clock.now(),
            ),
            settled: true,
            branch: tree::enabled(&events),
        })
    }

    /// 溢出恢复不看 `enabled`：不压这条 Run 就失败了。只救一次由 `plan_overflow` 按在线
    /// 状态判（上次压成摘要之后还没有成功的请求就不再压）。
    async fn plan_overflow(&self) -> Option<CompactionJob> {
        let config = current(self.config.as_ref())?;
        let (events, context, tools) = self.read().await?;
        let online = context_online(&events, &self.run, self.thread.as_deref());
        let settings = CompactionSettings::from(&config);
        let planner = if tree::enabled(&events) {
            plan_overflow_branch
        } else {
            plan_overflow
        };
        planner(PlanInput {
            context: &context,
            tools: &tools,
            online: &online,
            settings: &settings,
            model: &self.model,
            session: &self.session,
            run: &self.run,
            cache_cold: context_cache_cold(
                &events,
                &self.run,
                self.thread.as_deref(),
                self.clock.now(),
            ),
            compaction_enabled: config.enabled,
        })
    }
}

/// Branch-local accounting includes summaries and successful requests across Runs.
pub(crate) fn context_online(
    events: &[Event],
    run: &RunId,
    thread: Option<&[RunId]>,
) -> OnlineState {
    if tree::enabled(events) {
        return tree::select(events, Some(run), thread)
            .map(|selection| selection.online_state())
            .unwrap_or_default();
    }
    online_state(events, run)
}
pub(crate) fn context_cache_cold(
    events: &[Event],
    run: &RunId,
    thread: Option<&[RunId]>,
    now: time::OffsetDateTime,
) -> bool {
    if tree::enabled(events) {
        return tree::select(events, Some(run), thread)
            .is_ok_and(|selection| cache_cold(&selection.accounting, now));
    }
    cache_cold(events, now)
}

#[cfg(test)]
mod tree_tests {
    use super::*;
    use komo_kernel::events::{ContextCompacted, MessageAssistant};
    use komo_kernel::session::SessionEntry;
    use komo_kernel::types::ids::{EventId, Seq};
    use komo_kernel::types::turn::ToolCallRequest;
    use serde_json::{Value, json};

    fn event(
        seq: u64,
        run: &str,
        id: &str,
        parent: Option<&str>,
        mut entry: Value,
        payload: EventPayload,
    ) -> Event {
        entry["id"] = json!(id);
        entry["parentId"] = json!(parent);
        entry["timestamp"] = json!("2026-10-23T00:00:00Z");
        Event {
            entry: Some(Box::new(
                serde_json::from_value::<SessionEntry>(entry).unwrap(),
            )),
            v: 1,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("event-{seq}")),
            session: SessionId::from_raw("session"),
            run: Some(RunId::from_raw(run)),
            ts: komo_kernel::test_support::TestClock::fixed().now(),
            payload,
        }
    }
    fn api() -> EventPayload {
        EventPayload::Unknown {
            event_type: "session.entry".into(),
            raw: Value::Null,
        }
    }
    fn blank() -> AgentContext {
        AgentContext {
            system_prompt: "generated".into(),
            messages: vec![],
            run_from: 0,
            decay_candidates: vec![],
        }
    }
    const PROJECTION: ProjectionContext = ProjectionContext {
        model_result_bytes: 8192,
        decay: None,
    };

    #[tokio::test]
    async fn tree_context_reads_previous_run_summary_and_externalized_arguments_only_on_its_branch()
    {
        let dir = tempfile::tempdir().unwrap();
        let payloads = PayloadStore::new(komo_store::SessionPaths::at(dir.path()));
        let arguments = json!({"path":"long path".repeat(2000)});
        let arguments_ref = payloads.put_json(&arguments, None).await.unwrap();
        let summary_ref = payloads
            .put(b"external summary of previous Run")
            .await
            .unwrap();
        let mut compacted = komo_kernel::test_support::compacted(Seq(2), "unused");
        if let ContextCompacted::Compacted {
            summary,
            summary_ref: target,
            ..
        } = &mut compacted
        {
            *summary = None;
            *target = Some(summary_ref);
        }
        let assistant = MessageAssistant {
            round: 1,
            text: None,
            text_ref: None,
            tool_calls: vec![ToolCallRequest {
                call_id: ToolCallId::from_raw("internal"),
                provider_call_id: "pc".into(),
                name: "read".into(),
                arguments: Value::Null,
                arguments_ref: Some(arguments_ref.clone()),
            }],
            provider_blocks: None,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
        };
        let mut missing = arguments_ref;
        missing.0.path = "payloads/missing".into();
        let mut sibling = assistant.clone();
        sibling.text_ref = Some(missing);
        let events = vec![
            event(
                1,
                "old",
                "root",
                None,
                json!({"type":"message","message":{"role":"user","content":"old task"}}),
                api(),
            ),
            event(
                2,
                "old",
                "call",
                Some("root"),
                json!({"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":"pc","name":"read","arguments":null}]}}),
                EventPayload::MessageAssistant(assistant),
            ),
            event(
                3,
                "old",
                "result",
                Some("call"),
                json!({"type":"message","message":{"role":"toolResult","toolCallId":"pc","content":[{"type":"text","text":"already projected output"}]}}),
                api(),
            ),
            event(
                4,
                "old",
                "compact",
                Some("result"),
                json!({"type":"compaction","summary":"","firstKeptEntryId":"call","tokensBefore":200}),
                EventPayload::ContextCompacted(compacted),
            ),
            event(
                5,
                "new",
                "task",
                Some("compact"),
                json!({"type":"message","message":{"role":"user","content":"next task"}}),
                api(),
            ),
            event(
                6,
                "sibling",
                "sibling",
                Some("root"),
                json!({"type":"message","message":{"role":"assistant","content":[]}}),
                EventPayload::MessageAssistant(sibling),
            ),
        ];
        let context = assemble_tree_context(
            &events,
            Some(&RunId::from_raw("new")),
            None,
            blank(),
            PROJECTION,
            &payloads,
            None,
            &mut vec![],
        )
        .await
        .unwrap();
        assert_eq!(context.messages.len(), 4);
        assert!(
            context.messages[0]
                .text
                .as_ref()
                .unwrap()
                .contains("external summary of previous Run")
        );
        assert_eq!(context.messages[1].tool_calls[0].arguments, arguments);
        assert_eq!(
            context.messages[1].tool_calls[0].call_id.as_str(),
            "internal"
        );
        assert_eq!(context.messages[2].tool_results[0].provider_call_id, "pc");
        assert_eq!(context.run_from, 0);
    }

    #[tokio::test]
    async fn automatic_compaction_without_checkpoint_inherits_only_its_branch_system() {
        let dir = tempfile::tempdir().unwrap();
        let payloads = PayloadStore::new(komo_store::SessionPaths::at(dir.path()));
        let events = vec![
            event(
                1,
                "old",
                "system",
                None,
                json!({"type":"message","message":{"role":"system","content":"branch system","toolsAdded":[{"name":"read","parameters":{}}]}}),
                api(),
            ),
            event(
                2,
                "old",
                "text",
                Some("system"),
                json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"retain"}]}}),
                api(),
            ),
            event(
                3,
                "old",
                "compact",
                Some("text"),
                json!({"type":"compaction","summary":"summary","firstKeptEntryId":"text","tokensBefore":10}),
                EventPayload::ContextCompacted(komo_kernel::test_support::compacted(
                    Seq(2),
                    "summary",
                )),
            ),
            event(
                4,
                "new",
                "task",
                Some("compact"),
                json!({"type":"message","message":{"role":"user","content":"continue"}}),
                api(),
            ),
            event(
                5,
                "other",
                "other-system",
                Some("system"),
                json!({"type":"message","message":{"role":"system","content":"wrong branch"}}),
                api(),
            ),
        ];
        let mut tools = vec![];
        let context = assemble_tree_context(
            &events,
            Some(&RunId::from_raw("new")),
            None,
            blank(),
            PROJECTION,
            &payloads,
            None,
            &mut tools,
        )
        .await
        .unwrap();
        assert_eq!(context.system_prompt, "branch system");
        assert_eq!(tools[0].name, "read");
        assert!(
            context.messages[0]
                .text
                .as_ref()
                .unwrap()
                .contains("summary")
        );
        let online = context_online(&events, &RunId::from_raw("new"), None);
        assert_eq!(online.compaction_count, 1);
        assert_eq!(online.requests_since_last_summary(), Some(0));
    }
}
