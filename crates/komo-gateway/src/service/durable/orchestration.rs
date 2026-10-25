//! `OrchestrationHooks` 的 Gateway 实现（`task` / `monitor` 要 agent / gateway 侧才有的东西）。
//!
//! - `check_effort`：只给 `effort` 时选定的模型只有 gateway 知道（Agent 的默认，或任务上一次的），
//!   按它的档位校验（`EffortCapabilities`）。
//! - `contract_block`：直接调 `komo_agent` 的 `task_contract_block`，免得两处文字漂移。
//! - `start_monitor` / `stop_monitor`：委托给 [`MonitorRunner`]（进程、泵、合批、时限、进程组、
//!   遗留进程核对都是它的事，W2c 实现）。没装配时干净地失败，并说明是没装配。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::durable::ConversationId;
use komo_kernel::types::delegate::DelegateContract;
use komo_kernel::types::model::Effort;
use komo_runtime::durable::harness::{MonitorError, MonitorLaunch, OrchestrationHooks};

use super::assemble::StateSlot;
use super::hooks::{self, state_of};

/// 监听的进程那一半（`docs/monitor.md`）：runtime 只管登记与工具任务的三明治，进程归这里。
///
/// 事件由实现经 `Harness::submit(monitor::event_draft)` 交回，结束时调 `Harness::finish_monitor`，
/// 启动时调一次 `Harness::reconcile_monitors`——这些都不在这个 trait 上。
#[async_trait]
pub trait MonitorRunner: Send + Sync {
    /// 起（或认回、重挂）一个监听的进程。语义见 [`komo_runtime::durable::harness::LaunchMode`]。
    async fn start(&self, launch: &MonitorLaunch) -> Result<(), MonitorError>;

    /// 终止监听的进程组。幂等：已经没有的也算成功。
    async fn stop(&self, id: u64) -> Result<(), MonitorError>;
}

pub struct GatewayOrchestration {
    slot: StateSlot,
    monitors: Option<Arc<dyn MonitorRunner>>,
}

impl GatewayOrchestration {
    /// `monitors = None`：这台 Gateway 还没有装配监听（W2c 装配 `MonitorRunner`）。
    pub fn new(slot: StateSlot, monitors: Option<Arc<dyn MonitorRunner>>) -> Arc<Self> {
        Arc::new(GatewayOrchestration { slot, monitors })
    }

    fn unassembled() -> MonitorError {
        MonitorError::Failed(
            "这台 Gateway 没有装配监听（MonitorRunner 还没接上），monitor 用不了".into(),
        )
    }
}

#[async_trait]
impl OrchestrationHooks for GatewayOrchestration {
    async fn check_effort(
        &self,
        conversation: ConversationId,
        model: Option<&str>,
        effort: &Effort,
    ) -> Result<(), String> {
        let state = state_of(&self.slot).map_err(|e| e.to_string())?;
        let config = state.snapshot();
        let core = state.core().map_err(|e| e.to_string())?;
        let mut chosen = match model {
            Some(alias) => hooks::catalog_model(&config, alias).map_err(|e| e.to_string())?,
            None => {
                let resolved = hooks::resolve(&state, &*core.storage, conversation, &config)
                    .await
                    .map_err(|e| e.to_string())?;
                // 接着做的任务：它上一次用的模型；新派的：调用方 Agent 的默认模型。
                if resolved.is_task() {
                    resolved.model
                } else {
                    hooks::profile_model(&state, &config, &resolved.profile)
                }
            }
        };
        chosen.effort = Some(effort.clone());
        state
            .caps
            .check(&chosen)
            .map_err(|problem| problem.to_string())
    }

    fn contract_block(&self, contract: &DelegateContract) -> String {
        komo_agent::context::task_contract_block(contract)
    }

    async fn start_monitor(&self, launch: &MonitorLaunch) -> Result<(), MonitorError> {
        match &self.monitors {
            Some(runner) => runner.start(launch).await,
            None => Err(Self::unassembled()),
        }
    }

    async fn stop_monitor(&self, id: u64) -> Result<(), MonitorError> {
        match &self.monitors {
            Some(runner) => runner.stop(id).await,
            None => Err(Self::unassembled()),
        }
    }
}
