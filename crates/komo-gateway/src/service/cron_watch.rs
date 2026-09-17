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
//! 它**不是**恢复机制：进程活着的时候订阅事件流，重启后靠 `deliveries` 表里那一行
//! pending 补发（§11.4），不靠这里再看一遍。

use std::sync::Arc;

use komo_kernel::cron::{CronJob, FiringStatus, NotifyPolicy};
use komo_kernel::events::EventPayload;
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

/// 订阅这个 Run 的事件流，直到它有结论或停下来等人。
pub fn watch(state: Arc<GatewayState>, watched: Watched) {
    let mut events = state.hub.subscribe(&watched.session);
    tokio::spawn(async move {
        loop {
            let frame = match events.recv().await {
                Ok(frame) => frame,
                // 落后了就接着收：内存通知丢了不丢数据，状态还有周期扫描兜底。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            let komo_kernel::protocol::sse::SseEvent::Event(event) = &frame.event else {
                continue;
            };
            if event.run.as_ref() != Some(&watched.run) {
                continue;
            }
            match &event.payload {
                // **等待审批**：先投出去，再把触发状态记成 `waiting`。投递在前是因为
                // 这一条是给人看的，而状态是给清单看的。
                EventPayload::RunWaitingApproval(body) => {
                    deliver_approval(&state, &watched, &body.approval).await;
                    settle(&state, &watched, FiringStatus::Waiting, None).await;
                    // 不 return：批准之后这个 Run 会接着跑，终态还要记。
                }
                EventPayload::RunCompleted(body) => {
                    let text = body
                        .final_message
                        .clone()
                        .unwrap_or_else(|| "（这一轮没有文字回复）".to_string());
                    settle(&state, &watched, FiringStatus::Ok, None).await;
                    notify(&state, &watched, FiringStatus::Ok, summary(&watched, &text)).await;
                    return;
                }
                EventPayload::RunFailed(body) => {
                    settle(
                        &state,
                        &watched,
                        FiringStatus::Error,
                        Some(body.reason.clone()),
                    )
                    .await;
                    notify(
                        &state,
                        &watched,
                        FiringStatus::Error,
                        summary(&watched, &format!("失败：{}", body.reason)),
                    )
                    .await;
                    return;
                }
                EventPayload::RunCancelled(_) => {
                    settle(
                        &state,
                        &watched,
                        FiringStatus::Error,
                        Some("已取消".to_string()),
                    )
                    .await;
                    return;
                }
                // 「需要操作者判断」（§8.6）也是一次人工接手，和等待审批同一类：
                // 它在**问**，所以不受 `notify` 约束。
                EventPayload::RunNeedsAttention(body) => {
                    settle(
                        &state,
                        &watched,
                        FiringStatus::Waiting,
                        Some(body.reason.clone()),
                    )
                    .await;
                    let _ = state
                        .notifier
                        .deliver_home(Outbound::NeedsAttention {
                            session: watched.session.clone(),
                            run: watched.run.clone(),
                            reason: format!("定时任务「{}」：{}", watched.name, body.reason),
                        })
                        .await;
                    return;
                }
                _ => {}
            }
        }
    });
}

/// 把这条审批投到 home chat。
///
/// 来源会话那一半在 §11.4 里是"Run 的来源会话"——Cron 没有，所以这里只有 home chat。
/// 投不出去（一个 home_chat 都没配）**报到日志**，不静默丢弃：一个没人能回答的等待
/// 会让这个 Job 从此停在那里。
async fn deliver_approval(
    state: &Arc<GatewayState>,
    watched: &Watched,
    approval: &komo_kernel::types::ids::ApprovalId,
) {
    let record = match state.approval_repo.get(approval).await {
        Ok(Some(record)) => record,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, %approval, "读不出这条审批，投不出去");
            return;
        }
    };
    if record.decision.is_some() {
        return; // 已经决定过了，不必再问一次。
    }
    if let Err(error) = state
        .notifier
        .deliver_approval(None, komo_runtime::approvals::presentation(&record))
        .await
    {
        tracing::warn!(
            %error,
            job = %watched.job,
            %approval,
            "定时任务的审批请求投不出去：没人能回答它"
        );
    }
}

/// 更新本次触发状态（§10）。
async fn settle(
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
async fn notify(state: &Arc<GatewayState>, watched: &Watched, status: FiringStatus, text: String) {
    if !watched.notify.delivers(status) {
        tracing::debug!(job = %watched.job, notify = watched.notify.as_str(), "这一次不投");
        return;
    }
    let _ = state
        .notifier
        .deliver_home(Outbound::RunFinished {
            session: watched.session.clone(),
            run: watched.run.clone(),
            summary: text,
        })
        .await;
}

fn summary(watched: &Watched, body: &str) -> String {
    format!("定时任务「{}」：{body}", watched.name)
}
