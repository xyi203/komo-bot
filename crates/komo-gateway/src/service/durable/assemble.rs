//! 钩子与编排的装配，以及它们与 `GatewayState` 的断环。
//!
//! 钩子要读配置、技能、记忆——都在 `GatewayState` 上；而 `GatewayState` 要握着
//! `DurableCore`，`DurableCore` 又握着钩子。环用 `OnceLock<Weak<GatewayState>>` 断开，
//! 接法照 [`GatewayState::wire_task_spawner`]：`service::start` 先建空槽、造钩子与
//! `DurableCore`、装配 `GatewayState`，**之后**才 [`wire`] 把弱引用填进去。填之前钩子被问到
//! 就干净地失败（调度此刻也还是暂停的，不会被问到）。

use std::sync::{Arc, Mutex, OnceLock, Weak};

use komo_runtime::durable::harness::{AgentHooks, OrchestrationHooks};
use komo_runtime::tools::detach::DetachSlot;

use super::{hooks, monitor_runner, orchestration};
use crate::service::state::GatewayState;

/// 钩子拿到 `GatewayState` 的槽。
pub type StateSlot = Arc<OnceLock<Weak<GatewayState>>>;

pub fn new_slot() -> StateSlot {
    Arc::new(OnceLock::new())
}

/// 把 `GatewayState` 填进槽（只能填一次，重复填忽略），并接上 `bash` 转后台的接手方。
pub fn wire(slot: &StateSlot, state: &Arc<GatewayState>) {
    if slot.set(Arc::downgrade(state)).is_err() {
        tracing::warn!("Durable 钩子已经接过一次，忽略这次重复设置");
    }
    wire_detach(state);
}

/// `build_tools` 造 `bash` 时的 `DetachSlot`，按实例 ID 停在这里，等 [`wire`] 取走。`start()` 里
/// 它只是个局部变量，Durable 的接手方（监听的 `MonitorRunner`）要等整份状态才造得出来，而 `start()`
/// 不为它多接一根线——所以经这里递过去。只存弱引用：槽归 `BashTool`。
static PARKED_DETACH: Mutex<Vec<(String, Weak<DetachSlot>)>> = Mutex::new(Vec::new());

/// `build_tools` 调：停下这台实例的 `DetachSlot`。
pub fn park_detach_slot(instance_id: &str, slot: &Arc<DetachSlot>) {
    let mut parked = PARKED_DETACH.lock().expect("转后台的槽");
    parked.retain(|(_, weak)| weak.strong_count() > 0);
    parked.push((instance_id.to_owned(), Arc::downgrade(slot)));
}

fn take_detach_slot(instance_id: &str) -> Option<Arc<DetachSlot>> {
    let mut parked = PARKED_DETACH.lock().expect("转后台的槽");
    let at = parked.iter().position(|(id, _)| id == instance_id)?;
    parked.remove(at).1.upgrade()
}

/// 把监听的接手方装进 `bash` 的 `DetachSlot`（问编排钩子要，没装监听就没有）。
fn wire_detach(state: &Arc<GatewayState>) {
    let Some(slot) = take_detach_slot(&state.instance_id) else {
        return;
    };
    let Some(core) = state.durable.as_ref() else {
        return;
    };
    if let Some(detacher) = core.harness.deps().orchestration.detacher() {
        slot.set(detacher);
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
