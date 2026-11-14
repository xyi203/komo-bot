//! 一次触发投出去之后（§10 流程的最后两步：「保存结果和产物」→「更新本次触发状态」）。
//!
//! 一次触发投出去之后（§10 流程的最后两步：「保存结果和产物」→「更新本次触发状态」）。
//!
//! 两件事：「更新本次触发状态」——终态 → `ok` / `error`，写进 `cron_firings`（手动 run 没有触发
//! 记录，`settle` 什么都不写，§10：手动 run 不冒充定时触发）；以及按 `notify` 把结果投进对话
//! （[`notify`]）：聊天渠道删除后（`docs/redesign.md` P17）不再有 home chat，结果作为 Komo 说的一句话
//! 落进目标绑定的支线（P22）、没绑定落进主对话（`durable::chats::say`；投到哪由
//! `DurableCore::cron_route` 定）。
//!
//! **订阅那一层在 [`super::durable::background::cron_watch`]**：等 submission 结算再读终态帧。这里是
//! Cron 自己的那件事——回写触发状态。
//!
//! 它**不是**恢复机制：重启后靠 `durable::background::cron::recover` 重挂看客，不靠这里再看一遍。

use std::sync::Arc;

use komo_kernel::cron::{CronJob, FiringStatus, NotifyPolicy};
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
            name: job.name.clone(),
            notify: job.notify,
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

/// 按 `notify` 决定投不投（§10），投就是往对话里说一句 `定时任务「名字」：结果`。
///
/// 关联键 = 这一次触发的 id（`{job}@{计划时刻}`，手动 run 没有触发记录就用 Run）：同一次触发只投一次，
/// 重启后看客重挂再走到这里也不会重复。`on_error` 只在出错时投；一次**等待**（停住的运行在问你）不看
/// `notify`，它由 `durable::chats::attention` 投。
pub(crate) async fn notify(
    state: &Arc<GatewayState>,
    watched: &Watched,
    status: FiringStatus,
    text: &str,
) {
    if !watched.notify.delivers(status) {
        tracing::debug!(job = %watched.job, notify = watched.notify.as_str(), "这一次不投");
        return;
    }
    let Ok(core) = state.core() else { return };
    let firing = if watched.scheduled {
        format!(
            "{}@{}",
            watched.job,
            watched.scheduled_at.unix_timestamp_nanos() / 1_000_000
        )
    } else {
        format!("{}@run-{}", watched.job, watched.run)
    };
    let posted = async {
        let session = core.session(watched.session.as_str()).await?;
        let route = core.cron_route(&session).await?;
        let chat = core.resolve_route(route).await?;
        core.say_in(
            &chat.id,
            super::durable::chats::say::Say {
                text: format!("定时任务「{}」：{text}", watched.name),
                model_note: None,
                source: super::durable::chats::say::SaySource::Cron,
                key: format!("cron:{firing}"),
                data: serde_json::json!({
                    "job": watched.job,
                    "firing": firing,
                    "run": watched.run,
                    "session": watched.session,
                }),
            },
        )
        .await
    }
    .await;
    if let Err(error) = posted {
        tracing::warn!(%error, job = %watched.job, "定时任务的结果没能投到对话里");
    }
}
