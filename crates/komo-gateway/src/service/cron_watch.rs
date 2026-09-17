//! 一次触发投出去之后（§10 流程的最后两步：「保存结果和产物」→「更新本次触发状态」）。
//!
//! Cron 与聊天的分别全在**谁在看**。一条聊天消息有来源会话，Dispatcher 订阅那个
//! Session 的事件流、把终态回给那个会话（`Dispatcher::watch_reply`）；一次 Cron 触发
//! 没有来源会话，所以这里做同一件事、投到 **home chat**（§11.4：「来源是 Cron 或已
//! 断开的 TUI 时只有 home chat」）。
//!
//! 三条语句在这里落地，各只有一处实现：
//!
//! - **「新增危险操作暂停等待审批，不能因无人值守而自动放行」**（§10）：
//!   `run.waiting_approval` 一出现就把这条审批投到 home chat。没有这一步，一个 Cron
//!   Run 会停在等待上而**没有人知道它在等**——§7.4 的"投递到 Run 的来源会话与 home
//!   chat"在 Cron 这一侧就是这一句。
//! - **`notify`**（§10）：`always` / `on_error` / `never` 过滤**结果**的投递。
//!   `waiting` 不受它约束——那是任务在**问**，不是在报告，一条没人看见的提问等于这个
//!   Job 从此停在那里。
//! - **「更新本次触发状态」**：终态 → `ok` / `error`，停在等待上 → `waiting`。手动
//!   run 没有触发记录，`settle` 答 `false`，这是对的（§10：手动 run 不冒充定时触发）。
//!
//! **订阅那一层在 [`super::run_watch`]**：交互 Run 与 Cron 触发的分别只有"谁是来源
//! 会话"，两条循环会各投一遍同一条审批。这里剩下的是 Cron 自己的那两件事——回写触发
//! 状态与按 `notify` 过滤结果。
//!
//! 它**不是**恢复机制：进程活着的时候订阅事件流，重启后靠 `deliveries` 表里那一行
//! pending 补发（§11.4），不靠这里再看一遍。

use std::sync::Arc;

use komo_kernel::cron::{CronJob, FiringStatus, NotifyPolicy};
use komo_kernel::types::chat::Outbound;
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
    pub name: String,
    pub notify: NotifyPolicy,
    /// 手动 run 没有触发记录，不回写状态（§10）。
    pub scheduled: bool,
}

impl Watched {
    pub fn of(fired: &Fired, job: &CronJob, scheduled: bool) -> Watched {
        Watched {
            job: fired.job.clone(),
            session: fired.session.clone(),
            run: fired.run.clone(),
            scheduled_at: fired.scheduled_at,
            name: job.name.clone(),
            notify: job.notify,
            scheduled,
        }
    }
}

/// 盯一次触发：交给统一的看客（[`super::run_watch`]），来源是 Cron。
pub fn watch(state: Arc<GatewayState>, watched: Watched) {
    let (session, run) = (watched.session.clone(), watched.run.clone());
    super::run_watch::watch(
        state,
        session,
        run,
        super::run_watch::Watcher::Cron(Box::new(watched)),
    );
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

/// 按 `notify` 决定投不投（§10）。
pub(crate) async fn notify(
    state: &Arc<GatewayState>,
    watched: &Watched,
    status: FiringStatus,
    session: &SessionId,
    run: &RunId,
    text: &str,
) {
    if !watched.notify.delivers(status) {
        tracing::debug!(job = %watched.job, notify = watched.notify.as_str(), "这一次不投");
        return;
    }
    let _ = state
        .notifier
        .deliver_home(Outbound::RunFinished {
            session: session.clone(),
            run: run.clone(),
            summary: summary(watched, text),
        })
        .await;
}

fn summary(watched: &Watched, body: &str) -> String {
    format!("定时任务「{}」：{body}", watched.name)
}
