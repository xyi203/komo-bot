//! 钩子与编排的装配，以及它们与 `GatewayState` 的断环。
//!
//! 钩子要读配置、技能、记忆——都在 `GatewayState` 上；而 `GatewayState` 要握着
//! `DurableCore`，`DurableCore` 又握着钩子。环用 `OnceLock<Weak<GatewayState>>` 断开，
//! 接法照 [`GatewayState::wire_task_spawner`]：`service::start` 先建空槽、造钩子与
//! `DurableCore`、装配 `GatewayState`，**之后**才 [`wire`] 把弱引用填进去。填之前钩子被问到
//! 就干净地失败（调度此刻也还是暂停的，不会被问到）。

use std::sync::{Arc, OnceLock, Weak};

use komo_runtime::durable::harness::{AgentHooks, OrchestrationHooks};
use komo_runtime::tools::detach::DetachSlot;

use super::{hooks, monitor_runner, orchestration};
use crate::service::state::GatewayState;

/// 钩子拿到 `GatewayState` 的槽。
pub type StateSlot = Arc<OnceLock<Weak<GatewayState>>>;

pub fn new_slot() -> StateSlot {
    Arc::new(OnceLock::new())
}

/// 把 `GatewayState` 填进槽（只能填一次，重复填忽略），并把监听的接手方装进 `bash` 的
/// `DetachSlot`——接手方（`MonitorRunner`）要等整份状态才造得出来，所以 `bash` 先拿着空槽，
/// 这里再接上（问编排钩子要，没装监听就没有）。
pub fn wire(slot: &StateSlot, state: &Arc<GatewayState>, detach: &DetachSlot) {
    if slot.set(Arc::downgrade(state)).is_err() {
        tracing::warn!("Durable 钩子已经接过一次，忽略这次重复设置");
    }
    let core = &state.durable;
    core.attach_state(slot);
    if let Some(detacher) = core.harness.deps().orchestration.detacher() {
        detach.set(detacher);
    }
}

/// `AgentHooks` 的装配点：[`hooks::GatewayHooks`]（`docs/storage-durable-d5.md` §6）。
pub fn build_hooks(slot: &StateSlot) -> Arc<dyn AgentHooks> {
    hooks::GatewayHooks::new(Arc::clone(slot))
}

/// `OrchestrationHooks` 的装配点：[`orchestration::GatewayOrchestration`]，监听的进程那一半
/// 是 [`monitor_runner::GatewayMonitorRunner`]。
pub fn build_orchestration(slot: &StateSlot) -> Arc<dyn OrchestrationHooks> {
    let runner = monitor_runner::GatewayMonitorRunner::new(Arc::clone(slot));
    orchestration::GatewayOrchestration::new(Arc::clone(slot), Some(runner))
}
