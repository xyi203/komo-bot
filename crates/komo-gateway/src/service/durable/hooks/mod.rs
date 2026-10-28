//! Durable 模式的 `AgentHooks`（`docs/storage-durable-d5.md` §6）：harness 每个决定点问一次，
//! 钩子按**当前**配置快照现解析——热重载之后下一轮就用新的（"names resolved at each use"）。
//!
//! | 文件 | 回答什么 |
//! |---|---|
//! | `setup` | 这个 conversation 是谁（`komo.agent` → `komo.task` → 默认 Agent）、用哪个模型、工作目录、预算、缓存键 |
//! | `tools` | 能力面与工具表：MCP 有超时地等连上，撤掉的工具放桩，任务会话去掉 `task` / `monitor` |
//! | `env` | 工具执行环境：根与挂载、投影、时限、Session 目录上的输出存储 |
//! | `sections` | 系统段（key 与"已展示的段钉住"的规则见该文件） |
//! | `recall` | 每个 Run 的记忆召回块与遗忘改写（`docs/memory.md` §9.4） |
//! | `compaction` | `[compaction]` 的旋钮、摘要提示与摘要用的模型 |
//!
//! 钩子拿 `GatewayState` 的路径是 `StateSlot`（`assemble` 的断环）：槽没填之前被问到就干净地
//! 失败（调度此刻也还是暂停的，不会被问到）。

mod compaction;
/// `!` 命令直跑（`shell.rs`）也要同一份执行环境与能力面。
pub(crate) mod env;
mod recall;
mod sections;
mod setup;
pub(crate) mod tools;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::durable::{ConversationId, Message};
use komo_kernel::types::ids::SessionId;
use komo_runtime::durable::EntryDraft;
use komo_runtime::durable::harness::{
    AgentHooks, AgentSetup, CompactionSetup, ContextRequest, HookError, ToolSet,
};
use komo_store::{FileToolOutputStore, SessionPaths};

use super::assemble::StateSlot;
use crate::service::state::GatewayState;

pub(crate) use setup::{Resolved, catalog_model, profile_model, resolve};

/// `AgentHooks` 的 Gateway 实现。
pub struct GatewayHooks {
    slot: StateSlot,
    /// 每个 Session 一个输出存储：`begin` 开出来的流要由同一个实例 `publish`，所以不能每次
    /// 解析工具表都新造一个（同 `RoutedOutputs`）。造它不碰磁盘，只算路径。
    outputs: Mutex<BTreeMap<SessionId, Arc<FileToolOutputStore>>>,
}

impl GatewayHooks {
    pub fn new(slot: StateSlot) -> Arc<Self> {
        Arc::new(GatewayHooks {
            slot,
            outputs: Mutex::new(BTreeMap::new()),
        })
    }

    fn state(&self) -> Result<Arc<GatewayState>, HookError> {
        state_of(&self.slot)
    }

    fn output_store(
        &self,
        sessions_dir: &std::path::Path,
        session: &SessionId,
    ) -> Arc<FileToolOutputStore> {
        let mut stores = self.outputs.lock().expect("输出存储表");
        Arc::clone(stores.entry(session.clone()).or_insert_with(|| {
            Arc::new(FileToolOutputStore::new(
                SessionPaths::new(sessions_dir, session),
                session.clone(),
            ))
        }))
    }

    /// 解析 conversation 并组好工具表（`setup` 与 `tools` 共用）。
    async fn resolve_tools(
        &self,
        state: &Arc<GatewayState>,
        resolved: &Resolved,
    ) -> Result<ToolSet, HookError> {
        let config = state.snapshot();
        let outputs = self.output_store(&config.paths.sessions_dir, &resolved.session);
        tools::build(state, &config, resolved, outputs).await
    }
}

/// 从槽里取 `GatewayState`；还没接上或已经释放就失败。
pub(crate) fn state_of(slot: &StateSlot) -> Result<Arc<GatewayState>, HookError> {
    slot.get()
        .and_then(std::sync::Weak::upgrade)
        .ok_or_else(|| {
            HookError::Failed("Gateway 还没有就绪（或已经停了），Agent 钩子没法解析".into())
        })
}

#[async_trait]
impl AgentHooks for GatewayHooks {
    async fn setup(
        &self,
        conversation: ConversationId,
        context: &[Message],
    ) -> Result<AgentSetup, HookError> {
        let state = self.state()?;
        let config = state.snapshot();
        let core = state.core().map_err(|e| HookError::Failed(e.to_string()))?;
        let resolved = resolve(&state, &*core.storage, conversation, &config).await?;
        // 新的一段（一次模型请求）从没有插话开始：并进去的插话到这里已经落了地。
        core.lower_interjection(conversation);
        // 刚登录的 MCP 服务器这时连上，它的工具进这一轮的能力面（旧 `freeze_run` 同一步）。
        state.refresh_mcp().await;
        let tools = self.resolve_tools(&state, &resolved).await?;
        let sections = sections::build(&state, &resolved, &tools, context)?;
        Ok(AgentSetup {
            sections,
            tools,
            model: resolved.model.clone(),
            budget: resolved.budget,
            cache_key: Some(resolved.cache_key.clone()),
            max_output_tokens: None,
        })
    }

    async fn context_entries(
        &self,
        request: ContextRequest<'_>,
    ) -> Result<Vec<EntryDraft>, HookError> {
        let state = self.state()?;
        let config = state.snapshot();
        let core = state.core().map_err(|e| HookError::Failed(e.to_string()))?;
        let resolved = resolve(&state, &*core.storage, request.conversation, &config).await?;
        Ok(recall::entries(&state, &resolved, request).await)
    }

    /// 只要工具表：不召回记忆、不渲染系统段（默认实现会走完整的 `setup`）。
    async fn tools(&self, conversation: ConversationId) -> Result<ToolSet, HookError> {
        let state = self.state()?;
        let config = state.snapshot();
        let core = state.core().map_err(|e| HookError::Failed(e.to_string()))?;
        let resolved = resolve(&state, &*core.storage, conversation, &config).await?;
        self.resolve_tools(&state, &resolved).await
    }

    async fn compaction(&self, conversation: ConversationId) -> Result<CompactionSetup, HookError> {
        let state = self.state()?;
        let config = state.snapshot();
        let core = state.core().map_err(|e| HookError::Failed(e.to_string()))?;
        let resolved = resolve(&state, &*core.storage, conversation, &config).await?;
        Ok(compaction::build(&config, &resolved))
    }
}

#[cfg(test)]
mod tests;
