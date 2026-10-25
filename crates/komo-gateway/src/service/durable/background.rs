//! Durable 模式的后台循环（对应 Legacy 的 `service::spawn_background`）。
//!
//! 旧的调度器、审计补写、周期对账、旧的记忆队列与 Cron tick 在这里一概不起。配置轮询与
//! SIGHUP 与存储无关，照旧。后续包往这里加：
//!
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
}
