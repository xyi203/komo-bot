//! Durable 模式的后台循环（对应 Legacy 的 `service::spawn_background`）。
//!
//! 旧的调度器、审计补写、周期对账、旧的记忆队列与 Cron tick 在这里一概不起。配置轮询与
//! SIGHUP 与存储无关，照旧。后续包往这里加：
//!
//! - W2c：启动时对账监听（上一代遗留的进程核对 + `Harness::reconcile_monitors`），已在这里；
//! - W2b：`feed::spawn`（提交订阅 → SSE），由主控合并时补一行；
//! - W3b：cron tick；W3c：`MEMORY_TICK`；W4a：启动时做完停在 `closing` 的删除。

use std::sync::Arc;

use komo_kernel::traits::Shutdown;

use crate::service::state::GatewayState;

pub fn spawn(state: &Arc<GatewayState>, shutdown: &Shutdown) {
    {
        let state = Arc::clone(state);
        let shutdown = shutdown.clone();
        tokio::spawn(async move { crate::reload::watch(state, shutdown).await });
    }
    {
        let state = Arc::clone(state);
        let shutdown = shutdown.clone();
        tokio::spawn(async move { crate::reload::on_sighup(state, shutdown).await });
    }
    {
        let state = Arc::clone(state);
        tokio::spawn(async move { reconcile_monitors(&state).await });
    }
    // W3c：记忆的后台领取（`MEMORY_TICK`）。
    super::memory::spawn_tick(state, shutdown);
}

/// 启动时收掉上一代留下的监听（`docs/monitor.md` §5）：先让编排钩子核对并终止遗留的进程组、
/// 收转后台命令（`boot`），再对账登记里还写着 `running` 的——声明了 `resume_after_restart` 且还剩
/// ≥ 1 分钟的按原句柄重挂，其余交回"已中断"。调度此刻已经开始（`core.resume()`），`starting` 的
/// 登记归挂它的工具任务自己判，这里不碰。
async fn reconcile_monitors(state: &Arc<GatewayState>) {
    let Ok(core) = state.core() else { return };
    core.harness.deps().orchestration.boot().await;
    match core.harness.reconcile_monitors(&core.engine).await {
        Ok(done) if done.is_empty() => {}
        Ok(done) => tracing::info!(?done, "启动时对账了上一代的监听"),
        Err(error) => tracing::warn!(%error, "启动时对账监听失败"),
    }
}
