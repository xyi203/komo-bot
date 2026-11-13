//! 一次触发投出去之后（§10 流程的最后两步：「保存结果和产物」→「更新本次触发状态」）。
//!
//! 一次触发投出去之后（§10 流程的最后两步：「保存结果和产物」→「更新本次触发状态」）。
//!
//! 这里只剩「更新本次触发状态」：终态 → `ok` / `error`，写进 `cron_firings`。手动 run 没有触发
//! 记录，`settle` 什么都不写（§10：手动 run 不冒充定时触发）。结果不再往外投——聊天渠道已删除
//! （`docs/redesign.md` P17），主动消息改投网页对话是后续一步。
//!
//! **订阅那一层在 [`super::durable::background::cron_watch`]**：等 submission 结算再读终态帧。这里是
//! Cron 自己的那件事——回写触发状态。
//!
//! 它**不是**恢复机制：重启后靠 `durable::background::cron::recover` 重挂看客，不靠这里再看一遍。

use std::sync::Arc;

use komo_kernel::cron::{CronJob, FiringStatus};
use komo_kernel::types::ids::{CronJobId, RunId, SessionId};
use komo_runtime::scheduler::Fired;

use super::state::GatewayState;

/// 这一次触发要盯的东西。`scheduled_at` 是触发记录的一半主键——用它回写状态，不必再
/// 按 Run 反查一遍。
#[derive(Debug, Clone)]
pub struct Watched {
    pub job: CronJobId,
    pub session: SessionId,
    pub run: RunId,
    pub scheduled_at: time::OffsetDateTime,
    /// 手动 run 没有触发记录，不回写状态（§10）。
    pub scheduled: bool,
    /// 这个 Job 是不是命令直跑模式（§10）：是的话，终态的 ok / error 不看模型
    /// 回复，改读 `shell.command` 落下的那次 `bash` 结果（`durable::background::cron_watch`）。
    pub is_command: bool,
}

impl Watched {
    pub fn of(fired: &Fired, job: &CronJob, scheduled: bool) -> Watched {
        Watched {
            job: fired.job.clone(),
            session: fired.session.clone(),
            run: fired.run.clone(),
            scheduled_at: fired.scheduled_at,
            scheduled,
            is_command: job.command.is_some(),
        }
    }
}

/// 更新本次触发状态（§10）。
pub(crate) async fn settle(
    state: &Arc<GatewayState>,
    watched: &Watched,
    status: FiringStatus,
    error: Option<String>,
) {
    if !watched.scheduled {
        return; // 手动 run 没有触发记录。
    }
    if let Err(problem) = state
        .cron_scheduler()
        .settle(&watched.job, watched.scheduled_at, status, error)
        .await
    {
        tracing::warn!(%problem, job = %watched.job, "触发状态没记下来");
    }
}
