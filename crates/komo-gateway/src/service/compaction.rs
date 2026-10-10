//! 改写提示前缀在 Gateway 这一侧（§6）：把决策要的事实读齐，交给 `komo_agent` 的纯函数。
//! 压成摘要与把大结果换成短视图（衰减）共用这个决策点、同一本缓存账。
//!
//! 两个入口，同一套事实、同一个判断：
//!
//! - **装配时**（`GatewaySegments::segment`）：这一段开头就在计划边界上、贴着窗口、或者
//!   缓存已经冷了——决定压就把摘要请求放进 `Segment.compaction`，loop 先压再跑；决定衰减
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
use komo_agent::context::compaction::{CompactionSettings, Plan, PlanInput, plan};
use komo_agent::context::history::{self, ReplayScope, ResolvedMessage};
use komo_agent::context::{AgentContext, ContextInput, InvocationContext, assemble};
use komo_agent::skills::SkillCatalog;
use komo_kernel::compaction::{
    CompactionDecision, OnlineState, PROMPT_CACHE_TTL, Reshape, online_state,
};
use komo_kernel::events::{ContextCompacted, Event, EventPayload};
use komo_kernel::fold::fold;
use komo_kernel::projection::ProjectionContext;
use komo_kernel::protocol::config::CompactionConfig;
use komo_kernel::traits::{Clock, Ledger, LedgerError, ToolOutputStore};
use komo_kernel::types::ids::{RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::tool::ToolDefinition;
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
    pub workspace: PathBuf,
    pub tools: Vec<String>,
    pub memory: Option<String>,
    pub skills: Option<SkillCatalog>,
    pub invocation: InvocationContext,
    pub projection: ProjectionContext,
}

impl ContextTemplate {
    pub fn assemble(
        self,
        history: Vec<ResolvedMessage<'_>>,
        decayed: BTreeSet<ToolCallId>,
    ) -> AgentContext {
        assemble(ContextInput {
            instructions: self.instructions,
            workspace: self.workspace,
            tools: self.tools,
            history,
            decayed,
            memory: self.memory,
            skills: self.skills,
            invocation: self.invocation,
            projection: self.projection,
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
    /// 上一轮的调用都收尾了。没收尾时不能压（切点会把一轮的调用与结果分开），边界也先
    /// 不用掉；换短视图只动已完成结果的正文，照常算。
    pub settled: bool,
}

/// 判一次：要改写前缀就给出整批（摘要请求，或要换短视图的那批结果）；边界上不压就记一条
/// `skipped` 把边界用掉。记不下去只留日志——改写是省钱的手段，账本写不进去的问题会在
/// 下一次真正的写入上暴露。
pub(crate) async fn decide(ledger: &dyn Ledger, facts: Facts<'_>) -> Option<Reshape> {
    let settings = CompactionSettings::from(facts.config);
    let outcome = plan(PlanInput {
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
        Plan::Skip { reason, decision } if facts.settled => {
            skip(ledger, facts.run, reason, decision).await;
            None
        }
        Plan::Compact(_) | Plan::Skip { .. } | Plan::Nothing => None,
    }
}

async fn skip(ledger: &dyn Ledger, run: &RunId, reason: String, decision: CompactionDecision) {
    tracing::info!(run = %run, %reason, "在线压缩：这个计划边界上不压");
    let skipped = ContextCompacted::Skipped {
        reason,
        decision: Some(decision),
    };
    if let Err(error) = ledger.record_compaction(run, skipped).await {
        tracing::warn!(run = %run, %error, "记不下 context.compacted（skipped）");
    }
}

/// 读完一个 Session 的日志。
pub(crate) async fn read_events(
    ledger: &dyn Ledger,
    session: &SessionId,
) -> Result<Vec<Event>, LedgerError> {
    let mut all = Vec::new();
    let mut from = Seq::ZERO;
    loop {
        let batch = ledger.read(session, from, 0).await?;
        if batch.events.is_empty() {
            return Ok(all);
        }
        for event in batch.events {
            from = from.max(event.seq);
            all.push(event);
        }
        if batch.next.is_none() {
            return Ok(all);
        }
    }
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
    async fn context(&self, events: &[Event]) -> Result<AgentContext, LedgerError> {
        let payloads = PayloadStore::new(self.routed.ledgers().paths_for(&self.session));
        assemble_context(
            events,
            &self.run,
            self.thread.as_deref(),
            self.template.clone(),
            &payloads,
            self.outputs.as_deref(),
        )
        .await
    }
}

#[async_trait]
impl CompactionPlanner for GatewayPlanner {
    async fn plan(&self) -> Option<Reshape> {
        let config = current(self.config.as_ref())?;
        let events = match read_events(self.routed.as_ref(), &self.session).await {
            Ok(events) => events,
            Err(error) => {
                tracing::warn!(run = %self.run, %error, "读不出日志，这一轮不判压缩");
                return None;
            }
        };
        let context = match self.context(&events).await {
            Ok(context) => context,
            Err(error) => {
                tracing::warn!(run = %self.run, %error, "装配不出上下文，这一轮不判压缩");
                return None;
            }
        };
        let online = online_state(&events, &self.run);
        decide(
            self.routed.as_ref(),
            Facts {
                context: &context,
                tools: &self.tools,
                online: &online,
                config: &config,
                model: &self.model,
                session: &self.session,
                run: &self.run,
                cache_cold: cache_cold(&events, self.clock.now()),
                settled: true,
            },
        )
        .await
    }
}
