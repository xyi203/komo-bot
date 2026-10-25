//! Durable 模式下的记忆（D5 W3c）：装配与 `MEMORY_TICK`。
//!
//! 条目、证据、索引仍在 `aux.db` 的 toasty repo（`GatewayState.memory` / `memories` 不变）；换掉的只有
//! 两个接缝：提取读事件走 [`DurableSessionEvents`]（kernel 的 `legacy_events` 投影），工作队列走
//! [`DurableMemoryWork`]（游标在 conversation 级 document `komo.memory_cursor`）。注入是系统段
//! `memory`，uses 记在 `komo.memory`（见 `hooks/sections.rs`）。

use std::sync::Arc;

use komo_kernel::traits::Clock;
use komo_kernel::types::agent::MemoryMode;
use komo_runtime::memory::durable::{DurableMemoryWork, DurableSessionEvents, MemoryModes};
use komo_runtime::memory::{MemoryWorkLog, SessionEvents, WORK_BATCH};

use super::DurableCore;
use crate::service::state::GatewayState;
use komo_kernel::traits::Shutdown;

/// 后台那一拍的间隔。
const MEMORY_TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// `MemoryManager` 在 Durable 下要的两个接缝。`config` 供提取时按会话的 Agent 取记忆模式。
pub fn seams(
    core: &Arc<DurableCore>,
    config: &Arc<komo_runtime::config::ConfigHolder>,
    clock: &Arc<dyn Clock>,
) -> (Arc<dyn SessionEvents>, Arc<dyn MemoryWorkLog>) {
    let events: Arc<dyn SessionEvents> =
        Arc::new(DurableSessionEvents::new(Arc::clone(&core.storage)));
    let config = Arc::clone(config);
    let modes: MemoryModes = Arc::new(move |agent: Option<&str>| {
        let snapshot = config.current();
        let id = agent.unwrap_or(snapshot.agent.default_agent.as_str());
        match snapshot.agent.get(id) {
            Some(profile) => (profile.memory, profile.id.clone()),
            None => (MemoryMode::Shared, id.to_owned()),
        }
    });
    let work: Arc<dyn MemoryWorkLog> = Arc::new(DurableMemoryWork::new(
        Arc::clone(&core.storage),
        Arc::clone(&core.engine),
        Arc::clone(clock),
        modes,
    ));
    (events, work)
}

/// 后台领取循环（`background::spawn` 末尾调用）：失败的 run 放回 pending 下一拍重试；这一拍什么都没领
/// 到是常态，不记日志。
pub fn spawn_tick(state: &Arc<GatewayState>, shutdown: &Shutdown) {
    let state = Arc::clone(state);
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(MEMORY_TICK).await;
            if shutdown.is_cancelled() {
                return;
            }
            match state.memories.process_pending(WORK_BATCH).await {
                Ok(report)
                    if report.processed > 0
                        || report.failed > 0
                        || report.skipped > 0
                        || report.abandoned > 0 =>
                {
                    tracing::info!(
                        processed = report.processed,
                        applied = report.applied,
                        skipped = report.skipped,
                        failed = report.failed,
                        abandoned = report.abandoned,
                        "记忆提取这一轮结束"
                    );
                    // 写进了变更就补向量（§9.3）；没有向量后端时关键词索引已随正文写好。
                    if report.applied > 0
                        && state.memories.space().is_some()
                        && let Err(error) = state.memories.schedule_index()
                    {
                        tracing::warn!(%error, "索引补齐没提交上，新记忆先只靠关键词命中");
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "记忆提取这一轮没跑成"),
            }
        }
    });
}
