//! 在线压缩在 Gateway 这一侧（§6）：把决策要的事实读齐，交给 `komo_agent` 的纯函数。
//!
//! 两个入口，同一套事实、同一个判断：
//!
//! - **装配时**（`GatewaySegments::segment`）：这一段开头就在计划边界上、或者已经贴着
//!   窗口——决定压就把摘要请求放进 `Segment.compaction`，loop 先压再跑；
//! - **一轮收尾之后**（[`GatewayPlanner`]，loop 经 `CompactionPlanner` 来问）：重新读一遍
//!   日志、按同一份模板装配出 provider 正看着的那份视图，再判一次。
//!
//! 配置每次判断时读当前快照，不随 Run 冻结：决策连同全部中间量记进 `context.compacted`，
//! 回放只认那条事件，不会因为配置改了而换一种读法。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use komo_agent::context::compaction::{CompactionSettings, Plan, PlanInput, plan};
use komo_agent::context::history::{self, ReplayScope, ResolvedMessage};
use komo_agent::context::{AgentContext, ContextInput, InvocationContext, assemble};
use komo_agent::skills::SkillCatalog;
use komo_kernel::compaction::{CompactionDecision, CompactionJob, OnlineState, online_state};
use komo_kernel::events::{ContextCompacted, Event};
use komo_kernel::fold::fold;
use komo_kernel::projection::ProjectionContext;
use komo_kernel::protocol::config::CompactionConfig;
use komo_kernel::traits::{Ledger, LedgerError, ToolOutputStore};
use komo_kernel::types::ids::{RunId, Seq, SessionId};
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
    pub fn assemble(self, history: Vec<ResolvedMessage<'_>>) -> AgentContext {
        assemble(ContextInput {
            instructions: self.instructions,
            workspace: self.workspace,
            tools: self.tools,
            history,
            memory: self.memory,
            skills: self.skills,
            invocation: self.invocation,
            projection: self.projection,
        })
    }
}

/// 当前配置里开着的 `[compaction]`；关着（或没接配置）是 `None`。
pub(crate) fn enabled(config: Option<&Arc<ConfigHolder>>) -> Option<CompactionConfig> {
    config
        .map(|config| config.current().compaction.clone())
        .filter(|compaction| compaction.enabled)
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
}

/// 判一次：要压就给出摘要请求；边界上不压就记一条 `skipped` 把边界用掉。记不下去只
/// 留日志——压缩是省钱的手段，账本写不进去的问题会在下一次真正的写入上暴露。
pub(crate) async fn decide(ledger: &dyn Ledger, facts: Facts<'_>) -> Option<CompactionJob> {
    let settings = CompactionSettings::from(facts.config);
    let outcome = plan(PlanInput {
        context: facts.context,
        tools: facts.tools,
        online: facts.online,
        settings: &settings,
        model: facts.model,
        session: facts.session,
        run: facts.run,
    });
    match outcome {
        Plan::Compact(job) => Some(job),
        Plan::Skip { reason, decision } => {
            skip(ledger, facts.run, reason, decision).await;
            None
        }
        Plan::Nothing => None,
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
}

impl GatewayPlanner {
    async fn context(&self, events: &[Event]) -> Result<AgentContext, LedgerError> {
        let surface = fold(events);
        let scope = match &self.thread {
            Some(chain) => ReplayScope::Thread(chain),
            None => ReplayScope::Conversation(&self.run),
        };
        let payloads = PayloadStore::new(self.routed.ledgers().paths_for(&self.session));
        let resolved = context_sources::resolve_history(
            history::entries(&surface, scope),
            &payloads,
            self.outputs.as_deref(),
        )
        .await?;
        Ok(self.template.clone().assemble(resolved))
    }
}

#[async_trait]
impl CompactionPlanner for GatewayPlanner {
    async fn plan(&self) -> Option<CompactionJob> {
        let config = enabled(self.config.as_ref())?;
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
            },
        )
        .await
    }
}
