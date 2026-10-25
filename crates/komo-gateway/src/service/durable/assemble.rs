//! 钩子与编排的装配，以及它们与 `GatewayState` 的断环。
//!
//! 钩子要读配置、技能、记忆——都在 `GatewayState` 上；而 `GatewayState` 要握着
//! `DurableCore`，`DurableCore` 又握着钩子。环用 `OnceLock<Weak<GatewayState>>` 断开，
//! 接法照 [`GatewayState::wire_task_spawner`]：`service::start` 先建空槽、造钩子与
//! `DurableCore`、装配 `GatewayState`，**之后**才 [`wire`] 把弱引用填进去。填之前钩子被问到
//! 就干净地失败（调度此刻也还是暂停的，不会被问到）。

use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use komo_kernel::durable::{ConversationId, Message};
use komo_runtime::durable::harness::{
    AgentHooks, AgentSetup, HookError, NoOrchestration, OrchestrationHooks,
};

use crate::service::state::GatewayState;

/// 钩子拿到 `GatewayState` 的槽。
pub type StateSlot = Arc<OnceLock<Weak<GatewayState>>>;

pub fn new_slot() -> StateSlot {
    Arc::new(OnceLock::new())
}

/// 把 `GatewayState` 填进槽（只能填一次，重复填忽略）。
pub fn wire(slot: &StateSlot, state: &Arc<GatewayState>) {
    if slot.set(Arc::downgrade(state)).is_err() {
        tracing::warn!("Durable 钩子已经接过一次，忽略这次重复设置");
    }
}

/// 占位钩子：W1b 换成真正的实现（`hooks/` 下按 `docs/storage-durable-d5.md` §6）。
struct PlaceholderHooks {
    /// W1b：`setup` / `tools` / `compaction` 都从这里拿 `GatewayState`。
    #[allow(dead_code)]
    state: StateSlot,
}

#[async_trait]
impl AgentHooks for PlaceholderHooks {
    async fn setup(
        &self,
        _conversation: ConversationId,
        _context: &[Message],
    ) -> Result<AgentSetup, HookError> {
        Err(HookError::Failed(
            "Durable 模式的 Agent 钩子还没有装配（W1b）".into(),
        ))
    }
}

/// `AgentHooks` 的装配点。W1b 把 `PlaceholderHooks` 换成 `hooks::GatewayHooks`。
pub fn build_hooks(slot: &StateSlot) -> Arc<dyn AgentHooks> {
    Arc::new(PlaceholderHooks {
        state: Arc::clone(slot),
    })
}

/// `OrchestrationHooks` 的装配点。W1b 换成 `orchestration::GatewayOrchestration`。
pub fn build_orchestration(_slot: &StateSlot) -> Arc<dyn OrchestrationHooks> {
    Arc::new(NoOrchestration)
}
